//! The command on a pty, and the proxy that carries its bytes.

use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};

use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

use crate::bytes::Bytes;
use crate::proxy::{Proxy, Streams};
use crate::sink::Sink;

/// A pty we own, with the wrapped command attached to its slave.
///
/// The pty is what makes the command behave like a terminal program rather than
/// a pipeline: it gets a controlling terminal, a window size, and job control.
/// What it writes comes back out of the master, and what we write to the master
/// arrives on its stdin.
pub struct Pty {
    /// Both directions between the command and this terminal.
    proxy: Proxy,
    /// The command, so its exit can be waited on.
    child: Child,
    /// The grid the pty was created with, in cells.
    ///
    /// The client needs it: it publishes the session file, and a client cannot
    /// read the terminal's size from its environment inside a muxer's pane.
    cols: u32,
    rows: u32,
    /// The command's output, as something else can look at it.
    ///
    /// Held here rather than handed to a consumer, which is the inversion: the
    /// pty is the producer and knows nothing about who is watching, and a
    /// consumer is given [`Pty::bytes`] and subscribes. Nothing in the pty
    /// changes when a consumer is added.
    bytes: Bytes,
}

impl Pty {
    /// Start `command` on a fresh pty of this terminal's size.
    ///
    /// `stdin` is the keyboard and `sink` the screen. They are taken as arguments
    /// rather than read from the process, which is what lets a test drive this
    /// without a terminal of its own.
    ///
    /// Nothing is told where to send what it finds here: whoever wants the
    /// command's bytes calls [`Pty::bytes`] and subscribes.
    pub fn new(
        command: &[String],
        stdin: impl Read + Send + 'static,
        sink: Sink,
    ) -> io::Result<Self> {
        // The window size is a property of the terminal device, and
        // `tcgetwinsize` needs an fd, which a generic `Read` does not promise to
        // be. Its size is therefore read from this process's own stdin.
        let ws = rustix::termios::tcgetwinsize(io::stdin())?;

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(io::Error::from)?;
        grantpt(&master).map_err(io::Error::from)?;
        unlockpt(&master).map_err(io::Error::from)?;
        let name = ptsname(&master, Vec::new()).map_err(io::Error::from)?;
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(Path::new(OsStr::from_bytes(name.as_bytes())))?;
        rustix::termios::tcsetwinsize(&master, ws).map_err(io::Error::from)?;
        let master = std::fs::File::from(master);

        let child = exec(command, &slave)?;

        // The master twice: one handle is written for the command's input, the
        // other read for its output. A pty master is a single stream, so the two
        // directions cannot share one handle without one of them consuming the
        // other's bytes.
        //
        // A pty merges the command's stderr into its stdout, so there is nothing
        // separate to read for it. The `stderr` here is this process's, for the
        // proxy's own failures.
        let source = Streams::new(master.try_clone()?, master.try_clone()?, io::stderr());
        // This terminal's end, with the two directions crossed: the keyboard is
        // what this end *produces*, so it is the `stdout` here, and the screen is
        // what it consumes, so it is the `stdin`.
        let dest = Streams::new(sink, stdin, io::stderr());

        // The view is published into from the forwarding thread, so what it
        // carries is the chunk that was just read: a slice that already exists,
        // handed on rather than copied.
        let bytes = Bytes::new();
        let published = bytes.clone();
        let proxy = Proxy::new(source, dest).watching(move |chunk| published.publish(chunk));

        Ok(Self {
            proxy,
            child,
            cols: ws.ws_col as u32,
            rows: ws.ws_row as u32,
            bytes,
        })
    }

    /// The command's output, as a view a consumer can subscribe to.
    ///
    /// A clone of one `Arc`, so the caller keeps its own handle and the pty keeps
    /// publishing: subscribing is how a consumer joins, and dropping its handle is
    /// how it leaves.
    pub fn bytes(&self) -> Bytes {
        self.bytes.clone()
    }

    /// The grid the command's pty was created with, in cells.
    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Forward bytes until the command exits. Blocks.
    ///
    /// Two things can end this half, so both are waited on: the command exiting,
    /// which is the end of the session, and the byte pump ending, which is the pty
    /// closing. Whichever happens first is it.
    ///
    /// The pump itself runs on a blocking thread. It is raw fd IO with no await
    /// points, so the runtime has nothing to schedule; the thread is what keeps a
    /// blocking read from occupying a runtime worker.
    ///
    /// Dropping the child on the way out is the kill: it is `kill_on_drop`, so a
    /// session that ended for any reason leaves no command behind.
    pub async fn run(self) -> io::Result<()> {
        let Pty {
            proxy,
            mut child,
            ..
        } = self;
        let forwarded = tokio::task::spawn_blocking(move || proxy.run());
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                if !status.success() {
                    eprintln!("tui: command exited with {status}");
                }
                Ok(())
            }
            // A panicked blocking task is a failure to report, not a session that
            // simply ended.
            ended = forwarded => match ended {
                Ok(result) => result,
                Err(e) => Err(io::Error::other(format!("the byte pump failed: {e}"))),
            },
        }
    }
}

/// Start `cmd` on `slave`, with the pty as its controlling terminal.
///
/// `kill_on_drop` is what lets a caller stop the command by dropping it: the
/// session has two halves, and when one ends the other must not be left running
/// with nobody to read it.
fn exec(cmd: &[String], slave: &std::fs::File) -> io::Result<Child> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.kill_on_drop(true);
    c.stdin(Stdio::from(slave.try_clone()?));
    c.stdout(Stdio::from(slave.try_clone()?));
    c.stderr(Stdio::from(slave.try_clone()?));
    unsafe {
        c.pre_exec(|| {
            // New session, then adopt the pts as the controlling terminal so job
            // control and job-control signals behave.
            rustix::process::setsid().map_err(io::Error::from)?;
            rustix::process::ioctl_tiocsctty(std::io::stdin()).map_err(io::Error::from)?;
            Ok(())
        });
    }
    c.spawn()
}
