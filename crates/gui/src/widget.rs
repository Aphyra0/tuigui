//! What the client draws.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Fills its area with a colour and, in the middle, what this side knows.
///
/// The colour is the state of the handshake, which is the one thing the client
/// knows before anything else works. The text says the same thing in words: the
/// advert it announced while waiting, and the server's label once accepted. A
/// blank pane and a broken one look the same, so neither state is allowed to be
/// empty.
pub struct Session {
    /// Whether the server has answered.
    ///
    /// A separate field from the text rather than inferred from it: which state
    /// the session is in and what is drawn are different facts, and a widget that
    /// derived one from the other could not show anything while waiting.
    established: bool,
    /// What to draw, one entry per line.
    lines: Vec<String>,
    /// Tiled over the whole area instead of `lines`, when set.
    ///
    /// A character rather than a string: the point of a fill is that it repeats,
    /// and one field cannot disagree with itself about how many there are.
    fill: Option<char>,
}

impl Session {
    /// Nothing heard yet: the client is waiting to be found, and shows the advert
    /// it is waiting on.
    ///
    /// Showing the advert is not decoration. A client that cannot be dialed is
    /// stuck in this state and can say exactly why: the address it announced is on
    /// the screen, and a server that never dialed it is a server that never read
    /// it.
    pub fn waiting(advert: &str) -> Self {
        Self {
            established: false,
            lines: advert.lines().map(str::to_string).collect(),
            fill: None,
        }
    }

    /// The server accepted this client and named the link.
    pub fn established(label: String) -> Self {
        Self {
            established: true,
            lines: vec![label],
            fill: None,
        }
    }

    /// Established, and tiled edge to edge with `fill`.
    ///
    /// A separate constructor from [`Session::established`] because the text and
    /// the fill answer different questions: the label says which link this is,
    /// the fill is proof the pane is being painted at all. Filling over the label
    /// would throw away the first to show the second.
    pub fn filled(fill: char) -> Self {
        Self {
            established: true,
            lines: vec![],
            fill: Some(fill),
        }
    }
}

impl Widget for Session {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let background = if self.established {
            // Accepted: green, the answer the user is looking for.
            GREEN
        } else {
            // Waiting: a colour that cannot be mistaken for success.
            PINK
        };
        buf.set_style(area, Style::new().bg(background));

        // A fill is drawn instead of the lines, never over them: `filled` carries
        // no lines to lose, and this keeps the two paths from both writing a cell.
        if let Some(fill) = self.fill {
            for y in area.y..area.y.saturating_add(area.height) {
                for x in area.x..area.x.saturating_add(area.width) {
                    // Every cell, including the ones past the last column of a row:
                    // the point is a pane with no gaps in it, and skipping cells is
                    // how the background shows through.
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_char(fill);
                    }
                }
            }
            return;
        }

        // Centred as a block: off-centre text in a full-screen fill reads as an
        // accident rather than an answer. Clipped rather than wrapped, so a narrow
        // pane shows a prefix instead of pushing text out of the area.
        let width = area.width as usize;
        let height = area.height as usize;
        let first = area.y + (height.saturating_sub(self.lines.len()) / 2) as u16;
        for (row, line) in self.lines.iter().enumerate() {
            let row = first + row as u16;
            if row >= area.y + area.height {
                // More lines than the pane has rows. Drawing past the area would
                // land on whatever is next to it.
                break;
            }
            let line = clip(line, width);
            let x = area.x + ((width - line.chars().count()) / 2) as u16;
            buf.set_string(x, row, line, Style::new());
        }
    }
}

/// The longest prefix of `text` that fits in `width` columns.
///
/// By characters, not bytes: an advert can carry any text, and slicing at a byte
/// offset would panic in the middle of a multi-byte one.
fn clip(text: &str, width: usize) -> &str {
    match text.char_indices().nth(width) {
        Some((cut, _)) => &text[..cut],
        None => text,
    }
}

/// Hot pink, as the one thing on screen that cannot be a coincidence.
const PINK: Color = Color::Rgb(0xff, 0x69, 0xb4);
/// The colour of an accepted session.
const GREEN: Color = Color::Rgb(0x27, 0xae, 0x60);
