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
    /// Wall-clock time the source spent sleeping to hit the target fps.
    pub pacing_ms: f64,
    /// Virtual frame resolution in pixels.
    pub resolution: (u32, u32),
    /// Wall-clock cost of encoding a frame's pixels into TGP bytes (base64).
    pub encode_ms: f64,
    /// Wall-clock cost of sending a drained batch to the terminal: buffered
    /// writes plus the blocking flush (PTY handoff + terminal consumption),
    /// matching the stress binary's write stage. Averages over 1s.
    pub sink_ms: f64,
    /// Wall-clock cost of downscaling frame payloads (block LOD rungs +
    /// loading-layer coarse canvas), per frame (1s rolling average).
    pub downscale_ms: f64,
    /// Per-phase capture breakdown (averages), when the source reports it.
    pub announce_ms: f64,
    pub setup_ms: f64,
    pub copy_ms: f64,
    pub readout_ms: f64,
    /// Wall-clock cost of the ratatui shell redraw (footer diff + write),
    /// per frame (1s rolling average).
    pub draw_ms: f64,
    /// Tiles edited (a=f) in the last frame, over the total block grid slots.
    pub tiles_edited: u32,
    pub tiles_total: u32,
}

/// Render the shell and return the area the TGP image should be placed into —
/// the full body above three status footer rows (no header, no border).
pub fn draw(f: &mut Frame, _app: &str, _status: &str, stats: &Stats) -> Rect {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(f.area());

    let (rw, rh) = stats.resolution;
    let phases = if stats.announce_ms > 0.0 {
        format!(
            " · phases: announce {:.2} · setup {:.2} · copy {:.2} · readout {:.2} ms",
            stats.announce_ms, stats.setup_ms, stats.copy_ms, stats.readout_ms,
        )
    } else {
        String::new()
    };
    let tiles = if stats.tiles_total > 0 {
        let pct = stats.tiles_edited as f64 * 100.0 / stats.tiles_total as f64;
        format!("  ·  Tiles: {:.0}% updated", pct)
    } else {
        String::new()
    };
    let line1 = format!(
        " FPS: {:.0}  ·  Bw: {:.1} MiB/s  ·  Cap: {:.1} ms  ·  Enc: {:.1} ms  ·  Sink: {:.1} ms  ·  Draw: {:.1} ms  ·  {rw}x{rh}{phases}{tiles}",
        stats.fps,
        stats.bandwidth / (1024.0 * 1024.0),
        stats.capture_ms,
        stats.encode_ms,
        stats.sink_ms,
        stats.draw_ms,
    );

    f.render_widget(
        Paragraph::new(vec![line1.into()])
            .wrap(ratatui::widgets::Wrap { trim: true })
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        footer,
    );

    body
}
