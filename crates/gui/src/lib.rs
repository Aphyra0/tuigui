//! `gui` — the client of the remote-streaming experiment.
//!
//! Usage (inside a pane spawned by `tui`):
//!   gui <runnable>          # attach to the session `tui` published
//!
//! The client owns the pane's screen and draws it. What it draws is a widget
//! painted on `stdout`, which `tui` is forwarding: this side never writes the
//! terminal directly, so nothing it does can disturb the width `tui` measures
//! for the pane.
//!
//! The runnable's *pixels* travel out of band: `tui` paints them onto the real
//! terminal, which is the split that lets a multiplexer show an image it never
//! received. Reading those pixels is `Gui::run`'s job and is not written yet.

use std::io::{Read, Write};

mod widget;

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
}

impl Gui {
    /// Attach to the session and draw until it ends.
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
        // Read from `self.path` here. Not written: what the runnable produces,
        // and how a frame of it reaches `tui`, is the next piece of work.
        let _ = &self.path;

        // Drawn to the writer the caller gave, not to `io::stdout()`: the caller
        // is what knows where this pane's screen is, and writing to the process's
        // own stdout would ignore it.
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend)?;
        loop {
            terminal.draw(|frame| frame.render_widget(widget::Pink, frame.area()))?;
            tokio::task::yield_now().await;
        }
    }
}
