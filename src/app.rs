//! Application state and the main event loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::style::Color;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;

use crate::keymap::key_to_bytes;
use crate::persist::{self, SessionStore};
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

/// Lines scrolled per mouse-wheel notch.
const SCROLL_LINES: usize = 3;

/// How long the "✓ copied" confirmation stays in the center-pane title.
const COPIED_FLASH: Duration = Duration::from_secs(2);

/// Two left-clicks on the same cell within this window count as a double-click
/// (word select).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// An in-progress or completed text selection in the center pane, in
/// visible-screen cell coordinates `(row, col)` (0-based). `anchor` is where the
/// drag began; `cursor` is the current/last end.
///
/// `word_pivot` is set for a double-click (word) selection: it holds the
/// `(start, end)` cells of the originally double-clicked word, so dragging
/// extends the selection whole-word-at-a-time around that pivot.
struct Selection {
    anchor: (u16, u16),
    cursor: (u16, u16),
    dragging: bool,
    word_pivot: Option<((u16, u16), (u16, u16))>,
}

/// How often to re-read the per-instance hook state files. Most state changes
/// coincide with PTY output (which already triggers a redraw), but the idle
/// `Notification` can fire with no output, so we poll as a backstop.
const STATUS_POLL: Duration = Duration::from_millis(200);

/// What a Claude instance is doing, derived from its lifecycle hooks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Mid-turn: a prompt was submitted or a tool just ran.
    Working,
    /// Finished its turn (Stop), or freshly spawned and ready for a prompt.
    Waiting,
    /// Blocked on the user: a question, permission, or idle wait.
    NeedsInput,
}

impl Status {
    /// Parse the single word a hook writes into the state file.
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "working" => Some(Status::Working),
            "waiting" => Some(Status::Waiting),
            "needs" => Some(Status::NeedsInput),
            _ => None,
        }
    }

    /// `(dot color, short label)` for the sidebar / legend.
    pub fn indicator(self) -> (Color, &'static str) {
        match self {
            Status::Working => (Color::Yellow, "working"),
            Status::Waiting => (Color::Green, "ready"),
            Status::NeedsInput => (Color::LightRed, "needs you"),
        }
    }
}

/// Claude Code settings injected per session via `--settings`. The hooks write
/// a one-word state into `$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID` (both env vars
/// set by `TermSession::spawn`), which the sidebar polls. Content is fully
/// static — the instance id lives in the env, not the file — so every instance
/// shares this one file.
///
/// State machine: `UserPromptSubmit`/`PostToolUse` → working; `Stop` → waiting;
/// `PreToolUse[AskUserQuestion]` and the `permission_prompt`/`idle_prompt`
/// notifications → needs. `--dangerously-skip-permissions` suppresses the
/// permission UI but the events still let us flag idle/question waits; the
/// idle notification is the backstop if AskUserQuestion fires no hook.
const HOOK_SETTINGS_JSON: &str = r#"{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "printf working > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] }
    ],
    "PostToolUse": [
      { "hooks": [ { "type": "command", "command": "printf working > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] }
    ],
    "PreToolUse": [
      { "matcher": "AskUserQuestion", "hooks": [ { "type": "command", "command": "printf needs > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] }
    ],
    "Notification": [
      { "matcher": "permission_prompt|idle_prompt", "hooks": [ { "type": "command", "command": "printf needs > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "printf waiting > \"$MULPEX_STATE_DIR/$MULPEX_INSTANCE_ID\"" } ] }
    ]
  }
}
"#;

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
    /// The center pane's drawable rect (inside its border), in outer-terminal
    /// coordinates. Used to translate mouse events into pane-relative ones.
    center_inner: Rect,
    /// Active text selection in the center pane (drag-to-select), if any.
    selection: Option<Selection>,
    /// Time + position of the last left-click, for double-click detection.
    last_click: Option<(Instant, (u16, u16))>,
    /// A recent copy to flash in the title: when it happened + how many chars.
    copied_flash: Option<(Instant, usize)>,
    should_quit: bool,
    /// Per-run temp dir holding the hook settings file and one state file per
    /// instance. Removed on quit (see `Drop`).
    state_dir: PathBuf,
    /// Path to the `--settings` file inside `state_dir`.
    settings_path: PathBuf,
    /// Latest known status per instance id, refreshed from the state files.
    statuses: HashMap<usize, Status>,
    last_status_poll: Instant,
    /// Per-project store of session ids to restore on the next launch.
    store: SessionStore,
    /// Instance ids that have been "worked on" — either restored from a previous
    /// launch, or having fired at least one lifecycle hook this run (i.e. a
    /// prompt was submitted, so the session has real content). Only these are
    /// persisted; a freshly spawned, never-used instance is never remembered.
    worked: HashSet<usize>,
}

impl App {
    pub fn new(project_dir: PathBuf, area: Rect, keyboard_enhanced: bool) -> anyhow::Result<Self> {
        let dirty = Arc::new(AtomicBool::new(true));
        let (cols, rows) = ui::center_inner_size(area);
        let center_inner = ui::center_inner_rect(area);

        // Per-run scratch dir for the hook settings + per-instance state files.
        let state_dir = std::env::temp_dir().join(format!("mulpex-{}", std::process::id()));
        std::fs::create_dir_all(&state_dir)?;
        let settings_path = state_dir.join("settings.json");
        std::fs::write(&settings_path, HOOK_SETTINGS_JSON)?;

        // Restore the sessions worked on the last time Mulpex ran in this
        // project: relaunch each saved id with `--resume`. Any that fail to
        // resume (e.g. their transcript was cleaned up) simply don't come back;
        // they get pruned on the next persist.
        let store = SessionStore::new(&project_dir);
        let mut instances: Vec<TermSession> = Vec::new();
        let mut worked: HashSet<usize> = HashSet::new();
        for session_id in store.load() {
            let id = instances.len() + 1;
            if let Ok(session) = TermSession::spawn(
                id,
                &project_dir,
                rows,
                cols,
                Arc::clone(&dirty),
                &settings_path,
                &state_dir,
                &session_id,
                true,
            ) {
                worked.insert(id);
                instances.push(session);
            }
        }

        // No restorable sessions → start one fresh, with a brand-new session id.
        if instances.is_empty() {
            let session_id = persist::new_uuid();
            let first = TermSession::spawn(
                1,
                &project_dir,
                rows,
                cols,
                Arc::clone(&dirty),
                &settings_path,
                &state_dir,
                &session_id,
                false,
            )?;
            instances.push(first);
        }

        let next_id = instances.len() + 1;

        let project_name = project_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_dir.display().to_string());

        let app = Self {
            project_dir,
            project_name,
            instances,
            active: 0,
            focus: Focus::Center,
            keyboard_enhanced,
            quit_armed_at: None,
            dirty,
            next_id,
            center_cols: cols,
            center_rows: rows,
            center_inner,
            selection: None,
            last_click: None,
            copied_flash: None,
            should_quit: false,
            state_dir,
            settings_path,
            statuses: HashMap::new(),
            last_status_poll: Instant::now(),
            store,
            worked,
        };
        // Reconcile the store with what actually came back (prunes any sessions
        // that no longer resume).
        app.persist_sessions();
        Ok(app)
    }

    /// Write the set of worked-on instances' session ids to the per-project
    /// store, preserving sidebar order, so the next launch can restore them.
    fn persist_sessions(&self) {
        let ids: Vec<String> = self
            .instances
            .iter()
            .filter(|s| self.worked.contains(&s.id()))
            .map(|s| s.session_id().to_string())
            .collect();
        self.store.save(&ids);
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
                    Event::Key(key) => {
                        self.on_key(key);
                        redraw = true;
                    }
                    Event::Paste(text) => {
                        self.on_paste(&text);
                        redraw = true;
                    }
                    Event::Resize(w, h) => {
                        self.on_resize(Rect::new(0, 0, w, h));
                        redraw = true;
                    }
                    // The wheel scrolls our own scrollback view; only redraw
                    // when it actually moved (avoids a storm on bare moves).
                    Event::Mouse(me) => {
                        if self.on_mouse(me) {
                            redraw = true;
                        }
                    }
                    _ => {}
                }
            }

            // New output from any session → redraw.
            if self.dirty.swap(false, Ordering::Relaxed) {
                redraw = true;
            }

            // Refresh WORKING/WAITING/NEEDS indicators from the hook state files.
            if self.last_status_poll.elapsed() >= STATUS_POLL {
                if self.refresh_statuses() {
                    redraw = true;
                }
                self.last_status_poll = Instant::now();
            }

            // Clear the quit confirmation once its window lapses, so the banner
            // doesn't linger.
            if let Some(t) = self.quit_armed_at {
                if t.elapsed() >= QUIT_CONFIRM {
                    self.quit_armed_at = None;
                    redraw = true;
                }
            }

            // Clear the "✓ copied" flash once its window lapses.
            if let Some((t, _)) = self.copied_flash {
                if t.elapsed() >= COPIED_FLASH {
                    self.copied_flash = None;
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

    /// The last known status of instance `id`. A freshly spawned instance with
    /// no state file yet reads as `Waiting` (idle, ready for a prompt).
    pub fn status_of(&self, id: usize) -> Status {
        self.statuses.get(&id).copied().unwrap_or(Status::Waiting)
    }

    /// Re-read every live instance's hook state file. Returns whether any
    /// status changed (so the caller can request a redraw).
    fn refresh_statuses(&mut self) -> bool {
        let mut changed = false;
        let mut newly_worked = false;
        for session in &self.instances {
            let id = session.id();
            let file = std::fs::read_to_string(self.state_dir.join(id.to_string())).ok();
            // The presence of a state file means at least one lifecycle hook
            // fired — i.e. a prompt was submitted, so this is a real session
            // worth remembering.
            if file.is_some() && self.worked.insert(id) {
                newly_worked = true;
            }
            let status = file
                .as_deref()
                .and_then(|s| Status::from_word(s.trim()))
                .unwrap_or(Status::Waiting);
            if self.statuses.insert(id, status) != Some(status) {
                changed = true;
            }
        }
        if newly_worked {
            self.persist_sessions();
        }
        changed
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

    /// Handle a mouse event over the center pane. Two responsibilities:
    ///
    /// - **Wheel** scrolls Mulpex's own scrollback view of the focused Claude
    ///   (Claude renders inline and relies on terminal scrollback, so it doesn't
    ///   scroll itself).
    /// - **Left click-drag** selects text (tmux copy-mode style): we track the
    ///   drag over the vt100 grid, highlight it, and copy to the clipboard on
    ///   release — so plain drag works alongside the wheel, no modifier needed.
    ///
    /// Returns whether a redraw is needed. Coordinates are mapped to the pane's
    /// 0-based visible cells (clamped, so a drag past the edge selects to it).
    fn on_mouse(&mut self, me: MouseEvent) -> bool {
        let inner = self.center_inner;
        let inside = me.column >= inner.x
            && me.row >= inner.y
            && me.column < inner.x + inner.width
            && me.row < inner.y + inner.height;
        let cell = (
            me.row
                .saturating_sub(inner.y)
                .min(inner.height.saturating_sub(1)),
            me.column
                .saturating_sub(inner.x)
                .min(inner.width.saturating_sub(1)),
        );

        match me.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if !inside {
                    return false;
                }
                // Scrolling invalidates the selection (coords are view-relative).
                let cleared = self.selection.take().is_some();
                let scrolled = self.instances.get(self.active).is_some_and(|s| {
                    if matches!(me.kind, MouseEventKind::ScrollUp) {
                        s.scroll_up(SCROLL_LINES)
                    } else {
                        s.scroll_down(SCROLL_LINES)
                    }
                });
                cleared || scrolled
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if !inside {
                    // Clicking off the pane clears any existing selection.
                    self.last_click = None;
                    return self.selection.take().is_some();
                }
                let double = self
                    .last_click
                    .is_some_and(|(t, c)| t.elapsed() < DOUBLE_CLICK && c == cell);
                self.last_click = Some((Instant::now(), cell));

                self.selection = Some(if double {
                    // Word select: expand the clicked cell to word bounds and
                    // remember it as the pivot for word-by-word drag.
                    let (ws, we) = self
                        .instances
                        .get(self.active)
                        .map_or((cell.1, cell.1), |s| s.word_at(cell.0, cell.1));
                    let (start, end) = ((cell.0, ws), (cell.0, we));
                    Selection {
                        anchor: start,
                        cursor: end,
                        dragging: true,
                        word_pivot: Some((start, end)),
                    }
                } else {
                    Selection {
                        anchor: cell,
                        cursor: cell,
                        dragging: true,
                        word_pivot: None,
                    }
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(pivot) = self.selection.as_ref().and_then(|s| {
                    if s.dragging {
                        Some(s.word_pivot)
                    } else {
                        None
                    }
                }) else {
                    return false;
                };
                let (anchor, cursor) = if let Some((ps, pe)) = pivot {
                    // Word drag: union the pivot word with the word under cursor.
                    let (ws, we) = self
                        .instances
                        .get(self.active)
                        .map_or((cell.1, cell.1), |s| s.word_at(cell.0, cell.1));
                    (ps.min((cell.0, ws)), pe.max((cell.0, we)))
                } else {
                    (self.selection.as_ref().unwrap().anchor, cell)
                };
                let sel = self.selection.as_mut().unwrap();
                if sel.anchor == anchor && sel.cursor == cursor {
                    return false;
                }
                sel.anchor = anchor;
                sel.cursor = cursor;
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let finishing = self.selection.as_ref().is_some_and(|s| s.dragging);
                if !finishing {
                    return false;
                }
                if let Some(sel) = self.selection.as_mut() {
                    sel.dragging = false;
                }
                // A bare click (no drag, not a word select) selects nothing.
                let is_click = self
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.anchor == s.cursor && s.word_pivot.is_none());
                if is_click {
                    self.selection = None;
                } else if let Some(n) = self.copy_selection() {
                    self.copied_flash = Some((Instant::now(), n));
                }
                true
            }
            _ => false,
        }
    }

    /// Copy the current selection's text to the clipboard (`pbcopy`). Returns the
    /// number of chars copied, or `None` if there was nothing to copy.
    fn copy_selection(&self) -> Option<usize> {
        let sel = self.selection.as_ref()?;
        let (start, end) = if sel.anchor <= sel.cursor {
            (sel.anchor, sel.cursor)
        } else {
            (sel.cursor, sel.anchor)
        };
        let session = self.instances.get(self.active)?;
        let text = session.selection_text(start.0, start.1, end.0, end.1);
        if text.is_empty() {
            return None;
        }
        let n = text.chars().count();
        copy_to_clipboard(&text);
        Some(n)
    }

    /// The current selection as inclusive visible-cell coords `(sr, sc, er, ec)`
    /// in reading order, or `None` when there's nothing meaningful to highlight.
    /// A bare 1-cell click highlights nothing, but a 1-char word select does.
    pub fn selection_span(&self) -> Option<(u16, u16, u16, u16)> {
        let sel = self.selection.as_ref()?;
        if sel.anchor == sel.cursor && sel.word_pivot.is_none() {
            return None;
        }
        let (s, e) = if sel.anchor <= sel.cursor {
            (sel.anchor, sel.cursor)
        } else {
            (sel.cursor, sel.anchor)
        };
        Some((s.0, s.1, e.0, e.1))
    }

    /// Number of chars in a recent copy, while the "✓ copied" flash is showing.
    pub fn copied_flash(&self) -> Option<usize> {
        self.copied_flash
            .and_then(|(t, n)| (t.elapsed() < COPIED_FLASH).then_some(n))
    }

    fn send_to_active(&mut self, bytes: &[u8]) {
        // Any input clears the selection and snaps back to live output.
        self.selection = None;
        if let Some(session) = self.instances.get_mut(self.active) {
            if session.is_alive() {
                session.scroll_to_bottom();
                session.send(bytes);
            }
        }
    }

    fn on_resize(&mut self, area: Rect) {
        let (cols, rows) = ui::center_inner_size(area);
        self.center_cols = cols;
        self.center_rows = rows;
        self.center_inner = ui::center_inner_rect(area);
        for session in &mut self.instances {
            session.resize(rows, cols);
        }
    }

    /// Spawn a new Claude in the project dir and focus it.
    fn spawn_instance(&mut self) {
        let id = self.next_id;
        let session_id = persist::new_uuid();
        match TermSession::spawn(
            id,
            &self.project_dir,
            self.center_rows,
            self.center_cols,
            Arc::clone(&self.dirty),
            &self.settings_path,
            &self.state_dir,
            &session_id,
            false,
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

        // A closed instance is forgotten: drop its id from the worked set and
        // re-persist so it won't be restored next launch.
        self.worked.retain(|id| self.instances.iter().any(|s| s.id() == *id));
        self.persist_sessions();
        true
    }

    /// Center pane size `(cols, rows)`, for the info panel.
    pub fn center_size(&self) -> (u16, u16) {
        (self.center_cols, self.center_rows)
    }
}

/// Copy `text` to the macOS system clipboard via `pbcopy`. Best-effort: any
/// failure (e.g. `pbcopy` missing) is silently ignored.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            // `stdin` drops here, closing the pipe so `pbcopy` can finish.
        }
        let _ = child.wait();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Tear the sessions down first (each `TermSession::Drop` kills its
        // process group and waits) so no child can recreate a state file after
        // we remove the scratch dir.
        self.instances.clear();
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}
