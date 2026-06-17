//! Sidebar renderers. These are placeholders for milestone 1: the left pane
//! lists the single running session, the right pane shows general info.

use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
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
        Span::styled("Ctrl+M", key),
        Span::styled(" msgs", dim),
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

    // Messages: the persistent cross-instance conversation (newest first), with a
    // one-line snippet per message. Full bodies live in the Ctrl+M reader.
    lines.push(Line::from(""));
    let pending = app.pending_messages();
    let header = if pending > 0 {
        format!("Messages ({pending} unread)")
    } else {
        "Messages".to_string()
    };
    lines.push(Line::from(Span::styled(header, label())));
    let messages = app.messages();
    if messages.is_empty() {
        lines.push(Line::from(Span::styled(
            " (none yet)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Narrow pane: show the most recent handful as snippets, newest first.
        for m in messages.iter().take(6) {
            lines.push(Line::from(vec![
                Span::styled(format!(" #{}", m.from), Style::default().fg(Color::Cyan)),
                Span::styled("→", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("#{} ", m.to), Style::default().fg(Color::Cyan)),
                Span::styled(snippet(&m.body, 22), Style::default().fg(Color::Gray)),
            ]));
        }
        if messages.len() > 6 {
            lines.push(Line::from(Span::styled(
                " Ctrl+M for full log",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " (Ctrl+M: full log)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// First `max` chars of a message body, single-line (newlines → spaces), with an
/// ellipsis when truncated. For the narrow info-pane snippet.
fn snippet(body: &str, max: usize) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let s: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{s}…")
    } else {
        flat
    }
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

/// The full-screen cross-instance message reader (Ctrl+M). Renders the whole
/// persistent conversation — newest first, each message's full body word-wrapped
/// and the sender's own line breaks preserved — as a centered overlay over the
/// window. Scrolls with `app.msg_scroll` (↑↓ / PageUp-Down / wheel).
pub fn render_message_log(f: &mut Frame, area: Rect, app: &App) {
    // Centered reading pane with a small margin around it.
    let w = area.width.saturating_sub(4).min(110).max(1);
    let h = area.height.saturating_sub(2).max(1);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);

    let msgs = app.messages();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " Hub messages ({}) — ↑↓ scroll · Esc / Ctrl+M to close ",
            msgs.len()
        ));
    let inner = rect.inner(Margin::new(1, 1));
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if msgs.is_empty() {
        f.render_widget(
            Paragraph::new(" No hub messages yet — instances haven't sent any.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let now = now_unix();
    let wrap_w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{} ", m.from),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("→ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("#{}", m.to),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {} ago", ago(now.saturating_sub(m.ts))),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        for raw in m.body.split('\n') {
            for wrapped in wrap_text(raw, wrap_w) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(Color::Gray),
                )));
            }
        }
    }

    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    let scroll = app.msg_scroll.min(max_scroll);
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

/// Word-wrap one logical line to `width` columns, hard-breaking any word longer
/// than the width. An empty input yields one empty line (a blank row).
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        }
        while line.chars().count() > width {
            out.push(line.chars().take(width).collect());
            line = line.chars().skip(width).collect();
        }
    }
    out.push(line);
    out
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

