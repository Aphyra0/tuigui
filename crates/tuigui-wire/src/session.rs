//! Session file: how `gui` learns where to send frames without relying on
//! environment variables.
//!
//! Environment does not survive a multiplexer's server boundary: a pane spawned
//! by an already-running herdr server inherits the *server's* environment, not
//! the `tui`-wrapped client's. A file at a well-known path does, for the same
//! reason `SSH_AUTH_SOCK` is a path and not a pipe.
//!
//! Format is one `key=value` per line, so it is trivially inspectable and can
//! be provisioned by hand on a remote machine, where the server cannot write it.
//!
//! `tui` writes this and `gui` reads it, so each binary sees the other's half
//! as dead code. That is expected here, not a missing call site.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

/// Default path. `/tmp` because a session is machine-scoped and short-lived.
pub const DEFAULT_PATH: &str = "/tmp/tuigui/session";

pub fn path() -> PathBuf {
    std::env::var("TUI_SESSION").unwrap_or_else(|_| DEFAULT_PATH.into()).into()
}

/// What the server publishes and the client reads.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// Unix socket to connect to.
    pub socket: String,
    /// The image the placeholders must reference.
    pub image_id: u32,
    /// Reserved grid, in cells.
    pub cols: u32,
    pub rows: u32,
}

impl Session {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = format!(
            "socket={}\nimage_id={}\ncols={}\nrows={}\n",
            self.socket, self.image_id, self.cols, self.rows
        );
        // Write then rename: a client reading during startup must never see a
        // half-written file.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let body = std::fs::read_to_string(path)?;
        let mut s = Session::default();
        for line in body.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "socket" => s.socket = v.trim().to_string(),
                "image_id" => s.image_id = v.trim().parse().unwrap_or_default(),
                "cols" => s.cols = v.trim().parse().unwrap_or_default(),
                "rows" => s.rows = v.trim().parse().unwrap_or_default(),
                _ => {}
            }
        }
        Ok(s)
    }
}
