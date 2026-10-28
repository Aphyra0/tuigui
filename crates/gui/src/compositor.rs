//! The programs running in a compositor of ours, drawn into the pane as windows.
//!
//! The video path decodes a file; this one runs programs. Each program gets a
//! Wayland session of its own — a compositor in this process, one output — and
//! the pixels it commits are read straight out of that compositor's buffer.
//!
//! # What is whose
//!
//! The compositor holds the **session** ([`WindowingServer`]): the program and
//! its windows. It also holds the windows it has seen, in stacking order — the
//! further along the list, the closer to the user — and the one with focus.
//! Root is not in that list: it is the pane itself, always at the bottom.

use std::collections::LinkedList;
use std::sync::{Arc, Weak};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{StatefulWidget, Widget, WidgetRef};

use crate::DebugOptions;

mod inputs;
pub mod windowing;

pub use windowing::{StatefulWindow, WindowState, WindowingServer};

/// A program, or several, running in a compositor of ours.
///
/// `T` is the window session: a real one (wayland, later macOS) or the dummy
/// one (`--dbg-dummy-windowing`), which draws windows nobody is running.
pub struct Compositor<T: WindowingServer> {
    /// The session the windows live in. Dropping it ends them.
    windowing: T,
    /// The pane's own window: the program's, at the bottom of everything.
    root: Option<Weak<dyn StatefulWindow>>,
    /// The windows over it, in stacking order: the further along, the closer
    /// to the user.
    windows: LinkedList<Arc<dyn StatefulWindow>>,
    /// The window a key belongs to.
    focused: Option<Weak<dyn StatefulWindow>>,
    /// The switches this pane runs with (`--dbg-*`).
    debug: Arc<DebugOptions>,
    /// The pointer, as the compositor sees it between frames.
    pointer: inputs::Pointer,
    /// The pane's size as of the last render, in cells: where the window
    /// geometry was last worked out from.
    pane: (u16, u16),
}

impl<T: WindowingServer> Compositor<T> {
    /// Take a session and the switches, and start compositing.
    pub fn new(windowing: T, debug: Arc<DebugOptions>) -> Self {
        Self {
            windowing,
            root: None,
            windows: LinkedList::new(),
            focused: None,
            debug,
            pointer: inputs::Pointer::default(),
            pane: (0, 0),
        }
    }

    /// Whether the session's program has ended.
    ///
    /// A program that has not connected yet has not ended: the session is
    /// still waiting, which is what `is_running` answers.
    pub fn finished(&self) -> bool {
        !self.windowing.is_running()
    }

    /// Hand one event to the pane, which hands it to whatever it landed on.
    pub fn input(&mut self, event: &termina::Event) -> std::io::Result<()> {
        inputs::input(self, event)
    }

    /// One session turn, advanced: accept what arrived, dispatch what the
    /// program did. A dummy session moves forward by being drawn instead.
    pub fn advance(&mut self, timeout: std::time::Duration) {
        self.windowing.advance(timeout);
    }

    /// The pane, in cells, as of the last render.
    pub(crate) fn last_pane(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.pane.0,
            height: self.pane.1,
        }
    }

    /// The front-most window under a pointer position, and where it sits in
    /// the stack. The root is never a hit: it is the pane.
    pub(crate) fn front_window_at(
        &self,
        position: ratatui::layout::Position,
        pane: Rect,
    ) -> Option<(usize, Arc<dyn StatefulWindow>)> {
        self.windows
            .iter()
            .enumerate()
            .rev()
            .find(|(_, window)| area_of(window.as_ref(), pane).contains(position))
            .map(|(index, window)| (index, window.clone()))
    }

    /// Move a window to the front of the stack.
    pub(crate) fn raise(&mut self, index: usize) {
        let mut rest = self.windows.split_off(index);
        if let Some(window) = rest.pop_front() {
            // What followed it keeps its place under it, and the pulled
            // window goes on top of the lot.
            self.windows.append(&mut rest);
            self.windows.push_back(window);
        }
    }

    /// The window a key belongs to when nothing is focused: the root, which is
    /// the program itself.
    pub(crate) fn root_window(&self) -> Option<Arc<dyn StatefulWindow>> {
        self.root.as_ref().and_then(|root| root.upgrade())
    }

    /// The window a key belongs to, if it is still alive.
    pub(crate) fn focused_window(&self) -> Option<Arc<dyn StatefulWindow>> {
        self.focused.as_ref().and_then(|focused| focused.upgrade())
    }
}

/// Where a window sits on the pane, from its state and its base.
pub fn area_of(window: &dyn StatefulWindow, base: Rect) -> Rect {
    let state = window.state();
    Rect {
        x: base.x.saturating_add(state.position.0),
        y: base.y.saturating_add(state.position.1),
        width: state.size.0.max(1),
        height: state.size.1.max(1),
    }
}

/// One window: the shadow it casts, its own draw, the bar under it, and the
/// thin frame around it — then the windows it opened after it, inside it.
fn render_window(window: &Arc<dyn StatefulWindow>, base: Rect, root: bool, buf: &mut Buffer) {
    let area = area_of(window.as_ref(), base);
    // The shadow, drawn first: a light shade one column right and one row
    // down, so the window reads as raised. The window's own draw covers all
    // but the strip that peeks out to the right and below.
    if !root {
        // One row taller, for the bar the window leaves under it.
        let shadow = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.min(base.right().saturating_sub(area.x + 1)),
            height: (area.height + 1).min(base.bottom().saturating_sub(area.y + 1)),
        };
        for y in shadow.y..shadow.bottom() {
            for x in shadow.x..shadow.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char('░');
                    cell.set_fg(Color::Rgb(0x20, 0x20, 0x20));
                }
            }
        }
    }
    // The window's area is cleaned first: whatever is behind it — a parent's
    // picture, an earlier frame — must not leak into this one's cells.
    ratatui::widgets::Clear.render(area, buf);
    // The window's own content, on its whole area.
    WidgetRef::render_ref(window.as_ref(), area, buf);
    if !root {
        // The bar, under the window: one row, its full width.
        if area.bottom() < base.bottom() {
            let row = Rect {
                x: area.x,
                y: area.bottom(),
                width: area.width,
                height: 1,
            };
            ratatui::widgets::Clear.render(row, buf);
            buf.set_style(row, Style::new().bg(Color::DarkGray));
        }
        // The thin frame, just outside the window: a bar against each edge the
        // pane has room for.
        for y in area.y.saturating_sub(1)..area.bottom() {
            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_char('▏');
                cell.set_fg(Color::DarkGray);
            }
            if let Some(cell) = buf.cell_mut((area.right().saturating_sub(1), y)) {
                cell.set_char('▕');
                cell.set_fg(Color::DarkGray);
            }
        }
        if area.y > base.y {
            for x in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((x, area.y - 1)) {
                    cell.set_char('▁');
                    cell.set_fg(Color::DarkGray);
                }
            }
        }
    }
}

/// The compositor as the pane draws it: the root window, then every window
/// over it in stacking order.
impl<T: WindowingServer> StatefulWidget for &mut Compositor<T> {
    type State = ();

    fn render(self, area: Rect, buf: &mut Buffer, _state: &mut Self::State) {
        self.pane = (area.width, area.height);

        // The grab under way is applied here, once per frame: a press on a
        // window's bar moves it, a press on its edge resizes it. The window's
        // own state is where it ends up, so a release keeps it.
        if let Some(grab) = &self.pointer.grab {
            let dx = self.pointer.position.0 as i32 - grab.origin.0 as i32;
            let dy = self.pointer.position.1 as i32 - grab.origin.1 as i32;
            let mut state = grab.state;
            if grab.edge {
                state.size = (
                    (grab.state.size.0 as i32 + dx).max(1) as u16,
                    (grab.state.size.1 as i32 + dy).max(1) as u16,
                );
            } else {
                state.position = (
                    (grab.state.position.0 as i32 + dx).max(0) as u16,
                    (grab.state.position.1 as i32 + dy).max(0) as u16,
                );
            }
            grab.window.set_state(state);
        }
        // The session's root is the pane, so a change of pane is a change of
        // the root window's size: the server is asked to resize, and it is the
        // server that measures what lives in it.
        self.root = self.windowing.root_window();
        let root = self.root.as_ref().and_then(|root| root.upgrade());
        if let Some(root) = root.as_ref() {
            let wanted = (area.width, area.height);
            if root.state().size != wanted {
                self.windowing
                    .set_size((area.width as u32, area.height as u32));
            }
        }

        // Everything the session offers under the root, walked once.
        let mut offered: Vec<Arc<dyn StatefulWindow>> = Vec::new();
        if let Some(root) = root.as_ref() {
            collect(root, &mut offered);
        }

        // What died leaves the stack; what is new joins it above what was
        // there. A weak handle's only identity is the pointer, so that is how
        // two reads of the same window are told apart.
        let alive: Vec<Arc<dyn StatefulWindow>> = self
            .windows
            .iter()
            .filter(|known| offered.iter().any(|new| Arc::ptr_eq(known, new)))
            .cloned()
            .collect();
        self.windows = alive.into_iter().collect();
        for window in offered {
            if !self.windows.iter().any(|known| Arc::ptr_eq(known, &window)) {
                self.windows.push_back(window);
            }
        }

        if let Some(root) = root.as_ref() {
            render_window(root, area, true, buf);
        }
        for window in &self.windows {
            render_window(window, area, false, buf);
        }
    }
}

/// Walk a window's children into `out`, each before its own: a window is
/// drawn under the windows it opened.
fn collect(window: &Arc<dyn StatefulWindow>, out: &mut Vec<Arc<dyn StatefulWindow>>) {
    for child in window.get_children() {
        if let Some(child) = child.upgrade() {
            if !out.iter().any(|known| Arc::ptr_eq(known, &child)) {
                collect(&child, out);
                out.push(child);
            }
        }
    }
}
