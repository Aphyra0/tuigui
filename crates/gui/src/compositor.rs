//! The programs running in a compositor of ours, drawn into the pane as windows.
//!
//! The video path decodes a file; this one runs programs. Each program gets a Wayland
//! session of its own — a compositor in this process, one output — and the pixels it
//! commits are read straight out of that compositor's buffer.
//!
//! # What is whose
//!
//! The compositor holds the **session** ([`WindowingServer`]): the program, its clients
//! and its windows. It also holds the root window's place on the pane, so a resize of
//! the pane is a resize of the session. The layout of every other window lives on the
//! window itself, in its [`WindowState`], and the drawing walks the tree.

use std::sync::Arc;

use proto::windowing::{
    Client as ProtoClient, Window as ProtoWindow, WindowingServer,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{StatefulWidget, Widget, WidgetRef};

use crate::DebugOptions;

mod dummy;
mod inputs;

pub use dummy::DummyServer;

/// What this side parks on a client of the compositor.
///
/// Nothing: a client is a process, and the only fact this side needs about it — its pid
/// — the trait already carries.
pub type ClientData = ();

/// A window's place on the pane, in cells: its size and its position inside its
/// parent. This is what a window is drawn from, and what a drag moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowState {
    /// The window's size, in cells, border included.
    pub size: (u16, u16),
    /// The window's position inside its parent, in cells.
    pub position: (u16, u16),
}

/// A program, or several, running in a compositor of ours.
///
/// `T` is the window session: a real one (wayland, later macOS) or the dummy one
/// (`--dbg-dummy-windowing`), which draws windows nobody is running.
pub struct Compositor<T: WindowingServer<ClientData, WindowState>> {
    /// The session the windows live in. Dropping it ends them.
    windowing: T,
    /// The windows on the pane, in stacking order: the front is first.
    windows: std::collections::LinkedList<Box<dyn ProtoWindow<WindowState>>>,
    /// The switches this pane runs with (`--dbg-*`).
    debug: Arc<DebugOptions>,
    /// The pointer, as the compositor sees it between frames.
    pointer: inputs::Pointer,
    /// The pane's size as of the last render, in cells — what a change of
    /// pane is noticed against.
    last_pane: (u16, u16),
}

impl<T: WindowingServer<ClientData, WindowState>> Compositor<T> {
    /// Take a session and the switches, and start compositing.
    pub fn new(windowing: T, debug: Arc<DebugOptions>) -> Self {
        Self {
            windowing,
            windows: std::collections::LinkedList::new(),
            debug,
            pointer: inputs::Pointer::default(),
            last_pane: (0, 0),
        }
    }

    /// Whether the session's program has ended.
    ///
    /// A root client that has not connected yet is a program that has not
    /// started, not one that has ended: the session is still waiting.
    pub fn finished(&self) -> bool {
        self.windowing
            .root_client()
            .is_some_and(|client| !client.is_running())
    }

    /// Hand one event to the pane, which hands it to whatever it landed on.
    ///
    /// The pane is this side's to hit-test: it walks the window tree, finds the
    /// window a cell belongs to, and delivers a window-local event.
    pub fn input(&mut self, event: &termina::Event) -> std::io::Result<()> {
        inputs::input(self, event)
    }

    /// One session turn, advanced: accept what arrived, dispatch what the
    /// program did. A dummy session moves forward by being drawn instead.
    pub fn advance(&mut self, timeout: std::time::Duration) {
        self.windowing.advance(timeout);
    }
}

/// A client, drawn: its window at its place, the windows its program opened
/// after it inside it, and its helpers beside it.
pub struct TuiClient<'a> {
    client: &'a dyn ProtoClient<ClientData, WindowState>,
    /// The pulls: each window's place, and how far it now sits from it — the
    /// ended drags, plus the one under way.
    drags: Vec<(WindowState, (i16, i16))>,
}

impl<'a> TuiClient<'a> {
    pub fn new(
        client: &'a dyn ProtoClient<ClientData, WindowState>,
        drags: Vec<(WindowState, (i16, i16))>,
    ) -> Self {
        Self { client, drags }
    }
}

impl Widget for TuiClient<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // The client's window, then the windows its program opened after it,
        // each inside the one before.
        self.client.with_window(&mut |window| {
            render_window(window, area, true, &self.drags, buf)
        });
        // The helpers, beside it.
        self.client.with_children(&mut |child| {
            TuiClient::new(child, self.drags.clone()).render(area, buf);
        });
    }
}

/// One window: its own draw, a gray row under it (every window that is not
/// the root's), and its children inside it.
fn render_window(
    window: &dyn ProtoWindow<WindowState>,
    base: Rect,
    root: bool,
    drags: &[(WindowState, (i16, i16))],
    buf: &mut Buffer,
) {
    let mut place = WindowState::default();
    window.with_attached(&mut |state| place = *state);
    // A pull rides over its window's place: the ended drags, and the drag
    // under way, each belong to the one window they were grabbed on.
    let offset = match drags.iter().find(|(grabbed, _)| *grabbed == place) {
        Some((_, offset)) => *offset,
        None => (0, 0),
    };
    let area = Rect {
        x: (base.x as i32 + place.position.0 as i32 + offset.0 as i32).max(0) as u16,
        y: (base.y as i32 + place.position.1 as i32 + offset.1 as i32).max(0) as u16,
        width: place.size.0.max(1),
        height: place.size.1.max(1),
    };
    // The shadow, drawn first: a medium shade one column and one row over,
    // so the window reads as raised off the pane. The window's own draw
    // covers all but the strip that peeks out to the right and below.
    if !root {
        // One row taller, for the gray line the window leaves under it.
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
    WidgetRef::render_ref(window, area, buf);
    // The bar, under the window: one row, its full width, where there is
    // room left for it. Cleaned first, so what is behind it does not show
    // through the gray.
    if !root && area.bottom() < base.bottom() {
        let row = Rect {
            x: area.x,
            y: area.bottom(),
            width: area.width,
            height: 1,
        };
        ratatui::widgets::Clear.render(row, buf);
        buf.set_style(row, Style::new().bg(Color::DarkGray));
    }
    // The thin frame, just outside the window: a bar against each edge, in
    // the color of the bar. The top one sits above the window, the bottom
    // one below the bar, and the sides run from the one to the other.
    if !root {
        for y in area.y.saturating_sub(1)..area.bottom() {
            if y < base.y || y >= base.bottom() {
                continue;
            }
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
    window.with_children(&mut |child| render_window(child, area, false, drags, buf));
}

/// The compositor as the pane draws it: the session's clients, each as a
/// window tree.
impl<T: WindowingServer<ClientData, WindowState>> StatefulWidget for &mut Compositor<T> {
    type State = ();

    fn render(self, area: Rect, buf: &mut Buffer, _state: &mut Self::State) {
        // A change of pane is a change of session size: the server is asked
        // to resize, and it is the server that places its root window.
        let size = (area.width, area.height);
        let changed = self.last_pane != size;
        self.last_pane = size;
        if changed {
            self.windowing.set_size((area.width as u32, area.height as u32));
        }
        if let Some(client) = self.windowing.root_client() {
            let mut drags = self.pointer.pulled.clone();
            if let Some((place, _, _)) = self.pointer.drag {
                drags.push((place, self.pointer.offset()));
            }
            TuiClient::new(client, drags).render(area, buf);
        }
    }
}


