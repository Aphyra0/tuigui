//! What a terminal sends, turned into what a Wayland compositor takes.
//!
//! A terminal program holds two descriptors: bytes arriving, bytes going out.
//! [`Listener`] is that pair. Making it asks the terminal for its mouse and
//! keyboard events and starts a reader that runs for as long as the listener
//! lives; [`Listener::capture`] hands out a [`Capture`], and
//! [`Capture::finish`] returns everything that arrived since the last one.
//!
//! ```text
//!   Listener ──capture()──▶ Capture ──finish()──▶ Vec<InputEvent>
//!      ▲                        │
//!      └── the reader keeps running, for the whole session ──┘
//! ```
//!
//! # One reader, drained
//!
//! The reader is started once and never restarted. A capture is a *reading* of
//! what it has collected, not a thread of its own: `finish` drains the events
//! and returns, and the bytes that arrive afterwards are waiting for the next
//! capture. That shape is what makes a capture cheap enough to take every frame,
//! and it is why there is no thread to join and nothing to synchronise between
//! one frame and the next.
//!
//! The reader waits with `poll` rather than reading and sleeping. A read on a
//! descriptor with nothing to give returns immediately, so a loop that read
//! anyway would spin -- burning a core and doing nothing -- and that is exactly
//! the shape a wrapper around a quiet terminal must not have.
//!
//! # From terminal events to Wayland
//!
//! The two systems disagree about what an input *is*. Wayland's currency is an
//! evdev keycode: a position on the keyboard, which the client resolves into a
//! character through a keymap the compositor sent it. A terminal's currency is a
//! character plus modifiers. [`evdev`] is how a key position is recovered, which
//! is why case and control work: `a` and `Shift+A` are one key position with
//! different modifiers, and a modifier is a key the seat is sent rather than a
//! bit somebody has to interpret.

mod evdev;

pub use evdev::{
    Button, InputEvent, Key, KeyCode, KeyState, MouseEvent, MouseKind, Scroll, KEYMAP_NAME,
};

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
/// The escape sequences that ask a terminal for the events we want.
///
/// One request in parts: report mouse events (`1000`), report **all** motion
/// (`1003`), encode coordinates as decimal text (`1006`, without which a column
/// past 223 is unrepresentable), and bracketed paste (`2004`).
///
/// Motion is asked for unconditionally, not only while a button is held (`1002`).
/// A pointer has to be *seen* to hover over a control before it can be clicked,
/// and a terminal in the held-only mode reports nothing at all while the mouse is
/// merely moved -- so the pointer never reaches the program and the window looks
/// dead until something is pressed.
///
/// The kitty keyboard protocol is **not** requested, and that is deliberate. Its
/// disambiguate level (`>1u`) makes a terminal re-encode `Ctrl+C` as a `CSI u`
/// sequence rather than the byte `0x03` -- and the kernel's `ISIG` only raises an
/// interrupt for that byte, so requesting it silently removes the ability to stop
/// a session with `Ctrl+C`.
const ENABLE: &[&str] = &[
    "\x1b[?1000h", // mouse button events
    "\x1b[?1003h", // ... and every motion, held or not
    "\x1b[?1006h", // SGR encoding: decimal coordinates, no 223 limit
    "\x1b[?2004h", // bracketed paste
];

/// The same requests, turned off.
const DISABLE: &[&str] = &[
    "\x1b[?2004l",
    "\x1b[?1006l",
    "\x1b[?1003l",
    "\x1b[?1000l",
];

/// How long the reader waits for a byte before looking at its stop flag.
///
/// The wait is a `poll` timeout, not a sleep after a failed read: with nothing to
/// read, `poll` blocks in the kernel and the process costs nothing, where a read
/// that returned "nothing yet" would have to be followed by a sleep to avoid
/// spinning. This is the ceiling on how long a shutdown waits for the reader.
const POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// A terminal input: bytes to read, and a descriptor to wait on.
///
/// Both are needed together. Reading alone cannot wait -- a read on a descriptor
/// with nothing to give returns immediately, and a loop around it spins -- and a
/// descriptor alone cannot be read portably. The reader owns one of these, so the
/// two always refer to the same thing.
pub trait Input: Read + AsRawFd + Send + 'static {}

impl<T: Read + AsRawFd + Send + 'static> Input for T {}

/// A boxed terminal input, so a caller may hand in its own.
///
/// A box hides the descriptor, so it is forwarded here: the reader polls the same
/// descriptor it reads. `Read` is not forwarded -- the standard library already
/// implements it for a boxed reader, and forwarding it too would be two impls of
/// one trait for one type.
impl AsRawFd for Box<dyn Input> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        (**self).as_raw_fd()
    }
}

/// The two handles a terminal program has, and the reader over its input.
pub struct Listener<R: Input, W: Write> {
    /// What the reader appends to, and the flag that stops it.
    shared: std::sync::Arc<Shared>,
    /// The reader, joined when the listener goes away.
    reader: Option<std::thread::JoinHandle<()>>,
    /// Held so the protocols can be turned off again.
    output: W,
    /// Keeps the input's type in the struct's, without holding it: the reader
    /// owns the handle, and this is what says which kind of handle that was.
    _input: std::marker::PhantomData<R>,
}

/// What the reader and every capture share.
struct Shared {
    /// The events read so far, in arrival order.
    ///
    /// A mutex rather than a channel because there is one reader and the caller
    /// wants one snapshot of everything, not a queue to drain message by message.
    events: std::sync::Mutex<Vec<InputEvent>>,
    /// Set to stop the reader; it looks between waits.
    stop: std::sync::atomic::AtomicBool,
}

impl<R: Input, W: Write> Listener<R, W> {
    /// Ask the terminal for its events and start reading, for as long as this
    /// listener lives.
    ///
    /// Fallible in a way that matters: if the requests cannot be written, nothing
    /// will ever be received, and saying so now beats an empty session that looks
    /// like a terminal sending nothing.
    pub fn new(input: R, mut output: W) -> std::io::Result<Self> {
        for sequence in ENABLE {
            output.write_all(sequence.as_bytes())?;
        }
        output.flush()?;

        let shared = std::sync::Arc::new(Shared {
            events: std::sync::Mutex::new(Vec::new()),
            stop: std::sync::atomic::AtomicBool::new(false),
        });

        let thread_shared = std::sync::Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name("termputs".into())
            .spawn(move || read_loop(input, &thread_shared))?;

        Ok(Self {
            shared,
            reader: Some(reader),
            output,
            _input: std::marker::PhantomData,
        })
    }

    /// Take a reading of this terminal's input.
    ///
    /// Cheap: no thread is started and nothing waits. [`Capture::finish`] returns
    /// whatever the reader has collected since the last reading, and the reader
    /// carries on.
    pub fn capture(&self) -> Capture {
        Capture {
            shared: std::sync::Arc::clone(&self.shared),
        }
    }
}

impl<R: Input, W: Write> Drop for Listener<R, W> {
    fn drop(&mut self) {
        // Stop the reader first, so nothing reads the terminal after the
        // protocols are turned off, and so the join below is short.
        self.shared
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(reader) = self.reader.take() {
            // The reader is in a `poll` that times out in `POLL`, so this waits
            // at most that long.
            let _ = reader.join();
        }
        for sequence in DISABLE {
            // The terminal may already be gone, and there is nowhere useful to
            // report a failure. Best effort, no retry.
            let _ = self.output.write_all(sequence.as_bytes());
        }
        let _ = self.output.flush();
    }
}

/// One reading of a terminal's input.
pub struct Capture {
    shared: std::sync::Arc<Shared>,
}

impl Capture {
    /// The events read since the last reading, and forget them.
    ///
    /// Does not stop the reader: the terminal is still being read, and the next
    /// capture gets what arrives after this.
    pub fn finish(self) -> Vec<InputEvent> {
        match self.shared.events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            // A poisoned lock still holds a valid list, and the events appended
            // before the panic are exactly what was asked for.
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

/// Read the terminal for as long as the listener lives.
///
/// `poll` is what makes this quiet: it blocks until there is something to read or
/// until the timeout, and only then is a read issued at all. A descriptor with
/// nothing to give costs one syscall per [`POLL`], not a spin.
fn read_loop<R: Input>(mut input: R, shared: &Shared) {
    let fd = unsafe { BorrowedFd::borrow_raw(input.as_raw_fd()) };
    let mut buf = [0u8; 4096];
    loop {
        if shared.stop.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // A negative timeout would block forever, which would make the stop flag
        // unreachable; the timeout is what gives the flag a chance.
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: POLL.as_nanos() as _,
        };
        let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_) if fds[0].revents().contains(rustix::event::PollFlags::IN) => {}
            // Readable in a way that is not `IN` (a hangup, say): the read below
            // reports it, so this is not the place to decide.
            Ok(_) => continue,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                log::info!("could not wait on the terminal: {e}");
                return;
            }
        }

        match input.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let mut parsed = InputEvent::parse_all(&buf[..n]);
                if parsed.is_empty() {
                    continue;
                }
                match shared.events.lock() {
                    Ok(mut events) => events.append(&mut parsed),
                    Err(poisoned) => poisoned.into_inner().append(&mut parsed),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                log::info!("the terminal's input ended: {e}");
                return;
            }
        }
    }
}
