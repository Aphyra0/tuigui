//! The ring around a window and the bar under it: drawing the chrome, and the
//! resize a press on the ring starts.

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Offset, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Widget};
use termina::event::{MouseButton, MouseEvent, MouseEventKind};

use super::bar::Bar;
use crate::compositor::windowing::{StatefulWindow, WindowState};

#[derive(Default)]
pub struct Frame {
    bar: Bar,
    resize: Option<(Position, Rect)>,
}

impl Frame {
    /// Take the window's name for the frame about to be drawn, for the bar to show.
    pub fn set_title(&mut self, title: String) {
        self.bar.set_title(title);
    }

    pub fn handle_mouse(
        &mut self,
        window: &dyn StatefulWindow,
        mouse: MouseEvent,
        pane: Rect,
        content: Rect,
    ) -> Option<MouseEvent> {
        let row = u16::from(content.bottom() < pane.bottom());
        let bar = Rect::new(content.x, content.bottom(), content.width, row);
        let mouse = self.bar.handle_mouse(window, mouse, pane, bar)?;
        self.handle_resize(window, mouse, pane, content)
    }

    fn handle_resize(
        &mut self,
        window: &dyn StatefulWindow,
        mouse: MouseEvent,
        pane: Rect,
        content: Rect,
    ) -> Option<MouseEvent> {
        let at = Position::new(mouse.column, mouse.row);
        let ring = content.outer(Margin::new(1, 1));
        let started = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && ring.contains(at)
            && !ring.inner(Margin::new(1, 1)).contains(at);
        let Some((pressed, from)) = self.resize.or(started.then_some((at, content))) else {
            return Some(mouse);
        };

        let d = Offset::new(
            i32::from(at.x) - i32::from(pressed.x),
            i32::from(at.y) - i32::from(pressed.y),
        );
        let moved = from.offset(d);
        let (left, right, top) = (
            pressed.x < from.x,
            pressed.x >= from.right(),
            pressed.y < from.y,
        );

        #[rustfmt::skip]
        let area = {
            let x = if left { moved.x.clamp(pane.x, from.right().saturating_sub(2).max(pane.x)) } else { from.x };
            let y = if top { moved.y.clamp(pane.y, from.bottom().saturating_sub(1).max(pane.y)) } else { from.y };
            let w = if right { moved.right().clamp(from.x + 2, pane.right()) - from.x } else { from.right() - x };
            Rect::new(x, y, w, from.bottom() - y)
        };

        window.set_state(WindowState { area });
        self.resize = (!matches!(mouse.kind, MouseEventKind::Up(_))).then_some((pressed, from));
        None
    }
}

impl Widget for &mut Frame {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // `area` is the window's own cells; the chrome is the ring around them.
        Block::bordered()
            .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
            .border_style(Style::new().fg(Color::DarkGray))
            .render(area.outer(Margin::new(1, 1)), buf);
        let bar = Rect::new(area.x, area.bottom(), area.width, 1);
        Widget::render(&self.bar, bar, buf);
    }
}
