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
pub use proxy::Streams;
pub use pty::Pty as Command;
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
    /// The diagnostic switches, shared with the client.
    #[command(flatten)]
    pub debug: DebugOptions,
}

/// The switches that exist to diagnose a session, not to use one.
///
/// Every one of these makes the session worse in some way, and every one exists
/// because a failure in this pipeline is invisible: an image that is never sent, a
/// placement nothing resolves against, a medium the terminal cannot read, and a
/// blank pane all look the same from outside. Each switch removes one layer, so
/// what remains on screen is attributable to the layers still in place.
///
/// They are all named `--dbg-`, and they are all the caller's to pass: a server
/// does not decide to diagnose itself.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct DebugOptions {
    /// Draw a fixed blue image instead of the frames that arrive.
    ///
    /// Removes the wire codec and the decode, so what is on screen is attributable
    /// to the encode, the transmit and the placement. Blue means those are sound and
    /// the fault is upstream; a blank pane means it is here.
    #[arg(long = "dbg-blue-pixel")]
    pub blue_pixel: bool,
    /// Place the image for real instead of creating a virtual placement.
    ///
    /// A virtual placement (`U=1`) has no position of its own, so it draws nothing
    /// until placeholder cells resolve against it — correct, and silent when they do
    /// not. A real placement draws at the cursor and needs nothing from the client's
    /// text, which is what separates "no image" from "no cells".
    #[arg(long = "dbg-no-vplacement")]
    pub no_vplacement: bool,
    /// Send the pixels inside the escape instead of through shared memory.
    ///
    /// The shared memory medium (`t=s`) names an object the terminal maps, which is
    /// the fast path and the reason this pipeline is affordable. The inline path
    /// puts a base64 PNG in the escape, so it is slower and larger — and it cannot
    /// fail on a terminal that will not read the object, which is what makes it the
    /// control for "the image arrived but nothing was drawn".
    #[arg(long = "dbg-no-shm")]
    pub no_shm: bool,
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
        let (cols, rows) = (pty.cols(), pty.rows());

        // The medium is chosen here and nowhere else: a flag read at every frame is the branch
        // the type parameter exists to remove, but the flag still has to be read somewhere, and
        // this is the one place that knows what the user asked for. The two arms are different
        // types, so each is boxed to one.
        let server: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>>>> = if self
            .debug
            .no_shm
        {
            Box::pin(
                Server::<easytgp::Png>::new(
                    output,
                    cols,
                    rows,
                    self.debug,
                    controller,
                    sink.clone(),
                )
                .run(),
            )
        } else {
            Box::pin(
                Server::<easytgp::Shm>::new(
                    output,
                    cols,
                    rows,
                    self.debug,
                    controller,
                    sink.clone(),
                )
                .run(),
            )
        };

        tokio::select! {
            // The command's half: its own select races the bytes against the child, so this
            // is one arm of the whole session.
            session = pty.run() => session,
            // Nothing is reading the pty's output any more, so no client can be found and
            // there is no session left to serve.
            served = server => served,
        }
    }
}

/// Puts this terminal in raw mode for the session; restores it on drop.
///
/// A re-export so the call site reads `Raw::install()` without a module path,
/// while the explanation stays next to the termios code. See [`raw::Raw`].
pub use raw::Raw;
