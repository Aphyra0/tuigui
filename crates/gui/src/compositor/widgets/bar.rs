//! The row under a window, which moves it.

use ratatui::buffer::Buffer;
use ratatui::layout::{Offset, Position, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;
use termina::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::compositor::windowing::{StatefulWindow, WindowState};

#[derive(Default)]
pub struct Bar {
    drag: Option<Position>,
}

impl Bar {
    pub fn handle_mouse(
        &mut self,
        window: &dyn StatefulWindow,
        mouse: MouseEvent,
        pane: Rect,
        area: Rect,
    ) -> Option<MouseEvent> {
        let at = Position::new(mouse.column, mouse.row);
        let Some(last) = self.drag else {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && area.contains(at) {
                self.drag = Some(at);
                return None;
            }
            return Some(mouse);
        };
        let delta = Offset::new(
            i32::from(at.x) - i32::from(last.x),
            i32::from(at.y) - i32::from(last.y),
        );
        let room = pane.resize(Size::new(pane.width, pane.height.saturating_sub(1)));
        window.set_state(WindowState {
            area: (window.state().area + delta).clamp(room),
        });
        self.drag = (!matches!(mouse.kind, MouseEventKind::Up(_))).then_some(at);
        None
    }
}

impl Widget for &Bar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::new().bg(Color::DarkGray));
    }
}
