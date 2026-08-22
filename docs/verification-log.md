# Verification log

What has actually been measured, driven, or proven — and, just as importantly, what has NOT. This is
history: it records the evidence behind claims made elsewhere in the docs, so a later change can tell
"verified live" from "compiles clean". Nothing here is a rule; the rules live in
[CLAUDE.md](../CLAUDE.md) and the subsystem docs.

- Both crates + the Tauri app compile clean; `svelte-check` + `vite build` clean.
- The whole coordination hub works **end-to-end through `mulpex-helper`**: MCP `initialize` /
  `tools/list` (all 6 `hub_*` tools), `hub_send`→`hub_inbox` delivery + `messages.log`,
  `userpromptsubmit` task capture + peer snapshot, and `pretooluse` `O_EXCL` lock acquisition
  with a canonical-path + heartbeat token.
- **Run as a GUI and bundled.** `npm run tauri dev` and `tauri build` (→ `Mulpex.app` with the
  `mulpex-helper` sidecar inside `Contents/MacOS/`) both work; multi-instance spawn,
  focus-switch, resize, and session `--resume` verified in the window. *(This entry used to say
  "signed `Mulpex.app`". It was not: through v0.5.0 the bundler emitted no `_CodeSignature` at
  all — confirmed against the published tarball, not just a local build — and that unchecked
  claim is part of why the TCC breakage ([packaging.md](packaging.md)) went unexplained for so
  long. Signing is only real
  from v0.6.0, via `signingIdentity`.)*
- **Idle-wake hub listener verified live.** A `hub_send` from one instance woke the idle peer via
  its Monitor event (no injected prompt line), which read its inbox and replied — round-trip
  confirmed both directions, with the `⟳` marker and a clean sidebar (the sentinel +
  `<task-notification>` task-capture skips work).
- **Clean-start arming verified live.** A normal instance opened to an empty prompt (no bootstrap
  turn) and armed its Monitor invisibly from the `UserPromptSubmit` hook on the user's first
  message; the `armed/<id>` flag stopped the reminder on later turns.
- **`hub_spawn` verified live.** An instance spawned a task-seeded sibling that appeared in the
  sidebar auto-named, armed its own listener on its injected first turn, ran its task, and
  `hub_send`'d its result back to the spawner.
- **Multi-project (v0.4.0).** `cargo build`/`clippy` + `svelte-check` + `vite build` clean; the
  app boots to the picker with no open projects (fresh `open.txt`) and no panic, with the
  drag-drop listener registered. Released as `v0.4.0` with a signed `Mulpex_0.4.0_aarch64.dmg`.
  The interactive flows (tab/⌘P switching, per-project hub isolation, restore-all, dedup,
  no-orphans) are covered by the plan's verification checklist and want a live GUI to exercise.
- **Finder-launch fix (v0.4.1).** The bundled `.app` was exercised under a Finder-equivalent
  environment (`env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`): `claude` resolves out of
  `~/.local/bin` via `claude_bin::merged_path()`, renders in color (`TERM=xterm-256color`), and
  the picker shows the not-found banner when the CLI is genuinely unreachable. Released as
  `v0.4.1`.
- **Shift+Enter (v0.4.3).** *v0.4.2 changed the byte and did not fix it* — the byte was never the
  problem. Measured, not guessed, two ways: (1) driving the real `claude` on a Python PTY with
  `pyte` rendering the prompt box shows `\n`, `\x1b\r`, `\x1b[13;2u` and `\x1b\n` **all** insert a
  newline, so the original `\n` was fine; (2) driving the real `@xterm/xterm` build under jsdom
  and replaying the browser's keydown→keypress contract reproduces the bug exactly — the old
  handler emits `[27,13]` *and then* a stray `[13]`, the new one emits only `[27,13]`. The stray
  `\r` was the submit. Also worth knowing: Claude requests the **kitty keyboard protocol** and
  **modifyOtherKeys** at startup (visible in the PTY capture); xterm.js answers neither, so it
  can never disambiguate Shift+Enter on its own — the manual carve-out is mandatory.
- **Dropped paths (v0.4.5).** `shellQuote` was verified by round-tripping 17 adversarial paths
  (spaces, quotes, backticks, `$HOME`, `;rm -rf`, globs, unicode, backslashes, tab/CR/LF/ESC/BEL/
  DEL, and a mixed quote+newline+backslash case) through **real bash**: each yields exactly one
  argument equal to the original, a 17-path multi-drop splits back into 17, and no emitted text
  contains CR or LF (the invariant that stops a filename from submitting the prompt). Then
  confirmed live in the reinstalled `.app` by the user. `svelte-check` + `vite build` clean.
- **Bracketed-paste drops (v0.4.6).** The `[Image #N]` mechanism was found by measurement, not
  docs: driving a real `claude` over a PTY (`pyte`, typing only, never submitting) shows a *typed*
  image path stays plain text while the *same* path inside `ESC[200~ … ESC[201~` becomes
  `[Image #1]`. The app's exact `dropPaths` bytes were then replayed against a fresh `claude` per
  case — image, image-with-space, two images, csv-with-spaces, image+csv — all correct. The
  trailing-space-outside-the-markers bug was caught by this matrix and *only* by its non-image
  rows. 19 adversarial paths re-checked through real bash (one argument each, unicode bare, no
  CR/LF). `svelte-check` + `vite build` clean.
- **RTL / Hebrew.** Both halves measured rather than eyeballed (see [rendering.md](rendering.md)), then
  confirmed **live in the installed `.app`**: a session started with ⌘T and Hebrew typed into it
  renders words *and* letters in reading order, including a mixed Hebrew/English line. The
  intermediate state is the instructive one — after only the WebGL removal the letters were right
  and the words still ran left-to-right, and *that* was invisible to screenshot-reading; it took
  per-character `getBoundingClientRect` to see which end was which.
- **⌘1–9 → projects.** Verified live by driving the installed app with System Events: ⌘3 selects
  the third tab (`dream-email`), matching `ProjectTabBar` order. Menu accelerators only exist in
  the built app, so this cannot be checked from `vite build` alone.
- **Tab counts / hub panel / sidebar split.** Verified by screenshotting the installed app: tabs
  show the session count (`0` with no sessions, no badges), the hub panel shows only `MESSAGES`
  when quiet, and the sidebar divider sits at 72%. The red `needs` and amber unread badges are
  **build-verified only** — producing them needs a background project actually asking a question.
- **Quit paths / scratch-root leak.** All four measured before and after against a real bundle on
  an isolated `HOME` (`scratchpad/measure-quit.sh`); see the teardown section for the table. The
  startup sweep collected the 12 dirs the bug had already accumulated.
- **Auto-update, end to end, against a local endpoint.** Two real bundles (0.4.6 and 0.4.7) built
  with the endpoint repointed at `127.0.0.1:8787`, the 0.4.7 update artifacts served by
  `python3 -m http.server`, and the 0.4.6 app run against them: banner appeared with the right
  version and notes, the click downloaded + signature-verified + swapped the bundle
  (**0.4.6 → 0.4.7 on disk**), the old process exited, a new one came up, and **the old scratch
  root was removed** — teardown ran on the update restart. No GitHub release was involved. Run
  **twice**: once on `AppHandle::restart` (which passed, showing the command body does not land on
  the main thread today) and again on `request_restart`, which is what ships.
  Three harness notes for next time: the `dangerousInsecureTransportProtocol` flag is required for
  an `http://` test endpoint (test builds only); **the buttons are unreachable from System
  Events** — AX can't enumerate into the WKWebView and `click at` fails with -25204, so the click
  has to be a real `CGEvent` post (`scratchpad/click.swift`) at coordinates read off a screenshot;
  and **gate that click on the test app actually being frontmost, by pid**. A CGEvent goes to
  whatever is under the point, so an un-gated click lands in whatever app is in front (here, the
  user's terminal) — and `tell application "Mulpex" to activate` resolves *by name*, so it can
  raise or launch the installed `/Applications` bundle instead of the one under test.
- **The release pipeline** was proven by `scripts/release.sh --dry-run`: signed `.tar.gz` + `.sig`
  and a well-formed `latest.json`, without publishing. That dry run is what caught the
  `TAURI_SIGNING_PRIVATE_KEY` naming trap.
- **Auto-update confirmed in production, by the user**, on the real GitHub endpoint: an installed
  v0.4.7 offered v0.4.8, applied it, and relaunched — no Gatekeeper prompt and no second `xattr`,
  which is the claim that mattered. Each published release is additionally checked by re-fetching
  the tarball GitHub actually serves and comparing its SHA-256 to the signed local one, so a
  mismatched or truncated asset can't sit there verifying against nothing.
- **Mute (⌘M).** Exercised against a real running app on an isolated `HOME` with three restored
  sessions. Menu shape read out of the **accessibility API** rather than a screenshot (`Mute
  Session` key=M mods=0, `Messages` key=M mods=1, `Minimize` key=missing) — AX reaches the menu bar
  even though it can't enter the WKWebView. Then: ⌘M dims/sinks/marks and ⌘M again restores the row
  to its original slot with dot and status back and the tick cleared; ⌘] from #3 lands on the muted
  #2 at the bottom rather than wrapping to #1, which is the case where visible order and creation
  order actually differ; the store file writes the awkward `<uuid>\t\tmuted` row and the mute comes
  back after a restart. **Badge exclusion was measured by forging the hook state files** — writing
  `needs` into `state_dir/<id>` and dropping files into `inbox/<id>/` is exactly what the hook and
  a peer's `hub_send` do, so the UI sees genuine input: with two sessions in `needs` and 3 unread,
  the tab read red **1** / amber **1** and the strip `1 unread`. Plus 13 assertions on
  `displayOrder`/`needsCount`/`unreadCount` driven through the real `stores.ts`, and 3 `persist.rs`
  tests including both pre-mute file formats. **Not verified:** clicking the row's 🔇 without
  selecting it first — the keyboard path proves the handler, only the hit target is untested.
- **The launch-time check** was verified separately, because "no banner at startup" had two very
  different possible causes (silent failure vs. still in flight). Running the *published* 0.4.7
  against the real endpoint under an isolated `HOME` with no projects, screenshotting at 6/15/30/50 s
  and never touching the menu: banner present at 6 s. That ruled out the check and pointed at the
  `ready` gate (see **Auto-update** in [packaging.md](packaging.md)).
- **Attention / tab drag / spawn injection (unreleased, post-v0.5.0).** `svelte-check` (120 files,
  0 errors) and `cargo check` are clean at `6a6b653`. Whatever live exercise these got happened
  before this doc entry existed — anything not recorded here should be treated as unconfirmed and
  re-driven in the real app: the dock badge needs a claude actually in `needs`, a banner needs the
  window unfocused *and* macOS notification permission granted for the bundle, and the injection
  fix is only meaningfully tested by a multi-child `hub_spawn` (the one-child case passed under
  the old code too — that's exactly why the bug shipped).
- **Terminal sessions (shipped in v0.6.0).** Everything below the GUI is measured; the headline GUI
  flows were driven too — see the two entries at the end.
  - **The grid, on real captured PTY bytes.** `cargo build` and `vite build` were recorded through
    a real pty at 32×120 (`TERM=xterm-256color`, the same shape a Mulpex terminal gives its child)
    and committed as fixtures. Replayed through `Screen`, cargo's **17** progress-bar repaints
    collapse to nothing and the log is the 8 lines a human read; vite's column-aligned size table
    survives byte-for-byte. This is the case a line-based stripper fails, which is why it is the
    one that got measured. Plus 16 unit cases: CR rewrite, `CUU`+`EL` repaint, `ED 2` preserving
    history, OSC 0 titles dropped, OSC 8 link *text* kept, alt-screen suppressed, UTF-8 and CSI
    split across chunk boundaries, wrap/scroll, IL/DL, resize.
  - **The log file:** header round-trip (shared `termlog`, so both sides parse the same bytes),
    and a 2 MB write proving `base` advances, the cut lands on a line boundary, the oldest lines
    go, and **content written after a trim is still there** — the assertion that would catch a
    rename-based trim orphaning the writer's fd.
  - **Real shell → recorder → file**, with nothing simulated: a `$SHELL` on a real PTY running
    `seq 1 200` puts the scrolled-off lines in the log, the last screenful in `<id>.screen`, and
    no escape byte in either.
  - **Lifecycle**, against real shells: a self-exited terminal is kept and marked, is absent from
    `instances` and from the store, shows `exited` in the manifest, and only an explicit close
    removes it (and its log + cursors); an idle exited terminal does not make `reap_dead` rewrite
    the session store on every tick; the `termreq` handshake opens, delivers real keystrokes to
    the shell, refuses a bad id, and closes.
  - **Teardown leaves nothing**, measured two ways — a PTY harness replicating the spawn shape,
    and two tests through the real code. A same-process-group descendant (the `claude` shape) dies
    with the `killpg` and leaves no zombie; a shell's **backgrounded** job, which `killpg` cannot
    reach, dies with the tty sweep. That second test was confirmed to **fail** with the sweep
    disabled, so it is a real regression test and not a tautology.
  - **The MCP surface driven end to end against the real `mulpex-helper` binary** over stdio, with
    a stand-in poll loop: 10 tools listed; the opener sees its terminal's whole life while a
    different instance's first read is a flagged, capped tail; second reads return only what's new;
    the `printf` plumbing never reaches the model; completion + exit code reported; a send while a
    command is still running is *not* wrapped; Ctrl-C accepted and an unknown control key is a tool
    error rather than silence; a timed-out wait loses nothing. And the reason the transport went
    concurrent: **a `hub_instances` issued while a 30 s blocking read is parked comes back in
    < 1 s.** (`scratchpad/drive_mcp.py`.)
  - **The six gap fixes, through that same real binary** (2026-08-04, 58 checks, all pass): the
    stale mark retired so no completion is claimed for an untracked send; a heredoc reaching the
    shell with its terminator intact and untracked; Ctrl-C clearing the mark so the *next* command
    can be tracked again; a screen read with no `printf`/marker in it; a seeded
    `awk '{ print $1,   $3 }'` arriving with its spacing intact and a multi-line seed keeping its
    layout; and `wait_ms` sitting through partial output until the marker lands, reporting
    `waited_for`, and running for `full` reads too. **Confirmed non-tautological**: reverting
    `mark_action` to the pre-fix rule fails 9 of them, reproducing the original live symptoms
    verbatim.
  - `cargo test` (55: 33 app + 22 core) and `cargo clippy` clean — the only two clippy warnings are
    pre-existing (`hook.rs` needless-return, `persist.rs` items-after-test-module). No frontend
    change, so `svelte-check`/`vite build` were not re-run for the gap fixes.
  - **Driven in the real window (2026-08-05, v0.6.0 build).** ⌘⇧T opens a shell: the row renders
    `$ term #N` with the `running` readout, the pane shows a real interactive prompt (zsh rc files
    loaded, git branch in the prompt — proof of `-l -i`), and a typed `echo … && exit` leaves the
    row **`exited` and still readable**, which is the whole point of keeping it. The app was
    rebuilt and installed, so the gap fixes now run live too rather than only through the helper
    binary.
  - **Still not verified:** an instance-opened terminal appearing without stealing focus, stdin
    going dead on an exited row, and RTL/colour inside a shell pane.
- **Remote claude peers (`hub_remote_open`).** Proven end to end **against a real VM**
  (`state::remote_peer_live`, `#[ignore]`d, run with `MULPEX_TEST_SSH=…`): a local instance's
  terminal ssh'd in, a remote `claude` started under the base64'd peer rules, ran the task it was
  given, emitted `<<<MPX <token> done …>>>`, and a hub message landed in the driver's inbox reading
  "has FINISHED the work you gave it. It says: cwd is /tmp/mpx-probe" — with the token stripped from
  everything a model can read. That test **failed twice before it passed**, and both failures were
  real bugs, not harness noise: the backstop firing before the task was typed, and the `\r` being
  swallowed as paste content so the task sat unsubmitted in the input box.
  The **already-connected** flow is proven live too
  (`a_claude_launched_into_an_already_connected_terminal_signals_home`): a terminal ssh'd in by hand,
  `claude` launched into it on the far side with no `ssh_target` at all, task delivered, and the wake
  read "has FINISHED the work you gave it. It says: echo attached ran; output: attached". That test
  also failed first — on `➜  ~` — which is how the prompt-detection defect above was found.
  Offline: 16 `remote.rs` unit tests (marker grammar, wrap-at-every-position, foreign/missing token,
  strip, base64 vectors, and one asserting the rules' own example parses — the two halves of the
  contract cannot drift); 2 watcher tests against real shells standing in for a remote, one of them
  confirmed to fail with the `remote_awaiting` guard removed; 4 `mcp.rs` tests for the refusals and
  the read integration; and 3 `vtgrid` replays of real captures — the markdown-eats-underscores
  measurement, and `a_real_alt_screen_remote_claude_stays_readable`, confirmed to fail with *"the
  alt screen rendered nothing — the driver would be blind"* when the old early-return is restored.
  `cargo test` 93 (48 app + 45 core) green, `clippy` clean but for the two pre-existing warnings. No
  frontend change, so `svelte-check`/`vite build` were not re-run.

  **Driven in the real GUI (2026-08-09), which is where the three field bugs came from.** A real
  instance called `hub_remote_open` itself against a terminal the user had ssh'd in by hand, named
  itself `remote claude on tickets VM`, launched the remote, and drove a substantial cross-machine
  investigation with it. What that exposed, none of which the harness could have: the recorder going
  dark on the alternate screen (`<id>.screen` at **0 bytes** — the driver could read *nothing*), the
  completion sentinel being appended to the task text the remote reads as its prompt, and the
  screen-only history limit. All three fixed and pinned; the first two were **confirmed against the
  live scratch dir**, where `<id>.screen` went 0 → 7,658 bytes after the fix. **Still not verified:**
  the *idle wake* specifically — a driver that has ended its turn being woken by the hub message
  rather than reading the terminal within a turn it was already taking.
- **Login-shell environment forwarding (the "Not logged in" bug).** Diagnosed by reading the live
  process environments rather than by inference: the running app (`ps eww`) held exactly the bare
  LaunchServices set — `PATH=/usr/bin:/bin:…`, no `TERM`, no `LANG`, no token — and so did the
  claude it had spawned, while `~/.claude/.credentials.json` did not exist at all. The premise was
  then isolated in one command: the same `claude` under `env -i PATH=… HOME=…` answers
  `Not logged in · Please run /login`, and with `CLAUDE_CODE_OAUTH_TOKEN` added answers the prompt.
  Offline: `an_rc_file_export_reaches_a_spawned_child` exports a value **only** from the probe's
  `$ZDOTDIR/.zshrc`, then removes `ZDOTDIR` before spawning — so the child cannot source that file
  itself and the value can only arrive by being forwarded (the value carries a newline, which is
  what `env -0` is for). Confirmed non-tautological: with the forwarding loop removed it fails with
  `tok=[]`, the live symptom. Plus a denylist test pinning both directions.
  `cargo test` 54 green, `clippy` clean but for the pre-existing warning, `svelte-check` 120 files
  0 errors.
  **Driven in a real signed bundle**, launched with `env -i` in the LaunchServices shape and an
  isolated `HOME` whose only credential was a `.zshrc` export: the app process itself had no token,
  the claude it spawned via the File menu **did**, and that instance reached `Welcome back!`, ran a
  turn, answered, armed its hub listener and named its own row. Two false alarms along the way,
  both first-run wizardry on a fresh `HOME` rather than auth: the onboarding login screen, and the
  bypass-permissions warning — whose default selection is **`No, exit`**, so a stray Return kills
  the instance and it reaps like a normal exit.

- **Compaction status.** The hook sequence was captured from a real `/compact`
  (`scratchpad/compactprobe2.py`), which is the only way the two silences show up: no
  `UserPromptSubmit` for the slash command, and nothing at all between `PreCompact` and
  `SessionStart[source=compact]`. The first probe attempt is worth remembering — the conversation was
  too short and Claude Code answered *"Not enough messages to compact"*, so `PreCompact` fired with no
  `SessionStart` after it. That accident is what surfaced the refused-compact path the flag has to
  survive. `cargo test` 134 (59 app + 73 core + 2 helper) green, `clippy` clean but for the two
  pre-existing warnings, `svelte-check` 120 files 0 errors. Driven through the real helper across
  eight cases. **Not verified in the real window** — the settings template only changes for sessions
  started after a relaunch.
- **Stable instance numbers.** Diagnosed from the code and the on-disk stores rather than by
  re-driving the GUI: the store files under `~/.mulpex/sessions/` carry no id column, and
  `Core::open` assigns `sessions.len() + 1`, which is the whole of the renumbering. The
  order-drift half was found by reading the two `sticky` push sites, both of which discard the
  record's position. `cargo test` 130 (59 app + 71 core) green, `clippy` clean but for the two
  pre-existing warnings, `svelte-check` 120 files 0 errors. Both halves confirmed to fail when
  reverted. **Not verified in the real window** — and note the first launch after this ships still
  renumbers once, because the existing stores have no ids to restore; the save that follows writes
  them, and from then on the numbers hold.
- **Background work vs. `needs`.** The hook surface was measured, not assumed: a probe
  (`scratchpad/agentprobe.py`) registered every lifecycle hook against a real `claude` v2.1.234,
  launched a background agent and recorded all 29 events with their full payloads. That is what
  showed `Stop` firing *while* the agent ran, `background_tasks` riding on its payload, subagents
  inheriting the parent's hooks (their `Stop` arrives as `SubagentStop`, which Mulpex does not
  register, so they never touch the parent's status), and — from a second, longer probe — the
  `idle_prompt` notification landing exactly 60 s after `Stop` carrying no task information at all.
  `SubagentStart`/`SubagentStop` counting was the obvious design and is not needed: `Stop` already
  knows. `cargo test` 125 (56 app + 69 core) green, `clippy` clean but for the two pre-existing
  warnings. Then driven through the real `mulpex-helper` binary across all six cases.
  **Not verified in the real window** — the settings template only changes for sessions started
  after a relaunch, which was deliberately not done.
- **One geometry (the stale-last-few-lines bug).** Reproduced against a real `claude` before it was
  fixed, not reasoned about: tmux at 204x55 shows a clean blank row where the app showed leftover
  prompt text, and replaying that same captured byte stream through this xterm build at 204x55
  matches tmux byte for byte — so neither Claude Code nor xterm's parser was at fault. Reproducing
  it needed the *size history*: a PTY harness that boots claude at Mulpex's 120x32 spawn size,
  records the byte offset of the resize, and replays with the emulator starting at xterm's 80x24
  default. That leaves permanent debris; starting the emulator at the PTY's size does not. The
  ±200/+2000-byte skew runs are what established that a synchronised resize self-heals, which is
  what bounds the fix to attach time. `cargo test` 123 (56 app + 67 core) green, `clippy` clean but
  for the two pre-existing warnings, `svelte-check` 120 files 0 errors, `vite build` clean.
  **Not verified in the real window** — the fix only takes effect on a relaunch, which was
  deliberately not done (the user was mid-session in the running app).
- **Signing by certificate (shipped in v0.7.0).** The requirement change was verified directly:
  the installed bundle reports `identifier "com.mulpex.app" and certificate root = H"356eabc7…"`,
  `codesign --verify --deep --strict` passes, and — the point of the exercise — a **subsequent
  rebuild installed over it produced no permission prompt at all**, where the previous ad-hoc build
  had re-prompted. The ad-hoc failure was measured first: `codesign --verify -R='cdhash H"<old>"'`
  fails on the new bundle while TCC still records `auth_value=2`.
- **Session drag-to-reorder (shipped in v0.6.0).** The order math was driven through the **real
  `stores.ts`** (transpiled, not re-implemented) — 27 assertions on `clampToGroup` / `dragOrder` /
  `displayOrder` / the `reorderSessions` mutator, including the invariant that every emitted order
  survives re-sorting by `displayOrder` unchanged, both clamp directions, and the never-drop
  contract. Backend: `reordering_sessions_keeps_focus_and_never_drops_one` against real sessions.
  `cargo test` (36: 34 pass, 2 pre-existing ignored) + `clippy` clean, `svelte-check` 120 files
  0 errors, `vite build` clean. **Not verified:** the gesture itself — the pointer/threshold/
  indicator behavior and the clamp *as felt*. The v0.6.0 build is installed, so this can now simply
  be driven; it just hasn't been.
- **TCC / failed-start visibility (v0.6.0).** The diagnosis is in [packaging.md](packaging.md), with
  the shim-captured `rc=1` and the `/private/tmp` control. The fixes were then driven in the real
  window: a failed restore renders `⚠ claude #1 — failed to start` with `claude`'s own
  `No conversation found with session ID: …` still on screen *above* Mulpex's explanation, and the
  row is **kept** (the tab counts it) instead of vanishing; ⌘T on a `chmod 000` project refuses
  before spawning, with the folder name and the Settings path in the notice. Backend: three tests,
  two of them confirmed non-tautological by breaking the code — un-latching the failure mark fails
  `a_kept_failed_instance_does_not_make_every_poll_do_work` on exactly its mtime assertion, which
  is the "does work every 200 ms tick" regression [sessions.md](sessions.md) warns about. `dir_access_error`
  is tested against a real `chmod 000` directory (only root is excused, so it cannot pass
  vacuously).
- **The v0.6.0 release artifact itself.** Signing was verified in the *published* tarball, not just
  locally: re-fetched from GitHub, its SHA-256 matches the signed local build byte-for-byte, and
  the `.app` inside reports `Identifier=com.mulpex.app`, `Sealed Resources version=2`, and passes
  `codesign --verify --deep --strict` — where the published v0.5.0 fails all three. The same
  artifact was installed and launched before publishing: it runs under the hardened runtime the
  bundler adds (`flags=0x10002(adhoc,runtime)`), spawns `claude`, restores projects, and **kept its
  TCC grant** across the swap.
- **Cross-project messaging.** Offline: 17 `registry.rs` tests (the grammar in resolution order —
  including `claude#3` landing local rather than looking for a project called "claude", `term#5`
  refused, `<project>#all` refused; whole-component matching so `one` cannot hit `central-one`;
  the ambiguity error carrying both full paths *and* a suffix that itself resolves; the file's
  write-only-on-change and symlink identity), 6 new `mcp.rs` tests, and 3 in `state.rs`. Two are
  confirmed non-tautological by breaking the code: both bounce tests fail — on exactly their own
  assertion messages — when foreign routing is disabled.
  **Driven end to end through TWO real `mulpex-helper` processes** (`scratchpad/drive_xproject.py`,
  30 checks): a registry laid out as the poll loop writes it, then discovery, a send landing in the
  *other* project's own inbox, both feeds logging it, the recipient reading a sender address it can
  reply to verbatim, the reply arriving back, local messaging unchanged, all five refusals, and a
  closed project vanishing from `other_projects`. **That harness is what found the `same_dir` bug**
  — an instance seeing its own project among the "other" ones, invisible to unit tests whose
  strings agreed by construction.
  `cargo test` 117 (51 app + 66 core) green, `clippy` clean but for the two pre-existing warnings,
  `svelte-check` 120 files 0 errors, `vite build` clean.
  **Driven live in the installed app (2026-08-10)**, with the user's real `cloud` and `central-one`
  open: `cloud#1` found `central-one#1` in `other_projects`, messaged it, and got a reply — a full
  round trip 14 s apart, both feeds byte-identical, both inboxes drained, and the reply sent by
  pasting `sender_label`'s output straight back into `to`.
  **The idle wake is confirmed, and by better evidence than the timing:** `central-one`'s
  `tasks/1` still reads `just say ready` afterwards. A human prompt would have overwritten it via
  `userpromptsubmit`; it survives only because the turn arrived as a `<task-notification>` from the
  instance's own Monitor. That is the wake path, read out of the state files rather than off the
  screen — and it is the same claim listed as unverified for remote peers, now proven for hub
  messages.
  `HUB_RULES`' self-contained rule also landed rather than merely existing: the sender opened with
  "we can't see each other's files" and named its own repo, and the reply volunteered the actual
  shared contract between the two codebases (the `X-Api-Key` ingest endpoints).
- **Row naming backstops (2026-08-10).** The failure was diagnosed off the *live*
  scratch dir rather than reproduced (`armed/6` present, `named/6` absent, `namereq/` empty), and
  the tool was confirmed present in the **installed** helper's `tools/list` before the model was
  blamed. Both fixes are pinned by tests confirmed to fail when the fix is removed:
  `the_naming_nudge_comes_back_once_mid_turn_until_the_row_is_named` (fails with *"the reminder
  repeated within one turn"* if the once-per-turn dedup becomes `>=`) and
  `an_unnamed_instance_gets_a_provisional_name_that_never_persists` (fails on the store assertion
  if the guess is written into `names`). `cargo test` 119 (52 app + 67 core) green, `clippy` clean
  but for the two pre-existing warnings, `svelte-check` 120 files 0 errors, `vite build` clean.
  Then **confirmed by the user in the installed build** — the case that motivated it (a row that
  had sat showing a pasted prompt) now labels itself.
- **Spawned-task delivery (shipped in v0.7.4).** Diagnosed by measurement in this order, because
  the cheap reading was wrong at every step. (1) **Truncation ruled out first**: a Python PTY
  harness replicating `pty.rs`'s injection byte for byte against a real `claude` landed 50 / 2 k /
  8 k / 9 k-char tasks and an 8.5 k Hebrew one (15.5 KB, backticks, `===`, `--flags`) all on
  attempt 1; a separate `openpty` probe showed the tty blocks rather than truncating until ~25 KB.
  (2) **The field instance's own transcript** (`~/.claude/projects/…/<session-id>.jsonl`) showed
  the task arriving intact 91 s after the spawn — so the question was never "where did it go" but
  "why so late". The scratch dir gave the spawn instant (`spawn/` mtime) and `messages.log` gave
  the spawner's manual re-send 19 s *before* the real arrival. (3) **The readiness check was then
  run against a real `claude` cold-starting in the reporting project**: `input_box_ready` never
  matched — zero `╭`/`╰`, 408 `─` — so the injector could only type at the 90 s ceiling, while the
  identical harness in a scratch project matched at 0.5 s. After the fix, re-measured in that same
  project: **readiness at 1.1 s instead of never**. `cargo test --workspace` 135 (60 app + 75 core)
  green, `clippy` clean but for the pre-existing `hook.rs` warning. The chrome test is confirmed
  non-tautological (fails *"ruled input area not detected"* when reverted).
  **Not verified in the real window** — the fix only takes effect for instances spawned after a
  relaunch, which was deliberately not done while the user was working inside the app.

