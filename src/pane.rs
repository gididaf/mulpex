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

                let mut rows = vec![Line::from(vec![
                    Span::styled(marker, base),
                    Span::styled("● ", base.fg(dot_color)),
                    Span::styled(format!("claude #{}", session.id()), name_style),
                    Span::styled(format!("  {}", word), base.fg(dot_color).add_modifier(Modifier::DIM)),
                ])];
                // Second line: what this instance is currently working on (from
                // the hub), so you see at a glance who's doing what.
                if let Some(task) = app.task_of(session.id()) {
                    rows.push(Line::from(Span::styled(
                        format!("    {}", truncate(task, 24)),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                ListItem::new(rows)
            })
            .collect()
    };

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Full-width top bar: project name + path on one row, with a rule beneath it
/// separating it from the panes below.
pub fn render_top_bar(f: &mut Frame, area: Rect, app: &App) {
    let project = Line::from(vec![
        Span::styled(
            format!(" {}", app.project_name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {}", app.project_dir.display()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let rule = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(vec![project, rule]), area);
}

/// Full-width bottom bar: the key legend plus the keyboard-protocol indicator
/// (whether Ctrl+[ is distinguishable from Esc), all on one row.
pub fn render_bottom_bar(f: &mut Frame, area: Rect, app: &App) {
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::Gray);
    let sep = || Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let (kbd_text, kbd_color) = if app.keyboard_enhanced {
        ("kitty", Color::Green)
    } else {
        ("legacy: Ctrl+[ off", Color::Yellow)
    };
    let line = Line::from(vec![
        Span::styled(" Ctrl+T", key),
        Span::styled(" new", dim),
        sep(),
        Span::styled("Ctrl+]", key),
        Span::styled(" next", dim),
        sep(),
        Span::styled("Ctrl+[", key),
        Span::styled(" prev", dim),
        sep(),
        Span::styled("Ctrl+Q×2", key),
        Span::styled(" quit", dim),
        sep(),
        Span::styled("Ctrl+C", key),
        Span::styled(" → Claude", dim),
        Span::raw("   "),
        Span::styled(format!("[{kbd_text}]"), Style::default().fg(kbd_color)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// The right pane: the live coordination-hub state — the file-lock table and the
/// recent-edits feed. Identity (project) lives in the top bar and the key legend
/// in the bottom bar, so this pane is dedicated to what the instances are doing.
pub fn render_info(f: &mut Frame, area: Rect, app: &App, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused))
        .title(" info ");

    let mut lines = vec![Line::from(Span::styled("Locks", label()))];

    // File-lock table: each locked file → the instance holding it.
    let locks = app.locks();
    if locks.is_empty() {
        lines.push(Line::from(Span::styled(
            " (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let mut rows: Vec<(String, usize)> = locks
            .iter()
            .map(|(p, holder)| {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                (name, *holder)
            })
            .collect();
        rows.sort();
        for (name, holder) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!(" {name} "), Style::default().fg(Color::Gray)),
                Span::styled(format!("→ #{holder}"), Style::default().fg(Color::Cyan)),
            ]));
        }
    }

    // Waiting: instances whose edit is auto-waiting for a locked file to free.
    let waiting = app.waiting();
    if !waiting.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Waiting", label())));
        let mut rows: Vec<(usize, String, usize)> = waiting
            .iter()
            .map(|(id, (name, holder))| (*id, name.clone(), *holder))
            .collect();
        rows.sort();
        for (id, name, holder) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!(" #{id} ⏳ "), Style::default().fg(Color::Yellow)),
                Span::styled(name, Style::default().fg(Color::Gray)),
                Span::styled(format!(" (#{holder})"), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    // Recent edits feed (newest first): who edited what, and how long ago.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Recent edits", label())));
    let edits = app.recent_edits();
    if edits.is_empty() {
        lines.push(Line::from(Span::styled(
            " (none)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let now = now_unix();
        for e in edits {
            lines.push(Line::from(vec![
                Span::styled(format!(" #{} ", e.instance), Style::default().fg(Color::Cyan)),
                Span::styled(e.name.clone(), Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("  {} ago", ago(now.saturating_sub(e.ts))),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    // Hub messages: total queued across all instance inboxes (debug view of the
    // cross-instance mailbox).
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Messages", label())));
    let pending = app.pending_messages();
    if pending == 0 {
        lines.push(Line::from(Span::styled(
            " (none queued)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {pending} queued"),
            Style::default().fg(Color::Cyan),
        )));
    }

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// Truncate `s` to at most `max` chars, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn label() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Current unix time in seconds (for relative-age formatting).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact relative-age label, e.g. `5s`, `3m`, `2h`.
fn ago(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}
