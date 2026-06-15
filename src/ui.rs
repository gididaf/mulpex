//! The 3-pane layout and top-level render: instances sidebar | Claude | info.

use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Focus};
use crate::pane;

/// Fixed sidebar widths; the center pane takes the remaining space.
pub const LEFT_WIDTH: u16 = 30;
pub const RIGHT_WIDTH: u16 = 34;

/// Split an area into `[left sidebar, center, right sidebar]`.
pub fn layout(area: Rect) -> [Rect; 3] {
    let chunks = Layout::horizontal([
        Constraint::Length(LEFT_WIDTH),
        Constraint::Min(20),
        Constraint::Length(RIGHT_WIDTH),
    ])
    .split(area);
    [chunks[0], chunks[1], chunks[2]]
}

/// The drawable size of the center pane (inside its border), as `(cols, rows)`.
/// This is the size the embedded PTY is kept in sync with.
pub fn center_inner_size(area: Rect) -> (u16, u16) {
    let [_, center, _] = layout(area);
    let inner = center.inner(Margin::new(1, 1));
    (inner.width.max(1), inner.height.max(1))
}

pub fn draw(f: &mut Frame, app: &App) {
    let [left, center, right] = layout(f.area());

    pane::render_instances(f, left, app, matches!(app.focus, Focus::Left));
    pane::render_info(f, right, app, matches!(app.focus, Focus::Right));

    // Center pane: border drawn by us, the focused Claude screen composited inside.
    // A pending quit confirmation takes over the title/border (red) so it's
    // unmissable right where the user is looking.
    let focused = matches!(app.focus, Focus::Center);
    let (title, center_border) = if app.quit_armed() {
        (
            " ⚠  press Ctrl+Q again to quit ".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        let title = match app.active_session() {
            Some(session) => format!(" claude #{} ", session.id()),
            None => " claude ".to_string(),
        };
        (title, border_style(focused))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(center_border)
        .title(title);
    let inner = center.inner(Margin::new(1, 1));
    f.render_widget(block, center);

    match app.active_session() {
        Some(session) => {
            if let Ok(parser) = session.parser().read() {
                f.render_widget(PseudoTerminal::new(parser.screen()), inner);
            }
        }
        None => render_empty_center(f, inner),
    }
}

/// The clean center pane shown when there is no focused Claude.
fn render_empty_center(f: &mut Frame, inner: Rect) {
    if inner.height < 2 {
        return;
    }
    let hint = Paragraph::new(vec![
        Line::from("No active Claude").style(Style::default().fg(Color::Gray)),
        Line::from("Ctrl+T to start one")
            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)),
    ])
    .alignment(Alignment::Center);

    let y = inner.y + inner.height.saturating_sub(2) / 2;
    let area = Rect::new(inner.x, y, inner.width, 2);
    f.render_widget(hint, area);
}

pub fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}
