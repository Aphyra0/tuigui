//! The wayland session, wearing the windowing vocabulary.
//!
//! The session runs in `wayland`: surfaces, commits, the seat. This is the
//! layer over it: the session's windows as this crate's
//! [`StatefulWindow`] values, each with its state on the pane and the wrappers
//! of everything under it.
//!
//! A wrapper is **not** minted per read. The session hands its windows out as
//! `Arc`s, and the wrapper of each is kept here, keyed by that address: two
//! reads of one window are the same wrapper, which is what lets the compositor
//! keep a stacking order and a focus across frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, Weak};

use wayland::WaylandServer as Native;

use super::{StatefulWindow, WindowState, WindowingServer};

/// The wayland session as a windowing server.
pub struct Server {
    /// The session.
    inner: Native,
    /// One wrapper per session window, keyed by the address of the window the
    /// session holds. Shared with the wrappers so a child can mint its own
    /// children's.
    wrappers: Registry,
}

/// The wrappers the session's windows are reached through, keyed by the
/// address of the window each wraps.
#[derive(Clone, Default)]
struct Registry(Arc<Mutex<HashMap<usize, Weak<SessionWindow>>>>);

/// One window of the session: the session's window, its state on the pane, and
/// the wrappers of everything under it — the windows it opened after it, and
/// the root windows of the helper clients its process started.
pub struct SessionWindow {
    /// The session's window.
    inner: Weak<wayland::Window>,
    /// The window's state on the pane, in cells.
    state: RwLock<WindowState>,
    /// The wrappers of everything under this one.
    children: RwLock<Vec<Arc<SessionWindow>>>,
    /// The one registry of wrappers this window mints from.
    wrappers: Registry,
}

/// The address a session window is keyed by: the wrapper holds the window
/// weakly, so the address is the identity the wrapper is told apart by.
fn key_of(tree: &Arc<wayland::Window>) -> usize {
    Arc::as_ptr(tree) as usize
}

impl Registry {
    /// The wrapper for a session window, minted at the first read and reused
    /// after: a window that already has one keeps it, and with it the state
    /// the compositor put it in.
    fn wrap(&self, tree: &Arc<wayland::Window>, state: WindowState) -> Arc<SessionWindow> {
        let key = key_of(tree);
        let mut wrappers = self.0.lock().unwrap();
        if let Some(existing) = wrappers.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let wrapped = Arc::new(SessionWindow {
            inner: Arc::downgrade(tree),
            state: RwLock::new(state),
            children: RwLock::new(Vec::new()),
            wrappers: self.clone(),
        });
        wrappers.insert(key, Arc::downgrade(&wrapped));
        // Everything whose window died leaves the registry with it.
        wrappers.retain(|_, wrapper| wrapper.strong_count() > 0);
        wrapped
    }
}

impl Server {
    /// Start the session.
    pub fn new(options: wayland::Options) -> std::io::Result<Self> {
        let inner = Native::new(options).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            inner,
            wrappers: Registry::default(),
        })
    }
}

impl WindowingServer for Server {
    /// The root window: the session's, placed across the whole pane.
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>> {
        let tree = self.inner.root_tree()?;
        let state = WindowState {
            size: (self.inner.size().0 as u16, self.inner.size().1 as u16),
            position: (0, 0),
        };
        let wrapped = self.wrappers.wrap(&tree, state);
        Some(Arc::downgrade(&wrapped) as Weak<dyn StatefulWindow>)
    }

    fn spawn(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        self.inner.spawn(path)
    }

    fn advance(&mut self, timeout: std::time::Duration) {
        self.inner.step(timeout);
    }

    fn is_ready(&self) -> bool {
        self.inner.root_tree().is_some()
    }

    /// The root client's process is what ends the session.
    fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// The session is resized: the output's mode changes, and the session's
    /// root window is measured against the new size.
    fn set_size(&mut self, size: (u32, u32)) {
        self.inner.set_size(size);
    }
}

impl StatefulWindow for SessionWindow {
    fn input(&self, input: super::Input) -> Result<(), super::Error> {
        let Some(inner) = self.inner.upgrade() else {
            return Err(super::Error::Gone);
        };
        inner.queue_input(to_seat(input));
        Ok(())
    }

    fn capture(&self) -> Result<Vec<u8>, super::Error> {
        let Some(inner) = self.inner.upgrade() else {
            return Err(super::Error::Gone);
        };
        inner.capture().ok_or(super::Error::NotReady)
    }

    /// Ask the session's window to draw at a size. The next commit answers.
    fn set_size(&self, size: (u32, u32)) {
        if let Some(inner) = self.inner.upgrade() {
            inner.set_size(size);
        }
    }

    fn state(&self) -> WindowState {
        *self.state.read().unwrap()
    }

    fn set_state(&self, state: WindowState) {
        *self.state.write().unwrap() = state;
    }

    /// Everything under this window, as weak handles: the windows its program
    /// opened after it, and the root windows of the helpers it started. Each
    /// is the wrapper it already had, so the compositor sees the same window
    /// as last frame.
    fn get_children(&self) -> Vec<Weak<dyn StatefulWindow>> {
        let Some(inner) = self.inner.upgrade() else {
            return Vec::new();
        };
        let descendants = inner.descendants();
        let mut children = self.children.write().unwrap();
        *children = descendants
            .iter()
            .map(|tree| self.wrappers.wrap(tree, WindowState::default()))
            .collect();
        children
            .iter()
            .map(|child| Arc::downgrade(child) as Weak<dyn StatefulWindow>)
            .collect()
    }
}

/// A window, the way the pane draws it: every cell filled with the window's
/// own surface id, which is the anchor the other side scans for.
impl ratatui::widgets::WidgetRef for SessionWindow {
    fn render_ref(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let id = *inner.id();
        let style = ratatui::style::Style::new()
            .fg(ratatui::style::Color::Rgb(id.fg.0, id.fg.1, id.fg.2))
            .bg(ratatui::style::Color::Rgb(id.bg.0, id.bg.1, id.bg.2));
        buf.set_style(area, style);
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((column, row)) {
                    cell.set_char(id.symbol);
                }
            }
        }
    }
}

/// One input, from this crate's vocabulary to the seat's.
fn to_seat(input: super::Input) -> wayland::Input {
    use super::{Button as From, Key as FromKey, KeyState as FromState};
    use wayland::{Button as To, Key as ToKey, KeyState as ToState};

    let button = |button: From| match button {
        From::Left => To::Left,
        From::Middle => To::Middle,
        From::Right => To::Right,
        From::Back => To::Back,
        From::Forward => To::Forward,
    };

    match input {
        super::Input::Key { key, state } => wayland::Input::Key {
            key: match key {
                FromKey::Char(c) => ToKey::Char(c),
                FromKey::Return => ToKey::Return,
                FromKey::Tab => ToKey::Tab,
                FromKey::Backspace => ToKey::Backspace,
                FromKey::Delete => ToKey::Delete,
                FromKey::Escape => ToKey::Escape,
                FromKey::Space => ToKey::Space,
                FromKey::Left => ToKey::Left,
                FromKey::Right => ToKey::Right,
                FromKey::Up => ToKey::Up,
                FromKey::Down => ToKey::Down,
                FromKey::Home => ToKey::Home,
                FromKey::End => ToKey::End,
                FromKey::PageUp => ToKey::PageUp,
                FromKey::PageDown => ToKey::PageDown,
            },
            state: match state {
                FromState::Pressed => ToState::Pressed,
                FromState::Released => ToState::Released,
            },
        },
        super::Input::MouseMove { x, y, held } => wayland::Input::MouseMove {
            x,
            y,
            held: held.map(button),
        },
        super::Input::Button {
            button: from,
            down,
        } => wayland::Input::Button {
            button: button(from),
            down,
        },
        super::Input::Scroll { delta } => wayland::Input::Scroll {
            delta: wayland::ScrollDelta {
                vertical: delta.vertical,
                horizontal: delta.horizontal,
            },
        },
    }
}
