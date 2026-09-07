//! The project picker: one popup that is both the switcher and the opener.
//!
//! Three sources, in one list, in this order:
//!
//! 1. **Open projects** — the tmux sessions, in tab order. This is what makes the
//!    picker a switcher, and it is first because switching is the common case.
//! 2. **Recents** — `recents.rs`, so it is useful before you have opened anything
//!    through `mpx` on this machine.
//! 3. **The filesystem** — any directory, reached by typing a path.
//!
//! One list rather than three screens because the question is always the same one
//! ("which project?") and the answer is usually four keystrokes into it. Typing
//! filters; it never switches mode.
//!
//! **The picker performs the switch itself.** `display-popup -E` can only run a
//! command and close when it exits — there is nowhere for a chosen path to be
//! *returned* to. So this process calls `switch-client`, which is also why it must
//! do the opening of a new project rather than handing that back to a caller.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::tmux::Tmux;
use crate::tui::{pad, size, tail, trunc, RawMode};

/// How many directories a single browse listing will show. A path prefix matching
/// a thousand directories is a typo, not a query, and reading them all is time
/// spent between one keystroke and the next.
const MAX_DISK: usize = 60;

/// The shell command `display-popup -E` runs.
///
/// The binary's own absolute path, shell-quoted: a popup is spawned by the tmux
/// **server**, which carries whatever environment it first started with, so `PATH`
/// cannot be relied on to find `mpx` — the same reason `__MPX_BIN__` is
/// substituted into the config.
pub fn popup_command(exe: &Path) -> String {
    format!("{} pick", crate::sh_quote(&exe.to_string_lossy()))
}

/// The picker, in either of the two places it runs.
///
/// **Inside tmux** it is a popup over a live client, and choosing means
/// `switch-client`. **Standalone** it is the front door — `mpx` with nothing
/// running — where there is no client to move and choosing means *becoming* one.
/// That is the entire difference, and it is read off `$TMUX` rather than passed in,
/// because the answer is a property of where the process is, not of who called it.
pub fn run(conf: PathBuf) -> Result<()> {
    let t = Tmux::new(conf);
    let attached = std::env::var_os("TMUX").is_some();

    // The raw-mode guard has to be gone before `attach`, which `exec`s: a replaced
    // process runs no destructor, so the terminal would be handed on in raw mode
    // with its cursor hidden. Hence the loop is a function that returns a choice
    // rather than acting on it.
    let chosen = choose(&t, attached)?;
    match chosen {
        Some(session) => {
            t.attach(&session)?;
            unreachable!("attach replaces the process")
        }
        None => Ok(()),
    }
}

/// Run the list until something is chosen. `Some(session)` means "attach to this",
/// which only ever happens standalone; inside tmux the switch has already happened.
fn choose(t: &Tmux, attached: bool) -> Result<Option<String>> {
    let mut term = RawMode::enable();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?25l");

    // Read once, on the way in. The popup is modal and lives for a few seconds;
    // re-scanning tmux on every keystroke would buy nothing but latency.
    let open = open_projects(t);
    let recent = crate::recents::list();
    // Offered, never opened. `mpx` used to create a project for whatever directory
    // you happened to be standing in; now it puts that directory at the top of the
    // list and waits, which is what an IDE's welcome screen does.
    let here = std::env::current_dir().ok().and_then(|d| std::fs::canonicalize(d).ok());

    let mut query = String::new();
    let mut cursor = 0usize;
    let mut note = String::new();
    let mut last = String::new();

    loop {
        let entries = candidates(&open, &recent, here.as_deref(), &query);
        cursor = cursor.min(entries.len().saturating_sub(1));

        let frame = render(&entries, cursor, &query, &note, size((80, 24)));
        if frame != last {
            let _ = out.write_all(frame.as_bytes());
            let _ = out.flush();
            last = frame;
        }

        let seq = term.read();
        if seq.is_empty() {
            continue;
        }
        match input(&mut query, &seq) {
            Act::None => {}
            Act::Cancel => return Ok(None),
            Act::Up => cursor = cursor.saturating_sub(1),
            Act::Down => cursor = (cursor + 1).min(entries.len().saturating_sub(1)),
            // Typing moves the ground under the cursor, so it goes back to the top
            // — the best match is what you were narrowing towards.
            Act::Typed => cursor = 0,
            // Complete rather than open: the whole point is going deeper into a
            // path without retyping it. A trailing `/` is what makes the next
            // listing the directory's contents rather than its siblings.
            Act::Complete => {
                if let Some(e) = entries.get(cursor) {
                    query = format!("{}/", pretty(&e.dir));
                    cursor = 0;
                }
            }
            Act::Enter => {
                let Some(entry) = entries.get(cursor) else { continue };
                // Redraw before the slow part: opening a project spawns a claude,
                // and a popup that freezes with no explanation is how "did my key
                // do anything?" starts.
                note = format!("opening {}…", pretty(&entry.dir));
                let frame = render(&entries, cursor, &query, &note, size((80, 24)));
                let _ = out.write_all(frame.as_bytes());
                let _ = out.flush();

                match go(t, entry, attached) {
                    Ok(session) => return Ok(session),
                    Err(e) => {
                        note = format!("✗ {e:#}");
                        last.clear();
                    }
                }
            }
        }
    }
}

/// Open the project if it is not open yet, then go to it.
///
/// Returns the session to **attach** to when there is no client to move, and
/// `None` when the move has already happened.
fn go(t: &Tmux, entry: &Entry, attached: bool) -> Result<Option<String>> {
    let session = match &entry.session {
        Some(s) => {
            // Already open, so there is nothing to build — but this is still the
            // project you just chose to work in, and recents is ordered by that.
            // `ensure_session` records it on the other branch; skipping it here
            // left the list saying the last thing you *created* was the last thing
            // you touched, which it is not (measured 2026-09-06).
            crate::recents::add(&entry.dir);
            s.clone()
        }
        None => crate::ensure_session(t, &entry.dir)?,
    };
    if !attached {
        return Ok(Some(session));
    }
    t.switch_client(&["-t", &format!("={session}")])?;
    Ok(None)
}

/// One row: a directory, and whether it is already open.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    dir: PathBuf,
    /// The tmux session, when this project is open. Also the flag for it.
    session: Option<String>,
    kind: Kind,
    /// The directory the shell was standing in. Sorted first and labelled, and
    /// that is *all* it gets — `mpx` no longer opens it for you.
    here: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Open,
    Recent,
    Disk,
}

fn open_projects(t: &Tmux) -> Vec<(String, PathBuf)> {
    crate::core::scan(t)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.session, p.dir))
        .collect()
}

/// The list, filtered by what has been typed.
///
/// Here, then open, then recents, then whatever the path being typed points at —
/// and a directory never appears twice, so a recent project that is currently open
/// shows once, as open.
fn candidates(
    open: &[(String, PathBuf)],
    recent: &[PathBuf],
    here: Option<&Path>,
    query: &str,
) -> Vec<Entry> {
    let q = expand(query.trim());
    let mut out: Vec<Entry> = Vec::new();

    for (session, dir) in open {
        if matches(dir, &q) {
            out.push(Entry {
                dir: dir.clone(),
                session: Some(session.clone()),
                kind: Kind::Open,
                here: false,
            });
        }
    }
    for dir in recent {
        if matches(dir, &q) && !out.iter().any(|e| e.dir == *dir) {
            out.push(Entry { dir: dir.clone(), session: None, kind: Kind::Recent, here: false });
        }
    }
    for dir in browse(query) {
        if !out.iter().any(|e| e.dir == dir) {
            out.push(Entry { dir, session: None, kind: Kind::Disk, here: false });
        }
    }

    // The directory you are standing in goes into the list — it is the likeliest
    // answer and costs nothing to offer. If it is already there (open, or a recent)
    // it is *moved* rather than duplicated, so it keeps saying which of those it
    // is; being here is an extra fact about a row, not a fourth source.
    if let Some(dir) = here.filter(|d| matches(d, &q)) {
        let mut entry = match out.iter().position(|e| e.dir == dir) {
            Some(at) => out.remove(at),
            None => Entry { dir: dir.to_path_buf(), session: None, kind: Kind::Recent, here: false },
        };
        entry.here = true;
        out.insert(0, entry);
    }

    // **Two orders, because there are two questions.**
    //
    // A word is a name you are *recalling*, so the answer is grouped by what the
    // rows are: what is running, then what you have opened before.
    //
    // A path is a place you are *navigating*, so the list should read like a
    // directory listing: **shortest first**. Typing `~/docu` must offer
    // `~/Documents` above `~/Documents/Code/dreamvps/cloud`, because the short one
    // is the one Tab is for — completing to a descendant of somewhere you have not
    // arrived at yet is a step in the wrong direction. Observed the other way round
    // on 2026-09-06 and it makes the completion useless for descending.
    //
    // Length rather than component count: it is what "the short path" means, and a
    // deep-but-terse path is genuinely quicker to get to. The path breaks ties so
    // the order never depends on which source a row came from.
    if is_path(query) {
        out.sort_by(|a, b| {
            let (x, y) = (a.dir.as_os_str().len(), b.dir.as_os_str().len());
            x.cmp(&y).then_with(|| a.dir.cmp(&b.dir))
        });
    }
    out
}

/// Whether the query names a place on disk rather than a project.
///
/// One definition, used by both the browse listing and the ordering — they have to
/// agree, or the list would be sorted for navigating while containing nothing to
/// navigate to.
fn is_path(query: &str) -> bool {
    let q = query.trim();
    q.contains('/') || q.starts_with('~')
}

/// Case-insensitive substring, against the **whole path**.
///
/// Deliberately not a fuzzy subsequence match: `clo` finding `cloud` is what is
/// wanted, and a subsequence would also find `code/utilities/mulpex-old`. The path
/// rather than the basename so that `code/c` narrows the way you expect.
fn matches(dir: &Path, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    dir.to_string_lossy().to_lowercase().contains(&q.to_lowercase())
}

/// Directories on disk for a query that is a path.
///
/// **Only when the query contains a `/` or starts with `~`.** A bare word listing
/// the current directory's children would fill the list with noise on the way to
/// filtering the two lists that were actually asked for; a slash is an
/// unambiguous statement that you mean a place on disk.
fn browse(query: &str) -> Vec<PathBuf> {
    if !is_path(query) {
        return Vec::new();
    }
    let query = query.trim();
    let expanded = expand(query);
    // Split at the last `/`: everything before it is a real directory to read,
    // what follows is a prefix to filter its children by. A query ending in `/`
    // therefore lists that directory whole, which is what makes Tab-then-type
    // work as a descent.
    let (parent, prefix) = match expanded.rfind('/') {
        Some(at) => (&expanded[..=at], &expanded[at + 1..]),
        None => return Vec::new(),
    };
    let Ok(read) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    let mut out: Vec<PathBuf> = read
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            // Hidden directories stay hidden until you ask for one by typing the
            // dot — `~/code/` should not be half `.git` and `.cache`.
            if name.starts_with('.') && !prefix.starts_with('.') {
                return false;
            }
            name.contains(&prefix)
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out.truncate(MAX_DISK);
    out
}

/// Expand a leading `~`. Only leading: `~` elsewhere in a path is a literal
/// character and some directories really are called that.
fn expand(s: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return s.to_string();
    };
    let home = home.to_string_lossy().to_string();
    if s == "~" {
        return home;
    }
    match s.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => s.to_string(),
    }
}

/// The inverse, for display and for Tab: `$HOME` back to `~`, so a path fits the
/// row and reads as the thing you would have typed.
fn pretty(dir: &Path) -> String {
    let s = dir.to_string_lossy().to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return s;
    };
    let home = home.to_string_lossy().to_string();
    if s == home {
        return "~".into();
    }
    match s.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => s,
    }
}

// ---- drawing ---------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const REVERSE: &str = "\x1b[7m";

fn render(entries: &[Entry], cursor: usize, query: &str, note: &str, (w, h): (u16, u16)) -> String {
    let w = (w as usize).max(20);
    let h = h as usize;
    let mut s = String::from("\x1b[H\x1b[2J");
    let mut lines = 0usize;

    s.push_str(&format!("{BOLD}{}{RESET}\r\n", pad("open a project", w)));
    lines += 1;

    // The input line. The tail is kept when it overflows — you are typing at the
    // end, and a field that hides what you are typing is not a field. Through the
    // bidi conversion for the same reason the rename field is: a directory name
    // can be Hebrew, and this terminal does not reorder. See `bidi.rs`.
    let shown = crate::bidi::visual(&tail(query, w.saturating_sub(4)));
    s.push_str(&format!("{BOLD}> {RESET}{}\r\n", pad(&format!("{shown}▏"), w.saturating_sub(2))));
    s.push_str(&format!("{DIM}{}{RESET}\r\n", "─".repeat(w)));
    lines += 2;

    // Two lines are reserved at the bottom: the footer, and a blank above it.
    let room = h.saturating_sub(lines + 2);
    // Scroll the window of rows so the cursor is always inside it — the list is
    // arbitrarily long once the filesystem is in it.
    let first = if room == 0 { 0 } else { cursor.saturating_sub(room.saturating_sub(1)) };

    if entries.is_empty() {
        s.push_str(&format!("{DIM}{}{RESET}\r\n", pad("  nothing matches", w)));
        lines += 1;
    }
    for (i, e) in entries.iter().enumerate().skip(first).take(room) {
        let sel = i == cursor;
        let (mark, colour) = match e.kind {
            // Open is the only state that is *about* the running system, so it is
            // the only one with a colour. Recents and disk entries are both just
            // "a directory you might mean".
            Kind::Open => ("●", "\x1b[32m"),
            Kind::Recent => ("○", DIM),
            Kind::Disk => (" ", DIM),
        };
        let name = crate::bidi::visual(&name_of(&e.dir));
        let path = crate::bidi::visual(&pretty(&e.dir));
        // Name in a fixed column, path filling the rest: scanning down the names is
        // how you find a project, and the path is there to disambiguate two with
        // the same one.
        let namew = 18.min(w / 2);
        // Spelled out rather than given a glyph. This row is the one `mpx` used to
        // open without asking, so what it needs to say is *why it is first*, and a
        // symbol would only be another thing to learn.
        //
        // **The path is truncated to make room for it, not the other way round.**
        // Appending the label and letting the line be cut to width put it after an
        // arbitrarily long path, so on a deep directory the one word that explains
        // the row was the first thing to disappear (observed 2026-09-06). The path
        // can lose characters; the label cannot.
        let tag = if e.here { "  · here" } else { "" };
        let room = w
            .saturating_sub(1)
            .saturating_sub(3 + namew + tag.chars().count());
        let body = format!(
            "{} {} {}{}",
            mark,
            pad(&trunc(&name, namew), namew),
            trunc(&path, room),
            tag
        );

        s.push_str(if sel { "▸" } else { " " });
        if sel {
            s.push_str(REVERSE);
        } else {
            s.push_str(colour);
        }
        s.push_str(&pad(&body, w.saturating_sub(1)));
        s.push_str(RESET);
        s.push_str("\r\n");
        lines += 1;
    }

    while lines + 1 < h {
        s.push_str("\r\n");
        lines += 1;
    }
    let foot = if note.is_empty() {
        "⏎ open · ⇥ complete · ↑↓ move · esc cancel".to_string()
    } else {
        note.to_string()
    };
    s.push_str(&format!("{DIM}{}{RESET}", pad(&crate::bidi::visual(&foot), w)));
    s
}

/// What to call a project. The folder name, which is what the tab bar shows and
/// what everyone actually calls it.
fn name_of(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.to_string_lossy().to_string())
}

// ---- keys ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Act {
    None,
    Up,
    Down,
    Enter,
    Complete,
    Cancel,
    Typed,
}

/// Apply a burst of bytes to the query, and say what else it asked for.
///
/// One combined pass rather than a key parser plus a text field, because a path is
/// arbitrary UTF-8 and every printable byte belongs to it — there are no letter
/// shortcuts to compete with, which is exactly what makes this the simple half of
/// the sidebar's problem. `q` is a letter here, so **Esc is the only way out**.
fn input(query: &mut String, seq: &[u8]) -> Act {
    let chars: Vec<char> = String::from_utf8_lossy(seq).chars().collect();
    let mut act = Act::None;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Arrows. Held down, several arrive in one read; acting on each would
            // fly past the row you were aiming at, so the burst moves once.
            '\u{1b}' if chars.get(i + 1) == Some(&'[') => {
                match chars.get(i + 2) {
                    Some('A') => act = Act::Up,
                    Some('B') => act = Act::Down,
                    _ => {}
                }
                i += 3;
            }
            '\u{1b}' => return Act::Cancel,
            '\u{3}' => return Act::Cancel, // Ctrl-C
            '\r' | '\n' => return Act::Enter,
            '\t' => {
                act = Act::Complete;
                i += 1;
            }
            '\u{7f}' | '\u{8}' => {
                query.pop();
                act = Act::Typed;
                i += 1;
            }
            // Ctrl-U, because a path you have typed yourself into a corner is
            // faster to clear than to backspace.
            '\u{15}' => {
                query.clear();
                act = Act::Typed;
                i += 1;
            }
            c if c.is_control() => i += 1,
            c => {
                query.push(c);
                act = Act::Typed;
                i += 1;
            }
        }
    }
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(session: &str, dir: &str) -> (String, PathBuf) {
        (session.to_string(), PathBuf::from(dir))
    }

    /// Open first, then recents — switching is the common case, so what is already
    /// running is what the cursor starts on.
    #[test]
    fn open_projects_come_before_recents() {
        let open = vec![open("cloud", "/w/cloud")];
        let recent = vec![PathBuf::from("/w/other"), PathBuf::from("/w/cloud")];
        let got = candidates(&open, &recent, None, "");
        assert_eq!(got.len(), 2, "the open project must not appear twice: {got:?}");
        assert_eq!(got[0].kind, Kind::Open);
        assert_eq!(got[0].dir, PathBuf::from("/w/cloud"));
        assert_eq!(got[1].kind, Kind::Recent);
        assert_eq!(got[1].dir, PathBuf::from("/w/other"));
    }

    /// The directory you are standing in is **offered**, never opened. `mpx` used
    /// to create a project for it outright; now it is row one and nothing happens
    /// until Enter.
    #[test]
    fn the_current_directory_is_offered_first_and_only_offered() {
        let open = vec![open("cloud", "/w/cloud")];
        let recent = vec![PathBuf::from("/w/other")];
        let here = Path::new("/w/somewhere-new");

        let got = candidates(&open, &recent, Some(here), "");
        assert_eq!(got[0].dir, here, "it goes to the top");
        assert!(got[0].here);
        assert!(got[0].session.is_none(), "and it is not open — being listed is not opening");
        assert_eq!(got.len(), 3, "the other two are still there: {got:?}");
        assert!(!got[1].here && !got[2].here, "exactly one row is `here`");

        // Standing inside a project that is already open: moved to the top, still
        // marked open. Being here is an extra fact about a row, not a fourth source.
        let got = candidates(&open, &recent, Some(Path::new("/w/cloud")), "");
        assert_eq!(got.len(), 2, "not duplicated: {got:?}");
        assert_eq!(got[0].kind, Kind::Open);
        assert!(got[0].here);
        assert_eq!(got[0].session.as_deref(), Some("cloud"));

        // And it obeys the filter like everything else, or typing could never get
        // rid of it.
        let got = candidates(&open, &recent, Some(here), "other");
        assert!(got.iter().all(|e| !e.here), "filtered out: {got:?}");

        // The row says why it is first, in words — and keeps saying it however
        // long the path is, because the path is what gets truncated to make room.
        let deep = Path::new("/a/very/deep/nest/of/directories/that/will/not/fit/anywhere/near");
        for (dir, w) in [(here, 60u16), (deep, 44)] {
            let f = render(&candidates(&[], &[], Some(dir), ""), 0, "", "", (w, 10));
            assert!(f.contains("· here"), "width {w}:\n{f}");
            assert!(
                plain(&f).lines().all(|l| l.chars().count() <= w as usize),
                "width {w}:\n{f}"
            );
        }
    }

    /// Frames carry SGR sequences, which are not characters on screen — measuring
    /// a width without stripping them measures the wrong thing.
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            // CSI: ESC [ <params> <final byte in @..~>
            if chars.next() != Some('[') {
                continue;
            }
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        out
    }

    /// Typing narrows the list, case-insensitively, against the whole path.
    #[test]
    fn typing_filters_both_lists() {
        let open = vec![open("cloud", "/w/cloud")];
        let recent = vec![PathBuf::from("/w/central-one"), PathBuf::from("/other/cloudy")];
        assert_eq!(candidates(&open, &recent, None, "CLO").len(), 2, "case does not matter");
        assert_eq!(candidates(&open, &recent, None, "w/c").len(), 2, "a path fragment narrows");
        assert_eq!(candidates(&open, &recent, None, "central").len(), 1);
        assert!(candidates(&open, &recent, None, "zzz").is_empty());
    }

    /// Typing a path is navigating, so the list reads like a directory listing:
    /// shortest first, whatever source a row came from. `~/docu` offering
    /// `~/Documents/Code/dreamvps/cloud` above `~/Documents` makes ⇥ useless for
    /// descending — you cannot complete *towards* somewhere by jumping past it.
    #[test]
    fn a_path_query_puts_the_shortest_path_first() {
        let open = vec![open("cloud", "/Users/x/Documents/Code/dreamvps/cloud")];
        let recent = vec![
            PathBuf::from("/Users/x/Documents/Code/test2"),
            PathBuf::from("/Users/x/Documents"),
            PathBuf::from("/Users/x/Documents/Code/test"),
        ];

        let paths: Vec<String> = candidates(&open, &recent, None, "/Users/x/Docu")
            .iter()
            .map(|e| e.dir.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/Users/x/Documents",
                "/Users/x/Documents/Code/test",
                "/Users/x/Documents/Code/test2",
                "/Users/x/Documents/Code/dreamvps/cloud",
            ],
            "shortest first, even though cloud is the open one"
        );

        // A word is a name you are recalling, not a place you are navigating, so
        // the grouping the switcher depends on survives untouched.
        let got = candidates(&open, &recent, None, "test");
        assert_eq!(got[0].kind, Kind::Recent);
        let got = candidates(&open, &recent, None, "");
        assert_eq!(got[0].kind, Kind::Open, "open still leads a word query");

        // And `here` is pinned only when not navigating — mid-path it is just
        // another candidate, ordered by the same rule as the rest.
        let here = Path::new("/Users/x/Documents/Code/dreamvps/cloud");
        let got = candidates(&open, &recent, Some(here), "/Users/x/Docu");
        assert_eq!(got[0].dir, PathBuf::from("/Users/x/Documents"));
        assert!(got.last().unwrap().here, "still marked, just not pinned");
        let got = candidates(&open, &recent, Some(here), "");
        assert!(got[0].here, "pinned when recalling a name");
    }

    /// A bare word must not list the filesystem — the browse half is opt-in, and a
    /// `/` is the unambiguous way to ask for it.
    #[test]
    fn browsing_needs_a_slash() {
        let home = std::env::temp_dir().join(format!("mpx-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for name in ["alpha", "beta", ".hidden"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        std::fs::write(home.join("afile"), "x").unwrap();

        let base = home.to_string_lossy().to_string();
        assert!(browse("alpha").is_empty(), "a bare word browses nothing");

        let all = browse(&format!("{base}/"));
        let names: Vec<String> = all.iter().map(|p| name_of(p)).collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
        assert!(!names.contains(&".hidden".to_string()), "hidden stays hidden: {names:?}");
        assert!(!names.contains(&"afile".to_string()), "files are not projects");

        // A prefix filters the children, and asking for a dot reveals the hidden.
        assert_eq!(browse(&format!("{base}/al")).len(), 1);
        assert_eq!(browse(&format!("{base}/.")).len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `~` is expanded on the way in and restored on the way out, so what you
    /// typed, what is listed, and what Tab gives you back are the same text.
    #[test]
    fn home_survives_the_round_trip() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~/code/x"), format!("{home}/code/x"));
        assert_eq!(expand("~"), home);
        assert_eq!(expand("/abs/~/x"), "/abs/~/x", "only a LEADING tilde is a home");
        assert_eq!(pretty(Path::new(&format!("{home}/code/x"))), "~/code/x");
        assert_eq!(pretty(Path::new("/opt/x")), "/opt/x");
    }

    /// Every printable byte belongs to the path, so `q` is a letter and Esc is the
    /// only way out. Tab completes, it does not open.
    #[test]
    fn the_query_takes_text_and_esc_is_the_exit() {
        let mut q = String::new();
        assert_eq!(input(&mut q, b"~/co"), Act::Typed);
        assert_eq!(input(&mut q, b"de"), Act::Typed);
        assert_eq!(q, "~/code", "q, n, x and the rest are just letters here");
        assert_eq!(input(&mut q, b"q"), Act::Typed);
        assert_eq!(q, "~/codeq");
        assert_eq!(input(&mut q, b"\x7f"), Act::Typed);
        assert_eq!(q, "~/code");
        assert_eq!(input(&mut q, b"\t"), Act::Complete);
        assert_eq!(input(&mut q, b"\r"), Act::Enter);
        assert_eq!(input(&mut q, b"\x1b"), Act::Cancel);
        assert_eq!(input(&mut q, b"\x03"), Act::Cancel);
        assert_eq!(input(&mut q, b"\x15"), Act::Typed);
        assert_eq!(q, "", "Ctrl-U clears");
        // A burst of held arrows moves once, and does not become text.
        let mut q2 = String::new();
        assert_eq!(input(&mut q2, b"\x1b[B\x1b[B\x1b[B"), Act::Down);
        assert!(q2.is_empty(), "an arrow must not end up in the path");
        // Hebrew is a whole character, not bytes — a directory can be named in it.
        let mut q3 = String::new();
        input(&mut q3, "שלום".as_bytes());
        assert_eq!(q3, "שלום");
        input(&mut q3, b"\x7f");
        assert_eq!(q3, "שלו", "backspace must not leave half a character");
    }

    /// The frame must fill the popup exactly — one row short and the list crawls
    /// up the screen on every redraw.
    #[test]
    fn the_frame_fills_the_popup() {
        let entries = candidates(
            &[open("cloud", "/w/cloud")],
            &[PathBuf::from("/w/other")],
            None,
            "",
        );
        for h in [6u16, 12, 40] {
            let f = render(&entries, 0, "", "", (60, h));
            assert_eq!(f.split("\r\n").count(), h as usize, "height {h}");
        }
        // And with more rows than fit, the cursor stays visible.
        let many: Vec<Entry> = (0..40)
            .map(|i| Entry { dir: PathBuf::from(format!("/w/p{i}")), session: None, kind: Kind::Disk, here: false })
            .collect();
        let f = render(&many, 39, "", "", (60, 10));
        assert_eq!(f.split("\r\n").count(), 10);
        assert!(f.contains("/w/p39"), "the selected row must be on screen");
        assert!(!f.contains("/w/p0 "), "and the top of the list scrolled off");
    }

    /// The footer is the only place these keys are written down.
    #[test]
    fn the_keys_are_advertised() {
        let f = render(&[], 0, "", "", (60, 10));
        for key in ["⏎", "⇥", "↑↓", "esc"] {
            assert!(f.contains(key), "{key} missing");
        }
        assert!(f.contains("nothing matches"), "an empty list says so");
        // A note replaces the hints while something is happening.
        let f = render(&[], 0, "", "opening ~/x…", (60, 10));
        assert!(f.contains("opening ~/x…"));
    }
}
