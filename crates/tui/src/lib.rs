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
mod placeholder;
mod proxy;
mod pty;
mod raw;
mod server;
mod session;
mod sink;

use std::io::{self, Read, Write};

use pty::Pty;
use server::Server;
use sink::Sink;

pub use bytes::Bytes;
pub use pty::Pty as Command;
pub use proxy::Streams;
pub use server::Server as ImageServer;
pub use sink::Sink as Screen;

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

        // The keyboard, split in two on its way in. The terminal's answers to this side's
        // graphics escapes arrive on the same descriptor as the user's keystrokes, so the
        // router reads it and gives back only what was not a reply; that pipe is then the
        // keyboard the command sees. Without this the command would be typed the terminal's
        // own acknowledgements.
        //
        // The sink is the escape destination as well as the screen: one lock covers both the
        // command's bytes and this side's escapes, which is what stops an escape landing in
        // the middle of a line the command is printing.
        let (keyboard, replies) = io::pipe()?;
        let controller = easytgp::Controller::init_with_sink(
            stdin,
            replies,
            std::sync::Arc::new(std::sync::Mutex::new(sink.clone())),
        );

        // The pty next, because the server is built around what the pty produces: the pty
        // is the producer and the server reads it. There is no socket to bind and no session
        // file to write — a client names its own channel, and this side only answers.
        let pty = Pty::new(&self.command, keyboard, sink.clone())?;

        // One subscription, read by the server's own loop. Reading it there rather than on
        // the pty's forwarding thread is what lets the same loop wait on the clients too: a
        // scan on that thread would have to hand its finds over from it, and the finds would
        // be a third thing to select on.
        let output = pty.bytes().subscribe();
        // The pty's grid, which is the pane a client draws into: a client cannot read it from
        // its own environment, so the side that made the pty is the side that tells it.
        let server = Server::new(output, pty.cols(), pty.rows(), controller);

        tokio::select! {
            // The command's half: its own select races the bytes against the child, so this
            // is one arm of the whole session.
            session = pty.run() => session,
            // Nothing is reading the pty's output any more, so no client can be found and
            // there is no session left to serve.
            served = server.run() => served,
        }
    }
}

/// Puts this terminal in raw mode for the session; restores it on drop.
///
/// A re-export so the call site reads `Raw::install()` without a module path,
/// while the explanation stays next to the termios code. See [`raw::Raw`].
pub use raw::Raw;
