//! An embedded Claude Code session running on its own pseudo-terminal.
//!
//! Claude Code is itself a full-screen TUI, so we cannot pass its output
//! straight through to our terminal. Instead we spawn `claude` on a PTY sized
//! to the center pane, parse its ANSI/VT output into a `vt100` screen buffer on
//! a background thread, and let the UI composite that buffer into the pane.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// How many lines of scrolled-off output to retain per instance, so the wheel
/// can scroll back through the conversation. Grows lazily up to this cap.
const SCROLLBACK_LEN: usize = 10_000;

/// A live `claude` process plus the virtual screen it is drawing to.
pub struct TermSession {
    id: usize,
    parser: Arc<RwLock<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
}

impl TermSession {
    /// Spawn `claude` in `dir` on a PTY of `rows`x`cols`.
    ///
    /// We mirror the user's shell wrapper (`IS_SANDBOX=1` +
    /// `--dangerously-skip-permissions`) because `portable-pty` execs the
    /// binary directly and bypasses the zsh function that normally adds them.
    ///
    /// `dirty` is flipped to `true` whenever new output arrives so the main
    /// loop knows to redraw. `id` is a stable display identifier.
    ///
    /// `settings_path` is a Mulpex-generated settings file injected via
    /// `--settings` that wires Claude Code lifecycle hooks (Stop /
    /// UserPromptSubmit / …) to write this instance's WORKING/WAITING/NEEDS
    /// state into `state_dir/<id>`. The hooks key the file off the
    /// `MULPEX_INSTANCE_ID` / `MULPEX_STATE_DIR` env vars set here, so one
    /// static settings file serves every instance.
    pub fn spawn(
        id: usize,
        dir: &Path,
        rows: u16,
        cols: u16,
        dirty: Arc<AtomicBool>,
        settings_path: &Path,
        state_dir: &Path,
    ) -> anyhow::Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("claude");
        cmd.arg("--dangerously-skip-permissions");
        cmd.arg("--settings");
        cmd.arg(settings_path);
        cmd.env("IS_SANDBOX", "1");
        cmd.env("MULPEX_INSTANCE_ID", id.to_string());
        cmd.env("MULPEX_STATE_DIR", state_dir);
        cmd.cwd(dir);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;
        // `pair` (and the slave fd it still owns) drops here, so the reader gets
        // EOF when the child exits.

        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, SCROLLBACK_LEN)));
        let alive = Arc::new(AtomicBool::new(true));

        {
            let parser = Arc::clone(&parser);
            let alive = Arc::clone(&alive);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.write() {
                                p.process(&buf[..n]);
                            }
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                }
                alive.store(false, Ordering::Relaxed);
                dirty.store(true, Ordering::Relaxed);
            });
        }

        Ok(Self {
            id,
            parser,
            writer,
            master,
            child,
            alive,
            rows,
            cols,
        })
    }

    /// Stable display identifier (e.g. shown as "claude #3").
    pub fn id(&self) -> usize {
        self.id
    }

    /// Shared handle to the virtual screen, for rendering.
    pub fn parser(&self) -> &Arc<RwLock<vt100::Parser>> {
        &self.parser
    }

    /// How many lines back from the live bottom the view is currently scrolled
    /// (0 = following live output).
    pub fn scrollback(&self) -> usize {
        self.parser
            .read()
            .map(|p| p.screen().scrollback())
            .unwrap_or(0)
    }

    /// Scroll the view towards older output by `lines` (clamped to history).
    /// Returns whether the position actually changed.
    pub fn scroll_up(&self, lines: usize) -> bool {
        if let Ok(mut p) = self.parser.write() {
            let cur = p.screen().scrollback();
            p.set_scrollback(cur + lines); // vt100 clamps to available history
            p.screen().scrollback() != cur
        } else {
            false
        }
    }

    /// Scroll the view towards newer output by `lines`. Returns whether the
    /// position actually changed.
    pub fn scroll_down(&self, lines: usize) -> bool {
        if let Ok(mut p) = self.parser.write() {
            let cur = p.screen().scrollback();
            let new = cur.saturating_sub(lines);
            p.set_scrollback(new);
            new != cur
        } else {
            false
        }
    }

    /// Snap back to live output (bottom). Called when the user sends input, so
    /// typing always jumps to the prompt like a normal terminal.
    pub fn scroll_to_bottom(&self) {
        if let Ok(mut p) = self.parser.write() {
            p.set_scrollback(0);
        }
    }

    /// Text of a selection between two inclusive visible cells (reading order),
    /// for the clipboard. Coordinates are 0-based and scrollback-aware; `end_col`
    /// is made exclusive for vt100's `contents_between`.
    pub fn selection_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        self.parser
            .read()
            .map(|p| {
                p.screen()
                    .contents_between(start_row, start_col, end_row, end_col.saturating_add(1))
            })
            .unwrap_or_default()
    }

    /// The inclusive `(start_col, end_col)` of the word at `(row, col)` for
    /// double-click selection. If that cell isn't a word char, just the cell
    /// itself. Scrollback-aware (reads visible cells).
    pub fn word_at(&self, row: u16, col: u16) -> (u16, u16) {
        let Ok(p) = self.parser.read() else {
            return (col, col);
        };
        let screen = p.screen();
        let (_, cols) = screen.size();
        let is_word = |c: u16| {
            screen
                .cell(row, c)
                .and_then(|cell| cell.contents().chars().next())
                .is_some_and(is_word_char)
        };
        if !is_word(col) {
            return (col, col);
        }
        let mut start = col;
        while start > 0 && is_word(start - 1) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < cols && is_word(end + 1) {
            end += 1;
        }
        (start, end)
    }

    /// Resize the virtual screen and the PTY so Claude re-lays-out to the pane.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        if let Ok(mut p) = self.parser.write() {
            p.set_size(rows, cols);
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Forward raw bytes to Claude's stdin.
    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Whether `c` counts as part of a "word" for double-click selection. Beyond
/// alphanumerics we include the punctuation common in identifiers, paths, and
/// URLs so double-clicking a path/URL grabs the whole thing.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '+' | '@' | ':')
}

impl Drop for TermSession {
    fn drop(&mut self) {
        // Don't leave an orphaned `claude` behind when Mulpex exits. The child
        // is a session/group leader (portable-pty calls setsid), and `claude`
        // (Node) spawns helper subprocesses in that group — so kill the whole
        // process group, not just the direct pid.
        if let Some(pid) = self.child.process_id() {
            let pgid = pid as libc::pid_t;
            unsafe {
                libc::killpg(pgid, libc::SIGHUP);
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
