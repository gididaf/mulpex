//! Plain-text recording of a shell terminal's output, for `hub_terminal_read`.
//!
//! A claude instance runs in a different process from Mulpex, so it cannot reach
//! the PTY bytes in memory — it reads a file. That file has to hold something a
//! model can actually use, which rules out both extremes:
//!
//! - Raw PTY bytes are unreadable (escape sequences, colours, cursor motion).
//! - A *line*-based stripper is worse than it sounds: `cargo`, `vite`, `docker
//!   compose` and every spinner repaint by moving the cursor and erasing, so a
//!   filter that only drops escape sequences emits **every intermediate frame**,
//!   stacked. A 30-second build becomes thousands of near-identical lines.
//!
//! So this is a small terminal emulator: a bounded `rows x cols` grid of chars
//! that the escape sequences actually act on. A row is written to the log only
//! when it **scrolls off the top** (or on a screen clear, which is what a real
//! terminal's scrollback does too) — i.e. once it can no longer change. What is
//! still on screen lives in a separate snapshot file, so a dev server whose
//! output never scrolls is still readable.
//!
//! Deliberately hand-rolled rather than reaching for the `vt100` crate: the
//! desktop rewrite dropped that dependency on purpose (xterm.js is the real
//! emulator), and what is needed here is a fraction of a conformant VT — no
//! attributes, no colours, no wide-char metrics.
//!
//! Full-screen programs (`vim`, `htop`, `less`) are **suppressed** rather than
//! recorded: they repaint a whole screen continuously, and letting them into a
//! bounded log evicts everything genuinely useful.
//!
//! ## The log file
//!
//! `terminals/<id>.log` is `mulpex_core::termlog`'s fixed-width header followed
//! by plain text — see that module for the layout, which the MCP helper parses
//! from the other side.
//!
//! Trimming rewrites **in place on the one writer handle** and writes the header
//! **last**. A rename-based trim would unlink the inode the writer's fd points
//! at, and every later byte would go to a deleted file — silently, forever. A
//! reader that sees a changed `base` across its read simply retries.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use mulpex_core::termlog::{self, Header};

/// Fixed width of the log header, in bytes. Data begins here.
const HEADER_LEN: u64 = termlog::HEADER_LEN as u64;

/// Trim once the data section passes this…
const MAX_LOG: u64 = 1024 * 1024;
/// …down to this, cut at a line boundary.
const KEEP_LOG: u64 = 512 * 1024;

/// Placeholder written in place of a full-screen program's output.
const ALT_SCREEN_NOTE: &str = "[full-screen program — output omitted]";

/// Rewrite the on-screen snapshot at most this often while output is flowing.
const SCREEN_THROTTLE_MS: u64 = 120;

/// Refresh the header (for `idle_ms`) at most this often.
const HEADER_THROTTLE_MS: u64 = 100;

/// Hard ceiling on the grid, whatever the PTY claims. Bounds memory and stops a
/// bogus resize from allocating wildly.
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 500;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The grid + escape-sequence parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PState {
    Ground,
    /// Saw ESC; the next byte selects what this is.
    Esc,
    /// Inside `ESC [` — collecting parameter/intermediate bytes.
    Csi,
    /// Inside `ESC ]` — an OSC string, ended by BEL or ST.
    Osc,
    /// Inside DCS/SOS/PM/APC — ended by ST.
    Str,
    /// A two-character escape (`ESC ( B`, `ESC # 8`, …): swallow one more byte.
    EscInter,
}

/// A bounded terminal screen. Feed it PTY bytes; rows that scroll off the top
/// accumulate in `out` as plain text lines.
pub struct Screen {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    /// Per row: "this row ran off the right margin and continues on the next
    /// one". Without it a line longer than `cols` becomes a real newline in the
    /// transcript, silently splitting whatever straddles the boundary — a URL, a
    /// token, an id, a JSON value — so the text read back is not the text that
    /// was printed, and grepping the log for it can never match. Set only by
    /// `print`'s auto-wrap, cleared whenever the row is re-written or erased,
    /// and moved in lockstep with `cells` by every scroll/insert/delete.
    wrapped: Vec<bool>,
    row: usize,
    col: usize,
    saved: (usize, usize),
    /// Scroll region, inclusive.
    top: usize,
    bot: usize,
    /// Set between `?1049h` and `?1049l` — a full-screen program is drawing.
    ///
    /// It suppresses *logging*, not emulation. The grid keeps tracking the
    /// program's output so `screen_text` (and therefore `hub_terminal_read`'s
    /// `current_screen`) shows what is actually on screen. This used to drop
    /// every byte, which made a full-screen program completely unreadable —
    /// fine for a stray `vim`, fatal once Claude Code itself started using the
    /// alternate screen in v2.1.226, because a remote claude then had no
    /// readable output at all.
    suppressed: bool,
    state: PState,
    /// CSI parameter + intermediate bytes collected so far.
    seq: String,
    /// `true` while an ESC has been seen inside an OSC/DCS string (looking for
    /// the `\` that completes ST).
    esc_in_str: bool,
    /// Incomplete UTF-8 sequence carried across a chunk boundary. The reader
    /// thread hands us arbitrary 8 KB slices, so a multi-byte char is routinely
    /// split; decoding each chunk independently would spray U+FFFD.
    carry: Vec<u8>,
    /// Lines that have scrolled off, ready for the log.
    pub out: String,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = (rows as usize).clamp(1, MAX_ROWS);
        let cols = (cols as usize).clamp(1, MAX_COLS);
        Self {
            rows,
            cols,
            cells: vec![vec![' '; cols]; rows],
            wrapped: vec![false; rows],
            row: 0,
            col: 0,
            saved: (0, 0),
            top: 0,
            bot: rows - 1,
            suppressed: false,
            state: PState::Ground,
            seq: String::new(),
            esc_in_str: false,
            carry: Vec::new(),
            out: String::new(),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = (rows as usize).clamp(1, MAX_ROWS);
        let cols = (cols as usize).clamp(1, MAX_COLS);
        if rows == self.rows && cols == self.cols {
            return;
        }
        // No reflow: keep the top-left content, which is what a terminal that
        // doesn't rewrap does, and is all this needs to stay legible.
        let mut cells = vec![vec![' '; cols]; rows];
        for (r, dst) in cells.iter_mut().enumerate().take(rows.min(self.rows)) {
            for (c, ch) in dst.iter_mut().enumerate().take(cols.min(self.cols)) {
                *ch = self.cells[r][c];
            }
        }
        // A wrap flag means "continues past column `cols`", so it is only
        // meaningful for the width it was recorded at; a width change retires
        // every flag rather than rejoining rows at the wrong place.
        let mut wrapped = vec![false; rows];
        if cols == self.cols {
            for (r, w) in wrapped.iter_mut().enumerate().take(rows.min(self.rows)) {
                *w = self.wrapped[r];
            }
        }
        self.cells = cells;
        self.wrapped = wrapped;
        self.rows = rows;
        self.cols = cols;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.top = 0;
        self.bot = rows - 1;
    }

    /// The part of the screen still visible, trailing blank rows removed.
    pub fn screen_text(&self) -> String {
        let mut lines = self.logical_lines(self.rows);
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Push everything still on screen into `out` (on exit, or a screen clear).
    pub fn flush_visible(&mut self) {
        // Same reason as `scroll_up`: what is on an alt screen is a live view,
        // not history, and must not be dumped into the log on exit or on a
        // clear.
        if self.suppressed {
            return;
        }
        let mut last = None;
        for r in 0..self.rows {
            if !self.row_text(r).is_empty() {
                last = Some(r);
            }
        }
        let Some(last) = last else { return };
        for line in self.logical_lines(last + 1) {
            self.out.push_str(&line);
            self.out.push('\n');
        }
    }

    /// Rows `0..upto` as the lines they logically are: a run of rows joined by
    /// auto-wrap comes back as the single line it was printed as.
    fn logical_lines(&self, upto: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut open = false;
        for r in 0..upto.min(self.rows) {
            if self.wrapped[r] {
                cur.push_str(&self.row_full(r));
                open = true;
            } else {
                cur.push_str(&self.row_text(r));
                lines.push(std::mem::take(&mut cur));
                open = false;
            }
        }
        if open {
            lines.push(cur);
        }
        lines
    }

    fn row_text(&self, r: usize) -> String {
        let s: String = self.cells[r].iter().collect();
        s.trim_end().to_string()
    }

    /// A wrapped row untrimmed: it is full by definition, so a trailing space is
    /// content in the middle of a longer line, not padding.
    fn row_full(&self, r: usize) -> String {
        self.cells[r].iter().collect()
    }

    // -- byte/char intake ---------------------------------------------------

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut idx = 0usize;
        while idx < buf.len() {
            match std::str::from_utf8(&buf[idx..]) {
                Ok(s) => {
                    for c in s.chars() {
                        self.on_char(c);
                    }
                    idx = buf.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // Safe by construction: `valid_up_to` is a char boundary.
                        if let Ok(s) = std::str::from_utf8(&buf[idx..idx + valid]) {
                            for c in s.chars() {
                                self.on_char(c);
                            }
                        }
                        idx += valid;
                    }
                    match e.error_len() {
                        // Genuinely invalid bytes: consume and mark.
                        Some(n) => {
                            self.on_char('\u{FFFD}');
                            idx += n;
                        }
                        // Truncated tail: carry it to the next chunk.
                        None => break,
                    }
                }
            }
        }
        if idx < buf.len() {
            let tail = &buf[idx..];
            // A well-formed prefix is at most 3 bytes; anything longer is junk we
            // should not accumulate forever.
            if tail.len() <= 3 {
                self.carry.extend_from_slice(tail);
            }
        }
    }

    fn on_char(&mut self, c: char) {
        match self.state {
            PState::Ground => self.ground(c),
            PState::Esc => self.after_esc(c),
            PState::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.state = PState::Ground;
                    let seq = std::mem::take(&mut self.seq);
                    self.csi(&seq, c);
                } else if c == '\u{1b}' {
                    // Aborted sequence, a new one starts.
                    self.seq.clear();
                    self.state = PState::Esc;
                } else if (' '..='\u{3f}').contains(&c) {
                    if self.seq.len() < 64 {
                        self.seq.push(c);
                    }
                } else {
                    // An embedded C0 control acts immediately, sequence continues.
                    self.ground(c);
                }
            }
            PState::Osc | PState::Str => {
                if self.esc_in_str {
                    self.esc_in_str = false;
                    // `ESC \` is ST; anything else was a stray ESC inside the string.
                    self.state = PState::Ground;
                    if c != '\\' {
                        self.on_char(c);
                    }
                } else if c == '\u{1b}' {
                    self.esc_in_str = true;
                } else if c == '\u{07}' && self.state == PState::Osc {
                    self.state = PState::Ground;
                }
                // Everything else is string payload: dropped. For OSC 8
                // hyperlinks that is exactly right — the visible label sits
                // *between* two OSC strings, in ground state, so it survives.
            }
            PState::EscInter => self.state = PState::Ground,
        }
    }

    fn ground(&mut self, c: char) {
        match c {
            '\u{1b}' => {
                self.seq.clear();
                self.state = PState::Esc;
            }
            '\r' => {
                // Returning to column 0 means this row is being written from its
                // start again, so any wrap it recorded is stale. Conservative on
                // purpose: a line that is still long simply re-wraps as it is
                // reprinted.
                self.wrapped[self.row] = false;
                self.col = 0;
            }
            '\n' => self.newline(),
            '\u{0b}' | '\u{0c}' => self.newline(),
            '\u{08}' => self.col = self.col.saturating_sub(1),
            '\t' => {
                let next = (self.col / 8 + 1) * 8;
                self.col = next.min(self.cols - 1);
            }
            '\u{07}' => {}
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => self.print(c),
        }
    }

    fn after_esc(&mut self, c: char) {
        self.state = PState::Ground;
        match c {
            '[' => {
                self.seq.clear();
                self.state = PState::Csi;
            }
            ']' => {
                self.esc_in_str = false;
                self.state = PState::Osc;
            }
            'P' | 'X' | '^' | '_' => {
                self.esc_in_str = false;
                self.state = PState::Str;
            }
            '(' | ')' | '*' | '+' | '%' | '#' | ' ' => self.state = PState::EscInter,
            '7' => self.saved = (self.row, self.col),
            '8' => {
                self.row = self.saved.0.min(self.rows - 1);
                self.col = self.saved.1.min(self.cols - 1);
            }
            'D' => self.newline(),
            'E' => {
                self.newline();
                self.col = 0;
            }
            'M' => self.reverse_index(),
            'c' => {
                self.flush_visible();
                self.clear_all();
                self.row = 0;
                self.col = 0;
            }
            _ => {}
        }
    }

    fn print(&mut self, c: char) {
        if self.col >= self.cols {
            // Auto-wrap: this row does not end here, it continues below.
            self.wrapped[self.row] = true;
            self.col = 0;
            self.line_feed();
        }
        self.cells[self.row][self.col] = c;
        self.col += 1;
    }

    /// An explicit line break (LF/VT/FF/IND/NEL): whatever the cursor's row used
    /// to be, it ends here.
    fn newline(&mut self) {
        self.wrapped[self.row] = false;
        self.line_feed();
    }

    /// Move down one line, scrolling at the bottom of the region. Shared by the
    /// explicit break and by auto-wrap, which must *not* clear the flag it just
    /// set.
    fn line_feed(&mut self) {
        if self.row == self.bot {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
    }

    /// Scroll the region up by `n`; the departing line is logged only when the
    /// region reaches the top of the screen — an inset region belongs to an app
    /// managing its own pane, and its churn is not history.
    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n.min(self.rows) {
            // On the alt screen the grid is still maintained — that is what
            // makes a full-screen program *readable* — but its churn never
            // reaches the log. A TUI repaints continuously and would evict the
            // whole retained history within seconds.
            if self.top == 0 && !self.suppressed {
                // A row that wraps is emitted without a terminator: its
                // continuation scrolls off later and lands on the same line,
                // which is how a line longer than the screen survives the log.
                if self.wrapped[self.top] {
                    let line = self.row_full(self.top);
                    self.out.push_str(&line);
                } else {
                    let line = self.row_text(self.top);
                    self.out.push_str(&line);
                    self.out.push('\n');
                }
            }
            self.cells.remove(self.top);
            self.cells.insert(self.bot, vec![' '; self.cols]);
            self.wrapped.remove(self.top);
            self.wrapped.insert(self.bot, false);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.rows) {
            self.cells.remove(self.bot);
            self.cells.insert(self.top, vec![' '; self.cols]);
            self.wrapped.remove(self.bot);
            self.wrapped.insert(self.top, false);
        }
    }

    fn clear_all(&mut self) {
        for r in self.cells.iter_mut() {
            for c in r.iter_mut() {
                *c = ' ';
            }
        }
        self.wrapped.iter_mut().for_each(|w| *w = false);
    }

    fn blank_row(&mut self, r: usize, from: usize, to: usize) {
        for c in from..=to.min(self.cols - 1) {
            self.cells[r][c] = ' ';
        }
        // Erasing through the right margin ends the row: whatever it used to
        // continue into is no longer part of it.
        if to >= self.cols - 1 {
            self.wrapped[r] = false;
        }
    }

    // -- CSI ----------------------------------------------------------------

    fn csi(&mut self, seq: &str, final_byte: char) {
        let private = seq.starts_with('?');
        let body = seq.trim_start_matches('?');
        let params: Vec<usize> = body
            .split(';')
            .map(|p| p.trim().parse::<usize>().unwrap_or(0))
            .collect();
        let p1 = *params.first().unwrap_or(&0);
        let n = p1.max(1);

        if private {
            // Alt screen. Handled even while suppressed — this is how we get out.
            if matches!(final_byte, 'h' | 'l') && params.iter().any(|p| matches!(p, 47 | 1047 | 1049))
            {
                if final_byte == 'h' {
                    if !self.suppressed {
                        self.flush_visible();
                        self.clear_all();
                        self.out.push_str(ALT_SCREEN_NOTE);
                        self.out.push('\n');
                    }
                    self.suppressed = true;
                } else {
                    self.suppressed = false;
                    self.clear_all();
                    self.row = 0;
                    self.col = 0;
                    self.top = 0;
                    self.bot = self.rows - 1;
                }
            }
            return;
        }
        match final_byte {
            'A' => self.row = self.row.saturating_sub(n).max(self.top.min(self.row)),
            'B' => self.row = (self.row + n).min(self.bot),
            'C' => self.col = (self.col + n).min(self.cols - 1),
            'D' => self.col = self.col.saturating_sub(n),
            'E' => {
                self.row = (self.row + n).min(self.bot);
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(n);
                self.col = 0;
            }
            'G' | '`' => self.col = (n - 1).min(self.cols - 1),
            'd' => self.row = (n - 1).min(self.rows - 1),
            'H' | 'f' => {
                let r = params.first().copied().unwrap_or(0).max(1) - 1;
                let c = params.get(1).copied().unwrap_or(0).max(1) - 1;
                self.row = r.min(self.rows - 1);
                self.col = c.min(self.cols - 1);
            }
            'J' => match p1 {
                0 => {
                    let (row, col) = (self.row, self.col);
                    self.blank_row(row, col, self.cols - 1);
                    for r in row + 1..self.rows {
                        self.blank_row(r, 0, self.cols - 1);
                    }
                }
                1 => {
                    let (row, col) = (self.row, self.col);
                    for r in 0..row {
                        self.blank_row(r, 0, self.cols - 1);
                    }
                    self.blank_row(row, 0, col);
                }
                // A full clear is the one case where on-screen content is
                // genuinely leaving: keep it, exactly as a real terminal's
                // scrollback would.
                _ => {
                    self.flush_visible();
                    self.clear_all();
                }
            },
            'K' => {
                let (row, col) = (self.row, self.col);
                match p1 {
                    1 => self.blank_row(row, 0, col),
                    2 => self.blank_row(row, 0, self.cols - 1),
                    _ => self.blank_row(row, col, self.cols - 1),
                }
            }
            'L' => {
                for _ in 0..n.min(self.rows) {
                    if self.row <= self.bot {
                        self.cells.remove(self.bot);
                        self.cells.insert(self.row, vec![' '; self.cols]);
                        self.wrapped.remove(self.bot);
                        self.wrapped.insert(self.row, false);
                    }
                }
            }
            'M' => {
                for _ in 0..n.min(self.rows) {
                    if self.row <= self.bot {
                        self.cells.remove(self.row);
                        self.cells.insert(self.bot, vec![' '; self.cols]);
                        self.wrapped.remove(self.row);
                        self.wrapped.insert(self.bot, false);
                    }
                }
            }
            '@' => {
                for _ in 0..n.min(self.cols) {
                    self.cells[self.row].pop();
                    self.cells[self.row].insert(self.col, ' ');
                }
            }
            'P' => {
                for _ in 0..n.min(self.cols) {
                    if self.col < self.cells[self.row].len() {
                        self.cells[self.row].remove(self.col);
                        self.cells[self.row].push(' ');
                    }
                }
            }
            'X' => {
                let (row, col) = (self.row, self.col);
                self.blank_row(row, col, col + n - 1);
            }
            'S' => self.scroll_up(n),
            'T' => self.scroll_down(n),
            'r' => {
                let t = params.first().copied().unwrap_or(0).max(1) - 1;
                let b = params
                    .get(1)
                    .copied()
                    .filter(|b| *b > 0)
                    .unwrap_or(self.rows)
                    - 1;
                if t < b && b < self.rows {
                    self.top = t;
                    self.bot = b;
                    self.row = t;
                    self.col = 0;
                }
            }
            's' => self.saved = (self.row, self.col),
            'u' => {
                self.row = self.saved.0.min(self.rows - 1);
                self.col = self.saved.1.min(self.cols - 1);
            }
            // SGR, device reports, mouse modes, window ops: no effect on text.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The on-disk recorder
// ---------------------------------------------------------------------------

/// Owns a terminal's log + screen-snapshot files. One instance per shell
/// session, driven from that session's PTY reader thread.
pub struct Recorder {
    screen: Screen,
    file: File,
    screen_path: PathBuf,
    /// Logical offset of the first byte currently in the data section.
    base: u64,
    /// Bytes currently in the data section.
    len: u64,
    exited: bool,
    last_out_ms: u64,
    header_written_ms: u64,
    screen_written_ms: u64,
    screen_dirty: bool,
}

impl Recorder {
    pub fn new(log_path: PathBuf, screen_path: PathBuf, rows: u16, cols: u16) -> io::Result<Self> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&log_path)?;
        let mut rec = Self {
            screen: Screen::new(rows, cols),
            file,
            screen_path,
            base: 0,
            len: 0,
            exited: false,
            last_out_ms: now_ms(),
            header_written_ms: 0,
            screen_written_ms: 0,
            screen_dirty: true,
        };
        rec.write_header()?;
        rec.write_screen();
        Ok(rec)
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        let out = std::mem::take(&mut self.screen.out);
        if !out.is_empty() {
            let _ = self.append(&out);
        }
        self.last_out_ms = now_ms();
        self.screen_dirty = true;
        if self.last_out_ms.saturating_sub(self.screen_written_ms) >= SCREEN_THROTTLE_MS {
            self.write_screen();
        }
        if self.last_out_ms.saturating_sub(self.header_written_ms) >= HEADER_THROTTLE_MS {
            let _ = self.write_header();
        }
    }

    /// Called on a timer so the final chunk of a burst isn't left unpublished
    /// for the length of the throttle window (a claude reading five seconds
    /// later must not miss the last prompt line).
    pub fn settle(&mut self) {
        if self.screen_dirty {
            self.write_screen();
            let _ = self.write_header();
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.resize(rows, cols);
        self.screen_dirty = true;
    }

    /// The shell exited: commit what's still on screen and mark the header.
    pub fn finish(&mut self) {
        self.screen.flush_visible();
        let out = std::mem::take(&mut self.screen.out);
        if !out.is_empty() {
            let _ = self.append(&out);
        }
        self.screen.clear_all();
        self.exited = true;
        self.screen_dirty = true;
        self.write_screen();
        let _ = self.write_header();
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(HEADER_LEN + self.len))?;
        self.file.write_all(text.as_bytes())?;
        self.len += text.len() as u64;
        if self.len > MAX_LOG {
            self.trim()?;
        }
        Ok(())
    }

    /// Drop the front of the log, in place on this same handle. A rename would
    /// orphan the fd and every later write would vanish into a deleted inode.
    fn trim(&mut self) -> io::Result<()> {
        let cut = self.len - KEEP_LOG;
        let mut buf = vec![0u8; KEEP_LOG as usize];
        self.file.seek(SeekFrom::Start(HEADER_LEN + cut))?;
        self.file.read_exact(&mut buf)?;
        // Start at a line boundary so the first line isn't a fragment.
        let adj = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let kept = &buf[adj..];
        self.file.seek(SeekFrom::Start(HEADER_LEN))?;
        self.file.write_all(kept)?;
        self.file.set_len(HEADER_LEN + kept.len() as u64)?;
        self.base += cut + adj as u64;
        self.len = kept.len() as u64;
        // Header LAST: a reader that saw the old base now sees a new one and
        // knows to retry, rather than trusting a stale offset into moved data.
        self.write_header()
    }

    fn write_header(&mut self) -> io::Result<()> {
        let header = termlog::format_header(&Header {
            base: self.base,
            last_out_ms: self.last_out_ms,
            exited: self.exited,
        });
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(header.as_bytes())?;
        self.file.flush()?;
        self.header_written_ms = now_ms();
        Ok(())
    }

    /// Publish the visible screen. Written via a temp file + rename so a reader
    /// only ever sees a whole snapshot; no offsets are involved, so unlike the
    /// log there's nothing for the rename to invalidate.
    fn write_screen(&mut self) {
        let text = self.screen.screen_text();
        let tmp = self.screen_path.with_extension("screen.tmp");
        if std::fs::write(&tmp, text.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.screen_path);
        }
        self.screen_written_ms = now_ms();
        self.screen_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn feed(s: &mut Screen, bytes: &str) {
        s.feed(bytes.as_bytes());
    }

    fn drain(s: &mut Screen) -> String {
        std::mem::take(&mut s.out)
    }

    #[test]
    fn plain_lines_scroll_into_the_log() {
        let mut s = Screen::new(3, 20);
        feed(&mut s, "one\r\ntwo\r\nthree\r\nfour\r\n");
        // Three rows: "one" scrolled off when "four" needed a line.
        assert_eq!(drain(&mut s), "one\ntwo\n");
        assert_eq!(s.screen_text(), "three\nfour");
    }

    #[test]
    fn cr_rewrites_the_line_instead_of_stacking_frames() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "10%\r20%\r100%\r\n");
        assert_eq!(drain(&mut s), "");
        // One line, final state — not three progress frames.
        assert_eq!(s.screen_text(), "100%");
    }

    #[test]
    fn cursor_up_plus_erase_line_repaints_in_place() {
        let mut s = Screen::new(5, 30);
        feed(&mut s, "building...\r\n");
        // Move back up over the line, erase it, write the final state.
        feed(&mut s, "\x1b[1A\x1b[2K\rdone in 1.2s\r\n");
        assert_eq!(drain(&mut s), "");
        assert_eq!(s.screen_text(), "done in 1.2s");
    }

    #[test]
    fn sgr_colours_are_dropped_but_text_kept() {
        let mut s = Screen::new(3, 40);
        feed(&mut s, "\x1b[31merror\x1b[0m: bad\r\n");
        assert_eq!(s.screen_text(), "error: bad");
    }

    #[test]
    fn osc_title_is_dropped() {
        let mut s = Screen::new(3, 40);
        // What zsh emits around every prompt.
        feed(&mut s, "\x1b]0;user@host: ~/proj\x07$ ls\r\n");
        assert_eq!(s.screen_text(), "$ ls");
    }

    #[test]
    fn osc_title_terminated_by_st_is_dropped() {
        let mut s = Screen::new(3, 40);
        feed(&mut s, "\x1b]2;title\x1b\\$ ok\r\n");
        assert_eq!(s.screen_text(), "$ ok");
    }

    #[test]
    fn osc8_hyperlink_keeps_its_visible_label() {
        let mut s = Screen::new(3, 60);
        feed(
            &mut s,
            "\x1b]8;;https://example.com/x\x07click here\x1b]8;;\x07 done\r\n",
        );
        assert_eq!(s.screen_text(), "click here done");
    }

    /// The case that broke the remote-claude feature in the field: Claude Code
    /// v2.1.226 draws on the alternate screen, where v2.1.223 rendered inline.
    /// A full-screen program's output must stay *readable on the screen* even
    /// though it never enters the log — otherwise a remote claude has no
    /// readable output at all and its driver is blind.
    #[test]
    fn a_full_screen_program_is_still_readable_on_screen() {
        let mut s = Screen::new(6, 40);
        feed(&mut s, "before\r\n");
        feed(&mut s, "\x1b[?1049h");
        feed(&mut s, "\x1b[H● Migrated the schema\r\n<<<MPX tok done all good>>>\r\n");

        let screen = s.screen_text();
        assert!(screen.contains("Migrated the schema"), "TUI unreadable: {screen:?}");
        assert!(
            screen.contains("<<<MPX tok done all good>>>"),
            "a signal on the alt screen must be findable: {screen:?}"
        );

        // …and none of it may reach the log, which a repainting TUI would
        // otherwise flood.
        let logged = drain(&mut s);
        assert!(logged.contains("before"), "{logged:?}");
        assert!(logged.contains(ALT_SCREEN_NOTE), "{logged:?}");
        assert!(!logged.contains("Migrated"), "the TUI leaked into the log: {logged:?}");
    }

    /// A repainting alt-screen program must not grow the log at all, however
    /// long it runs — the property that justifies suppressing it in the first
    /// place.
    #[test]
    fn a_repainting_alt_screen_never_grows_the_log() {
        let mut s = Screen::new(6, 40);
        feed(&mut s, "\x1b[?1049h");
        let before = s.out.len();
        for i in 0..500 {
            feed(&mut s, &format!("\x1b[H\x1b[2Jframe {i}\r\n"));
        }
        assert_eq!(s.out.len(), before, "the alt screen grew the log");
        assert!(s.screen_text().contains("frame 499"), "latest frame not on screen");
    }

    #[test]
    fn full_screen_programs_are_suppressed() {
        let mut s = Screen::new(4, 30);
        feed(&mut s, "before\r\n");
        feed(&mut s, "\x1b[?1049h");
        feed(&mut s, "\x1b[HTOP  1 2 3\r\nlots of junk\r\n");
        feed(&mut s, "\x1b[?1049l");
        feed(&mut s, "after\r\n");
        let logged = drain(&mut s);
        assert!(logged.contains("before"), "{logged:?}");
        assert!(logged.contains(ALT_SCREEN_NOTE), "{logged:?}");
        assert!(!logged.contains("junk"), "{logged:?}");
        assert_eq!(s.screen_text(), "after");
    }

    #[test]
    fn clear_screen_preserves_history() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "kept\r\n");
        feed(&mut s, "\x1b[2J\x1b[H");
        feed(&mut s, "fresh\r\n");
        assert_eq!(drain(&mut s), "kept\n");
        assert_eq!(s.screen_text(), "fresh");
    }

    #[test]
    fn utf8_split_across_chunks_is_not_mangled() {
        let mut s = Screen::new(2, 20);
        let text = "שלום ✓".as_bytes();
        // Feed one byte at a time — the worst possible chunking.
        for b in text {
            s.feed(&[*b]);
        }
        assert_eq!(s.screen_text(), "שלום ✓");
    }

    #[test]
    fn escape_sequence_split_across_chunks_does_not_leak() {
        let mut s = Screen::new(3, 20);
        s.feed(b"a\x1b");
        s.feed(b"[31");
        s.feed(b"mb");
        assert_eq!(s.screen_text(), "ab");
    }

    #[test]
    fn tabs_expand_and_backspace_moves_back() {
        let mut s = Screen::new(2, 40);
        feed(&mut s, "ab\tc");
        assert_eq!(s.screen_text(), "ab      c");
        feed(&mut s, "\x08X");
        assert_eq!(s.screen_text(), "ab      X");
    }

    /// A line longer than the screen is ONE line, not one per screen row: the
    /// wrap is the terminal's, not the text's. Splitting it corrupts whatever
    /// straddles the margin, silently.
    #[test]
    fn wrapping_at_the_right_margin_scrolls() {
        let mut s = Screen::new(2, 4);
        feed(&mut s, "abcdefgh");
        // Exactly fills the screen: nothing has left it yet.
        assert_eq!(drain(&mut s), "");
        assert_eq!(s.screen_text(), "abcdefgh");
        feed(&mut s, "ijkl");
        // Row 0 has scrolled off mid-line, so it is emitted unterminated — its
        // continuation joins it when that row leaves too.
        assert_eq!(drain(&mut s), "abcd");
        assert_eq!(s.screen_text(), "efghijkl");
    }

    /// The whole point, end to end: a token split across the margin comes back
    /// out of the *log* intact, so grepping for it can match.
    #[test]
    fn a_wrapped_line_is_rejoined_in_the_log() {
        let mut s = Screen::new(3, 20);
        let long = "https://example.com/a/very/long/path?token=abcdef123456";
        feed(&mut s, &format!("{long}\r\nnext\r\n"));
        s.flush_visible();
        let logged = drain(&mut s);
        assert!(
            logged.lines().any(|l| l == long),
            "wrapped line was not rejoined: {logged:?}"
        );
    }

    /// A wrapped row is full, so its trailing spaces are content in the middle
    /// of the line and must not be trimmed away at the seam.
    #[test]
    fn spaces_at_the_wrap_seam_survive() {
        let mut s = Screen::new(2, 4);
        feed(&mut s, "ab  cd");
        assert_eq!(s.screen_text(), "ab  cd");
    }

    /// An explicit newline is a real line break even when the row happens to be
    /// exactly full — only an auto-wrap joins.
    #[test]
    fn an_exactly_full_row_ending_in_a_newline_is_not_joined() {
        let mut s = Screen::new(3, 4);
        feed(&mut s, "abcd\r\nefgh\r\n");
        assert_eq!(s.screen_text(), "abcd\nefgh");
    }

    /// Repainting a wrapped row (the CUU + EL shape a progress bar uses) retires
    /// its wrap: the row no longer continues anywhere.
    #[test]
    fn a_repainted_row_stops_being_wrapped() {
        let mut s = Screen::new(3, 4);
        feed(&mut s, "abcdefgh");
        assert_eq!(s.screen_text(), "abcdefgh");
        // Back to the start of the wrapped row, erase it, write something short.
        feed(&mut s, "\x1b[2A\r\x1b[2Kxy");
        assert_eq!(s.screen_text(), "xy\nefgh");
    }

    #[test]
    fn insert_and_delete_lines_shift_within_the_screen() {
        let mut s = Screen::new(4, 10);
        feed(&mut s, "a\r\nb\r\nc\r\n");
        // Cursor is on row 3; go to row 1 and delete it.
        feed(&mut s, "\x1b[2;1H\x1b[M");
        assert_eq!(s.screen_text(), "a\nc");
    }

    #[test]
    fn resize_keeps_content_and_clamps_cursor() {
        let mut s = Screen::new(4, 20);
        feed(&mut s, "hello\r\nworld");
        s.resize(2, 10);
        assert_eq!(s.screen_text(), "hello\nworld");
        feed(&mut s, "!");
        assert_eq!(s.screen_text(), "hello\nworld!");
    }

    #[test]
    fn flush_visible_commits_the_tail_on_exit() {
        let mut s = Screen::new(5, 20);
        feed(&mut s, "last line");
        assert_eq!(drain(&mut s), "");
        s.flush_visible();
        assert_eq!(drain(&mut s), "last line\n");
    }

    // -- recorder / log file ------------------------------------------------

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mulpex-vtgrid-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_log(path: &Path) -> (u64, bool, String) {
        let raw = std::fs::read(path).unwrap();
        // Parsed the same way the MCP helper parses it, so this exercises the
        // shared format rather than a test-local copy of it.
        let h = termlog::parse_header(&raw).expect("valid header");
        let data = String::from_utf8_lossy(&raw[HEADER_LEN as usize..]).to_string();
        (h.base, h.exited, data)
    }

    #[test]
    fn recorder_writes_header_log_and_screen() {
        let dir = tmpdir("basic");
        let log = dir.join("1.log");
        let screen = dir.join("1.screen");
        let mut rec = Recorder::new(log.clone(), screen.clone(), 3, 20).unwrap();
        rec.push(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        rec.settle();

        let (base, exited, data) = read_log(&log);
        assert_eq!(base, 0);
        assert!(!exited);
        assert_eq!(data, "one\ntwo\n");
        assert_eq!(std::fs::read_to_string(&screen).unwrap(), "three\nfour");

        rec.finish();
        let (_, exited, data) = read_log(&log);
        assert!(exited);
        assert_eq!(data, "one\ntwo\nthree\nfour\n");
    }

    #[test]
    fn trim_advances_base_and_keeps_writing_to_the_same_file() {
        let dir = tmpdir("trim");
        let log = dir.join("2.log");
        let mut rec = Recorder::new(log.clone(), dir.join("2.screen"), 2, 80).unwrap();

        // ~2 MB of numbered lines, well past MAX_LOG.
        let line = |i: usize| format!("line {i:060}");
        for i in 0..30_000usize {
            rec.push(format!("{}\r\n", line(i)).as_bytes());
        }
        let (base, _, data) = read_log(&log);
        assert!(base > 0, "expected a trim to have happened");
        assert!(
            (data.len() as u64) <= MAX_LOG,
            "log not trimmed: {}",
            data.len()
        );
        // A rename-based trim would have orphaned the writer's fd here, and
        // everything after the first trim would have vanished silently.
        // The last line is still on screen (2 rows), so 29_998 is the newest
        // line to have scrolled off.
        assert!(
            data.contains(&line(29_998)),
            "post-trim writes were lost: log ends {:?}",
            &data[data.len().saturating_sub(80)..]
        );
        assert!(
            data.starts_with("line "),
            "trim did not cut at a line boundary"
        );
        // The oldest lines are the ones that went.
        assert!(!data.contains(&line(0)));
        // base + what's in the file == every byte ever written (66 per line).
        assert_eq!(base + data.len() as u64, 29_999 * 66);
    }
}

/// Replays of PTY bytes captured from real programs, not hand-written escape
/// sequences. Both fixtures were recorded through a real pty at 32x120 with
/// `TERM=xterm-256color` — the same shape a Mulpex terminal gives its child.
///
/// `cargo` is the case that justifies emulating a grid at all: its progress bar
/// repaints one line 17 times with CR and `EL`, and a stripper that merely drops
/// escape sequences would emit all 17 frames as separate lines.
#[cfg(test)]
mod replays {
    use super::*;

    fn replay(raw: &[u8]) -> String {
        let mut s = Screen::new(32, 120);
        // Chunked the way the PTY reader thread delivers it.
        for chunk in raw.chunks(8192) {
            s.feed(chunk);
        }
        s.flush_visible();
        s.out
    }

    #[test]
    fn a_real_cargo_build_reads_like_the_terminal_showed_it() {
        let log = replay(include_bytes!("../tests/fixtures/cargo-build.bin"));

        assert!(!log.contains('\u{1b}'), "escape sequences leaked into the log");
        assert!(!log.contains('\r'), "carriage returns leaked into the log");

        // Every meaningful line survives…
        for expect in [
            "   Compiling serde_core v1.0.229",
            "   Compiling mulpex-core v0.4.6",
            "    Finished `dev` profile",
        ] {
            assert!(log.contains(expect), "missing {expect:?} in:\n{log}");
        }
        // …and not one of the 17 progress-bar repaints does.
        assert!(
            !log.contains("Building ["),
            "progress-bar frames were recorded as history:\n{log}"
        );
        assert_eq!(
            log.lines().count(),
            8,
            "expected the 7 Compiling lines + Finished, got:\n{log}"
        );
    }

    #[test]
    fn a_real_vite_build_survives_intact() {
        let log = replay(include_bytes!("../tests/fixtures/vite-build.bin"));
        assert!(!log.contains('\u{1b}'));
        assert!(log.contains("vite v6.4.3 building for production..."));
        assert!(log.contains("modules transformed."));
        assert!(log.contains("built in"));
        // Column-aligned output has to stay aligned to stay readable.
        assert!(log.contains("dist/index.html                   0.39 kB"), "{log}");
    }
}

/// Replays of a real remote `claude` reached over ssh, recorded through a real
/// pty at 32x120 against a live machine.
///
/// These pin the two facts the remote-peer feature is built on, both of which
/// were found by measurement and neither of which is safe to assume:
///
/// 1. A remote claude does **not** take the alternate screen. If it did, the
///    recorder would replace the entire session with `[full-screen program —
///    output omitted]` and there would be no transcript to find a signal in —
///    the feature would be impossible rather than merely broken.
/// 2. Claude Code renders its output as **markdown**, so `__x__` is bold and its
///    underscores are eaten before the bytes ever reach the terminal. That is
///    why the signal delimiters are angle-bracket runs.
#[cfg(test)]
mod remote_claude_replays {
    use super::*;

    fn replay(raw: &[u8]) -> (String, String) {
        let mut s = Screen::new(32, 120);
        for chunk in raw.chunks(8192) {
            s.feed(chunk);
        }
        let screen = s.screen_text();
        s.flush_visible();
        (s.out.clone(), screen)
    }

    /// NOTE: this fixture is Claude Code **v2.1.223**, which rendered inline.
    /// v2.1.226 moved to the alternate screen, so the absence of `?1049h` here
    /// is a fact about this recording, NOT a property of remote claudes — it
    /// was briefly relied on as the latter, and the feature shipped blind
    /// against a newer remote because of it. The alt-screen path is covered by
    /// `a_full_screen_program_is_still_readable_on_screen`.
    #[test]
    fn a_remote_claude_over_ssh_is_legible_to_the_recorder() {
        let raw = include_bytes!("../tests/fixtures/remote-claude-ssh.bin");
        let (log, screen) = replay(raw);
        let all = format!("{log}\n{screen}");
        assert!(!log.contains('\u{1b}'), "escape sequences leaked into the log");
        // Its actual work survives as readable text, spaces and all — the naive
        // "strip the escapes" approach welds these words together, because the
        // TUI positions each one with a cursor jump rather than a space.
        assert!(
            all.contains("Created /tmp/mpx-probe/hello.txt containing banana"),
            "the remote's reply is not legible in the transcript"
        );
    }

    /// The measurement that chose the marker syntax. `__MPX_TO_LOCAL__` was
    /// asked for; `MPX_TO_LOCAL` is what arrived.
    #[test]
    fn markdown_eats_underscore_delimiters_but_not_angle_brackets() {
        let (log, screen) = replay(include_bytes!("../tests/fixtures/remote-claude-ssh.bin"));
        let all = format!("{log}\n{screen}");
        assert!(
            !all.contains("__MPX_TO_LOCAL__"),
            "the underscore-delimited marker survived — this fixture no longer shows the bug"
        );
        assert!(
            all.contains("MPX_TO_LOCAL done"),
            "the remote did not emit the marker at all"
        );

        let (log, screen) = replay(include_bytes!("../tests/fixtures/remote-claude-markers.bin"));
        let all = format!("{log}\n{screen}");
        for survivor in ["<<<MPX done alpha>>>", "[[MPX done charlie]]", "MPX-SIGNAL done delta"] {
            assert!(all.contains(survivor), "delimiter did not survive: {survivor}");
        }
    }
}
