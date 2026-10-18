//! `tui` — run a command in a pty we own and forward its bytes to this terminal.
//!
//! ```text
//!   this terminal  <── Pty ──▶  the command's pty  ──▶  the command
//!                     Server: unix socket, images
//! ```
//!
//! The command is a normal terminal program: it is given a pty, and everything
//! it reads and writes is forwarded unchanged. The server is a second channel a
//! client sends frames on, kept separate from the byte stream so nothing a client
//! sends can appear in the command's input.
//!
//! Both channels write the screen through one [`Sink`], which is what stops a
//! frame from landing in the middle of the command's output.

mod proxy;
mod pty;
mod server;
mod sink;

use std::io::{self, Read, Write};

use pty::Pty;
use server::Server;
use sink::Sink;

pub use pty::Pty as Command;
pub use proxy::Streams;
pub use sink::Sink as Screen;

// A session, as the command line describes it. `run` is where it starts.
//
// `command` is trailing and required: `tui --path P` with nothing to run has no
// meaning, and clap rejects it rather than starting a server around an empty
// command. `allow_hyphen_values` is what lets the wrapped command's own flags
// (`-l`, `--color`) through instead of being read as `tui`'s.
#[derive(Debug, Clone, clap::Parser)]
#[command(
    name = "tui",
    about = "Run a command in a pty and forward it to this terminal"
)]
pub struct Tui {
    /// Unix socket to listen on.
    #[arg(long = "path", env = "TUI_SOCK", default_value = "/tmp/tui.sock")]
    pub socket: String,
    /// Where to publish the session file a client reads to find the socket and
    /// grid size. Defaults to the shared path, or `TUI_SESSION`.
    #[arg(long = "session", env = "TUI_SESSION")]
    pub session_path: Option<std::path::PathBuf>,
    /// The command to run, with its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

impl Tui {
    /// Start both channels until either ends.
    ///
    /// Both halves are waited on together, and whichever ends first ends the
    /// other: the command's child is `kill_on_drop`, so dropping its half kills
    /// it, and the server's future being dropped closes the listener. A session
    /// where one half is gone has no reader for the other, so leaving the other
    /// running would be a process with nobody watching it.
    ///
    /// Async, with the runtime owned by the caller: a binary gets one from
    /// `#[tokio::main]`, and a test or an embedder can use whatever it already
    /// has rather than being made to build a second one.
    pub async fn run(
        self,
        stdin: impl Read + Send + 'static,
        stdout: impl Write + Send + Sync + 'static,
    ) -> io::Result<()> {
        let _raw = Raw::install()?;
        let sink = Sink::new(stdout);

        let pty = Pty::new(&self.command, stdin, sink.clone())?;
        let server = Server::new(
            sink,
            &self.socket,
            pty.cols(),
            pty.rows(),
            self.session_path.clone(),
        )?;

        tokio::select! {
            // The command's half: its own select races the bytes against the
            // child, so this is one arm of the whole session.
            session = pty.run() => session,
            // The frame channel ended, so nothing is drawing any more.
            served = server.run() => served,
        }
    }
}

/// Holds this terminal in raw mode, and restores the old settings when dropped.
///
/// Raw mode is not optional for a full-screen wrapper. With the line discipline
/// on, typed keys are echoed here *and* by the command (double echo), output
/// newlines are translated a second time, and keys are not delivered until a
/// newline. The command's own pty has its own discipline either way; this one is
/// about the terminal `tui` is attached to.
struct Raw {
    original: rustix::termios::Termios,
}

impl Raw {
    /// Put this process's stdin in raw mode.
    ///
    /// Raw means *everything off*, and that is the point: this terminal is only a
    /// pipe for bytes. `OPOST` off is what stops a `\n` becoming `\r\n` on the way
    /// out, `ICRNL` off is what stops a `\r` becoming `\n` on the way in, and
    /// `ICANON` off is what stops keys from being held until a newline. Any one of
    /// them left on would mean this layer *adds* to the stream rather than passing
    /// it, which is the one thing it must never do.
    ///
    /// `ISIG` stays cleared, deliberately. Restoring it would turn `^C` into a
    /// signal for this process and eat the byte, so the command would never see
    /// it. The command's own pty has `ISIG` on, so the byte is forwarded there and
    /// its line discipline raises the signal, which is the side that should get
    /// it.
    ///
    /// Reads the termios from `io::stdin()`: this is the terminal the process is
    /// attached to, which is the one whose line discipline would otherwise mangle
    /// the bytes on their way through.
    fn install() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = rustix::termios::tcgetattr(&stdin)?;
        let mut raw = original.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            io::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}
