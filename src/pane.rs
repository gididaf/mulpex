//! Sidebar renderers. These are placeholders for milestone 1: the left pane
//! lists the single running session, the right pane shows general info.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::ui::border_style;

pub fn render_instances(f: &mut Frame, area: Rect, app: &App, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(" instances ");

    let items: Vec<ListItem> = if app.instances.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(none — Ctrl+T)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.instances
            .iter()
            .enumerate()
            .map(|(i, session)| {
                let active = i == app.active;
                let marker = if active { "▸ " } else { "  " };
                let (dot_color, word) = app.status_of(session.id()).indicator();

                // Active rows get the cyan highlight bar; the status dot/word
                // keep their own colour on top of it so state stays legible.
                let base = if active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let name_style = if active {
                    base
                } else {
                    base.fg(Color::Gray)
                };

                ListItem::new(Line::from(vec![
                    Span::styled(marker, base),
                    Span::styled("● ", base.fg(dot_color)),
                    Span::styled(format!("claude #{}", session.id()), name_style),
                    Span::styled(format!("  {}", word), base.fg(dot_color).add_modifier(Modifier::DIM)),
                ]))
            })
            .collect()
    };

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

pub fn render_info(f: &mut Frame, area: Rect, app: &App, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(" info ");

    let (cols, rows) = app.center_size();
    let active_label = match app.active_session() {
        Some(session) => format!(" {} running (#{} focused)", app.instances.len(), session.id()),
        None => " 0 running".to_string(),
    };
    let lines = vec![
        Line::from(Span::styled("Project", label())),
        Line::from(format!(" {}", app.project_name)),
        Line::from(Span::styled(
            format!(" {}", app.project_dir.display()),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled("Instances", label())),
        Line::from(active_label),
        Line::from(""),
        Line::from(Span::styled("Center pane", label())),
        Line::from(format!(" {}×{}", cols, rows)),
        Line::from(""),
        Line::from(Span::styled("Keyboard", label())),
        if app.keyboard_enhanced {
            Line::from(Span::styled(
                " enhanced (kitty)",
                Style::default().fg(Color::Green),
            ))
        } else {
            Line::from(Span::styled(
                " legacy (Ctrl+[ off)",
                Style::default().fg(Color::Yellow),
            ))
        },
        Line::from(""),
        Line::from(Span::styled("Status", label())),
        status_legend(Color::Green, "ready (waiting for you)"),
        status_legend(Color::Yellow, "working"),
        status_legend(Color::LightRed, "needs you"),
        Line::from(""),
        Line::from(Span::styled("Keys", label())),
        Line::from(" Ctrl+T    new instance"),
        Line::from(" Ctrl+]    next instance"),
        Line::from(" Ctrl+[    prev instance"),
        Line::from(" Ctrl+Q ×2 quit Mulpex"),
        Line::from(" Ctrl+C    → Claude"),
    ];

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn label() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// One `● description` row for the info-pane status legend.
fn status_legend(color: Color, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ● ", Style::default().fg(color)),
        Span::styled(text.to_string(), Style::default().fg(Color::Gray)),
    ])
}
