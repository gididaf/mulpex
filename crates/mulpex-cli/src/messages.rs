//! The messages feed: who said what to whom, and the address to reply at.
//!
//! `mcp::hub_send` appends a tab-separated line to `<state_dir>/messages.log` for
//! every message — on **both** sides of a cross-project send, so each project's
//! log is complete on its own. This reads it back.
//!
//! The point of the feed is not history for its own sake. An instance's inbox is
//! drained the moment it is read, so by the time anyone looks, the message is gone
//! from everywhere except here. That is why every row carries a **reply-able
//! address** rather than a bare number: `claude#3` in this project,
//! `<project>#<n>` in another — the same grammar `hub_send` accepts, so a row can
//! be acted on without translating anything.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the log's tail to read. The log is append-only and unbounded; this
/// is the window, not the whole file.
const TAIL_BYTES: u64 = 128 * 1024;

/// Default number of records shown.
pub const DEFAULT_MAX: usize = 40;

pub struct Msg {
    pub ts: u64,
    /// An **address** (`claude#2`, `central-one#3`), not a number: once a sender
    /// can live in a different project, the project is part of who they are.
    pub from: String,
    pub to: String,
    pub body: String,
}

/// Read the last `max` records, newest first.
pub fn read(path: &Path, max: usize) -> Vec<Msg> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let tail = len.min(TAIL_BYTES);
    if f.seek(SeekFrom::End(-(tail as i64))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // Seeking into the middle of the file almost certainly landed mid-line, and a
    // half record would be parsed as a whole one.
    if tail < len && !lines.is_empty() {
        lines.remove(0);
    }

    let mut out = Vec::new();
    for line in lines.iter().rev() {
        if out.len() >= max {
            break;
        }
        let mut parts = line.splitn(4, '\t');
        let (Some(ts), Some(from), Some(to), Some(body)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(ts) = ts.parse::<u64>() else { continue };
        out.push(Msg { ts, from: from.to_string(), to: to.to_string(), body: unescape(body) });
    }
    out
}

/// Undo `log_message`'s escaping. Order matters: unescaping `\\` first would turn
/// a literal `\` followed by `n` into a newline the sender never wrote.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Render the feed, oldest at the top so it reads as a conversation.
pub fn render(msgs: &[Msg], now: u64) -> String {
    if msgs.is_empty() {
        return "no messages yet\n".into();
    }
    let mut out = String::new();
    for m in msgs.iter().rev() {
        out.push_str(&format!("{:>7}  {} → {}\n", ago(now.saturating_sub(m.ts)), m.from, m.to));
        // The body is the part written by a model, in whatever language the user
        // works in — so it is the part that needs converting for a terminal that
        // does not reorder. Line by line, so one Hebrew line cannot flip the
        // alignment of the English ones around it. See `bidi.rs`.
        for line in crate::bidi::visual_block(&m.body).lines() {
            out.push_str(&format!("         {line}\n"));
        }
        out.push('\n');
    }
    out.push_str("reply with: hub_send to that address, or `mpx send <address> <text>`\n");
    out
}

/// Compact relative time. Absolute timestamps in a feed you glance at are noise —
/// what matters is whether something arrived just now or an hour ago.
fn ago(secs: u64) -> String {
    match secs {
        0..=1 => "now".into(),
        2..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mpx-msg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn records_come_back_newest_first_with_their_addresses() {
        let d = tmp("read");
        let p = d.join("messages.log");
        std::fs::write(
            &p,
            "100\tclaude#1\tclaude#2\thello\n\
             200\tcentral-one#3\tclaude#1\tfrom another project\n",
        )
        .unwrap();
        let msgs = read(&p, 10);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].ts, 200);
        assert_eq!(msgs[0].from, "central-one#3", "the project is part of the address");
        assert_eq!(msgs[1].body, "hello");
    }

    /// A multi-line message is stored escaped on one line. Getting the order of
    /// the unescapes wrong turns a literal backslash-n into a newline.
    #[test]
    fn escaping_round_trips_including_a_literal_backslash() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\tb"), "a\tb");
        assert_eq!(unescape("C:\\\\path"), "C:\\path");
        assert_eq!(unescape("a\\\\nb"), "a\\nb", "an escaped backslash then an n is not a newline");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    /// A malformed line must cost only itself.
    #[test]
    fn a_broken_line_does_not_poison_the_feed() {
        let d = tmp("broken");
        let p = d.join("messages.log");
        std::fs::write(&p, "not a record\n100\tclaude#1\tclaude#2\tfine\nalso bad\n").unwrap();
        let msgs = read(&p, 10);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "fine");
    }

    #[test]
    fn a_missing_log_is_empty_rather_than_an_error() {
        assert!(read(Path::new("/nonexistent/messages.log"), 10).is_empty());
        assert_eq!(render(&[], 0), "no messages yet\n");
    }

    #[test]
    fn the_feed_reads_oldest_first_and_names_the_reply_address() {
        let msgs = vec![
            Msg { ts: 90, from: "claude#2".into(), to: "claude#1".into(), body: "second".into() },
            Msg { ts: 50, from: "claude#3".into(), to: "claude#1".into(), body: "first".into() },
        ];
        let out = render(&msgs, 100);
        let first = out.find("first").unwrap();
        let second = out.find("second").unwrap();
        assert!(first < second, "oldest at the top, like a conversation");
        assert!(out.contains("claude#3 → claude#1"));
        assert!(out.contains("hub_send"), "every feed says how to reply");
    }

    #[test]
    fn relative_times_stay_short() {
        assert_eq!(ago(0), "now");
        assert_eq!(ago(45), "45s");
        assert_eq!(ago(90), "1m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(172_800), "2d");
    }
}
