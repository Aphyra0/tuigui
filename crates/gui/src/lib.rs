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

use wire::message::Message;
use wire::transport::{Connection, Transport};
use wire::unix::UnixSocket;

mod widget;

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
        _stdin: impl Read + Send + 'static,
        stdout: impl Write + Send + 'static,
    ) -> std::io::Result<()> {
        // Read from `self.path` here. Not written: what the runnable produces, and
        // how a frame of it reaches `tui`, is the next piece of work.
        let _ = &self.path;

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
        draw_until_ended(&mut terminal, connection, announcement).await
    }
}

/// Draw until the server closes, reporting what it sends.
///
/// Nothing draws frames yet, so a frame is reported rather than shown. The link is already
/// established — the greeting was answered when the server dialed — so the pane is green
/// before the first message arrives.
async fn draw_until_ended(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<impl Write>>,
    connection: Connection,
    announcement: String,
) -> std::io::Result<()> {
    // Green, and named with what this client announced: the one thing the user is waiting to
    // see, and the proof that this pane is talking to the session it thinks.
    terminal.draw(|frame| {
        frame.render_widget(
            widget::Session::established(announcement),
            frame.area(),
        )
    })?;

    loop {
        match connection.recv().await {
            Ok(Some(Message::Frame(body))) => {
                eprintln!("gui: dropped a {} byte frame (no display yet)", body.len());
            }
            Ok(Some(Message::Error(text))) => {
                // The server reports what *this* client asked for and failed, so this side is
                // the one that should stop.
                return Err(std::io::Error::other(format!("the server failed: {text}")));
            }
            Ok(Some(Message::Ping(_))) | Ok(Some(Message::Pong)) => {}
            // The server closed. The session is over, which is not an error.
            Ok(None) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
