//! Settling queued inputs onto the seat: one dispatch per input event.
//!
//! The walk belongs to the clients: `RootClient::dispatch_inputs` drains its
//! window tree and hands on to its helpers, which do the same.
//!
//! # Two vocabularies, kept apart
//!
//! A key and a pointer are not two cases of one thing. UI Events keeps them in
//! separate modules for the same reason a seat does: a key event is a position and
//! a state, while a pointer event carries a whole state — position, held buttons,
//! pressure, tilt — and flattening them would mean a variant that has to carry all
//! the fields it does not use. So the queue holds [`KeyboardEvent`] and
//! [`PointerEvent`] apart, the dispatch is entered twice, and the translation to
//! what the seat takes happens here, at the only point where Linux is the thing
//! being addressed.

use smithay::input::keyboard::FilterResult;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Point;
use ui_events::keyboard::{Code, KeyState, KeyboardEvent, Modifiers};
use ui_events::pointer::{PointerButton, PointerEvent};
use ui_events::ScrollDelta;

use crate::state::State;
use crate::CompError;

impl State {
    /// Dispatch every queued input onto the seat.
    ///
    /// Called by `step`, so inputs land at a well-defined point in the protocol
    /// stream. The walk is the client tree's own: the root dispatches its window
    /// tree, then every helper it started.
    pub(crate) fn dispatch_inputs(&mut self) {
        // Cloned out of the field so the borrow of `self` ends before the walk needs it
        // mutably: the record is behind an `Arc`, so this is a share of one count.
        let Some(root) = self.root_client.clone() else {
            return;
        };
        root.dispatch_inputs(self);
    }

    /// Deliver one key event to `focus`.
    ///
    /// A modified key is a *sequence*, not one event: the seat derives its
    /// modifier state from the keys it has seen pressed, so `Ctrl+C` is press
    /// control, press `c`, release `c`, release control. The modifiers hold for
    /// the group, which is why they are pressed once around it rather than per
    /// event.
    pub(crate) fn dispatch_key_input(
        &mut self,
        focus: &WlSurface,
        event: &KeyboardEvent,
    ) -> Result<(), CompError> {
        // A repeat is a press the terminal produced on its own; the seat has no
        // notion of one, so it is delivered as what it is: another press.
        let Some(code) = key_evdev(event.code) else {
            // A key with no position on this keyboard: nothing to press.
            return Ok(());
        };
        // The modifiers wrap the key, so they are pressed before it and released
        // after it, in the reverse order they went down.
        let held = modifier_evdev(event.modifiers);
        match event.state {
            KeyState::Down => {
                for modifier in &held {
                    self.press_keycode(focus, *modifier, KeyState::Down)?;
                }
                self.press_keycode(focus, code, KeyState::Down)?;
            }
            KeyState::Up => {
                self.press_keycode(focus, code, KeyState::Up)?;
                for modifier in held.iter().rev() {
                    self.press_keycode(focus, *modifier, KeyState::Up)?;
                }
            }
        }
        Ok(())
    }

    /// Let go of every key the seat holds that this step's queue did not carry.
    ///
    /// A terminal reports a press and cannot report the release unless the keyboard
    /// protocol asked for event types, which nothing asks for. So a key that is down
    /// and was not in this step's queue is a key whose release was never said, and
    /// leaving it pressed leaves the program repeating it for the rest of the session.
    ///
    /// Only the focused surface releases, because the pressed keys are the seat's and the
    /// seat has one recipient: every other window in the walk has an empty queue, and
    /// would let go of the key the focused one just pressed.
    pub(crate) fn release_unrenewed(&mut self, focus: &WlSurface, queue: &[KeyboardEvent]) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if keyboard.current_focus().as_ref() != Some(focus) {
            return;
        }
        for keycode in keyboard.pressed_keys() {
            // Wayland keycodes carry X11's offset of 8, which `press_keycode` adds.
            let evdev = u32::from(keycode.raw()).saturating_sub(8);
            let renewed = queue.iter().any(|event| {
                event.state == KeyState::Down && key_evdev(event.code) == Some(evdev)
            });
            if renewed {
                continue;
            }
            if let Err(e) = self.press_keycode(focus, evdev, KeyState::Up) {
                tracing::debug!("a key was not released: {e}");
            }
        }
    }

    /// Deliver one pointer event to `focus`.
    pub(crate) fn dispatch_mouse_input(
        &mut self,
        focus: &WlSurface,
        event: &PointerEvent,
    ) -> Result<(), CompError> {
        match event {
            PointerEvent::Move(update) => self.move_pointer(focus, update.current.position.x, update.current.position.y),
            // A button is pressed where the pointer is, so the position travels with it:
            // a click that did not move the pointer first would otherwise land wherever
            // the last move left it, which for a window nobody has hovered is the corner.
            PointerEvent::Down(button) => {
                self.move_pointer(focus, button.state.position.x, button.state.position.y)?;
                self.press_button(focus, button.button, true)
            }
            PointerEvent::Up(button) => {
                self.move_pointer(focus, button.state.position.x, button.state.position.y)?;
                self.press_button(focus, button.button, false)
            }
            PointerEvent::Scroll(scroll) => {
                self.move_pointer(focus, scroll.state.position.x, scroll.state.position.y)?;
                self.scroll_pointer(focus, scroll.delta)
            }
            // Enter, leave, cancel and gestures say nothing a seat can be told:
            // a seat's focus is the surface, and it has no grab to cancel.
            PointerEvent::Enter(_)
            | PointerEvent::Leave(_)
            | PointerEvent::Cancel(_)
            | PointerEvent::Gesture(_) => Ok(()),
        }
    }

    /// Put one keycode on the seat.
    fn press_keycode(
        &mut self,
        focus: &WlSurface,
        evdev: u32,
        state: KeyState,
    ) -> Result<(), CompError> {
        let keyboard = self.seat.get_keyboard().ok_or(CompError::NotReady)?;
        // The keyboard follows the window the key was delivered to. Comparing against
        // that surface rather than asking whether anything is focused is what lets
        // focus move: `is_focused` answers true from the first window ever typed into,
        // so the keyboard stayed with it and every later window got nothing.
        if keyboard.current_focus().as_ref() != Some(focus) {
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
                KeyState::Down => smithay::backend::input::KeyState::Pressed,
                KeyState::Up => smithay::backend::input::KeyState::Released,
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward::<()>,
        );
        Ok(())
    }

    fn move_pointer(&mut self, focus: &WlSurface, x: f64, y: f64) -> Result<(), CompError> {
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
    fn press_button(
        &mut self,
        focus: &WlSurface,
        button: Option<PointerButton>,
        down: bool,
    ) -> Result<(), CompError> {
        // A button the seat has no code for is not a button it can be told about.
        let Some(button) = button.and_then(button_evdev) else {
            return Ok(());
        };
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let time = crate::server_time();
        let serial = self.next_serial();
        if down {
            // A click makes the window the one the keyboard talks to, which is what
            // the pane means by focusing it. Compared against the surface, so a click
            // on a different window moves the keyboard to it.
            if let Some(kbd) = self.seat.get_keyboard() {
                if kbd.current_focus().as_ref() != Some(focus) {
                    kbd.set_focus(self, Some(focus.clone()), serial);
                }
            }
        }
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time,
                button,
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

    fn scroll_pointer(&mut self, _focus: &WlSurface, delta: ScrollDelta) -> Result<(), CompError> {
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        // A wheel reports lines, a touchpad reports pixels, and the seat takes
        // both: the discrete count for clients that want steps, the pixel amount
        // for the ones that scroll smoothly.
        let (steps_v, steps_h) = match delta {
            ScrollDelta::LineDelta(x, y) => (y, x),
            ScrollDelta::PageDelta(x, y) => (y, x),
            ScrollDelta::PixelDelta(p) => (p.y as f32 / 10.0, p.x as f32 / 10.0),
        };
        let time = crate::server_time();
        let mut frame = smithay::input::pointer::AxisFrame::new(time)
            .source(smithay::backend::input::AxisSource::Wheel);
        if steps_v != 0.0 {
            frame = frame
                .value(
                    smithay::backend::input::Axis::Vertical,
                    f64::from(steps_v) * 10.0,
                )
                .v120(
                    smithay::backend::input::Axis::Vertical,
                    steps_v.round() as i32,
                );
        }
        if steps_h != 0.0 {
            frame = frame
                .value(
                    smithay::backend::input::Axis::Horizontal,
                    f64::from(steps_h) * 10.0,
                )
                .v120(
                    smithay::backend::input::Axis::Horizontal,
                    steps_h.round() as i32,
                );
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
        let compositor = crate::client::compositor_client_state(&client)
            .ok_or_else(|| CompError::Server("an input target has no compositor state".into()))?;
        let scale = compositor.client_scale() * self.output_scale;
        if scale.is_finite() && scale > 0.0 {
            Ok(scale)
        } else {
            Err(CompError::Server(format!(
                "input scale {scale} is unusable"
            )))
        }
    }
}

/// The evdev keycode for a physical key.
///
/// `None` is a real answer: the table has 151 entries whose evdev column is zero,
/// meaning this platform has no code for that key. Sending a stand-in would type
/// the wrong thing.
fn key_evdev(code: Code) -> Option<u32> {
    use std::str::FromStr as _;
    let mapping = keycode::KeyMappingCode::from_str(&code.to_string()).ok()?;
    let map = keycode::KeyMap::from(mapping);
    (map.evdev != 0).then_some(u32::from(map.evdev))
}

/// The evdev button code for a pointer button.
fn button_evdev(button: PointerButton) -> Option<u32> {
    Some(match button {
        PointerButton::Primary => 0x110,   // BTN_LEFT
        PointerButton::Auxiliary => 0x112, // BTN_MIDDLE
        PointerButton::Secondary => 0x111, // BTN_RIGHT
        PointerButton::X1 => 0x113,        // BTN_SIDE
        PointerButton::X2 => 0x114,        // BTN_EXTRA
        _ => return None,
    })
}

/// The keycodes that stand for a modifier set, in press order.
///
/// A modifier is a key position like any other, so it comes from the same table
/// rather than from a constant: `ShiftLeft` is the same lookup as `KeyA`, and a
/// platform whose number differs gets the right one for free.
fn modifier_evdev(modifiers: Modifiers) -> Vec<u32> {
    [
        (Modifiers::SHIFT, Code::ShiftLeft),
        (Modifiers::CONTROL, Code::ControlLeft),
        (Modifiers::ALT, Code::AltLeft),
        // UI Events calls the key this terminal calls Super, and macOS calls
        // Command, `Meta` — the same position XKB names LEFTMETA.
        (Modifiers::META, Code::MetaLeft),
    ]
    .into_iter()
    .filter(|(flag, _)| modifiers.contains(*flag))
    .filter_map(|(_, code)| key_evdev(code))
    .collect()
}
