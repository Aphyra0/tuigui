//! The seat: draining the windows' input mailboxes and dispatching onto the
//! seat, plus the pointer-state query.

use smithay::input::keyboard::FilterResult;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Point, Serial};
use smithay::wayland::compositor::CompositorClientState;

use crate::state::{ClientState, State};
use crate::window::Input;
use crate::{Button, CompError, KeyState, ScrollDelta};

impl State {
    /// Drain every window's input mailbox and dispatch what was queued.
    ///
    /// Called by `step`, so inputs land at a well-defined point in the
    /// protocol stream. Each queue is dispatched against the window it came
    /// from: a child's inputs focus the child, the root's focus the root.
    pub(crate) fn drain_inputs(&mut self) {
        let mut targets: Vec<(WlSurface, Vec<Input>)> = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_window_inputs(root, &mut targets);
        }
        for (surface, inputs) in targets {
            for input in inputs {
                self.dispatch_input(&surface, input);
            }
        }
    }

    fn dispatch_input(&mut self, focus: &WlSurface, input: Input) {
        let result = match input {
            Input::Key { evdev, state } => self.handle_keycode(focus, evdev, state),
            Input::MouseMove { x, y } => self.handle_mouse_move(focus, x, y),
            Input::Button { button, down } => self.handle_button(focus, button, down),
            Input::Scroll { delta } => self.handle_scroll(focus, delta),
        };
        if let Err(e) = result {
            tracing::debug!("a queued input did not dispatch: {e}");
        }
    }

    /// Deliver a key by its evdev code, with no character resolution here.
    ///
    /// The seat takes a keycode and derives its modifier state from the keys it
    /// sees, so anything that knows a key position can drive it directly. That
    /// is what makes a modifier and a shifted character expressible at all: a
    /// character cannot carry a modifier.
    pub(crate) fn handle_keycode(
        &mut self,
        focus: &WlSurface,
        evdev: u32,
        state: KeyState,
    ) -> Result<(), CompError> {
        let keyboard = self.seat.get_keyboard().ok_or(CompError::NotReady)?;
        if !keyboard.is_focused() {
            let serial = self.next_serial();
            keyboard.set_focus(self, Some(focus.clone()), serial);
        }
        // Wayland keycodes are evdev codes plus 8 (X11's offset), which is the
        // conversion this crate's own `Key` also performs.
        let keycode = smithay::backend::input::Keycode::new(evdev + 8);
        let serial = self.next_serial();
        let time = crate::server_time();
        keyboard.input(
            self,
            keycode,
            match state {
                KeyState::Pressed => smithay::backend::input::KeyState::Pressed,
                KeyState::Released => smithay::backend::input::KeyState::Released,
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward::<()>,
        );
        Ok(())
    }

    pub(crate) fn handle_mouse_move(
        &mut self,
        focus: &WlSurface,
        x: f64,
        y: f64,
    ) -> Result<(), CompError> {
        let (w, h) = self.server_size;
        let x = x.clamp(0.0, w.max(2) as f64 - 1.0);
        let y = y.clamp(0.0, h.max(2) as f64 - 1.0);
        let loc = Point::new(x, y);
        self.pointer_loc = loc;
        // smithay reports surface-local pointer coords as `location * scale`
        // (PointerTarget::motion -> to_client), where the scale it composes with
        // includes the output's. The location injected here is in **output**
        // pixels -- that is what the caller's cell arithmetic produces -- so the
        // two divisions below turn it into the surface-local position the client
        // expects.
        //
        // The client's own content scale is the first: it is set on admission
        // and smithay multiplies by it. The output's advertised scale is the
        // second, and it is not the same number: a client that honours it lays
        // its surface out at `output / scale` logical pixels, so it reads the
        // position in that smaller space. Dividing by only the client scale
        // leaves every coordinate off by the output scale, which is why the
        // pointer drifts away from the cursor as it moves.
        let client_scale = focus
            .client()
            .map(|c| self.compositor_client_state(&c).client_scale())
            .unwrap_or(1.0);
        let client_scale = if client_scale.is_finite() && client_scale > 0.0 {
            client_scale
        } else {
            1.0
        };
        let output_scale = if self.output_scale.is_finite() && self.output_scale > 0.0 {
            self.output_scale
        } else {
            1.0
        };
        let loc = Point::new(
            x / (client_scale * output_scale),
            y / (client_scale * output_scale),
        );
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let serial = self.next_serial();
        let time = crate::server_time();
        pointer.motion(
            self,
            Some((focus.clone(), Point::new(0.0, 0.0))),
            &smithay::input::pointer::MotionEvent {
                location: loc,
                serial,
                time,
            },
        );
        pointer.frame(self);
        Ok(())
    }

    /// Press a pointer button and leave it down.
    ///
    /// Separate from the click so a **drag** is expressible: dragging is a press,
    /// then motion with the button still held, then a release, and a single
    /// press-and-release request can never describe one -- the button is up again
    /// before the motion arrives, so the client sees a click and then a plain
    /// pointer move.
    pub(crate) fn handle_button(
        &mut self,
        focus: &WlSurface,
        button: Button,
        down: bool,
    ) -> Result<(), CompError> {
        if self.pointer_loc.x == 0.0 && self.pointer_loc.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(focus, w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let time = crate::server_time();
        let serial = self.next_serial();
        if down {
            self.buttons = self.buttons.saturating_add(1);
            // Establish the keyboard focus on press, like anvil's
            // `on_pointer_button`: a real client routes clicks/hover through the
            // active window, and without a prior key event it never became active.
            if let Some(kbd) = self.seat.get_keyboard() {
                if !kbd.is_focused() {
                    kbd.set_focus(self, Some(focus.clone()), serial);
                }
            }
        } else {
            self.buttons = self.buttons.saturating_sub(1);
        }
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time,
                button: button.evdev_code(),
                state: if down {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        pointer.frame(self);
        Ok(())
    }

    pub(crate) fn handle_scroll(
        &mut self,
        focus: &WlSurface,
        delta: ScrollDelta,
    ) -> Result<(), CompError> {
        if self.pointer_loc.x == 0.0 && self.pointer_loc.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(focus, w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let (steps_v, steps_h) = (delta.vertical, delta.horizontal);
        let time = crate::server_time();
        let mut frame = smithay::input::pointer::AxisFrame::new(time)
            .source(smithay::backend::input::AxisSource::Wheel);
        if steps_v != 0 {
            frame = frame
                .value(smithay::backend::input::Axis::Vertical, steps_v as f64 * 10.0)
                .v120(smithay::backend::input::Axis::Vertical, steps_v);
        }
        if steps_h != 0 {
            frame = frame
                .value(
                    smithay::backend::input::Axis::Horizontal,
                    steps_h as f64 * 10.0,
                )
                .v120(smithay::backend::input::Axis::Horizontal, steps_h);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(())
    }

    /// Resolve a client's compositor state, whichever kind of client it is.
    /// XWayland's client carries `XWaylandClientData` (not our `ClientState`)
    /// with its own compositor state; our admitted clients carry `ClientState`.
    pub(crate) fn compositor_client_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<ClientState>() {
            return &data.compositor_state;
        }
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        // Fall back to a temporary default won't work (cannot return ref to
        // temp); treat as fatal. This is a programming error.
        panic!("client has neither ClientState nor XWaylandClientData");
    }
}

/// Collect the drained queues of a window and its whole subtree.
fn collect_window_inputs(win: &crate::window::Window, out: &mut Vec<(WlSurface, Vec<Input>)>) {
    let inputs = win.take_inputs();
    if !inputs.is_empty() {
        out.push((win.surface().clone(), inputs));
    }
    for child in win.children() {
        collect_window_inputs(child, out);
    }
}
