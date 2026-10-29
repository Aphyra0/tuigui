//! A terminal's events, as W3C UI Events.
//!
//! Nothing here knows about windows, focus or stacking: this is the dialect
//! translation and nothing else. A terminal reports a key as a *character* plus a
//! modifier mask and a mouse as a *cell*; UI Events describes a key by the
//! *position* that produced it and a pointer by a pixel position. Turning one into
//! the other is the whole of this module, and it is a pure function of its input —
//! who the event then belongs to is the compositor's question.
//!
//! # The one thing a terminal cannot say
//!
//! A terminal has no notion of a key's position, so [`char_code`] names the
//! position each character sits at on the standard PC-105 layout: the same
//! assumption the keymap a seat hands its clients already makes. That table is the
//! price of translating a character-oriented source into a position-oriented
//! vocabulary, and it is paid once here rather than once per backend.

use ui_events::ScrollDelta;
use ui_events::keyboard::{
    Code, Key, KeyState, KeyboardEvent, Location, Modifiers as KeyModifiers, NamedKey,
};
use ui_events::pointer::{
    PointerButton, PointerButtonEvent, PointerEvent, PointerId, PointerInfo, PointerScrollEvent,
    PointerState, PointerType, PointerUpdate,
};

/// The pointer's identity, as every event from a terminal carries it.
///
/// A terminal has one pointer and no way to name it, so it is the primary one: the
/// id that means "the mouse" rather than "a touch contact".
pub fn pointer_info() -> PointerInfo {
    PointerInfo {
        pointer_id: Some(PointerId::PRIMARY),
        persistent_device_id: None,
        pointer_type: PointerType::Mouse,
    }
}

/// A terminal key, as a UI Events keyboard event.
pub fn key(key: &termina::event::KeyEvent) -> KeyboardEvent {
    use termina::event::{KeyCode, KeyEventKind};

    let (code, logical) = match key.code {
        KeyCode::Char(' ') => (Code::Space, Key::Character(" ".into())),
        KeyCode::Char(c) => (
            char_code(c),
            Key::Character(c.to_lowercase().to_string().into()),
        ),
        KeyCode::Enter => (Code::Enter, Key::Named(NamedKey::Enter)),
        // A back-tab is the tab key with shift held, which `modifiers` already
        // says: there is one position, not two.
        KeyCode::Tab | KeyCode::BackTab => (Code::Tab, Key::Named(NamedKey::Tab)),
        KeyCode::Backspace => (Code::Backspace, Key::Named(NamedKey::Backspace)),
        KeyCode::Delete => (Code::Delete, Key::Named(NamedKey::Delete)),
        KeyCode::Escape => (Code::Escape, Key::Named(NamedKey::Escape)),
        KeyCode::Left => (Code::ArrowLeft, Key::Named(NamedKey::ArrowLeft)),
        KeyCode::Right => (Code::ArrowRight, Key::Named(NamedKey::ArrowRight)),
        KeyCode::Up => (Code::ArrowUp, Key::Named(NamedKey::ArrowUp)),
        KeyCode::Down => (Code::ArrowDown, Key::Named(NamedKey::ArrowDown)),
        KeyCode::Home => (Code::Home, Key::Named(NamedKey::Home)),
        KeyCode::End => (Code::End, Key::Named(NamedKey::End)),
        KeyCode::PageUp => (Code::PageUp, Key::Named(NamedKey::PageUp)),
        KeyCode::PageDown => (Code::PageDown, Key::Named(NamedKey::PageDown)),
        // A key this vocabulary has no position for. `Unidentified` is the
        // protocol's own answer, and the seat then has nothing to look up.
        _ => (Code::Unidentified, Key::Named(NamedKey::Unidentified)),
    };
    KeyboardEvent {
        state: match key.kind {
            KeyEventKind::Release => KeyState::Up,
            _ => KeyState::Down,
        },
        key: logical,
        code,
        location: Location::Standard,
        modifiers: modifiers(key.modifiers),
        repeat: matches!(key.kind, KeyEventKind::Repeat),
        is_composing: false,
    }
}

/// A terminal mouse event, as a UI Events pointer event.
///
/// `at` is where the pointer is in the receiving window's own cells, which is the
/// caller's arithmetic, not this module's: a pane position and a window position
/// differ by the window's place in the stack, and only the compositor knows that.
///
/// The button a motion carries is read from the event: UI Events puts the pressed
/// buttons in the state rather than in the event, so a drag is a move whose state
/// still says a button is down.
pub fn pointer(mouse: &termina::event::MouseEvent, at: (f64, f64)) -> Option<PointerEvent> {
    use termina::event::MouseEventKind;

    let mut state = PointerState {
        position: dpi::PhysicalPosition { x: at.0, y: at.1 },
        ..PointerState::default()
    };
    if let MouseEventKind::Drag(button) = mouse.kind {
        state.buttons.insert(button_of(button)?);
    }
    Some(match mouse.kind {
        MouseEventKind::Down(button) => PointerEvent::Down(PointerButtonEvent {
            button: button_of(button),
            pointer: pointer_info(),
            state,
        }),
        MouseEventKind::Up(button) => PointerEvent::Up(PointerButtonEvent {
            button: button_of(button),
            pointer: pointer_info(),
            state,
        }),
        MouseEventKind::Drag(_) | MouseEventKind::Moved => PointerEvent::Move(PointerUpdate {
            pointer: pointer_info(),
            current: state,
            coalesced: Vec::new(),
            predicted: Vec::new(),
        }),
        // A wheel reports lines, which is the unit a terminal's scroll already is:
        // one event is one step.
        MouseEventKind::ScrollUp => scroll(state, ScrollDelta::LineDelta(0.0, -1.0)),
        MouseEventKind::ScrollDown => scroll(state, ScrollDelta::LineDelta(0.0, 1.0)),
        MouseEventKind::ScrollLeft => scroll(state, ScrollDelta::LineDelta(-1.0, 0.0)),
        MouseEventKind::ScrollRight => scroll(state, ScrollDelta::LineDelta(1.0, 0.0)),
    })
}

fn scroll(state: PointerState, delta: ScrollDelta) -> PointerEvent {
    PointerEvent::Scroll(PointerScrollEvent {
        pointer: pointer_info(),
        delta,
        state,
    })
}

/// The position a character sits at on the standard PC-105 layout.
///
/// A terminal does not report a position, so this is the layout assumed in
/// exchange for one: the unshifted symbol row, and letters by their lowercase form
/// because a terminal reports `A` for one key with shift held.
fn char_code(c: char) -> Code {
    match c.to_ascii_lowercase() {
        'a' => Code::KeyA,
        'b' => Code::KeyB,
        'c' => Code::KeyC,
        'd' => Code::KeyD,
        'e' => Code::KeyE,
        'f' => Code::KeyF,
        'g' => Code::KeyG,
        'h' => Code::KeyH,
        'i' => Code::KeyI,
        'j' => Code::KeyJ,
        'k' => Code::KeyK,
        'l' => Code::KeyL,
        'm' => Code::KeyM,
        'n' => Code::KeyN,
        'o' => Code::KeyO,
        'p' => Code::KeyP,
        'q' => Code::KeyQ,
        'r' => Code::KeyR,
        's' => Code::KeyS,
        't' => Code::KeyT,
        'u' => Code::KeyU,
        'v' => Code::KeyV,
        'w' => Code::KeyW,
        'x' => Code::KeyX,
        'y' => Code::KeyY,
        'z' => Code::KeyZ,
        '1' => Code::Digit1,
        '2' => Code::Digit2,
        '3' => Code::Digit3,
        '4' => Code::Digit4,
        '5' => Code::Digit5,
        '6' => Code::Digit6,
        '7' => Code::Digit7,
        '8' => Code::Digit8,
        '9' => Code::Digit9,
        '0' => Code::Digit0,
        ',' => Code::Comma,
        '.' => Code::Period,
        '/' => Code::Slash,
        ';' => Code::Semicolon,
        '\'' => Code::Quote,
        '[' => Code::BracketLeft,
        ']' => Code::BracketRight,
        '\\' => Code::Backslash,
        '-' => Code::Minus,
        '=' => Code::Equal,
        '`' => Code::Backquote,
        _ => Code::Unidentified,
    }
}

/// A terminal's modifier mask, as UI Events names it.
fn modifiers(modifiers: termina::event::Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::empty();
    if modifiers.contains(termina::event::Modifiers::SHIFT) {
        out |= KeyModifiers::SHIFT;
    }
    if modifiers.contains(termina::event::Modifiers::CONTROL) {
        out |= KeyModifiers::CONTROL;
    }
    if modifiers.contains(termina::event::Modifiers::ALT) {
        out |= KeyModifiers::ALT;
    }
    // A terminal's "super" is the Command/Windows key, which UI Events calls
    // Meta. The same key under a different name is the whole reason a shared
    // vocabulary is worth translating into.
    if modifiers.contains(termina::event::Modifiers::SUPER) {
        out |= KeyModifiers::META;
    }
    out
}

/// A terminal button, as UI Events names it.
pub fn button_of(button: termina::event::MouseButton) -> Option<PointerButton> {
    use termina::event::MouseButton;
    Some(match button {
        MouseButton::Left => PointerButton::Primary,
        MouseButton::Middle => PointerButton::Auxiliary,
        MouseButton::Right => PointerButton::Secondary,
    })
}
