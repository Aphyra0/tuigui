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

use easytgp::{Controller, DynImage, Format, PlacementRequest};
use wire::message::Message;
use wire::transport::Connection;

use crate::sink::Sink;
use crate::DebugOptions;

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

/// The frame that is painted when [`DebugOptions::blue_pixel`] is set.
///
/// Small and solid, because its whole purpose is to be unmistakable: anything else
/// on screen is not this, and this cannot be confused with a decoded frame.
const BLUE: (u32, u32) = (64, 64);

/// What a session knows beyond its link.
///
/// Generic over the medium, which is fixed when the session is built: a caller that
/// knows its terminal cannot map a shared memory object picks [`easytgp::Png`], and
/// one that knows it can picks [`easytgp::Shm`]. Neither pays for the other's
/// branch, and the choice is a type rather than a flag.
pub struct State<F: Format> {
    /// The grid the client is drawing into, in cells.
    ///
    /// From the pty, because that is the size the client sees: a pane does not
    /// pass its geometry down through the environment, so the only side that knows
    /// it is the one that made the pty.
    pub cols: u32,
    pub rows: u32,
    /// The diagnostic switches this session was started with.
    pub debug: DebugOptions,
    /// The image the frames are sent under, and the pending operations on it.
    ///
    /// Kept across frames rather than rebuilt, because it owns the unacknowledged
    /// transmits and placements: a frame adds to it and `sync` is what waits for
    /// those to land. `None` until the first frame, since an image cannot be created
    /// before there are pixels for it.
    pub image: Option<DynImage<F>>,
    /// Creates image requests, and owns the reply router behind them.
    pub controller: Arc<Controller>,
    /// The one writer to the screen.
    ///
    /// Held so a cursor move lands in the same sequence as the escapes: a real
    /// placement draws where the cursor is, and a move written by some other writer
    /// could land between the move and the command that needed it.
    pub sink: Sink,
}

/// One client the server has attached.
///
/// Holds its link and its state, and decides what to do with whatever arrives. The
/// decision is here rather than in the server's loop so the loop stays one
/// `select!` over several clients instead of a loop that also has to know what a
/// frame means.
pub struct Session<F: Format> {
    /// The link. Every session has one, so it is not optional.
    pub connection: Connection,
    /// What has been learned about the client.
    pub state: State<F>,
}

impl<F: Format> Session<F> {
    pub fn new(
        connection: Connection,
        cols: u32,
        rows: u32,
        debug: DebugOptions,
        controller: Arc<Controller>,
        sink: Sink,
    ) -> Self {
        Self {
            connection,
            state: State {
                cols,
                rows,
                debug,
                image: None,
                controller,
                sink,
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

    /// Paint one frame into this pane.
    ///
    /// The frame that arrived is a JPEG, and the terminal does not accept JPEG, so it
    /// is decoded here and rebuilt as a request in this session's medium. Which
    /// medium that is is the type parameter, not a runtime choice: [`Format::request`]
    /// is what turns pixels into a request for it.
    async fn paint(&mut self, jpeg: &[u8]) -> std::io::Result<()> {
        let pixels = if self.state.debug.blue_pixel {
            // The frame is ignored on purpose, so nothing in the wire path, the codec
            // or the decode is exercised: what appears on screen is attributable to
            // the encode, the transmit and the placement alone.
            image::RgbImage::from_pixel(BLUE.0, BLUE.1, image::Rgb([0, 0, 255]))
        } else {
            let decoded = turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::RGB).map_err(|e| {
                std::io::Error::other(format!("decoding a {} byte frame: {e}", jpeg.len()))
            })?;
            let (width, height) = (decoded.width as u32, decoded.height as u32);
            // The buffer is moved into the image rather than copied out of it: at
            // frame sizes a copy is the whole frame again.
            image::RgbImage::from_raw(width, height, decoded.pixels).ok_or_else(|| {
                std::io::Error::other(format!("{width}x{height} does not match the decoded buffer"))
            })?
        };

        let request = F::request(&self.state.controller, pixels)
            .map_err(|e| std::io::Error::other(format!("building a frame request: {e}")))?;

        let placement = PlacementRequest {
            cols: Some(self.state.cols),
            rows: Some(self.state.rows),
            placement_id: Some(PLACEMENT_ID),
            // The design's placement is virtual: a prototype with no position of its
            // own, which the client's placeholder cells resolve against, so the picture
            // follows text a multiplexer may move. A real placement draws where the
            // cursor is, which is the experiment — and it is what needs the cursor put
            // back at the origin below.
            virtual_placement: !self.state.debug.no_vplacement,
            // Without this the terminal moves the cursor by the placement's own box
            // after placing, once per frame: on a stream that is a scroll per frame.
            fix_cursor: true,
            ..Default::default()
        };

        if !placement.virtual_placement {
            // A real placement draws at the cursor, so the cursor has to be at the
            // pane's origin before every frame — otherwise the picture lands wherever
            // the last one left the cursor, and the stream marches down the screen.
            //
            // Written through the same sink as the escapes, so no other writer can
            // land in the middle of it, and as a plain cursor move rather than a
            // clear: a clear would take the client's cells with it.
            self.state
                .sink
                .write_all(crossterm::cursor::MoveTo(0, 0).to_string().as_bytes())
                .map_err(|e| std::io::Error::other(format!("moving the cursor: {e}")))?;
        }

        // One command, not two. Transmitting and placing separately leaves the image
        // on screen with nothing drawing it in between — the terminal frees an image's
        // placements when its data is replaced — and that gap is a round trip wide,
        // which is a visible flicker on every frame.
        //
        // Timed, because this is the one await in the program that a terminal can leave
        // hanging: the acknowledgement it waits for is a reply a terminal may not send,
        // and a session that stalls here shows an empty pane forever with no clue why.
        let mut image = tokio::time::timeout(TRANSMIT_TIMEOUT, request.transplace(IMAGE_ID, placement))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "tui: the terminal did not acknowledge the image transmit within {TRANSMIT_TIMEOUT:?}"
                )
            })
            .map_err(tgp)?;

        // Waited on: until the placement lands nothing is on screen, so a frame that was
        // not placed is a frame the user never sees.
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

    /// Every frame is transmitted under one id, and placed again.
    ///
    /// Both halves are one test because the escape destination is global to the
    /// process and can only be set once: two tests would race for it, and whichever
    /// lost would observe an empty terminal rather than a wrong painting. The
    /// behaviour under test is a sequence anyway — first frame, then next.
    ///
    /// The re-placement is the point rather than a detail. A transmit under an id the
    /// terminal already has **frees that image's placements**, so a session that places
    /// only its first frame shows one picture and then a blank pane. What is asserted
    /// here is that the second frame carries its own placement, under the same id.
    #[tokio::test]
    async fn every_frame_reuses_one_id_and_places_itself_again() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (keys, keys_writer) = std::io::pipe().expect("a pipe for the input");

        // One writer into the one buffer, which is what the program does: `tui::run` gives
        // the same sink to the library that writes the escapes and to the session that
        // moves the cursor. Two writers here would model an interleaving the `Sink` exists
        // to prevent, and would put the escapes and the cursor move where nothing can
        // observe their order.
        //
        // TODO: refactor. The test builds the wiring the program builds, by hand, and it is
        // the one place that knows both halves have to be the same object. `Tui::run` is
        // where that belongs: a helper that takes a stream and returns the sink and the
        // controller together would keep the test from being able to get it wrong.
        let sink = crate::sink::Sink::new(Terminal {
            written: written.clone(),
            keys: Mutex::new(keys_writer),
        });
        let controller = Controller::init_with_sink(
            keys,
            sink.clone(),
            Arc::new(Mutex::new(sink.clone())),
        );

        let dir = std::env::temp_dir().join(format!("tui-session-{}", std::process::id()));
        let mut session = Session::<easytgp::Shm>::new(
            connection(&dir).await,
            120,
            40,
            DebugOptions::default(),
            controller,
            sink,
        );
        session
            .handle_msg(Message::Frame(jpeg(8, 8)))
            .await
            .expect("paint the first frame");

        let first = String::from_utf8(written.lock().unwrap().clone()).expect("escapes are text");
        // One command, `a=T`, that both replaces the pixels and makes the placement.
        // The two used to be separate, and the terminal frees an image's placements when
        // its data is replaced -- so between them the picture was on screen with nothing
        // drawing it, for as long as a round trip takes. That gap is the flicker.
        assert!(first.contains("a=T"), "one transplace command: {first:?}");
        assert!(first.contains("i=99"), "{first:?}");
        assert!(first.contains("t=s"), "{first:?}");
        // The transmitted data's size, and the cell box it is fitted into. Both travel
        // on one command and are different keys.
        assert!(first.contains("s=8"), "{first:?}");
        assert!(first.contains("v=8"), "{first:?}");
        assert!(first.contains("c=120"), "{first:?}");
        assert!(first.contains("r=40"), "{first:?}");
        // The design's placement is virtual, so the client's placeholder cells are what
        // anchor the image rather than the cursor.
        assert!(first.contains("U=1"), "{first:?}");
        assert!(first.contains("p=98"), "{first:?}");
        // Without this the terminal moves the cursor by the placement's box once per
        // frame, which walks the picture down the screen.
        assert!(first.contains("C=1"), "{first:?}");

        let after_first = first.len();
        session
            .handle_msg(Message::Frame(jpeg(8, 8)))
            .await
            .expect("paint a later frame");

        let all = written.lock().unwrap().clone();
        let second = String::from_utf8(all[after_first..].to_vec()).expect("escapes are text");
        // The same id, so the client's cells keep naming the image being updated, and its
        // own placement, under the same placement id -- a repeat transmit replaces the
        // placement that was there.
        assert!(second.contains("a=T"), "a later frame transplaces: {second:?}");
        assert!(second.contains("i=99"), "the same id: {second:?}");
        assert!(
            second.contains("p=98"),
            "under the same placement id: {second:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
