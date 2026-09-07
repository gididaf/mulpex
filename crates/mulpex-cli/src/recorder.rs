//! Plain-text transcripts of shell terminals, for `hub_terminal_read`.
//!
//! This is the CLI's replacement for `src-tauri/src/vtgrid.rs` — and the clearest
//! case in the whole port for letting tmux be the emulator. The desktop app owns
//! the PTY bytes, so it hand-rolled a bounded VT grid (1,680 lines: escape
//! sequences, wrapping, scroll regions, the alternate screen) purely so a row
//! could be written to the log *once it could no longer change*. tmux already
//! maintains that grid. What is left here is polling two numbers and copying text.
//!
//! **The output format is not ours to choose.** `mulpex-core::mcp` reads these
//! files from inside a `claude`, is shared verbatim with the desktop app, and must
//! not change. So this writes exactly what it reads:
//!
//! - `terminals/<id>.log` — [`mulpex_core::termlog`]'s 50-byte header then plain
//!   text. Only rows that have **scrolled off** go here.
//! - `terminals/<id>.screen` — what is on screen right now, which for a short
//!   command is the only place its output exists.
//! - `terminals/<id>.frames` — length-prefixed snapshots of a full-screen
//!   program, which repaints in place and so scrolls nothing into the log.
//! - `terminals/index`, `terminals/meta` — the manifest and per-terminal state
//!   `hub_instances` and `hub_terminal_read` merge in.
//!
//! ## Three behaviours preserved on purpose
//!
//! **The completion marker is found in the log, not the screen.** `mcp.rs` looks
//! for `__MPX_DONE_` in the log only, so a short command whose output never
//! scrolls reports finished late. That asymmetry exists in the desktop app today;
//! fixing it on one side would make the two hosts disagree about the same file.
//!
//! **A full-screen program's last frame is the one worth having.** `?1049l`
//! restores the previous screen, so the frame a reader wants is the one that would
//! otherwise never be recorded. The desktop sees the escape sequence and snapshots
//! at that instant. Polling cannot: measured, `capture-pane -a` after the program
//! exits answers `no alternate screen`, so tmux keeps nothing to go back for. What
//! this does instead is keep the newest alt-screen capture in memory and flush it
//! the moment `alternate_on` drops — leaving the last frame **up to one tick
//! (200 ms) stale** rather than absent. That is the one place the CLI is weaker
//! than the app, and it is a bounded weakness rather than a missing feature.
//!
//! **`last_out_ms` is screen-change-based, not per-byte** — the documented
//! semantic change from the plan. The app stamps it on every PTY read; here the
//! finest available grain is a tick, and output that redraws the same screen (a
//! spinner on its second revolution) does not count as activity at all. `idle_ms`
//! is therefore coarser and, for a repainting program, larger. `remote.rs`'s
//! silence backstop reads `idle_ms`, so it fires slightly sooner — in the safe
//! direction, since it only ever asks whether a turn has ended.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use mulpex_core::termlog::{self, Header};

use crate::core::Project;
use crate::tmux::{Cap, Tmux};

/// Trim the transcript once its data section passes this…
const MAX_LOG: u64 = 1024 * 1024;
/// …down to this, cut at a line boundary.
const KEEP_LOG: u64 = 512 * 1024;
/// Same, for the frame log.
const MAX_FRAMES: u64 = 1024 * 1024;
const KEEP_FRAMES: u64 = 512 * 1024;

/// Never append a frame more often than this. A TUI repaints many times a second
/// and one frame per second is what a reader can actually use — but the *newest*
/// capture is always kept in memory regardless, so the exit flush is fresh.
const FRAME_MIN_INTERVAL_MS: u64 = 1000;

/// Record header: the marker, the wall-clock ms, and the exact byte length of the
/// screen text that follows. Length-prefixed rather than delimiter-separated
/// because the payload is arbitrary screen content — scanning it for a separator
/// is exactly how that kind of format breaks. Parsed by `mcp::read_frames`.
const FRAME_MARK: &str = "--- MPXF";

/// What stands in the transcript where a full-screen program's output would be.
/// Without it `new_output` is empty and the reader has no way to learn that a
/// history exists at all, in `frames`.
const ALT_SCREEN_NOTE: &str =
    "[full-screen program — repaints in place, so nothing scrolls into this history; \
read it with hub_terminal_read's frames instead]";

/// How far past the last consumed row each capture reaches back, to absorb the
/// drift between measuring `history_size` and capturing. Measured drift on a
/// paced loop was one row; this is two orders of magnitude of headroom, and the
/// overlap search skips whatever it re-reads.
const DRIFT_SLACK_ROWS: u64 = 64;

/// Ceiling on one capture, so a pane that scrolled its whole 50,000-row history
/// in one tick is still bounded work.
const MAX_CAPTURE_ROWS: u64 = 4_000;

/// How much of the transcript's tail to align against. Must comfortably exceed
/// `DRIFT_SLACK_ROWS` rows of text, or the overlap cannot reach the join.
const OVERLAP_BYTES: u64 = 32 * 1024;

/// How long to wait for a shell to paint its prompt before typing a seeded
/// command in anyway. A shell is up in milliseconds, so this is a backstop.
const SEED_TIMEOUT_MS: u64 = 5_000;

/// How long a seed waits for its terminal to show up in a scan before being
/// discarded. Only ever one tick in practice; the expiry exists so a window that
/// died on creation cannot leave the command queued forever.
const SEED_EXPIRY_MS: u64 = 60_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// FNV-1a, byte-for-byte the function `mcp::screen_fingerprint` uses. It is
/// written here and compared there, in another process, so the algorithm is part
/// of the on-disk contract — `DefaultHasher` is explicitly not stable enough to
/// stand in.
fn fingerprint(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The append-and-trim transcript writer.
///
/// Trimming rewrites **in place on this one handle** and writes the header
/// **last**. A rename-based trim would unlink the inode this fd points at and
/// every later byte would go to a deleted file — silently, forever. Header-last
/// is what lets a reader that sees `base` change mid-read know the data moved and
/// simply retry (`mcp::LogView::open`).
struct Log {
    f: File,
    base: u64,
    len: u64,
    last_out_ms: u64,
    exited: bool,
}

impl Log {
    fn open(path: &Path) -> std::io::Result<Log> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut f = OpenOptions::new().read(true).write(true).create(true).open(path)?;
        let size = f.metadata()?.len();
        let mut log = if size >= termlog::HEADER_LEN as u64 {
            let mut buf = vec![0u8; termlog::HEADER_LEN];
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut buf)?;
            match termlog::parse_header(&buf) {
                // Reopening an existing transcript (a daemon restart): keep every
                // byte and every logical offset, because readers hold cursors
                // into them and resetting would replay the whole history.
                Some(h) => Log {
                    f,
                    base: h.base,
                    len: size - termlog::HEADER_LEN as u64,
                    last_out_ms: h.last_out_ms,
                    exited: h.exited,
                },
                None => Log { f, base: 0, len: 0, last_out_ms: now_ms(), exited: false },
            }
        } else {
            Log { f, base: 0, len: 0, last_out_ms: now_ms(), exited: false }
        };
        if log.len == 0 {
            log.f.set_len(0)?;
            log.write_header()?;
        }
        Ok(log)
    }

    fn write_header(&mut self) -> std::io::Result<()> {
        let h = Header { base: self.base, last_out_ms: self.last_out_ms, exited: self.exited };
        self.f.seek(SeekFrom::Start(0))?;
        self.f.write_all(termlog::format_header(&h).as_bytes())?;
        self.f.flush()
    }

    /// The last `n` bytes of the data section, for aligning a capture against
    /// what is already recorded. Cut forward to a char boundary so the comparison
    /// never starts inside a multi-byte character.
    fn tail(&mut self, n: u64) -> String {
        let want = n.min(self.len);
        let mut buf = vec![0u8; want as usize];
        if self
            .f
            .seek(SeekFrom::Start(termlog::HEADER_LEN as u64 + self.len - want))
            .and_then(|_| self.f.read_exact(&mut buf))
            .is_err()
        {
            return String::new();
        }
        match std::str::from_utf8(&buf) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&buf[e.valid_up_to()..]).into_owned(),
        }
    }

    fn append(&mut self, text: &str) -> std::io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.f.seek(SeekFrom::Start(termlog::HEADER_LEN as u64 + self.len))?;
        self.f.write_all(text.as_bytes())?;
        self.len += text.len() as u64;
        self.f.flush()?;
        if self.len > MAX_LOG {
            self.trim()?;
        }
        Ok(())
    }

    /// Drop the front of the data section, keeping `KEEP_LOG` bytes cut at a line
    /// boundary, and advance `base` by exactly what was dropped.
    fn trim(&mut self) -> std::io::Result<()> {
        // Guard, not decoration: `len - KEEP_LOG` would wrap to ~2^64 in release
        // and the read below would try to allocate it.
        let Some(drop_at) = self.len.checked_sub(KEEP_LOG) else { return Ok(()) };
        let mut data = vec![0u8; self.len as usize];
        self.f.seek(SeekFrom::Start(termlog::HEADER_LEN as u64))?;
        self.f.read_exact(&mut data)?;
        // Cut at the next newline so the transcript never begins mid-line, and at
        // a char boundary so the reader's `from_utf8_lossy` sees no mojibake.
        let mut cut = drop_at as usize;
        while cut < data.len() && data[cut] != b'\n' {
            cut += 1;
        }
        if cut < data.len() {
            cut += 1;
        }
        let kept = &data[cut..];
        self.f.seek(SeekFrom::Start(termlog::HEADER_LEN as u64))?;
        self.f.write_all(kept)?;
        self.f.set_len(termlog::HEADER_LEN as u64 + kept.len() as u64)?;
        self.base += cut as u64;
        self.len = kept.len() as u64;
        // Header last: until this line a reader still sees the old `base`, and the
        // mismatch across its own read is its signal to retry.
        self.write_header()
    }
}

/// Everything the recorder remembers about one terminal between ticks.
struct TermState {
    log: Log,
    /// `history_size` at the last tick; the delta is what to capture.
    history: u64,
    alt: bool,
    /// Fingerprint of the screen last written to `<id>.screen`.
    screen_fp: u64,
    /// The newest alt-screen capture, not yet appended. Held so it can be flushed
    /// the moment the program exits — see the module header.
    pending_frame: Option<String>,
    last_frame_ms: u64,
    frames_len: u64,
    /// A command to type once the shell has painted its prompt.
    seed: Option<String>,
    seed_deadline: u64,
    /// The shell's own command name, so `command_running` can say whether the
    /// foreground process is something else.
    shell_name: String,
}

#[derive(Default)]
pub struct Recorder {
    terms: HashMap<(PathBuf, usize), TermState>,
    /// Commands waiting for their terminal to appear. A window is created inside
    /// a request, but the recorder only learns of it from the *next* scan, so the
    /// seed has to outlive that gap — dropping it here would mean
    /// `hub_terminal_open(command: …)` silently opened an empty shell.
    seeds: HashMap<(PathBuf, usize), (String, u64)>,
    /// Last content written per state dir, so an unchanged manifest is not
    /// rewritten five times a second.
    index: HashMap<PathBuf, String>,
    meta: HashMap<PathBuf, String>,
}

/// One capture to run this tick.
struct Job {
    window: String,
    what: Cap,
}

/// The captures belonging to one terminal, as indices into the flat job list.
struct TermJobs {
    key: (PathBuf, usize),
    /// Whether the pane was on the alternate screen **at the scan this tick's
    /// captures belong to**. Carried rather than read again later: the captures
    /// and this flag have to describe the same instant, or a frame is filed
    /// against the wrong screen.
    alt: bool,
    /// The scrolled-off rows, and the same range plus the topmost visible row.
    /// Present together or not at all — the second is only ever read to decide
    /// whether the first ends mid-line.
    history: Option<(usize, usize)>,
    screen: usize,
}

impl Recorder {
    /// Record a command to type into a terminal once its prompt appears.
    pub fn seed(&mut self, state_dir: &Path, id: usize, command: String) {
        self.seeds
            .insert((state_dir.to_path_buf(), id), (command, now_ms() + SEED_EXPIRY_MS));
    }

    /// Forget a terminal that is gone. Its files stay on disk: a reader mid-wait
    /// must be told the terminal closed, not handed an empty transcript.
    ///
    /// Seeds are **not** filtered by liveness. A request creates the window and
    /// the seed together, but the scan this tick ran on is older than both, so a
    /// brand-new terminal is legitimately absent from `live` — dropping its seed
    /// here would lose the command on every single `hub_terminal_open`. They
    /// expire on a clock instead.
    fn forget_missing(&mut self, live: &[(PathBuf, usize)]) {
        self.terms.retain(|k, _| live.contains(k));
        let now = now_ms();
        self.seeds.retain(|_, (_, expiry)| *expiry > now);
    }

    /// One pass over every terminal in every project.
    pub fn tick(&mut self, t: &Tmux, projects: &[Project]) {
        let mut live: Vec<(PathBuf, usize)> = Vec::new();
        let mut jobs: Vec<Job> = Vec::new();
        let mut plan: Vec<TermJobs> = Vec::new();

        for p in projects {
            for inst in p.instances.iter().filter(|i| i.is_shell()) {
                let key = (p.state_dir.clone(), inst.id);
                live.push(key.clone());
                if !self.terms.contains_key(&key) {
                    let path = p.state_dir.join("terminals").join(format!("{}.log", inst.id));
                    let Ok(log) = Log::open(&path) else { continue };
                    let seed = self.seeds.remove(&key).map(|(c, _)| c);
                    let frames_len = std::fs::metadata(
                        p.state_dir.join("terminals").join(format!("{}.frames", inst.id)),
                    )
                    .map(|m| m.len())
                    .unwrap_or(0);
                    self.terms.insert(
                        key.clone(),
                        TermState {
                            log,
                            // Adopt the pane's current history rather than 0: a
                            // daemon that restarts must not re-capture and
                            // re-append every row the terminal ever scrolled.
                            history: inst.history,
                            alt: inst.alt,
                            screen_fp: 0,
                            pending_frame: None,
                            last_frame_ms: 0,
                            frames_len,
                            seed,
                            seed_deadline: now_ms() + SEED_TIMEOUT_MS,
                            shell_name: inst.current_command.clone(),
                        },
                    );
                }
                let Some(st) = self.terms.get(&key) else { continue };
                // A dead pane has nothing new to say, but its screen must stay
                // exactly as it was — an exited terminal has to remain readable.
                if inst.dead {
                    continue;
                }
                let delta = history_delta(st.history, inst.history);
                let history = (delta > 0).then(|| {
                    // Reach back past where we stopped; `absorb_history` finds the
                    // join by content. Sized so the slack cannot be swallowed by
                    // ordinary drift, and capped so a huge burst is still bounded.
                    let rows = (delta + DRIFT_SLACK_ROWS).min(MAX_CAPTURE_ROWS);
                    let a = jobs.len();
                    jobs.push(Job {
                        window: inst.pane.clone(),
                        what: Cap::History { rows, with_top: false },
                    });
                    jobs.push(Job {
                        window: inst.pane.clone(),
                        what: Cap::History { rows, with_top: true },
                    });
                    (a, a + 1)
                });
                let screen = jobs.len();
                jobs.push(Job { window: inst.pane.clone(), what: Cap::Screen });
                plan.push(TermJobs { key, alt: inst.alt, history, screen });
            }
        }
        self.forget_missing(&live);

        let captured = self.run_jobs(t, &jobs);
        let got = |i: usize| captured.get(i).and_then(|o: &Option<String>| o.clone());
        for tj in &plan {
            let Some(st) = self.terms.get_mut(&tj.key) else { continue };
            if let Some((a, b)) = tj.history {
                if let (Some(rows), Some(with_top)) = (got(a), got(b)) {
                    st.absorb_history(&rows, &with_top);
                }
            }
            // Before the screen is looked at, not after. `take_screen` decides
            // whether a capture is a full-screen program's frame from `st.alt`,
            // so updating that in a later pass left it a whole tick behind — and
            // the flag is wrong on exactly the two ticks that matter. Measured:
            // the *only* frame recorded for a `vim` session was the shell prompt
            // drawn after it quit, and vim's own screen was never stored at all.
            st.sync_alt(tj.alt, &tj.key.0, tj.key.1);
            if let Some(screen) = got(tj.screen) {
                st.take_screen(&tj.key.0, tj.key.1, &screen);
            }
        }

        // Second pass: the per-instance facts that need no capture.
        for p in projects {
            for inst in p.instances.iter().filter(|i| i.is_shell()) {
                let Some(st) = self.terms.get_mut(&(p.state_dir.clone(), inst.id)) else { continue };
                st.history = inst.history;
                st.sync_exit(inst.dead);
                if let Some(cmd) = st.due_seed(inst) {
                    let mut bytes = cmd.into_bytes();
                    bytes.push(b'\r');
                    let _ = t.send_bytes(&inst.pane, &bytes);
                }
            }
            self.publish(p);
        }
    }

    /// Run every capture, batched, falling back to individual calls for whatever
    /// the batch did not return. A tmux command sequence **stops at the first
    /// failure**, and a window can close between the scan and the capture, so a
    /// short result is an ordinary event — but it must not blank the terminals
    /// that come after the one that went away.
    fn run_jobs(&self, t: &Tmux, jobs: &[Job]) -> Vec<Option<String>> {
        let targets: Vec<(String, Cap)> =
            jobs.iter().map(|j| (j.window.clone(), j.what)).collect();
        let batched = t.capture_batch(&targets);
        jobs.iter()
            .enumerate()
            .map(|(i, j)| match batched.get(i) {
                Some(s) => Some(s.clone()),
                None => t.capture(&j.window, j.what).ok(),
            })
            .collect()
    }

    /// Write `terminals/index` and `terminals/meta`.
    fn publish(&mut self, p: &Project) {
        let dir = p.state_dir.join("terminals");
        let _ = std::fs::create_dir_all(&dir);

        // `<id>\t<running|exited>\t<label>` — parsed `splitn(3, '\t')` with the
        // label last, which is why anything else has to go in `meta` instead.
        let mut index = String::new();
        for inst in p.instances.iter().filter(|i| i.is_shell()) {
            let state = if inst.dead { "exited" } else { "running" };
            let label = crate::core::label_of(&inst.window_name).unwrap_or_default();
            index.push_str(&format!("{}\t{}\t{}\n", inst.id, state, label.replace(['\t', '\n'], " ")));
        }
        if self.index.get(&p.state_dir) != Some(&index) {
            let _ = std::fs::write(dir.join("index"), &index);
            self.index.insert(p.state_dir.clone(), index);
        }

        let mut map = serde_json::Map::new();
        for inst in p.instances.iter().filter(|i| i.is_shell() && !i.dead) {
            let Some(st) = self.terms.get(&(p.state_dir.clone(), inst.id)) else { continue };
            let mut entry = serde_json::Map::new();
            entry.insert(
                "command_running".into(),
                serde_json::json!(inst.current_command != st.shell_name),
            );
            if !inst.current_path.is_empty() {
                entry.insert("cwd".into(), serde_json::json!(inst.current_path));
            }
            map.insert(inst.id.to_string(), serde_json::Value::Object(entry));
        }
        let meta = serde_json::Value::Object(map).to_string();
        if self.meta.get(&p.state_dir) != Some(&meta) {
            let _ = std::fs::write(dir.join("meta"), &meta);
            self.meta.insert(p.state_dir.clone(), meta);
        }
    }
}

impl TermState {
    /// Absorb the rows that have scrolled off, and write whatever is genuinely new.
    ///
    /// `rows` is a **deliberately over-sized** window ending at the newest
    /// scrolled-off row; `with_top` is the same window plus the first visible row.
    /// The second one answers exactly one question — does the last scrolled-off
    /// row *continue* onto the screen? `-J` joins wrapped rows only within a
    /// single capture, so a line longer than the screen straddles the boundary and
    /// would be cut in half at whichever tick the boundary fell on (measured: a
    /// 300-character line reached the log as 180). A line that continues is
    /// written **without a terminator**, so its continuation lands on it when that
    /// row scrolls off later. This is `vtgrid::scroll_up`'s `row_full` rule.
    ///
    /// The window is over-sized because **row arithmetic cannot be trusted**. tmux
    /// addresses history relative to its newest row, and the pane keeps scrolling
    /// between the scan that measured `history_size` and the capture a few
    /// milliseconds later — so a window of exactly `delta` rows silently slides.
    /// Measured on a paced loop: one line lost and one duplicated per drift. So
    /// the window reaches back well past where we stopped, and alignment comes
    /// from **content**: the log already ends with whatever this window starts
    /// with, and only the text past that overlap is new. Self-correcting, and
    /// indifferent to how far the pane drifted.
    fn absorb_history(&mut self, rows: &str, with_top: &str) {
        let text = render_rows(rows, with_top);
        if text.is_empty() {
            return;
        }
        let tail = self.log.tail(OVERLAP_BYTES);
        let seen = overlap(&tail, &text);
        // `seen == 0` with a non-empty log means the window did not reach back to
        // where we stopped — more scrolled in one tick than the slack covers. Emit
        // anyway: a duplicated line is visibly odd, a dropped one is invisible.
        let fresh = &text[seen..];
        if fresh.is_empty() {
            return;
        }
        self.last_out_activity();
        let _ = self.log.append(fresh);
    }

    /// The visible screen. Written only when it actually changed, because a
    /// terminal sitting at a prompt would otherwise be rewritten five times a
    /// second forever.
    fn take_screen(&mut self, state_dir: &Path, id: usize, text: &str) {
        let screen = text.trim_end_matches('\n');
        let fp = fingerprint(screen);
        if self.alt {
            // The alternate screen is the *only* record a full-screen program
            // leaves, so keep the newest one whether or not it is due to be
            // appended. This is what makes the exit flush possible.
            if fp != self.screen_fp {
                self.pending_frame = Some(screen.to_string());
            }
            // Due only on the interval — but gated on there being something held,
            // NOT on the screen having changed this tick. A program that paints
            // once and then sits still (a file open in `vim`) changes `fp` exactly
            // once, and tying the flush to that change meant its one real frame
            // was held forever while the blank startup screen was all that got
            // recorded. Observed: one frame, and not the one with the text in it.
            if self.pending_frame.is_some()
                && now_ms().saturating_sub(self.last_frame_ms) >= FRAME_MIN_INTERVAL_MS
            {
                self.flush_frame(state_dir, id);
            }
        }
        if fp == self.screen_fp {
            return;
        }
        self.screen_fp = fp;
        self.last_out_activity();
        let dir = state_dir.join("terminals");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{id}.screen")), screen);
    }

    /// Append the held snapshot to `<id>.frames`.
    fn flush_frame(&mut self, state_dir: &Path, id: usize) {
        let Some(text) = self.pending_frame.take() else { return };
        if text.trim().is_empty() {
            return;
        }
        let at = now_ms();
        let record = format!("{FRAME_MARK} {at} {}\n{text}", text.len());
        let path = state_dir.join("terminals").join(format!("{id}.frames"));
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            if f.write_all(record.as_bytes()).is_ok() {
                self.frames_len += record.len() as u64;
            }
        }
        self.last_frame_ms = at;
        if self.frames_len > MAX_FRAMES {
            trim_frames(&path);
            self.frames_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
    }

    /// React to the pane entering or leaving the alternate screen.
    fn sync_alt(&mut self, alt: bool, state_dir: &Path, id: usize) {
        if alt == self.alt {
            return;
        }
        self.alt = alt;
        if alt {
            // Entering: say so in the transcript, or `new_output` goes empty with
            // nothing to explain why.
            let _ = self.log.append(&format!("{ALT_SCREEN_NOTE}\n"));
            self.pending_frame = None;
        } else {
            // Leaving: this is the frame worth having, and tmux has already
            // discarded the alternate screen (measured), so the held capture is
            // the only copy that exists.
            self.flush_frame(state_dir, id);
        }
    }

    fn sync_exit(&mut self, dead: bool) {
        if dead == self.log.exited {
            return;
        }
        self.log.exited = dead;
        let _ = self.log.write_header();
    }

    /// Stamp activity and refresh the header, at most once per changed tick.
    fn last_out_activity(&mut self) {
        self.log.last_out_ms = now_ms();
        let _ = self.log.write_header();
    }

    /// The seeded command, once the shell looks ready — first output, then a
    /// settled screen. Mirrors the app's "wait for output plus a short quiet",
    /// with a backstop so a shell that prints no prompt at all still runs it.
    fn due_seed(&mut self, inst: &crate::core::Instance) -> Option<String> {
        self.seed.as_ref()?;
        let painted = self.screen_fp != 0;
        let idle = inst.current_command == self.shell_name;
        if (painted && idle) || now_ms() >= self.seed_deadline {
            return self.seed.take();
        }
        None
    }
}

/// Create an empty, valid transcript for a terminal that has just been opened.
///
/// `mcp::LogView::open` treats a missing file as "no such terminal", so this is
/// what makes a read immediately after an open return an empty transcript rather
/// than an error naming a terminal that demonstrably exists. Called from the
/// request handler, not the poll loop, because the reply is what the caller races.
pub fn create_log(state_dir: &Path, id: usize) {
    let path = state_dir.join("terminals").join(format!("{id}.log"));
    let _ = Log::open(&path);
}

/// Turn a captured window into the exact text the transcript would hold for it,
/// so it can be compared against what is already there.
fn render_rows(rows: &str, with_top: &str) -> String {
    let body = rows.trim_end_matches('\n');
    if body.is_empty() {
        return String::new();
    }
    let continues = with_top.trim_end_matches('\n').lines().count() == body.lines().count();
    let lines: Vec<&str> = body.lines().collect();
    let mut out = String::with_capacity(body.len() + lines.len());
    for (i, line) in lines.iter().enumerate() {
        if i + 1 == lines.len() && continues {
            // A wrapped row is full by definition, so its trailing spaces are
            // content and no newline may end it.
            out.push_str(line);
        } else {
            // `-J` preserves trailing spaces, which turns every blank row into a
            // full width of them — 120 of them per cleared row on this terminal.
            // `vtgrid::row_text` trims; so does this.
            out.push_str(line.trim_end_matches(' '));
            out.push('\n');
        }
    }
    out
}

/// How many bytes of `text` the log already ends with: the largest `k` where
/// `tail` ends with `text[..k]`.
///
/// The window overlaps what we already wrote, so this is the join. Searching from
/// the longest match down means the common case — the whole overlap region
/// matches — exits on the first iteration. Repetitive output is handled correctly
/// by taking the **longest** overlap: for a tail of three identical lines and a
/// window of four, the longest match consumes three and leaves exactly one new.
fn overlap(tail: &str, text: &str) -> usize {
    let mut k = tail.len().min(text.len());
    while k > 0 {
        if text.is_char_boundary(k) && tail.as_bytes().ends_with(&text.as_bytes()[..k]) {
            return k;
        }
        k -= 1;
    }
    0
}

/// How many scrolled-off rows to capture, given what we had consumed and what the
/// pane reports now.
///
/// The subtraction is the easy half. The case that matters is `now < consumed`:
/// `clear` (and anything else emitting `ESC[3J`) **discards tmux's scrollback**,
/// so `history_size` drops back toward zero and starts climbing again. A
/// `saturating_sub` reads that as "nothing new" and then adopts the smaller
/// number — losing every row that scrolled during that tick, silently. Measured:
/// `clear; seq 1 20000` arrived starting at 27, and a `clear`ed wrapped line lost
/// its first 120 characters. After a reset the whole surviving history is new.
fn history_delta(consumed: u64, now: u64) -> u64 {
    if now < consumed {
        now
    } else {
        now - consumed
    }
}

/// Keep the newest `KEEP_FRAMES` bytes, cut at a **record** boundary — a frame
/// log trimmed mid-payload would leave a length prefix promising bytes that are
/// no longer there, and `mcp::read_frames` would stop at that record and hide
/// every good frame after it.
fn trim_frames(path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let mut rest = raw.as_str();
    let mut offset = 0usize;
    while raw.len() - offset > KEEP_FRAMES as usize {
        let Some(nl) = rest.find('\n') else { break };
        let header = &rest[..nl];
        let Some(len) = header.rsplit(' ').next().and_then(|l| l.parse::<usize>().ok()) else {
            break;
        };
        let body = &rest[nl + 1..];
        if body.len() < len {
            break;
        }
        let consumed = nl + 1 + len;
        offset += consumed;
        rest = &body[len..];
    }
    let _ = std::fs::write(path, &raw[offset..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mpx-rec-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn state(path: &Path) -> TermState {
        TermState {
            log: Log::open(path).unwrap(),
            history: 0,
            alt: false,
            screen_fp: 0,
            pending_frame: None,
            last_frame_ms: 0,
            frames_len: 0,
            seed: None,
            seed_deadline: 0,
            shell_name: "zsh".into(),
        }
    }

    fn data_of(path: &Path) -> String {
        let raw = std::fs::read(path).unwrap();
        String::from_utf8_lossy(&raw[termlog::HEADER_LEN..]).into_owned()
    }

    /// The bug this exists for: a line longer than the screen scrolls off over
    /// **several ticks**, and `-J` joins only within one capture. Terminating the
    /// first half turned a measured 300-character line into 180 characters and a
    /// stray fragment. A row that continues gets no newline, so the continuation
    /// lands on it — `vtgrid::scroll_up`'s rule.
    #[test]
    fn a_line_still_being_written_is_left_unterminated_until_it_finishes() {
        let d = tmp("wrap");
        let path = d.join("w.log");
        let mut st = state(&path);
        // Tick 1: the first rows scrolled off, the rest is still on screen — so
        // the `-E 0` capture joins it and both have one line.
        st.absorb_history("WWWfirst", "WWWfirstAndTheStillVisibleTail");
        // Tick 2: the tail scrolls off, and a separate line follows it on screen.
        st.absorb_history("WWWtail", "WWWtail\nsomething else");
        assert_eq!(data_of(&path), "WWWfirstWWWtail\n");
    }

    /// `-J` preserves trailing spaces, so every blank row arrives as a full width
    /// of them — a 120-column terminal was writing 120 spaces into the transcript
    /// for each cleared row.
    #[test]
    fn trailing_padding_is_trimmed_from_finished_lines() {
        let d = tmp("pad");
        let path = d.join("p.log");
        let mut st = state(&path);
        st.absorb_history("hello        \n            ", "hello\n\nnext");
        assert_eq!(data_of(&path), "hello\n\n");
    }

    /// The capture window deliberately re-reads rows already recorded, because
    /// the pane keeps scrolling between measuring `history_size` and capturing.
    /// Measured before this: `F_3` lost and `F_11` written twice on a paced loop.
    /// Overlapping windows must produce each line exactly once.
    #[test]
    fn a_window_that_re_reads_old_rows_writes_each_line_once() {
        let d = tmp("drift");
        let path = d.join("d.log");
        let mut st = state(&path);
        st.absorb_history("F_1\nF_2\nF_3", "F_1\nF_2\nF_3\nvisible");
        // Next tick's window reaches back past F_1 — every row here but the last
        // two is already recorded.
        st.absorb_history("F_1\nF_2\nF_3\nF_4\nF_5", "F_1\nF_2\nF_3\nF_4\nF_5\nvisible");
        assert_eq!(data_of(&path), "F_1\nF_2\nF_3\nF_4\nF_5\n");
    }

    /// The overlap must take the LONGEST match, or repeated identical lines —
    /// a progress log, a stack of blanks — re-align to the wrong one and the
    /// transcript grows a line every tick.
    #[test]
    fn repeated_identical_lines_align_on_the_longest_overlap() {
        let d = tmp("rep");
        let path = d.join("r.log");
        let mut st = state(&path);
        st.absorb_history("same\nsame\nsame", "same\nsame\nsame\nx");
        st.absorb_history("same\nsame\nsame\nsame", "same\nsame\nsame\nsame\nx");
        assert_eq!(data_of(&path), "same\nsame\nsame\nsame\n");
    }

    /// A window carrying nothing new must write nothing, or an idle terminal
    /// grows its transcript five times a second.
    #[test]
    fn a_window_with_nothing_new_writes_nothing() {
        let d = tmp("idle");
        let path = d.join("i.log");
        let mut st = state(&path);
        st.absorb_history("a\nb", "a\nb\nvisible");
        let before = data_of(&path);
        st.absorb_history("a\nb", "a\nb\nvisible");
        assert_eq!(data_of(&path), before);
    }

    #[test]
    fn the_overlap_is_the_longest_prefix_the_tail_ends_with() {
        assert_eq!(overlap("abc", "bcd"), 2);
        assert_eq!(overlap("", "abc"), 0, "an empty log overlaps nothing");
        assert_eq!(overlap("abc", "xyz"), 0, "no anchor: caller emits it all");
        assert_eq!(overlap("abc", "abc"), 3, "nothing new at all");
    }

    /// `clear` discards tmux's scrollback, so `history_size` goes *backwards*.
    /// Treating that as "nothing new" loses everything that scrolled in the same
    /// tick — measured as `seq 1 20000` reaching the transcript from 27.
    #[test]
    fn a_cleared_scrollback_is_re_read_rather_than_treated_as_no_output() {
        assert_eq!(history_delta(100, 140), 40, "the ordinary case");
        assert_eq!(history_delta(100, 100), 0, "nothing scrolled");
        assert_eq!(history_delta(20_000, 26), 26, "reset: everything left is new");
        assert_eq!(history_delta(20_000, 0), 0, "reset with nothing yet to read");
    }

    /// The header this writes is parsed by `mcp.rs` in another process. If the
    /// two ever disagree every terminal read fails with "no term#N".
    #[test]
    fn a_fresh_log_is_readable_by_the_shared_parser() {
        let d = tmp("fresh");
        let p = d.join("1.log");
        let mut log = Log::open(&p).unwrap();
        log.append("hello\n").unwrap();
        let raw = std::fs::read(&p).unwrap();
        let h = termlog::parse_header(&raw).expect("mcp.rs must be able to parse this");
        assert_eq!(h.base, 0);
        assert!(!h.exited);
        assert_eq!(&raw[termlog::HEADER_LEN..], b"hello\n");
    }

    /// Reopening must not reset `base`: readers hold logical offsets into the
    /// old numbering, and starting over would replay the whole transcript to
    /// every one of them.
    #[test]
    fn reopening_a_log_keeps_its_offsets() {
        let d = tmp("reopen");
        let p = d.join("2.log");
        {
            let mut log = Log::open(&p).unwrap();
            log.append("one\n").unwrap();
            log.base = 900; // as if a trim had happened
            log.write_header().unwrap();
        }
        let log = Log::open(&p).unwrap();
        assert_eq!(log.base, 900);
        assert_eq!(log.len, 4);
    }

    /// A trim must advance `base` by exactly what it dropped, or every reader's
    /// saved position silently maps to the wrong bytes.
    #[test]
    fn a_trim_cuts_at_a_line_and_advances_base_by_what_it_dropped() {
        let d = tmp("trim");
        let p = d.join("3.log");
        let mut log = Log::open(&p).unwrap();
        let line = "x".repeat(99) + "\n";
        // A fixed count, not `while len <= MAX_LOG`: `append` trims itself, so
        // that condition can never go false and the loop runs forever.
        let n = MAX_LOG / line.len() as u64 + 2;
        for _ in 0..n {
            log.append(&line).unwrap();
        }
        let written = n * line.len() as u64;
        assert!(log.base > 0, "it must actually have trimmed");
        assert_eq!(log.base + log.len, written, "no byte may be lost or invented");
        assert!(log.len <= KEEP_LOG + line.len() as u64);

        let raw = std::fs::read(&p).unwrap();
        let h = termlog::parse_header(&raw).unwrap();
        assert_eq!(h.base, log.base);
        let data = &raw[termlog::HEADER_LEN..];
        assert!(data.starts_with(b"xxx"), "the transcript must not begin mid-line");
    }

    /// The frame format is `mcp::read_frames`' to define; this asserts the shape
    /// it walks — marker, ms, byte length, then exactly that many bytes.
    #[test]
    fn a_frame_record_is_length_prefixed() {
        let d = tmp("frames");
        let mut st = TermState {
            log: Log::open(&d.join("t.log")).unwrap(),
            history: 0,
            alt: true,
            screen_fp: 0,
            pending_frame: Some("VIM SCREEN".into()),
            last_frame_ms: 0,
            frames_len: 0,
            seed: None,
            seed_deadline: 0,
            shell_name: "zsh".into(),
        };
        st.flush_frame(&d, 4);
        let raw = std::fs::read_to_string(d.join("terminals/4.frames")).unwrap();
        let (header, body) = raw.split_once('\n').unwrap();
        assert!(header.starts_with(FRAME_MARK));
        let len: usize = header.rsplit(' ').next().unwrap().parse().unwrap();
        assert_eq!(&body[..len], "VIM SCREEN");
    }

    /// Leaving the alternate screen must flush the held snapshot: tmux has
    /// already thrown the alternate screen away by then (measured), so if this
    /// does not write it, the last thing `vim` showed is gone for good.
    #[test]
    fn leaving_the_alternate_screen_flushes_the_last_frame() {
        let d = tmp("altexit");
        let mut st = TermState {
            log: Log::open(&d.join("t.log")).unwrap(),
            history: 0,
            alt: true,
            screen_fp: 1,
            pending_frame: Some("the last thing vim drew".into()),
            // Recent enough that the interval throttle would have skipped it.
            last_frame_ms: now_ms(),
            frames_len: 0,
            seed: None,
            seed_deadline: 0,
            shell_name: "zsh".into(),
        };
        st.sync_alt(false, &d, 7);
        let raw = std::fs::read_to_string(d.join("terminals/7.frames")).unwrap();
        assert!(raw.contains("the last thing vim drew"), "throttle must not eat the exit frame");
    }

    /// A program that paints once and then sits still — a file open in `vim` —
    /// changes the screen exactly once. Tying the flush to that change held its
    /// only real frame forever and recorded just the blank startup screen.
    #[test]
    fn a_settled_full_screen_program_still_gets_its_frame_recorded() {
        let d = tmp("settled");
        let mut st = state(&d.join("s.log"));
        st.alt = true;
        // Tick 1: the blank screen vim shows before it paints.
        st.take_screen(&d, 9, "");
        // Tick 2: it paints. Too soon for the interval, so this is only held.
        st.take_screen(&d, 9, "the file's contents");
        // Later ticks: the screen never changes again.
        st.last_frame_ms = now_ms() - FRAME_MIN_INTERVAL_MS - 1;
        st.take_screen(&d, 9, "the file's contents");
        let raw = std::fs::read_to_string(d.join("terminals/9.frames")).unwrap();
        assert!(raw.contains("the file's contents"), "the painted screen must reach the frame log");
    }

    /// Entering it leaves a note, so an empty `new_output` is explained rather
    /// than merely empty.
    #[test]
    fn entering_the_alternate_screen_is_recorded_in_the_transcript() {
        let d = tmp("altenter");
        let path = d.join("t.log");
        let mut st = TermState {
            log: Log::open(&path).unwrap(),
            history: 0,
            alt: false,
            screen_fp: 0,
            pending_frame: None,
            last_frame_ms: 0,
            frames_len: 0,
            seed: None,
            seed_deadline: 0,
            shell_name: "zsh".into(),
        };
        st.sync_alt(true, &d, 1);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("full-screen program"));
    }

    /// A malformed tail must not cost the good records before it.
    #[test]
    fn trimming_frames_cuts_only_at_a_record_boundary() {
        let d = tmp("ftrim");
        let p = d.join("f.frames");
        let mut raw = String::new();
        let body = "y".repeat(4096);
        for i in 0..300 {
            raw.push_str(&format!("{FRAME_MARK} {i} {}\n{body}", body.len()));
        }
        std::fs::write(&p, &raw).unwrap();
        trim_frames(&p);
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with(FRAME_MARK), "must start at a record header");
        assert!((after.len() as u64) <= KEEP_FRAMES + body.len() as u64 + 32);
    }

    /// The fingerprint is written by this process and compared by `mcp.rs` in
    /// another, so it is a wire format. These are its own values, pinned.
    #[test]
    fn the_screen_fingerprint_is_stable_fnv1a() {
        assert_eq!(fingerprint(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
