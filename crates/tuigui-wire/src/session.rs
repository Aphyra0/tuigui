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
    /// How the client should draw its grid cells.
    ///
    /// Written by the server, which is the side that knows whether anything
    /// re-renders the pane's bytes: only then can a marker be rewritten into a
    /// placeholder.
    pub grid: GridMode,
}

/// Which cell a client draws, decided by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridMode {
    /// A real placeholder cell, for a client drawing straight to the terminal.
    ///
    /// The image id goes in the cell's foreground color (see `p2p::id_to_color`)
    /// and the terminal resolves it with no help from us.
    #[default]
    Placeholder,
    /// A magenta `p2p::MARKER` cell, for a client whose bytes reach the terminal
    /// through a multiplexer.
    ///
    /// The id cannot travel in the color: a muxer parses the pane into cells and
    /// re-renders them, which loses the color and blanks a placeholder cell. The
    /// wrapper instead rewrites each marker into a placeholder on the way out,
    /// after the muxer has had its turn.
    Marker,
}

impl Session {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = format!(
            "socket={}\nimage_id={}\ncols={}\nrows={}\ngrid={}\n",
            self.socket,
            self.image_id,
            self.cols,
            self.rows,
            match self.grid {
                GridMode::Placeholder => "placeholder",
                GridMode::Marker => "marker",
            }
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
                // An unknown value keeps the default rather than guessing: the
                // wrong choice draws a grid that silently shows nothing.
                "grid" => {
                    s.grid = match v.trim() {
                        "marker" => GridMode::Marker,
                        _ => GridMode::Placeholder,
                    }
                }
                _ => {}
            }
        }
        Ok(s)
    }
}
