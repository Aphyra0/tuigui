//! Status UI for the shell around the streamed pane.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Render the shell and return the pane's inner rect — the top-left cell of
/// the bordered pane where the TGP image should be placed.
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

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" live capture ");
    let inner = block.inner(body);
    f.render_widget(block, body);

    f.render_widget(
        Paragraph::new(format!(" {status} · last chunk {last_bytes}B · q quit ")),
        footer,
    );

    inner
}
