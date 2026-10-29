//! The row under a window: what it is called, the grip that moves it, and the
//! button that asks it to close.

use ratatui::buffer::Buffer;
use ratatui::layout::{Offset, Position, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;
use termina::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::compositor::windowing::{StatefulWindow, WindowState};

/// Eight dots, the standard grip: the bar is what a window is moved by.
const GRIP: char = '⣿';

/// The button that asks the window to close, drawn on its last cell.
const CLOSE: char = '✕';

#[derive(Default)]
pub struct Bar {
    drag: Option<Position>,
    /// Whether the pointer is over the close button, which is the whole of what
    /// makes it red: a press is one event, and it would have nothing to warn about.
    closing: bool,
    /// What the window calls itself, as of the frame being drawn. Held here rather
    /// than read in `render`, because a `Widget` is handed an area and nothing
    /// else.
    title: String,
}

impl Bar {
    /// Take the window's name for the frame about to be drawn.
    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    /// Where the close button sits, when the bar has the room for one. One cell in
    /// from the right edge, so it is not flush against the frame's own column.
    fn close_area(area: Rect) -> Option<Rect> {
        (area.width > 3).then(|| Rect::new(area.right() - 2, area.y, 1, 1))
    }

    pub fn handle_mouse(
        &mut self,
        window: &dyn StatefulWindow,
        mouse: MouseEvent,
        pane: Rect,
        area: Rect,
    ) -> Option<MouseEvent> {
        let at = Position::new(mouse.column, mouse.row);
        let close = Self::close_area(area).filter(|close| close.contains(at));
        self.closing = close.is_some();

        if close.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                window.terminate();
            }
            // The button is not a grip: nothing drags from under it, and the
            // press does not fall through to the window either.
            return None;
        }

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
        // Grey on a darker grey: the bar is chrome, and every cell of it is grey
        // unless the close button says otherwise.
        let style = Style::new().fg(Color::Gray).bg(Color::DarkGray);
        // Every cell is written, not just styled: the bar is drawn over whatever
        // was there, and a styled cell keeps the character it already had.
        for column in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((column, area.y)) {
                cell.set_char(' ').set_style(style);
            }
        }

        let mut x = area.x;
        let close = Bar::close_area(area);
        // The grip, a space, then the name — clipped so it never reaches the
        // close button.
        let text_end = close.map_or(area.right(), |close| close.x);
        let text = format!("{GRIP} {}", self.title);
        for grapheme in text.chars() {
            if x >= text_end {
                break;
            }
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char(grapheme).set_style(style);
            }
            x += 1;
        }

        if let Some(close) = close {
            if let Some(cell) = buf.cell_mut((close.x, close.y)) {
                cell.set_char(CLOSE).set_style(style);
                if self.closing {
                    cell.set_fg(Color::Red);
                }
            }
        }
    }
}
