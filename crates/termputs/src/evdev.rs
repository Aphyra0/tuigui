//! The bridge: a terminal's events in the form Wayland takes.
//!
//! Wayland's input currency is an evdev **keycode** -- a position on the
//! keyboard -- which a client resolves into a character through the keymap the
//! compositor sent it. A terminal's currency is a character plus modifiers, and
//! in kitty's protocol a Unicode codepoint. Both sides agree on *which physical
//! key*, so that is the pivot: [`Key::keycode`] maps a terminal key to the
//! keycode Wayland wants.
//!
//! The keycodes are [`kbvm::Keycode`]s, named by [`kbvm::evdev`] rather than
//! written as numbers. A Wayland keycode is an evdev code plus 8 (X11's offset),
//! and the compositor performs that conversion itself when it builds its own
//! keycode, so what leaves this module is the evdev form.
//!
//! # Modifiers are keys, not a mask
//!
//! The compositor's injection takes one keycode and a press/release state -- it
//! has no modifier parameter, because on a seat a modifier *is* a key press: the
//! seat derives its XKB state from the keys it sees. So `Ctrl+C` is not one
//! event but a sequence: press the control key, press and release `c`, release
//! the control key. [`Modifiers::keycodes`] is the first step of that, so a
//! caller does not have to know which keycodes mean which modifier.

use kbvm::{evdev, Keycode};
use terminput::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    ScrollDirection,
};

/// The name of the keyboard layout these keycodes assume.
///
/// The compositor's seat is configured with a keymap (`XkbConfig::default()` in
/// `crates/compositor/src/state.rs`), and the character table below only
/// produces the characters a user expects if that keymap is the standard PC-105
/// layout. A different layout needs a different table, so this is a name and a
/// deliberate assumption rather than a silent one.
pub const KEYMAP_NAME: &str = "pc105/us";

/// Which modifiers a key event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const SUPER: u8 = 1 << 3;
    pub const HYPER: u8 = 1 << 4;
    pub const META: u8 = 1 << 5;

    /// No modifiers.
    pub const NONE: Self = Self(0);

    /// A modifier set from raw bits.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw bits, for a caller that wants to match a specific set.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Whether every modifier in `mask` is held.
    pub fn contains(self, mask: u8) -> bool {
        self.0 & mask == mask
    }

    /// The keys that have to be held for these modifiers to be in effect.
    ///
    /// A seat learns its modifiers from the keys it is sent, so delivering a
    /// shortcut means pressing these first. Only the left-hand keys are named,
    /// which is what a terminal can tell apart: the input says `ctrl`, never
    /// which of the two control keys a finger is on.
    pub fn keycodes(self) -> Vec<Keycode> {
        let mut keys = Vec::new();
        if self.contains(Self::SHIFT) {
            keys.push(evdev::LEFTSHIFT);
        }
        if self.contains(Self::CTRL) {
            keys.push(evdev::LEFTCTRL);
        }
        if self.contains(Self::ALT) {
            keys.push(evdev::LEFTALT);
        }
        if self.contains(Self::SUPER) {
            keys.push(evdev::LEFTMETA);
        }
        keys
    }

    /// Translate the terminal's modifier flags into these bits.
    fn from_terminput(m: KeyModifiers) -> Self {
        let mut bits = 0;
        if m.contains(terminput::SHIFT) {
            bits |= Self::SHIFT;
        }
        if m.contains(terminput::CTRL) {
            bits |= Self::CTRL;
        }
        if m.contains(terminput::ALT) {
            bits |= Self::ALT;
        }
        if m.contains(terminput::SUPER) {
            bits |= Self::SUPER;
        }
        if m.contains(terminput::HYPER) {
            bits |= Self::HYPER;
        }
        if m.contains(terminput::META) {
            bits |= Self::META;
        }
        Self(bits)
    }
}

/// Whether a key went down, came up, or repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
    Repeated,
}

/// A key, as the terminal reported it, with the modifiers that were held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// The key itself: a character for the printable keys, a name otherwise.
    pub code: KeyCode,
    /// What was held when it was pressed.
    pub modifiers: Modifiers,
}

impl Key {
    /// The keycode Wayland wants for this key, or `None` when it has none.
    ///
    /// `None` is a real answer and not an error: a terminal can report a media
    /// key or a paste payload that has no key position, and inventing one would
    /// type something the user did not press. The caller decides; this says only
    /// what it knows.
    pub fn keycode(&self) -> Option<Keycode> {
        Some(match self.code {
            KeyCode::Char(c) => char_keycode(c)?,
            KeyCode::Enter => evdev::ENTER,
            KeyCode::Esc => evdev::ESC,
            KeyCode::Backspace => evdev::BACKSPACE,
            KeyCode::Tab => evdev::TAB,
            KeyCode::Delete => evdev::DELETE,
            KeyCode::Insert => evdev::INSERT,
            KeyCode::Left => evdev::LEFT,
            KeyCode::Right => evdev::RIGHT,
            KeyCode::Up => evdev::UP,
            KeyCode::Down => evdev::DOWN,
            KeyCode::Home => evdev::HOME,
            KeyCode::End => evdev::END,
            KeyCode::PageUp => evdev::PAGEUP,
            KeyCode::PageDown => evdev::PAGEDOWN,
            KeyCode::CapsLock => evdev::CAPSLOCK,
            KeyCode::ScrollLock => evdev::SCROLLLOCK,
            KeyCode::NumLock => evdev::NUMLOCK,
            KeyCode::PrintScreen => evdev::SYSRQ,
            KeyCode::Pause => evdev::PAUSE,
            KeyCode::Menu => evdev::MENU,
            KeyCode::F(n) => function_keycode(n)?,
            KeyCode::Modifier(kind, direction) => modifier_keycode(kind, direction)?,
            // A media key is not a key position a seat maps, and KeypadBegin is
            // the keypad's 5, which this table does not carry.
            KeyCode::Media(_) | KeyCode::KeypadBegin => return None,
        })
    }

    /// The character this key produces, when it produces one.
    ///
    /// Straight from the report: kitty sends the unshifted codepoint, so the
    /// shift is applied here and a caller does not have to. This is the same
    /// information the compositor's seat re-derives from the keymap, exposed
    /// because a caller like a shortcut matcher wants the character without a
    /// round trip.
    pub fn character(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if self.modifiers.contains(Modifiers::SHIFT) => {
                c.to_uppercase().next()
            }
            KeyCode::Char(c) => Some(c),
            _ => None,
        }
    }
}

/// The keycode for F1..F12, which are not one contiguous run.
fn function_keycode(n: u8) -> Option<Keycode> {
    Some(match n {
        1 => evdev::F1,
        2 => evdev::F2,
        3 => evdev::F3,
        4 => evdev::F4,
        5 => evdev::F5,
        6 => evdev::F6,
        7 => evdev::F7,
        8 => evdev::F8,
        9 => evdev::F9,
        10 => evdev::F10,
        11 => evdev::F11,
        12 => evdev::F12,
        // F13 and up exist in evdev but a terminal does not send them, so this
        // declines rather than picking a number.
        _ => return None,
    })
}

/// The keycode for a modifier key.
///
/// The direction says which side of the keyboard it came from, when the input
/// distinguishes them; a terminal usually does not, and the left-hand key is the
/// answer for `Unspecified` because that is the one a shortcut is written
/// against.
fn modifier_keycode(
    kind: terminput::ModifierKeyCode,
    direction: terminput::ModifierDirection,
) -> Option<Keycode> {
    use terminput::{ModifierDirection, ModifierKeyCode};
    let right = matches!(direction, ModifierDirection::Right);
    Some(match kind {
        ModifierKeyCode::Shift if right => evdev::RIGHTSHIFT,
        ModifierKeyCode::Shift => evdev::LEFTSHIFT,
        ModifierKeyCode::Control if right => evdev::RIGHTCTRL,
        ModifierKeyCode::Control => evdev::LEFTCTRL,
        ModifierKeyCode::Alt if right => evdev::RIGHTALT,
        ModifierKeyCode::Alt => evdev::LEFTALT,
        ModifierKeyCode::Super if right => evdev::RIGHTMETA,
        ModifierKeyCode::Super => evdev::LEFTMETA,
        // Hyper, Meta and the ISO level shifts have no key every keyboard has;
        // declining is honest rather than picking one.
        _ => return None,
    })
}

/// The keycode for a printable character on the standard PC-105 layout.
///
/// Uppercase and lowercase are the same key position -- the shift is carried in
/// the modifiers, not the key -- which is what makes `Shift+A` representable
/// here. That is the difference from the compositor's hand-written table, which
/// lowercased and so could not express a shifted character at all.
fn char_keycode(c: char) -> Option<Keycode> {
    // The digit row, and the symbol each digit produces when shifted.
    const ROW: [Keycode; 10] = [
        evdev::_1,
        evdev::_2,
        evdev::_3,
        evdev::_4,
        evdev::_5,
        evdev::_6,
        evdev::_7,
        evdev::_8,
        evdev::_9,
        evdev::_0,
    ];
    const SHIFTED: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];

    if let Some(digit) = c.to_digit(10) {
        // `1`..`9` are indices 0..8; `0` is the last.
        let index = if digit == 0 { 9 } else { digit as usize - 1 };
        return Some(ROW[index]);
    }
    if let Some(index) = SHIFTED.iter().position(|s| *s == c) {
        return Some(ROW[index]);
    }
    Some(match c.to_ascii_lowercase() {
        'a' => evdev::A,
        'b' => evdev::B,
        'c' => evdev::C,
        'd' => evdev::D,
        'e' => evdev::E,
        'f' => evdev::F,
        'g' => evdev::G,
        'h' => evdev::H,
        'i' => evdev::I,
        'j' => evdev::J,
        'k' => evdev::K,
        'l' => evdev::L,
        'm' => evdev::M,
        'n' => evdev::N,
        'o' => evdev::O,
        'p' => evdev::P,
        'q' => evdev::Q,
        'r' => evdev::R,
        's' => evdev::S,
        't' => evdev::T,
        'u' => evdev::U,
        'v' => evdev::V,
        'w' => evdev::W,
        'x' => evdev::X,
        'y' => evdev::Y,
        'z' => evdev::Z,
        // The punctuation keys, each with the symbol it produces when shifted.
        ' ' => evdev::SPACE,
        ',' | '<' => evdev::COMMA,
        '.' | '>' => evdev::DOT,
        '/' | '?' => evdev::SLASH,
        ';' | ':' => evdev::SEMICOLON,
        '\'' | '"' => evdev::APOSTROPHE,
        '[' | '{' => evdev::LEFTBRACE,
        ']' | '}' => evdev::RIGHTBRACE,
        '\\' | '|' => evdev::BACKSLASH,
        '-' | '_' => evdev::MINUS,
        '=' | '+' => evdev::EQUAL,
        '`' | '~' => evdev::GRAVE,
        _ => return None,
    })
}

/// A mouse button, as Wayland names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// The evdev code for this button.
    pub fn evdev(self) -> u32 {
        match self {
            Button::Left => 0x110,   // BTN_LEFT
            Button::Right => 0x111,  // BTN_RIGHT
            Button::Middle => 0x112, // BTN_MIDDLE
        }
    }
}

/// Convert a button, or `None` for one the protocol does not name.
fn button_from(button: MouseButton) -> Option<Button> {
    Some(match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
        MouseButton::Unknown => return None,
    })
}

/// A wheel or trackpad scroll step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Up,
    Down,
    Left,
    Right,
}

impl From<ScrollDirection> for Scroll {
    fn from(direction: ScrollDirection) -> Self {
        match direction {
            ScrollDirection::Up => Scroll::Up,
            ScrollDirection::Down => Scroll::Down,
            ScrollDirection::Left => Scroll::Left,
            ScrollDirection::Right => Scroll::Right,
        }
    }
}

/// The kinds of mouse event a terminal reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down(Button),
    Up(Button),
    /// Motion with a button held.
    Drag(Button),
    /// Motion with none held.
    Moved,
    Scroll(Scroll),
}

/// What a mouse did, at a position in the terminal's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// The column, in cells.
    pub column: u16,
    /// The row, in cells.
    pub row: u16,
    /// What happened.
    pub kind: MouseKind,
}

/// One thing the terminal reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key { key: Key, state: KeyState },
    Mouse(MouseEvent),
    /// Text pasted as a unit, which is not a run of keystrokes.
    Paste(String),
    Resize { rows: u32, cols: u32 },
    FocusGained,
    FocusLost,
}

impl InputEvent {
    /// Parse every complete event in `bytes`.
    ///
    /// A read can end mid-sequence, and `terminput` reports an incomplete tail
    /// as `None`; there is no event for it to become, so it is dropped. The cost
    /// is one lost keystroke when a terminal splits a sequence across two
    /// writes, which is why a caller should read in as large a chunk as the
    /// terminal will give.
    pub fn parse_all(bytes: &[u8]) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            match Event::parse_from(rest) {
                Ok(None) => break,
                Ok(Some(event)) => {
                    // `parse_from` consumes one event but does not report how
                    // many bytes, so the length is recovered by encoding the
                    // event back. Without it the next iteration would re-parse
                    // the same bytes forever.
                    let Some(consumed) = consumed_len(&event) else {
                        break;
                    };
                    if consumed == 0 || consumed > rest.len() {
                        break;
                    }
                    events.extend(convert(event));
                    rest = &rest[consumed..];
                }
                // Unparseable from here on. Dropping the rest is the same
                // decision the compositor's own parser made.
                Err(_) => break,
            }
        }
        events
    }
}

/// How many bytes `event` occupied in the input.
///
/// `terminput` does not report it, so it is recovered by re-encoding. Two kinds
/// cannot be re-encoded: a paste is framed rather than encoded, and a resize is
/// not encoded at all. A paste is handled by its framing; a resize cannot be
/// reconstructed here and so returns `None`, which stops the parse rather than
/// risking a loop.
fn consumed_len(event: &Event) -> Option<usize> {
    match event {
        Event::Paste(text) => Some(PASTE_START.len() + text.len() + PASTE_END.len()),
        Event::Resize { .. } => None,
        other => {
            let mut buf = [0u8; 64];
            other
                .encode(&mut buf, terminput::Encoding::Xterm)
                .ok()
                .filter(|n| *n > 0)
        }
    }
}

/// The two halves of a bracketed-paste frame.
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// The characters that only appear when shift is held.
///
/// A legacy terminal sends these for the shifted form of a key with no modifier
/// bits to say so, so the character is the only evidence that shift was down.
/// Uppercase letters are not listed: their case is the evidence.
const SHIFTED_SYMBOLS: [char; 21] = [
    '!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+', '{', '}', '|', ':', '"',
    '<', '>', '?', '~',
];

/// Turn one parsed event into ours.
fn convert(event: Event) -> Vec<InputEvent> {
    match event {
        Event::Key(key) => {
            let state = match key.kind {
                KeyEventKind::Press => KeyState::Pressed,
                KeyEventKind::Repeat => KeyState::Repeated,
                KeyEventKind::Release => KeyState::Released,
            };
            let mut modifiers = Modifiers::from_terminput(key.modifiers);
            // A legacy terminal reports no modifier bits at all: a shifted key
            // arrives as the shifted character and nothing else. `A` is therefore
            // read here as `A`, and the shift has to be recovered from the
            // character itself or the program would be typed a lowercase `a`.
            //
            // Only inferred when the report carried no shift, so a terminal that
            // does say so is believed over this guess.
            if !modifiers.contains(Modifiers::SHIFT) {
                if let KeyCode::Char(c) = key.code {
                    if c.is_uppercase() || SHIFTED_SYMBOLS.contains(&c) {
                        modifiers = Modifiers::from_bits(modifiers.bits() | Modifiers::SHIFT);
                    }
                }
            }
            vec![InputEvent::Key {
                key: Key {
                    code: key.code,
                    modifiers,
                },
                state,
            }]
        }
        Event::Mouse(mouse) => {
            let kind = match mouse.kind {
                MouseEventKind::Down(button) => match button_from(button) {
                    Some(button) => MouseKind::Down(button),
                    None => return Vec::new(),
                },
                MouseEventKind::Up(button) => match button_from(button) {
                    Some(button) => MouseKind::Up(button),
                    None => return Vec::new(),
                },
                MouseEventKind::Drag(button) => match button_from(button) {
                    Some(button) => MouseKind::Drag(button),
                    None => return Vec::new(),
                },
                MouseEventKind::Moved => MouseKind::Moved,
                MouseEventKind::Scroll(direction) => {
                    MouseKind::Scroll(direction.into())
                }
            };
            vec![InputEvent::Mouse(MouseEvent {
                column: mouse.column,
                row: mouse.row,
                kind,
            })]
        }
        Event::Paste(text) => vec![InputEvent::Paste(text)],
        Event::Resize { rows, cols } => vec![InputEvent::Resize { rows, cols }],
        Event::FocusGained => vec![InputEvent::FocusGained],
        Event::FocusLost => vec![InputEvent::FocusLost],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: Modifiers) -> Key {
        Key { code, modifiers }
    }

    /// A letter is a key position, and its case is the shift, not the key.
    #[test]
    fn letters_share_a_key_and_shift_decides_the_case() {
        let lower = key(KeyCode::Char('a'), Modifiers::NONE);
        let upper = key(KeyCode::Char('a'), Modifiers::from_bits(Modifiers::SHIFT));
        assert_eq!(lower.keycode(), Some(evdev::A));
        assert_eq!(
            upper.keycode(),
            lower.keycode(),
            "shift must not move the key position"
        );
        assert_eq!(upper.character(), Some('A'));
    }

    /// A shifted digit is a symbol on the same key position.
    #[test]
    fn shifted_symbols_land_on_the_digit_row() {
        let one = key(KeyCode::Char('1'), Modifiers::NONE);
        let bang = key(KeyCode::Char('!'), Modifiers::from_bits(Modifiers::SHIFT));
        assert_eq!(one.keycode(), Some(evdev::_1));
        assert_eq!(bang.keycode(), Some(evdev::_1));
    }

    /// Ctrl is a modifier, not a different key, which is the whole reason the
    /// legacy encoding could not express `C-c`.
    #[test]
    fn ctrl_is_a_modifier_not_a_keycode() {
        let ctrl_c = key(KeyCode::Char('c'), Modifiers::from_bits(Modifiers::CTRL));
        assert_eq!(ctrl_c.keycode(), Some(evdev::C));
        assert!(ctrl_c.modifiers.contains(Modifiers::CTRL));
        assert_eq!(
            ctrl_c.modifiers.keycodes(),
            [evdev::LEFTCTRL],
            "a seat learns ctrl from the ctrl key, so it has to be pressed"
        );
    }

    /// The function keys are the ones the compositor could not deliver at all.
    #[test]
    fn function_keys_have_positions() {
        assert_eq!(key(KeyCode::F(5), Modifiers::NONE).keycode(), Some(evdev::F5));
        assert_eq!(function_keycode(13), None, "unmeasured is not a position");
    }

    /// A key with no position says so rather than becoming one.
    #[test]
    fn unknown_keys_are_not_invented() {
        assert_eq!(
            key(KeyCode::Media(terminput::MediaKeyCode::Play), Modifiers::NONE)
                .keycode(),
            None
        );
        assert_eq!(key(KeyCode::Char('世'), Modifiers::NONE).keycode(), None);
    }

    /// A printable run parses into one event per character.
    #[test]
    fn a_run_of_letters_is_a_run_of_events() {
        let events = InputEvent::parse_all(b"hi");
        assert_eq!(events.len(), 2, "each byte is its own key event");
    }
}
