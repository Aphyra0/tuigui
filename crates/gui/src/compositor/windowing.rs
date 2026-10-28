//! The windowing vocabulary: what a session is, what a window is, and the
//! input a window takes.
//!
//! A **session** runs one program and gives back what it draws. Its windows
//! form a tree: the root window, and the windows the program opened after it,
//! each opening more. The backends live beside this module — [`wayland`] and
//! [`dummy`] — and this module is the traits they implement, plus the state
//! the compositor keeps about a window.
//!
//! Deliberately absent: keycodes and button codes. Those are per-platform
//! (evdev on Linux, a virtual keycode on macOS), so the vocabulary here is
//! logical — a key, a button, a wheel step — and the backend owns the mapping.

pub mod dummy;
pub mod wayland;

use std::sync::Weak;

use ratatui::widgets::WidgetRef;

/// A window's state, as the compositor keeps it: its size and its position
/// inside its parent, in cells. This is what a window is drawn from, and what
/// a drag moves or a border pull resizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowState {
    /// The window's size, in cells.
    pub size: (u16, u16),
    /// The window's position inside its parent, in cells.
    pub position: (u16, u16),
}

/// The window session as a whole: what a program runs in.
///
/// A server owns one session: it starts the program, steps it forward, and
/// offers the windows it produced. The holder works through this trait, so a
/// backend with no real program behind it — a debug one that only draws — is
/// as good a session as a wayland one.
pub trait WindowingServer {
    /// The root window: the program's main one, as a weak handle. `None`
    /// while it has not been built; the session dropping the window is what
    /// kills the handle.
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>>;

    /// Start the session's program. A session whose program is not its own to
    /// start — the dummy's, or a session attached to one already running —
    /// starts nothing.
    fn spawn(&mut self, _path: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }

    /// One turn of the session: accept what arrived, dispatch what the
    /// program did, and flush back down the sockets. A dummy session moves
    /// forward by being drawn instead, so its turn is nothing.
    fn advance(&mut self, _timeout: std::time::Duration) {}

    /// Whether the session's program has connected. False while nobody has
    /// said otherwise.
    fn is_ready(&self) -> bool {
        false
    }

    /// Whether the session's program is still running: the fact that ends a
    /// session.
    fn is_running(&self) -> bool {
        true
    }

    /// Resize the session: the size every window is measured against.
    fn set_size(&mut self, size: (u32, u32));
}

impl WindowingServer for Box<dyn WindowingServer> {
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>> {
        (**self).root_window()
    }

    fn spawn(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        (**self).spawn(path)
    }

    fn advance(&mut self, timeout: std::time::Duration) {
        (**self).advance(timeout)
    }

    fn is_ready(&self) -> bool {
        (**self).is_ready()
    }

    fn is_running(&self) -> bool {
        (**self).is_running()
    }

    fn set_size(&mut self, size: (u32, u32)) {
        (**self).set_size(size);
    }
}

/// One admitted window: what it draws, where it sits, and what may be pushed
/// into it.
///
/// A window keeps its own state, so a later read reads where the compositor
/// put it. Children are handed out as weak handles, so the compositor can
/// keep a registry of what it has seen and prune what died.
pub trait StatefulWindow: WidgetRef + Send + Sync {
    /// Queue one input event for delivery to this window.
    fn input(&self, input: Input) -> Result<(), Error>;

    /// What the window last drew: RGBA8, row-major, `width * height * 4` bytes.
    /// `NotReady` until it has drawn at least once.
    fn capture(&self) -> Result<Vec<u8>, Error>;

    /// Ask this window to draw at `size` pixels instead of its current one.
    ///
    /// The client decides whether to follow: its next commit is the answer.
    fn set_size(&self, size: (u32, u32));

    /// The window's state: its size and its place inside its parent.
    fn state(&self) -> WindowState;

    /// Put the window somewhere else, or make it another size.
    fn set_state(&self, state: WindowState);

    /// Every window this one opened after it, as weak handles. Always all of
    /// them: the caller keeps what it has not seen before — told apart by the
    /// pointer, not by bookkeeping — and prunes the ones that died.
    fn get_children(&self) -> Vec<Weak<dyn StatefulWindow>>;
}

/// One queued input event. Positions are window-local cells, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// A key, by the character or the named key it is.
    Key { key: Key, state: KeyState },
    /// The pointer moved to a position inside the window.
    ///
    /// `held` is the button carried down while the move happened: a drag. A
    /// plain motion carries none.
    MouseMove { x: f64, y: f64, held: Option<Button> },
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
