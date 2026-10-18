//! `tuigui` — run a runnable here, locally, in one command.
//!
//! It starts the server (a pty plus the TGP plumbing that owns this terminal),
//! then starts the client inside it, which does the drawing, and stops when either
//! ends. Nothing about the protocol changes: the two halves meet only through the
//! wire. This module exists to make the common case a single command instead of
//! two terminals.
//!
//! Both halves being started by one command does not make them one process: the
//! client runs *inside* the server's pty, and finds the server the same way any
//! other client does — by announcing a channel in its own output. There is nothing
//! to pass between them out of band, which is why this file has no path, no socket
//! name, and no state to share.

use std::path::{Path, PathBuf};

const USAGE: &str = "usage: tuigui <runnable>\n\
     e.g. tuigui /path/to/app";

/// What `tuigui` should do.
#[derive(Debug, Clone)]
pub struct Config {
    /// The runnable to draw, which is what produces the frames.
    pub runnable: PathBuf,
}

impl Config {
    /// Parse `<runnable>`.
    ///
    /// The runnable is the user's request and the server's command is derived from
    /// it, so there is no second command and no address to pass: a client names its
    /// own channel, so there is nothing here for a user to configure, and a flag
    /// would be a knob with one setting.
    pub fn from_args(
        args: impl Iterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = args.peekable();
        if let Some(arg) = args.peek() {
            // Accepted no longer: the tunnel it selected was removed. Failing
            // loudly beats starting a local session for someone who asked for a
            // remote one and wondering why nothing is remote.
            if arg == "--net" {
                return Err("--net was removed: announce a channel instead".into());
            }
        }
        let runnable = args.next().ok_or(USAGE)?;
        Ok(Self {
            runnable: PathBuf::from(runnable),
        })
    }
}

/// Start the session: the server owns this terminal, and the command inside it is
/// this same binary re-entered as the client.
///
/// The client has to be *inside* the pty, not beside the server on the same
/// terminal. The server owns the real terminal's input and output, and its rewrite
/// of what the pane draws only sees what comes out of the pty: a client drawing
/// directly would have its bytes delivered unrewritten, so the terminal would show
/// the raw text and never an image. Piping the client through the pty also makes
/// the server the only reader of fd 0, so nothing steals a keystroke.
pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let session = tui::Tui {
        command: server_command(&config.runnable),
    };
    // Nothing is drawn from this thread: the client runs inside the pty, which is
    // what puts its drawing through the rewrite. This only waits for the session to
    // end, which happens when the client inside it exits.
    //
    // `std::io`'s handles, not tokio's: the session takes plain streams and puts the
    // blocking reads on their own thread, so there is nothing async about them to
    // wrap.
    Ok(session.run(std::io::stdin(), std::io::stdout()).await?)
}

/// The command the server runs: this same binary, re-entered as the client for
/// `runnable`.
fn server_command(runnable: &Path) -> Vec<String> {
    let mut cmd = vec![std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "tuigui".into())];
    cmd.push(INTERNAL_CLIENT_FLAG.into());
    cmd.push(runnable.to_string_lossy().into_owned());
    cmd
}

/// The flag `tuigui` uses to re-enter itself as the pty's client.
///
/// A flag rather than an environment variable: the pty's command line is what the
/// server execs, and an environment marker would be inherited by anything the
/// client itself spawns, making the re-entry ambiguous.
pub const INTERNAL_CLIENT_FLAG: &str = "--client";
