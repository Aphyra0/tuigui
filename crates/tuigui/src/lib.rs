//! `tuigui` — run an app here, locally, in one command.
//!
//! It starts the server (a pty plus the TGP plumbing that owns this terminal),
//! then starts the client, which does the drawing, and stops the server when
//! the client is done. Nothing about the protocol changes: the two halves still
//! meet only through the wire and the session file. This module exists to make
//! the common case a single command instead of two terminals.
//!
//! The server is started with a stop handle, so an app that outlives its client
//! (a shell at a prompt) does not hold the terminal forever after the client has
//! finished.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tuigui_tui::Config as ServerConfig;

/// Default socket. Under `/tmp` because a session is machine-scoped and
/// short-lived, matching the session file the server publishes next to it.
const DEFAULT_SOCKET: &str = "/tmp/tuigui.sock";

/// Where the server publishes its connection details.
///
/// Duplicated from `tuigui_wire::session::DEFAULT_PATH` rather than depended on:
/// this crate only needs to know where to look, and the environment override is
/// shared, so both sides agree without a dependency.
fn session_path() -> PathBuf {
    std::env::var("TUI_SESSION")
        .unwrap_or_else(|_| "/tmp/tuigui/session".into())
        .into()
}

/// What `tuigui` should do.
#[derive(Debug, Clone)]
pub struct Config {
    /// The client to run, which is what produces the frames.
    pub client: tuigui_gui::Command,
    /// Unix socket for the server to listen on.
    pub socket: String,
}

impl Config {
    /// Parse `[--socket P] <client args...>`.
    ///
    /// The arguments are the *client's*, since the client is what the user is
    /// asking for (`tuigui play clip.mp4`). The server's command defaults to the
    /// user's shell, which is what should be on screen behind the frames.
    pub fn from_args(
        args: impl Iterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut socket = std::env::var("TUI_SOCK").unwrap_or_else(|_| DEFAULT_SOCKET.into());
        let mut args = args.peekable();

        while let Some(arg) = args.peek() {
            match arg.as_str() {
                "--socket" => {
                    args.next();
                    socket = args.next().ok_or("--socket needs a value")?;
                }
                // Accepted no longer: the tunnel it selected was removed. Failing
                // loudly beats starting a socket-only server for someone who
                // asked for a remote one and wondering why nothing dials.
                "--net" => {
                    return Err("--net was removed: frames travel over the unix socket only".into())
                }
                _ => break,
            }
        }

        let rest: Vec<String> = args.collect();
        if rest.is_empty() {
            return Err(USAGE.into());
        }
        Ok(Self {
            client: tuigui_gui::command_from_args(rest.into_iter())?,
            socket,
        })
    }
}

const USAGE: &str = "usage: tuigui [--socket P] <client args...>\n\
     e.g. tuigui play /tmp/clip.mp4\n\
     e.g. tuigui placeholders";

/// The command the server runs: the user's shell, so the frames land on top of
/// a normal prompt. `SHELL` is honored, with `sh` as the portable fallback.
fn default_server_command() -> Vec<String> {
    vec![std::env::var("SHELL").unwrap_or_else(|_| "sh".into())]
}

/// Start the server, run the client, then stop the server.
pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let path = session_path();

    // Remove the previous session first. Otherwise `wait_for_session` is
    // satisfied by a stale file from an earlier run and the client connects to
    // a socket nobody is listening on, which reads as a confusing handshake
    // failure rather than "the server is not up yet".
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing stale {}: {e}", path.display()).into()),
    }

    let server_config = ServerConfig {
        socket: config.socket.clone(),
        session_path: Some(path.clone()),
        command: default_server_command(),
    };

    // The server runs until the wrapped command exits, so it cannot share this
    // thread: the client owns the terminal from here on.
    let handle = tuigui_tui::spawn(server_config)?;
    wait_for_session(&path, Duration::from_secs(10))?;

    let client_res = tuigui_gui::run(config.client);

    // The client holds the screen until interrupted, so reaching here means it
    // finished or failed. Either way nothing will draw again, and the wrapped
    // shell would sit at a prompt inside a pty nobody reads, so stop it.
    handle.stop();
    client_res
}

/// Block until `path` exists, or fail.
///
/// A real synchronization point: the client reads the socket path and the grid
/// size out of this file, so starting it earlier would have it connect to
/// nothing.
fn wait_for_session(path: &Path, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!("no session at {} within {timeout:?}", path.display()).into())
}
