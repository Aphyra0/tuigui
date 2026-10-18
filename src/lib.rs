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

use std::path::PathBuf;

/// Default socket. Under `/tmp` because a session is machine-scoped and
/// short-lived, matching the session file the server publishes next to it.
const DEFAULT_SOCKET: &str = "/tmp/tuigui.sock";

const USAGE: &str = "usage: tuigui [--socket P] <client args...>\n\
     e.g. tuigui play /tmp/clip.mp4\n\
     e.g. tuigui placeholders";

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
    pub client: gui::Command,
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
            client: gui::command_from_args(rest.into_iter())?,
            socket,
        })
    }
}

/// Start the session: the server owns this terminal, and the command inside it
/// is this same binary re-entered as the client.
///
/// The client has to be *inside* the pty, not beside the server on the same
/// terminal. The server is the only writer of the real terminal, and its rewrite
/// of the grid into placeholder cells only sees what comes out of the pty: a
/// client drawing directly would have its marker cells delivered unrewritten, so
/// the terminal would show blocks and never an image. Piping the client through
/// the pty also makes the server the only reader of fd 0, so a quit key reaches
/// exactly one place.
pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let path = session_path();

    // Remove the previous session first. Otherwise a stale file from an earlier
    // run makes the client connect to a socket nobody is listening on, which
    // reads as a confusing handshake failure rather than "the server is not up
    // yet".
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing stale {}: {e}", path.display()).into()),
    }

    let session = tui::Tui {
        socket: config.socket,
        session_path: Some(path),
        command: server_command(&client_argv(&config.client)),
    };
    // Nothing is drawn from this thread: the client runs inside the pty, which is
    // what puts its grid through the rewrite. This only waits for the session to
    // end, which happens when the client inside it exits.
    //
    // `std::io`'s handles, not tokio's: the session takes plain streams and puts
    // the blocking reads on their own thread, so there is nothing async about
    // them to wrap.
    Ok(session.run(std::io::stdin(), std::io::stdout()).await?)
}

/// The command the server runs: this same binary, re-entered to run the client.
fn server_command(client: &[String]) -> Vec<String> {
    let mut cmd = vec![
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "tuigui".into()),
    ];
    cmd.push(INTERNAL_CLIENT_FLAG.into());
    cmd.extend(client.iter().cloned());
    cmd
}

/// The flag `tuigui` uses to re-enter itself as the pty's client.
///
/// A flag rather than an environment variable: the pty's command line is what the
/// server execs, and an environment marker would be inherited by anything the
/// client itself spawns (ffmpeg), making the re-entry ambiguous.
pub const INTERNAL_CLIENT_FLAG: &str = "--client";

/// The client's own argv, as a command line, for the pty.
fn client_argv(client: &gui::Command) -> Vec<String> {
    match client {
        gui::Command::Placeholders => vec!["placeholders".into()],
        gui::Command::Play(path) => vec!["play".into(), path.clone()],
    }
}
