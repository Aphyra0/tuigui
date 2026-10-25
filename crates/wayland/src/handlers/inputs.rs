//! Settling queued inputs onto the seat: one dispatch per input event.
//!
//! The walk belongs to the clients: `RootClient::dispatch_inputs` drains its
//! window tree and hands on to its helpers, which do the same. This module only
//! knows how to put one event on the seat.

use smithay::input::keyboard::FilterResult;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Point;

use crate::inputs::{Button, Input, KeyState, ScrollDelta, button_evdev};
use crate::state::State;
use crate::CompError;

impl<C, W> State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    /// Dispatch every queued input onto the seat.
    ///
    /// Called by `step`, so inputs land at a well-defined point in the protocol
    /// stream. The walk is the client tree's own: the root dispatches its window
    /// tree, then every helper it started.
    pub(crate) fn dispatch_inputs(&mut self) {
        let Some(root) = self.root_client.as_ref().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        root.dispatch_inputs(self);
    }

    /// Dispatch one queued event against `focus`.
    pub(crate) fn dispatch_input(
        &mut self,
        focus: &WlSurface,
        input: Input,
    ) -> Result<(), CompError> {
        match input {
            Input::Key { key, state } => self.handle_keycode(focus, crate::inputs::key_evdev(key), state),
            Input::MouseMove { x, y } => self.handle_mouse_move(focus, x, y),
            Input::Button { button, down } => self.handle_button(focus, button, down),
            Input::Scroll { delta } => self.handle_scroll(focus, delta),
        }
    }

    /// Deliver a key by its evdev code, with no character resolution here.
    ///
    /// The seat takes a keycode and derives its modifier state from the keys it
    /// sees, so anything that knows a key position can drive it directly. That
    /// is what makes a modifier and a shifted character expressible at all: a
    /// character cannot carry a modifier.
    fn handle_keycode(
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
        // Wayland keycodes are evdev codes plus 8 (X11's offset).
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

    fn handle_mouse_move(
        &mut self,
        focus: &WlSurface,
        x: f64,
        y: f64,
    ) -> Result<(), CompError> {
        let (w, h) = self.server_size;
        let x = x.clamp(0.0, w.max(2) as f64 - 1.0);
        let y = y.clamp(0.0, h.max(2) as f64 - 1.0);
        let scale = self.input_scale(focus)?;
        // smithay reports surface-local pointer coords as `location * scale`,
        // where the composed scale includes the output's. The location the
        // caller queues is in **output** pixels, so both scales are divided out
        // to reach the surface-local position the client expects.
        let loc = Point::new(x / scale, y / scale);
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
    /// Separate from the click so a **drag** is expressible: dragging is a
    /// press, then motion with the button still held, then a release, and a
    /// single press-and-release request can never describe one -- the button is
    /// up again before the motion arrives, so the client sees a click and then
    /// a plain pointer move.
    fn handle_button(
        &mut self,
        focus: &WlSurface,
        button: Button,
        down: bool,
    ) -> Result<(), CompError> {
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        // A button travels with a position, and the seat's is the pointer's own.
        let at = pointer.current_location();
        if at.x == 0.0 && at.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(focus, w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let time = crate::server_time();
        let serial = self.next_serial();
        if down {
            // Establish the keyboard focus on press, like anvil's
            // `on_pointer_button`: a real client routes clicks/hover through the
            // active window, and without a prior key event it never became active.
            if let Some(kbd) = self.seat.get_keyboard() {
                if !kbd.is_focused() {
                    kbd.set_focus(self, Some(focus.clone()), serial);
                }
            }
        }
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time,
                button: button_evdev(button),
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

    fn handle_scroll(&mut self, focus: &WlSurface, delta: ScrollDelta) -> Result<(), CompError> {
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let at = pointer.current_location();
        if at.x == 0.0 && at.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(focus, w as f64 / 2.0, h as f64 / 2.0)?;
        }
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

    /// The scale that turns output pixels into the surface-local position the
    /// client expects: its own content scale times the output's advertised
    /// scale.
    fn input_scale(&self, focus: &WlSurface) -> Result<f64, CompError> {
        let client = focus
            .client()
            .ok_or_else(|| CompError::Server("an input target has no client".into()))?;
        let compositor = crate::client::compositor_client_state::<C, W>(&client)
            .ok_or_else(|| CompError::Server("an input target has no compositor state".into()))?;
        let scale = compositor.client_scale() * self.output_scale;
        if scale.is_finite() && scale > 0.0 {
            Ok(scale)
        } else {
            Err(CompError::Server(format!("input scale {scale} is unusable")))
        }
    }
}
