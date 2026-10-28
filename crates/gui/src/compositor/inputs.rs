//! Input: what an event delivered to the pane becomes for a window.
//!
//! The compositor owns the mouse: it walks the stack from the front, finds the
//! window a cell belongs to, and either moves or resizes it (the bar, a
//! border) or forwards the event to it, window-local. A key belongs to the
//! focused window and nothing else.

use ratatui::layout::Rect;

use super::windowing::{Button, Input, Key, KeyState, ScrollDelta, StatefulWindow, WindowingServer};
use super::{area_of, Compositor};

/// The pointer, as the compositor sees it between frames.
#[derive(Default)]
pub struct Pointer {
    /// Where the pointer is, in pane cells.
    pub(crate) position: (u16, u16),
    /// The button held, if one is.
    pub(crate) held: Option<Button>,
    /// The grab: what the press started, and where the pointer was then.
    pub(crate) grab: Option<Grab>,
}

/// What a press on a window started.
pub(crate) struct Grab {
    /// The window it was pressed on.
    pub(crate) window: std::sync::Arc<dyn StatefulWindow>,
    /// The window's state when the press happened.
    pub(crate) state: super::WindowState,
    /// Where the pointer was when the press happened.
    pub(crate) origin: (u16, u16),
    /// Whether the press was on the window's edge rather than its bar.
    pub(crate) edge: bool,
}

/// Hand one terminal event to the pane.
pub fn input<T: WindowingServer>(
    compositor: &mut Compositor<T>,
    event: &termina::Event,
) -> std::io::Result<()> {
    match event {
        termina::Event::Mouse(mouse) => mouse_input(compositor, mouse),
        termina::Event::Key(key) => key_input(compositor, key),
        _ => Ok(()),
    }
}

/// The mouse: what the pointer is doing, and the window it is doing it to.
fn mouse_input<T: WindowingServer>(
    compositor: &mut Compositor<T>,
    mouse: &termina::event::MouseEvent,
) -> std::io::Result<()> {
    use termina::event::MouseEventKind;

    compositor.pointer.position = (mouse.column, mouse.row);
    let pane = compositor.last_pane();

    match mouse.kind {
        // A press grabs: the front-most window under the pointer, if any.
        MouseEventKind::Down(button) => {
            compositor.pointer.held = Some(from_button(button));
            let position = ratatui::layout::Position::new(mouse.column, mouse.row);
            if let Some((index, window)) = compositor.front_window_at(position, pane) {
                let area = area_of(window.as_ref(), pane);
                let inner = inner_of(area);
                let edge = area.contains(position) && !inner.contains(position);
                compositor.pointer.grab = Some(super::inputs::Grab {
                    window: window.clone(),
                    state: window.state(),
                    origin: (mouse.column, mouse.row),
                    edge,
                });
                compositor.raise(index);
                compositor.focused = Some(std::sync::Arc::downgrade(&window));
                return Ok(());
            }
        }
        // A drag is the grab: the window moves by its bar, or resizes by its
        // edge. Nothing is forwarded while it happens.
        MouseEventKind::Drag(_) | MouseEventKind::Moved => {
            if compositor.pointer.grab.is_some() {
                return Ok(());
            }
            if let Some(window) = compositor.focused_window() {
                let area = area_of(window.as_ref(), pane);
                let local = local_of(mouse, area);
                window
                    .input(Input::MouseMove {
                        x: local.0,
                        y: local.1,
                        held: compositor.pointer.held,
                    })
                    .map_err(std::io::Error::other)?;
            }
            return Ok(());
        }
        MouseEventKind::Up(_) => {
            compositor.pointer.held = None;
            if compositor.pointer.grab.take().is_some() {
                return Ok(());
            }
            if let Some(window) = compositor.focused_window() {
                window
                    .input(Input::Button {
                        button: from_button(match mouse.kind {
                            MouseEventKind::Up(button) => button,
                            _ => unreachable!("matched above"),
                        }),
                        down: false,
                    })
                    .map_err(std::io::Error::other)?;
            }
            return Ok(());
        }
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {
            let position = ratatui::layout::Position::new(mouse.column, mouse.row);
            if let Some((_, window)) = compositor.front_window_at(position, pane) {
                let area = area_of(window.as_ref(), pane);
                let delta = match mouse.kind {
                    MouseEventKind::ScrollUp => ScrollDelta { vertical: -1, horizontal: 0 },
                    MouseEventKind::ScrollDown => ScrollDelta { vertical: 1, horizontal: 0 },
                    MouseEventKind::ScrollLeft => ScrollDelta { vertical: 0, horizontal: -1 },
                    _ => ScrollDelta { vertical: 0, horizontal: 1 },
                };
                window
                    .input(Input::Scroll { delta })
                    .map_err(std::io::Error::other)?;
                let _ = area;
            }
            return Ok(());
        }
    }

    // A press that landed on no window is the pane's own: nothing to do.
    Ok(())
}

/// A key belongs to the focused window, or to the root until something is
/// focused: the pane is the program, and a key reaches the program first.
fn key_input<T: WindowingServer>(
    compositor: &mut Compositor<T>,
    key: &termina::event::KeyEvent,
) -> std::io::Result<()> {
    let Some(code) = from_key(key.code) else {
        return Ok(());
    };
    let Some(window) = compositor
        .focused_window()
        .or_else(|| compositor.root_window())
    else {
        return Ok(());
    };
    window
        .input(Input::Key {
            key: code,
            state: match key.kind {
                termina::event::KeyEventKind::Release => KeyState::Released,
                _ => KeyState::Pressed,
            },
        })
        .map_err(std::io::Error::other)
}

/// A window's content area: what the frame leaves.
pub fn inner_of(area: Rect) -> Rect {
    ratatui::widgets::Block::bordered().inner(area)
}

/// Where a pointer position is inside a window, in its own cells.
fn local_of(mouse: &termina::event::MouseEvent, area: Rect) -> (f64, f64) {
    (
        mouse.column as f64 - area.x as f64,
        mouse.row as f64 - area.y as f64,
    )
}

/// A termina button, as the session names it.
fn from_button(button: termina::event::MouseButton) -> Button {
    use termina::event::MouseButton;
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Middle => Button::Middle,
        MouseButton::Right => Button::Right,
    }
}

/// A termina key, as the session names it. Unnamed keys are skipped.
fn from_key(code: termina::event::KeyCode) -> Option<Key> {
    use termina::event::KeyCode;
    Some(match code {
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Return,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Escape => Key::Escape,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    })
}
