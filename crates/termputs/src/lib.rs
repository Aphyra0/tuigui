//! What a terminal sends, turned into what a Wayland compositor takes.
//!
//! A terminal program holds two descriptors: bytes arriving, bytes going out.
//! [`Listener`] is that pair, and it is the thing to keep: it turns the
//! terminal's mouse and keyboard protocols on when it is made, off when it is
//! dropped, and hands out a [`Capture`] on demand.
//!
//! ```text
//!   Listener ──capture()──▶ Capture ──(a thread reads)──▶ events accumulate
//!      ▲                       └── finish() ──▶ Vec<InputEvent>
//!      └────────────────── another capture() starts the next one
//! ```
//!
//! A [`Capture`] is one round of reading: it exists, a thread appends to it, and
//! [`Capture::finish`] stops that thread and returns everything it read. The
//! protocols are not touched by a capture, which is what makes a capture
//! disposable: a caller that wants input once per frame finishes one and starts
//! the next, and the terminal never sees mouse reporting flicker off between
//! frames.
//!
//! # Keep the listener, discard the captures
//!
//! `capture` takes `&self` and clones the two handles into the reader thread, so
//! the listener outlives any number of captures. The handles must be cloneable,
//! which a terminal's standard handles are: every clone refers to the same
//! descriptor.
//!
//! # From terminal events to Wayland
//!
//! The two systems disagree about what an input *is*. Wayland's currency is an
//! evdev keycode: a position on the keyboard, which the client resolves into a
//! character through a keymap the compositor sent it. A terminal's currency is a
//! character plus modifiers, and in kitty's protocol a Unicode codepoint.
//! [`InputEvent`] carries both, and [`evdev`] is how the key position is
//! recovered -- which is why case and control finally work: `a` and `Shift+A`
//! are one key position with different modifiers, and `Ctrl+C` is `c` with a
//! modifier rather than the byte `0x03` that collides with typing ETX.

mod evdev;

pub use evdev::{
    Button, InputEvent, Key, KeyState, MouseEvent, MouseKind, Scroll, KEYMAP_NAME,
};

use std::io::{Read, Write};

/// The escape sequences that ask a terminal for the events we want.
///
/// One request in parts: report mouse events (`1000`), report motion while a
/// button is held (`1002`), encode coordinates as decimal text (`1006`, without
/// which a column past 223 is unrepresentable), and bracketed paste (`2004`).
///
/// The kitty keyboard protocol is **not** requested, and that is deliberate. Its
/// disambiguate level (`>1u`) makes a terminal re-encode `Ctrl+C` as a `CSI u`
/// sequence rather than the byte `0x03` -- and the kernel's `ISIG` only raises
/// an interrupt for that byte, so requesting it silently removes the ability to
/// stop the session with `Ctrl+C`. A key that kills the process is worth more
/// than a key that reaches the program, and the program gets every other key
/// either way.
const ENABLE: &[&str] = &[
    "\x1b[?1000h", // mouse button events
    "\x1b[?1002h", // ... and motion while a button is down
    "\x1b[?1006h", // SGR encoding: decimal coordinates, no 223 limit
    "\x1b[?2004h", // bracketed paste
];

/// The same requests, turned off.
const DISABLE: &[&str] = &[
    "\x1b[?2004l",
    "\x1b[?1006l",
    "\x1b[?1002l",
    "\x1b[?1000l",
];

/// How long the reader waits before looking at the stop flag again.
///
/// The input is expected to be non-blocking, because a blocking read cannot be
/// interrupted by a flag and a capture has to be able to finish. With nothing to
/// read, the loop would otherwise spin; this is the pause that keeps a quiet
/// terminal from costing a core.
const IDLE: std::time::Duration = std::time::Duration::from_millis(1);

/// The two handles a terminal program has, and the protocols they carry.
///
/// Made once and kept: it owns the requests that turn the terminal's mouse and
/// keyboard reporting on, and it is the only thing that turns them off again.
///
/// The input is held behind a lock rather than owned outright, so the listener
/// works with a handle that cannot be cloned -- a process's own stdin is the
/// case that matters. A capture borrows it for as long as it reads, which is why
/// only one capture may be live at a time and why the input must be
/// non-blocking.
pub struct Listener<R: Read, W: Write> {
    input: std::sync::Arc<std::sync::Mutex<R>>,
    output: W,
}

impl<R: Read + Send + 'static, W: Write> Listener<R, W> {
    /// Ask the terminal for its events, once, for as long as the listener lives.
    ///
    /// Fallible in a way that matters: if the requests cannot be written, nothing
    /// will ever be received, and saying so now beats an empty session that looks
    /// like a terminal sending nothing.
    pub fn new(input: R, mut output: W) -> std::io::Result<Self> {
        for sequence in ENABLE {
            output.write_all(sequence.as_bytes())?;
        }
        output.flush()?;
        Ok(Self {
            input: std::sync::Arc::new(std::sync::Mutex::new(input)),
            output,
        })
    }

    /// Start reading the terminal's input.
    ///
    /// The input handle is shared with the reader thread, so this can be called
    /// again for the next round of reading. The caller finishes each capture and
    /// starts the next; nothing about the terminal changes in between.
    ///
    /// The input **must be non-blocking**. A blocking read cannot be interrupted
    /// by the stop flag, so [`Capture::finish`] would wait on it forever. Making
    /// the descriptor non-blocking is the caller's job, because it owns the
    /// descriptor and knows when it wants one.
    pub fn capture(&self) -> std::io::Result<Capture> {
        Capture::start(std::sync::Arc::clone(&self.input))
    }
}

impl<R: Read, W: Write> Drop for Listener<R, W> {
    fn drop(&mut self) {
        for sequence in DISABLE {
            // The terminal may already be gone, and there is nowhere useful to
            // report a failure. Best effort, no retry.
            let _ = self.output.write_all(sequence.as_bytes());
        }
        let _ = self.output.flush();
    }
}

/// One round of reading a terminal's input.
pub struct Capture {
    /// What the reading thread appends to.
    events: std::sync::Arc<std::sync::Mutex<Vec<InputEvent>>>,
    /// Told to stop when the capture finishes.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The reading thread, joined on finish so nothing outlives the capture.
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Start the reading thread.
    fn start<R: Read + Send + 'static>(
        input: std::sync::Arc<std::sync::Mutex<R>>,
    ) -> std::io::Result<Self> {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let thread_events = std::sync::Arc::clone(&events);
        let thread_stop = std::sync::Arc::clone(&stop);
        let reader = std::thread::Builder::new()
            .name("termputs".into())
            .spawn(move || read_loop(input, &thread_events, &thread_stop))?;

        Ok(Self {
            events,
            stop,
            reader: Some(reader),
        })
    }

    /// Stop the reader and return everything it read.
    ///
    /// The reading thread is joined, so nothing is still appending when the
    /// events are taken. The terminal's protocols are left alone: this capture
    /// is one round, and the listener owns the protocols.
    pub fn finish(mut self) -> Vec<InputEvent> {
        self.stop();
        self.take_events()
    }

    /// Tell the reader to stop and wait for it.
    fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    /// Take the events out of the shared list.
    fn take_events(&self) -> Vec<InputEvent> {
        match self.events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            // A poisoned lock still holds a valid list, and the events appended
            // before the panic are exactly what was asked for.
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read the terminal while the capture lives.
fn read_loop<R: Read>(
    input: std::sync::Arc<std::sync::Mutex<R>>,
    events: &std::sync::Mutex<Vec<InputEvent>>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut buf = [0u8; 4096];
    loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // The lock is held across the read, so a second capture cannot claim the
        // same bytes. It is released between reads, which is what lets another
        // capture take over once this one is finished.
        let read = match input.lock() {
            Ok(mut input) => input.read(&mut buf),
            // A poisoned lock still guards a usable descriptor; the panic that
            // poisoned it happened in a reader, and the bytes are still there.
            Err(poisoned) => poisoned.into_inner().read(&mut buf),
        };
        match read {
            Ok(0) => return,
            Ok(n) => {
                let mut parsed = InputEvent::parse_all(&buf[..n]);
                if parsed.is_empty() {
                    continue;
                }
                match events.lock() {
                    Ok(mut events) => events.append(&mut parsed),
                    Err(poisoned) => poisoned.into_inner().append(&mut parsed),
                }
            }
            // Nothing to read yet. The stop flag is the only way out, so this is
            // where a quiet terminal is given a chance to be interrupted.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(IDLE);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                log::info!("the terminal's input ended: {e}");
                return;
            }
        }
    }
}
