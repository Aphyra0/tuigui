//! `gui` — the client of the remote-streaming experiment.
//!
//! Usage (inside a pane spawned by `tui`):
//!   gui <runnable>          # be found by the server, and draw into its pane
//!
//! The client owns the pane's screen and draws it. What it draws is a widget
//! painted on `stdout`, which `tui` is forwarding: this side never writes the
//! terminal directly, so nothing it does can disturb the width `tui` measures for
//! the pane.
//!
//! The runnable's *pixels* travel out of band: `tui` paints them onto the real
//! terminal, which is the split that lets a multiplexer show an image it never
//! received. Reading those pixels is `Gui::run`'s job and is not written yet.
//!
//! # Who listens, and who dials
//!
//! The client **listens**. It is the half that cannot be reached: it may be inside
//! a multiplexer's pane, on another machine, or started by something whose
//! environment nobody controls. Its own output is the only channel it is
//! guaranteed to have, so it listens on a channel of its own choosing and writes
//! that choice there. The server, already reading every byte the pty forwards,
//! dials it.
//!
//! ```text
//!   listen(path) ──▶ write the advert into stdout ──▶ pty ──▶ server dials it
//!                                                              │
//!                        accept ◀──────────────────────────────┘
//! ```
//!
//! Nothing about the channel is provisioned from outside. There is no session
//! file, no path to keep in step, and nothing to clean up after a crash: a stale
//! file is a client dialing a socket nobody is listening on, and announcing
//! in-band cannot go stale.

use std::io::{Read, Write};
use std::time::Duration;

use wire::message::Message;
use wire::transport::{Connection, Transport};
use wire::unix::UnixSocket;

// First, and with `macro_use`, because `eprln!` is defined here: a module that uses the
// macro has to be declared after the one that introduces it.
#[macro_use]
mod log;
mod compositor;
mod widget;

/// The size of this terminal in pixels, for the compositor's output.
///
/// An app run under a compositor is configured with a window size in pixels, and the
/// only honest source for it is the terminal's own cell geometry: the pane's grid is
/// what the picture is finally fitted to, so an output sized to it is one the app lays
/// itself out against correctly.
///
/// The pixel fields of a terminal's window size are documented as unreliable and often
/// zero (they are a property of the graphics layer, not the tty), so a terminal that
/// does not report them is not an error: [`DEFAULT_CELL`] is then used, which is the
/// fallback every program that needs a cell size in pixels uses. That is an assumption,
/// and it is stated rather than hidden, because a terminal that does report them gets
/// its real geometry.
fn output_size() -> std::io::Result<(u32, u32)> {
    let ws = ratatui::crossterm::terminal::window_size()?;
    let (cols, rows) = (ws.columns as u32, ws.rows as u32);
    let (cell_w, cell_h) = match (ws.width, ws.height) {
        (w, h) if w > 0 && h > 0 => (
            (w as u32 / cols.max(1)).max(1),
            (h as u32 / rows.max(1)).max(1),
        ),
        _ => DEFAULT_CELL,
    };
    Ok((cols.max(1) * cell_w, rows.max(1) * cell_h))
}

/// The cell size assumed when the terminal does not report one, in pixels.
///
/// Standard for a monospace face at the sizes terminals ship with, and it only has to
/// be close: the compositor's output is what the app lays out against, and the terminal
/// fits the resulting picture to the same cell grid either way.
const DEFAULT_CELL: (u32, u32) = (10, 20);

/// Where this client listens when nothing else is asked for.
///
/// Under `XDG_RUNTIME_DIR` and not `/tmp`, because `/tmp` is world-writable and a
/// socket there is reachable by every user on the machine. That directory is the
/// one the platform already made private to this user and clears on logout.
///
/// Named for the process, because a path is per-client: two clients in two panes
/// must not share one, and the announcement carries the path, so only this side
/// has to know what it is.
fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&dir).join(format!("tuigui-{}.sock", std::process::id()))
}

/// What kind of runnable a path names.
///
/// The distinction is by extension and by nothing else, because that is all the
/// command line carries: `gui clip.mp4` and `gui /usr/bin/foot` are the same shape of
/// argument, and what to *do* with them is the only thing that differs. Reading it
/// here, once, is what keeps the two paths from each having to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runnable {
    /// A clip, decoded with ffmpeg.
    Video,
    /// A program, run inside a compositor of our own and captured frame by frame.
    App,
}

impl Runnable {
    /// Classify `path`.
    ///
    /// Only the `.mp4` extension counts as a clip, case-insensitively. Everything else
    /// is a program, which is the safe default: an unrecognized path is far more likely
    /// to be a binary than an exotic video container, and running a binary is what this
    /// command is for.
    pub fn classify(path: &std::path::Path) -> Self {
        let is_mp4 = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
        if is_mp4 {
            Runnable::Video
        } else {
            Runnable::App
        }
    }
}

/// A runnable to stream into this pane.
///
/// The path is a positional argument because it is the whole command: `gui
/// /path/to/app` has nothing else to decide, and a flag would only add a word to
/// type for every invocation.
#[derive(Debug, Clone, clap::Parser)]
#[command(
    name = "gui",
    about = "Draw a runnable's output into the pane `tui` is serving"
)]
pub struct Gui {
    /// The runnable to draw.
    pub path: std::path::PathBuf,
    /// The socket to listen on. Defaults to a per-process path under
    /// `XDG_RUNTIME_DIR`, which is what the server is told to dial.
    #[arg(long = "socket", env = "TUI_SOCKET")]
    pub socket: Option<std::path::PathBuf>,
    /// The diagnostic switches, which belong to the client.
    #[command(flatten)]
    pub debug: DebugOptions,
}

/// The switches that exist to diagnose a session, not to use one.
///
/// The client's half of the set: what the *sender* can do to make a failure visible.
/// A session has two ends and either can be the reason nothing appears, so each end
/// has its own switches and each stays with the code it changes.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct DebugOptions {
    /// Play the runnable again from the start when it ends, forever.
    ///
    /// A clip is short and a session is not, so a run that ends after a few seconds
    /// is hard to look at while something else is being examined, which is the usual
    /// case when one of these switches is needed at all.
    #[arg(long = "dbg-loop-video")]
    pub loop_video: bool,
}

impl Gui {
    /// Listen, announce, and draw until the session ends.
    ///
    /// `stdin` and `stdout` are taken as arguments rather than read from the
    /// process: they are this client's whole interface to the outside world, and
    /// passing them in is what lets the session be driven by a caller that is not
    /// a terminal (`tuigui`, or a test). The caller owns its own runtime, so this
    /// is `async` with no runtime of its own.
    pub async fn run(
        self,
        stdin: impl Read + Send + 'static,
        stdout: impl Write + Send + 'static,
    ) -> std::io::Result<()> {
        // Drawn to the writer the caller gave, not to `io::stdout()`: the caller is what
        // knows where this pane's screen is, and writing to the process's own stdout would
        // ignore it.
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend)?;

        // The socket, and the address that goes in the announcement. The two are one fact,
        // which is why the transport produces both.
        let socket = UnixSocket::new(self.socket.clone().unwrap_or_else(socket_path));

        // Bound *before* anything puts the address on screen. The drawing and the
        // announcement are the same stream, so a pane that shows its address before the
        // socket exists is a pane whose address does not answer: a server reading it would
        // dial a socket nobody is listening on, and having been told not to dial twice it
        // would never try again.
        socket.bind().await?;

        // What this client announces, and also what the pane shows while it waits: a client
        // that announced an address nobody dialed can say which address, which is the
        // difference between a visible stall and a blank pane that looks like a crash.
        let announcement = socket
            .to_magic()
            .ok_or_else(|| std::io::Error::other("the socket transport has no address"))?;

        // The announcement goes out as *drawing*, centered, which is the only way it gets
        // into the stream: nothing here writes the terminal directly, so the text a server
        // reads is the same text a user sees.
        terminal.draw(|frame| {
            frame.render_widget(widget::Session::waiting(&announcement), frame.area())
        })?;

        // A dial that never comes is not an error to report: the client waits, and the pane
        // keeps saying which address it is waiting on. The wait answers the server's
        // greeting, so what comes back is a link that is established.
        let connection = socket.accept().await?;
        // Whatever the session did is logged rather than drawn, since the pane above it is
        // a picture and text written into it is never seen. The log is the record; this
        // only makes sure the failure is in it.
        let outcome = draw_until_ended(
            &mut terminal,
            connection,
            announcement,
            &self.path,
            stdin,
            self.debug,
        )
        .await;
        if let Err(e) = &outcome {
            eprln!("gui: {e}");
        }
        outcome
    }
}

/// What the pane is tiled with once the session is up.
///
/// The *anchor*: this character is what the server replaces with image
/// placeholders, so the fill is not decoration, it is the address of the picture.
/// One character and not a sequence, because a fill is drawn a screen at a time
/// and anything longer would make the rewrite cost grow with the marker.
///
/// A teardrop-spoked asterisk: not box drawing or a block, so a pane full of it
/// cannot be mistaken for a border or a shadow, and not in any font's common set,
/// so it reads as deliberate.
const FILL: char = '❋';

/// Draw until the server closes, reporting what it sends.
///
/// The link is already established — the greeting was answered when the server dialed — so the
/// pane is green before the first message arrives. Then the pane is tiled with [`FILL`], which
/// is the *anchor*: it travels to the server as bytes, the server answers it with an image and
/// a virtual placement, and the terminal resolves the cells against them. Nothing this side
/// knows about the image except that it exists is needed.
async fn draw_until_ended(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<impl Write>>,
    connection: Connection,
    announcement: String,
    path: &std::path::Path,
    stdin: impl Read + Send + 'static,
    debug: DebugOptions,
) -> std::io::Result<()> {
    // Green, and named with what this client announced: the one thing the user is waiting to
    // see, and the proof that this pane is talking to the session it thinks.
    terminal.draw(|frame| {
        frame.render_widget(
            widget::Session::established(announcement),
            frame.area(),
        )
    })?;

    // Held before the fill, so the green is legible as a state rather than a flash on the
    // way to the pattern. This is the only delay in the client; everything after it is
    // driven by what arrives on the link.
    tokio::time::sleep(Duration::from_secs(1)).await;
    terminal.draw(|frame| frame.render_widget(widget::Session::filled(FILL), frame.area()))?;

    // Frames start only now: the fill has been drawn and forwarded, so the server has the
    // grid it needs to create the image under. A frame sent before that would name an image
    // the terminal does not have, and the frame would be lost.
    let mut frames = match Runnable::classify(path) {
        // A clip is decoded from the file.
        Runnable::Video => decode(path, debug.loop_video)?,
        // A program runs in a compositor of our own, and this client's keyboard goes to
        // it. `_stdin` is that keyboard: the pane's input is this process's, which is
        // where the caller's reader comes from.
        Runnable::App => compositor::capture(path, output_size()?, stdin)?,
    };

    loop {
        tokio::select! {
            // A decoded frame, on its way to the server as the wire codec's bytes.
            frame = frames.recv() => match frame {
                Some(jpeg) => connection.send(&Message::Frame(jpeg)).await?,
                // The runnable ended: a clip that finished, or a program that exited
                // (including one that could not start at all). The session ends with it,
                // because there is nothing left to draw and no reason to keep the pane
                // open. This has to be an explicit arm: a pattern of `Some(jpeg)` would
                // simply stop matching when the channel closed, leaving only the other
                // arm, and a server that sends nothing backs would leave the client
                // blocked forever with a frozen pane.
                None => return Ok(()),
            },
            received = connection.recv() => match received? {
                // The server reports what *this* client asked for and failed, so this side is
                // the one that should stop.
                Some(Message::Error(text)) => {
                    return Err(std::io::Error::other(format!("the server failed: {text}")));
                }
                Some(Message::Ping(_)) | Some(Message::Pong) | Some(Message::Frame(_)) => {}
                // The server closed. The session is over, which is not an error.
                None => return Ok(()),
            },
        }
    }
}

/// Decode `path` into JPEG frames, as they become available.
///
/// ffmpeg is a sidecar process and its output is blocking IO, so both it and the encoder live
/// on a thread of their own and hand frames over a channel. The thread is what keeps a
/// blocking read out of the runtime, and the channel is what paces it: one queued frame, so a
/// link that falls behind drops the frames nobody would have looked at rather than building a
/// delay between the video and the pane.
///
/// `loop_video` makes ffmpeg rewind the input rather than end, which is what keeps a session
/// alive after a short clip. The looping is ffmpeg's and not a loop here: it re-reads the file
/// and re-emits its frames at the same rate, so this side sees one continuous stream and
/// needs no state about where the clip began.
fn decode(
    path: &std::path::Path,
    loop_video: bool,
) -> std::io::Result<tokio::sync::mpsc::Receiver<Vec<u8>>> {
    use ffmpeg_sidecar::command::FfmpegCommand;

    let (tx, frames) = tokio::sync::mpsc::channel(1);
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("gui-video".into())
        .spawn(move || {
            let mut command = FfmpegCommand::new();
            // `-re` belongs before the input, which is where ffmpeg wants it: it paces the file
            // at its own frame rate instead of decoding all of it at once. Without it a short
            // clip is over before its first frame has been drawn.
            command.realtime();
            if loop_video {
                // Also before the input, and `-1` is ffmpeg's "forever": the flag is a
                // property of how the input is read, so it is no use after `-i`.
                command.args(["-stream_loop", "-1"]);
            }
            command.input(path.as_os_str());
            let mut child = match command.rawvideo().spawn() {
                Ok(child) => child,
                Err(e) => {
                    eprln!("gui: could not start ffmpeg on {}: {e}", path.display());
                    return;
                }
            };
            let events = match child.iter() {
                Ok(events) => events,
                Err(e) => {
                    eprln!("gui: could not read ffmpeg's output: {e}");
                    return;
                }
            };

            let mut encoder = match turbojpeg::Compressor::new() {
                Ok(encoder) => encoder,
                Err(e) => {
                    eprln!("gui: no JPEG encoder available: {e}");
                    return;
                }
            };
            if let Err(e) = encoder.set_quality(QUALITY) {
                eprln!("gui: the encoder refused quality {QUALITY}: {e}");
                return;
            }

            for frame in events.filter_frames() {
                let width = frame.width as usize;
                let height = frame.height as usize;
                let jpeg = encoder.compress_to_vec(turbojpeg::Image {
                    pixels: frame.data.as_slice(),
                    width,
                    // ffmpeg's rawvideo output is packed, so a row is exactly its pixels.
                    pitch: width * 3,
                    height,
                    format: turbojpeg::PixelFormat::RGB,
                });
                let jpeg = match jpeg {
                    Ok(jpeg) => jpeg,
                    Err(e) => {
                        eprln!("gui: dropped a {width}x{height} frame: {e}");
                        continue;
                    }
                };
                // Blocks while the link is behind, which is the backpressure: ffmpeg reads a
                // file, and a file read ahead of what can be sent is memory.
                if tx.blocking_send(jpeg).is_err() {
                    // The client stopped taking frames, so the session is gone. Stop decoding
                    // rather than filling memory with frames nobody will take.
                    let _ = child.quit();
                    return;
                }
            }
            let _ = child.quit();
        })
        .map_err(std::io::Error::other)?;
    Ok(frames)
}

/// JPEG quality. The pixels are on their way to a pane, where they are fitted to a grid of
/// cells, so this buys legibility rather than fidelity.
const QUALITY: i32 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// The extension is the whole decision, so this pins what counts as an extension.
    #[test]
    fn only_mp4_is_a_clip() {
        for clip in ["a.mp4", "a.MP4", "/tmp/b.Mp4", "./dir/c.mp4"] {
            assert_eq!(
                Runnable::classify(std::path::Path::new(clip)),
                Runnable::Video,
                "{clip} should be decoded as a clip"
            );
        }
        for app in [
            "/usr/bin/foot",
            "firefox",
            "a.mkv",
            "a.mp4.txt",
            "mp4",
            "a.mp",
            "/tmp/no-extension",
        ] {
            assert_eq!(
                Runnable::classify(std::path::Path::new(app)),
                Runnable::App,
                "{app} should be run as a program"
            );
        }
    }

    /// The extension is read the way `Path` reads it, trailing separators and all.
    ///
    /// Pinned because it is the one case where the *name* looks like a clip and the
    /// classification is not what a string split would give: `a.mp4/` names a directory
    /// (`Path::extension` drops the trailing slash), and it is classified as a clip,
    /// which is what a caller who wrote a directory named after a clip asked for.
    #[test]
    fn a_trailing_separator_does_not_hide_the_extension() {
        assert_eq!(
            Runnable::classify(std::path::Path::new("a.mp4/")),
            Runnable::Video
        );
    }
}
