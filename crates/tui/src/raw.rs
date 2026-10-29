//! Raw mode on the terminal `tui` is attached to.

use std::io;

/// Holds this terminal in raw mode, and restores the old settings when dropped.
///
/// Raw mode is not optional for a full-screen wrapper. With the line discipline
/// on, typed keys are echoed here *and* by the command (double echo), output
/// newlines are translated a second time, and keys are not delivered until a
/// newline. The command's own pty has its own discipline either way; this one is
/// about the terminal `tui` is attached to.
pub struct Raw {
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
    ///
    /// Nothing here touches the *terminal's* modes (`?7` autowrap and the like), which
    /// live outside the tty and cannot be read back.
    pub fn install() -> io::Result<Self> {
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
