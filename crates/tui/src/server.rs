//! The image server: a unix socket a client connects to and sends frames on.

use std::io;
use std::os::unix::net::UnixListener as StdUnixListener;

use tokio::net::{UnixListener, UnixStream};

use tuigui_wire::{p2p, session};

use crate::sink::Sink;

/// The socket a client sends frames to, and the session file describing it.
///
/// What a client sends is read and discarded for now: the point of the server
/// existing at this stage is the connection, the session file, and the framing,
/// so a client can be run against it end to end while the drawing is left for
/// later.
pub struct Server {
    /// The one writer to the screen, shared with the pty's proxy.
    ///
    /// Held but not yet written to: once frames are drawn they go out through
    /// this, which is what keeps a frame from landing in the middle of the
    /// command's output.
    sink: Sink,
    listener: StdUnixListener,
}

impl Server {
    /// Bind `socket_path` and publish the session file. Does not accept yet.
    ///
    /// Every failure returns before a client could connect, so a caller never
    /// gets a server that looks started and is not.
    ///
    /// `cols` and `rows` are published in the session file: the client reads its
    /// grid size from there rather than from the environment, which a muxer's
    /// panes do not inherit.
    pub fn new(
        sink: Sink,
        socket_path: &str,
        cols: u32,
        rows: u32,
        session_path: Option<std::path::PathBuf>,
    ) -> io::Result<Self> {
        match std::fs::remove_file(socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("removing stale {socket_path}: {e}"),
                ))
            }
        }
        let listener = StdUnixListener::bind(socket_path)?;

        let file = session::Session {
            socket: socket_path.to_string(),
            image_id: 0,
            cols,
            rows,
        };
        // A caller that named no path takes the shared default, which is not a
        // fallback for a failed read: there is nothing to read, so there is
        // nothing to conceal.
        let path = session_path.unwrap_or_else(session::path);
        file.write(&path)?;
        Ok(Self { sink, listener })
    }

    /// Accept clients until cancelled.
    ///
    /// Cancellation is the caller's: this future is dropped by whoever is racing
    /// it, which closes the listener, so there is no stop flag to check.
    pub async fn run(self) -> io::Result<()> {
        // Not written yet, but the field is the reason a second writer to the
        // screen cannot exist: once frames are drawn they go through here.
        let _sink = self.sink;
        let listener = UnixListener::from_std(self.listener)?;
        loop {
            let (stream, _) = listener.accept().await?;
            // Each client on its own task: a stalled one must not stop the next
            // from connecting.
            tokio::spawn(async move {
                if let Err(e) = drain(stream).await {
                    eprintln!("tui: client ended: {e}");
                }
            });
        }
    }
}

/// Read messages from one client until it disconnects.
async fn drain(mut stream: UnixStream) -> io::Result<()> {
    loop {
        match p2p::Msg::read_async(&mut stream).await? {
            Some(p2p::Msg::Hello { cols, rows, .. }) => {
                eprintln!("tui: client connected, grid {cols}x{rows}");
            }
            Some(p2p::Msg::Frame { width, height, .. }) => {
                eprintln!("tui: dropped a {width}x{height} frame (no display yet)");
            }
            Some(_) => {}
            // A disconnect is the normal end of a client, not a failure.
            None => return Ok(()),
        }
    }
}
