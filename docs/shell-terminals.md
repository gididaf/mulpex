# Shell terminals (⌘⇧T + `hub_terminal_*`)

A session is either a `claude` or a plain interactive shell. This is the shell half: what gets
launched, why the hub knows nothing about it, and how its output reaches a claude in another
process. Touches `pty.rs` (`SessionKind`, `Recorder`, `kill`), `vtgrid.rs`, `mulpex_core::termlog`
and `mcp.rs`'s `hub_terminal_*` tools.

Back to [CLAUDE.md](../CLAUDE.md).

A session is now either a `claude` or a **plain interactive shell** (`pty.rs::SessionKind`).
Terminals live in the *same* `Core.sessions` vec, the same per-project id space and the same
sidebar list (labelled `term #N`), so everything keyed by `(project, id)` — attach, `send_bytes`,
resize, close, the `OutputSink`, `TerminalManager`, `killpg` teardown — stayed kind-agnostic and
was not touched. What differs is what gets launched, what the hub knows about it (nothing), and
that its output is **recorded to disk**.

The user opens one with ⌘⇧T; an instance opens one with `hub_terminal_open` and can then drive it
(`_send`), read it (`_read`) and close it (`_close`). Any instance may drive **any** terminal in
its project, including one the user opened — inspecting the dev server the user started is a large
part of the point. `hub_instances` carries a `terminals` array so that costs no extra call.

- **Why not just the Bash tool:** Bash is request/response and cannot hold a process. A terminal
  is for the long-running and the interactive — dev server, watcher, `tail -f`, REPL, a build you
  want to glance at later, a command that asks questions partway through. `HUB_RULES` says exactly
  this, so instances don't open a terminal for `ls`.
- **`$SHELL -l -i`, not `-l`.** `-l` alone is login-but-*not*-interactive: zsh skips `.zshrc`,
  prints no prompt, and treats the PTY as a script. Both flags are required.
- **A terminal must not inherit a hub identity.** `portable_pty` passes the parent env through, so
  `shell_command()` `env_remove`s all three `MULPEX_*` vars: if Mulpex was launched from inside a
  Mulpex claude, a `claude` the user then typed into a terminal would write status files under a
  *terminal's* id and corrupt the hub. Same class of defence as the `CLAUDE_CODE_CHILD_SESSION`
  removal ([sessions.md](sessions.md)). Terminals also don't go through `claude_command()`, which
  *errors* when `claude`
  is missing — a shell must not fail to open for that reason.
- **Hub isolation is by omission.** Terminals are excluded from `write_live_instances` (so
  `hub_send` never offers a shell as a peer), from `hub_snapshot`'s `statuses`, and from
  `persist_sessions`. Excluding them from `statuses` is what keeps `attention.ts`, the updater's
  busy guard and the tab badges correct *for free* — all three key off a session having a status
  entry. A synthetic `waiting` would have put every terminal into that math and painted a green
  "ready" dot on a shell forever.
- **Not persisted across restart**, deliberately: a restored terminal would be a fresh shell with
  no scrollback, and re-running a seeded command could be destructive. `persist_sessions` filters
  on `kind`, *not* on an empty `session_id` — and no column was added to the store file, because
  `persist.rs` parses it with `splitn(3, '\t')`.

## An exited terminal is kept; a dead instance is not

A shell that exits stays in the list marked `exited`, with its output still readable, until
someone closes it (⌘W / `hub_terminal_close`). Otherwise a command's output would vanish at the
moment the command finished — the exact thing "read it anytime" exists for.

That makes "dead" no longer mean "remove", which two things depend on:

- **`Core.closing`** marks a session as asked-to-go-away. `reap_dead` removes a dead session only
  if it's a Claude *or* it's in `closing`, so an explicit close and a self-exit still follow one
  single-sourced path.
- **`reap_dead`'s early-return tests removability, not liveness.** The old
  `if sessions.iter().all(is_alive) { return }` would be false *forever* once one terminal exited,
  and the whole body — two disk writes, including re-truncating the `instances` file the helper
  reads — would run on every 200 ms tick for the life of the app. Guarded by
  `a_kept_exited_terminal_does_not_make_every_poll_do_work`.
- **`hub.rs` diffs the session list** instead of emitting only on add/remove, which is how an
  alive→exited transition reaches the sidebar at all. That also closed a standing gap: a rename or
  a mute changed the backend and emitted nothing.
- **Never `wait()` the child to learn it exited.** Liveness is reader-thread EOF; leaving the
  child a zombie keeps its pid unrecyclable, which is what makes the `killpg` in `Drop` /
  `teardown_all` safe for a terminal that may sit in the list for a long time after dying.

## Killing a session: `killpg` is not enough once a shell is involved

`Session::kill` now sweeps **by controlling terminal** before the `killpg`, and that extra step
exists entirely because of terminals:

- A `claude` is a node process and does **no job control**, so everything its Bash tool spawns
  inherits its process group. One `killpg` reaches the lot — measured: descendant `pgid ==` session
  `pgid`, gone after close, no zombie (`child.wait()` reaps the direct child).
- An **interactive shell does** job control and puts every job in its own process group.
  A *foreground* job still dies, but from the hangup rather than the `killpg`: dropping the master
  SIGHUPs the tty's foreground group. A **backgrounded `cmd &` is in neither** — measured, it
  survived both ⌘W and app quit, which is a hole in the no-orphans guarantee.
- Giving the shell a grace period after SIGHUP so it could hup its own jobs **does not fix it**
  (measured at 150 ms and 400 ms; the job was still alive both times). Don't reach for that.

So the sweep SIGKILLs every process whose `e_tdev` matches this session's tty
(`proc_listpids` + `proc_pidinfo`, macOS-only, with a no-op fallback elsewhere). That set is
exactly this session's descendants: the device is ours for as long as the master is open, which is
why the sweep must run **inside `kill()` while `self.master` is still alive** — after the device is
released the kernel can hand the same number to an unrelated pty. `tty_dev` is latched by the
reader thread on first output (proof the child has claimed the tty; it cannot be read right after
`spawn_command`, which has only returned from the fork), so teardown still knows it once the child
is gone. Guarded by `closing_a_terminal_also_kills_its_background_jobs`, which was confirmed to
fail with the sweep disabled.

## The transcript (`vtgrid.rs` + `mulpex_core::termlog`)

The MCP helper is a *separate process*, so it cannot reach PTY bytes in memory — it reads a file.
Creating/driving a terminal is a `termreq/` file handshake through the poll loop (same shape as
`hub_spawn`); **reading is not**, which is what makes "read anytime" cheap enough to poll.

**Why a grid emulator and not a line-based ANSI stripper.** `cargo`, `vite`, `docker compose` and
every spinner repaint by moving the cursor and erasing. A filter that merely drops escape
sequences emits *every intermediate frame*, stacked. Measured on real captured PTY bytes
(`src-tauri/tests/fixtures/cargo-build.bin`, recorded through a real pty at 32×120): cargo's
progress bar repaints one line **17 times**; replayed through `vtgrid` the log is 8 lines — the 7
`Compiling` lines and `Finished`, exactly what a human read on screen. Frozen as
`vtgrid::replays`. So `Screen` is a bounded rows×cols grid the escape sequences actually act on,
and a row reaches the log only when it can no longer change.

- **A row is logged when it scrolls off the top** — or on a full clear (`ED 2/3`), which is what a
  real terminal's scrollback does too. Consequence: **what is still on screen is not in the log**,
  so a dev server sitting at a steady screen produces no history at all. That is why there is a
  second file (`<id>.screen`, temp+rename) and why `hub_terminal_read` returns both `new_output`
  (history since your cursor) and `current_screen` (re-sent every read, labelled as live).
- **Alt-screen (`?1049h`) is suppressed**, replaced by one `[full-screen program — output
  omitted]` line. A stray `vim`/`htop` would otherwise evict the whole 1 MB budget.
- **Partial UTF-8 *and* partial escape sequences carry across chunks.** The reader thread delivers
  arbitrary 8 KB slices; per-chunk `from_utf8_lossy` sprays U+FFFD at every boundary and a
  straddling CSI leaks `[0m` into "stripped" text. Both have tests.
- **A settle thread publishes the screen on a timer.** The reader thread only runs when there is
  output, so without it the last chunk of a burst — usually the prompt itself — would sit
  unpublished indefinitely.
- **The log trims in place on the one writer fd, header last.** A rename-based trim would unlink
  the inode the writer points at and every later byte would vanish into a deleted file, silently,
  forever. Writing the header last means a reader that sees `base` change across its own read
  knows the data moved and retries — without that check a trim landing mid-read returns *the wrong
  window of text*, which is a wrong answer, not an error. Both covered by tests.
- `terminals/index` (`<id>\t<running|exited>\t<label>`) is the manifest `hub_instances` reads; it
  is refreshed from the poll loop **on change**, because a shell can exit with nothing else
  happening and an index that only updated on add/remove would advertise a dead shell as running.

## Reading incrementally, and knowing when a command finished

- **Per-instance cursors** (`terminals/cursors/<id>.<instance>`, logical offsets) so a read returns
  only what's new to *that* reader — polling a build doesn't re-deliver everything.
- **The opener's cursor is seeded to 0**, so its first read is the terminal's whole life. A claude
  reading someone *else's* terminal for the first time gets the **tail** (200 lines) with
  `first_read: true` — never a 1 MB dump, never a confusing empty result.
- **`wait_ms` (≤30 s) blocks** until new output, completion, or exit. This is why
  `mcp.rs`'s transport became **concurrent** (thread per `tools/call`, only the stdout write
  serialized): Claude Code batches independent tool calls routinely, and a serial loop would park
  the whole batch behind one waiting read. Verified: a `hub_instances` issued while a 30 s read is
  parked comes back in well under a second. A timed-out wait with nothing new does **not** advance
  the cursor.
- **Completion is a sentinel, not a guess.** `hub_terminal_send` submits
  `<cmd>; printf '\n__MPX_DONE_<token>_%s__\n' "$?"` and records the token; a read reports
  `command_finished` + `exit_code` and strips both the marker line and the `; printf …` tail off
  the echoed command. `idle_ms` alone is a weak signal — a linking build is silent for a minute
  mid-run.
- **The sentinel is only appended when the terminal is at a prompt.** If a tracked command hasn't
  produced its marker yet, the text being sent is an *answer to that running command*, not a new
  command line, and `; printf …` would feed the program nonsense. That rule is automatic (no
  parameter) and is what makes `submit: true` safe for both cases. The rule lives in one pure
  function, `mcp.rs::mark_action` → `Track | Keep | Clear`, which is where the two ways it used to
  misfire were fixed (see **The six driving-session gaps** below).
- **Stripping the echoed tail is newline-tolerant, and has to be.** The shell echoes the command
  *including* the ~47-character `; printf …` tail, so any command longer than about half the
  terminal width wraps — and the grid turns that wrap into a real newline, landing anywhere inside
  the tail, `__MPX_DONE_` included. A per-line `find` misses it entirely on long commands and
  leaves half of it behind on medium ones. `strip_echoed_tails` therefore matches across newlines
  and swallows them with the span, which also rejoins the echoed command into the one line it was
  before wrapping. Guarded by a test that wraps at *every* position inside the tail. (Completion
  detection was never affected — the printed marker is ~25 chars and never wraps.)

## The six driving-session gaps — found 2026-08-03, all fixed 2026-08-04

The first time a claude actually drove a terminal for a whole task (instance #2, ~35 calls against
an interactive `ssh` session on a remote box) surfaced six gaps. All six are now fixed; each is kept
here because the *reason* is the part that's expensive to rediscover, and three of them are shapes
that will recur. Every one was **reproduced against the real `mulpex-helper`** before and after:
reverting `mark_action` to the pre-fix rule fails 9 of the harness's checks — including the exact
live symptoms, `PY; printf …` on the heredoc terminator and `exit_code: 0` reported while
`cd … && ls -la && cat config.yaml` was still running.

- **A stale `command_finished` / `exit_code` was reported for the wrong command.** `mark_path` was
  written only when `track` was true and **never cleared when it was false**, so after an untracked
  send the mark still named the *previous* command's token — which by then had completed — and the
  next read answered `command_finished: true, exit_code: 0` about a command still in flight. The 0
  belonged to a `docker ps` two sends earlier. The dangerous one: a *wrong answer*, not an error,
  and a model branches on it. Now `mark_action` returns `Clear` for any untracked send made while
  the terminal is at a prompt. A mark for a command that is still **running** is deliberately
  `Keep`t — that input is an answer to it, and its completion is still worth reporting.
  **Interrupts clear too**, which is not an extra: Ctrl-C kills the `; printf …` along with the
  command, so the marker never prints, and a dangling mark would leave the terminal reading as
  permanently not-idle — nothing sent afterwards could ever be tracked again. That wedge is its own
  harness check.
- **The sentinel broke any multi-line input; a heredoc hung the shell.** The marker is appended to
  the end of the *whole string*, so on multi-line input it lands on the last line — and for a
  heredoc that line is the terminator: `PY` became `PY; printf …`, which no longer terminates it,
  leaving the shell in `>` continuation until someone sent Ctrl-C. It was **nondeterministic**: the
  same claude's first heredoc went through untouched, purely because tracking happened to be off.
  Fixed as chosen (Gidi, 2026-08-03) by not tracking multi-line input at all — deterministic, and
  it cannot hang the shell. The cost is no exit code for heredocs, and the tool description says so.
  The two alternatives stay rejected: `{ … } ; printf …` hangs on unbalanced quotes/braces instead,
  and a marker on its own typed-ahead line is swallowed by anything that reads stdin (`cat`, a REPL).
  The same guard is on the **seeded** command in `hub_terminal_open` (`seed_and_mark`).
- **Long lines were hard-wrapped, so verbatim data came back mangled.** `vtgrid` had no soft-wrap
  tracking: a row overflowing `cols` became a real `\n` in both `screen_text` and the log, so real
  reads were full of `--no-lege\nnd` and JSON split mid-value — a token, URL or id straddling the
  boundary was silently corrupted, and grepping the log for it could never match. `Screen` now
  carries a per-row `wrapped` flag, set **only** by `print`'s auto-wrap, cleared whenever the row is
  re-written (CR), erased through the right margin, or replaced, and moved in lockstep with `cells`
  by every scroll / IL / DL. A wrapped row is serialized **untrimmed and unterminated**, so its
  continuation joins it — including across the log, where the continuation scrolls off in a later
  tick. Consequence by design: a log line is no longer bounded by the terminal width.
  Two non-obvious bits: an *exactly full* row followed by LF is **not** joined (deferred wrap — the
  flag is set when a further char actually arrives, which is also why the cargo/vite fixtures still
  replay byte-identically), and CR clearing the flag is the conservative choice — a line that is
  still long simply re-wraps as it reprints. The old test asserting `"abcd\nefgh"` was the bug,
  pinned; it now asserts `"abcdefgh"`.
  *(The old note here said this "touches the renderer the UI shares" — it does not. `vtgrid` is used
  only by `pty.rs`'s `Recorder`; the UI is xterm.js and never reads it. Blast radius is the
  transcript and `<id>.screen`.)*
- **`current_screen` was never marker-stripped.** `new_output` went through `strip_markers` while
  `<id>.screen` was read straight off disk and inserted raw, so every screen read carried the
  visible `; printf '\n__MPX_DONE_…' "$?"` plumbing. It mattered more than it looked: a claude
  driving a shell tends to prefix `clear;` (itself a workaround for `new_output` lagging the
  screen), which makes the screen the primary channel. Both now go through `strip_markers`.
- **`hub_terminal_open` mangled its seed command.** `.split_whitespace().join(" ")` collapsed all
  whitespace, silently rewriting a script before it ever ran. The command is now passed **verbatim**;
  only the *label* is flattened (`flatten_label`, capped at `LABEL_MAX_CHARS` with an ellipsis).
- **`wait_ms` was silently ignored when `full: true`**, and when tracking was off it quietly
  degraded from "wait for the command to finish" to "wait for any byte" — which is why nearly every
  wait in that session either timed out or came back early. Now: with a command in flight the wait
  is **for its completion**; with nothing tracked it is for output; `full` no longer disables it
  (what you read and how long you block are unrelated questions); and the reply carries
  **`waited_for: "completion" | "output"`** so a caller never has to guess which it got.
  Writing the test exposed a third bug in the same lines: the loop compared against the total *at
  call time*, so a read with unread output already in hand still blocked the whole window waiting
  for **more**. The condition is now "is there anything this reader hasn't seen", which is what the
  schema always claimed. A timed-out wait with nothing new still does not advance the cursor.

