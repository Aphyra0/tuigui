//! `gui` — the client of the remote-streaming experiment.
//!
//! Usage (inside a pane spawned by `tui`):
//!   gui <program>           # be found by the server, and draw into its pane
//!
//! The client owns the pane's screen and draws it. What it draws is a widget painted on
//! `stdout`, which `tui` is forwarding: this side never writes the terminal directly, so
//! nothing it does can disturb the width `tui` measures for the pane.
//!
//! The program's *pixels* travel out of band: `tui` paints them onto the real terminal,
//! which is the split that lets a multiplexer show an image it never received. What this
//! side sends is the encoded frames, and the pane is tiled with the anchor that gives them
//! somewhere to be drawn.
//!
//! # The session is a state machine, and the drawing drives it
//!
//! Choosing a socket, dialing it, starting the program and streaming it are the states in
//! [`widget::State`]. Each one is a step that has to await, and each one is also what gets
//! drawn, so the step is driven from the draw: one frame is one step. See
//! [`widget::Session`].

use std::io::{Read, Write};

use codecs::CodecId;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use widget::{GuiWidget, Session};

mod compositor;
mod widget;

/// A program to stream into this pane.
#[derive(Debug, Clone, clap::Parser)]
#[command(
    name = "gui",
    about = "Draw a program's output into the pane `tui` is serving"
)]
pub struct Gui {
    /// The program to run and draw.
    pub program: std::path::PathBuf,
    /// The codec the session streams with.
    #[command(flatten)]
    pub streaming: StreamingOptions,
    /// The diagnostic switches, which belong to the client.
    #[command(flatten)]
    pub debug: DebugOptions,
}

/// The codec a session streams with: the one choice a session is used with.
///
/// JPEG is the default, because a JPEG is a picture of least commitment: no
/// stream history, no temporal compression, and every terminal grid can show
/// one. The two flags are mutually exclusive, and neither given means JPEG.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct StreamingOptions {
    /// Stream JPEG: one complete picture per frame (Default).
    #[arg(long, conflicts_with = "h264")]
    pub jpeg: bool,
    /// Stream H.264: temporally compressed, at the cost of stream history.
    #[arg(long, conflicts_with = "jpeg")]
    pub h264: bool,
}

impl StreamingOptions {
    /// The codec this session streams with.
    pub fn codec(&self) -> CodecId {
        match self.h264 {
            true => CodecId::H264,
            false => CodecId::Jpeg,
        }
    }
}

/// The switches that exist to diagnose a session, not to use one.
///
/// The client's half of the set: what the *sender* can do to make a failure visible. A
/// session has two ends and either can be the reason nothing appears, so each end has its
/// own switches and each stays with the code it changes.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
// A group id of its own: `tuigui` flattens this beside `tui`'s own DebugOptions,
// and clap names a flattened group after the struct — two of them called the
// same thing is a command that refuses to build at all.
#[group(id = "gui-debug")]
pub struct DebugOptions {
    /// Play the runnable again from the start when it ends, forever.
    ///
    /// A clip is short and a session is not, so a run that ends after a few seconds is hard
    /// to look at while something else is being examined, which is the usual case when one
    /// of these switches is needed at all.
    #[arg(long = "dbg-loop-video")]
    pub loop_video: bool,
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
    #[arg(long = "dbg-trace-gui")]
    pub trace_gui: bool,
}

impl Gui {
    /// Run the session until it ends.
    ///
    /// `stdin` and `stdout` are taken as arguments rather than read from the process: they
    /// are this client's whole interface to the outside world, and passing them in is what
    /// lets the session be driven by a caller that is not a terminal (`tuigui`, or a
    /// test). The caller owns its own runtime, so this is `async` with no runtime of its
    /// own.
    pub async fn run(
        self,
        stdin: impl Read + std::os::fd::AsRawFd + Send + 'static,
        stdout: impl Write + Send + 'static,
    ) -> std::io::Result<()> {
        // Before anything can fail, so the first failure is in the log rather than missing
        // from it.
        let log_path = install_logger();
        #[cfg(feature = "tracy")]
        if self.debug.trace_gui {
            install_tracing()?;
        }
        log::info!("gui {} starting, log at {}", self.program.display(), log_path.display());

        // Drawn to the writer the caller gave, not to `io::stdout()`: the caller is what
        // knows where this pane's screen is, and writing to the process's own stdout would
        // ignore it.
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // The output size is the terminal's own, read once: the compositor's output is the
        // program's window, and it does not change while the session runs.
        let Size { output, cell } = output_size()?;

        // The terminal is asked for its mouse and keyboard events before the
        // session starts, so nothing typed while it connects is lost.
        //
        // This end owns its own terminal, and its line discipline has to go: with
        // `ECHO` on the kernel writes every keystroke straight back out, and with
        // `ICANON` on nothing is delivered until a newline. Both are invisible to
        // this process until it fixes them, and both are the difference between
        // reading keys and reading the echo of keys.
        // Held for the whole session: dropping it restores the terminal, which is
        // what the name and the lack of an underscore say.
        let _tty = Raw::install()?;
        let listener = termputs::Listener::new(
            Box::new(stdin) as widget::Input,
            Box::new(std::io::stdout()) as widget::Output,
        )?;

        let mut session = Session::new(self.program, output, cell, listener, self.streaming.codec());

        loop {
            // The frame's span is on `GuiWidget::render`, which is what one call
            // to `draw` actually is.
            terminal.draw(|frame| {
                frame.render_stateful_widget(GuiWidget, frame.area(), &mut session)
            })?;
            if session.done() {
                return Ok(());
            }
        }
    }
}

/// The size of this terminal, in output pixels and in one cell.
///
/// Taken from the terminal itself: the pane's geometry is what the picture is fitted to, so
/// an output sized to it is one the program lays itself out against correctly. The cell size
/// is kept as well, because a mouse event arrives in cells and the compositor's pointer is
/// in pixels, so a click has to be converted with exactly the cell this derived.
///
/// The pixel fields of a terminal's window size are documented as unreliable and often zero
/// — they are a property of the graphics layer, not the tty. A terminal that does not report
/// them is therefore not an error, and the cell size is assumed instead. Returning the
/// reported pixels directly would be `0x0` in that case, and a zero-sized output is not a
/// small window: it is no surface at all, so a program started into it never commits a
/// buffer and never says why.
fn output_size() -> std::io::Result<Size> {
    let ws = ratatui::crossterm::terminal::window_size()?;
    let (cols, rows) = (ws.columns as u32, ws.rows as u32);
    let (cell_w, cell_h) = match (ws.width, ws.height) {
        (w, h) if w > 0 && h > 0 => (
            (w as u32 / cols.max(1)).max(1),
            (h as u32 / rows.max(1)).max(1),
        ),
        _ => DEFAULT_CELL,
    };
    let output = (cols.max(1) * cell_w, rows.max(1) * cell_h);
    log::info!(
        "the terminal reports {cols}x{rows} cells, {}x{} pixels -> cell {cell_w}x{cell_h}, output {}x{}",
        ws.width,
        ws.height,
        output.0,
        output.1
    );
    Ok(Size {
        output,
        cell: (cell_w, cell_h),
    })
}

/// The terminal's size: the output in pixels, and one cell in pixels.
struct Size {
    /// The whole pane, in pixels, which is the compositor's output.
    output: (u32, u32),
    /// One cell, in pixels: how a mouse position in cells becomes a pointer in
    /// pixels.
    cell: (u32, u32),
}

/// Holds this terminal in raw mode, and restores the old settings when dropped.
///
/// Raw mode is not optional for a program that reads keys: with the line
/// discipline on, every keystroke is echoed by the kernel, keys are held until a
/// newline, and `Ctrl+C` never arrives as a byte because `ISIG` turns it into a
/// signal for this process. The session is a client, so it must be the one that
/// sees the bytes.
struct Raw {
    original: rustix::termios::Termios,
}

impl Raw {
    /// Put this process's stdin in raw mode, and put it back on drop.
    fn install() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let original = rustix::termios::tcgetattr(&stdin).map_err(std::io::Error::from)?;
        let mut raw = original.clone();
        raw.make_raw();
        // `make_raw` clears `ISIG`, which turns `^C` from a signal into a byte.
        // It is put back so `^C` still interrupts this process: the session has
        // no other way out, and a client that cannot be stopped is worse than one
        // that cannot forward an interrupt.
        raw.local_modes.insert(rustix::termios::LocalModes::ISIG);
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)
            .map_err(std::io::Error::from)?;
        Ok(Self { original })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}

/// The cell size assumed when the terminal does not report one, in pixels.
///
/// Standard for a monospace face at the sizes terminals ship with, and it only has to be
/// close: the compositor's output is what the program lays out against, and the terminal
/// fits the resulting picture to the same cell grid either way.
const DEFAULT_CELL: (u32, u32) = (10, 20);

/// Install the capturer every `log` record goes to, and return where it writes.
///
/// At the bottom of this file because it is small, and here rather than in a module
/// because there is one of it.
fn install_logger() -> std::path::PathBuf {
    // One fixed path, so it is the same file every run and can simply be read afterwards.
    // A path per process would mean knowing a pid before the log could be found, which is
    // exactly what is not known when the session is the thing that failed.
    let path = {
        let dir = std::env::var("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                std::path::Path::new(&home).join(".cache")
            })
            .join("tuigui");
        dir.join("gui.log")
    };

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    // A log that cannot be opened is not a reason to refuse the session: the pane is the
    // job, and the log is a diagnostic for when the job goes wrong.
    if let Some(file) = file {
        static CAPTURER: std::sync::OnceLock<Capturer> = std::sync::OnceLock::new();
        let capturer = CAPTURER.get_or_init(|| Capturer {
            file: std::sync::Mutex::new(file),
        });
        let _ = log::set_logger(capturer);
        log::set_max_level(log::LevelFilter::Trace);
    }
    path
}

/// Send `tracing`'s spans to the Tracy profiler, so a session can be looked at
/// rather than guessed at.
///
/// The client is the right half to hold it: it is the process that runs the
/// compositor, and a frame is a capture here, an encode here, and a round trip to
/// the server — one timeline with all three on it is the whole point.
///
/// Cheap enough to be unconditional. Tracy's client records into a lock-free
/// ring and a background thread drains it to the profiler, so a span costs a few
/// nanoseconds and nothing at all when no profiler is attached. A switch would
/// have to be set before the interesting session started, which is the one that
/// is not already being profiled.
/// Send `tracing`'s spans to the Tracy profiler, so a session can be looked at
/// rather than guessed at.
///
/// The client is the right half to hold it: it is the process that runs the
/// compositor, and a frame is a capture here, an encode here, and a round trip
/// to the server — one timeline with all three on it is the whole point.
#[cfg(feature = "tracy")]
fn install_tracing() -> std::io::Result<()> {
    use tracing::Level;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt as _;

    // This workspace's crates, and nothing else. A dependency's spans are a wall
    // of zones that answers none of the questions a frame is profiled for, and
    // there are far more of them than there are of ours — enough to bury the
    // picture. Filtered here rather than hidden in the profiler's own UI, so a
    // trace is clean wherever it is opened and however many processes are
    // writing to it.
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
    // `set_global_default` and **not** `try_init`. `try_init` also installs a
    // bridge that takes over the `log` facade, and this process has already
    // given `log` its own logger — the file every other diagnostic goes to. The
    // bridge refuses to replace it, `try_init` reports that as its own failure,
    // and the tracing subscriber is never installed at all. That is the whole
    // shape of "the profiler connects and there is nothing there".
    //
    // A second call fails here too, which is right: one subscriber per process.
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(ours).with(layer))
    .map_err(|e| {
        log::error!("could not install the profiler: {e}");
        std::io::Error::other(format!("could not install the profiler: {e}"))
    })?;

    // One span, guaranteed, before anything else is instrumented. It answers "is
    // anything reaching the profiler at all", which is the first question when a
    // timeline comes back empty.
    tracing::info!(target: "gui", "profiler installed");
    // And in the file, so the question can be answered without a profiler at all:
    // `gui.log` either says this or says why not.
    log::info!("profiler installed; spans are being sent");
    Ok(())
}

/// The one logger: every record is appended to the log file.
///
/// Appended and never truncated, so a session's lines are not lost by the next session and
/// a client that was killed outright still left its log behind.
struct Capturer {
    file: std::sync::Mutex<std::fs::File>,
}

impl log::Log for Capturer {
    fn enabled(&self, _: &log::Metadata) -> bool {
        // Everything. This is one pane's log, and a filter that dropped levels would drop
        // the lines nobody thought to raise.
        true
    }

    fn log(&self, record: &log::Record) {
        if let Ok(mut file) = self.file.lock() {
            // A failure to write is dropped: a diagnostic must not be able to stop what it
            // is describing, and the only place to report it is this file.
            let _ = writeln!(
                file,
                "{}: {}",
                record.level().as_str().to_lowercase(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}
