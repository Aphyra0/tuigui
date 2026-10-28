//! Input shapes: the vocabulary is logical — a key, a button, a wheel step —
//! and only the evdev mapping below is Linux's business.

/// One queued input event. Positions are window-local pixels, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// A key, by the character or the named key it is.
    Key { key: Key, state: KeyState },
    /// The pointer moved to a position inside the window.
    ///
    /// `held` is the button carried down while the move happened: a drag. A
    /// plain motion carries none.
    MouseMove {
        x: f64,
        y: f64,
        held: Option<Button>,
    },
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

/// The evdev code (linux/input-event-codes.h values) for a key.
pub(crate) fn key_evdev(key: Key) -> u32 {
    const KEY_BACKSPACE: u32 = 14;
    const KEY_TAB: u32 = 15;
    const KEY_ENTER: u32 = 28;
    const KEY_ESC: u32 = 1;
    const KEY_SPACE: u32 = 57;
    const KEY_DELETE: u32 = 111;
    const KEY_HOME: u32 = 102;
    const KEY_END: u32 = 107;
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
///
/// Hand-written rather than derived, because evdev has no arithmetic to it: the letters
/// are contiguous only up to `h` (`a`..`h` are 30..=35), then `i` is 23, and `j`..`l`
/// are 36..=38. `;` really is 39, so an additive table that assumed `a`..`z` was one run
/// typed `;` for `j`.
fn char_evdev(c: char) -> u32 {
    const ROW_NUM: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 1..0
    match c.to_ascii_lowercase() {
        '1'..='9' => ROW_NUM[(c as u8 - b'1') as usize],
        '0' => ROW_NUM[9],
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
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

