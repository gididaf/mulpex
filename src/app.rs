//! Application state and the main event loop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;

use crate::keymap::key_to_bytes;
use crate::term_session::TermSession;
use crate::ui;

/// Which pane currently has focus. Only the center pane is interactive in
/// milestone 1; `Left`/`Right` exist for the upcoming pane-switching milestone.
#[allow(dead_code)]
pub enum Focus {
    Left,
    Center,
    Right,
}

/// Ctrl+Q must be pressed twice within this window to quit.
const QUIT_CONFIRM: Duration = Duration::from_secs(3);

pub struct App {
    pub project_dir: PathBuf,
    pub project_name: String,
    /// Running Claude sessions for this project. Exited ones are reaped, so
    /// every entry here is live.
    pub instances: Vec<TermSession>,
    /// Index into `instances` of the focused session (only valid when non-empty).
    pub active: usize,
    pub focus: Focus,
    /// Whether the Kitty keyboard protocol is active (decides if Ctrl+[ can be
    /// distinguished from Esc). Shown in the info pane.
    pub keyboard_enhanced: bool,
    /// Set on the first Ctrl+Q; a second Ctrl+Q within `QUIT_CONFIRM` quits.
    quit_armed_at: Option<Instant>,
    dirty: Arc<AtomicBool>,
    next_id: usize,
    center_cols: u16,
    center_rows: u16,
    should_quit: bool,
}

impl App {
    pub fn new(project_dir: PathBuf, area: Rect, keyboard_enhanced: bool) -> anyhow::Result<Self> {
        let dirty = Arc::new(AtomicBool::new(true));
        let (cols, rows) = ui::center_inner_size(area);

        let first = TermSession::spawn(1, &project_dir, rows, cols, Arc::clone(&dirty))?;

        let project_name = project_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_dir.display().to_string());

        Ok(Self {
            project_dir,
            project_name,
            instances: vec![first],
            active: 0,
            focus: Focus::Center,
            keyboard_enhanced,
            quit_armed_at: None,
            dirty,
            next_id: 2,
            center_cols: cols,
            center_rows: rows,
            should_quit: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
        let mut redraw = true;
        loop {
            // Remove any sessions whose `claude` has exited before drawing.
            if self.reap_dead() {
                redraw = true;
            }

            if redraw {
                terminal.draw(|f| ui::draw(f, self))?;
                redraw = false;
            }

            if event::poll(Duration::from_millis(15))? {
                match event::read()? {
                    Event::Key(key) => self.on_key(key),
                    Event::Paste(text) => self.on_paste(&text),
                    Event::Resize(w, h) => self.on_resize(Rect::new(0, 0, w, h)),
                    _ => {}
                }
                redraw = true;
            }

            // New output from any session → redraw.
            if self.dirty.swap(false, Ordering::Relaxed) {
                redraw = true;
            }

            // Clear the quit confirmation once its window lapses, so the banner
            // doesn't linger.
            if let Some(t) = self.quit_armed_at {
                if t.elapsed() >= QUIT_CONFIRM {
                    self.quit_armed_at = None;
                    redraw = true;
                }
            }

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// The currently focused session, if any.
    pub fn active_session(&self) -> Option<&TermSession> {
        self.instances.get(self.active)
    }

    /// Handle Ctrl+Q: arm on the first press, quit on a second press within the
    /// confirmation window.
    fn on_quit_key(&mut self) {
        match self.quit_armed_at {
            Some(t) if t.elapsed() < QUIT_CONFIRM => self.should_quit = true,
            _ => self.quit_armed_at = Some(Instant::now()),
        }
    }

    /// Whether a quit confirmation is currently pending (drives the banner).
    pub fn quit_armed(&self) -> bool {
        self.quit_armed_at
            .is_some_and(|t| t.elapsed() < QUIT_CONFIRM)
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Mulpex's reserved combos. Everything else (incl. Ctrl+C and Claude's
        // own Ctrl-shortcuts) forwards to the focused Claude.
        let m = key.modifiers;
        if m.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => {
                    self.on_quit_key();
                    return;
                }
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    self.spawn_instance();
                    return;
                }
                // Ctrl+]  → next. With the Kitty protocol it arrives as ']';
                // a legacy terminal maps Ctrl+] to Char('5'), so accept both.
                KeyCode::Char(']') | KeyCode::Char('5') => {
                    self.focus_next();
                    return;
                }
                // Ctrl+[ → previous, but ONLY when the terminal disambiguates it
                // from Esc (Kitty protocol). In a legacy terminal Ctrl+[ IS Esc
                // (KeyCode::Esc), which we deliberately do not match here so Esc
                // keeps flowing to Claude untouched.
                KeyCode::Char('[') => {
                    self.focus_prev();
                    return;
                }
                _ => {}
            }
        }

        if let Some(bytes) = key_to_bytes(&key) {
            self.send_to_active(&bytes);
        }
    }

    fn on_paste(&mut self, text: &str) {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        self.send_to_active(&out);
    }

    fn send_to_active(&mut self, bytes: &[u8]) {
        if let Some(session) = self.instances.get_mut(self.active) {
            if session.is_alive() {
                session.send(bytes);
            }
        }
    }

    fn on_resize(&mut self, area: Rect) {
        let (cols, rows) = ui::center_inner_size(area);
        self.center_cols = cols;
        self.center_rows = rows;
        for session in &mut self.instances {
            session.resize(rows, cols);
        }
    }

    /// Spawn a new Claude in the project dir and focus it.
    fn spawn_instance(&mut self) {
        let id = self.next_id;
        match TermSession::spawn(
            id,
            &self.project_dir,
            self.center_rows,
            self.center_cols,
            Arc::clone(&self.dirty),
        ) {
            Ok(session) => {
                self.next_id += 1;
                self.instances.push(session);
                self.active = self.instances.len() - 1;
            }
            // If spawning fails we simply don't add an instance; the next
            // redraw still happens via the key event.
            Err(_) => {}
        }
    }

    fn focus_next(&mut self) {
        if !self.instances.is_empty() {
            self.active = (self.active + 1) % self.instances.len();
        }
    }

    fn focus_prev(&mut self) {
        let len = self.instances.len();
        if len > 0 {
            self.active = (self.active + len - 1) % len;
        }
    }

    /// Drop sessions whose `claude` has exited. Returns whether anything was
    /// removed. When the focused session dies, focus moves to a neighbour; when
    /// the last one dies, the center pane shows the empty-state hint.
    fn reap_dead(&mut self) -> bool {
        if self.instances.is_empty() || self.instances.iter().all(|s| s.is_alive()) {
            return false;
        }

        let old_active = self.active;
        let mut kept: Vec<TermSession> = Vec::with_capacity(self.instances.len());
        let mut new_active: Option<usize> = None;

        for (idx, session) in std::mem::take(&mut self.instances).into_iter().enumerate() {
            if session.is_alive() {
                if idx == old_active {
                    new_active = Some(kept.len());
                }
                kept.push(session);
            }
            // Dead sessions drop here, which kills their process group.
        }

        self.instances = kept;
        self.active = match new_active {
            Some(a) => a,
            None if self.instances.is_empty() => 0,
            // The focused session died; focus the nearest surviving neighbour.
            None => old_active.min(self.instances.len() - 1),
        };
        true
    }

    /// Center pane size `(cols, rows)`, for the info panel.
    pub fn center_size(&self) -> (u16, u16) {
        (self.center_cols, self.center_rows)
    }
}
