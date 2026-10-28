//! Input: what an event delivered to the pane becomes for a window.
//!
//! The mouse is the compositor's first: it tracks the pointer, and a press on
//! a window's bar — the gray line under it — becomes a drag of that window,
//! an offset the drawing applies. Everything else is forwarded to the root
//! window, the way a seat forwards to the focused surface.

use proto::windowing::{self, WindowingServer};
use ratatui::layout::{Position, Rect};

use super::{ClientData, Compositor, WindowState};

/// The pointer, as the compositor sees it between frames.
///
/// Pure compositor work: where the pointer is, what it holds, and what it
/// drags. Dragging a window's bar moves the window on the pane as an offset
/// of the drawing; the window itself is never touched.
#[derive(Default)]
pub struct Pointer {
    /// Where the pointer is, in pane cells.
    pub(crate) position: (u16, u16),
    /// The button held, if one is.
    pub(crate) held: Option<termina::event::MouseButton>,
    /// The drag under way: the grabbed window's place, where the pointer was
    /// when the bar was pressed, and what the window had already been pulled
    /// before this grab.
    pub(crate) drag: Option<(WindowState, (u16, u16), (i16, i16))>,
    /// The pulls that ended: each window's place, and how far it now sits
    /// from it.
    pub(crate) pulled: Vec<(WindowState, (i16, i16))>,
}

impl Pointer {
    /// How far the drag has pulled its window: what the window had been
    /// pulled before this grab, plus where the pointer has moved since.
    pub(crate) fn offset(&self) -> (i16, i16) {
        match self.drag {
            Some(drag) => self.offset_of(drag),
            None => (0, 0),
        }
    }

    /// The same, for one drag tuple: its parked pull, plus where the pointer
    /// has moved since its grab.
    pub(crate) fn offset_of(&self, drag: (WindowState, (u16, u16), (i16, i16))) -> (i16, i16) {
        let (_, origin, base) = drag;
        (
            (base.0 as i32 + self.position.0 as i32 - origin.0 as i32) as i16,
            (base.1 as i32 + self.position.1 as i32 - origin.1 as i32) as i16,
        )
    }

    /// The pull parked on a place, removed: the start of a new drag of it.
    pub(crate) fn take_pulled(&mut self, place: &WindowState) -> (i16, i16) {
        match self.pulled.iter().position(|(pulled, _)| pulled == place) {
            Some(index) => self.pulled.remove(index).1,
            None => (0, 0),
        }
    }
}

/// Hand one terminal event to the pane.
pub fn input<T: WindowingServer<ClientData, WindowState>>(
    compositor: &mut Compositor<T>,
    event: &termina::Event,
) -> std::io::Result<()> {
    if let termina::Event::Mouse(mouse) = event {
        // The pointer first: where it is, what it holds.
        let pointer = &mut compositor.pointer;
        pointer.position = (mouse.column, mouse.row);
        pointer.held = match mouse.kind {
            termina::event::MouseEventKind::Down(button)
            | termina::event::MouseEventKind::Drag(button) => Some(button),
            _ => None,
        };

        // A press on a window's bar starts a drag of that window: compositor
        // work, so the press is not forwarded.
        if matches!(mouse.kind, termina::event::MouseEventKind::Down(_)) {
            let mut found = None;
            if let Some(client) = compositor.windowing.root_client() {
                let mut outcome = Ok(());
                visit_bar(
                    client,
                    Rect::default(),
                    true,
                    Position::new(mouse.column, mouse.row),
                    &pointer.pulled,
                    &mut found,
                    &mut outcome,
                );
                outcome?;
            }
            if let Some(place) = found {
                let base = pointer.take_pulled(&place);
                pointer.drag = Some((place, (mouse.column, mouse.row), base));
                return Ok(());
            }
        }

        // Motion while dragging is the drag: the drawing applies the offset,
        // and nothing is forwarded.
        if matches!(
            mouse.kind,
            termina::event::MouseEventKind::Drag(_) | termina::event::MouseEventKind::Moved
        ) && pointer.drag.is_some()
        {
            return Ok(());
        }

        // A release ends the drag, unforwarded — and the pull stays: the
        // window keeps where it was dragged to.
        if matches!(mouse.kind, termina::event::MouseEventKind::Up(_)) {
            if let Some((place, origin, base)) = pointer.drag.take() {
                let pull = pointer.offset_of((place, origin, base));
                pointer.pulled.push((place, pull));
                return Ok(());
            }
        }
    }

    let Some(client) = compositor.windowing.root_client() else {
        return Ok(());
    };
    let Some(input) = proto_input(event) else {
        return Ok(());
    };
    let mut delivered = Ok(());
    client.with_window(&mut |window| {
        if delivered.is_ok() {
            delivered = window.input(input).map_err(std::io::Error::other);
        }
    });
    delivered.map(|_| ())
}

/// The window whose bar the pointer is on, the front one when they overlap,
/// then its helper clients' — each at the same place, the way the drawing
/// walks them.
fn visit_bar(
    client: &dyn windowing::Client<ClientData, WindowState>,
    base: Rect,
    root: bool,
    pointer: Position,
    pulled: &[(WindowState, (i16, i16))],
    found: &mut Option<WindowState>,
    outcome: &mut std::io::Result<()>,
) {
    client.with_window(&mut |window| {
        if outcome.is_ok() && found.is_none() {
            *outcome = walk_bar(window, base, root, pointer, pulled, found).map(|_| ());
        }
    });
    client.with_children(&mut |child| {
        if outcome.is_ok() && found.is_none() {
            visit_bar(child, base, false, pointer, pulled, found, outcome);
        }
    });
}

/// One window: its bar is the row under it, when there is room for one.
/// Children first, so a child's bar wins over the parent's.
fn walk_bar(
    window: &dyn windowing::Window<WindowState>,
    base: Rect,
    root: bool,
    pointer: Position,
    pulled: &[(WindowState, (i16, i16))],
    found: &mut Option<WindowState>,
) -> std::io::Result<bool> {
    let mut place = WindowState::default();
    window.with_attached(&mut |state| place = *state);
    // The window sits where its parked pull has left it, if one has.
    let pull = match pulled.iter().find(|(grabbed, _)| *grabbed == place) {
        Some((_, pull)) => *pull,
        None => (0, 0),
    };
    let area = Rect {
        x: (base.x as i32 + place.position.0 as i32 + pull.0 as i32).max(0) as u16,
        y: (base.y as i32 + place.position.1 as i32 + pull.1 as i32).max(0) as u16,
        width: place.size.0.max(1),
        height: place.size.1.max(1),
    };
    let mut child_hit = false;
    let mut failure = Ok(());
    window.with_children(&mut |child| {
        match walk_bar(child, area, false, pointer, pulled, found) {
            Ok(hit) => child_hit |= hit,
            Err(e) => failure = Err(e),
        }
    });
    failure?;
    if child_hit {
        return Ok(true);
    }
    let hit = !root
        && area.bottom() < base.bottom()
        && pointer.y == area.bottom()
        && pointer.x >= area.x
        && pointer.x < area.right();
    if hit {
        *found = Some(place);
    }
    Ok(hit)
}

/// One terminal event, as the seat names it. What is not a key, a button, a
/// move or a wheel step — protocol responses, focus, paste — is not input a
/// window takes, and is dropped.
fn proto_input(event: &termina::Event) -> Option<windowing::Input> {
    match event {
        termina::Event::Key(key) => Some(windowing::Input::Key {
            key: proto_key(key.code)?,
            state: match key.kind {
                termina::event::KeyEventKind::Release => windowing::KeyState::Released,
                _ => windowing::KeyState::Pressed,
            },
        }),
        termina::Event::Mouse(mouse) => Some(
            match mouse.kind {
                termina::event::MouseEventKind::Down(button) => windowing::Input::Button {
                    button: proto_button(button),
                    down: true,
                },
                termina::event::MouseEventKind::Up(button) => windowing::Input::Button {
                    button: proto_button(button),
                    down: false,
                },
                termina::event::MouseEventKind::Moved | termina::event::MouseEventKind::Drag(_) => {
                    windowing::Input::MouseMove {
                        x: mouse.column as f64,
                        y: mouse.row as f64,
                        held: None,
                    }
                }
                termina::event::MouseEventKind::ScrollDown => wheel(0, 1),
                termina::event::MouseEventKind::ScrollUp => wheel(0, -1),
                termina::event::MouseEventKind::ScrollLeft => wheel(-1, 0),
                termina::event::MouseEventKind::ScrollRight => wheel(1, 0),
            },
        ),
        _ => None,
    }
}

/// A termina button, as the seat names it.
fn proto_button(button: termina::event::MouseButton) -> windowing::Button {
    use termina::event::MouseButton;
    match button {
        MouseButton::Left => windowing::Button::Left,
        MouseButton::Middle => windowing::Button::Middle,
        MouseButton::Right => windowing::Button::Right,
    }
}

/// One wheel step.
fn wheel(horizontal: i32, vertical: i32) -> windowing::Input {
    windowing::Input::Scroll {
        delta: windowing::ScrollDelta {
            vertical,
            horizontal,
        },
    }
}

/// A termina key, as the session's seat names it. Unnamed keys are skipped.
fn proto_key(code: termina::event::KeyCode) -> Option<windowing::Key> {
    use termina::event::KeyCode;
    Some(match code {
        KeyCode::Char(' ') => windowing::Key::Space,
        KeyCode::Char(c) => windowing::Key::Char(c),
        KeyCode::Enter => windowing::Key::Return,
        KeyCode::Tab => windowing::Key::Tab,
        KeyCode::Backspace => windowing::Key::Backspace,
        KeyCode::Delete => windowing::Key::Delete,
        KeyCode::Escape => windowing::Key::Escape,
        KeyCode::Left => windowing::Key::Left,
        KeyCode::Right => windowing::Key::Right,
        KeyCode::Up => windowing::Key::Up,
        KeyCode::Down => windowing::Key::Down,
        KeyCode::Home => windowing::Key::Home,
        KeyCode::End => windowing::Key::End,
        KeyCode::PageUp => windowing::Key::PageUp,
        KeyCode::PageDown => windowing::Key::PageDown,
        _ => return None,
    })
}
