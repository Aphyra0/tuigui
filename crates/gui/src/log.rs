//! Where `gui`'s diagnostics go.
//!
//! The client paints the whole pane, so a line written to standard error while it is
//! drawing is covered by the next frame and never seen — which is when the line matters
//! most. It is appended to a file instead, at one fixed path, so the same file can always
//! be read afterwards.

use std::path::PathBuf;

/// The one file every diagnostic is appended to.
///
/// Appended and never truncated: a session's lines are not lost by the next session, and a
/// client that was killed outright still left its log behind.
pub fn path() -> PathBuf {
    let dir = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::Path::new(&home).join(".cache")
        })
        .join("tuigui");
    dir.join("gui.log")
}

/// Write one line to the log, formatted like `eprintln!`.
///
/// A failure to write is dropped rather than reported: the only place to report it would
/// be the terminal this exists to avoid, and a diagnostic that can fail must not be able
/// to stop what it is describing.
macro_rules! eprln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let line = format!($($arg)*);
        let path = $crate::log::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{line}");
        }
    }};
}
