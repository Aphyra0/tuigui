//! `tui` — run a command in a pty we own and forward its bytes to this terminal.
//!
//! ```text
//!   this terminal  <── Pty ──▶  the command's pty  ──▶  the command
//!                                    │
//!                                    └── a view of the output ──▶ Server: dials a client
//! ```
//!
//! The command is a normal terminal program: it is given a pty, and everything it
//! reads and writes is forwarded unchanged. A client that wants to draw into this
//! pane cannot be reached before it says how, so it announces a channel of its own
//! in that output, and the server — which is already watching every byte — dials
//! it.
//!
//! The pty hands out a **view** of its output rather than being given a place to
//! send it: the producer does not know about its consumers, so adding one is not a
//! change to the pty.
//!
//! Both channels write the screen through one [`Sink`], which is what stops a
//! frame from landing in the middle of the command's output.

mod announce;
mod bytes;
mod proxy;
mod pty;
mod raw;
mod server;
mod session;
mod sink;
mod transport;

use std::io::{self, Read, Write};

use pty::Pty;
use server::Server;
use sink::Sink;

pub use bytes::Bytes;
pub use pty::Pty as Command;
pub use proxy::Streams;
pub use server::Server as ImageServer;
pub use session::{Session, Sessions};
pub use sink::Sink as Screen;
pub use transport::InfluxByPath;

// A session, as the command line describes it. `run` is where it starts.
//
// `command` is trailing and required: `tui` with nothing to run has no meaning,
// and clap rejects it rather than starting a server around an empty command.
// `allow_hyphen_values` is what lets the wrapped command's own flags (`-l`,
// `--color`) through instead of being read as `tui`'s.
#[derive(Debug, Clone, clap::Parser)]
#[command(
    name = "tui",
    about = "Run a command in a pty and forward it to this terminal"
)]
pub struct Tui {
    /// The command to run, with its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

impl Tui {
    /// Start both halves until either ends.
    ///
    /// Both are waited on together, and whichever ends first ends the other: the
    /// command's child is `kill_on_drop`, so dropping its half kills it, and the
    /// server's future being dropped stops it accepting clients. A session where
    /// one half is gone has no reader for the other, so leaving the other running
    /// would be a process with nobody watching it.
    ///
    /// Async, with the runtime owned by the caller: a binary gets one from
    /// `#[tokio::main]`, and a test or an embedder can use whatever it already has
    /// rather than being made to build a second one.
    pub async fn run(
        self,
        stdin: impl Read + Send + 'static,
        stdout: impl Write + Send + Sync + 'static,
    ) -> io::Result<()> {
        let _raw = Raw::install()?;
        let sink = Sink::new(stdout);

        // The pty first, because the server is built around what the pty produces:
        // the pty is the producer and the server subscribes to it. There is no
        // socket to bind and no session file to write — a client names its own
        // channel, and this side only answers.
        let pty = Pty::new(&self.command, stdin, sink.clone())?;

        // The scan runs on the pty's forwarding thread, so what it finds is
        // offered on a blocking channel rather than awaited: that thread has no
        // runtime to await on, and must not wait for one.
        let (found_tx, found_rx) = std::sync::mpsc::channel();
        let mut announcement = announce::Announcement::new(found_tx);
        pty.bytes().subscribe(move |chunk| announcement.feed(chunk));

        let server = Server::new(sink, found_rx);

        tokio::select! {
            // The command's half: its own select races the bytes against the
            // child, so this is one arm of the whole session.
            session = pty.run() => session,
            // Nothing is watching the pty's output any more, so no client can be
            // found and there is no session left to serve.
            served = server.run() => served,
        }
    }
}

/// Puts this terminal in raw mode for the session; restores it on drop.
///
/// A re-export so the call site reads `Raw::install()` without a module path,
/// while the explanation stays next to the termios code. See [`raw::Raw`].
pub use raw::Raw;
