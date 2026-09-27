//! Input injection into the headless session.
//!
//! The terminal side (ratatui/crossterm) produces keys and mouse events; this
//! module translates them into Wayland input for the caged app:
//! - keyboard: `zwp_virtual_keyboard_manager_v1`
//! - pointer:  `zwlr_virtual_pointer_v1` (wlroots-native)
//!
//! The wire-level client is not wired yet (M3); this module fixes the API and
//! provides the event translation tables so the TUI side can be built now.

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

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

/// A live wayland input client that injects events into the headless cage
/// session. Connects to the socket, binds
/// `zwlr_virtual_pointer_manager_v1`, and turns [`PointerEvent`]s into
/// wayland requests on a single virtual pointer.
pub struct InputSock {
    _connection: Connection,
    queue: EventQueue<InputState>,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    state: InputState,
    /// Frame size (px) of the virtual output, for normalizing absolute coords.
    frame_w: u32,
    frame_h: u32,
}

/// Mutable dispatch state carried by the virtual-pointer queue. Kept separate
/// from the proxy handles so dispatch never aliases them (mirrors `capture.rs`).
#[derive(Default)]
struct InputState {
    _seat: Option<WlSeat>,
}

impl InputSock {
    /// Open a wayland connection to `socket` and create a virtual pointer.
    pub fn connect(socket: &std::path::Path) -> Result<InputSock, CageError> {
        if !socket.exists() {
            return Err(CageError::Input(format!(
                "socket {} does not exist",
                socket.display()
            )));
        }
        let dir = socket.parent().unwrap().to_path_buf();
        let name = socket
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        std::env::set_var("WAYLAND_DISPLAY", &name);

        let conn = Connection::connect_to_env().map_err(|e| CageError::Input(e.to_string()))?;
        let (globals, mut queue) = registry_queue_init::<InputState>(&conn)
            .map_err(|e| CageError::Input(format!("registry init: {e}")))?;

        let qh = queue.handle();
        let _output = globals
            .bind::<WlOutput, _, _>(&qh, 1..=1, ())
            .map_err(|e| CageError::Input(format!("bind output: {e}")))?;
        let manager = globals
            .bind::<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _, _>(
                &qh,
                1..=2,
                (),
            )
            .map_err(|e| CageError::Input(format!("bind virtual pointer manager: {e}")))?;

        let mut state = InputState::default();
        queue
            .roundtrip(&mut state)
            .map_err(|e| CageError::Input(e.to_string()))?;

        // Create a virtual pointer; no seat requested (compositor picks one).
        // `create_virtual_pointer` has no reply event, so no further roundtrip
        // is needed — sending the request is enough and avoids a possible hang
        // waiting on a server that never acks a new inert object.
        let pointer = manager.create_virtual_pointer(None, &qh, ());

        Ok(InputSock {
            _connection: conn,
            queue,
            pointer,
            state,
            frame_w: 0,
            frame_h: 0,
        })
    }

    /// Record the virtual-output pixel size so absolute pointer coords can be
    /// normalized. Call with the captured frame's dimensions.
    pub fn set_frame_size(&mut self, w: u32, h: u32) {
        self.frame_w = w;
        self.frame_h = h;
    }

    /// Render one pointer event onto the virtual pointer and push to wire.
    fn apply(&mut self, ev: PointerEvent) -> Result<(), CageError> {
        match ev {
            PointerEvent::Motion { x, y } => {
                // Absolute pixel coords over the virtual output.
                let ex = self.frame_w.max(1);
                let ey = self.frame_h.max(1);
                self.pointer
                    .motion_absolute(now_ms(), x.round() as u32, y.round() as u32, ex, ey);
            }
            PointerEvent::Button { code, state } => {
                let s = match state {
                    KeyState::Press => ButtonState::Pressed,
                    KeyState::Release => ButtonState::Released,
                };
                self.pointer.button(now_ms(), code, s);
            }
            PointerEvent::Axis { dy, .. } => {
                // A positive value on the vertical axis = scroll away from the
                // user (mouse wheel up); negative wheel deltas are the norm.
                let value = (if dy.is_sign_positive() { 1.0 } else { -1.0 }) * dy.abs().max(0.5);
                self.pointer.axis(now_ms(), Axis::VerticalScroll, value);
                self.pointer.axis_discrete(now_ms(), Axis::VerticalScroll, value, value as i32);
            }
        }
        // Finalize the sequence; the compositor treats these as one unit.
        self.pointer.frame();
        // Actually send bytes, then process any already-queued replies (we
        // never block waiting for input events back from a virtual pointer).
        self.queue
            .flush()
            .map_err(|e| CageError::Input(format!("flush: {e}")))?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| CageError::Input(format!("dispatch: {e}")))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl InputSink for InputSock {
    async fn send_key(&mut self, ev: KeyEvent) -> Result<(), CageError> {
        let _ = ev; // keyboard forwarding is on the next step; mouse first.
        Ok(())
    }

    async fn send_pointer(&mut self, ev: PointerEvent) -> Result<(), CageError> {
        self.apply(ev)
    }
}

fn now_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// The virtual pointer object we create never fires events back at us; ignore
// it and the manager entirely.
wayland_client::delegate_noop!(InputState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(InputState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(InputState: ignore WlSeat);
wayland_client::delegate_noop!(InputState: ignore WlOutput);

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
