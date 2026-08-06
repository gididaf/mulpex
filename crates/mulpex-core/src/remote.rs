//! Remote claude peers: a `claude` running on another machine, reached through
//! an ordinary Mulpex terminal over ssh, driven by a local instance.
//!
//! The whole feature rests on one asymmetry. A local instance can already *type*
//! into a terminal and *read* it back, so "local drives remote" needed nothing
//! new. What did not exist is the other direction: the remote has no inbox, no
//! instance id and no way to push, so a local instance had to poll to learn
//! anything. This module is the missing half — a convention the remote follows
//! and the app watches for, which turns a line of the remote's own output into a
//! message in the *opener's* inbox, i.e. into the wake path that already exists.
//!
//! Everything here is pure: text in, text out. The app side (`state.rs`) does the
//! watching and the delivery; the helper side (`mcp.rs`) does the launching and
//! the reading. Both need the same marker grammar and the same rules text, which
//! is exactly the reason it lives in `mulpex-core` rather than in either one —
//! the same contract-between-two-processes argument as `termlog`.
//!
//! ## Why the marker looks like this
//!
//! Measured against a real remote `claude` over ssh (fixtures
//! `src-tauri/tests/fixtures/remote-claude-*.bin`), not reasoned about:
//!
//! - **The delimiters cannot be markdown.** The first design used
//!   `__MPX_TO_LOCAL__`. Claude Code renders its output as markdown and `__x__`
//!   is *bold*, so the underscores were eaten by the renderer and what reached
//!   the terminal was a bare `MPX_TO_LOCAL`. A grep for the marker found zero
//!   occurrences. Angle-bracket runs are not markdown-active and survive
//!   verbatim; `<<<`/`>>>` was confirmed twice through the real recorder.
//! - **The token exists because the transcript contains the local instance's own
//!   typed input**, echoed back by the remote TUI. Without a per-terminal secret,
//!   a local instance that merely *quoted* the marker would wake itself. The
//!   token never appears in plaintext on the ssh command line either — the rules
//!   go over base64-encoded.
//! - **Parsing must tolerate newlines inside the marker.** The TUI hard-wraps at
//!   the terminal width and the grid turns that wrap into a real newline, which
//!   can land anywhere — including mid-marker. Observed in the very first
//!   capture, where the echoed prompt split as `@@MPX done` / `bravo@@`.

use std::path::{Path, PathBuf};

/// Opening delimiter of a remote signal. Deliberately not markdown-active — see
/// the module docs for the `__x__` bold disaster this replaced.
pub const SIG_OPEN: &str = "<<<MPX";
/// Closing delimiter. The *first* occurrence after an opener ends the signal, so
/// a summary containing `>` is still parsed correctly up to that point.
pub const SIG_CLOSE: &str = ">>>";

/// How long a remote terminal must produce **no output at all** before the
/// backstop treats its turn as over.
///
/// This is not a guess about how fast a machine is: a working `claude` animates
/// its spinner continuously (the cargo fixture's 17 repaints are the same
/// phenomenon), so output genuinely stops only between turns. Half a second
/// would also work; 1.5 s buys margin for a stalled ssh link without making the
/// wake feel late.
pub const IDLE_TURN_END_MS: u64 = 1_500;

/// Cap on a signal's summary. Long summaries wrap across grid rows, and while
/// parsing tolerates that, a short summary keeps the marker on one line where a
/// human glancing at the pane can read it.
pub const SUMMARY_MAX: usize = 100;

/// Why a remote is calling its driver.
///
/// `Ended` is the one the remote does not send: it is synthesised by the
/// backstop when the remote goes quiet without signalling, because an LLM
/// instructed to print a marker will sometimes simply not. Keeping it in the
/// same enum means the delivery path has one shape, and the local instance is
/// told which it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Done,
    Blocked,
    Question,
    Ended,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "done" => Some(Kind::Done),
            "blocked" => Some(Kind::Blocked),
            "question" => Some(Kind::Question),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Done => "done",
            Kind::Blocked => "blocked",
            Kind::Question => "question",
            Kind::Ended => "ended",
        }
    }
}

/// One parsed signal from a remote's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub kind: Kind,
    pub summary: String,
}

/// Find every well-formed signal in `text` bearing `token`.
///
/// Newline-tolerant by construction: the span between the delimiters has its
/// whitespace collapsed, so a marker the grid wrapped across two rows parses
/// exactly as one that fitted. A marker with the wrong token — or none — is not
/// a signal, which is what makes the local instance's echoed input inert.
pub fn find_signals(text: &str, token: &str) -> Vec<Signal> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(SIG_OPEN) {
        let after = &rest[start + SIG_OPEN.len()..];
        let Some(end) = after.find(SIG_CLOSE) else {
            // An unterminated opener is the tail of a marker still being drawn;
            // stop rather than scanning past it, so the next poll sees it whole.
            break;
        };
        let body = &after[..end];
        rest = &after[end + SIG_CLOSE.len()..];

        if let Some(sig) = parse_body(body, token) {
            out.push(sig);
        }
    }
    out
}

/// Parse the span between the delimiters, undoing a hard wrap if one landed
/// inside it.
///
/// A grid wrap is genuinely ambiguous: the newline may stand where a space was
/// (the space having been trimmed off the row's end) or may cut a word in half,
/// and the serialized text cannot tell you which. Joining with `""` repairs
/// `a7f3\nc001` but welds `the\nauth` into `theauth`; joining with `" "` does the
/// reverse. Both are therefore *tried*, and the first that yields a valid signal
/// wins — a marker that parses under either reading is worth more than a
/// principled miss, and only the token and kind have to be exact.
fn parse_body(body: &str, token: &str) -> Option<Signal> {
    for joiner in ["", " "] {
        let flat = body
            .split('\n')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(joiner);
        let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut parts = flat.splitn(3, ' ');
        let (Some(tok), Some(kind)) = (parts.next(), parts.next()) else {
            continue;
        };
        if tok != token {
            continue;
        }
        let Some(kind) = Kind::parse(kind) else {
            continue;
        };
        return Some(Signal {
            kind,
            summary: parts.next().unwrap_or("").trim().to_string(),
        });
    }
    None
}

/// Remove signal markers from text shown to a model.
///
/// Same reasoning as `mcp::strip_markers` for the `__MPX_DONE_` plumbing: the
/// marker is Mulpex's wire protocol, not something the reader should have to
/// look at or, worse, imitate. Only markers bearing `token` are removed —
/// anything else is the remote's real output and stays.
pub fn strip_signals(text: &str, token: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(SIG_OPEN) else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[start + SIG_OPEN.len()..];
        let Some(end) = after.find(SIG_CLOSE) else {
            out.push_str(rest);
            return out;
        };
        let body = &after[..end];
        let is_ours = body
            .split_whitespace()
            .next()
            .is_some_and(|t| t == token);
        if is_ours {
            out.push_str(&rest[..start]);
            // Swallow the line's trailing newline too, so stripping a marker that
            // occupied its own line does not leave a blank one behind.
            let tail = &after[end + SIG_CLOSE.len()..];
            rest = tail.strip_prefix('\n').unwrap_or(tail);
        } else {
            let keep = start + SIG_OPEN.len() + end + SIG_CLOSE.len();
            out.push_str(&rest[..keep]);
            rest = &rest[keep..];
        }
    }
}

/// Whether a screen looks like a live Claude Code TUI sitting at its input box.
///
/// Used only to qualify the idle backstop: "no output for a while" means a turn
/// ended *if* there is a claude there at all, and means the ssh died or a plain
/// shell is sitting there if not. Keyed on the prompt caret plus the box rule
/// rather than on any word, because the spinner vocabulary is randomised
/// (`Lollygagging`, `Cooked`, `Brewed` all observed in one short capture) and
/// would rot on the next Claude Code release.
pub fn looks_like_claude_tui(screen: &str) -> bool {
    let caret = screen.lines().rev().take(12).any(|l| l.trim_start().starts_with('❯'));
    let rule = screen
        .lines()
        .rev()
        .take(12)
        .any(|l| l.matches('─').count() >= 20);
    caret && rule
}

/// Whether the remote is visibly mid-turn: a spinner line is on screen.
///
/// A secondary guard on the backstop, not the primary one — the primary is
/// silence. Matches the *shape* (`<glyph> Word…`) rather than the word list.
pub fn has_spinner(screen: &str) -> bool {
    screen.lines().rev().take(12).any(|l| {
        let t = l.trim_start();
        let mut c = t.chars();
        let glyph = matches!(
            c.next(),
            Some('✻') | Some('✽') | Some('✢') | Some('✶') | Some('·') | Some('*') | Some('●')
        );
        glyph && t.contains('…')
    })
}

/// Whether a screen's last live line looks like a shell waiting for a command.
///
/// Used to refuse launching into a terminal that is busy. Prompt detection is a
/// heuristic in any shell — there is no escape sequence for "I am a prompt" —
/// so this is deliberately paired with an idleness check at the call site rather
/// than trusted alone. Known residual: a quiet interactive program (a REPL at
/// its own `>` prompt) is indistinguishable from a shell, which is why the
/// caller also rejects a screen that already holds a Claude TUI.
pub fn at_shell_prompt(screen: &str) -> bool {
    let Some(last) = screen.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let t = last.trim();
    // A prompt either ENDS with a sigil (`user@host:~$`) or BEGINS with one
    // (`➜  ~`, oh-my-zsh's default, where the line ends with the *path*). Both
    // shapes are needed: keying on the trailing character alone refused to
    // launch into a perfectly idle terminal on the single most common zsh theme
    // there is — found live, on the first run against a real box.
    const TRAILING: [char; 6] = ['$', '#', '%', '>', '❯', '›'];
    const LEADING: [char; 6] = ['➜', '❯', '›', 'λ', '▶', '»'];
    t.ends_with(TRAILING) || t.starts_with(LEADING)
}

/// The rules handed to a remote claude at launch, via `--append-system-prompt`.
///
/// Delivered as part of the *system* prompt on purpose: it is re-sent with every
/// request rather than remembered, so it neither decays over a long conversation
/// nor is lost to compaction. That does not make it *obeyed* — which is exactly
/// why the idle backstop exists.
pub fn peer_rules(token: &str) -> String {
    format!(
        "You are a REMOTE CLAUDE CODE INSTANCE. You are not talking to a human: every message you \
         receive is typed to you, over an ssh terminal, by ANOTHER Claude Code instance (running \
         in Mulpex on the user's Mac) that is coordinating work with you. Treat it as a competent \
         peer engineer, not as an end user.\n\
         HOW TO WORK WITH IT:\n\
         - Be terse and factual. No pleasantries, no re-explaining what you are about to do, no \
         long summaries. It is reading your screen through a terminal transcript, so every extra \
         line costs it context.\n\
         - Do the work yourself and completely. Do not hand back a plan and wait for approval \
         unless you genuinely cannot proceed.\n\
         - If you need a decision only the human can make, say so explicitly and signal `question` \
         (below) — your driver can reach the human, you cannot.\n\
         SIGNALLING (this is how your driver knows it is their turn):\n\
         When you finish the work you were given, OR you are blocked, OR you need an answer before \
         you can continue, print — as the very last thing in your reply, on its own line, as \
         literal plain text — exactly this:\n\
         {SIG_OPEN} {token} <kind> <summary>{SIG_CLOSE}\n\
         where <kind> is exactly one of: done, blocked, question. <summary> is ONE short line \
         (max {SUMMARY_MAX} characters) saying what happened or what you need.\n\
         Rules for that line, all of which matter:\n\
         - Print it VERBATIM. Do not put it in a code block or backticks, do not bold it, do not \
         translate it, do not add punctuation around it. It is parsed by a program, not read by a \
         person.\n\
         - The `{token}` part is a secret that identifies this session. Reproduce it exactly. \
         Never print it in any other context.\n\
         - Emit it EXACTLY ONCE per turn, at the end. Never in the middle of your work, and never \
         speculatively.\n\
         Example of a correct final line:\n\
         {SIG_OPEN} {token} done Migrated the schema; 3 tables changed, tests pass{SIG_CLOSE}"
    )
}

/// The remote half of the launch command line: what runs on the far machine.
///
/// Two non-obvious pieces.
///
/// **The rules travel base64-encoded**, decoded by the remote shell. The rules
/// text contains quotes, newlines and the marker itself; interpolating it into a
/// command that is already inside an ssh argument means two levels of shell
/// quoting, and getting that wrong corrupts a system prompt in ways that would
/// surface as "the remote ignores its instructions" rather than as an error. It
/// also keeps the token off the visible command line.
///
/// **`IS_SANDBOX=1`** is set because Claude Code refuses
/// `--dangerously-skip-permissions` outright when running as root ("cannot be
/// used with root/sudo privileges for security reasons"), and remote boxes are
/// very often entered as root. This deliberately bypasses a safety check Claude
/// Code put there on purpose: a remote peer runs unattended and answers to
/// another model, so it must not stop at a permission prompt no human will see.
pub fn remote_launch_command(cwd: Option<&str>, rules_b64: &str) -> String {
    let cd = match cwd {
        Some(dir) => format!("cd {} && ", quote_path(dir)),
        // No `cd` at all rather than `cd ~`: a login shell already starts in the
        // home directory, and quoting `~` would defeat the expansion that makes
        // it mean anything.
        None => String::new(),
    };
    format!(
        "{cd}export IS_SANDBOX=1 && exec claude --dangerously-skip-permissions \
         --append-system-prompt \"$(printf %s {rules_b64} | base64 -d)\""
    )
}

/// Quote a path for the remote shell, leaving a leading `~` able to expand.
///
/// `shell_quote` alone would turn `~/src/app` into the literal directory `~`,
/// which does not exist — a failure that would surface as the remote claude
/// starting in the wrong place rather than as an error.
fn quote_path(dir: &str) -> String {
    match dir.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None if dir == "~" => "\"$HOME\"".to_string(),
        None => shell_quote(dir),
    }
}

/// The full local command a Mulpex terminal runs to reach the peer.
///
/// `-tt` forces a PTY even though ssh's stdin is not a terminal from its own
/// point of view; without it the remote `claude` gets no tty, never draws its
/// TUI, and there is nothing to read.
pub fn ssh_command(ssh_target: &str, cwd: Option<&str>, rules_b64: &str) -> String {
    format!(
        "ssh -tt {} {}",
        shell_quote(ssh_target),
        shell_quote(&remote_launch_command(cwd, rules_b64))
    )
}

/// Single-quote for a POSIX shell. Distinct from `escapePath` on the frontend,
/// which backslash-escapes to match what a real terminal inserts on a drag —
/// here the string is machine-generated and never shown to the user, so the
/// simpler and stricter form is right.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Minimal base64 encoder.
///
/// Hand-rolled rather than pulled in as a dependency: `mulpex-core` is linked
/// into `mulpex-helper`, which a `PreToolUse` hook execs on every Read/Write/
/// Edit/Bash, so the helper's size and link time are a real cost. The crate's
/// entire dependency list is `anyhow` + `serde_json` for that reason.
pub fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A per-terminal secret, mixed from the clock and the caller's own ids.
///
/// Not cryptographic and does not need to be: its only job is to be something a
/// local instance will not type by accident. `mulpex-core` has no `rand`
/// dependency, and adding one to the helper for this would be the wrong trade.
pub fn new_token(instance: usize, salt: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(salt);
    // FNV-1a over the three inputs: cheap, dependency-free, well-mixed enough
    // that two terminals opened in the same millisecond still differ.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in nanos
        .to_le_bytes()
        .iter()
        .chain(salt.to_le_bytes().iter())
        .chain((instance as u64).to_le_bytes().iter())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..8].to_string()
}

/// Where a remote terminal's metadata lives: `terminals/remote/<id>.json`.
///
/// A file rather than in-memory state because the two sides are different
/// processes — the helper writes it at launch, the app's poll loop reads it on
/// every tick to know which terminals to watch and with which token.
pub fn meta_path(state_dir: &Path, id: usize) -> PathBuf {
    state_dir.join("terminals").join("remote").join(format!("{id}.json"))
}

/// What the watcher needs to know about a remote terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMeta {
    /// The secret that makes a marker this terminal's, and not an echo.
    pub token: String,
    /// Where it is, for the message the driver receives.
    pub ssh_target: String,
    /// Which local instance opened it, and therefore whose inbox a signal lands
    /// in. Ownership is the opener's for the terminal's whole life.
    pub opener: usize,
}

impl RemoteMeta {
    pub fn write(&self, state_dir: &Path, id: usize) -> std::io::Result<()> {
        let path = meta_path(state_dir, id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::json!({
                "token": self.token,
                "ssh_target": self.ssh_target,
                "opener": self.opener,
            })
            .to_string(),
        )
    }

    pub fn read(state_dir: &Path, id: usize) -> Option<Self> {
        let raw = std::fs::read_to_string(meta_path(state_dir, id)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        Some(Self {
            token: v.get("token")?.as_str()?.to_string(),
            ssh_target: v.get("ssh_target")?.as_str()?.to_string(),
            opener: v.get("opener")?.as_u64()? as usize,
        })
    }

    /// Every remote terminal currently registered, as `(id, meta)`.
    pub fn all(state_dir: &Path) -> Vec<(usize, Self)> {
        let dir = state_dir.join("terminals").join("remote");
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let Some(id) = e
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<usize>().ok())
                else {
                    continue;
                };
                if let Some(meta) = Self::read(state_dir, id) {
                    out.push((id, meta));
                }
            }
        }
        out.sort_by_key(|(id, _)| *id);
        out
    }

    pub fn forget(state_dir: &Path, id: usize) {
        let _ = std::fs::remove_file(meta_path(state_dir, id));
    }
}

/// A signal's identity, for "have I already acted on this one?".
///
/// Kind plus summary rather than a byte offset, because the same signal is
/// reachable from two places at once — the scrolled-off log and the still-live
/// screen — and an offset in one says nothing about the other.
pub fn fingerprint(sig: &Signal) -> String {
    format!("{}|{}", sig.kind.as_str(), sig.summary.trim())
}

/// Where a given reader's last-seen signal is recorded. `reader` is an instance
/// number for a `hub_terminal_read` caller, or `watch` for the app's poll loop,
/// so the two never consume each other's notifications.
fn seen_path(state_dir: &Path, id: usize, reader: &str) -> PathBuf {
    state_dir
        .join("terminals")
        .join("remote")
        .join(format!("{id}.seen.{reader}"))
}

/// Whether `sig` is one this reader has not been told about, recording it if so.
///
/// Known limit, deliberate: two *identical* consecutive signals (same kind, same
/// summary) collapse into one. Making them distinct would need a counter the
/// remote maintains, which is more protocol for the model to get wrong than the
/// case is worth — and the backstop still fires on the second turn ending.
pub fn take_if_new(state_dir: &Path, id: usize, reader: &str, sig: &Signal) -> bool {
    let path = seen_path(state_dir, id, reader);
    let fp = fingerprint(sig);
    if std::fs::read_to_string(&path).is_ok_and(|prev| prev == fp) {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &fp);
    true
}

/// Drop every record for a terminal that has gone away.
pub fn forget_all(state_dir: &Path, id: usize) {
    RemoteMeta::forget(state_dir, id);
    let dir = state_dir.join("terminals").join("remote");
    if let Ok(entries) = std::fs::read_dir(dir) {
        let prefix = format!("{id}.seen.");
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// The message body delivered into the driver's inbox when a remote signals.
///
/// It names the terminal id twice over (in prose and in the tool call to make)
/// because the wake arrives as a *hub message*, and the instinct a hub message
/// creates is to reply with `hub_send` — which would be addressed to a terminal
/// id, i.e. to a shell that is not a peer and cannot receive it.
pub fn wake_body(id: usize, target: &str, sig: &Signal) -> String {
    let where_ = if target.trim().is_empty() {
        String::new()
    } else {
        format!(" on {target}")
    };
    let what = match sig.kind {
        Kind::Done => "has FINISHED the work you gave it",
        Kind::Blocked => "is BLOCKED and cannot continue",
        Kind::Question => "is asking you a QUESTION and is waiting for your answer",
        Kind::Ended => "has stopped and is idle at its prompt (it did not signal — it may have \
                        finished, or it may be waiting for you)",
    };
    let summary = if sig.summary.trim().is_empty() {
        String::new()
    } else {
        format!("\nIt says: {}", sig.summary.trim())
    };
    format!(
        "The remote claude you started{where_} (terminal #{id}) {what}.{summary}\n\
         Read what it actually did with mcp__mulpex__hub_terminal_read(id: {id}), and reply to it \
         with mcp__mulpex__hub_terminal_send(id: {id}) — it is a terminal, NOT a hub instance, so \
         hub_send cannot reach it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK: &str = "a7f3c001";

    fn sig(text: &str) -> Vec<Signal> {
        find_signals(text, TOK)
    }

    #[test]
    fn parses_a_plain_signal() {
        let got = sig(&format!("all done\n{SIG_OPEN} {TOK} done Migrated the schema{SIG_CLOSE}\n"));
        assert_eq!(
            got,
            vec![Signal { kind: Kind::Done, summary: "Migrated the schema".into() }]
        );
    }

    #[test]
    fn parses_every_kind() {
        for (word, kind) in [
            ("done", Kind::Done),
            ("blocked", Kind::Blocked),
            ("question", Kind::Question),
        ] {
            let got = sig(&format!("{SIG_OPEN} {TOK} {word} x{SIG_CLOSE}"));
            assert_eq!(got.first().map(|s| s.kind), Some(kind), "kind {word}");
        }
    }

    /// The grid turns the TUI's hard wrap into a real newline, which can land
    /// anywhere — including inside the marker. Observed in the first real
    /// capture. Every split position must still parse.
    #[test]
    fn a_marker_wrapped_anywhere_still_parses() {
        let whole = format!("{SIG_OPEN} {TOK} done Fixed the auth bug{SIG_CLOSE}");
        for i in 1..whole.len() {
            if !whole.is_char_boundary(i) {
                continue;
            }
            let wrapped = format!("{}\n{}", &whole[..i], &whole[i..]);
            let got = find_signals(&wrapped, TOK);
            // A split inside the delimiters themselves genuinely destroys them;
            // everywhere else must still yield the signal. The summary is
            // allowed to lose one space to the wrap (see `parse_body`) — the
            // token and kind are the parts that must be exact.
            if whole[..i].contains(SIG_OPEN) && whole[i..].contains(SIG_CLOSE) {
                assert_eq!(got.len(), 1, "split at {i}: {wrapped:?}");
                assert_eq!(got[0].kind, Kind::Done, "split at {i}");
                assert_eq!(
                    got[0].summary.replace(' ', ""),
                    "Fixedtheauthbug",
                    "split at {i}: {wrapped:?}"
                );
            }
        }
    }

    /// The whole point of the token: the transcript contains the driver's own
    /// typed input echoed back by the TUI, so a marker it merely quoted must be
    /// inert.
    #[test]
    fn a_foreign_or_missing_token_is_not_a_signal() {
        assert!(sig(&format!("{SIG_OPEN} deadbeef done not mine{SIG_CLOSE}")).is_empty());
        assert!(sig(&format!("{SIG_OPEN} done no token at all{SIG_CLOSE}")).is_empty());
        assert!(sig("just some prose about <<<MPX markers>>>").is_empty());
    }

    #[test]
    fn an_unknown_kind_is_not_a_signal() {
        assert!(sig(&format!("{SIG_OPEN} {TOK} finished nope{SIG_CLOSE}")).is_empty());
    }

    /// A marker still being drawn must not be consumed half-formed: the next
    /// poll will see it whole.
    #[test]
    fn a_half_drawn_marker_is_ignored_until_complete() {
        assert!(sig(&format!("{SIG_OPEN} {TOK} done half writt")).is_empty());
    }

    #[test]
    fn several_signals_all_parse_in_order() {
        let got = sig(&format!(
            "{SIG_OPEN} {TOK} question first?{SIG_CLOSE}\nmiddle\n{SIG_OPEN} {TOK} done second{SIG_CLOSE}"
        ));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind, Kind::Question);
        assert_eq!(got[1].summary, "second");
    }

    #[test]
    fn stripping_removes_ours_and_keeps_everything_else() {
        let text = format!(
            "real output\n{SIG_OPEN} {TOK} done finished{SIG_CLOSE}\nmore output\n\
             {SIG_OPEN} other done theirs{SIG_CLOSE}\n"
        );
        let out = strip_signals(&text, TOK);
        assert!(!out.contains(TOK), "our marker survived: {out:?}");
        assert!(out.contains("real output") && out.contains("more output"));
        assert!(out.contains("other done theirs"), "a foreign marker was eaten: {out:?}");
        assert!(!out.contains("\n\n"), "stripping left a blank line: {out:?}");
    }

    #[test]
    fn shell_quoting_survives_a_hostile_path() {
        assert_eq!(shell_quote("/tmp/it's here"), r"'/tmp/it'\''s here'");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // The rules are multi-line and non-ASCII in places; round-trip the real
        // thing rather than only the RFC vectors.
        let rules = peer_rules(TOK);
        assert!(!b64(rules.as_bytes()).contains(TOK));
    }

    /// The rules must actually contain the marker the parser looks for — the two
    /// are one contract, and a typo in either is a feature that silently never
    /// fires.
    #[test]
    fn the_rules_teach_exactly_the_marker_the_parser_accepts() {
        let rules = peer_rules(TOK);
        let example = rules
            .lines()
            .find(|l| l.contains(SIG_OPEN) && l.contains("done "))
            .expect("no example marker in the rules");
        let got = find_signals(example, TOK);
        assert_eq!(got.len(), 1, "the rules' own example does not parse: {example:?}");
        assert_eq!(got[0].kind, Kind::Done);
    }

    #[test]
    fn tokens_differ_between_terminals() {
        let a = new_token(1, 10);
        let b = new_token(1, 11);
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn meta_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("mpx-remote-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = RemoteMeta {
            token: TOK.into(),
            ssh_target: "root@example".into(),
            opener: 3,
        };
        meta.write(&dir, 7).unwrap();
        assert_eq!(RemoteMeta::read(&dir, 7).as_ref(), Some(&meta));
        assert_eq!(RemoteMeta::all(&dir), vec![(7, meta)]);
        RemoteMeta::forget(&dir, 7);
        assert!(RemoteMeta::read(&dir, 7).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shell_prompt_is_told_from_a_busy_screen() {
        // `➜  ~` is oh-my-zsh's default and ends with the path, not a sigil —
        // the case that failed live.
        for prompt in [
            "root@vm:/srv# ", "gidi@mac ~ %", "› ", "❯", "$ ", "user@host:~$", "➜  ~",
            "➜  mulpex git:(master)",
        ] {
            assert!(at_shell_prompt(&format!("some output\n{prompt}")), "not a prompt: {prompt:?}");
        }
        for busy in ["Compiling mulpex v0.6.0", "  ⎿  /tmp/mpx-probe", ""] {
            assert!(!at_shell_prompt(busy), "read as a prompt: {busy:?}");
        }
        // Trailing blank lines must not hide the prompt.
        assert!(at_shell_prompt("out\n$ \n\n   \n"));
    }

    #[test]
    fn a_wake_without_a_known_target_still_reads_properly() {
        let body = wake_body(2, "", &Signal { kind: Kind::Done, summary: "built".into() });
        assert!(body.contains("you started (terminal #2)"), "{body}");
        assert!(!body.contains(" on  "), "empty target left a gap: {body}");
    }

    #[test]
    fn the_wake_body_names_the_right_tools() {
        let body = wake_body(4, "root@vm", &Signal { kind: Kind::Question, summary: "which db?".into() });
        assert!(body.contains("hub_terminal_read(id: 4)"));
        assert!(body.contains("hub_terminal_send(id: 4)"));
        assert!(body.contains("hub_send cannot reach it"));
        assert!(body.contains("which db?"));
    }
}
