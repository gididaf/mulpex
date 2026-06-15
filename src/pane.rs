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
            "(none — Ctrl+N)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.instances
            .iter()
            .enumerate()
            .map(|(i, session)| {
                let active = i == app.active;
                let marker = if active { "▸ " } else { "  " };
                let text = format!("{}● claude #{}", marker, session.id());
                let style = if active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Green)
                };
                ListItem::new(Line::from(Span::styled(text, style)))
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
