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

/// Split the whole window vertically into `[top bar, middle (3-pane band),
/// bottom bar]`. The top bar (Project + a rule) is 2 rows; the bottom bar (keys
/// + keyboard mode) is 1 row; the 3 panes get everything in between.
pub fn outer_layout(area: Rect) -> [Rect; 3] {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    [chunks[0], chunks[1], chunks[2]]
}

/// Split the window into `[left sidebar, center, right sidebar]`. The split is
/// taken over the middle band (between the top and bottom bars), so callers pass
/// the full window rect and still get the correct pane geometry.
pub fn layout(area: Rect) -> [Rect; 3] {
    let [_, middle, _] = outer_layout(area);
    let chunks = Layout::horizontal([
        Constraint::Length(LEFT_WIDTH),
        Constraint::Min(20),
        Constraint::Length(RIGHT_WIDTH),
    ])
    .split(middle);
    [chunks[0], chunks[1], chunks[2]]
}

/// The drawable rect of the center pane, inside its border. This is where the
/// embedded Claude screen lives — the single source of truth for both the PTY
/// size and mouse-coordinate translation.
pub fn center_inner_rect(area: Rect) -> Rect {
    let [_, center, _] = layout(area);
    center.inner(Margin::new(1, 1))
}

/// The drawable size of the center pane (inside its border), as `(cols, rows)`.
/// This is the size the embedded PTY is kept in sync with.
pub fn center_inner_size(area: Rect) -> (u16, u16) {
    let inner = center_inner_rect(area);
    (inner.width.max(1), inner.height.max(1))
}

pub fn draw(f: &mut Frame, app: &App) {
    let [top, _middle, bottom] = outer_layout(f.area());
    let [left, center, right] = layout(f.area());

    pane::render_top_bar(f, top, app);
    pane::render_bottom_bar(f, bottom, app);
    pane::render_instances(f, left, app, matches!(app.focus, Focus::Left));
    pane::render_info(f, right, app, matches!(app.focus, Focus::Right));

    // Center pane: border drawn by us, the focused Claude screen composited inside.
    // A pending quit confirmation takes over the title/border (red) so it's
    // unmissable right where the user is looking.
    let focused = matches!(app.focus, Focus::Center);
    let scrollback = app.active_session().map_or(0, |s| s.scrollback());
    let (title, center_border) = if app.quit_armed() {
        (
            " ⚠  press Ctrl+Q again to quit ".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if let Some(n) = app.copied_flash() {
        let id = app.active_session().map_or(0, |s| s.id());
        (
            format!(" claude #{}   ✓ copied {} char{} ", id, n, if n == 1 { "" } else { "s" }),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else if scrollback > 0 {
        // Scrolled into history: make it obvious you're not at live output.
        let id = app.active_session().map_or(0, |s| s.id());
        (
            format!(" claude #{}  ↑ scrollback −{} · type to return ", id, scrollback),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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
            highlight_selection(f, inner, app);
        }
        None => render_empty_center(f, inner),
    }
}

/// Overlay the drag-selection highlight onto the already-rendered center pane by
/// flipping the background of the selected cells (linear, reading-order range).
fn highlight_selection(f: &mut Frame, inner: Rect, app: &App) {
    let Some((sr, sc, er, ec)) = app.selection_span() else {
        return;
    };
    let last_col = inner.width.saturating_sub(1);
    let buf = f.buffer_mut();
    for row in sr..=er.min(inner.height.saturating_sub(1)) {
        let (c0, c1) = if sr == er {
            (sc, ec)
        } else if row == sr {
            (sc, last_col)
        } else if row == er {
            (0, ec)
        } else {
            (0, last_col)
        };
        for col in c0..=c1.min(last_col) {
            if let Some(cell) = buf.cell_mut((inner.x + col, inner.y + row)) {
                cell.set_bg(Color::Blue);
            }
        }
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
