//! What the client draws.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::buffer::Buffer;
use ratatui::widgets::Widget;

/// Fills its area with pink.
///
/// A placeholder for the real surface: it proves the client owns the pane and
/// that its drawing reaches the terminal through `tui`, which is the part that
/// has to be true before anything is drawn on top of it. The colour is fixed
/// rather than derived from anything so that a run with nothing else working
/// still shows an unmistakable block of pink.
pub struct Pink;

impl Widget for Pink {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::new().bg(PINK));
    }
}

/// Hot pink, as the one thing on screen that cannot be a coincidence.
const PINK: Color = Color::Rgb(0xff, 0x69, 0xb4);
