//! Status UI for the shell around the streamed pane.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
    Frame,
};

/// Render the shell and return the area the TGP image should be placed into —
/// the full body below the header / above the footer (no border).
pub fn draw(f: &mut Frame, app: &str, status: &str, last_bytes: usize) -> Rect {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Paragraph::new(format!(" tuigui — {app}")).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );

    f.render_widget(
        Paragraph::new(format!(" {status} · last chunk {last_bytes}B · q quit ")),
        footer,
    );

    body
}
