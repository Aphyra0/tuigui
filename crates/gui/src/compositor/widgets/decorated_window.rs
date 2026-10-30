//! A window with its chrome: the window's own cells, and the frame around it.
//! The root has no frame — it is the pane, not a window over anything.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{Clear, Widget, WidgetRef};
use termina::event::MouseEvent;

use super::frame::Frame;
use crate::compositor::windowing::{StatefulWindow, event_map};

/// One window, as the pane draws it and hands events to it.
pub struct DecoratedWindow {
    pub window: Arc<dyn StatefulWindow>,
    pub frame: Option<Frame>,
}

/// One cell of the terminal, in pixels: the whole grid divided out.
///
/// A window is laid out in cells and its program draws in pixels, so this is the
/// conversion between the two, and the terminal's own report is the only place it
/// comes from. A grid of no cells would be a division by zero rather than a size.
fn cell_size(winsize: rustix::termios::Winsize) -> (u32, u32) {
    (
        (u32::from(winsize.ws_xpixel) / u32::from(winsize.ws_col).max(1)).max(1),
        (u32::from(winsize.ws_ypixel) / u32::from(winsize.ws_row).max(1)).max(1),
    )
}

impl DecoratedWindow {
    pub fn new(window: Arc<dyn StatefulWindow>, root: bool) -> Self {
        Self {
            window,
            frame: (!root).then(Frame::default),
        }
    }

    /// Tell the window's program to draw at the size the window occupies.
    ///
    /// A window is a box of cells on the pane, and its program draws a picture: those
    /// two are the same fact only if someone converts one into the other, which is what
    /// this does. Without it a window keeps drawing at whatever size it chose when it
    /// was opened, and the pane stretches that picture over the box — a window that
    /// looks right only if the two sizes happen to agree, which for the root they do
    /// because it is told the pane's own size.
    ///
    /// The root has no chrome, so its box is the pane and it is measured in pixels
    /// directly by the compositor; every other window loses a row to its bar and its
    /// ring, and is measured here.
    pub fn maybe_set_size(&self, winsize: rustix::termios::Winsize) {
        if self.frame.is_none() {
            return;
        }
        let size = cell_size(winsize);
        let area = self.window.state().area;
        self.window.set_size((
            u32::from(area.width) * size.0,
            u32::from(area.height) * size.1,
        ));
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent, pane: Rect) -> Option<MouseEvent> {
        let area = self.window.state().area;
        let mouse = self
            .frame
            .as_mut()
            .and_then(|frame| frame.handle_mouse(self.window.as_ref(), mouse, pane, area))?;
        if !area.contains(Position::new(mouse.column, mouse.row)) {
            return Some(mouse);
        }
        if let Some(event) = event_map::pointer(
            &mouse,
            (
                f64::from(mouse.column - area.x),
                f64::from(mouse.row - area.y),
            ),
        ) {
            let _ = self.window.pointer_input(event);
        }
        None
    }
}

impl Widget for &mut DecoratedWindow {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        WidgetRef::render_ref(self.window.as_ref(), area, buf);
        if let Some(frame) = self.frame.as_mut() {
            // The bar shows what the window calls itself, which only the window
            // knows: it is read once per frame, here, rather than kept in step.
            frame.set_title(self.window.title());
            frame.render(area, buf);
        }
    }
}
