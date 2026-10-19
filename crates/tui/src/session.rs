//! One attached client, and what it says.
//!
//! A [`Session`] is a link plus the state of the thing at the other end: the
//! client's grid, the controller that creates images, and whether one has been
//! placed. All of it is made once and updated by what arrives, which is why it is
//! a struct rather than a set of arguments.
//!
//! ```text
//!   a Frame ──▶ handle_msg ──▶ decode, transmit, place
//! ```
//!
//! Each frame replaces the image and re-places it, because a transmit under an
//! existing id frees that image's placements in the terminal.

use std::sync::Arc;

use easytgp::{Controller, DynImage, PlacementRequest, Png};
use wire::message::Message;
use wire::transport::Connection;

/// The image id the client's placeholder cells carry, as their foreground colour.
///
/// Fixed rather than negotiated: the two halves of this are one program, and the
/// only fact they would agree on is a number neither wants to vary. The cost is
/// that two panes on one terminal collide, which is a problem for the day a second
/// pane exists.
pub const IMAGE_ID: u32 = 99;

/// The placement id of the virtual placement the cells resolve against.
///
/// Distinct from [`IMAGE_ID`] because the two are different facts: the image is
/// the pixels, the placement is the box they are fitted into.
pub const PLACEMENT_ID: u32 = 98;

/// How long to wait for a terminal to acknowledge an image transmit.
///
/// A terminal that never answers is a terminal that will never draw the frame, so
/// waiting is pointless past the point where a working one would have replied.
/// Generous rather than tight: an acknowledgement is one short escape, and the cost
/// of being wrong is failing a session on a terminal that was merely slow.
const TRANSMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// What a session knows beyond its link.
pub struct State {
    /// The grid the client is drawing into, in cells.
    ///
    /// From the pty, because that is the size the client sees: a pane does not
    /// pass its geometry down through the environment, so the only side that knows
    /// it is the one that made the pty.
    pub cols: u32,
    pub rows: u32,
    /// The image the client's cells are resolved against.
    ///
    /// `None` until the first frame, because an image cannot be created before
    /// there are pixels for it: the protocol rejects a zero-sized one. Held rather
    /// than created per frame because its id is what the cells name, and a handle
    /// is the only thing that knows that id without tracking it separately.
    ///
    /// A `DynImage<Png>`, so the pixels travel inside the escape: nothing here
    /// depends on a shared memory object the terminal has to be able to read.
    pub image: Option<DynImage<Png>>,
    /// Creates image requests, and owns the reply router behind them.
    pub controller: Arc<Controller>,
}

/// One client the server has attached.
///
/// Holds its link and its state, and decides what to do with whatever arrives. The
/// decision is here rather than in the server's loop so the loop stays one
/// `select!` over several clients instead of a loop that also has to know what a
/// frame means.
pub struct Session {
    /// The link. Every session has one, so it is not optional.
    pub connection: Connection,
    /// What has been learned about the client.
    pub state: State,
}

impl Session {
    pub fn new(connection: Connection, cols: u32, rows: u32, controller: Arc<Controller>) -> Self {
        Self {
            connection,
            state: State {
                cols,
                rows,
                image: None,
                controller,
            },
        }
    }

    /// Do what `message` means.
    ///
    /// The whole conversation in one place: a frame is painted, an error is
    /// reported, and a greeting is not news. Painting waits for the terminal, which
    /// is why this is `async`: the placement of an image is acknowledged, and the
    /// next frame must not be sent before the terminal has the one before it.
    pub async fn handle_msg(&mut self, message: Message) -> std::io::Result<()> {
        match message {
            Message::Frame(jpeg) => self.paint(&jpeg).await,
            Message::Error(text) => {
                eprintln!("tui: a client reported: {text}");
                Ok(())
            }
            // The handshake's own messages, which were answered when the link was
            // established and mean nothing on their own.
            Message::Ping(label) => {
                eprintln!("tui: a client greeted us as {label}");
                Ok(())
            }
            Message::Pong => Ok(()),
        }
    }

    /// Paint one JPEG frame into this pane.
    ///
    /// The bytes are decoded here and re-encoded as PNG, which is what the terminal
    /// accepts inline: the pixels travel in the escape itself, so nothing depends on
    /// a shared memory object being readable.
    ///
    /// The first frame creates the image and places it, and waits for both: an image
    /// that is not placed draws nothing. Every later frame replaces the pixels of
    /// the same image, which keeps whatever references it valid, because the image's
    /// id is what those references name.
    async fn paint(&mut self, jpeg: &[u8]) -> std::io::Result<()> {
        let decoded = turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::RGB).map_err(|e| {
            std::io::Error::other(format!("decoding a {} byte frame: {e}", jpeg.len()))
        })?;
        let (width, height) = (decoded.width as u32, decoded.height as u32);
        // The buffer is moved into the image rather than copied out of it: at frame
        // sizes a copy is the whole frame again.
        let pixels = image::RgbImage::from_raw(width, height, decoded.pixels).ok_or_else(|| {
            std::io::Error::other(format!("{width}x{height} does not match the decoded buffer"))
        })?;

        let request = self
            .state
            .controller
            .new_png(pixels)
            .map_err(|e| std::io::Error::other(format!("encoding a frame: {e}")))?;

        // Timed, because this is the one await in the program that a terminal can
        // leave hanging: the acknowledgement it waits for is a reply a terminal may
        // not send, and a session that stalls here shows an empty pane forever with
        // no clue why. A timeout turns that into a stop with a name attached, which
        // is the difference between a bug and a mystery.
        let mut image = tokio::time::timeout(TRANSMIT_TIMEOUT, request.transmit_as(IMAGE_ID))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "tui: the terminal did not acknowledge the image transmit within {TRANSMIT_TIMEOUT:?}"
                )
            })
            .map_err(tgp)?;

        // Placed on *every* frame, not only the first, because a transmit under an
        // existing id **frees that image's placements**: the terminal treats a repeat
        // transmit as replacing the image wholesale, and its placements go with the
        // old data. So the first frame shows and the second one deletes the placement
        // that put it there — a pane that flashes once and then goes blank.
        //
        // Re-placing is cheap and it is what makes the picture come back. The
        // alternative is the animation-frame path (`a=f` plus `a=a,c=`), which adds a
        // frame instead of replacing the image and therefore keeps the placement; it
        // is the shape the design wants and this is the shape that works today.
        image.place(PlacementRequest {
            cols: Some(self.state.cols),
            rows: Some(self.state.rows),
            placement_id: Some(PLACEMENT_ID),
            ..Default::default()
        });
        // Waited on: until the placement lands nothing is on screen, so a frame that
        // was not placed is a frame the user never sees.
        image.sync().await.map_err(tgp)?;
        self.state.image = Some(image);
        Ok(())
    }
}

/// A `tgp` failure as an `io::Error`, for the one layer that reports one.
fn tgp(e: easytgp::TGPError) -> std::io::Error {
    std::io::Error::other(format!("the graphics protocol failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use wire::unix::UnixSocket;
    use wire::transport::Transport as _;

    /// A terminal that collects what it is sent and answers the liveness probes.
    ///
    /// The library's reply path is a real conversation: a transfer asks the
    /// terminal for an acknowledgement and waits for it, so a test that never
    /// answers would hang rather than fail. This answers the way a terminal does —
    /// echoing the marker it was asked about — which is what makes the awaited
    /// transmit complete.
    struct Terminal {
        /// Everything written to the terminal, escapes and all.
        written: Arc<Mutex<Vec<u8>>>,
        /// Where the acknowledgement goes: the input side of the library.
        keys: Mutex<std::io::PipeWriter>,
    }

    impl Write for Terminal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(buf);

            // A probe is `a=q,i=<marker>`, and the answer is that marker back with
            // an `OK`. A real terminal sends exactly this, so answering here is not
            // a stub: it is the other half of the protocol.
            let text = String::from_utf8_lossy(buf);
            for part in text.split('\x1b').filter(|p| p.starts_with("_Ga=q")) {
                let Some(marker) = part
                    .split(',')
                    .find_map(|cd| cd.strip_prefix("i="))
                    .map(|v| v.trim_end_matches('\u{5c}'))
                else {
                    continue;
                };
                let ack = format!("\x1b_Gi={marker};OK\x1b\\");
                self.keys.lock().unwrap().write_all(ack.as_bytes())?;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A JPEG frame of one flat colour, which is all the painter looks at.
    ///
    /// Encoded through the raw pixel form rather than the `image` integration: that
    /// integration is a feature this crate does not need, and a test should not be
    /// the reason to add one.
    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut encoder = turbojpeg::Compressor::new().expect("a JPEG encoder");
        encoder.set_quality(80).expect("quality");
        encoder
            .compress_to_vec(turbojpeg::Image {
                pixels: &vec![20u8; width as usize * height as usize * 3],
                width: width as usize,
                pitch: width as usize * 3,
                height: height as usize,
                format: turbojpeg::PixelFormat::RGB,
            })
            .expect("encode a frame")
    }

    /// A connection to throw away, so a session can be built.
    ///
    /// The link is not what these tests are about, but a session cannot exist
    /// without one: it is the thing a session is attached to.
    async fn connection(dir: &std::path::Path) -> Connection {
        let _ = std::fs::remove_dir_all(dir);
        let path = dir.join("s.sock");
        let bound = UnixSocket::new(&path);
        bound.bind().await.expect("bind");
        let dialer = UnixSocket::new(&path);
        let (accepted, _dialed) = tokio::join!(bound.accept(), dialer.connect());
        accepted.expect("accept")
    }

    /// A first frame creates the image and the virtual placement; a later one does not.
    ///
    /// Both halves are one test because the escape destination is global to the
    /// process and can only be set once: two tests would race for it, and whichever
    /// lost would observe an empty terminal rather than a wrong painting. The
    /// behaviour under test is a sequence anyway — first frame, then next.
    ///
    /// The first frame is the one that matters. The cells the client drew are on
    /// screen before any frame arrives, so an image that does not exist yet draws
    /// nothing; this is what makes the placeholders resolve. Every frame after it is
    /// one command under the same id, which keeps the cells valid, because the id
    /// lives in their colour.
    #[tokio::test]
    async fn a_first_frame_creates_the_image_and_a_later_one_does_not_re_place_it() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (keys, keys_writer) = std::io::pipe().expect("a pipe for the input");
        let terminal = Terminal {
            written: written.clone(),
            keys: Mutex::new(keys_writer),
        };
        let controller = Controller::init_with_sink(
            keys,
            std::io::sink(),
            Arc::new(Mutex::new(terminal)),
        );

        let dir = std::env::temp_dir().join(format!("tui-session-{}", std::process::id()));
        let mut session = Session::new(connection(&dir).await, 120, 40, controller);
        session
            .handle_msg(Message::Frame(jpeg(8, 8)))
            .await
            .expect("paint the first frame");

        let first = String::from_utf8(written.lock().unwrap().clone()).expect("escapes are text");
        // The image: the fixed id, the frame's size, and the shared memory medium.
        assert!(first.contains("i=99"), "{first:?}");
        assert!(first.contains("t=s"), "{first:?}");
        assert!(first.contains("s=8"), "{first:?}");
        assert!(first.contains("v=8"), "{first:?}");
        // The virtual placement: `U=1` with the pane's grid and the placement id,
        // which is what the client's placeholder cells resolve against.
        assert!(first.contains("a=p"), "{first:?}");
        assert!(first.contains("U=1"), "{first:?}");
        assert!(first.contains("c=120"), "{first:?}");
        assert!(first.contains("r=40"), "{first:?}");
        assert!(first.contains("p=98"), "{first:?}");

        let after_first = first.len();
        session
            .handle_msg(Message::Frame(jpeg(8, 8)))
            .await
            .expect("paint a later frame");

        let all = written.lock().unwrap().clone();
        let second = String::from_utf8(all[after_first..].to_vec()).expect("escapes are text");
        assert!(second.contains("i=99"), "the same id: {second:?}");
        assert!(
            !second.contains("a=p"),
            "a placement is not remade per frame: {second:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
