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
    pub fn spawn(
        id: usize,
        dir: &Path,
        rows: u16,
        cols: u16,
        dirty: Arc<AtomicBool>,
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
        cmd.env("IS_SANDBOX", "1");
        cmd.cwd(dir);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;
        // `pair` (and the slave fd it still owns) drops here, so the reader gets
        // EOF when the child exits.

        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 0)));
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
