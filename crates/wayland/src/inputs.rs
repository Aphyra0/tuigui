//! Input shapes: keys, buttons, scroll, and the pointer state as seen by the
//! session.

/// A keyboard key. Named keys cover the common navigation and editing set;
/// [`Key::Char`] covers printable text.
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

impl Key {
    /// The evdev code (linux/input-event-codes.h values).
    pub(crate) fn evdev_code(self) -> u32 {
        const KEY_BACKSPACE: u32 = 14;
        const KEY_TAB: u32 = 15;
        const KEY_ENTER: u32 = 28;
        const KEY_ESC: u32 = 1;
        const KEY_SPACE: u32 = 57;
        const KEY_DELETE: u32 = 111;
        const KEY_HOME: u32 = 102;
        const KEY_END: u32 = 106;
        const KEY_PAGEUP: u32 = 104;
        const KEY_PAGEDOWN: u32 = 109;
        const KEY_LEFT: u32 = 105;
        const KEY_RIGHT: u32 = 106;
        const KEY_UP: u32 = 103;
        const KEY_DOWN: u32 = 108;
        match self {
            Key::Char(c) => char_evdev(c),
            Key::Return => KEY_ENTER,
            Key::Tab => KEY_TAB,
            Key::Backspace => KEY_BACKSPACE,
            Key::Delete => KEY_DELETE,
            Key::Escape => KEY_ESC,
            Key::Space => KEY_SPACE,
            Key::Left => KEY_LEFT,
            Key::Right => KEY_RIGHT,
            Key::Up => KEY_UP,
            Key::Down => KEY_DOWN,
            Key::Home => KEY_HOME,
            Key::End => KEY_END,
            Key::PageUp => KEY_PAGEUP,
            Key::PageDown => KEY_PAGEDOWN,
        }
    }
}

/// evdev code for a printable ASCII char on the standard PC 105-key layout
/// (unshifted symbol row).
fn char_evdev(c: char) -> u32 {
    const ROW_NUM: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 1..0
    match c {
        '1'..='9' => ROW_NUM[(c as u8 - b'1') as usize],
        '0' => ROW_NUM[9],
        'a'..='z' | 'A'..='Z' => {
            let lower = c.to_ascii_lowercase();
            30 + (lower as u8 - b'a') as u32
        }
        ',' => 51,
        '.' => 52,
        '/' => 53,
        ';' => 39,
        '\'' => 40,
        '[' => 26,
        ']' => 27,
        '\\' => 43,
        '-' => 12,
        '=' => 13,
        '`' => 41,
        _ => 0,
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub(crate) fn evdev_code(self) -> u32 {
        match self {
            Button::Left => 0x110,   // BTN_LEFT
            Button::Right => 0x111,  // BTN_RIGHT
            Button::Middle => 0x112, // BTN_MIDDLE
        }
    }
}

/// Keyboard key state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// Scroll delta in discrete wheel steps (positive = down / right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollDelta {
    pub vertical: i32,
    pub horizontal: i32,
}

impl ScrollDelta {
    pub fn vertical(steps: i32) -> Self {
        ScrollDelta {
            vertical: steps,
            horizontal: 0,
        }
    }
}

/// The session's current pointer position and button state, queried via
/// [`crate::WaylandServer::pointer_state`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PointerState {
    /// Pointer position in output pixels.
    pub x: f64,
    pub y: f64,
    /// True if any button is currently held.
    pub button_down: bool,
}
