//! Input injection into the headless session.
//!
//! The terminal side (ratatui/crossterm) produces keys and mouse events; this
//! module translates them into Wayland input for the caged app:
//! - keyboard: `zwp_virtual_keyboard_manager_v1`
//! - pointer:  `zwlr_virtual_pointer_v1` (wlroots-native)
//!
//! The wire-level client is not wired yet (M3); this module fixes the API and
//! provides the event translation tables so the TUI side can be built now.

use crate::CageError;

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
    Motion {
        x: f64,
        y: f64,
    },
    Button {
        code: u32,
        state: KeyState,
    },
    /// Scroll: positive = away from user (up).
    Axis {
        dx: f64,
        dy: f64,
    },
}

/// Sink for input events; one instance per session.
#[async_trait::async_trait]
pub trait InputSink: Send {
    async fn send_key(&mut self, ev: KeyEvent) -> Result<(), CageError>;
    async fn send_pointer(&mut self, ev: PointerEvent) -> Result<(), CageError>;
}

/// Not-yet-wired sink that records what it was asked to inject. Used by tests
/// and as the M3 placeholder so the TUI can ship before the Wayland glue.
#[derive(Default)]
pub struct RecordingSink {
    pub keys: Vec<KeyEvent>,
    pub pointers: Vec<PointerEvent>,
}

#[async_trait::async_trait]
impl InputSink for RecordingSink {
    async fn send_key(&mut self, ev: KeyEvent) -> Result<(), CageError> {
        self.keys.push(ev);
        Ok(())
    }

    async fn send_pointer(&mut self, ev: PointerEvent) -> Result<(), CageError> {
        self.pointers.push(ev);
        Ok(())
    }
}

/// crossterm mouse button -> evdev button code (BTN_LEFT 0x110).
pub fn crossterm_button_to_evdev(button: crossterm_stub::MouseButton) -> u32 {
    match button {
        crossterm_stub::MouseButton::Left => 0x110,
        crossterm_stub::MouseButton::Right => 0x111,
        crossterm_stub::MouseButton::Middle => 0x112,
    }
}

/// Tiny local re-declaration of the three mouse buttons we care about, so the
/// cage crate does not need to depend on crossterm.
pub mod crossterm_stub {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MouseButton {
        Left,
        Right,
        Middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recording_sink_records() {
        let mut sink = RecordingSink::default();
        sink.send_key(KeyEvent {
            code: 30,
            state: KeyState::Press,
        })
        .await
        .unwrap();
        sink.send_pointer(PointerEvent::Motion { x: 1.0, y: 2.0 })
            .await
            .unwrap();
        assert_eq!(sink.keys.len(), 1);
        assert_eq!(sink.pointers.len(), 1);
        assert_eq!(
            crossterm_button_to_evdev(crossterm_stub::MouseButton::Left),
            0x110
        );
    }
}
