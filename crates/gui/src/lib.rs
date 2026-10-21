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
    /// The diagnostic switches, which belong to the client.
    #[command(flatten)]
    pub debug: DebugOptions,
}

/// The switches that exist to diagnose a session, not to use one.
///
/// The client's half of the set: what the *sender* can do to make a failure visible. A
/// session has two ends and either can be the reason nothing appears, so each end has its
/// own switches and each stays with the code it changes.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct DebugOptions {
    /// Play the runnable again from the start when it ends, forever.
    ///
    /// A clip is short and a session is not, so a run that ends after a few seconds is hard
    /// to look at while something else is being examined, which is the usual case when one
    /// of these switches is needed at all.
    #[arg(long = "dbg-loop-video")]
    pub loop_video: bool,
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

        let mut session = Session::new(self.program, output, cell, listener);

        loop {
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
