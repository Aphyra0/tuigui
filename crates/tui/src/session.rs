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

use std::io;
use std::sync::{Arc, RwLock, Weak};

use tracing::Instrument;
use easytgp::{Controller, DynImage, Format, PlacementRequest};
use codecs::CodecId;
use wire::message::{Message, SurfaceId};
use wire::transport::Connection;

use crate::error::{Error, Result};
use crate::sink::Sink;
use crate::DebugOptions;

/// The image id a window's frames are sent under.
///
/// Named by the window's own surface, not by a constant: two windows are two
/// pictures, and one shared id would make the second replace the first. Derived
/// rather than allocated so both ends agree without a negotiation, and so it is
/// stable across frames — a replacement under the same id is what makes a frame an
/// update rather than a new image.
pub fn image_id(surface: &SurfaceId) -> u32 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    surface.hash(&mut hasher);
    // Kept out of the low range: ids below a few hundred are the ones a terminal's
    // own examples use, and a picture is not worth colliding with one over.
    (hasher.finish() % (u32::MAX as u64 - 1024)) as u32 + 1024
}

/// The z index a window's picture is drawn at.
///
/// Below `INT32_MIN / 2`, which is the band the protocol draws behind cell
/// backgrounds as well as behind text (`docs/kitty-graphics-protocol.rst`), so a
/// window's picture cannot cover another window's content.
///
/// The order *within* that band comes from the gui, which is the side with a window
/// manager: it sends a `z` per window, and this shifts the whole range into the
/// band. A window's rank is therefore the gui's, not invented here.
fn stacking(z: i32) -> i32 {
    // The band's floor. `INT32_MIN / 2` is -1,073,741,824, and the band reaches up to
    // -1, which is a billion distinct values for the gui's own ordering.
    const FLOOR: i32 = i32::MIN / 2;
    const CEILING: i32 = -1;
    FLOOR.saturating_add(z).min(CEILING)
}

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

/// What one window knows about itself.
///
/// Generic over the medium, which is fixed when the session is built: a caller that
/// knows its terminal cannot map a shared memory object picks [`easytgp::Png`], and
/// one that knows it can picks [`easytgp::Shm`]. Neither pays for the other's
/// branch, and the choice is a type rather than a flag.
///
/// One of these per window, not per session: two windows are two pictures, and a
/// decoder that followed one stream cannot follow the other.
pub struct State<F: Format> {
    /// The image the frames are sent under, and the pending operations on it.
    ///
    /// Kept across frames rather than rebuilt, because it owns the unacknowledged
    /// transmits and placements: a frame adds to it and `sync` is what waits for
    /// those to land. `None` until the first frame, since an image cannot be created
    /// before there are pixels for it.
    pub image: Option<DynImage<F>>,
    /// The codec's decoder, kept across frames, built when the client announces
    /// its codec. A decoder that cannot follow the stream is dropped here and
    /// rebuilt after a `ReinitStreaming` — its state is the only thing the trait
    /// object owns, which is why losing it is a message rather than a guess.
    pub decoder: Option<Box<dyn codecs::Decoder>>,
}

impl<F: Format> State<F> {
    /// A window that has not been drawn into yet.
    pub(crate) fn new() -> Self {
        Self {
            image: None,
            decoder: None,
        }
    }
}

/// One client the server has attached.
///
/// Holds its link and what belongs to the link rather than to a window, and decides
/// what to do with whatever arrives. The decision is here rather than in the
/// server's loop so the loop stays one `select!` over several clients instead of a
/// loop that also has to know what a frame means.
pub struct Session<F: Format> {
    /// The link. Every session has one, so it is not optional.
    pub connection: Connection,
    /// The diagnostic switches this session was started with.
    pub debug: std::sync::Arc<DebugOptions>,
    /// The codec the client announced, which is what a decoder is built from
    /// whenever one must be built: an announcement, a reset, a rebuild.
    pub codec: Option<CodecId>,
    /// Creates image requests, and owns the reply router behind them.
    pub controller: Arc<Controller>,
    /// The one writer to the screen.
    ///
    /// Held so a cursor move lands in the same sequence as the escapes: a real
    /// placement draws where the cursor is, and a move written by some other writer
    /// could land between the move and the command that needed it.
    pub sink: Sink,
    /// The window tree the remote compositor reported.
    ///
    /// The compositor owns the tree and this side mirrors it: a window is placed by
    /// the surface its cells carry, so a window this side has not been told about
    /// cannot be drawn. `None` until a client reports itself, which is also the
    /// reason a frame can arrive with nowhere to go.
    pub root: Option<crate::server::Client<F>>,
    /// The surfaces currently drawn into this pane, for the proxy to blank.
    ///
    /// Weak, so the registry never keeps a window alive: a window owns its surface,
    /// and when the window goes the entry stops resolving. That is how a closed
    /// window stops having its cells blanked without anyone having to say so.
    pub registered_surfaces: Arc<RwLock<Vec<Weak<SurfaceId>>>>,
    /// The screen the command is drawing into, shared with the proxy.
    ///
    /// Read, never written: the proxy is the side that feeds it, and this side asks
    /// it where a window's cells are.
    pub screen: Arc<RwLock<vt100::Parser>>,
}

impl<F: Format> Session<F> {
    pub fn new(
        connection: Connection,
        debug: std::sync::Arc<DebugOptions>,
        controller: Arc<Controller>,
        sink: Sink,
        registered_surfaces: Arc<RwLock<Vec<Weak<SurfaceId>>>>,
        screen: Arc<RwLock<vt100::Parser>>,
    ) -> Self {
        Self {
            connection,
            debug,
            codec: None,
            controller,
            sink,
            root: None,
            registered_surfaces,
            screen,
        }
    }

    /// Take `surface` into the registry, and hand back the shared handle.
    ///
    /// Shared first, then registered weakly: the window owns the id, the registry only
    /// points at it, and when the window goes the entry stops resolving — which is
    /// what stops its cells being blanked.
    fn register(&self, surface: SurfaceId) -> Arc<SurfaceId> {
        let surface = Arc::new(surface);
        if let Ok(mut registered) = self.registered_surfaces.write() {
            registered.push(Arc::downgrade(&surface));
        }
        surface
    }

    /// Do what `message` means.
    ///
    /// The whole conversation in one place: a frame is painted, an error is
    /// reported, and a greeting is not news. Painting waits for the terminal, which
    /// is why this is `async`: the placement of an image is acknowledged, and the
    /// next frame must not be sent before the terminal has the one before it.
    ///
    /// Returns nothing, because a failure here is the *client's* to act on: the client
    /// is the side with a pane and a decision to make about it, and the server has no
    /// better answer than the one it can send. So every failure — a frame that would
    /// not decode, a transmit the terminal never acknowledged — goes back as
    /// [`Message::Error`] and the client exits, which is what a client that cannot
    /// draw should do.
    pub async fn handle_msg(&mut self, message: Message) {
        if let Err(e) = match message {
            Message::Frame {
                pid,
                surface,
                bytes,
            } => self.paint(&bytes, pid, surface).await,
            Message::InitStreaming { codec } => {
                // The client named its codec. It is kept on the session because it is
                // a fact about the link: every window this client's programs open
                // streams in it. The decoders are per window, and are built when a
                // frame first needs one.
                tracing::info!("a client will stream {codec:?}");
                self.codec = Some(codec);
                Ok(())
            }
            Message::ReinitStreaming => {
                // Reinit is a server-to-client message: the side that owns the
                // decoder is the side that asks for a keyframe. A client that
                // sends one has the halves backwards, and the error is what goes
                // back — reported, not obeyed.
                Err(io::Error::other("a client cannot ask its server for a keyframe; that is a server-to-client message").into())
            }
            Message::Error(text) => {
                tracing::warn!("a client reported: {text}");
                Ok(())
            }
            // The handshake's own messages, which were answered when the link was
            // established and mean nothing on their own.
            Message::Ping(label) => {
                tracing::info!("a client greeted us as {label}");
                Ok(())
            }
            Message::Pong => Ok(()),
            // A client of the compositor is ready to be drawn. The first one is the
            // session's root: a session runs one program, and a helper is a client of
            // its own that hangs under whichever pid started it.
            Message::InitClient { pid, surface, z } => {
                let surface = self.register(surface);
                let client = crate::server::Client::new(pid, surface, z);
                match self.root.as_mut() {
                    Some(root) => {
                        // A helper: a client of its own, started by the session's
                        // program. It hangs under the root rather than under the pid
                        // that spawned it, because a session is one program and every
                        // other connection in it was started by that program or by
                        // something it started — the tree's depth is a fact the
                        // compositor has and this side does not need to rebuild.
                        root.sub_clients.push(client);
                        Ok(())
                    }
                    None => {
                        self.root = Some(client);
                        Ok(())
                    }
                }
            }
            // Another toplevel from a client the session already knows: a child of
            // that client's root window, addressed under the same pid.
            Message::NewWindow { pid, surface, z } => {
                let surface = self.register(surface);
                match self.root.as_mut().and_then(|root| root.find_mut(pid)) {
                    Some(owner) => {
                        owner
                            .window
                            .children
                            .push(crate::server::Window::new(surface, z));
                        Ok(())
                    }
                    None => Err(io::Error::other(format!(
                        "a window arrived for an unknown client {pid}"
                    ))
                    .into()),
                }
            }
            Message::DeleteWindow { surface } => {
                let removed = self
                    .root
                    .as_mut()
                    .is_some_and(|root| root.remove_window(surface));
                if removed {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("no window draws into {surface:?}")).into())
                }
            }
            Message::Restack { surface, z } => {
                let restacked = self
                    .root
                    .as_mut()
                    .is_some_and(|root| root.restack(surface, z));
                if restacked {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("no window draws into {surface:?}")).into())
                }
            }
            // Not handled yet. Nothing here is a failure — the client sends these in
            // the normal course of a session — so they are reported rather than
            // obeyed or turned into an error the client would see as the end.
            other @ Message::CompositorRequest(_) => {
                tracing::warn!("tui: {other:?} is not handled yet");
                Ok(())
            }
        } {
            tracing::error!("{e}");
            // Reported rather than returned: the link is the only channel a client listens
            // on, and a failure it is not told about is a pane that stops changing for no
            // stated reason. Retried because this is the last thing the peer is ever told —
            // a report that is itself lost leaves that same silent pane.
            let report = Message::Error(e.to_string());
            futures_retry::FutureRetry::new(
                || self.connection.send(&report),
                crate::error::ReportFailure,
            )
            .await
            .unwrap_or_else(|(e, attempts)| {
                panic!("could not report a failure to the client after {attempts} attempts: {e}")
            });
        }
    }

    /// Paint one frame into this pane.
    /// The frame that arrives is a stretch of the codec's stream, and the terminal does
    /// not accept that, so it is decoded here and rebuilt as a request in this session's
    /// medium. Which medium that is is the type parameter, not a runtime choice:
    /// [`Format::request`] is what turns pixels into a request for it.
    ///
    /// The frame is painted into the window its surface names: the surface
    /// identifies the cells, and the frame is a stretch of *that* window's stream, so
    /// the decoder it continues is that window's. The pid is the report's own — the
    /// client that sent it owns the window.
    ///
    /// One span per frame, so the profiler's timeline is divided at the same
    /// boundary the pane's is. The awaits are inside it, which is why this is
    /// `#[instrument]` rather than a held guard: `Span::enter`'s guard must not
    /// outlive an `.await`, and the attribute enters and exits per poll instead.
    /// The two waits it contains — the terminal's acknowledgement of the
    /// transmit, and of the placement — are the two numbers this exists to see.
    #[tracing::instrument(name = "frame", skip(self, stream))]
    async fn paint(&mut self, stream: &[u8], pid: u32, surface: SurfaceId) -> Result<()> {
        // A frame for a window this side has not been told about is not a failure: the
        // announcement and the picture are two messages, and a window that is already
        // drawing can be announced after the drawer's first commit has been encoded. The
        // frame is dropped and the next one — which arrives after the announcement — is
        // painted, so nothing is lost but one turn.
        let Some(root) = self.root.as_mut() else {
            tracing::debug!("a frame for {surface:?} arrived before its client was reported");
            return Ok(());
        };
        let Some(window) = root.get_window_mut(pid, surface) else {
            tracing::debug!("a frame for a window this session does not know ({pid}, {surface:?})");
            return Ok(());
        };

        // Where the window is, read off the screen rather than sent: the cells
        // carrying its surface are its box, so a multiplexer that moved the text
        // moved the window without anyone having to say so.
        //
        // A window with no cells is not placed. It has scrolled away, or been
        // covered entirely, or has not drawn yet, and all three mean the same thing
        // to a placement: there is nothing on screen for it to go behind.
        let rectangle = {
            let screen = self.screen.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            window.rectangle(screen.screen())
        };
        let Some((left, top, width, height)) = rectangle else {
            return Ok(());
        };

        let state = &mut window.state;
        let pixels = if self.debug.blue_pixel {
            // The frame is ignored on purpose, so nothing in the wire path, the codec
            // or the decode is exercised: what appears on screen is attributable to
            // the encode, the transmit and the placement alone.
            image::RgbImage::from_pixel(BLUE.0, BLUE.1, image::Rgb([0, 0, 255]))
        } else {
            // The decoder is built from the codec the client announced, and kept:
            // building one per frame is the expensive part, not the decoding.
            if state.decoder.is_none() {
                let codec = self.codec.ok_or_else(|| {
                    io::Error::other("a frame arrived before any codec was announced")
                })?;
                state.decoder = Some(codec.to_decoder()?);
            }
            let _decode = tracing::info_span!("decode").entered();
            let decoder = state.decoder.as_mut().unwrap();
            match decoder.decode(stream) {
                Ok(Some(frame)) => image::RgbImage::from_raw(frame.width, frame.height, frame.rgb)
                    .ok_or(Error::FrameSize {
                        width: frame.width,
                        height: frame.height,
                    })?,
                // Headers, or a frame the codec held back: nothing to show yet,
                // and nothing to ask for — the next frame the client sends is what
                // the decoder held, and a static window sends again when it moves.
                Ok(None) => return Ok(()),
                // The decoder's state is what failed: drop it, and ask the client
                // for a keyframe — the frame after this one rebuilds the decoder
                // from the announced codec and starts the stream over.
                Err(e) => {
                    state.decoder = None;
                    let _ = self.connection.send(&Message::ReinitStreaming).await;
                    tracing::error!("the codec failed: {e}");
                    return Ok(());
                }
            }
        };

        let request = F::request(&self.controller, pixels).map_err(Error::Stage)?;

        let placement = PlacementRequest {
            // The box the surface's own cells occupy, in cells. The image is fitted to
            // it, so the picture is scaled to exactly the window's area.
            //
            // Only the *size* goes here. `x`/`y` are not position: in the protocol they
            // crop a source rectangle out of the image, and the position is wherever the
            // cursor is. That is why the cursor is moved below.
            cols: Some(u32::from(width)),
            rows: Some(u32::from(height)),
            // Behind the text *and* behind the cell backgrounds, so a window's
            // picture cannot cover another window's content. Ordered among windows by
            // `z`, which is what `stacking` is for.
            z: Some(stacking(window.z)),
            placement_id: Some(PLACEMENT_ID),
            // A real placement draws at the cursor, and nothing here is virtual: the
            // cells are the window's own area, so the picture goes where the cursor is
            // put.
            virtual_placement: false,
            // Without this the terminal moves the cursor by the placement's own box
            // after placing, once per frame: on a stream that is a scroll per frame.
            fix_cursor: true,
            ..Default::default()
        };

        // The picture is drawn from the cursor, so the cursor goes to the window's own
        // top-left cell first. This is the position — the protocol has no key for it,
        // which is why it is a cursor move and not a field on the placement.
        //
        // Written through the same sink as the escapes, so no other writer can land in
        // the middle of it, and as a plain cursor move rather than a clear: a clear would
        // take the client's cells with it.
        self.sink
            .write_all(crossterm::cursor::MoveTo(left, top).to_string().as_bytes())
            .map_err(Error::Screen)?;

        // One command, not two. Transmitting and placing separately leaves the image
        // on screen with nothing drawing it in between — the terminal frees an image's
        // placements when its data is replaced — and that gap is a round trip wide,
        // which is a visible flicker on every frame.
        //
        // Timed, because this is the one await in the program that a terminal can leave
        // hanging: the acknowledgement it waits for is a reply a terminal may not send,
        // and a session that stalls here shows an empty pane forever with no clue why.
        let transplace_span = tracing::info_span!("transplace");
        // The image is named by the window's own surface, not by a constant: two
        // windows are two pictures, and one id would make the second replace the
        // first. A hash of the signature is enough — it is stable across frames, and
        // a collision is a window drawn over another rather than a corrupted picture.
        let image_id = image_id(&surface);
        let mut image = tokio::time::timeout(
            TRANSMIT_TIMEOUT,
            request
                .transplace(image_id, placement)
                .instrument(transplace_span),
        )
        .await
        .map_err(|_| Error::TransmitTimeout(TRANSMIT_TIMEOUT))?
        .map_err(Error::Graphics)?;

        // Waited on: until the placement lands nothing is on screen, so a frame that was
        // not placed is a frame the user never sees.
        let ack_span = tracing::info_span!("image.sync");
        image
            .sync()
            .instrument(ack_span)
            .await
            .map_err(Error::Graphics)?;
        state.image = Some(image);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use wire::unix::UnixSocket;
    use wire::transport::Transport as _;

    /// The window the tests paint into: a pid and a surface are its identity, and
    /// both are what a frame names.
    const PID: u32 = 4242;
    const SURFACE: SurfaceId = SurfaceId {
        symbol: '❋',
        fg: (1, 2, 3),
        bg: (4, 5, 6),
    };

    /// The escapes that fill a `width` x `height` block of the surface's cells at
    /// `(left, top)`, one based.
    ///
    /// What a real client draws into its pane: the cells carrying the surface's
    /// signature are the box the image is placed in, so a test that wants a placement
    /// has to draw them.
    fn draw_surface(left: u16, top: u16, width: u16, height: u16) -> String {
        let (r, g, b) = SURFACE.fg;
        let (br, bg_, bb) = SURFACE.bg;
        let mut out = String::new();
        for row in top..top + height {
            out.push_str(&format!(
                "\x1b[{};{}H\x1b[38;2;{r};{g};{b}m\x1b[48;2;{br};{bg_};{bb}m",
                row + 1,
                left + 1
            ));
            out.extend(std::iter::repeat(SURFACE.symbol).take(width as usize));
        }
        out
    }

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

    /// A codec frame of one flat colour, which is all the painter looks at.
    ///
    /// Encoded through the codec itself rather than anything simpler: a test that
    /// feeds the decoder bytes another codec produced would pass by accident.
    fn codec_frame(width: u32, height: u32) -> Vec<u8> {
        let mut encoder = codecs::h264::Encoder::new(width, height).expect("an encoder");
        let bgra = vec![20u8, 20, 20, 255].repeat((width * height) as usize);
        let stream = codecs::Encoder::encode(&mut encoder, &bgra, width, height, false)
            .expect("encode a frame");
        assert!(!stream.is_empty(), "the first frame of a stream carries its picture");
        stream
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
        let screen = Arc::new(RwLock::new(vt100::Parser::new(
            std::num::NonZeroU16::new(40).expect("rows"),
            std::num::NonZeroU16::new(120).expect("cols"),
            0,
        )));
        let mut session = Session::<easytgp::Shm>::new(
            connection(&dir).await,
            Arc::new(DebugOptions::default()),
            controller,
            sink,
            Arc::new(RwLock::new(Vec::new())),
            screen.clone(),
        );
        session
            .handle_msg(Message::InitStreaming { codec: CodecId::H264 })
            .await;
        // The window the frames are addressed to: a frame names its pid and its
        // surface, and a session that has not been told about that window has nowhere
        // to paint it.
        session
            .handle_msg(Message::InitClient {
                pid: PID,
                surface: SURFACE,
                z: 0,
            })
            .await;
        // The window's cells, drawn where the picture is meant to go: the rectangle
        // comes from the screen, so a window with no cells on it is a window that is
        // not placed. The proxy does this in a session; here it is done by hand.
        {
            let mut screen = screen.write().unwrap();
            screen.process(draw_surface(3, 5, 20, 10).as_bytes());
        }
        session
            .handle_msg(Message::Frame {
                pid: PID,
                surface: SURFACE,
                bytes: codec_frame(8, 8),
            })
            .await;
        let first = String::from_utf8(written.lock().unwrap().clone()).expect("escapes are text");
        // One command, `a=T`, that both replaces the pixels and makes the placement.
        // The two used to be separate, and the terminal frees an image's placements when
        // its data is replaced -- so between them the picture was on screen with nothing
        // drawing it, for as long as a round trip takes. That gap is the flicker.
        assert!(first.contains("a=T"), "one transplace command: {first:?}");
        // The image is named by the window's own surface, so two windows are two
        // pictures rather than one replacing the other.
        let image = image_id(&SURFACE);
        assert!(first.contains(&format!("i={image}")), "{first:?}");
        assert!(first.contains("t=s"), "{first:?}");
        // The transmitted data's size, and the cell box it is fitted into. Both travel
        // on one command and are different keys.
        assert!(first.contains("s=8"), "{first:?}");
        assert!(first.contains("v=8"), "{first:?}");
        // The box is the surface's own cells: `draw_surface` puts a 20x10 window at
        // (3, 5), and that is what the placement is given.
        assert!(first.contains("c=20"), "{first:?}");
        assert!(first.contains("r=10"), "{first:?}");
        assert!(first.contains("x=3"), "{first:?}");
        assert!(first.contains("y=5"), "{first:?}");
        // Behind the text and behind the backgrounds, which is the band below
        // `INT32_MIN / 2`. The gui's rank is added to the band's floor, and rank 0
        // is the floor itself.
        assert!(
            first.contains(&format!("z={}", i32::MIN / 2)),
            "{first:?}"
        );
        assert!(first.contains("p=98"), "{first:?}");
        // Without this the terminal moves the cursor by the placement's box once per
        // frame, which walks the picture down the screen.
        assert!(first.contains("C=1"), "{first:?}");

        let after_first = first.len();
        session
            .handle_msg(Message::Frame {
                pid: PID,
                surface: SURFACE,
                bytes: codec_frame(8, 8),
            })
            .await;

        let all = written.lock().unwrap().clone();
        let second = String::from_utf8(all[after_first..].to_vec()).expect("escapes are text");
        // The same id, so the client's cells keep naming the image being updated, and its
        // own placement, under the same placement id -- a repeat transmit replaces the
        // placement that was there.
        assert!(second.contains("a=T"), "a later frame transplaces: {second:?}");
        assert!(
            second.contains(&format!("i={}", image_id(&SURFACE))),
            "the same id: {second:?}"
        );
        assert!(
            second.contains("p=98"),
            "under the same placement id: {second:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
