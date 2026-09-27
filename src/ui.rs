//! Status UI for the shell around the streamed pane.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
    Frame,
};

/// Live metrics shown in the bottom status bar.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Frames rendered per second (1s rolling average).
    pub fps: f64,
    /// Bytes transmitted per second.
    pub bandwidth: f64,
    /// Wall-clock cost to capture and emit one frame, in milliseconds.
    pub capture_ms: f64,
    /// Virtual frame resolution in pixels.
    pub resolution: (u32, u32),
}

/// Render the shell and return the area the TGP image should be placed into —
/// the full body above a single status footer (no header, no border).
pub fn draw(f: &mut Frame, app: &str, status: &str, stats: &Stats) -> Rect {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

    let (rw, rh) = stats.resolution;
    let line = format!(
        " {status} · {app}  ·  {:.0} fps · {:.1} MiB/s · capture {:.1} ms · {rw}x{rh}  · q quit ",
        stats.fps,
        stats.bandwidth / (1024.0 * 1024.0),
        stats.capture_ms,
    );

    f.render_widget(
        Paragraph::new(line).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        footer,
    );

    body
}
