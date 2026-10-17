//! The client's TUI: a `StatefulWidget` that paints the Unicode placeholder
//! grid and overlays a stats readout on top of it.
//!
//! A widget rather than a bare `terminal.draw(|f| ...)` closure, for the same
//! reason `gobo-renderer`'s renderer is one: the drawing state (the image id,
//! the counters, the measured timings) has to survive between frames, and
//! ratatui's `StatefulWidget` is the mechanism for exactly that. Passing the
//! state as arguments to a closure would mean threading it through every call
//! site, and the overlay would have nowhere natural to live.
//!
//! # Why the grid is the whole screen
//!
//! Placeholder cells are ordinary text, so any cell *not* filled with a
//! placeholder is a hole the image cannot cover. The grid therefore fills the
//! full area, and the stats overlay is drawn on top of it.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::StatefulWidget;

use tuigui_wire::p2p;
use tuigui_wire::session;

/// The stats overlay's height in rows.
pub const OVERLAY_HEIGHT: u16 = 3;

/// The placeholder grid plus its stats overlay.
///
/// Stateless itself: everything it needs is in [`GridState`], which is what
/// `StatefulWidget::render` takes by `&mut`, so the widget and its state can
/// each be constructed at the call site.
pub struct GridWidget;

/// What the widget needs that only the caller has: the image the cells must
/// reference, and the numbers to display.
pub struct GridState {
    /// The id the server assigned. Every cell encodes it in its foreground
    /// color, so this must not change for the life of the grid: a new id would
    /// need every cell repainted, and would break the in-place frame edits the
    /// server relies on.
    pub image_id: u32,
    /// Which cell to draw, as the server decided when it published the session.
    ///
    /// The server knows whether anything re-renders this client's bytes, which
    /// is what decides between a placeholder the terminal resolves itself and a
    /// marker the server rewrites.
    pub grid: session::GridMode,
    /// Set when the server moved to the other image and every cell must be
    /// repainted with the new id.
    ///
    /// The server alternates between two images so a frame never lands in pixels
    /// the cells are already resolving against. The cost is this repaint: the id
    /// is part of each cell's color, so a grid still naming the previous image
    /// draws nothing.
    pub repaint_grid: bool,
    /// Client-side counters and timings.
    pub stats: Stats,
    /// The server's own report, absent until it sends one.
    ///
    /// Kept separate from `stats` rather than merged: the two sides measure
    /// different halves of the pipeline, and hiding which half a number came
    /// from would defeat the point of showing both.
    pub server: Option<p2p::ServerStats>,
}

impl StatefulWidget for GridWidget {
    type State = GridState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        // A grid the protocol cannot address is an error, not a thing to paper
        // over with a plausible-looking fallback cell. Painting the reason where
        // the image would be is the only way the user finds out.
        if let Err(e) = fill_placeholders(area, buf, state.image_id, state.grid) {
            buf.set_line(
                area.x,
                area.y,
                &Line::styled(e, Style::new().fg(Color::White).bg(Color::Red)),
                area.width,
            );
            return;
        }

        // Drawn last so it lands on top of the placeholder cells. The cells
        // underneath stay in the buffer, so the image keeps resolving behind the
        // overlay.
        let overlay = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(OVERLAY_HEIGHT),
            width: area.width,
            height: OVERLAY_HEIGHT.min(area.height),
        };
        render_stats(overlay, buf, state);
    }
}

/// Fill `area` with grid cells for `image_id`.
///
/// In [`GridMode::Placeholder`] a cell is the placeholder scalar alone, with the
/// image id in the foreground color: the terminal resolves it with no help. In
/// [`GridMode::Marker`] a cell is a magenta block, which is what has to survive a
/// multiplexer's re-render, and the wrapper rewrites it into the placeholder form
/// once it can apply the id for real.
fn fill_placeholders(
    area: Rect,
    buf: &mut Buffer,
    image_id: u32,
    mode: session::GridMode,
) -> Result<(), String> {
    let (r, g, b) = match mode {
        // The id itself, in 24-bit color. Getting the byte order wrong renders
        // nothing at all: the terminal looks up a different image id and finds
        // no image, without any error to show for it.
        session::GridMode::Placeholder => p2p::id_to_color(image_id),
        session::GridMode::Marker => p2p::MARKER_RGB,
    };
    let fg = Color::Rgb(r, g, b);

    // Every row and column index must exist in the table. Checked up front so
    // the failure names the size rather than showing a grid of wrong cells.
    let max = area.width.max(area.height) as usize;
    if max > p2p::DIACRITICS.len() {
        return Err(format!(
            "grid {}x{} needs diacritic index {max} but the table has {} entries: the \
             terminal is too large to address with Unicode placeholders",
            area.width,
            area.height,
            p2p::DIACRITICS.len(),
        ));
    }

    for y in 0..area.height {
        for x in 0..area.width {
            // Row and column count from the top-left of `area`. It is the full
            // screen today, but stating it relative to `area` keeps the indices
            // correct if the grid is ever drawn into a smaller rect.
            let cell = &mut buf[(area.x + x, area.y + y)];
            // Infallible here: both indices were bounds-checked above.
            let symbol = cell_symbol(y, x, mode).ok_or_else(|| {
                format!("no diacritic for row {y} column {x} within a {}x{} grid", area.width, area.height)
            })?;
            cell.set_symbol(&symbol);
            // Foreground, not background: both cells are glyphs, so what fills
            // the cell is the glyph's own color. A background would sit behind
            // the block and never show.
            cell.set_fg(fg);
            cell.set_bg(Color::Reset);
        }
    }
    Ok(())
}

/// The text of one grid cell: the cell's scalar, then row and column.
///
/// `None` when an index is outside the table. Returning the fallback diacritic
/// instead would draw a valid-looking cell that shows the wrong part of the
/// image, which is worse than admitting the index does not exist.
fn cell_symbol(row: u16, col: u16, mode: session::GridMode) -> Option<String> {
    let scalar = match mode {
        session::GridMode::Placeholder => p2p::PLACEHOLDER,
        session::GridMode::Marker => p2p::MARKER,
    };
    Some(format!(
        "{}{}{}",
        scalar,
        diacritic(row as usize)?,
        diacritic(col as usize)?,
    ))
}

/// The diacritic for a row or column index, or `None` if the table has no such
/// index.
fn diacritic(n: usize) -> Option<char> {
    p2p::DIACRITICS.get(n).copied()
}

/// Draw the stats readout into `area`.
///
/// The overlay is the client's window into both halves of the pipeline. The
/// client cannot see the cost of *showing* a frame, and the server cannot see
/// the cost of encoding one, so neither side is sufficient alone.
pub fn render_stats(area: Rect, buf: &mut Buffer, state: &GridState) {
    // Clamp to the buffer, not just to `area`. A caller that hands us an area
    // partly outside the buffer would otherwise panic on the first out-of-range
    // cell, which is a rough way to learn the terminal shrank.
    let area = area.intersection(buf.area);
    // The marker background, so a cell the overlay writes keeps the one thing
    // that identifies it as part of the grid. ratatui diffs against its own last
    // frame, so a cell whose symbol changes is resent: without this the overlay
    // would strip the marker from exactly the columns it covers, and the wrapper
    // would find no cell there.
    let (r, g, b) = p2p::MARKER_RGB;
    let marker_bg = Color::Rgb(r, g, b);
    for (i, line) in stats_lines(state).into_iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.y + area.height {
            break;
        }
        let width = area.width as usize;
        let mut spans = line.spans.into_iter();
        // Walked cell by cell rather than written as a line: the background has
        // to be set per cell, and `set_line` would clear what it does not cover.
        let mut x = 0usize;
        for span in spans.by_ref() {
            let text: Vec<char> = span.content.chars().collect();
            for ch in text {
                if x >= width {
                    break;
                }
                let cell = &mut buf[(area.x + x as u16, y)];
                cell.set_char(ch);
                cell.set_fg(span.style.fg.unwrap_or(Color::White));
                cell.set_bg(marker_bg);
                x += 1;
            }
            if x >= width {
                break;
            }
        }
    }
}

/// The overlay's lines, as label/value pairs.
///
/// One field group per line rather than side-by-side columns: the values are
/// long enough that a narrow terminal would truncate them, and a truncated
/// timing is worse than no timing.
fn stats_lines(state: &GridState) -> Vec<Line<'static>> {
    let s = &state.stats;
    let label = Style::new().fg(Color::DarkGray);
    // The readout must state its own foreground. The overlay is drawn on top of
    // placeholder cells whose fg is the *image id encoded as a color* (id 1 is
    // near-black, so the text vanished), and a span with no fg set inherits the
    // cell's. White here is a deliberate choice of foreground, not a default.
    let value = Style::new().fg(Color::White);

    let client = Line::from(vec![
        Span::styled("gui   ", label),
        Span::styled(
            format!(
                "sent {} ({})  {}  enc {}/frame  send {}/frame",
                s.sent_frames,
                human_bytes(s.sent_bytes),
                fps(s.fps_per_frame()),
                per_frame(ms(s.encode_total, s.sent_frames)),
                per_frame(ms(s.send_total, s.sent_frames)),
            ),
            value,
        ),
    ]);

    let client2 = Line::from(vec![
        Span::styled("      ", label),
        Span::styled(
            format!(
                "decode {}/frame  echo {}/frame  draw {}/frame",
                per_frame(ms(s.decode_total, s.sent_frames)),
                per_frame(ms(s.echo_total, s.sent_frames)),
                per_frame(ms(s.draw_total, s.sent_frames)),
            ),
            value,
        ),
    ]);

    let server = match &state.server {
        Some(sv) => Line::from(vec![
            Span::styled("server", label),
            Span::styled(
                format!(
                    " got {} ({})  err {}  {}x{}  decode {:.1}ms  push {:.1}ms  behind {}  re-sent {}",
                    sv.frames,
                    human_bytes(sv.bytes),
                    sv.errors,
                    sv.width,
                    sv.height,
                    sv.decode_us as f64 / 1000.0,
                    sv.sync_us as f64 / 1000.0,
                    s.sent_frames.saturating_sub(sv.frames),
                    sv.reasserts,
                ),
                value,
            ),
        ]),
        None => Line::from(vec![
            Span::styled("server", label),
            Span::styled(" waiting for its first report", value),
        ]),
    };

    vec![client, client2, server]
}

/// Client-side counters and cumulative timings.
///
/// Totals plus a frame count, not a running average: an average cannot be
/// inspected for whether it is still moving, and the reader can divide.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub sent_frames: u64,
    pub sent_bytes: u64,
    /// Time spent encoding a frame for the wire.
    pub encode_total: Duration,
    /// Time spent writing a frame to the socket, including any blocking on a
    /// server that is not keeping up.
    pub send_total: Duration,
    /// Time spent turning the decoded frame into JPEG or raw bytes.
    pub decode_total: Duration,
    /// Time spent reading the server's report back.
    pub echo_total: Duration,
    /// Time spent redrawing the grid.
    pub draw_total: Duration,
    /// When the first frame was sent, for the overall rate.
    pub started: Option<Instant>,
    /// The most recent frame's interval, for a readout that reacts before the
    /// overall average does.
    pub last_frame_at: Option<Instant>,
    pub last_interval: Duration,
}

impl Stats {
    /// Record one sent frame.
    pub fn record_send(&mut self, bytes: u64, encode: Duration, send: Duration) {
        let now = Instant::now();
        self.started.get_or_insert(now);
        if let Some(prev) = self.last_frame_at.replace(now) {
            self.last_interval = now.duration_since(prev);
        }
        self.sent_frames += 1;
        self.sent_bytes += bytes;
        self.encode_total += encode;
        self.send_total += send;
    }

    /// The instantaneous rate, from the last frame's interval.
    ///
    /// `None` before two frames have been sent, when there is no interval to
    /// measure. Returning `0.0` there would claim a measured frame rate of
    /// zero, which is a different and wrong statement.
    ///
    /// Shown rather than the overall rate because it is what responds to a
    /// change: the overall average barely moves once enough frames are in.
    pub fn fps_per_frame(&self) -> Option<f64> {
        if self.last_interval.is_zero() {
            return None;
        }
        Some(1.0 / self.last_interval.as_secs_f64())
    }
}

/// A duration divided by a count, in milliseconds, or `None` before any count.
///
/// `None` rather than a placeholder string, so the caller decides how to show
/// "no measurement yet" instead of parsing it back out of formatted text.
fn ms(total: Duration, count: u64) -> Option<String> {
    if count == 0 {
        return None;
    }
    Some(format!("{:.2}ms", total.as_secs_f64() * 1000.0 / count as f64))
}

/// A measured per-frame timing. A missing measurement must read as missing,
/// never as a small number that looks like a real one.
fn per_frame(v: Option<String>) -> String {
    v.unwrap_or_else(|| "unmeasured".into())
}

/// A measured rate, shown to one decimal, or `unmeasured`.
fn fps(v: Option<f64>) -> String {
    match v {
        Some(f) => format!("{f:.1}fps"),
        None => "unmeasured".into(),
    }
}

/// Bytes as a short human-readable string.
fn human_bytes(n: u64) -> String {
    const K: u64 = 1024;
    match n {
        0..K => format!("{n}B"),
        _ if n < K * K => format!("{:.1}K", n as f64 / K as f64),
        _ if n < K * K * K => format!("{:.1}M", n as f64 / (K * K) as f64),
        _ => format!("{:.1}G", n as f64 / (K * K * K) as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first cell is the marker plus the first two diacritics.
    ///
    /// Deliberately *not* the placeholder scalar: the client draws text a
    /// multiplexer re-renders unchanged, and the wrapper turns each marker into
    /// the placeholder cell, in the one place that knows the live image id.
    #[test]
    fn the_first_cell_is_the_marker_with_row_and_column_zero() {
        assert_eq!(
            cell_symbol(0, 0, session::GridMode::Marker).as_deref(),
            Some("\u{2588}\u{0305}\u{0305}")
        );
    }

    /// With no multiplexer in the path there is nothing to rewrite a marker, so
    /// the cells are placeholders the terminal resolves itself.
    #[test]
    fn a_placeholder_grid_uses_the_placeholder_scalar() {
        assert_eq!(
            cell_symbol(0, 0, session::GridMode::Placeholder).as_deref(),
            Some("\u{10EEEE}\u{0305}\u{0305}")
        );
    }

    /// Row and column advance independently, each from its own index.
    #[test]
    fn row_and_column_are_encoded_separately() {
        let m = session::GridMode::Marker;
        assert_eq!(cell_symbol(1, 0, m).as_deref(), Some("\u{2588}\u{030D}\u{0305}"));
        assert_eq!(cell_symbol(0, 1, m).as_deref(), Some("\u{2588}\u{0305}\u{030D}"));
    }

    /// Every cell of a marker grid is drawn in the marker color, which is the
    /// signal the wrapper keys on to find cells at all.
    ///
    /// The foreground, not the background: the cell is a full block, so a
    /// background would sit behind the glyph and never show.
    #[test]
    fn every_marker_cell_is_drawn_in_the_marker_color() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        let area = Rect::new(0, 0, 4, 3);
        fill_placeholders(area, &mut buf, 9, session::GridMode::Marker)
            .expect("a small grid fits");
        let (r, g, b) = p2p::MARKER_RGB;
        let want = Color::Rgb(r, g, b);
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(buf[(x, y)].fg, want, "cell ({x},{y}) lost the marker");
                assert_eq!(buf[(x, y)].bg, Color::Reset, "cell ({x},{y}) set a bg");
            }
        }
    }

    /// A placeholder grid carries the image id in the cell color instead, since
    /// the terminal resolves those cells with no help from the wrapper.
    #[test]
    fn every_placeholder_cell_carries_the_image_id() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 2));
        let area = Rect::new(0, 0, 2, 2);
        fill_placeholders(area, &mut buf, 42, session::GridMode::Placeholder)
            .expect("a small grid fits");
        let (r, g, b) = p2p::id_to_color(42);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(r, g, b));
    }

    /// Past the end of the table there is no valid diacritic, and the caller
    /// must be told so rather than handed a fallback that paints the wrong cell.
    #[test]
    fn a_row_past_the_table_is_reported_not_faked() {
        let far = p2p::DIACRITICS.len() + 100;
        assert_eq!(diacritic(far), None);
        assert_eq!(cell_symbol(far as u16, 0, session::GridMode::Marker), None);
    }

    /// The overlay is drawn inside its area and no further, so it cannot
    /// scribble over cells it was not given.
    #[test]
    fn the_overlay_stays_inside_its_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 8));
        let state = GridState {
            image_id: 1,
            grid: session::GridMode::Placeholder,
            repaint_grid: false,
            stats: Stats::default(),
            server: None,
        };
        render_stats(Rect::new(0, 6, 40, OVERLAY_HEIGHT), &mut buf, &state);
        // Row 5 is untouched; the overlay starts at row 6.
        assert_eq!(buf[(0, 5)].symbol(), " ");
        assert!(buf[(0, 6)].symbol().starts_with('g'));
    }

    /// An area that runs past the buffer is clamped rather than panicking: a
    /// terminal can shrink between the resize event and the redraw.
    #[test]
    fn an_overlay_past_the_buffer_is_clamped() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 8));
        let state = GridState {
            image_id: 1,
            grid: session::GridMode::Placeholder,
            repaint_grid: false,
            stats: Stats::default(),
            server: None,
        };
        render_stats(Rect::new(0, 6, 40, 10), &mut buf, &state);
        // Nothing to assert beyond "it returned"; the point is that it did.
        assert_eq!(buf[(0, 7)].symbol().len(), 1);
    }

    /// The server line reports the report's own numbers, including how far
    /// behind the client's send count the server is.
    #[test]
    fn the_server_line_shows_its_report() {
        let mut stats = Stats::default();
        stats.record_send(10, Duration::ZERO, Duration::ZERO);
        stats.record_send(10, Duration::ZERO, Duration::ZERO);
        stats.record_send(10, Duration::ZERO, Duration::ZERO);
        let state = GridState {
            image_id: 1,
            grid: session::GridMode::Placeholder,
            repaint_grid: false,
            stats,
            server: Some(p2p::ServerStats {
                frames: 1,
                bytes: 20,
                errors: 0,
                width: 64,
                height: 48,
                decode_us: 1500,
                sync_us: 2500,
                reasserts: 9,
            }),
        };
        let text: String = stats_lines(&state)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("got 1 (20B)"), "{text}");
        assert!(text.contains("decode 1.5ms"), "{text}");
        assert!(text.contains("push 2.5ms"), "{text}");
        // Three sent, one drawn server-side.
        assert!(text.contains("behind 2"), "{text}");
    }

    /// Before the server reports, the line says so rather than showing zeros
    /// that look like a real report of nothing happening.
    #[test]
    fn the_server_line_says_when_there_is_no_report_yet() {
        let state = GridState {
            image_id: 1,
            grid: session::GridMode::Placeholder,
            repaint_grid: false,
            stats: Stats::default(),
            server: None,
        };
        let text = stats_lines(&state)[2].to_string();
        assert!(text.contains("waiting"), "{text}");
    }
}
