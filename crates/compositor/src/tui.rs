//! Terminal input event types.
//!
//! The TUI side (crossterm/ratatui) produces keys and pointer motions in
//! evdev-code form; this module fixes the wire shapes consumed by the input
//! thread before dispatch into the compositor's `Client`.

/// A key event as produced by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Linux evdev key code (XKB keysym + 8, matching libinput conventions).
    pub code: u32,
    pub state: KeyState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Press,
    Release,
}

/// Pointer event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerEvent {
    /// Move to absolute output coordinates (pixels).
    Motion { x: f64, y: f64 },
    Button { code: u32, state: KeyState },
    /// Scroll: positive = away from user (up).
    Axis { dx: f64, dy: f64 },
}