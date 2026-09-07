//! The instance list, as a live pane down the left of every window.
//!
//! This is Mulpex's sidebar, and it exists because a window bar is not one: the
//! desktop app's sidebar shows each instance's *status*, its name, and how much
//! mail is waiting — three things at once, per row, updated continuously. tmux's
//! own window list can show one short label per window and nothing else.
//!
//! **tmux draws the layout; this only draws the list.** Every instance window is
//! `[ sidebar | claude ]`, so the sidebar is an ordinary tmux pane running an
//! ordinary program. That is the whole reason this file is ~400 lines instead of
//! the ~1,700 a self-drawn UI would need: nothing here paints `claude`'s output,
//! handles a resize, or encodes a key for it. tmux does all of that, as it already
//! did before any of this existed.
//!
//! **One sidebar per window, not one per project.** Each window carries its own
//! copy, which sounds wasteful and is not: only the visible one is ever drawn (the
//! others idle on a long sleep), and it removes the alternative entirely — moving
//! one shared pane between windows, which means resizing panes, which is the one
//! operation that can corrupt a terminal for good. The copy that is on screen is
//! always in the window you are looking at, so "which instance am I in?" needs no
//! bookkeeping at all: it is this pane's own window.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::core::{self, Instance, Project};
use crate::tmux::Tmux;
use crate::tui::{pad, size, tail, trunc, wrap, RawMode};

/// Width of the pane, in columns. Wide enough for `claude#12 ●  9✉` plus a name
/// worth reading, narrow enough to leave a usable claude beside it on 80 columns.
pub const WIDTH: u16 = 26;

/// How long a *hidden* sidebar waits between checks when it can read the daemon's
/// published focus. A file read is microseconds, so this can be short enough that
/// arriving at a window feels instant.
const IDLE_MS: u64 = 80;

/// ...and how long it waits when it has to ask **tmux** instead, because no daemon
/// is publishing. A `display-message` costs ~3.4 ms of CPU and every window has a
/// sidebar, so polling this path at `IDLE_MS` would burn a sixth of a core
/// discovering that nothing changed. The cost of the fallback is latency; the cost
/// of getting this wrong is a machine that is never idle.
const IDLE_MS_NO_DAEMON: u64 = 700;

/// How long the result of an action stays on the footer before the key hints come
/// back. Long enough to read a daemon error, short enough that the pane does not
/// end up permanently displaying something that happened minutes ago.
const NOTE_TTL: Duration = Duration::from_secs(6);

pub fn run(project_dir: &Path, conf: PathBuf) -> Result<()> {
    let t = Tmux::new(conf.clone());
    let me = std::env::var("TMUX_PANE").unwrap_or_default();
    // The window this pane lives in *is* the current instance, so there is no
    // "selected instance" to track and get wrong.
    let my_window = t.display(&me, "#{window_id}").unwrap_or_default();

    let mut term = RawMode::enable();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?25l"); // no cursor: this pane is a list, not a prompt

    let state_dir = crate::statedir::state_dir_for(project_dir);
    let mut act = Actions::new(project_dir, &conf);
    let mut mode = Mode::List;
    let mut cursor: usize = 0;
    let mut last = String::new();
    // Whether anything has been put on this pane yet. A window is created
    // **detached** and only then switched to, so a brand-new sidebar's first look
    // finds itself hidden — and going to sleep at that point is what left a new
    // claude's strip blank for 679 ms (measured 2026-09-04). The first frame is
    // drawn whether or not anyone is looking yet; every later one is not.
    let mut painted = false;
    loop {
        act.poll();

        // Two facts: is this window on screen, and does this pane hold the
        // keyboard. They are separate — the sidebar is visible in every window and
        // focused in almost none, which is the whole point of it being a display
        // rather than a menu.
        //
        // Read from the daemon's published file when there is one, because every
        // window runs one of these and a tmux query costs a process. See
        // `core::publish_focus`.
        let (visible, focused, cheap) = match core::read_focus(&state_dir) {
            Some((window, pane)) => (window == my_window, pane == me, true),
            None => {
                let flags = t.display(&me, "#{window_active}#{pane_active}").unwrap_or_default();
                let mut chars = flags.chars();
                (chars.next() == Some('1'), chars.next() == Some('1'), false)
            }
        };
        if !visible && painted {
            // Nothing to draw and nobody watching. Also drop the redraw cache, so
            // coming back to this window repaints from scratch rather than trusting
            // a picture that is however many seconds stale.
            last.clear();
            let nap = if cheap { IDLE_MS } else { IDLE_MS_NO_DAEMON };
            std::thread::sleep(Duration::from_millis(nap));
            continue;
        }

        let project = project_of(&t, project_dir);
        let rows: Vec<Row> = project.as_ref().map(rows_of).unwrap_or_default();
        cursor = cursor.min(rows.len().saturating_sub(1));

        // A claude or terminal this pane just created: go there, and put the cursor
        // on it so the list agrees with where the keyboard went.
        if let Some(id) = act.wants_focus() {
            if let Some(at) = rows.iter().position(|r| r.id == id) {
                cursor = at;
                focus(&t, &rows[at].window, &rows[at].pane);
                act.focused();
            }
        }

        let pane = Pane {
            window: &my_window,
            dir: project_dir,
            size: size((WIDTH, 24)),
            focused,
        };
        let frame = render(
            &rows,
            cursor,
            &pane,
            &footer(&mode, &act, Instant::now(), focused),
            match &mode {
                Mode::Rename { id, buf } => Some((id, buf)),
                _ => None,
            },
        );
        if frame != last {
            let _ = out.write_all(frame.as_bytes());
            let _ = out.flush();
            last = frame;
            painted = true;
        }

        let seq = term.read();
        if seq.is_empty() {
            continue;
        }

        // The rename field owns every byte typed while it is open, including the
        // ones that are keys elsewhere — `n` has to be a letter in a name.
        if let Mode::Rename { id, buf } = &mut mode {
            match edit(buf, &seq) {
                Edit::Typing => continue,
                Edit::Cancelled => mode = Mode::List,
                Edit::Done => {
                    let (id, name) = (*id, buf.clone());
                    act.rename(id, &name);
                    mode = Mode::List;
                }
            }
            continue;
        }

        let Some(key) = parse(&seq) else { continue };

        // A pending "close #N?" owns the keyboard until it is answered. Only `y`
        // goes through, and **every** other key cancels rather than falling through
        // to its normal meaning: the prompt is there because closing a claude ends
        // a conversation, and a scheme where the wrong keystroke still does
        // something is not a confirmation.
        if let Mode::Confirm { id, label } = &mode {
            if matches!(key, Key::Yes) {
                act.send(&format!("closing {label}"), "close", &id.to_string());
            }
            mode = Mode::List;
            continue;
        }

        match key {
            Key::Up => cursor = cursor.saturating_sub(1),
            Key::Down => cursor = (cursor + 1).min(rows.len().saturating_sub(1)),
            Key::Home => cursor = 0,
            Key::End => cursor = rows.len().saturating_sub(1),
            Key::Enter => {
                if let Some(row) = rows.get(cursor) {
                    focus(&t, &row.window, &row.pane);
                }
            }
            Key::New => act.send("new claude", "new", ""),
            Key::Term => act.send("new terminal", "term", ""),
            Key::Close => {
                if let Some(row) = rows.get(cursor) {
                    mode = Mode::Confirm {
                        id: row.id,
                        label: format!("{}#{}", row.kind, row.id),
                    };
                }
            }
            // Project switching wraps, matching tmux's own next/previous — with
            // two projects open, "next" and "previous" reaching different tabs
            // would be the surprising behaviour.
            Key::NextProject => act.switch(&["-n"]),
            Key::PrevProject => act.switch(&["-p"]),
            Key::Projects => act.pick(),
            Key::Messages => act.messages(),
            // Mute is a statement about the *view*, so it toggles off what the row
            // currently says rather than tracking a separate idea of the truth.
            Key::Mute => {
                if let Some(row) = rows.get(cursor) {
                    let (op, doing) = if row.muted {
                        ("unmute", "unmuting")
                    } else {
                        ("mute", "muting")
                    };
                    act.send(&format!("{doing} {}#{}", row.kind, row.id), op, &row.id.to_string());
                }
            }
            // No confirmation: a restart resumes the same conversation in the same
            // window, so there is nothing to lose by pressing it and nothing to undo.
            Key::Restart => {
                if let Some(row) = rows.get(cursor) {
                    act.send(&format!("restarting claude#{}", row.id), "restart", &row.id.to_string());
                }
            }
            Key::Rename => {
                if let Some(row) = rows.get(cursor) {
                    // Pre-filled with the current name, so the common edit is a
                    // tweak rather than retyping it.
                    mode = Mode::Rename { id: row.id, buf: row.name.clone().unwrap_or_default() };
                }
            }
            // Hand the keyboard back to the claude in this window.
            Key::Quit => {
                if let Some(row) = rows.iter().find(|r| r.window == my_window) {
                    focus(&t, &row.window, &row.pane);
                }
            }
            Key::Yes => {}
        }
    }
}

/// What the keyboard is currently for.
enum Mode {
    List,
    /// A close waiting on `y`. The id is captured when the key was pressed, not
    /// read from the cursor when it is answered — the list re-sorts underneath on
    /// every tick, and confirming "close claude#2" must not close whatever has
    /// since moved under the cursor.
    Confirm { id: usize, label: String },
    /// A name being typed, against the id it was opened on — same reason.
    Rename { id: usize, buf: String },
}

/// What a burst of bytes did to the field.
enum Edit {
    Typing,
    Done,
    Cancelled,
}

/// Apply typed bytes to the rename field.
///
/// Decoded as UTF-8 rather than byte-by-byte: a name here is usually Hebrew, and
/// pushing bytes as chars would split every one of them. `from_utf8_lossy` is safe
/// because a terminal delivers a character's bytes in one read.
fn edit(buf: &mut String, seq: &[u8]) -> Edit {
    for ch in String::from_utf8_lossy(seq).chars() {
        match ch {
            '\r' | '\n' => return Edit::Done,
            '\u{1b}' => return Edit::Cancelled,
            '\u{7f}' | '\u{8}' => {
                buf.pop();
            }
            // Everything else that is not printable is dropped rather than
            // inserted: a stray arrow key would otherwise put `^[[A` in the name.
            c if c.is_control() => {}
            c => buf.push(c),
        }
    }
    Edit::Typing
}

/// Anything that changes the world, run off the draw loop.
///
/// Every one of these is slow enough to be visible: `ipc::post` waits on a daemon
/// tick, and `display-popup -E` blocks for as long as the popup is open. Done
/// inline they would freeze the list — and a list that stops updating is exactly
/// how "did my key do anything?" starts.
struct Actions {
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    state_root: PathBuf,
    dir: PathBuf,
    conf: PathBuf,
    /// In-flight requests. A count, not a flag: pressing `n` twice means two
    /// claudes, the same as ⌘T twice does in the app.
    pending: usize,
    doing: Option<String>,
    note: Option<(String, Instant)>,
    /// An instance to jump to as soon as the list contains it, and when the wish
    /// expires. Held rather than acted on immediately because the reply arrives on
    /// a worker thread and only the draw loop knows where the rows are.
    focus_want: Option<(usize, Instant)>,
}

/// What a finished request came back with.
struct Done {
    msg: String,
    /// The instance it created, if it created one.
    focus: Option<usize>,
}

/// How long a pending focus stays wanted. The window exists before the daemon
/// replies, so one tick is normally enough; this only bounds the case where it
/// somehow never appears, so that a stale wish cannot hijack a later keystroke.
const FOCUS_GRACE: Duration = Duration::from_secs(3);

impl Actions {
    fn new(dir: &Path, conf: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        Actions {
            tx,
            rx,
            state_root: crate::statedir::state_root(),
            dir: dir.to_path_buf(),
            conf: conf.to_path_buf(),
            pending: 0,
            doing: None,
            note: None,
            focus_want: None,
        }
    }

    fn poll(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            if self.pending == 0 {
                self.doing = None;
            }
            if let Some(id) = done.focus {
                self.focus_want = Some((id, Instant::now()));
            }
            self.note = Some((done.msg, Instant::now()));
        }
    }

    /// The instance the draw loop should jump to, once it can see it.
    fn wants_focus(&mut self) -> Option<usize> {
        let (id, at) = self.focus_want?;
        if at.elapsed() > FOCUS_GRACE {
            self.focus_want = None;
            return None;
        }
        Some(id)
    }

    fn focused(&mut self) {
        self.focus_want = None;
    }

    /// Ask the daemon for something. Mutations go through it because instance-id
    /// allocation has to be single-threaded — see `ipc.rs`.
    fn send(&mut self, doing: &str, op: &str, arg: &str) {
        self.pending += 1;
        self.doing = Some(doing.to_string());
        // Creating something means going to it, the way ⌘T does in the app — and
        // the op is the whole test, so a caller cannot forget to ask for it or ask
        // for it on a close.
        let creates = matches!(op, "new" | "term");
        let tx = self.tx.clone();
        let root = self.state_root.clone();
        let project = self.dir.to_string_lossy().to_string();
        let (op, arg) = (op.to_string(), arg.to_string());
        std::thread::spawn(move || {
            let done = match std::env::current_exe()
                .map_err(anyhow::Error::from)
                .and_then(|me| crate::daemon::ensure_running(&root, &me))
                .and_then(|_| crate::ipc::post(&root, &crate::ipc::Request { op, project, arg }))
            {
                Ok(s) => Done {
                    focus: if creates { crate::core::id_in_reply(&s) } else { None },
                    msg: s,
                },
                Err(e) => Done { msg: format!("✗ {e:#}"), focus: None },
            };
            let _ = tx.send(done);
        });
    }

    /// Give an instance a name.
    ///
    /// Written straight into `namereq/<id>` — the same file `hub_set_name` uses, so
    /// the daemon's existing `process_name_requests` renames the window *and*
    /// writes the `named/<id>` flag that stops the hook nagging the instance to name
    /// itself. A human naming it counts; there is nothing left to ask for.
    ///
    /// A file, not a daemon op, for one reason beyond reuse: **the name never
    /// touches a command line.** It is arbitrary text the user just typed, and
    /// tmux's `#{q:...}` is not shell quoting (measured — a value containing a quote
    /// came through raw).
    fn rename(&mut self, id: usize, name: &str) {
        let name = crate::core::sanitize_label(name);
        if name.is_empty() {
            self.note = Some(("a name cannot be empty".into(), Instant::now()));
            return;
        }
        let dir = crate::statedir::state_dir_for(&self.dir).join(mulpex_core::NAMEREQ_DIR);
        let msg = match std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(id.to_string()), &name))
        {
            Ok(()) => format!("renamed #{id}"),
            Err(e) => format!("✗ {e}"),
        };
        self.note = Some((msg, Instant::now()));
    }

    /// Move the attached client to another project. Fast enough to be worth doing
    /// inline, but routed through the same reporting path so a failure is a line
    /// on the footer rather than nothing at all.
    fn switch(&mut self, args: &[&str]) {
        if let Err(e) = Tmux::new(self.conf.clone()).switch_client(args) {
            self.note = Some((format!("✗ {e:#}"), Instant::now()));
        }
    }

    /// The hub message feed, floated over the panes.
    ///
    /// `-d <project dir>` is load-bearing: `mpx messages` resolves its project from
    /// the working directory, and a popup inheriting the pane's cwd would quietly
    /// show a different project's feed.
    fn messages(&mut self) {
        self.pending += 1;
        self.doing = Some("messages".into());
        let (tx, conf, dir) = (self.tx.clone(), self.conf.clone(), self.dir.clone());
        std::thread::spawn(move || {
            let cmd = "mpx messages 60; echo; echo '[any key]'; \
                       read -r -k1 -s 2>/dev/null || read -n1";
            let msg = match Tmux::new(conf).display_popup(&dir, "80%", "70%", cmd) {
                Ok(()) => String::new(),
                Err(e) => format!("✗ {e:#}"),
            };
            let _ = tx.send(Done { msg, focus: None });
        });
    }

    /// The project picker, floated over the panes.
    ///
    /// Same popup route as the feed, and off the draw loop for the same reason:
    /// `display-popup -E` blocks for as long as the popup is open, and opening a
    /// project inside it spawns a claude — seconds, during which this list must
    /// keep updating. The picker does the switching itself, so nothing comes back
    /// here but a failure.
    fn pick(&mut self) {
        self.pending += 1;
        self.doing = Some("projects".into());
        let (tx, conf, dir) = (self.tx.clone(), self.conf.clone(), self.dir.clone());
        std::thread::spawn(move || {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mpx"));
            let cmd = crate::picker::popup_command(&exe);
            let msg = match Tmux::new(conf).display_popup(&dir, "70%", "60%", &cmd) {
                Ok(()) => String::new(),
                Err(e) => format!("✗ {e:#}"),
            };
            let _ = tx.send(Done { msg, focus: None });
        });
    }
}

/// What the bottom of the pane says.
///
/// The keys are the default because this list is the only place they are written
/// down — there is no menu bar to read them off, and an action nobody can discover
/// may as well not exist.
fn footer(mode: &Mode, act: &Actions, now: Instant, focused: bool) -> Vec<String> {
    if let Mode::Confirm { label, .. } = mode {
        return vec![format!("close {label}?"), "y confirm · any key no".into()];
    }
    if let Mode::Rename { .. } = mode {
        return vec!["⏎ save · esc cancel".into()];
    }
    if let Some(doing) = &act.doing {
        return vec![format!("… {doing}")];
    }
    if let Some((msg, at)) = &act.note {
        if !msg.is_empty() && now.duration_since(*at) < NOTE_TTL {
            return vec![msg.clone()];
        }
    }
    // Which keys are worth writing down depends on which keys can arrive. This
    // pane holds the keyboard almost never, and a list of letters that do nothing
    // is worse than no list — so unfocused it advertises the four keys that work
    // from anywhere, which is the scheme a person actually uses.
    if !focused {
        return vec![
            "^] ^[ move".into(),
            "^T new · ^W close".into(),
            "^P project".into(),
        ];
    }
    vec![
        "↑↓ move · ⏎ open".into(),
        "n new · t term · x close".into(),
        "m mute · r name · R restart".into(),
        "p project · < > switch".into(),
        "M mail · q back to claude".into(),
    ]
}

/// Move to a window and put the cursor in its claude, not its sidebar — landing
/// on the sidebar again would mean every jump needs a second keystroke.
fn focus(t: &Tmux, window: &str, pane: &str) {
    let _ = t.select_window(window);
    let _ = t.select_pane(pane);
}

/// This project, read out of tmux like everything else.
fn project_of(t: &Tmux, dir: &Path) -> Option<Project> {
    core::scan(t).ok()?.into_iter().find(|p| p.dir == dir)
}

/// One rendered line pair: an instance and what is worth knowing about it.
struct Row {
    id: usize,
    kind: String,
    window: String,
    pane: String,
    name: Option<String>,
    status: String,
    unread: usize,
    muted: bool,
    dead: bool,
}

fn rows_of(p: &Project) -> Vec<Row> {
    p.instances
        .iter()
        .map(|i: &Instance| Row {
            id: i.id,
            kind: i.kind.clone(),
            window: i.window.clone(),
            pane: i.pane.clone(),
            name: core::label_of(&i.window_name),
            // A terminal is never a hub peer, so it has no status to read and
            // asking for one would invent a `waiting` it never wrote.
            status: if i.is_claude() { status_of(&p.state_dir, i.id) } else { String::new() },
            unread: if i.is_claude() { unread(&p.state_dir, i.id) } else { 0 },
            muted: i.muted,
            dead: i.dead,
        })
        .collect()
}

/// One-word status, straight off disk.
///
/// A **missing** file is not idleness — the instance has simply never taken a
/// turn. `mcp::status_of` answers `waiting` for it, reporting ignorance in the
/// same word it reports being idle; on a row you are watching to decide whether to
/// go and look, that distinction is the whole point.
fn status_of(state_dir: &Path, id: usize) -> String {
    match std::fs::read_to_string(state_dir.join(id.to_string())) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "starting".into(),
    }
}

fn unread(state_dir: &Path, id: usize) -> usize {
    std::fs::read_dir(state_dir.join("inbox").join(id.to_string()))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

// ---- drawing ---------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const REVERSE: &str = "\x1b[7m";

/// Colour by what the row is asking of you, not by what it is doing.
///
/// `needs` means **needs YOU** — it is the only state that should pull your eye,
/// so it is the only one that gets a bright colour.
fn status_mark(status: &str) -> (&'static str, &'static str) {
    match status {
        "needs" => ("\x1b[91m", "◆"),   // bright red: waiting on you
        "working" => ("\x1b[33m", "●"), // yellow: busy, nothing wanted
        "waiting" => ("\x1b[2m", "○"),  // dim: idle
        "starting" => ("\x1b[2m", "·"), // dim: has never taken a turn
        _ => ("\x1b[2m", " "),
    }
}

/// What the pane knows about **itself**, as opposed to the list it is drawing.
///
/// Grouped rather than passed loose because these four always travel together and
/// are read together: two of them (`window`, `focused`) are the reason a row can be
/// marked two different ways at once.
struct Pane<'a> {
    /// The window this sidebar lives in — which *is* the instance you are in, so
    /// there is no "current instance" to track separately and get wrong.
    window: &'a str,
    dir: &'a Path,
    size: (u16, u16),
    /// Whether this pane holds the keyboard. A cursor and a key hint are both
    /// statements about the next keystroke, so both are gated on it.
    focused: bool,
}

fn render(
    rows: &[Row],
    cursor: usize,
    pane: &Pane,
    foot: &[String],
    editing: Option<(&usize, &String)>,
) -> String {
    let (my_window, dir, focused) = (pane.window, pane.dir, pane.focused);
    let (w, h) = pane.size;
    let w = w.max(8) as usize;
    // Home the cursor and overwrite; **do not clear**. Every line below is padded
    // to exactly `w` and there are exactly `h` of them, so the repaint covers each
    // cell on its own. A `\x1b[2J` in front of that is not free: the frame is a
    // few kilobytes and a PTY write is delivered in chunks, so tmux can render the
    // erase before the content arrives — an empty pane for one frame, every time
    // the list changes. That is the flicker on opening a claude.
    let mut s = String::from("\x1b[H");

    let title = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    s.push_str(&format!("{BOLD}{}{RESET}\r\n", pad(&title, w)));
    s.push_str(&format!("{DIM}{}{RESET}\r\n", pad(&"─".repeat(w), w)));

    let mut lines = 2usize;
    for (i, r) in rows.iter().enumerate() {
        if lines + 2 > h as usize {
            break;
        }
        // Two different things, deliberately shown differently: the window you are
        // IN (reverse) and the row your keyboard cursor is on (a ▸ gutter). They
        // are usually the same row and, the moment you start moving, they are not.
        //
        // The gutter is drawn **only while this pane has the keyboard**. A cursor
        // is a statement about where the next key lands; on a pane that receives
        // none it is decoration that cannot move, permanently marking row one for
        // no reason. ("what is the right arrow near claude#1?" — 2026-09-06.)
        let here = r.window == my_window;
        let sel = focused && i == cursor;
        let (colour, glyph) = status_mark(&r.status);

        let mut left = format!("{}#{}", r.kind, r.id);
        if r.dead {
            left = format!("✗ {left}");
        }
        let mail = if r.unread > 0 { format!(" {}✉", r.unread) } else { String::new() };
        let head = format!("{} {}{}", left, glyph, mail);

        s.push_str(if sel { "▸" } else { " " });
        if here {
            s.push_str(REVERSE);
        }
        if r.muted {
            s.push_str(DIM);
        }
        s.push_str(colour);
        s.push_str(&pad(&head, w.saturating_sub(1)));
        s.push_str(RESET);
        s.push_str("\r\n");
        lines += 1;

        // The name an instance gave itself is model-written text in whatever
        // language the user works in, so it goes through the bidi conversion for
        // the same reason the messages feed does. See `bidi.rs`.
        //
        // While this row is being renamed the same line becomes the field, so the
        // text is edited where it will appear rather than in a box somewhere else.
        // The tail is kept rather than the head when it overflows — you are typing
        // at the end, and a field that hides what you are typing is not a field.
        match editing {
            Some((id, buf)) if *id == r.id => {
                let shown = crate::bidi::visual(&tail(buf, w.saturating_sub(4)));
                s.push_str(&format!("  {REVERSE}{}{RESET}\r\n", pad(&format!("{shown}▏"), w.saturating_sub(2))));
            }
            _ => {
                let name = r.name.clone().unwrap_or_else(|| "—".into());
                let name = crate::bidi::visual(&trunc(&name, w.saturating_sub(3)));
                s.push_str(&format!("  {DIM}{}{RESET}\r\n", pad(&name, w.saturating_sub(2))));
            }
        }
        lines += 1;
    }

    // Footer, pinned to the bottom so it does not wander as instances come and go.
    // A daemon error is arbitrary length, so wrap rather than truncate: the useful
    // half of "the mulpex daemon did not answer in 5s — check …" is the end.
    // Always at least one line, even when there is nothing to say: the frame is
    // written without a trailing newline so that it ends exactly at the bottom row,
    // and an empty footer would leave one `\r\n` too many.
    let mut foot: Vec<String> = foot.iter().flat_map(|l| wrap(l, w)).collect();
    if foot.is_empty() {
        foot.push(String::new());
    }
    let foot = &foot[foot.len().saturating_sub((h as usize).saturating_sub(lines).max(1))..];
    // `\x1b[K` — erase the line, do not merely skip it.
    //
    // Every other line here is padded to exactly `w`, so it overwrites what was
    // under it. These blank ones write **nothing**, and a bare `\r\n` moves the
    // cursor without clearing: with the screen no longer wiped each frame, a row
    // that disappears from the list stays on the pane for good. Measured — a closed
    // claude was still listed ten seconds later, on a sidebar that was repainting
    // correctly the whole time.
    //
    // Only safe on these lines, and deliberately not appended to the others: after
    // a line that exactly fills the width the cursor has already wrapped, so a `K`
    // there would erase the line *below* instead.
    while lines + foot.len() < h as usize {
        s.push_str("\x1b[K\r\n");
        lines += 1;
    }
    for (i, line) in foot.iter().enumerate() {
        s.push_str(&format!("{DIM}{}{RESET}", pad(line, w)));
        if i + 1 < foot.len() {
            s.push_str("\r\n");
        }
    }
    s
}

// ---- keys ------------------------------------------------------------------

/// The keys are **plain letters**, unmodified, because focus is in this pane and
/// nothing else is competing for them. That is the whole point of the sidebar
/// owning the keyboard: `Ctrl-]` is the one key taken from claude, and everything
/// else costs nothing because it only means anything in here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Up,
    Down,
    Home,
    End,
    Enter,
    Quit,
    New,
    Term,
    Close,
    NextProject,
    PrevProject,
    Projects,
    Messages,
    Yes,
    Mute,
    Rename,
    Restart,
}

fn parse(seq: &[u8]) -> Option<Key> {
    let mut out = None;
    let mut i = 0;
    while i < seq.len() {
        match seq[i] {
            0x1b if seq.get(i + 1) == Some(&b'[') => {
                out = match seq.get(i + 2) {
                    Some(b'A') => Some(Key::Up),
                    Some(b'B') => Some(Key::Down),
                    Some(b'H') => Some(Key::Home),
                    Some(b'F') => Some(Key::End),
                    _ => out,
                };
                i += 3;
            }
            b'\r' | b'\n' => {
                out = Some(Key::Enter);
                i += 1;
            }
            b'k' => {
                out = Some(Key::Up);
                i += 1;
            }
            b'j' => {
                out = Some(Key::Down);
                i += 1;
            }
            b'g' => {
                out = Some(Key::Home);
                i += 1;
            }
            b'G' => {
                out = Some(Key::End);
                i += 1;
            }
            b'n' => {
                out = Some(Key::New);
                i += 1;
            }
            b't' => {
                out = Some(Key::Term);
                i += 1;
            }
            b'x' => {
                out = Some(Key::Close);
                i += 1;
            }
            // `<`/`>`, not `[`/`]`. Ctrl-[ / Ctrl-] move between *instances* from
            // anywhere, including inside this pane — so brackets meaning "project"
            // here would be the same two keys meaning two different things
            // depending on a modifier. One meaning per key.
            b'>' => {
                out = Some(Key::NextProject);
                i += 1;
            }
            b'<' => {
                out = Some(Key::PrevProject);
                i += 1;
            }
            b'p' => {
                out = Some(Key::Projects);
                i += 1;
            }
            b'M' => {
                out = Some(Key::Messages);
                i += 1;
            }
            b'y' => {
                out = Some(Key::Yes);
                i += 1;
            }
            b'm' => {
                out = Some(Key::Mute);
                i += 1;
            }
            b'r' => {
                out = Some(Key::Rename);
                i += 1;
            }
            b'R' => {
                out = Some(Key::Restart);
                i += 1;
            }
            b'q' | 0x1b => {
                out = Some(Key::Quit);
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane<'a>(window: &'a str, dir: &'a str, size: (u16, u16), focused: bool) -> Pane<'a> {
        Pane { window, dir: Path::new(dir), size, focused }
    }

    fn row(id: usize, kind: &str, status: &str) -> Row {
        Row {
            id,
            kind: kind.into(),
            window: format!("@{id}"),
            pane: format!("%{id}"),
            name: Some(format!("task {id}")),
            status: status.into(),
            unread: 0,
            muted: false,
            dead: false,
        }
    }

    /// `needs` means needs YOU. It is the one state that should pull the eye, so
    /// it is the only one drawn bright.
    #[test]
    fn only_needing_you_is_loud() {
        assert_eq!(status_mark("needs").0, "\x1b[91m");
        for quiet in ["working", "waiting", "starting", ""] {
            assert_ne!(status_mark(quiet).0, "\x1b[91m", "{quiet} must not shout");
        }
    }

    /// A claude that has never taken a turn is `starting`, not `waiting`: the
    /// status file is absent, and absence is ignorance rather than idleness.
    #[test]
    fn a_missing_status_file_reads_as_starting() {
        let d = std::env::temp_dir().join(format!("mpx-sb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(status_of(&d, 1), "starting");
        std::fs::write(d.join("1"), "needs").unwrap();
        assert_eq!(status_of(&d, 1), "needs");
    }

    /// The window you are in and the row your cursor is on are different facts.
    /// They start equal and diverge the moment you press a key, which is exactly
    /// when you need to be able to tell them apart.
    #[test]
    fn the_current_window_and_the_cursor_are_marked_separately() {
        let rows = vec![row(1, "claude", "working"), row(2, "claude", "needs")];
        let frame = render(&rows, 1, &pane("@1", "/x/proj", (26, 20), true), &[], None);
        let line1 = frame.lines().find(|l| l.contains("claude#1")).unwrap();
        let line2 = frame.lines().find(|l| l.contains("claude#2")).unwrap();
        assert!(line1.contains(REVERSE), "row 1 is the window we are in");
        assert!(!line1.starts_with('▸'), "but the cursor is not on it");
        assert!(line2.starts_with('▸'), "row 2 has the cursor");
        assert!(!line2.contains(REVERSE), "and is not where we are");
    }

    /// A terminal is never a hub peer, so it has no status to show — inventing one
    /// would put a `waiting` on a row that never wrote a status file.
    #[test]
    fn a_terminal_row_carries_no_status() {
        let p = Project {
            session: "p".into(),
            dir: "/p".into(),
            state_dir: std::env::temp_dir().join("mpx-sb-none"),
            instances: vec![Instance {
                id: 3,
                window: "@3".into(),
                pane: "%3".into(),
                kind: "term".into(),
                window_name: "term#3 ▸ dev server".into(),
                dead: false,
                dead_status: String::new(),
                history: 0,
                alt: false,
                current_command: "zsh".into(),
                current_path: "/p".into(),
                muted: false,
            }],
            active_window: String::new(),
            active_pane: String::new(),
        };
        let rows = rows_of(&p);
        assert_eq!(rows[0].status, "");
        assert_eq!(rows[0].unread, 0);
        assert_eq!(rows[0].name.as_deref(), Some("dev server"));
    }

    /// Holding an arrow delivers several presses in one read; acting on each would
    /// fly past the row you were aiming at.
    #[test]
    fn a_burst_of_keys_moves_once() {
        assert!(matches!(parse(b"\x1b[A\x1b[A\x1b[A"), Some(Key::Up)));
        assert!(matches!(parse(b"\x1b[B"), Some(Key::Down)));
        assert!(matches!(parse(b"\r"), Some(Key::Enter)));
        assert!(matches!(parse(b"q"), Some(Key::Quit)));
        assert!(parse(b"").is_none());
        assert!(parse(b"zzz").is_none(), "keys we do not claim are ignored");
    }

    /// The frame is rebuilt every tick and only written when it changed — an
    /// unchanged sidebar must not repaint five times a second forever.
    #[test]
    fn an_unchanged_list_produces_an_identical_frame() {
        let rows = vec![row(1, "claude", "working")];
        let hints = footer(&Mode::List, &Actions::new(Path::new("/x/p"), Path::new("/c")), Instant::now(), true);
        let a = render(&rows, 0, &pane("@1", "/x/p", (26, 20), true), &hints, None);
        let b = render(&rows, 0, &pane("@1", "/x/p", (26, 20), true), &hints, None);
        assert_eq!(a, b);
    }

    /// The action keys, and the ones deliberately not claimed. `y` only means
    /// anything at a confirm prompt, but it has to parse or the prompt cannot be
    /// answered.
    #[test]
    fn the_action_keys_parse() {
        for (byte, want) in [
            (&b"n"[..], Key::New),
            (b"t", Key::Term),
            (b"x", Key::Close),
            (b">", Key::NextProject),
            (b"<", Key::PrevProject),
            (b"p", Key::Projects),
            (b"M", Key::Messages),
            (b"y", Key::Yes),
            (b"m", Key::Mute),
            (b"r", Key::Rename),
            (b"R", Key::Restart),
        ] {
            assert_eq!(parse(byte), Some(want), "{:?}", byte);
        }
        assert_eq!(parse(b"\x1b[A"), Some(Key::Up));
        assert_eq!(parse(b"\x1b[B"), Some(Key::Down));
        // Keys we do not claim stay unclaimed rather than doing something near-miss.
        // Brackets belong to Ctrl-[ / Ctrl-] (move instance), which tmux claims
        // before this pane ever sees them. One meaning per key.
        for unclaimed in [&b"d"[..], b"\t", b"[", b"]"] {
            assert_eq!(parse(unclaimed), None, "{:?}", unclaimed);
        }
    }

    /// Closing a claude ends a conversation, and the list re-sorts under the
    /// cursor every tick. So the id is captured when `x` is pressed, and only `y`
    /// goes through — a scheme where the wrong key still closes something is not a
    /// confirmation.
    #[test]
    fn the_close_prompt_names_what_it_will_close() {
        let mode = Mode::Confirm { id: 2, label: "claude#2".into() };
        let act = Actions::new(Path::new("/x/p"), Path::new("/c"));
        let f = footer(&mode, &act, Instant::now(), true);
        assert_eq!(f[0], "close claude#2?");
        assert!(f[1].contains("y confirm"));
        assert!(f[1].contains("any key no"), "anything but y cancels: {}", f[1]);
    }

    /// The footer is where the keys are written down — there is no menu bar to
    /// read them off, so an action missing from here is an action nobody finds.
    #[test]
    fn every_action_key_is_advertised() {
        let act = Actions::new(Path::new("/x/p"), Path::new("/c"));
        let text = footer(&Mode::List, &act, Instant::now(), true).join(" ");
        for key in ["n ", "t ", "x ", "m ", "r ", "R ", "p ", "<", ">", "M ", "q ", "⏎"] {
            assert!(text.contains(key), "{key:?} missing from {text:?}");
        }
    }

    /// A cursor is a statement about where the next key lands, so a pane that
    /// receives none must not draw one — it can never move off row one, which is
    /// exactly how it was noticed ("what is the right arrow near claude#1?").
    /// Unfocused, the footer also advertises the keys that do work from anywhere.
    #[test]
    fn an_unfocused_sidebar_has_no_cursor_and_offers_the_bare_keys() {
        let rows = vec![row(1, "claude", "working"), row(2, "claude", "waiting")];
        let act = Actions::new(Path::new("/x/p"), Path::new("/c"));
        let blurred = footer(&Mode::List, &act, Instant::now(), false);

        let text = blurred.join(" ");
        for key in ["^]", "^[", "^T", "^W", "^P"] {
            assert!(text.contains(key), "{key} missing from {text:?}");
        }
        assert!(!text.contains("r name"), "letters that cannot arrive: {text:?}");

        let f = render(&rows, 0, &pane("@1", "/x/p", (26, 20), false), &blurred, None);
        assert!(!f.contains('▸'), "no cursor on a pane that receives no keys");
        // The window you are IN is a different fact and does not depend on focus.
        let line = f.lines().find(|l| l.contains("claude#1")).unwrap();
        assert!(line.contains(REVERSE), "still shows where you are");

        let focused = render(&rows, 0, &pane("@1", "/x/p", (26, 20), true), &[], None);
        assert!(focused.contains('▸'), "and it comes back when the keys can arrive");
    }

    /// The rename field takes text, not keys — `n` has to be a letter in a name and
    /// not "new claude". Decoded as UTF-8 because the names here are usually Hebrew.
    #[test]
    fn the_rename_field_takes_text() {
        let mut b = String::new();
        assert!(matches!(edit(&mut b, "hello".as_bytes()), Edit::Typing));
        assert!(matches!(edit(&mut b, " nx".as_bytes()), Edit::Typing));
        assert_eq!(b, "hello nx", "letters that are keys elsewhere are just letters");
        assert!(matches!(edit(&mut b, b"\x7f\x7f"), Edit::Typing));
        assert_eq!(b, "hello ");
        // A whole character, not a byte: backspacing Hebrew must not leave half of it.
        let mut h = String::from("שלום");
        edit(&mut h, b"\x7f");
        assert_eq!(h, "שלו");
        edit(&mut h, "ם".as_bytes());
        assert_eq!(h, "שלום");
        // A stray arrow key must not end up in the name as `^[[A`.
        let mut c = String::from("x");
        assert!(matches!(edit(&mut c, b"\x1b[A"), Edit::Cancelled));
        assert!(matches!(edit(&mut String::new(), b"\r"), Edit::Done));
    }

    /// A row that leaves the list must leave the *pane*. The frame no longer wipes
    /// the screen, so every line it emits has to cover what was under it — and the
    /// blank ones write nothing at all, so they carry an erase instead. Without it
    /// a closed claude stayed on screen for good, on a sidebar that was otherwise
    /// repainting perfectly (measured 2026-09-06).
    #[test]
    fn shrinking_the_list_erases_the_rows_it_dropped() {
        let two = vec![row(1, "claude", "working"), row(2, "claude", "waiting")];
        let one = vec![row(1, "claude", "working")];
        let p = pane("@1", "/x/p", (26, 14), false);

        let before = render(&two, 0, &p, &[], None);
        let after = render(&one, 0, &p, &[], None);
        assert!(before.contains("claude#2"));
        assert!(!after.contains("claude#2"));

        // The shorter frame must still account for every row of the pane, and the
        // ones it does not draw into must be erased rather than skipped.
        assert_eq!(after.split("\r\n").count(), 14);
        assert!(after.contains("\x1b[K"), "blank lines must erase:\n{after:?}");
        // ...and never by clearing the whole screen, which is what the repaint
        // avoids in the first place.
        assert!(!after.contains("\x1b[2J"));
    }

    /// The footer is pinned to the bottom, so a taller footer must eat blank space
    /// rather than push the list off the top or overflow the pane.
    #[test]
    fn the_footer_stays_inside_the_pane() {
        let rows = vec![row(1, "claude", "working")];
        for foot in [vec![], vec!["one".to_string()], footer(&Mode::List, &Actions::new(Path::new("/p"), Path::new("/c")), Instant::now(), true)] {
            let frame = render(&rows, 0, &pane("@1", "/x/p", (26, 10), true), &foot, None);
            assert_eq!(frame.split("\r\n").count(), 10, "{foot:?} must fill 10 rows exactly");
        }
    }
}
