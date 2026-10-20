//! Which panel is open over the session.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Paragraph, Widget as _};

/// What is open over the session.
///
/// Only one at a time, which is why this is an enum and not a set of flags: the panels
/// are drawn in the same place, and two of them open would be one covering the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupState {
    /// Nothing open. The session has the whole pane.
    None,
    /// Statistics about the stream. Not measured yet.
    Stats,
    /// Settings. Not built yet.
    Settings,
}

/// The buttons on the bottom line, left to right.
pub const BUTTONS: [&str; 2] = ["stats", "settings"];

/// The buttons' labels, as the text drawn for each.
pub fn label(index: usize) -> &'static str {
    BUTTONS[index]
}

/// Which button, if any, a click at `column` lands on.
///
/// Laid out left to right with a gap, which is the same arithmetic the drawing uses: the
/// one place the two could disagree is here, so there is one function for both.
pub fn button_at(column: u16) -> Option<usize> {
    let mut x = 1u16;
    for (index, label) in BUTTONS.iter().enumerate() {
        let width = label.len() as u16 + 2;
        if column >= x && column < x + width {
            return Some(index);
        }
        x += width + 1;
    }
    None
}

/// The button this label opens.
pub fn popup_for(index: usize) -> PopupState {
    match BUTTONS[index] {
        "stats" => PopupState::Stats,
        _ => PopupState::Settings,
    }
}

impl PopupState {
    /// Whether the button at `index` is the one showing this panel.
    pub fn is_open_for(self, index: usize) -> bool {
        self == popup_for(index)
    }

    /// Draw the panels that are actually built, over `content`.
    ///
    /// Both are placeholders: what they will show is not measured yet, and a panel that
    /// claimed numbers would be worse than one that says it has none.
    pub fn render(self, content: Rect, buf: &mut Buffer) {
        match self {
            PopupState::None => {}
            PopupState::Stats => {
                let panel = centered(content, 40, 7);
                buf.set_style(panel, Style::new().bg(Color::Rgb(0x20, 0x20, 0x40)));
                Paragraph::new("stats: not measured yet")
                    .centered()
                    .render(panel, buf);
            }
            PopupState::Settings => {
                let panel = centered(content, 40, 7);
                buf.set_style(panel, Style::new().bg(Color::Rgb(0x20, 0x40, 0x20)));
                Paragraph::new("settings: none yet")
                    .centered()
                    .render(panel, buf);
            }
        }
    }
}

/// A `w`x`h` rectangle in the middle of `area`, clamped to fit.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
