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
mod error;
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
pub use error::{Error, Result};
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
#[derive(Debug, Clone, Default, clap::Args)]
// A group id of its own: `tuigui` flattens this beside `gui`'s own DebugOptions,
// and clap names a flattened group after the struct — two of them called the
// same thing is a command that refuses to build at all.
#[group(id = "tui-debug")]
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
    /// Mark the cells the surface mask fires on, instead of blanking them.
    ///
    /// The mask exists to blank the cells that carry a window's signature, so the
    /// picture drawn behind them is visible. When nothing appears, the two possible
    /// faults are "the mask never matched" and "the image never arrived", and both look
    /// like an empty pane. A pink `*` on a default background is the same edit made
    /// visible: pink cells mean the mask works and the picture is the problem.
    #[arg(long = "dbg-pink-placeholders")]
    pub pink_placeholders: bool,
    /// Dial a client at this path instead of waiting to be announced to.
    ///
    /// The session's normal shape is the client announcing an address that only it
    /// can name, because it is the half that cannot know whether anyone is
    /// listening. That breaks when the announcement does not survive the trip -- a
    /// multiplexer that re-renders the pane, a pty whose output is not this
    /// process's to read -- and the client is then listening on a socket nobody
    /// hears about.
    ///
    /// This is the other direction: the server dials instead, on a path both ends
    /// know without telling each other. Bare, it uses the address a local session's
    /// client always binds; with a path it dials that. Only useful for a session on
    /// one machine, and only as a diagnostic, because it assumes an address rather
    /// than learning one.
    ///
    /// The path is written with `=`, as in `--dbg-dial-unix=/tmp/other.sock`. The
    /// value is optional and the command follows the flag, so a space-separated
    /// value would be read as the command instead.
    #[arg(
        long = "dbg-dial-unix",
        value_name = "PATH",
        default_missing_value = "/run/user/1000/tuigui.sock",
        num_args = 0..=1,
        require_equals = true
    )]
    pub dial_unix: Option<std::path::PathBuf>,
    /// Profile this half of the session, so a frame can be looked at rather than
    /// guessed at.
    ///
    /// Turns on the Tracy client, which is what turns this crate's spans into
    /// zones the profiler can show. Off unless it is asked for: the client is a
    /// compiled library with a background thread of its own, and a session
    /// nobody is looking at should not carry it.
    ///
    /// Present only in a build with the `tracy` feature — the client is a
    /// compiled library, so a build without one has nothing to turn on and offers
    /// no way to ask for one either.
    #[cfg(feature = "tracy")]
    #[arg(long = "dbg-trace-tui")]
    pub trace_tui: bool,
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
        // Before anything else can fail. The pane is this process's screen, so a
        // diagnostic written to stdout would land in the middle of it; a file is the only
        // place left. Installed unconditionally, because the failures this exists for are
        // the ones nobody asked to reproduce.
        install_logger();
        #[cfg(feature = "tracy")]
        if self.debug.trace_tui {
            install_tracing()?;
        }
        let _raw = Raw::install()?;
        let sink = Sink::new(stdout);

        // One set of switches, shared: the pty's proxy reads them on its forwarding
        // thread while the server and every session read them from the loop, and a copy
        // per reader would be several answers to one question.
        let debug = std::sync::Arc::new(self.debug);

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

        // The two things the pty and the server share, made before either: the pty's
        // proxy feeds the screen and blanks the registered surfaces, and the server
        // reads the screen and has its sessions register surfaces into the list.
        //
        // Made here rather than in the server because the pty is built first, and a
        // screen that the proxy could not see would be a screen that never updates.
        let ws = rustix::termios::tcgetwinsize(std::io::stdin())?;
        let screen = std::sync::Arc::new(std::sync::RwLock::new(vt100::Parser::new(
            std::num::NonZeroU16::new(ws.ws_row).expect("a terminal needs a row"),
            std::num::NonZeroU16::new(ws.ws_col).expect("a terminal needs a column"),
            0,
        )));
        let surfaces = std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));

        // The pty next, because the server is built around what the pty produces: the pty
        // is the producer and the server reads it. There is no socket to bind and no session
        // file to write — a client names its own channel, and this side only answers.
        let pty = Pty::new(
            &self.command,
            keyboard,
            sink.clone(),
            debug.clone(),
            screen.clone(),
            surfaces.clone(),
        )?;

        // One subscription, read by the server's own loop. Reading it there rather than on
        // the pty's forwarding thread is what lets the same loop wait on the clients too: a
        // scan on that thread would have to hand its finds over from it, and the finds would
        // be a third thing to select on.
        let output = pty.bytes().subscribe();

        // The medium is chosen here and nowhere else: a flag read at every frame is the branch
        // the type parameter exists to remove, but the flag still has to be read somewhere, and
        // this is the one place that knows what the user asked for. The two arms are different
        // types, so each is boxed to one.
        let server: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>>>> = if debug.no_shm
        {
            Box::pin(
                Server::<easytgp::Png>::new(
                    output,
                    debug.clone(),
                    controller,
                    sink.clone(),
                    screen,
                    surfaces,
                )
                .run(),
            )
        } else {
            Box::pin(
                Server::<easytgp::Shm>::new(
                    output,
                    debug,
                    controller,
                    sink.clone(),
                    screen,
                    surfaces,
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

/// Send `tracing`'s spans to the Tracy profiler, so a session can be looked at
/// rather than guessed at.
///
/// The server is the right half to hold it: this is the process that decodes a
/// frame, transmits it, and waits on the terminal's acknowledgement — three
/// different waits that look identical from outside, which is what the profiler
/// is for.
///
/// Cheap enough to be unconditional. Tracy's client records into a lock-free
/// ring and a background thread drains it to the profiler, so a span costs a few
/// nanoseconds and nothing at all when no profiler is attached. A switch would
/// have to be set before the interesting session started, which is the one that
/// is not already being profiled.
///
/// `tuigui` runs this half in its own process, so installing it here covers both
/// commands; the client half installs its own, because it is a separate process
/// inside the pty.
/// Send `tracing`'s spans to the Tracy profiler, so a session can be looked at
/// rather than guessed at.
/// Send every `log` record to `/tmp/tuigui.log`.
///
/// The path is fixed rather than per process, so the log of the run that just failed is
/// the one that is read. A single line per record: this is a file to grep, not to read.
fn install_logger() {
    struct File(log::LevelFilter);

    impl log::Log for File {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= self.0
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            // Opened per record and appended: a session is long and a file held open
            // across it would be one more thing to get wrong on the way out. The cost is
            // one open per line, which these diagnostics can afford.
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/tuigui.log")
            {
                let _ = writeln!(
                    file,
                    "[{}] {}: {}",
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }

        fn flush(&self) {}
    }

    // A logger already installed wins: one capturer per process, and the second call is
    // a caller that has nothing to say.
    let _ = log::set_boxed_logger(Box::new(File(log::LevelFilter::Debug)));
    log::set_max_level(log::LevelFilter::Debug);
}

///
/// The server is the right half to hold it: this is the process that decodes a
/// frame, transmits it, and waits on the terminal's acknowledgement — three
/// different waits that look identical from outside, which is what the profiler
/// is for.
#[cfg(feature = "tracy")]
fn install_tracing() -> io::Result<()> {
    use tracing::Level;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt as _;

    // This workspace's crates, and nothing else. A dependency's spans are a wall
    // of zones that answers none of the questions a frame is profiled for, and
    // there are far more of them than there are of ours. Filtered here rather
    // than hidden in the profiler's own UI, so a trace is clean wherever it is
    // opened and however many processes are writing to it.
    let ours = Targets::new()
        .with_target("compositor", Level::TRACE)
        .with_target("gui", Level::TRACE)
        .with_target("tui", Level::TRACE)
        .with_target("tuigui", Level::TRACE)
        .with_target("wire", Level::TRACE)
        .with_target("termputs", Level::TRACE);

    let layer = tracing_tracy::TracyLayer::default();

    // Reported, and not dropped. The client is built by `TracyLayer::default()`,
    // so it starts listening *before* this returns: a profiler connects and then
    // shows an empty timeline whenever no subscriber ever reached the spans.
    //
    // `set_global_default` and **not** `try_init`: `try_init` also installs a
    // bridge that takes over the `log` facade, and a caller may already have
    // given `log` its own logger. The bridge refuses to replace it, `try_init`
    // reports that as its own failure, and the tracing subscriber is never
    // installed at all — the whole shape of "the profiler connects and there is
    // nothing there".
    //
    // A second call fails here too, which is right: one subscriber per process.
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(ours).with(layer))
        .map_err(|e| io::Error::other(format!("could not install the profiler: {e}")))?;

    // One span, guaranteed, before anything else is instrumented. It answers "is
    // anything reaching the profiler at all", which is the first question when a
    // timeline comes back empty.
    tracing::info!(target: "tui", "profiler installed");
    Ok(())
}
