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

impl DecoratedWindow {
    pub fn new(window: Arc<dyn StatefulWindow>, root: bool) -> Self {
        Self {
            window,
            frame: (!root).then(Frame::default),
        }
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
            frame.render(area, buf);
        }
    }
}
