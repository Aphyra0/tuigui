//! Forwarding bytes between this terminal and the command's pty.

use std::io::{self, Read, Write};
use std::thread;

/// One end of the proxy: the standard streams of the process at that end.
///
/// `stdin` is what feeds that process, so the proxy writes to it; `stdout` is
/// what that process produces, so the proxy reads it. They are that process's
/// descriptors, not `tui`'s, which is why the terminal end is built with them
/// crossed: its stdin is the keyboard, so it is the field the *other* direction
/// reads from.
pub struct Streams {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Write + Send>,
}

impl Streams {
    pub fn new(
        stdin: impl Write + Send + 'static,
        stdout: impl Read + Send + 'static,
        stderr: impl Write + Send + 'static,
    ) -> Self {
        Self {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        }
    }
}

/// Forwards bytes between two ends.
///
/// Both directions are one end's stdout into the other's stdin:
///
/// ```text
///   source.stdout  ->  dest.stdin     the command's output, to the screen
///   dest.stdout    ->  source.stdin   the keyboard, to the command
/// ```
///
/// `stderr` is written only by the end that reports a failure, so a command's
/// own stderr is never fed back into it as input.
pub struct Proxy {
    /// The command's end: the pty master.
    source: Streams,
    /// This terminal's end.
    dest: Streams,
}

impl Proxy {
    pub fn new(source: Streams, dest: Streams) -> Self {
        Self { source, dest }
    }

    /// Forward both directions until the command's output ends. Blocks.
    ///
    /// The output direction is the lifetime: when it ends the command is gone.
    /// The keyboard direction only makes sense while it runs, so it gets a
    /// thread, and its error is returned from here once that thread is joined.
    pub fn run(self) -> io::Result<()> {
        let Proxy {
            mut source,
            mut dest,
        } = self;
        let keyboard =
            thread::spawn(move || copy(&mut *dest.stdout, &mut *source.stdin));
        let shown = copy(&mut *source.stdout, &mut *dest.stdin);
        // A panic in the keyboard thread is not a forwarding failure, so it is
        // not reported as one.
        shown.and(keyboard.join().unwrap_or(Ok(())))
    }
}

/// Copy from `from` to `to` until it ends, flushing after each read.
///
/// `EINTR` is retried rather than reported: signals are routine here, and
/// treating one as an end would stop forwarding for the rest of the session.
///
/// `?Sized` because the ends are boxed: `&mut impl Read` alone is `Sized`, and
/// the streams here are `dyn`.
fn copy<R: Read + ?Sized, W: Write + ?Sized>(from: &mut R, to: &mut W) -> io::Result<()> {
    let mut buf = [0u8; 16384];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                to.write_all(&buf[..n])?;
                to.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
