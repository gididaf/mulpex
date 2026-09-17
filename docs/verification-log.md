# Verification log

What has actually been measured, driven, or proven — and, just as importantly, what has NOT. This is
history: it records the evidence behind claims made elsewhere in the docs, so a later change can tell
"verified live" from "compiles clean". Nothing here is a rule; the rules live in
[CLAUDE.md](../CLAUDE.md) and the subsystem docs.

- Both crates + the Tauri app compile clean; `svelte-check` + `vite build` clean.
- **Sidebar kind-split + context menu driven in a real browser (headless Chrome, 2026-08-22).** The
  actual `stores.ts` bundle was exercised for the ordering (kind split, mute sinking, all four
  clamp boundaries, and an exhaustive `dragOrder(from,to)` sweep proving every emitted order is
  already-grouped and loses no row), and the actual `InstanceList` / `ContextMenu` components were
  mounted and clicked for the rest: the rendered order and the single divider above the first
  terminal, right-click reporting the clicked row *without* selecting it, the empty-space menu
  firing exactly once (and never in addition to a row's), edge-flipping at the viewport corner,
  arrow-key navigation skipping the separator, Enter/Escape/click-away, and that the open menu holds
  `document.activeElement`. **That last one caught a live bug:** `focus()` on the still
  `visibility: hidden` menu was silently ignored, so keystrokes kept reaching the terminal
  underneath — invisible by inspection, obvious the moment activeElement was read.
- **NOT verified:** the context menu's clipboard write inside WKWebView. `navigator.clipboard`
  works in Chrome (where it was tested) and there is an `execCommand` fallback behind it, but which
  of the two actually runs in the shipped app is unmeasured.
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

- **Plan explanations (`ExitPlanMode`), 2026-09-01.** Driven on a real `claude` v2.1.252 on a PTY
  with Mulpex's own flags before a line of product code was written (`scratchpad/probe`: catch-all
  hook settings + a python `openpty` driver). Measured: `PreToolUse[ExitPlanMode]` fires with
  `tool_input = {plan, planFilePath}` at 09:47:18.2 and the approval dialog +
  `Notification{permission_prompt}` land at 09:47:24.2 — the hook is **~6 s ahead of the dialog**;
  `Stop` does **not** fire while the plan waits; `permission_prompt` already resolves to `needs`
  through the existing `notify_status`, so the plan hook's own `needs` write is redundancy, not a
  fix. **The trap this cost two probe runs:** `--dangerously-skip-permissions` silently overrides
  `--permission-mode plan` (every payload read `bypassPermissions`, no `ExitPlanMode` ever fired,
  Claude just wrote prose); plan mode is reachable **only by shift+tab**, four presses from bypass
  (measured cycle bypass → auto → manual → accept edits → plan). The pipe was then verified
  **through the real `mulpex-helper` binary** fed that captured payload (writes `needs` + a
  well-formed `explainplan/3`), the **real summarizer** with the real flags on two plans (1.5 KB
  each → one Hebrew sentence, 4–5 s), and the panel CSS in **headless Chrome** against the
  component's own `<style>` (plan = 2px `rgb(152,195,121)` edge + green `תוכנית` tag flush right,
  no overlap with the timestamp; question keeps cyan). Finally **confirmed by the user in a dev
  build**: a real plan on screen, the row red, the one-line explanation in the panel.
  `cargo test --workspace` green (78 app + 87 core), `svelte-check` 122 files 0 errors.

- **`hub_spawn` task truncation, 2026-09-01.** Reported second-hand from another project: spawned
  children were starting on a fragment of their brief. Root-caused and fixed by measurement only —
  every theory formed by reading code was wrong, in order:

  1. *"A length cap in Mulpex."* Wrong. The chain `hub_spawn` → `spawn/<uuid>.json` → poll loop →
     spawn spec was traced and grepped; nothing truncates the task string.
  2. *"The tty input queue drops bytes on overflow."* Wrong. A python `pty.fork` harness with the
     slave in libuv-style raw mode reading 1 s late, written 600 / 1200 / 3000 / 6000 / 12000
     bytes in one `write`: **every byte arrived, at both `IMAXBEL` settings.** The master write
     just blocks. (The plausible-looking BSD `ttyflush`-on-overflow path never fires here.)
  3. **The TUI.** Driving a real `claude` v2.1.252 on a PTY exactly as `pty.rs` did — one
     `write_all`, 400 ms, a separate `\r` — and reading **the child's own transcript `.jsonl`**:
     sent 1200 / 1998 / 3000 / 6000 → **received 1022 every time**, cut mid-word. 1022 = macOS
     `TTYHOG` (1024) − 2. `claude` renders the burst as `[Pasted text #1]` and submits one chunk.

  **Reading the pane could not have found this**, and two attempts to do so wasted time: at ≥1200
  characters the input box shows only the placeholder, so the content is nowhere on screen; and
  asking the child to echo its own prompt back produced a generic greeting. The transcript is the
  only ground truth for "what did `claude` actually receive".

  **Fix verified before writing it** (argv positional prompt): 3,000 and 12,000-character prompts
  arrive **byte-exact**; argv coexists with `--session-id` + `--settings`; the `UserPromptSubmit`
  hook fires with the **full** 3,000-character prompt (so delivery verification and sidebar naming
  survive); and with the **real `mulpex-helper mcp`** wired in via `--mcp-config`, a child given a
  2,107-character argv prompt called `mcp__mulpex__hub_instances` **on its first turn** — MCP
  readiness before the prompt runs is measured, not assumed.

  **Delivery verification verified through the real `mulpex-helper` binary**, fed a
  `UserPromptSubmit` payload the way `claude` invokes it: a 6,124-character expected prompt
  truncated to 1022 → verdict `partial`; the whole prompt → verdict cleared. Both cases consume
  the expectation file.

  **The shell-seed path (`hub_terminal_open`) was measured too, and is NOT affected**: a real
  `zsh -l -i` on a PTY ran seeded commands of 348 / 1048 / 1648 / 4148 / **9148** characters, all
  in full. zsh's ZLE appends input chunks; `claude`'s paste handling replaces. No change made.

  **Contradiction worth keeping:** [hub.md](hub.md) records 2 k / 8 k / 9 k tasks landing intact
  on claude **v2.1.235**, and a 10,213-character prompt reaching a child's transcript whole. That
  measurement was not wrong — the behaviour changed under an upgrade nobody here made. Which is
  the actual lesson: a TUI is not an interface.

  **NOT verified:** the tail-keeping variant the field report described (its brief began mid-word
  and ended with Mulpex's trailing boilerplate). Only head-keeping reproduced here, across every
  size tested. The retry loop submitting a later fragment as a second queued prompt is a
  *hypothesis*, never measured; that loop is deleted, so the question is now unanswerable rather
  than answered. Also not verified in the running app: the fix has not been exercised through the
  GUI, because the user was working inside Mulpex and it was not restarted.
  `cargo test --workspace` green (79 app + 88 core) plus the `#[ignore]`d argv end-to-end test.

- **Remote task truncation (`hub_remote_open`), 2026-09-01.** The same defect as the local spawn
  bug above, in the last remaining typing path: `mcp::inject_task` typed a remote claude's task
  into its TUI with the identical byte pattern (one write, 400 ms, a separate `\r`) into the
  identical program, so the same **1022-character** cap applied.

  **Fixed the same way** — base64 through argv, reusing the wrapper the rules blob already used, so
  none of the shell-quoting questions that delayed this were actually open.

  **The typed command line is not the constraint.** A real `zsh -l -i` on a PTY was driven with
  seeded command lines of 9,148 / 16,148 / 24,148 / 32,148 / 48,148 / 64,148 / **128,148**
  characters — every one executed in full. (This also supersedes the older "the PTY starts
  discarding above ~25 KB" note: not reproducible for a blocking `write_all` into a shell.) The
  real ceilings are on the far side — `MAX_ARG_STRLEN` (128 KiB per single argument on Linux) and
  `ARG_MAX` (1 MiB) — hence `MAX_REMOTE_TASK_CHARS` = 32,000, which **refuses** rather than trims.

  **Verified end-to-end without a remote box.** The exact command line
  `remote::remote_launch_command` generates for a 6,000-character task (8,352 bytes) was typed into
  a real `zsh -l -i` on a PTY, and the `claude` it launched received the task **byte-exact**
  (`exact: true`), read from that child's own transcript `.jsonl`.

  **The ssh hop was then verified too**, on a throwaway Ubuntu 6.8 x86_64 box the user provided.
  Method: install a key (the production path is `ssh -tt` with key auth — there is nowhere to type
  a password), move the real `claude` aside and put a stub on the remote `PATH` that writes its
  argv NUL-separated to a file, type the command `ssh_command` really generates into a real local
  `zsh -l -i` on a PTY, then read the argv back over ssh. Results: the remote process got
  **argc 4** — `--dangerously-skip-permissions`, `--append-system-prompt`, the decoded rules, and
  the task as the **last** argument — with `cwd=/tmp/mpx-probe` and `IS_SANDBOX=1` correct. A
  6,000-character task returned byte-identical; **at the full 32,000-character cap** (42,921-byte
  ssh command line) it returned with a matching FNV-1a hash computed independently on each side.
  So neither ssh, nor the remote login shell, nor the `printf | base64 -d` round trip alters a
  byte. `/root/.local/bin/claude` was restored (verified: `claude --version` → 2.1.226) and every
  `/tmp/mpx-*` artifact removed.

  **Why a stub rather than a real remote claude:** the local run already proved a real `claude`
  receives an argv prompt byte-exact, so the only untested link was transport. A stub isolates
  exactly that, needs no auth on the far side, and spends no model turns.

  **NOT verified:** anything through the GUI — Mulpex was not restarted, the user was working
  inside it. `cargo test --workspace` green (79 app + 90 core).

- **Restart an instance in place (⌘⇧R), 2026-09-01.** Verified **offline only**, and the split is
  worth being precise about.

  **Verified:** the two new `state.rs` tests drive a real `Core` with real `claude` children —
  `restarting_an_instance_keeps_its_row_and_clears_the_dead_childs_state` kills the middle of three
  restored instances and asserts the replacement is alive at the same index with the same id,
  `session_id` and name, with `state_dir/<id>` and `armed/<id>` gone and `failed` cleared;
  `restarting_an_instance_with_nothing_to_resume_refuses_without_killing_it` asserts the guard
  refuses *and leaves the instance alive*. `cargo test` green (81 passed, 5 pre-existing ignores);
  `svelte-check` 0 errors; `vite build` clean.

  **Not re-verified because it is the same code path as the proven one:** `--resume` itself. The
  restart spawns the identical `SpawnSpec::Claude { resume: true }` that `Core::open` has used for
  every launch since v0.1, and "`--resume` appends to the same transcript rather than forking a new
  id" was measured against the real CLI earlier (see [sessions.md](sessions.md)). What the unit
  tests exercise is the in-place *replacement*, not whether the conversation comes back.

  **NOT verified:** anything through the GUI. Mulpex was not restarted — the user was working
  inside it — so the frontend half (`terminals.reattach`: `reset()` + a fresh `Channel`), the native
  confirm dialog, the ⌘⇧R accelerator reaching `is_forwarded`, and the resumed pane repainting at
  the right geometry are all **unproven in the real app**. The `is_forwarded` allowlist entry in
  particular is this repo's classic silent failure: the item builds, shows its accelerator, and
  nothing happens.

- **Qualified Copy address + ⌘⇧←/→ move-project (2026-09-02).** Verified **offline only**:
  `svelte-check` 0 errors, `cargo build` clean. `instanceAddress` / `claudeInAnotherProject` /
  `moveProject` have **no test at all** — the clamp, the `applyProjectOrder` round-trip and the
  qualifier's trigger condition are unproven by anything but reading. Worth knowing that the
  address *claims* in [frontend.md](frontend.md) are not guesses: `send_foreign`'s "naming my own
  project by name is simply a local send" branch and `parse_address`'s `term#…` refusal were read
  in `mulpex-core`, and `registry.rs`'s own tests already pin `p.address(3) == "central-one#3"`.

  **NOT verified:** anything driven through the GUI. Two macOS permissions the agent's shell did
  not hold blocked it — Screen Recording (fixed mid-session by relaunching Mulpex) and then
  Assistive Access, which `osascript` needs to place a synthetic keystroke; without it no key can
  be sent to the app at all, so the accelerators were never fired by anyone but the user. A
  `tauri dev` build *was* launched against `~/.mulpex-dev` with three projects open, which is the
  rig to reuse — the seed is `~/.mulpex-dev/open.txt`, and tab reordering persists straight back
  into that file, which makes it a clean textual oracle for a move (no screenshot needed). Project
  *switching* has no such oracle: nothing on disk records the active project.

- **A menu accelerator loses to xterm's textarea (2026-09-02, observed by the user).** ⌘⇧← / ⌘⇧→
  moved the project tab with the sidebar focused and did nothing with the terminal focused. That
  is the whole reason the shortcut is claimed in the webview; it is a *report*, not an
  instrumented measurement, but it is a clean A/B and the cause (⌘⇧←/→ is an AppKit
  text-selection binding WebKit performs and reports handled) explains it exactly.

  **Still unexplained:** why ⌘⇧[ / ⌘⇧] were dead. They are not a text-editing binding, so the
  focus story above does not cover them, and the AppKit shifted-punctuation hypothesis in
  [../CLAUDE.md](../CLAUDE.md) is untested. **The experiment that would settle it takes ten
  seconds:** click the sidebar (so focus is out of the terminal) and press ⌘⇧]. If it switches
  project, the brackets had the same focus cause as the arrows and the hypothesis is wrong.

- **The Explainer's failure reason, auto-retry and `נסה שוב` (2026-09-03).** The one thing that
  *was* measured is the thing the fix turns on: driving the real `claude -p` with the summarizer's
  exact flags, a bad `CLAUDE_CODE_OAUTH_TOKEN` exits **1** with `Failed to authenticate. API Error:
  401 OAuth access token is invalid.` on **stdout** and an **empty stderr**, and an unknown
  `--model` likewise puts its user-facing sentence on stdout (its stderr carries a different,
  catalog-level message). That is why a real failure entry could only say `exit 1`, and it is
  pinned by `a_dead_summarizer_reports_the_reason_not_just_the_code` over those exact strings.
  The retry's bookkeeping — in-place replace, no coalescing, the stash dying with its row — is
  covered by `a_retry_replaces_its_row_in_place_and_its_stash_is_bounded_by_the_feed`, driving the
  real module singleton. `svelte-check` 0 errors, `vite build` clean, 84 Rust tests pass.

  **NOT verified:** anything in the running app. Mulpex was not rebuilt or relaunched (the user was
  working inside it), so the button's rendering and RTL placement, the in-place `מסביר…`, the
  `seq`-keyed upsert against a live `explain-update`, and `retry_explain` actually being reachable
  through `invoke_handler` are **unproven in the real app** — the last one being this repo's
  classic silent failure (a command absent from the allowlist fails only at runtime). Nothing here
  needed a *summarizer failure to occur on demand*, which is the missing rig: two attempts at one
  (a bogus token, an unreachable `ANTHROPIC_BASE_URL`) both failed — the first because `claude`
  authenticated anyway from outside the env, the second because it retried until the 90 s timeout —
  and that is why the reason logic was extracted into the pure `failure_reason()` and tested
  there instead.

- **`cargo fmt` is not safe to run here (2026-09-03, measured).** This tree is not
  rustfmt-default-clean: one `cargo fmt` reformatted `claude_bin.rs`, `menu.rs`, `pty.rs`,
  `state.rs`, `vtgrid.rs` and parts of `commands.rs`/`explainer.rs` that no one had touched (~700
  lines of noise in a 400-line change, and it re-wrapped hand-formatted single-line struct
  literals). There is no `rustfmt.toml`. It was reverted per-file; see the trap in
  [../src-tauri/CLAUDE.md](../src-tauri/CLAUDE.md).

## The restart wake (2026-09-03)

**Driven, not inferred.** The report that arrived described steps 2-4 as inferred from notification
text; all four were then confirmed against real artifacts before any code changed.

- **The loop, in a live transcript.** `~/.claude/projects/…-dreamvps-cloud/b17543f0….jsonl`:
  IDX 256 the injected turn (`origin.kind: "task-notification"`, `promptSource: "system"`), IDX 257
  the `hook_additional_context` attachment carrying `ARM_LISTENER_NUDGE`, IDX 259 the model arming a
  fresh persistent Monitor. `bvpgm5lxl` (armed the previous day, reported dead on this launch) →
  `bjwjmo0kw` (armed on that wake) is the self-perpetuation, in the log.
- **A `UserPromptSubmit` block really does suppress the turn.** Headless probe: `num_turns: 0`,
  `output_tokens: 0`, `duration_api_ms: 0`, no assistant message.
- **…and it works on a task-notification specifically, which headless could NOT show.** Under
  `claude -p --resume` the notification arrives as `promptSource: "sdk"` and **the hook does not
  fire for it** — only for the real prompt. Reproduced properly on a PTY instead: orphan a
  persistent Monitor by SIGKILL, resume interactively → the hook fires, blocks, and the transcript
  shows `preventContinuation: true` with **no assistant row after it**.
- **End-to-end through the real `mulpex-helper`.** Real `claude` on a PTY, helper wired as the
  `UserPromptSubmit` hook: the wake produced no assistant turn, `armed/1` stayed absent (no new
  orphan — the loop is dead), and the status file was still `waiting`, blocked by the helper's own
  reason string.
- **The non-regression that mattered most.** Mail planted in `inbox/1/` still arrives on a
  Monitor-event wake while the arm nudge is suppressed — the peer snapshot is not gated.

Two traps worth keeping from the session:

- **`CLAUDE_CODE_CHILD_SESSION=1` is in a Mulpex instance's own environment**, so a `claude` spawned
  from a Bash tool inherits it and **writes no transcript at all** — the interactive probe silently
  produced nothing until the variable was scrubbed. Exactly the failure `src-tauri/CLAUDE.md`
  warns about, met from the other side.
- **SIGKILL alone loses the transcript.** The first orphan attempt left no `.jsonl`; matching
  teardown's real SIGHUP→grace→SIGKILL sequence was needed before the session flushed.
- **A stale `target/debug/mulpex-helper` will happily answer probes with the old behaviour.** The
  first helper-level run reported the pre-fix output because the binary predated the edit. Check its
  mtime before believing a probe.

## `mpx` Phase 5 — terminals and the transcript (2026-09-06)

`crates/mulpex-cli/src/recorder.rs` replaces `vtgrid.rs`'s 1,680-line VT emulator with ~470 lines of
capture/diff/append. The on-disk format is unchanged, because `mulpex-core::mcp` reads these files
from inside a `claude` and is shared with the desktop app.

**tmux semantics, measured (not read off the man page):**

- **`capture-pane -a` after a full-screen program exits answers `no alternate screen`.** tmux
  discards it, so "the last frame is taken on the way out" is *not* recoverable after the fact. The
  CLI keeps the newest alt capture in memory and flushes it when `alternate_on` drops — the exit
  frame is up to one tick (200 ms) stale rather than absent. The one place the CLI is weaker than
  the app, and bounded.
- **A failing command aborts the rest of a tmux command sequence.** A window closing mid-tick would
  otherwise blank every terminal after it in the batch, silently.
- **20 sequential `capture-pane` calls: 263 ms. The same 20 batched: 12 ms.** At a 200 ms tick the
  sequential form spends most of the tick in `fork`.
- **`-J` is mandatory** — a 300-character line comes back as 80/80/80/60 without it.
- **`clear` resets `history_size`** (measured 63 → 1).

**Bugs the measurements found, none of which were visible by inspection:**

- **Wrapped lines cut in half.** `-J` joins only *within* one capture, and a line longer than the
  screen scrolls off over several ticks. A 300-character line reached the log as 180. Fixed with
  `vtgrid::scroll_up`'s own rule — a row that continues is written without a terminator — detected
  by capturing one row further (`-E 0`) and comparing line counts.
- **Rows lost *and* duplicated under paced output.** History grows between the scan that reads
  `history_size` and the capture ~10 ms later, so a window of exactly `delta` rows slides: `F_3`
  lost, `F_11` written twice. The row count is fixed when the argv is built, so this cannot be made
  atomic; the window now reaches back 64 rows and alignment comes from **content overlap** with the
  transcript's own tail.
- **`clear` read as "nothing new".** `history_size` goes backwards and `saturating_sub` returned 0,
  then adopted the smaller number: `clear; seq 1 20000` reached the transcript starting at **27**.
- **Frames filed against the wrong screen.** The alt flag was updated in a later pass than the
  capture, so it lagged one tick — and it is wrong on exactly the two ticks that matter. The only
  frame recorded for a full-screen program was the shell prompt drawn *after* it quit.
- **A settled full-screen program never got its frame.** The interval flush was gated on the screen
  changing *this tick*; a file open in a pager changes it once.
- **`hub_terminal_read` immediately after `hub_terminal_open` answered "no term#N"** — the log was
  created by the poll loop a tick later. The desktop app creates it synchronously; now so does this.

**Driven end-to-end** (real tmux, real `mulpex-helper mcp` speaking JSON-RPC as a `claude` does):
wrapped 300-character line intact; 43 paced lines with 0 lost and 0 duplicated; a 19,935-line
firehose contiguous with 0 breaks; repaints collapsed; `command_running`/`cwd`; a seeded open; two
readers with independent cursors; frames holding what the pager drew while the post-exit screen does
not; an exited terminal still readable; and a completion marker surviving the transcript to yield
exit 7.

**Confirmed pre-existing and deliberately NOT fixed — this is shared `mcp.rs`, so it is the desktop
app too.** `command_finished` and `wait_ms`'s completion wait **do not fire for a command that has
finished.** The `__MPX_DONE_` marker is the last thing the shell prints, so it is still on screen,
and `find_completion` searches the **log** only. Driven by hand against the shipped desktop app
twice — `sleep 2; echo hi` and a 120-line command both returned `timed_out: true` and "The command
you submitted is still running", with the output plainly visible on `current_screen`. It resolves
only once later output pushes the marker into the history. The CLI reproduces this exactly.

**Semantic change, documented rather than rediscovered:** `last_out_ms` is screen-change-based, not
per-byte — a tick is the finest grain a poller has, and output that redraws the same screen is not
counted as activity. `idle_ms` is therefore coarser, and larger for a repainting program, so
`remote.rs`'s silence backstop fires slightly sooner — the safe direction, since it only ever asks
whether a turn has ended.

## `mpx` Phase 6 — fan-out, restart-in-place, mute, multi-project (2026-09-06)

Driven with **six real spawned children**, because the one-item case always passes.

**`hub_spawn` with six tasks.** All six created, drip-fed 500 ms apart; every one had its task
**delivered and verified** (`tasks_delivered: [2,3,4,5,6,7]`, nothing mangled, nothing pending); all
six named themselves through `hub_set_name` and got their `named/<id>` flag; all six armed their
hub listener (`armed/` held 2–7); and all six reported back to `claude#1` with the right payload.

**The bug that found:** the delivery watchdog treated *absent from the scan* as death, and the scan
a tick runs on predates the windows that tick creates. So the **last child of every batch looked
dead the instant it was born** — the six-task call came back after 3.9 s with `ok: false` and
"these instances exist but NEVER received their task: [7]", while claude#7 went on to do the work
and report back. A loud wrong answer is still a wrong answer. A just-spawned child now gets a grace
period before its absence counts.

**Restart-in-place** (`mpx restart 2`, the CLI's ⌘⇧R). tmux's `respawn-pane -k` replaces the
process inside the window, so this is structural rather than careful bookkeeping: window id, window
index, name and every `@mpx_*` option survive because the window is never destroyed. Verified — the
pane pid changed (9013 → 27567) while `claude#2 ▸ ACK to claude#1` kept its number, its name, its
position and its `@mpx_session_id`; `armed/2` was cleared and **came back** (the instance re-armed);
`resumed/2` was written and consumed by the hook; and a message planted in its inbox beforehand was
still delivered, draining to empty.

**The session uuid lives on the window** (`@mpx_session_id`). Nothing else records it and nothing
else has to: the window and the conversation live and die together, which is the same reason
`@mpx_id`/`@mpx_kind` are there.

**Mute** is a window option too. `mpx mute 3` shows in `mpx ls`, `unmute` clears it, and muting a
terminal is **refused** rather than half-honoured — a terminal produces none of the signals mute
silences.

**Multi-project.** Two projects on one tmux server and one state root: `registry.json` listed both
with terminals correctly absent (`p6other -> [1]`, `p6proj -> [1..7]`, no `term#8`);
`hub_instances` reported `other_projects`; `hub_send` to `p6other#1` was delivered across;
`to: "all"` went to `[2,3,4,5,6,7]` and left the other project untouched, confirmed in both
projects' `messages.log`. Tearing down one project left the other running ("1 other project(s)
still running") — never `kill-server`.

**Teardown.** `mpx down` on the fanned-out project swept 7 processes that ignored the hangup and
left zero claudes, zero `exec-spec` launchers and no daemon referencing that scratch root.

## `mpx` Phase 7 — messages feed, RTL, remote peers (2026-09-06)

### RTL is settled, and checked against Phase 0's reference

`bidi.rs` calls the Unicode Bidirectional Algorithm (`unicode-bidi`) on text `mpx`
itself prints — the messages feed and the instance list, both of which carry model-written
text in the language the user works in. (A `hub_spawn` child named itself in Hebrew the first time
Phase 6 was run, so this is not hypothetical.)

Verified against `fribidi 1.0.16`, the reference Phase 0 used and the output the user confirmed
readable, on Phase 0's own mixed case `תיקנתי את pty.rs ואת state.rs`:

```
mpx     : 0073 0074 0061 0074 0065 002E 0072 0073 0020 05EA 05D0 05D5 …
fribidi : 0073 0074 0061 0074 0065 002E 0072 0073 0020 05EA 05D0 05D5 …
IDENTICAL
```

Compared as codepoints, never read off a screen — transcription re-applies BiDi and hides which end
is which. The Latin identifiers stay left-to-right inside the reversed Hebrew, which is exactly what
the hand-rolled reverse-the-runs attempt got wrong in Phase 0. It stays a setting (`MPX_BIDI=off`):
a terminal that implements the UBA itself would apply it twice and put the text back.

### Remote peers, driven against a real box

A real remote `claude` on `185.131.144.28` over ssh, signalling back to `claude#1`:

- the signal reached the driver's inbox as `from: 0`, `from_terminal: 2`, with the
  `<<<MPX …>>>` marker **stripped** and the reply instructions naming `hub_terminal_send`
  (a remote is a terminal; `hub_send` can never reach it);
- **the same signal woke it once, not once per tick** — the count held at 1 across ~60 ticks,
  with the fingerprint on disk in `terminals/remote/2.seen.watch`;
- a **new, distinct** signal woke it again, and advanced the fingerprint.

**NOT verified live: the silence backstop.** It fires only when a remote finishes *without*
signalling, and this remote signalled every time, including when told not to. Its bookkeeping — owed
once when the driver speaks, settled once when anything is delivered — is unit-tested, but the live
path was never entered. Say so rather than claiming it.

### Three findings about shared code, none of them fixed here

1. **`ssh -tt <host> '<cmd>'` does not source `.bashrc`, so the remote's token is invisible.**
   Measured directly: `token via ssh -tt: MISSING` on a box whose `.bashrc` exports
   `CLAUDE_CODE_OAUTH_TOKEN` from a token-manager block. `remote::ssh_command` builds exactly that
   shape, so `hub_remote_open` with an `ssh_target` would launch a **logged-out** claude there. The
   documented `terminal_id` path (ssh in yourself first, then hand the terminal over) works and is
   what the whole verification above used. This is `mulpex-core`, so it is the desktop app too.
2. **`hub_remote_open` timed out on a first-run trust prompt.** The remote claude asked
   "Is this a project you created or one you trust?" and sat there; the 30 s readiness wait expired
   and the note offered a guess-list — "ssh may be asking for a password or a host key, the
   directory may not exist, or claude may not be installed there" — that does not include what
   actually happened. Answering the prompt let everything proceed normally.
3. **An echoed `<<<MPX` swallows the next real signal.** `find_signals` walks openers in order; a
   *partial* marker with no closer of its own consumes forward to the next real marker's `>>>`, and
   that real signal is lost. Provoked here by a probe message that contained the literal marker
   text — which is the thing the code already warns about ("showing it to the reader invites it to
   imitate it") — and it persisted for as long as the echoed line stayed on the remote's screen.
   Cleared by `/clear` on the remote, after which the pending signal was delivered immediately.

### Also

`mpx messages` reads `messages.log` **straight from disk, never the daemon** — the feed is what you
reach for when something looks stuck, so it must not be blockable by a busy poll loop. Every row
carries a reply-able address (`claude#2`, `central-one#3`) because an inbox is drained the moment it
is read: by the time anyone looks, the feed is the only record. `C-q M` opens it in a
`display-popup -E`, which floats and resizes nothing — the one binding shipped ahead of the rest of
the key scheme.

---

## UI-2a — the key scheme, measured (2026-09-06)

The plan deferred the key scheme and said the open questions were "which prefix, whether a second
tier of bindings is worth it at all". Both were settled by driving a real tmux client on a pty
(`pty.fork()` → `tmux attach`) rather than by reading man pages, because **`send-keys` cannot test
a key binding**: it writes bytes into a pane and never passes through the key tables at all. The
first probe used it and proved nothing.

### Which Ctrl keys can be bound with no prefix

`bind -n <key>` in the root table, one key sent per second from an attached client, each binding
appending its own name to a file:

```
C-]  C-\  C-^  C-_  C-Space  C-s  C-g  C-t     all fired
C-[                                            fired the Escape binding
```

So a bare Ctrl key is available and costs no prefix — but `C-[` **is** byte `0x1b`, the same byte
as Escape, and `C-M`/`C-I`/`C-H` are likewise Enter/Tab/Backspace. This is the same wall the legacy
terminal Mulpex hit: its own `CLAUDE.md` records `Ctrl+]` (next) always working and `Ctrl+[` (prev)
working *only* under the kitty protocol.

**tmux hides this rather than reporting it.** `bind -n C-[ …` is accepted, exits 0, and
`list-keys -T root` prints it on its own line next to a separately-bound `Escape` — so every signal
says the binding exists. It simply never fires. A real event with nowhere to arrive, in the tool
itself.

### What was chosen, and why one key

`Ctrl-]` focuses the sidebar and toggles back. Of the keys that do fire it is the only one that is
not a signal character (`C-\` is QUIT), not claimed by readline or claude, and not a three-finger
stretch (`C-^` is Ctrl-Shift-6). Everything else is a **plain unmodified letter inside the
sidebar**, which is what makes one key enough: the free bare-Ctrl budget is about four keys and
Mulpex has fifteen actions, but a pane that owns the keyboard has the whole alphabet.

### Driven end to end

`scratchpad/ui2a/drive.py`, two projects, one attached client, every assertion read back out of
tmux rather than off the screen:

- `Ctrl-]` into the sidebar and back, repeatedly, from the first second the client is attached.
- `t` opened a terminal, `n` added a claude, both appearing as new `@mpx_id` windows.
- The sidebar rendered all three rows plus the key hints.
- `x` prompted `close claude#3?`; a key that was not `y` cancelled and closed nothing; `y` closed
  exactly one.
- `]` / `[` moved the client between projects.
- `M` floated the feed, waited for a key, and handed the keyboard back to the sidebar.
- `mpx down` on both left no session and no stray `claude`.

### Two findings

1. **`-f` is read once, when the server starts.** The first run of the new binding did nothing at
   all with a correct config on disk: the tmux server had been up since before the file changed, and
   `-f` is not re-read for a new session on an existing server. `mpx` now `source-file`s its own
   config before every attach (`Tmux::source_conf`). Without it, an `mpx` updated underneath a live
   session keeps the old keys until the last project is torn down — a bug report with no visible
   cause.
2. **`display-message -p -t =<session> '#{@mpx_side}'` does not resolve to the active pane.** It
   returned the *other* pane's empty value while the sidebar was demonstrably active
   (`pane_active=1` in `list-panes`), so the probe reported the toggle broken when the toggle was
   fine. Read a per-pane option off `list-panes`, never off a session target. This cost one wrong
   diagnosis and is the second time in this project that the measurement, not the product, was the
   thing that was broken.

### Not verified

`x` on the last remaining instance, and `x` on the row whose own window the sidebar is drawing —
the second kills the pane doing the asking. Both are reachable by hand; neither was driven.

### A bug the probe found by accident

Two zero-byte files, `instances` and `terminals/`, appeared **in the repository root** while the
UI-2a driver ran. Cause: `up()` tags a new session with `@mpx_project` and `@mpx_state_dir` in two
separate tmux calls, and the 200 ms poll can land between them. `core::scan` gated adoption on
`@mpx_project` alone, so that one tick built a `Project` whose `state_dir` was `PathBuf::from("")`
— which is not an error and not empty-checked anywhere downstream, because it **joins to a relative
path**. Every write that tick went to the daemon's own working directory.

The repo's own list of failure shapes already names this one: *a default that reads as an
assertion*. Fixed at both ends — `@mpx_project` is now written **last**, so the field that gates
adoption is the one written after everything it implies exists; and `core::is_ours` requires both
tags, which also covers a session tagged by an older build. Guarded by
`core::tests::a_session_is_ours_only_once_both_tags_are_written`.

---

## The three bare keys (2026-09-06, superseding UI-2a's single key)

User feedback after trying UI-2a: *"the keyboard shortcuts still terrible. let's start with CTRL+[
and CTRL+] to immediately move between instances (without focusing on the sidebar) and CTRL+T to
open new instance."* So the sidebar is no longer the route to everything; three keys act directly.

### `Ctrl-[` is bindable after all — but only sometimes

The UI-2a measurement ("`C-[` fires the Escape binding") was correct and incomplete: it was made in
a synthetic pty with no terminal emulator to negotiate with. Re-measured with the same harness,
sending each encoding by hand:

```
raw 0x1b                      -> Escape
ESC[91;5u     (kitty CSI-u)   -> C-[
ESC[27;5;91~  (modifyOtherKeys) -> C-[
```

tmux understands **both** extended encodings, and — the part that makes this safe — raw `0x1b`
still reaches the Escape binding, so `bind -n C-[` does not steal Escape. The failure mode where a
terminal reports legacy keys is `C-[` doing nothing at all, not Escape breaking.

`set -as terminal-features ',*:extkeys'` is what asks the outer terminal for them. tmux was *not*
observed emitting the request in the synthetic pty with or without that line (only bracketed paste)
— a pty that never answers a DA query may be why, but this is **not established**. Whether iTerm2
3.6.10 negotiates it is therefore **unverified here and testable only by pressing the key**.

### `claude` binds ctrl+t

`grep -aoiE 'ctrl\+[a-z]'` over the `claude` binary: `ctrl+x` 48, `ctrl+o` 34, `ctrl+s` 30,
`ctrl+e` 24, `ctrl+b` 22, `ctrl+a` 18, `ctrl+k` 14, `ctrl+d` 12, `ctrl+u`/`ctrl+r`/`ctrl+g` 11,
**`ctrl+t` 10**, `ctrl+n`/`ctrl+f` 8, `ctrl+v` 7. (String counts in a binary, so this shows the key
is referenced, not that it is live in every mode.) Taking `C-t` at the root table therefore costs a
real claude binding — so `C-q C-t`, `C-q C-]` and `C-q C-[` send the literal key through.

### `#{q:...}` is not shell quoting

Measured: `run-shell -b "echo PROJ=#{q:@mpx_project}"` with the option set to
`/tmp/a dir'with quotes` produced that string **raw** — `#{q:}` escapes tmux's own special
characters, not the shell's. So a project directory must never reach a binding's command line. The
binding passes a **pane id** (`%12`) and `mpx key` reads the path back out of tmux, where it stays
a `String`.

The binary's own path is substituted into the config at render time (`main.rs::render_conf`,
`__MPX_BIN__`, single-quoted by us) rather than being looked up on `PATH`: `run-shell` is executed
by the tmux **server**, which carries whatever environment it was first started with.

### Driven end to end

`scratchpad/ui2a/keys3.py` — Ctrl-T twice (the plural) each added a claude; Ctrl-] moved forward
through three instances and wrapped, landing in the claude and never the sidebar strip; raw `0x1b`
moved nothing; both extended encodings of Ctrl-[ moved back; `C-q C-t` and `C-q C-]` changed
nothing in Mulpex; `mpx key new %99999` reported "not in a Mulpex project" rather than failing
silently. Teardown left no session.

### One consequence

The sidebar's project keys moved from `[`/`]` to `<`/`>`. Ctrl-[ / Ctrl-] are root-table bindings
that fire *inside* the sidebar too, so brackets would otherwise have meant "instance" or "project"
depending on a modifier. The sidebar itself is back on `C-q s`; the bare key it had is now
next-instance.

### Not verified

Whether the user's terminal negotiates extended keys — i.e. whether `Ctrl-[` does anything at all
on their machine. `C-q p` is the same command and always works.

### "[ and ] reversed" — they were not reversed, they were arbitrary (2026-09-06)

User report after trying the three keys: *"working but [ and ] reversed."*

`new-window -t "=session:"` with no index makes tmux pick the **lowest free index**, and `up()`
frees index 0 when it kills the placeholder window. So on every project the *second* claude was
created at index 0 — in front of the first — and the third at index 2. `next-window` walks index
order; the sidebar sorts by instance id. With two instances the disagreement reads exactly as
"reversed"; with four it is not reversed at all, it is arbitrary.

**The previous probe could not have caught this.** It asserted that Ctrl-] *moved* to a different
window and that it *wrapped*, and both were true in the wrong order. Direction was never checked.
A test that walks a list must assert the sequence, not that the position changed.

Fixed at both ends:

1. `new_window` now passes `-a -t '={session}:{end}'`, so windows append and index order tracks id
   order. Verified: four instances at indices 1,2,3,4.
2. The keys no longer use `next-window` at all. `mpx key next|prev <window-id>` walks
   `core::scan`'s instance list — the same list, in the same order, that the sidebar draws — and
   focuses `Instance.pane`, which is the instance's own pane, so landing on the sidebar strip is
   not a case that has to be handled. Verified forward `1→2→3→4→1` and backward `1→4→3→2→1`.

Cost of routing a navigation key through a process: **46 ms** per move in a debug build (61 ms
before batching `select-window` and `select-pane` into one tmux invocation and dropping a
`display-message` call by passing `#{window_id}` instead of `#{pane_id}`). Release will be lower.
`-f` is not re-rendered on the key path — it is read at server start and ignored by commands sent
to a running server.

### `run-shell` hijacks the pane you are looking at (2026-09-06)

User screenshot after pressing Ctrl-T: the claude they were working in replaced by a black pane
showing the single line `claude#2`, with a yellow `[0/0]` in the top-right corner.

Nothing was broken — claude#2 had been created correctly, and the claude behind was untouched.
`run-shell` **displays a command's output by putting the current pane into view-mode**, tmux's
copy-mode overlay (`[0/0]` is its position indicator). `mpx new` prints its reply, `println!`ing
`claude#2`, and one line of stdout was enough. Measured directly:

```
before run-shell -b 'echo …'   pane_in_mode=0
after                          pane_in_mode=1  pane_mode=view-mode
after 'echo … >/dev/null 2>&1' pane_in_mode=0
```

`-b` does not prevent it. **A key binding must produce no output at all**, which makes the
status line (`display-message`) the only channel it has: `mpx key` now reports its own failures
there and prints nothing on success, and the bindings redirect both streams.

The feedback for a successful Ctrl-T is the new row appearing in the sidebar, which is always on
screen. This is the same shape as the rest of this list — the mechanism that shows you an error is
the one that also shows you nothing, and `>/dev/null` on the *error* stream is only safe once
something else carries the words.

Guarded by `scratchpad/ui2a/quiet.py`: after Ctrl-T, Ctrl-] and Ctrl-[, no pane in the project is
in any mode; and `mpx key new @99999` puts "not in a Mulpex project" in tmux's message log without
hijacking anything to say it.

One thing that surfaced while testing the error path: **`@mpx_project` is a session option**, so
every window inherits it — including one made with tmux's own `new-window`. "Is this window in a
Mulpex project?" cannot be answered by reading it.

---

## Close, mute, rename, restart (2026-09-06)

Four user decisions, then built: reap on exit mirroring the desktop; `Ctrl-W` with a confirm;
`m`/`r`/`R` as sidebar letters rather than bare Ctrl keys; rename edited inline in the sidebar.

### Reap policy, ported from `Workspace::reap_dead`

A dead instance is **kept** if it is a terminal, or if it died within `EARLY_DEATH_GRACE` (10 s) of
being launched — that row is the only place a failed start's error is ever shown. Anything else has
its window killed. The `✗` prefix **latches** the decision, or a row kept for dying at 2 s becomes
removable the moment the grace elapses and is silently reaped ten seconds later; the desktop hit
exactly that and its comment says so.

"When was this launched" needed somewhere to live: `@mpx_born`, a window option written in
`claudewin::launch` — the one place a spawn and a restart both pass through, so **a restart is a
birth too** and a relaunched claude that dies immediately keeps its row. Read only when an instance
is already dead, so the scan every tick pays for is unchanged. A window with no stamp is treated as
having died early: keeping a row that should have gone is recoverable, removing one holding the
only copy of an error is not.

Driven: three claudes; killing one that had been up 16 s removed its row with no `✗` left behind; a
pane respawned to exit 3 inside the grace kept its row, was marked `✗ claude#2 (exit 3)`, still had
`claude: bad flag` readable on screen, and was **still there 12 s later**.

### `respawn-pane -t <window>` restarts the wrong pane

Found by driving `R` from the sidebar. `restart` passed `inst.window`, and a window target resolves
to that window's **active pane** — which, when you press `R` in the sidebar, is the sidebar. So
`claude` was relaunched *into the instance list*, leaving the real claude running untouched beside
it. Every outward signal was right: same window, same id, same name, same session uuid, daemon
reported success.

Fixed by targeting `inst.pane` (`core::scan` skips sidebar panes, so it is always the instance's
own). `WindowSlot::Replace` now carries both — the pane to respawn and the window that holds the
`@mpx_*` options.

This is the same shape as the `#{q:}` and `display-message -t <session>` findings: **a tmux target
that accepts what you give it and resolves to something else.** Three in two days. Give tmux the
narrowest id that can express what you mean.

### Rename writes a file, never a command line

`r` opens the row itself as a field, pre-filled. On Enter the sidebar writes `namereq/<id>` — the
same file `hub_set_name` uses — so the daemon's existing `process_name_requests` renames the window
*and* writes the `named/<id>` flag that stops the hook nagging the instance to name itself (a human
naming it counts). No new daemon op, and, the real reason: the typed text is arbitrary user input
and `#{q:...}` is not shell quoting.

The field takes **text, not keys** — `n` has to be a letter in a name — and decodes UTF-8 rather
than bytes, because the names here are usually Hebrew and byte-wise backspace would split a
character. Verified: `שלום` backspaces to `שלו`; a stray arrow key cancels rather than inserting
`^[[A`; Esc does not commit.

### Driven end to end

`scratchpad/ui2a/manage.py` and `restart.py`: reap-on-exit and the failed-start keep; `r` typed a
name that reached the tmux window and the sidebar, with Esc leaving it unchanged; `m` toggled
`@mpx_muted` on and back; `Ctrl-W` prompted, cancelled on a key that was not `y`, and closed exactly
one on `y`; `R` on a claude that had never spoken was **refused in words on the footer**, and after
a real turn restarted it into the same window, same number, same name, same session uuid, a
different pid, with the sidebar pane still running `mpx`.

### A message that can only flash (2026-09-06)

*"when we do mpx we see a message for half second something with 'nothing to open'"* —
`mpx: nothing open yet — starting <dir>`, printed by `open_workspace` immediately before `exec`ing
`tmux attach`, which clears the screen. It could never be read. `mpx: re-attaching to <session>`
had the same shape on the other branch.

Removed both, and the directory moved into `up()`'s **error context** instead — the one path where
the screen is still there to say it on. Verified: `mpx` with tmux off `PATH` now answers
`mpx: opening <dir> — nothing was already open, so mpx used the current directory: running 'tmux -V'
(is tmux installed?)`, and a normal start shows no such text at all in its first three seconds.

Which project opened is answered permanently by the tab along the top; it never needed a line of
its own. A near-relative of *a real event with nowhere to arrive*: a real message, with nowhere to
be seen.

### Creating an instance lands you in it (2026-09-06)

*"when we open new claude instance we should auto focus to that, exactly like the GUI"* — and the
GUI's rule turns out to be **who asked**, not what was made. `Core::create_session` and
`spawn_terminal` both take a `focus: bool`: `commands.rs` passes `true` (⌘T, ⌘⇧T — a person), and
`state.rs`'s `hub_terminal_open` handler passes `false` (a claude).

Mirrored without adding a flag, because in the CLI the *route* already carries the distinction:
`Ctrl-T` and the sidebar's `n`/`t` go through `key_action`/`Actions::send` and now focus what they
made; `hub_spawn` (`fanout.rs`) and `hub_terminal_open` (`terminals.rs`) never touch those paths.
A parameter would have been a second answer to a question the call graph already answers.

Which instance to focus is read back out of the daemon's own reply (`claude#3`, `term#12`) rather
than re-derived — two answers to "what was just created" is one more than there should be.

The sidebar's reply arrives on a worker thread, so the wish is *held* (`focus_want`) and spent by
the draw loop once the row appears, with a 3 s bound so a wish that is never satisfiable cannot
hijack a later keystroke. The cursor moves with the keyboard, so the list keeps agreeing with where
you are.

Driven: `Ctrl-T` twice landed in claude#2 then claude#3; `n` landed in claude#4 and `t` in term#5,
in the instance pane and not the sidebar strip each time; and a `termreq/` request written by hand —
which is exactly what a claude's `hub_terminal_open` does — created term#6 and **did not move the
screen**.

### The project picker (2026-09-06)

UI-2b: `p` in the sidebar and `C-q p` anywhere open one popup that is both the switcher and the
opener — open projects, then recents, then anything on disk.

**The risk worth measuring was `switch-client` from inside a `display-popup`.** A popup can only
run a command and close when it exits; there is nowhere for a chosen path to be *returned* to, so
`mpx pick` has to do the switching itself. Whether tmux resolves a client from in there is exactly
the shape that has gone wrong three times here — a target tmux accepts and then resolves to
something other than what was meant (`#{q:...}` not shell-quoting, `display-message -t <session>`
answering for the wrong pane, `respawn-pane -t <window>` hitting the active pane). Driven with a
real attached client: it works, both for a session that already exists and for one the picker
creates a second earlier.

Also driven end to end: Esc closes the popup and changes nothing; typing an absolute path opens a
project that had never been open, spawns its `claude#1`, and lands the keyboard **in the claude and
not the sidebar strip** (the same "who asked" rule as `Ctrl-T`); switching back to a project that is
open works; and the sidebar's `p` reaches the same popup.

**A bug the probe found:** switching to an **already open** project skipped the recents update,
because that branch never reaches `ensure_session`, which is where the recording lives. The list
then said the last thing you *created* was the last thing you touched. Recents is ordered by what
you chose to work in, so both branches record.

The popup's *contents* cannot be captured by tmux — a popup is not a pane, so `capture-pane` cannot
see one and `list-panes` does not list it. The picker's process is the only honest signal that a
popup is open (`pgrep -f "mpx pick"`), and the screen itself was driven separately on a bare pty:
a path lists directories but not files and not hidden ones, typing narrows, `↓` then `⇥` completes
the highlighted row **and descends into it** (children listed, siblings gone), and a bare word with
no `/` browses nothing at all.

One probe lesson worth keeping: a `check` whose predicate was `lambda got: got or <fallback>`
printed `False` and reported PASS. An assertion with an escape hatch is not an assertion.

**Two decisions recorded with it.** The recents list reads mpx's own
`~/.mulpex-cli/recents.txt` *and* the desktop app's `~/.mulpex/recents.txt`, read-only, so the first
`p` on a Mac already knows your projects. This is **not** the collision `statedir.rs` avoids: that
one is `persist::SessionStore`, keyed by project dir, where sharing would hand the same `--resume`
uuid to two claudes and interleave one transcript. A recents file is a list of paths — nothing
resumes off it and nothing is written back to it. And `bind p` overrides tmux's own prefix-`p`
(previous-window) deliberately: that walks windows by *index*, which is an implementation detail
nobody is looking at.

`create_size` exists because the two callers sit on different ttys: `mpx up` is on the terminal the
session is about to fill, while the picker is inside a popup whose tty is a 70%×60% box. Asking the
tmux **client** (`#{client_width}`, no `-t` — a `-t` is a pane target and would be the same
resolves-to-something-else mistake again) is what stops a project being created at the size of the
popup that opened it.

### Ctrl-P, and what a stolen key actually costs (2026-09-06)

*"can we make it CTRL+P and block claude default behavior?"* — yes, and it turns out to be the
cheapest key taken so far. A bare `bind -n` **is** the block: tmux consumes the key and claude never
sees the byte.

Counting references in the claude binary (2.1.263) is the measurement this project already used to
pick `C-q`, but the raw count is the wrong number. What matters is whether the key is a **primary**
binding or an **alias**:

```
ctrl+space  0    ctrl+j 3    ctrl+p 6    ctrl+g 8    ctrl+t 10    ctrl+o 20
```

All six `ctrl+p` uses are "move up", and every one sits beside an arrow and a `k`:
`up:"select:previous", k:"select:previous", "ctrl+p":"select:previous"` — likewise
`scroll:lineUp`, `footer:up`, `messageSelector:up`. zsh binds `^P` to `up-line-or-history`, which
`↑` also does. So taking Ctrl-P removes an **alias** and no capability. Compare `C-w`: only 2 refs
in claude, but readline's `delete-word-backward`, used while typing — by reference count it looks
like the cheap one and it is the most expensive of the five.

Bound as `display-popup` **directly, never through `run-shell`**: run-shell either blocks the whole
tmux server (foreground) or shows its output by dropping the pane you are looking at into view-mode
(background, measured earlier the same day). A popup is tmux's own async construct and has neither
problem.

Driven with a real client on a pty — `send-keys` cannot test a binding, it writes into a pane and
never passes through the key tables. Ctrl-P opened the picker from inside claude and from the
sidebar pane (the root table wins over the pane), Esc closed it, the other bare keys still fired,
and the negative case held: **`C-q C-p` opened no popup**, sending the literal through instead.

### The prefix stops being part of the scheme (2026-09-06)

*"please omit all the C-q or the other shortcuts. i'm not going to use them anyway"* — and then,
on being told `C-q s` was the only door to the sidebar: *"why do we need to open the sidebar?"*

The right question, and the answer is mostly **you don't**. The sidebar is a *display*: it is on
screen in every window and needs no key to be read. Focusing it only ever mattered for the five
actions with no bare key — and four of them are commands (`mpx term`, `mpx mute <id>`,
`mpx restart <id>`, `mpx messages`), which is where they now live. The fifth, rename, stays
sidebar-only on purpose (the typed name must never reach a command line) and is parked.

So the scheme is **four bare keys and nothing else**: `Ctrl-]` `Ctrl-[` `Ctrl-T` `Ctrl-W` `Ctrl-P`
— five keys, four rows. The prefix bindings are all still *bound*, because unbinding them costs
something and gains nothing: `C-q C-w` is the only way to get readline's delete-word back, and
tmux's own pane navigation reaches the sidebar regardless. They are simply no longer advertised.

**One hole this leaves, flagged rather than papered over:** from a claude pane with no terminal
open there is no route to a shell. `mpx term` needs a shell to type it into, and `Ctrl-T` makes a
claude. Today the answer is a second ssh session; if that turns out to bite, new-terminal wants the
one remaining free key — `ctrl+space`, measured at **zero** references in claude and only
`set-mark-command` in zsh.

### A cursor on a pane that receives no keys (2026-09-06)

*"what is the right arrow near claude#1? old icon we need to remove?"* — not old, but dead as of
the same afternoon. The `▸` gutter is the **keyboard cursor**: which row `↑↓` is on. Once the
sidebar stopped being something you focus, that cursor could never move off row one — a statement
about where the next key lands, drawn on a pane that gets none.

So the sidebar now asks tmux for two facts rather than one (`#{window_active}#{pane_active}`, one
query) and draws the gutter **only while it holds the keyboard**. The reverse-video "you are in
this window" mark is a different fact and does not depend on focus, so it stays either way — the
two were always separate and this is where that pays.

The footer follows the same rule, because which keys are worth writing down depends on which keys
can arrive: unfocused it advertises `^] ^[ move · ^T new · ^W close · ^P project`, the four that
work from anywhere, and the letter list comes back only when the letters can.

Verified by reading the real pane with `capture-pane` on a driven client, in all three states: no
`▸` with the claude focused, `▸` plus the letters after `C-q s`, and gone again after `q`.

### "I tried to close a claude and the whole thing stuck" (2026-09-06)

Nothing was stuck. The tmux server answered every query instantly; the project simply had one
window, holding a dead pane with nothing in it.

What the daemon's own log said: `cloud claude#1 died before it started`. What tmux said about the
same pane: `dead=1  status=0  born=…` — a **clean exit**, seconds after the project was opened.
`reap_dead` decided on age alone, so a claude the user had just quit was filed as a failed spawn
and kept, marked `✗ claude#1 (exit 0)`.

Then the second half: a claude drops the alternate screen on its way out, so the pane the policy
preserved *to show the reason* was blank. Black rectangle, keyboard pointed at a pane that accepts
nothing, `Ctrl-]`/`Ctrl-[` with nowhere to go because there was exactly one instance. `Ctrl-T` and
`Ctrl-P` still worked the whole time — a live program that looks dead, which is this repo's
"a real event with nowhere to arrive" seen from the user's side.

**Fix: the exit status decides, together with the age.** `#{pane_dead_status}` was already on
`Instance` and already recorded in the Phase 0 findings as free; the policy just never read it. A
row is kept only when it is the sole record of something that went **wrong**:

- `exit 0` → removed, however soon it came. You asked it to quit.
- non-zero **and** young → kept, marked `✗` with the code. A spawn that fails and vanishes shows
  you nothing.
- non-zero and old → removed. SIGINT on an hour-old claude exits 130 and you meant it.

Both halves are load-bearing: age alone keeps a clean quit, status alone keeps every claude you
ever interrupted. The decision is now `keeps_its_row(is_shell, dead_status, age)`, a pure function
with no tmux in it, so the table above is a test rather than a paragraph.

**The bug the probe found while proving the fix — worth more than the fix.** The integration run
still logged `died before it started` with the corrected binary in place. The **daemon does not
pick up a new build**: `ensure_running` returns early whenever a daemon is alive, and that daemon
runs whatever code it exec'd from, until every project closes ("no sessions left, exiting") or it
is killed. Measured directly — daemon started 15:54:04, binary rebuilt 15:58:53, still serving the
old policy. `Tmux::source_conf` exists precisely because the tmux *server* has this shape; the
daemon has it too and nothing addresses it. Until it does, "I fixed it and it still does the old
thing" is a false report waiting to happen, and it already produced one here.

**A consequence to know before pressing Ctrl-W:** removing the row of a project's *last* instance
kills its last window, and tmux destroys a session that has no windows (measured, on a throwaway
session). So closing your last claude closes the project — the tab goes and the client lands on
another project, or detaches if there is none. The desktop keeps a zero-session project alive with
a "press ⌘T" empty state; tmux cannot, since a session with no windows does not exist.

### `mpx` offers a project instead of opening one (2026-09-06)

*"can we make `mpx` not auto open the current project, exactly like the IDE?"*

It used to create a project for whatever directory the shell happened to be in. The odds are good
and not good enough, and being wrong costs a project you now have to notice and close. With
nothing running, `mpx` now shows the picker with **that directory at the top, marked `· here`**, and
opens nothing until Enter. Attaching is untouched: with a project already running, `mpx` still
rejoins it instantly — that is the ssh-reconnect case and the reason any of this exists. Only the
auto-*create* went away.

**The picker needed a second mode to be the front door.** In a popup there is a live client and
choosing means `switch-client`; standalone there is no client and choosing means *becoming* one.
Read off `$TMUX`, because it is a property of where the process is rather than of who called it.

That split forced a structural change worth noting: `attach` **`exec`s**, and a replaced process
runs no destructor — so the `RawMode` guard would never restore the terminal, handing the next
program a tty in raw mode with a hidden cursor. The loop is therefore a function that *returns* a
choice, with the attach performed by the caller after the guard has dropped.

**A bug found by looking at the real screen rather than the test.** The `· here` label was appended
after the path and the whole line then cut to width — so on a deep directory the one word
explaining why that row is first was the first thing to vanish. Driven from a scratchpad path, the
row read `…-mulpex/d7918b…` and nothing else. The path is now truncated to make room for the label:
the path can afford to lose characters, the label cannot.

**And a bad assertion, caught by its own failure.** The width check counted SGR escapes as
characters. A frame is not its own width; the test now strips CSI sequences before measuring.

Driven with **no tmux server at all**, which was previously unreachable: `mpx` in an unopened
directory offered rather than opened, Esc left with nothing created, Enter opened the project and
made that process its client, and a second `mpx` rejoined without showing the picker at all.

### Two questions, two orders (2026-09-06)

*"the auto complete should sort the opposite way, the short path at start"* — typing `~/docu`
listed `~/Documents/Code/dreamvps/cloud` above `~/Documents`, which makes ⇥ useless for descending:
you cannot complete *towards* a directory by jumping past it into something nested under it.

The fix is not a sort tweak, it is noticing the list answers two different questions:

- **A word is a name you are recalling.** The answer is grouped by what the rows *are* — what is
  running, then what you have opened before, with the directory you are standing in pinned on top.
  That grouping is what makes the picker a switcher and it stays exactly as it was.
- **A path is a place you are navigating.** The list should read like a directory listing:
  **shortest first**, whatever source a row came from — an open project included, and `here` no
  longer pinned, because mid-path it is just another candidate.

Length rather than component count: it is what "the short path" means, and a deep-but-terse path is
genuinely quicker to reach. The full path breaks ties, so the order never depends on which source a
row happened to come from.

`is_path()` is now one function used by **both** the browse listing and the ordering. They have to
agree — a list sorted for navigating but containing nothing to navigate to would be the same defect
from the other side.

Driven on a real screen, both shapes:

```
> ~/docu                          > clo
   Documents   ~/Documents        ▸ ○ cloud  ~/Documents/Code/dreamvps/cloud
 ○ test        ~/Documents/Code/test
 ○ test2       ~/Documents/Code/test2
 ○ cloud       ~/Documents/Code/dreamvps/cloud
```

### The sidebar's two lags, and closing a project's last tab (2026-09-06)

*"the sidebar have weird lags. when we open new claude he turn black for a moment. when we close
claude he keep showing in for a moment"* and *"when we close the last tab of a project, mulpex close
immediately instead of focus on the other project"*.

**Measured first, both of them:** a new claude's sidebar sat blank for **679 ms** and a closed row
lingered **1.32 s**. 679 is not a coincidence — it is `IDLE_MS = 700`. A window is created
**detached** and only then switched to, so a brand-new sidebar's first look finds itself hidden,
goes to sleep, and is still asleep when you arrive in it.

**The obvious fix was measured and rejected.** Polling faster is not free: a
`tmux display-message` costs 10 ms wall and ~3.4 ms of CPU, and *every window runs a sidebar*, so
seven hidden ones checking at 150 ms is about a sixth of a core, spent forever discovering that
nothing changed.

So the daemon publishes the answer instead. It already asks tmux this, once per tick, for every
project — `core::publish_focus` writes `<active window> <active pane>` into the project's state dir
and the sidebars read it. N polls become one, and each sidebar's check becomes a file read, which
it can afford to do every 80 ms. A sidebar that finds no file falls back to asking tmux at the old
700 ms, so a daemon that is down costs latency and nothing else. The first frame is now drawn
whether or not anyone is looking yet, which is what removes the blank entirely.

Result: **679 ms → 12 ms** blank, **1.32 s → 0.65 s** for a closed row.

**A bug I introduced, and only measurement caught it.** Dropping the `\x1b[2J` from each frame — so
the pane is overwritten rather than wiped — left the *blank filler lines* as bare `\r\n`, which
move the cursor without erasing. Every visible line is padded to the full width and covers what was
under it; those write nothing. A closed claude therefore stayed on the pane **for good** — ten
seconds, twenty — on a sidebar that was repainting perfectly the whole time. The filler now carries
`\x1b[K`, and only the filler: after a line that exactly fills the width the cursor has already
wrapped, so an erase there would clear the line *below*.

Worth separating what was measured from what was not: the 679 ms was measured and fully explains
"turns black". That the `\x1b[2J` also caused a flash is a **hypothesis** — the no-clear repaint is
kept because it is strictly less work, not because the flicker was demonstrated.

**The last tab was one line.** A project *is* a tmux session, so closing its last instance kills its
last window and tmux destroys the session — and tmux's default `detach-on-destroy on` then detaches
the client. Closing one project dropped you out of Mulpex with the others still running, which
reads as a crash rather than a close. `off` switches to another session instead. Driven: two
projects open, closing tabB's only instance left the client attached on tabA and `mpx` still
running; closing the last project then does end the client, which is correct — a tmux session with
no windows cannot exist, so there is no zero-project state to sit in.

## Session restore: `mpx down` is stop, not forget (2026-09-07)

tmux already carried a project across a detach, an ssh drop and a closed laptop, so what was
missing was narrower than "restore" usually means: the cases where **the tmux server itself goes
away** — a reboot, `mpx down`, `kill-server`. The `--resume` uuid lived only on the window that had
just been destroyed, so every conversation went with it.

Four decisions, taken by the user before any code: `mpx down` keeps the conversations (the desktop
app's rule — closing a project keeps its store); closing the last instance means the same thing,
since it lands in the same place; the **open set** is remembered too, so `mpx` after a reboot puts
every project back; and terminals are not restored, because a shell has no conversation to resume.

One thing is settled by tmux rather than by preference: a session with no windows does not exist,
so a project with nothing saved must still open one claude. The app's "zero instances, press ⌘T"
state has no equivalent here.

### A collision that had never fired

`persist::SessionStore::new` resolves its home through `mulpex_core::mulpex_home()`, which reads
`MULPEX_HOME` — and **`mpx` never sets it**. The first call to the store from the CLI would
therefore have written into the desktop app's `~/.mulpex/sessions/`: exactly the silent
conversation corruption `statedir.rs` exists to prevent, in the one file whose whole job is
preventing it. It had gone unnoticed because nothing in `mpx` had ever called the store.

`SessionStore::in_home(home, project_dir)` now takes the home explicitly and `new` delegates to it,
so the CLI cannot reach the app's store by omission. Guarded by
`restore::tests::the_store_lives_under_the_clis_own_home`, which asserts the two paths differ.

### What the store contains, and the exception that makes it work

Only a claude that has **actually worked** — `state_dir/<id>`, the file its hook writes on the first
turn — is worth saving; an instance opened and never spoken to has no conversation. Driven and
confirmed: alpha had `claude#1` (given a codeword) and `claude#2` (never used), and only #1 was in
the store and only #1 came back.

The exception is load-bearing rather than tidy. A restored claude has a **fresh state dir**, so it
looks unworked, and the first tick after a restore would rewrite the store empty — deleting the
conversations it had just brought back. The desktop app solves this with an in-memory `sticky` list;
here `@mpx_restored` sits on the tmux window, so it survives the daemon dying too. Checked
explicitly: the restored row is still in the store a tick later.

### The bug this feature was built to prevent, caused by its own first version

The daemon's first cut removed a project from `open.txt` when its session **disappeared** — reasoning
that closing the last instance destroys the session, which is true. Then the probe killed the tmux
server, and every project vanished at once: the daemon read that as "the user closed them all" and
emptied the set. `mpx` reopened nothing. **The reopen set was wiped by exactly the event it exists
for**, and the daemon log said so in plain words three times over.

The premise was wrong. "The session is gone" and "the user closed it" are the same observation a
tick later, so the fix is not to observe better — it is to key on the act instead. Closing the last
row is something the daemon **does**: `mpx close`, `Ctrl-W`, or reaping a claude that exited on its
own. A server dying is something it never sees, and now touches nothing.

Another instance of a shape already in `CLAUDE.md` — *a default that reads as an assertion*. Absence
was being reported in the same word as intent.

### Measured

Real `mpx`, real tmux, real `claude` children, under a scratch `MULPEX_HOME`
(`scratchpad/ui2a/restore.py`). The decisive check is not that the process relaunched but that the
**conversation is still there**: a codeword given before `mpx down`, asked for again after `mpx up`.

- a new project opens with one claude; its uuid is on the window
- the store is under the CLI's home, and the desktop app's was not written
- only the claude that spoke is saved; the never-used one does not come back
- mute survives, and so does the name the claude gave itself via `hub_set_name`
- `mpx down` leaves the reopen set and keeps the store; `mpx up` restores `claude#1` as
  `claude#1`, resuming the same uuid, marked restored
- **the restored claude still knows the codeword**
- killing the tmux server changes the reopen set not at all, and plain `mpx` reopens both projects
- closing a project's last instance takes it out of the set on its own, keeping its store

### Two probe bugs worth recording, because both are traps

**A pane is not a transcript.** The first check looked for the codeword *on screen* after the
restore, and it failed against working code: `--resume` repaints the conversation, then the hub's
own task-notification scrolls it away, so the text is nowhere on the pane — not even in
`capture-pane -S -`. The evidence was in `~/.claude/projects/…/<uuid>.jsonl` all along. The check
now **asks** the restored claude what the codeword was, which is the only version of the question a
user would recognise.

**`.strip()` ate a field.** `tmux()` stripped the whole output of `list-panes -F`, and a
tab-separated format whose last field is empty ends its line with a tab. Only the *last* line loses
it, so that one row came back one field short and was silently dropped — and the row in question was
the claude pane, whose `@mpx_restored` is empty precisely when it is a fresh spawn. It read exactly
like "the project opened with no claude". `rstrip("\n")`, never `.strip()`.

## `hub_close` (2026-09-08)

### Driven

- **The tool is exposed.** `tools/list` from the real `mulpex-helper mcp` binary lists `hub_close`
  between `hub_spawn` and `hub_remote_open`, with the schema as written. Not inferred from the
  source — the built helper was run and its stdout read.
- **The caller-side refusals refuse, and queue nothing.** Real helper process, `MULPEX_INSTANCE_ID=3`:
  `to: "claude#3"`, `to: "all"` and `to: "monorepo#7"` each come back `isError: true` with their
  own reason, and `termreq/` is empty afterwards. That last part is the assertion that matters — a
  request written on the way to a refusal would be applied later by a poll loop that never sees the
  reasoning.
- **The wire format between the two processes.** `to: ["7", "claude#7", 8]` queues exactly
  `{"force":false,"from":3,"ids":[7,8],"op":"close_instance"}` — deduped, and the same shape
  `state.rs::apply_terminal_request` parses in the passing app test. With nothing listening the
  call ends in `Mulpex did not respond`, not a hang.
- **The app half, against real sessions** (`close_instance_closes_idle_claudes_and_refuses_the_rest`):
  two spawned claudes and one terminal; the idle claude closes, the busy one is refused **and is
  still alive afterwards**, the terminal is refused with `hub_terminal_close` named in the reason,
  an unknown id is refused, `force: true` then closes the busy one, and both rows disappear on the
  next reap.

### NOT driven

- **The `mpx` (tmux) side.** `mulpex-cli/src/terminals.rs` mirrors the desktop arm and shares
  `close_busy_reason`, and it compiles, but `mulpex-cli` has no tmux fixture and no live daemon was
  driven. Unverified.
- **Live in the shipped app.** Everything above ran against the debug build; the running Mulpex is
  an older release and was deliberately not restarted.

### A probe bug worth recording

**The helper drops a tool-call reply if stdin closes immediately after the call.** `mcp::run`
handles each `tools/call` on its own thread and returns as soon as `stdin.lock().lines()` ends,
without joining `workers` — so a piped probe (`printf … | mulpex-helper mcp`) gets the `initialize`
reply and *nothing else*, for every tool, including ones that were working perfectly. It reads
exactly like "the new tool is not wired up". Keep stdin open (`{ printf …; sleep 8; }`) and the
replies arrive. In production `claude` holds stdin open for the life of the instance, so this only
bites at shutdown — where the side effect still happens and only the reply is lost. Left as-is;
recorded so the next probe does not re-diagnose it.

## Bidi: the sentence, not the letters (2026-09-09)

Reported second-hand by a claude in another project: a Claude Code `AskUserQuestion` picker whose
question is Hebrew with English identifiers in it renders unreadably; Hebrew alone is fine. The
guess in the report was "xterm has no bidi". It does not, but bidi was not what was missing.

### Driven

- **The base direction was the whole bug.** Nothing in the app sets `direction` or `unicode-bidi`
  on the rows, so every row is an **LTR paragraph**. Real xterm 5.5.0 in headless Chrome, each
  character's x measured with `Range.getBoundingClientRect` (never read off a screenshot — see the
  RTL note in [rendering.md](rendering.md)), Hebrew letters mapped to order-preserving Latin so the
  report itself cannot be re-bidi'd. For the reported sentence the visual order is
  `[הכרטיס…ל][-VM][עדיין מסומן][PAUSED][מ-7 בספטמבר][?]` **left to right** — every Hebrew run
  internally correct, the runs in the wrong half of the line. Read from the right edge that is
  exactly the scramble reported, `?` first.
- **On the real thing, not a mock.** A live `claude` was driven on a PTY at 120x32 until it rendered
  an actual `AskUserQuestion` picker with that question (`scratchpad/bidi-repro/`, modelled on the
  geometry harness), and those 13382 bytes were replayed into this build. With
  `unicode-bidi: plaintext` per row: the question row reads right-to-left correctly end to end,
  `ל-VM` and `ה-VM` weld to the right word, `'waiting for claude'` sits at the end of its
  description instead of the start. The `────` separators, `4. Type something.` and the
  `Enter to select · …` hint are **byte-identical** in both modes.
- **`plaintext` is a no-op unless a row starts with a strong RTL character.** Measured:
  `╭── Still paused ──╮` and `Ticket עדיין is PAUSED` (Latin first) render identically with and
  without it. A pure box-drawing rule has no strong character at all and stays LTR.
- **`direction: rtl` also reads correctly** and stays rejected: it flips the entire grid.
- **The cost, measured and accepted:** a Hebrew-first row becomes a real RTL paragraph, so its
  leading `❯ 1.`, `│` and indentation move to the right edge. In a list Claude Code renders half in
  Hebrew and half in English the markers therefore land on both sides. Gidi chose this over a
  toggle.

### Bidi isolates are a trap in a terminal

`U+2068 FSI` / `U+2069 PDI` **do** fix the ordering from the content side — wrapping the Latin runs
made the whole line lay out as one correct RTL run even under an LTR paragraph. They are still the
wrong answer: measured on `@xterm/headless` 5.5.0, each isolate lands in **its own cell with
`getWidth() === 1`**. It draws nothing (the browser renders it zero-width), so it does not show up
as a visible box — it silently eats a column, and both xterm's wrapping and Ink's own width math on
the emitting side count it. `U+200E/U+200F` (LRM/RLM) and `U+200B` are free by comparison (merged
into the previous cell at width 0) but cannot help: paragraph direction comes from CSS, not from
content, so no character the emitter inserts can set it. Measured — RLM after each Latin run left
the line messier than before.

### A harness note

The obvious `unicode-bidi: plaintext` on `.xterm-rows` **does nothing**, which is what the earlier
RTL work recorded as "changes nothing (already the default behavior)". The bidi paragraphs are the
**row divs**, not their container, so the property has to land on `.xterm-rows > div`. The first
measurement of this bug repeated the same mistake and produced a clean "no effect" result.

### Not verified

- **Live in the shipped `.app`.** `npm run build` only; Mulpex was not restarted (the user is
  working inside it).
- **Arabic**, and any row mixing RTL text with the *cursor* — the caret is still column-based, the
  residual limit already recorded in [rendering.md](rendering.md).

## 2026-09-16 — the sidebar dots: red narrowed, and why yellow never went green

Driven against a real `claude` and the real `mulpex-helper` binary, not reasoned about. The probe
(`scratchpad/monprobe`) runs `claude -p` with a throwaway `--settings` whose `PostToolUse` and `Stop`
hooks are a bare `cat >> …jsonl`, in an environment with `MULPEX_*` and `CLAUDE_CODE_CHILD_SESSION`
scrubbed.

### Measured: Claude Code removed persistent Monitors

The payload `note_persistent_monitor` was built on no longer exists. `persistent` **moved from
`tool_input` to `tool_response`, and is now permanently `false`** — the Monitor tool has no such
input parameter any more (its schema is `additionalProperties: false`, so passing one is rejected),
and every monitor expires (30 min cap):

```json
"tool_input":  {"description":"Mulpex hub inbox","timeout_ms":600000,"command":"INBOX=…"}
"tool_response":{"taskId":"bp1jr29w8","timeoutMs":600000,"persistent":false}
```

So the `PostToolUse` recorder returned early on every call and `monitors/` was never written.
Confirmed on the **user's live scratch dir** (`mulpex-1149/3`): `monitors/` empty, `armed/1` and
`bg/1` both present, status `working`. That is the whole "yellow never turns green" report.

### Measured: the listener's command reaches `Stop` verbatim

The identifying fact the fix now rests on. With the real `HUB_RULES` command armed, the `Stop`
payload carries it in full — not summarised, not truncated:

```json
{"id":"bnxvw92ez","type":"shell","status":"running","description":"Mulpex hub inbox",
 "command":"INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; ARMED=…; while true; do … done"}
```

This also corrects an earlier note in [sessions.md](sessions.md): the previously recorded
`"command":"while true; do sleep 1; done"` was that probe's own short command, **not** evidence of
truncation.

### Driven through the real helper

- **Red narrowed.** Stop → `waiting`; idle_prompt → `waiting`; permission_prompt → `waiting`;
  `PreToolUse[AskUserQuestion]` → `needs`, and its own permission_prompt 6 s later → still `needs`;
  PostToolUse (the user answered) → `working`; `PreToolUse[ExitPlanMode]` → `needs`, surviving both
  its permission_prompt and a later idle_prompt; Stop with an agent running → `working`, and the
  idle_prompt after it → still `working`.
- **Yellow fixed, on the verbatim captured payload.** Stop with only the listener → `waiting`, `bg`
  flag clear; idle_prompt after → `waiting`; Stop with the listener *plus* a real background shell →
  `working`, flag set; idle_prompt after → still `working`; Stop with nothing → `waiting`.
  `monitors/` is no longer created at all.
- **The heartbeat.** The `HUB_RULES` command run as literal shell ticks `armed/<id>`'s mtime every
  second (1789538468 → 1789538470 across two seconds). Through the helper's `userpromptsubmit`: a
  fresh heartbeat emits **no** arm nudge, a 120-s-old one **does**, a missing flag **does**.

### Not verified

- **Live in the shipped `.app`.** Tests + the real helper binary only; Mulpex was not restarted (the
  user is working inside it), so the running app still carries the old helper.
- **That an instance actually re-arms on being told its monitor expired.** `HUB_RULES` now asks for
  it, and the heartbeat is the backstop that does not depend on it, but the model's own response to
  an expiry notice was not driven.
- **Whether anything other than the plan dialog still fires `permission_prompt`** under
  `--dangerously-skip-permissions`. Nothing in the fix depends on the answer.

## 2026-09-16 — the listener: a transcribed command, a nudge storm, and six orphans

Started from a screenshot of `warweb#65` showing three `Monitor` tasks at once and a duplicate wake
per hub message.

### Measured: the listener command is retyped from context, and drifts

Read off the instances' own transcript `.jsonl` files (`~/.claude/projects/<slug>/*.jsonl`), counting
every `Monitor` tool call whose input contains `new hub message` and checking whether the in-loop
`touch` (added that morning by `003ad33`) was present:

| instance | arms | carrying the current command |
| --- | --- | --- |
| `warweb#65` (`c30f48b2`) | 72 | only #72, at 10:49 — **immediately after a `/compact`** |
| `warweb#74` (`9154c9be`) | 34 | none |
| `cloudraw#3` (`0a33866e`) | 4 | both of today's (it had compacted earlier) |

The instance's `--append-system-prompt` argv was read straight off `ps -ww` and **did** contain both
`touch`es, so the rules were correct and the transcription was not. Compaction is what accidentally
healed it: dropping the old `Monitor` call from context left the system prompt as the nearest copy.

Consequence, confirmed on disk: `armed/65` frozen at 10:23:57 while its loop was demonstrably alive
(CPU accumulating, a live `sleep 1` child), so `listener_armed` was false on every turn, so the arm
nudge fired on every turn, so Monitors stacked.

### Measured: `background_tasks` is a `Stop`-only field

Real interactive `claude` v2.1.273 on a PTY (Haiku 4.5), scratch dir, hooks dumping raw stdin:

| hook | `background_tasks` |
| --- | --- |
| `userpromptsubmit` | **absent** — including a prompt taken 24 s after a Monitor was armed |
| `posttooluse` | absent |
| `stop` | present, with `id`, `status` and the full `command` |

This killed the first plan (teach `listener_armed` to read the task list) and produced the
`Stop` → `relisten/<id>` → nudge handshake instead.

### Measured: a listener escapes every kill path

`ps -o pid,ppid,pgid,sess,stat` on live listeners: `PGID == pid`, `SESS 0`, state `Ss` (no `+`) —
own process group, no controlling terminal, so neither `killpg(child_pid)` nor `kill_tty_session`
can reach one. Six were alive with `ppid 1` at the time of writing, started 09:25, 09:27, 09:39,
09:40 and 10:41 the *previous* day plus one at 09:59, all belonging to Mulpex processes that had
exited; their scratch roots were long gone and they were still forking a `sleep` once a second.

Demonstrated live rather than inferred: closing the probe terminal killed its `claude`, and its
Monitor shell was immediately `ppid 1` and still running.

### A probe bug worth recording

`KERN_PROCARGS2` asked for its size with a null `oldp` answers **32 bytes** — enough for `sleep 60`
and nothing else. A buffer sized from that silently returns a *truncated* command line, and the
first version of the orphan sweep matched nothing while looking like it worked. Use `KERN_ARGMAX`.

Also: it returns **argv only** for our own children — a real orphan's blob came back 756 bytes with
no `MULPEX_*` anywhere. So "which state dir does this listener serve?" is unanswerable from outside
the process (argv spells it `$MULPEX_STATE_DIR`, unexpanded), and the reaper keys on `ppid == 1`
instead.

### Driven through the real binaries

- **`mulpex-helper listen`:** heartbeat ticks `armed/<id>` once a second; two messages then one more
  produce exactly `mulpex: 2 new hub message(s)` then `mulpex: 1 new hub message(s)`; a second
  listener for the same instance prints `standing down` and exits 0; removing the state dir makes it
  exit on its own; killing the pid in `pids/<id>` makes it exit on its own and clear its lock.
- **The `Stop` → nudge handshake, through `mulpex-helper hook`:** a stale `armed/<id>` plus one
  superseded-format listener writes `relisten/7 = bstale1`; the next `userpromptsubmit` emits the
  repair nudge naming `bstale1` and the command to arm instead, and consumes the note. A healthy
  single listener writes no note and produces no nudge. Two listeners write `b1 b2`.
- **The rendered rules and nudge** were read as an instance sees them, not just asserted on: the
  command sits on its own line in both.
- 110 `mulpex-core` tests, 86 `src-tauri` tests.

### Not verified

- **Live in the shipped `.app`.** Tests and the real helper binary only; Mulpex was not restarted
  (the user is working inside it), so the running app still carries the 0.18.2 helper.
- **That a real instance obeys the repair nudge** — stops the named tasks and arms the new command.
  The handshake that produces the nudge is driven end to end; the model's response to it is not.
- **The six live orphans were left alone**, by the user's decision. The reaper's *selection* is
  tested (`only_a_parentless_listener_is_reaped`) against purpose-built processes; it has not been
  run against them.

## 2026-09-16 (evening) — the Explainer back to an automatic feed

The morning's on-demand Explainer (⌘⇧E only, one cached entry, three auto-clears) was reverted the
same day at the user's request; the three-part formatting, the question/plan handling and the
failure/retry path from that version were kept. `docs/explainer.md` has the resulting design.

### Measured: `transcript_path` is on the `PreToolUse` payload

The trigger's one unmeasured assumption. The original feed's `askq`/`plan` hooks forwarded
`tool_input`, so no captured `PreToolUse` payload on this machine carried the path, and the
documented hook contract ("common fields on every event") had never been checked here. Driven with
a headless `claude -p --settings <capture hook>` running one `Bash` call (`scratchpad/pretool`):
the `PreToolUse` payload carries `session_id`, `transcript_path`, `cwd`, `prompt_id`,
`permission_mode`, `effort`, `hook_event_name`, `tool_name`, `tool_input`, `tool_use_id`. The
`Stop` payload from the same run carries it too. So the hooks write the path for all three events.

### Driven through the real helper

`mulpex-helper hook askq` / `hook plan` / `hook stop` with synthetic payloads on stdin, against a
scratch `MULPEX_STATE_DIR`: `explainreq/4` = `/tmp/x.jsonl\ndialog`, `explainreq/5` =
`/tmp/y.jsonl\ndialog`, `explainreq/6` = `/tmp/z.jsonl` (no marker), and the status words `needs`,
`needs`, `waiting` alongside — the two jobs of each hook, both landing.

### Driven in `npm run tauri dev` (by the user)

- A `say ok` turn produced a three-part entry by itself; the dev log showed
  `[explainer] project 1 claude#3: WORK: … DID: … NEED: …`. ⌘⇧E hid and showed the column without
  losing it.
- The 10-entry feed with timestamps, dimmed history and sticky scroll was accepted after the same
  QA pass.

### Tests

- `a_finished_turn_hands_its_transcript_to_the_explainer`, `a_pending_question_reaches_the_explainer`,
  `a_pending_plan_reaches_the_explainer` (hook.rs, restored and adapted: path + `dialog` line);
  `explain_requests_are_consumed_and_only_live_claudes_count` (state.rs, restored);
  `a_dialog_request_waits_for_the_dialog_entry` (explainer.rs, new — the question entry is appended
  from another thread 400 ms in and the reader waits for it; a dialog that never comes falls back
  to the prose); `the_feed_is_newest_first_capped_and_forgettable` (cap of 10, oldest gone
  completely); `queued_requests_coalesce_per_instance` (request-body parsing + latest-wins).
- 111 `mulpex-core` tests, 92 `src-tauri` tests, `svelte-check` clean.

### Not verified

- **The `dialog` race live.** Whether a real `askq` hook can be drained before the `AskUserQuestion`
  entry is in the transcript was not measured; the guard is there so it does not matter which way
  it goes, and the unit test drives the guard, not Claude Code's write order.
- **The frontend cap on a real 11-turn run.** `applyExplainFor`'s slice is exercised by reading, not
  by a driven session; the backend cap is unit-tested.
- **Live in the shipped `.app`.** Dev build only; the running Mulpex was not restarted.

## 2026-09-17 — the Explainer explained a turn's first line as the whole turn

Reported from a screenshot: a warweb turn that ended with two decisions for the user got the entry
"בודק עכשיו את מצב ה-git… / כלום, אפשר להמשיך" — a summary of its *opening* line.

### Measured: the turn was one turn, and the reader took its first line

- The real transcript (`-Users-gididaf-Documents-Code-games-warweb/c30f48b2….jsonl`, 45 MB): prompt
  at 05:38:20Z, `text` "Two decisions are open. Let me check the current state of the tree first…"
  at 05:38:33, two `Bash` `tool_use`/`tool_result` pairs, final `text` at 05:38:58. No user entry,
  no `<task-notification>`, no dialog in between. The panel's entry, timestamped 08:39 local,
  matches the first `text` and nothing after it.
- Cause by inspection, then reproduced: the evening-of-09-16 `read_turn_settled` retried only on an
  *empty* turn; a turn that spoke mid-way is non-empty the instant `Stop` fires, and the docs even
  recorded the partial read as "accepted". The 2026-08-30 feed slept 400 ms before its first read,
  which is why it never showed this.

### Measured: `Stop` carries `last_assistant_message`, and only the last message

- `claude` 2.1.274 headless, `--settings` with a `Stop` hook that dumps its stdin, a prompt that
  makes the model say one line, run `echo`, then say another: the payload is
  `{session_id, transcript_path, cwd, prompt_id, permission_mode, hook_event_name, stop_hook_active,
  last_assistant_message: "final answer", background_tasks, session_crons}`. The field holds the
  final message alone ("final answer"), not "first line here" — exactly the text the reader must
  wait for.
- The same hook copying the transcript at `Stop` time: in `-p` the final `text` was **already in
  the file** (30 lines at Stop, 32 after). So headless does not reproduce the race; the 08:38 turn
  and the 2026-08-30 measurement are both interactive sessions.

### Replayed on the real transcript

- The warweb transcript cut just before its final `text` entry, that entry appended from another
  thread 900 ms after `read_turn_settled` started. Old input (no final text): settled at once on
  "…so the second one is accurate." — the bug. New input (the entry's text as `final_text`): the
  turn ends with "…then one full commit." and holds the mid-turn line once, read from the file, not
  appended. Settled ~1.1 s after the append in `--release` (one read of the 45 MB file ≈ 105 ms;
  ≈ 770 ms in a debug build, which is why the wait budget is attempts, not wall-clock).

### Tests

- `a_finished_turn_forwards_the_message_it_ended_on` (hook.rs: `path\nfinal\n<text>`, blank text →
  path alone, a dialog request never carries it); `a_turn_that_spoke_midway_waits_for_its_final_message`
  and `a_final_message_that_never_lands_is_appended` (explainer.rs); `queued_requests_coalesce_per_instance`
  extended for the `final` body. 112 `mulpex-core` tests, 94 `src-tauri` tests.

### Not verified

- **Live in the shipped `.app`.** The running Mulpex was not restarted; the fix is verified on the
  real transcript and the real `claude`'s payload, not on a driven interactive session.

## 2026-09-17 — agentalk: the pane that was `working` forever

### Measured: what an agentalk-paired instance actually holds

Reported by the paired instance itself (`cloudraw#3`), not read off a screen — a screenshot of a
Hebrew/mixed pane re-applies BiDi, and `background_tasks` never appears in a pane at all. Three
long-running background things, verbatim:

```
1) Bash run_in_background  [bash id bi5bpldo7]   description: "Arm agentalk poll loop"
   . '/tmp/agentalk-session-f31d5bca82b7068a-cloudraw_.env' && curl -fsS
     'https://agentalk.dev/loop.sh' -o /tmp/agentalk-loop.sh && . /tmp/agentalk-loop.sh
2) Monitor                 [task id br3blyunn]   description: "agentalk channel events"
   tail -f -n +1 '/tmp/agentalk-events-f31d5bca82b7068a-cloudraw_.log'
3) Monitor                 [task id bc2bdtzqi]   description: "Mulpex hub inbox"
   "/Applications/Mulpex.app/Contents/MacOS/mulpex-helper" listen
```

- (1) is an infinite `curl` poll; (2) is a `tail -f`. **Neither ever exits**, so `Stop` saw work in
  flight at every turn end and the row was yellow for as long as the pairing was up.
- **The channel id and the participant name change on every re-pair** — that instance had replaced
  channel `f7517f834a94332d` ~30 min earlier. Only the fixed path prefixes are stable.
- **`description` is free text the model writes.** (1)'s was that instance's own invention. (2)'s
  happens to be dictated by agentalk's bootstrap output, but the rule is still: match the command.
- (1) and (2) are *different kinds* with *different command shapes* for one feature, so exempting
  either alone leaves the pane busy.

### Driven through the real `mulpex-helper hook stop`

Debug helper, temp `MULPEX_STATE_DIR`, the verbatim commands above piped in as `background_tasks`
(`scratchpad/drive-helper.sh`):

| payload | status | `bg/3` | `watching/3` |
| --- | --- | --- | --- |
| agentalk loop + events tail | `waiting` | absent | present |
| + the hub listener | `waiting` | absent | present |
| + `npm test -- --run` | `working` | present | present |
| `npm test -- --run` alone | `working` | present | absent |
| nothing running | `waiting` | absent | absent |

Then the user list end to end: `MULPEX_HOME` pointed at a scratch home whose `watchers.txt` held
`kafka-console-consumer`, with a matching command as the only task — `working` without the file,
`waiting` with it. Proves `mulpex_home()` resolution, the parse (comments + blanks skipped) and the
substring match in one pass, without writing into `~/.mulpex`.

### Tests

`agentalks_poll_loop_and_events_tail_are_watchers` (the two commands verbatim; the same pair with a
different channel id; plural payloads with the listener and with real work; and the negative case —
`cd …/utilities/agentalk && npm run build` must stay `working`),
`a_watcher_is_idle_to_the_sidebar_and_busy_to_the_updater` (both flags, and a purged `watching/`
subdir being rebuilt), `the_user_can_add_watcher_patterns_of_their_own`,
`the_seeded_template_is_inert_and_never_overwrites`. 116 `mulpex-core` tests;
`cargo check --workspace` and `npm run check` (122 files, 0 errors) clean.

### Not verified

- **Live in the shipped `.app`.** The running Mulpex was not restarted — the user was working
  inside it. `cloudraw#3`'s row going green is expected at its next turn end after a relaunch (the
  match is retroactive: no re-pair, no re-arm), and has not been watched happen.
- **`busySessionCount` with an update actually pending.** The `watching` branch is covered by the
  Rust-side flag test and by inspection only; nothing exercised the real update banner.
- **The seeded `watchers.txt` appearing on launch.** `seed_watchers_template_at` is tested directly;
  the `setup()` call site was not run.
