//! The window session as a caller sees it, with no operating system in it.
//!
//! A **session** runs one program and gives back what it draws. The program may
//! start helpers that connect on their own; those are the session's
//! sub-clients, and a sub-client may start more. Every process owns a tree of
//! windows: a root window, and the windows its program opened afterwards.
//!
//! This module is the vocabulary of that shape, and the traits a backend
//! implements. Which backend is a compile-time fact of the platform
//! (spec/PLATFORMS.md): smithay supplies it on Linux, Quartz on macOS.
//!
//! Deliberately absent: keycodes and button codes. Those are per-platform
//! (evdev on Linux, a virtual keycode on macOS), so the vocabulary here is
//! logical — a key, a button, a wheel step — and the backend owns the mapping.

/// One queued input event. Positions are output pixels, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// A key, by the character or the named key it is.
    Key { key: Key, state: KeyState },
    /// The pointer moved to a position inside the window.
    MouseMove { x: f64, y: f64 },
    /// A pointer button went down or up.
    Button { button: Button, down: bool },
    /// A wheel or trackpad scroll.
    Scroll { delta: ScrollDelta },
}

/// A key, logically: printable text as a character, everything else by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Return,
    Tab,
    Backspace,
    Delete,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Whether a key or button went down or up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// A pointer button, by role rather than by code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// How far a wheel or trackpad scrolled, in steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollDelta {
    /// Vertical steps: positive is down.
    pub vertical: i32,
    /// Horizontal steps: positive is right.
    pub horizontal: i32,
}

/// Why a window could not do what was asked of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The client that owned the window is gone, so nothing is listening.
    Gone,
    /// The window exists but has not drawn yet: there is nothing to capture.
    NotReady,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Gone => f.write_str("the window is gone"),
            Error::NotReady => f.write_str("the window has not drawn yet"),
        }
    }
}

impl std::error::Error for Error {}

/// A client of the session: the program itself, or a helper it started.
///
/// The tree is the process tree. `is_root` is what tells the two apart, and
/// `pid` is the identity both kinds carry.
///
/// `C` and `W` are the state the holder parks on a client and on a window. They
/// are part of the trait, not of the backend, because the holder works through
/// these traits and never through a backend's concrete types: whatever it
/// attaches has to be reachable from here.
///
/// Handles are reached through `with_*` closures rather than returned: a
/// backend keeps its records behind locks (they are mutated while the session
/// steps), so a borrow cannot outlive the guard that produced it. The guard is
/// taken and dropped inside each method, which is also what keeps a caller from
/// holding one across a step that wants the write lock.
pub trait Client<C, W>: Send + Sync {
    /// The wayland peer's (or the window server's) process id.
    fn pid(&self) -> u32;

    /// Whether this is the session's own program rather than a helper.
    fn is_root(&self) -> bool;

    /// The state the holder parked on this client.
    fn with_attached(&self, f: &mut dyn FnMut(&C));

    /// Call `f` with this client's root window, once the program has opened
    /// one. Not called at all while the window is absent.
    fn with_window(&self, f: &mut dyn FnMut(&dyn Window<W>));

    /// Call `f` with each helper this client started, each its own connection.
    fn with_children(&self, f: &mut dyn FnMut(&dyn Client<C, W>));
}

/// One admitted window: what it draws, and what may be pushed into it.
///
/// `W` is the state the holder parks on a window, carried here so the holder
/// can read it back through the trait.
pub trait Window<W>: Send + Sync {
    /// Queue one input event for delivery to this window.
    fn input(&self, input: Input) -> Result<(), Error>;

    /// What the window last drew: RGBA8, row-major, `width * height * 4` bytes.
    /// `NotReady` until it has drawn at least once.
    fn capture(&self) -> Result<Vec<u8>, Error>;

    /// The state the holder parked on this window.
    fn with_attached(&self, f: &mut dyn FnMut(&W));

    /// Call `f` with each window this one's program opened after it, in tree
    /// order.
    fn with_children(&self, f: &mut dyn FnMut(&dyn Window<W>));
}
