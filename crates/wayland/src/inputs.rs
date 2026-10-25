//! Input shapes: the vocabulary is `proto`'s, so a caller writes the same code
//! on every platform; only the evdev mapping below is Linux's business.

pub use proto::windowing::{Button, Input, Key, KeyState, ScrollDelta};

/// The session's current pointer position and button state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PointerState {
    /// Pointer position in output pixels.
    pub x: f64,
    pub y: f64,
    /// True if any button is currently held.
    pub button_down: bool,
}

/// The evdev code (linux/input-event-codes.h values) for a key.
pub(crate) fn key_evdev(key: Key) -> u32 {
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
    match key {
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

/// The evdev code for a pointer button.
pub(crate) fn button_evdev(button: Button) -> u32 {
    match button {
        Button::Left => 0x110,    // BTN_LEFT
        Button::Middle => 0x112,  // BTN_MIDDLE
        Button::Right => 0x111,   // BTN_RIGHT
        Button::Back => 0x113,    // BTN_SIDE
        Button::Forward => 0x114, // BTN_EXTRA
    }
}
