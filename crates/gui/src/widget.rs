//! The client's TUI: a `StatefulWidget` that paints a full screen of colored
//! background cells and overlays a stats readout on top of it.
//!
//! A widget rather than a bare `terminal.draw(|f| ...)` closure, for the same
//! reason a renderer takes its state explicitly: the drawing state (the slot, the
//! counters, the measured timings) has to survive between frames, and ratatui's
//! `StatefulWidget` is the mechanism for exactly that. Passing the state as
//! arguments to a closure would mean threading it through every call site, and
//! the overlay would have nowhere natural to live.
//!
//! # Why the grid is the whole screen
//!
//! The image is drawn over the full rectangle, so any cell left unwritten is a
//! hole it cannot cover. The grid therefore fills the full area, and the stats
//! overlay is drawn on top of it.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::StatefulWidget;

use tuigui_wire::p2p;

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
    /// The id the server assigned.
    ///
    /// Unused by the drawing itself: the grid is colored background cells, not a
    /// placeholder grid, so no cell encodes an id. Kept because the handshake and
    /// the server's reports still carry it.
    pub image_id: u32,
    /// Which half of the double buffer the grid should paint.
    ///
    /// The client decides this and nothing else about the swap: it flips the slot
    /// after sending each frame, and the server maps the slot to whichever image
    /// it finished. Flipping has to repaint, because a pane re-renders only cells
    /// whose content changed, and the slot is what the colors are derived from.
    pub slot: usize,
    /// Set when the grid's painted content changed and the screen must be
    /// updated.
    ///
    /// The client's own buffer is otherwise identical between frames, so a draw
    /// without this would write nothing and the slot change would never reach the
    /// terminal.
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
        if let Err(e) = fill_background(area, buf, state.slot) {
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

/// Fill `area` with spaces carrying randomly chosen background colors.
///
/// The Unicode placeholder grid this replaced was slow: two combining diacritics
/// per cell meant three scalars, and the terminal spent its time shaping and
/// measuring them for every cell of every repaint. A space with a background is
/// one glyph, one cell, and no shaping.
///
/// The colors carry no information. They are picked from `state.slot` so that
/// flipping the slot changes every cell, which is what makes a repaint visible:
/// a pane re-renders only the cells whose content changed, and a grid of identical
/// cells would never repaint.
///
/// This is a bench marker, not a protocol. It draws a picture-shaped thing so the
/// cost of painting a full screen can be measured in isolation; the frame stream
/// is not connected to it.
fn fill_background(area: Rect, buf: &mut Buffer, slot: usize) -> Result<(), String> {
    // A tiny, cheap hash of (x, y, slot): deterministic, so two runs are
    // comparable, and slot-dependent, so a flip is a visible change.
    for y in 0..area.height {
        for x in 0..area.width {
            let h = color_hash(x, y, slot);
            let cell = &mut buf[(area.x + x, area.y + y)];
            cell.set_char(' ');
            cell.set_fg(Color::Reset);
            cell.set_bg(Color::Rgb(
                (h >> 16) as u8,
                (h >> 8) as u8,
                h as u8,
            ));
        }
    }
    Ok(())
}

/// A scattered 24-bit color for cell `(x, y)` in `slot`.
///
/// A small integer hash rather than a random number generator: no state to carry
/// between frames, and a cell's color is a function of its position, so the
/// picture is stable across repaints except for the slot.
fn color_hash(x: u16, y: u16, slot: usize) -> u32 {
    let mut h = (x as u32)
        .wrapping_mul(0x9E37_79B1)
        ^ (y as u32).wrapping_mul(0x85EB_CA6B)
        ^ (slot as u32).wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^= h >> 13;
    h.wrapping_mul(0x27D4_EB2D) ^ (h >> 16)
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
    // No background: the overlay's own text is written in a color that names no
    // slot, so the wrapper forwards those cells as ordinary text rather than
    // turning them into placeholders. The grid cells underneath keep their slot
    // color and still resolve.
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

    /// Every cell is a space with a background color.
    ///
    /// The Unicode grid this replaced cost three scalars per cell; a space with a
    /// background is one glyph, which is the whole point of the change.
    #[test]
    fn every_cell_is_a_colored_space() {
        let area = Rect::new(0, 0, 4, 3);
        let mut buf = Buffer::empty(area);
        fill_background(area, &mut buf, 0).expect("a small grid fits");
        for y in 0..3 {
            for x in 0..4 {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell ({x},{y})");
                assert!(
                    matches!(cell.bg, Color::Rgb(..)),
                    "cell ({x},{y}) must carry a background color"
                );
            }
        }
    }

    /// The two slots paint different colors, which is what carries a swap.
    ///
    /// A pane re-renders only the cells whose content changed, so if both slots
    /// painted the same colors the flip would never reach the screen.
    #[test]
    fn the_two_slots_paint_different_colors() {
        let area = Rect::new(0, 0, 8, 8);
        let mut first = Buffer::empty(area);
        let mut second = Buffer::empty(area);
        fill_background(area, &mut first, 0).expect("fits");
        fill_background(area, &mut second, 1).expect("fits");
        let differing = (0..8).flat_map(|y| (0..8).map(move |x| (x, y))).any(|(x, y)| {
            first[(x, y)].bg != second[(x, y)].bg
        });
        assert!(differing, "the slots must paint different colors");
    }

    /// A cell's color is a function of its position, not of hidden state.
    ///
    /// Deterministic so two runs are comparable; a random generator would make
    /// one frame's cost incomparable with the next, which is the measurement this
    /// grid exists for.
    #[test]
    fn a_cells_color_is_stable_for_same_position_and_slot() {
        assert_eq!(color_hash(3, 7, 0), color_hash(3, 7, 0));
        assert_ne!(
            color_hash(3, 7, 0),
            color_hash(7, 3, 0),
            "row and column must not be interchangeable"
        );
    }

    /// The overlay is drawn inside its area and no further, so it cannot
    /// scribble over cells it was not given.
    #[test]
    fn the_overlay_stays_inside_its_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 8));
        let state = GridState {
            image_id: 1,
            slot: 0,
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
            slot: 0,
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
            slot: 0,
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
            slot: 0,
            repaint_grid: false,
            stats: Stats::default(),
            server: None,
        };
        let text = stats_lines(&state)[2].to_string();
        assert!(text.contains("waiting"), "{text}");
    }
}
