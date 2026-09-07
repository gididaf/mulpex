//! Typed wrappers over the `tmux` binary.
//!
//! Every call is an argv vector handed to `Command`, never a shell string —
//! the project directory, an instance name and a task all reach tmux as data,
//! and one `;` in a path would otherwise become a second tmux command.
//!
//! Mulpex runs its **own tmux server** on the `mulpex` socket with a config it
//! ships (`assets/mulpex.tmux.conf`). The user's `~/.tmux.conf` is deliberately
//! not sourced, so `mpx` behaves identically on this Mac and on every server it
//! is installed on, with nothing to keep in sync remotely.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// The socket name. `tmux -L mulpex` is a wholly separate server from the user's
/// own `tmux`, so `tmux ls` and their sessions are untouched.
pub const SOCKET: &str = "mulpex";

/// `-e` on `new-window` and `display-popup` arrived in tmux 3.2, and we depend on
/// both. Debian 11 still ships 3.1c, so this is a real check, not a formality.
pub const MIN_VERSION: (u32, u32) = (3, 2);

pub struct Tmux {
    conf: std::path::PathBuf,
}

impl Tmux {
    pub fn new(conf: std::path::PathBuf) -> Self {
        Tmux { conf }
    }

    fn base(&self) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-L").arg(SOCKET);
        c.arg("-f").arg(&self.conf);
        c
    }

    /// Run a tmux command, returning trimmed stdout. A non-zero exit carries
    /// tmux's own stderr, which is where `command too long`, `no server running`
    /// and `can't find window` are phrased better than anything we would write.
    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.base();
        cmd.args(args);
        let out = cmd.output().context("running tmux (is it installed?)")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "tmux {}: {}",
                out.status.code().map(|c| c.to_string()).unwrap_or_default(),
                if err.is_empty() { "failed".into() } else { err }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// True when the named session exists. Distinguished from an error because
    /// "no server running" is the normal first-launch state, not a failure.
    pub fn has_session(&self, name: &str) -> bool {
        self.run(["has-session", "-t", &format!("={name}")]).is_ok()
    }

    /// Create a detached session whose first window is a placeholder we replace.
    /// `-x/-y` matter because a detached server otherwise assumes 80x24 and the
    /// first client attach would resize every pane at once.
    pub fn new_session(&self, name: &str, cwd: &Path, cols: u16, rows: u16) -> Result<()> {
        self.run([
            "new-session".as_ref(),
            "-d".as_ref(),
            "-s".as_ref(),
            name.as_ref(),
            "-c".as_ref(),
            cwd.as_os_str(),
            "-x".as_ref(),
            cols.to_string().as_ref(),
            "-y".as_ref(),
            rows.to_string().as_ref(),
        ] as [&OsStr; 10])?;
        Ok(())
    }

    /// Spawn a window running `argv`. Returns tmux's window id (`@N`), which is
    /// stable across renames and index shuffles — unlike `session:index`.
    ///
    /// `-d` so a `hub_spawn` child never steals the user's focus.
    pub fn new_window(
        &self,
        session: &str,
        name: &str,
        cwd: &Path,
        env: &[(String, String)],
        argv: &[String],
    ) -> Result<String> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "new-window".into(),
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{window_id}".into(),
            // Append after the last window. Without `-a {end}`, tmux picks the
            // **lowest free index** — and `up()` frees index 0 when it kills the
            // placeholder, so claude#2 was created *in front of* claude#1 and
            // claude#3 after it. Window order then had nothing to do with instance
            // order, which is what made `next-window` feel reversed (2026-09-06).
            // Instance ids only ever increase, so appending keeps the two agreeing.
            "-a".into(),
            "-t".into(),
            format!("={session}:{{end}}").into(),
            "-n".into(),
            name.into(),
            "-c".into(),
            cwd.as_os_str().to_os_string(),
        ];
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}").into());
        }
        // `--` separates tmux's own options from the command. Passing argv as
        // separate arguments (not one string) is what makes tmux execvp it
        // rather than hand it to a shell.
        args.push("--".into());
        for a in argv {
            args.push(a.into());
        }
        self.run(args)
    }

    /// Rename a window. This is the instance list: the window bar *is* the
    /// sidebar, so a rename is how a status change or a `hub_set_name` becomes
    /// visible to a user who is looking at a different window.
    pub fn rename_window(&self, window_id: &str, name: &str) -> Result<()> {
        self.run(["rename-window", "-t", window_id, name]).map(|_| ())
    }

    /// Tag a window or session with one of our `@mpx_*` user options.
    ///
    /// These are what make tmux the state store: they survive renames, index
    /// shuffles and — the point — a daemon restart, so nothing has to be
    /// reconstructed from a side file that could disagree with reality.
    pub fn set_user_option(&self, target: &str, window: bool, key: &str, value: &str) -> Result<()> {
        let mut args = vec!["set-option"];
        if window {
            args.push("-w");
        }
        args.extend_from_slice(&["-t", target, key, value]);
        self.run(args).map(|_| ())
    }

    /// Every pane's controlling terminal in one session. Used by teardown, which
    /// must read them *before* killing anything.
    pub fn session_ttys(&self, session: &str) -> Result<Vec<String>> {
        let out = self.run(["list-panes", "-s", "-t", &format!("={session}"), "-F", "#{pane_tty}"])?;
        Ok(out.lines().filter(|l| !l.trim().is_empty()).map(|s| s.to_string()).collect())
    }

    pub fn kill_window(&self, window_id: &str) -> Result<()> {
        self.run(["kill-window", "-t", window_id]).map(|_| ())
    }

    /// Put the sidebar down the left of a window, at a fixed width.
    ///
    /// `-b` puts it *before* the existing pane, so the instance keeps the right
    /// hand side and the layout reads the way the desktop app does. The instance
    /// pane is resized once, at startup, before it has painted anything worth
    /// keeping — tmux owns the emulation, so a resize here is an ordinary reflow
    /// and not the cross-emulator corruption the desktop app has to avoid.
    pub fn split_sidebar(&self, window_id: &str, cwd: &Path, argv: &[String]) -> Result<String> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "split-window".into(),
            "-h".into(),
            "-b".into(),
            "-d".into(),
            "-l".into(),
            crate::sidebar::WIDTH.to_string().into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
            "-t".into(),
            window_id.into(),
            "-c".into(),
            cwd.as_os_str().to_os_string(),
            "--".into(),
        ];
        for a in argv {
            args.push(a.into());
        }
        self.run(args)
    }

    /// Tag a single pane. Distinct from `set_user_option`'s window scope: a
    /// window option is inherited by every pane in it, which is exactly what the
    /// sidebar must not be.
    pub fn set_pane_option(&self, pane: &str, key: &str, value: &str) -> Result<()> {
        self.run(["set-option", "-p", "-t", pane, key, value]).map(|_| ())
    }

    pub fn select_window(&self, window_id: &str) -> Result<()> {
        self.run(["select-window", "-t", window_id]).map(|_| ())
    }

    pub fn select_pane(&self, target: &str) -> Result<()> {
        self.run(["select-pane", "-t", target]).map(|_| ())
    }

    /// Go to a window and land on one specific pane, in **one** tmux invocation.
    ///
    /// A key binding pays a process spawn per tmux call, and this one runs on every
    /// press of Ctrl-] — `;` as its own argument is tmux's command separator, so
    /// the pair costs what a single command costs.
    pub fn focus(&self, window: &str, pane: &str) -> Result<()> {
        self.run(["select-window", "-t", window, ";", "select-pane", "-t", pane])
            .map(|_| ())
    }

    /// Put a line on the status line of every attached client.
    ///
    /// The only place a key binding can say anything: it runs under `run-shell -b`,
    /// where stdout and stderr are discarded and a failure is indistinguishable
    /// from a key that is not bound.
    pub fn message(&self, text: &str) -> Result<()> {
        self.run(["display-message", text]).map(|_| ())
    }

    /// Re-read the shipped config into a server that is already running.
    ///
    /// `-f` is only read when the server *starts*, and this server outlives every
    /// client by design — so an `mpx` that has been updated underneath a live
    /// session would otherwise keep the old key bindings until the last project was
    /// torn down, which is a bug report ("the new key does nothing") with no visible
    /// cause. Sourcing is idempotent: `set` and `bind` overwrite.
    pub fn source_conf(&self) -> Result<()> {
        let conf = self.conf.clone();
        self.run(["source-file".as_ref(), conf.as_os_str()] as [&OsStr; 2])
            .map(|_| ())
    }

    /// Move the attached client to another project.
    ///
    /// `arg` is `-n`/`-p` (next/previous, wrapping) or `-t <session>`. No `-c`:
    /// run from inside a pane, tmux resolves the client from `$TMUX` and falls
    /// back to the best client for the current session. Measured working from a
    /// pane process on 2026-09-06 — the sidebar is one, and passing a client tty
    /// it would have to look up first is a lookup that can go stale.
    pub fn switch_client(&self, args: &[&str]) -> Result<()> {
        let mut v = vec!["switch-client"];
        v.extend_from_slice(args);
        self.run(v).map(|_| ())
    }

    /// Float a popup over the panes and block until it closes.
    ///
    /// `-E` closes it when the command exits. It **resizes nothing**, which is the
    /// entire reason overlays are popups here and not panes: a resize is the one
    /// operation that can corrupt a terminal for good (`docs/rendering.md`).
    ///
    /// `-d` pins the working directory, because the command inside is an `mpx`
    /// subcommand that resolves its project from the cwd — inheriting whatever
    /// directory the pane happened to be in would read the wrong project's feed.
    pub fn display_popup(&self, cwd: &Path, w: &str, h: &str, shell_cmd: &str) -> Result<()> {
        let args: [&OsStr; 9] = [
            "display-popup".as_ref(),
            "-E".as_ref(),
            "-d".as_ref(),
            cwd.as_os_str(),
            "-w".as_ref(),
            w.as_ref(),
            "-h".as_ref(),
            h.as_ref(),
            shell_cmd.as_ref(),
        ];
        self.run(args).map(|_| ())
    }

    /// Sessions on this server, in creation order — the projects, and the order
    /// the tabs along the top render in.
    pub fn sessions(&self) -> Vec<String> {
        self.run(["list-sessions", "-F", "#{session_name}"])
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Replace a pane's process **in place**, keeping the pane and its window.
    ///
    /// This is what makes restart-in-place (⌘⇧R's equivalent) a one-liner rather
    /// than a kill-and-respawn: the window id, its number, its name, its position
    /// in the bar and every `@mpx_*` option survive, so the instance comes back as
    /// the same `claude#N` and not as a new row at the end. `-k` kills whatever is
    /// running first, and also revives a pane that `remain-on-exit` is holding
    /// dead — a claude that crashed is restartable, which is when you most want it.
    ///
    /// **`pane` must be a pane id, never a window id.** A window target resolves to
    /// that window's *active* pane — so a restart triggered from the sidebar, where
    /// the sidebar is the active pane, relaunched `claude` **into the sidebar**
    /// (observed 2026-09-06: the instance list replaced by a claude, the real claude
    /// still running untouched beside it).
    pub fn respawn_pane(&self, pane: &str, cwd: &Path, argv: &[String]) -> Result<()> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "respawn-pane".into(),
            "-k".into(),
            "-t".into(),
            pane.into(),
            "-c".into(),
            cwd.as_os_str().to_os_string(),
        ];
        args.push("--".into());
        for a in argv {
            args.push(a.into());
        }
        self.run(args).map(|_| ())
    }

    /// Read one of our `@mpx_*` options back. Absent reads as empty, which is the
    /// honest answer for a window tagged by an older build.
    pub fn user_option(&self, target: &str, key: &str) -> String {
        self.run(["display-message", "-p", "-t", target, &format!("#{{{key}}}")])
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// Kill one session and everything in it. **Always prefer this to
    /// `kill-server`**: the server hosts every open project, so killing it takes
    /// other projects' running claudes with it — work that belongs to a different
    /// directory and a different person's attention.
    pub fn kill_session(&self, name: &str) -> Result<()> {
        self.run(["kill-session", "-t", &format!("={name}")]).map(|_| ())
    }

    /// How many sessions the server still hosts, so a teardown can tell whether
    /// it just removed the last project.
    pub fn session_count(&self) -> usize {
        self.run(["list-sessions", "-F", "#{session_name}"])
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// Query one format string against a target, e.g. `#{pane_dead}`.
    ///
    /// Unused until Phase 3's poll loop; kept here because the three query
    /// wrappers below are the whole of what replaces `pty.rs`'s liveness and
    /// `vtgrid.rs`'s screen reads, and splitting them across phases would hide
    /// that.
    #[allow(dead_code)]
    pub fn display(&self, target: &str, format: &str) -> Result<String> {
        self.run(["display-message", "-p", "-t", target, format])
    }

    /// A format evaluated against the **current client**, with no target at all.
    ///
    /// Distinct from `display` on purpose: a `-t` is a *pane* target, and
    /// `#{client_*}` asks about the terminal a person is looking at. Passing a pane
    /// there is the shape that has already gone wrong twice here — a target tmux
    /// accepts and resolves to something other than what was meant.
    pub fn display_here(&self, format: &str) -> Result<String> {
        self.run(["display-message", "-p", format])
    }

    /// One `list-panes` over the whole server; the poll loop needs every pane's
    /// state per tick and N separate queries would be N round trips.
    pub fn list_panes_all(&self, format: &str) -> Result<Vec<String>> {
        let out = self.run(["list-panes", "-a", "-F", format])?;
        Ok(out.lines().map(|l| l.to_string()).collect())
    }

    /// Type raw bytes into a pane. `-H` takes hex pairs, so the bytes arrive
    /// exactly as given — no shell quoting, no key-name translation, and a `;`
    /// or a newline in the text cannot become a tmux command.
    pub fn send_bytes(&self, target: &str, data: &[u8]) -> Result<()> {
        let mut args: Vec<String> =
            vec!["send-keys".into(), "-t".into(), target.into(), "-H".into()];
        for b in data {
            args.push(format!("{b:02x}"));
        }
        self.run(args).map(|_| ())
    }

    /// Capture a pane's visible screen, or a slice of its scrollback.
    ///
    /// `-J` is **mandatory**: without it a line the terminal wrapped comes back
    /// as several 80-column fragments (measured — a 300-character line arrived as
    /// 80/80/80/60), and a reader would see broken output for every long line.
    pub fn capture(&self, target: &str, what: Cap) -> Result<String> {
        self.run_raw(&capture_argv(target, what))
    }

    /// Run several capture-like commands in one tmux invocation.
    ///
    /// Worth the machinery: 20 separate `capture-pane` calls measured 263 ms
    /// (13 ms of process spawn each), the same 20 batched measured 12 ms. At a
    /// 200 ms tick the sequential form would spend most of the tick in `fork`.
    ///
    /// **A failing command aborts the rest of the sequence** (measured: a bad
    /// target in the middle silently truncated everything after it). A window can
    /// close between the scan and the capture, so a short result is expected, not
    /// exceptional — this returns what it got and the caller re-captures the
    /// stragglers individually, where one failure can only affect its own pane.
    pub fn capture_batch(&self, targets: &[(String, Cap)]) -> Vec<String> {
        if targets.is_empty() {
            return Vec::new();
        }
        // A nonce, not a fixed string: the payload is arbitrary screen content
        // and a full-screen program can draw whatever it likes, our separator
        // included. The same reasoning the frame log's length prefix exists for.
        let nonce = format!("MPXSEP{:x}", nonce());
        let mut args: Vec<String> = Vec::new();
        for (i, (target, what)) in targets.iter().enumerate() {
            if i > 0 {
                args.push(";".into());
                args.push("display-message".into());
                args.push("-p".into());
                args.push(nonce.clone());
                args.push(";".into());
            }
            args.extend(capture_argv(target, *what));
        }
        let Ok(out) = self.run_raw(&args) else {
            return Vec::new();
        };
        out.split(&format!("{nonce}\n")).map(str::to_string).collect()
    }

    /// Like `run`, but keeps the output verbatim. Trailing newlines are the
    /// segment boundaries a batched capture is split on, and a screen's blank
    /// lines are part of the screen.
    fn run_raw(&self, args: &[String]) -> Result<String> {
        let mut cmd = self.base();
        cmd.args(args);
        let out = cmd.output().context("running tmux")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!("tmux: {}", if err.is_empty() { "failed".into() } else { err });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[allow(dead_code)]
    pub fn set_option(&self, args: &[&str]) -> Result<()> {
        let mut v = vec!["set-option"];
        v.extend_from_slice(args);
        self.run(v).map(|_| ())
    }

    /// Hand this terminal over to tmux. Replaces the process, so it returns only
    /// on failure — `mpx` *is* the tmux client from here on.
    pub fn attach(&self, session: &str) -> Result<std::convert::Infallible> {
        use std::os::unix::process::CommandExt;
        let mut cmd = self.base();
        cmd.args(["attach-session", "-t", &format!("={session}")]);
        cmd.stdin(Stdio::inherit());
        Err(anyhow::Error::new(cmd.exec()).context("attaching to tmux"))
    }

    /// tmux's version as (major, minor). `3.6a` → (3, 6); the letter suffix is a
    /// patch level and never affects feature availability.
    pub fn version() -> Result<(u32, u32)> {
        let out = Command::new("tmux")
            .arg("-V")
            .output()
            .context("running `tmux -V` (is tmux installed?)")?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse_version(&text).with_context(|| format!("parsing tmux version from {text:?}"))
    }
}

/// What one capture asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    /// The visible screen.
    Screen,
    /// The last `rows` rows that have scrolled off.
    ///
    /// `with_top` extends the range by the topmost *visible* row. That variant is
    /// not for its text — it is how a continuation is detected. `-J` joins wrapped
    /// rows only *within* one capture, so a line still being written straddles the
    /// history/screen boundary and would otherwise be cut in half at whichever
    /// tick the boundary fell on (measured: a 300-character line reached the log
    /// as 180 characters). Capturing one row further and comparing line counts
    /// says exactly whether the last history row wraps onward.
    History { rows: u64, with_top: bool },
}

/// The argv for one capture. Split out so the single and batched paths, and the
/// two ends of the continuation check, cannot drift.
fn capture_argv(target: &str, what: Cap) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "capture-pane".into(),
        "-p".into(),
        "-J".into(),
        "-t".into(),
        target.into(),
    ];
    if let Cap::History { rows, with_top } = what {
        // History rows are addressed negatively: -1 is the row that scrolled off
        // most recently, so `-S -n -E -1` is exactly the last n of them and
        // nothing that is still on screen. `-E 0` adds the first visible row.
        v.push("-S".into());
        v.push(format!("-{rows}"));
        v.push("-E".into());
        v.push(if with_top { "0".into() } else { "-1".to_string() });
    }
    v
}

/// Enough randomness for a separator no program will draw by accident. Not
/// security — a collision costs one garbled tick, and the caller re-captures.
fn nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ (std::process::id() as u64).rotate_left(32)
}

/// Split out so it can be tested without a tmux binary present.
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.split_whitespace().nth(1)?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor))
}

/// A tmux session name may not contain `.` or `:` (they are target separators),
/// and a project directory basename routinely does.
pub fn session_name(project_dir: &Path) -> String {
    let base = project_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    let cleaned: String = base
        .chars()
        .map(|c| if c == '.' || c == ':' { '-' } else { c })
        .collect();
    if cleaned.is_empty() {
        "project".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_the_letter_suffix_release() {
        assert_eq!(parse_version("tmux 3.6a\n"), Some((3, 6)));
        assert_eq!(parse_version("tmux 3.2\n"), Some((3, 2)));
        assert_eq!(parse_version("tmux 3.1c\n"), Some((3, 1)));
        assert_eq!(parse_version("tmux next-3.4\n"), None);
    }

    /// `-J` is what rejoins a wrapped line; without it a 300-character echo came
    /// back as four fragments (measured). It must be on both capture paths.
    #[test]
    fn every_capture_joins_wrapped_lines() {
        assert!(capture_argv("@1", Cap::Screen).contains(&"-J".to_string()));
        let h = Cap::History { rows: 5, with_top: false };
        assert!(capture_argv("@1", h).contains(&"-J".to_string()));
    }

    /// History rows are addressed negatively, and the range must stop at -1 —
    /// running past it would re-capture rows that are still on screen and emit
    /// them a second time when they later scroll off. The `with_top` variant
    /// stops at 0 instead, and exists only to detect a continuation.
    #[test]
    fn a_history_capture_asks_for_exactly_the_scrolled_off_rows() {
        let v = capture_argv("@1", Cap::History { rows: 21, with_top: false });
        let at = v.iter().position(|a| a == "-S").expect("-S present");
        assert_eq!(&v[at + 1..at + 4], &["-21", "-E", "-1"]);

        let w = capture_argv("@1", Cap::History { rows: 21, with_top: true });
        let at = w.iter().position(|a| a == "-S").expect("-S present");
        assert_eq!(&w[at + 1..at + 4], &["-21", "-E", "0"]);
    }

    /// `.` and `:` are tmux target separators, so a directory called
    /// `my.project` would otherwise address a window inside session `my`.
    #[test]
    fn session_name_strips_tmux_target_separators() {
        assert_eq!(session_name(Path::new("/a/b/my.project")), "my-project");
        assert_eq!(session_name(Path::new("/a/b/plain")), "plain");
        assert_eq!(session_name(Path::new("/")), "project");
    }
}
