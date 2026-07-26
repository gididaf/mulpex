# Mulpex (desktop app) — design notes

Native macOS Tauri app hosting multiple coordinated Claude Code sessions. Successor to the
terminal-UI mulpex (`../mulpex-deprecated`). This doc covers what's specific to the desktop
rewrite; the coordination-hub semantics are unchanged from the deprecated project's `CLAUDE.md`
(the hook/mcp/persist modules were copied verbatim).

## The key idea

**xterm.js is the terminal emulator.** The old TUI needed `vt100` + `tui-term` only to composite
Claude's ANSI output into a ratatui sub-rect; in the browser xterm.js does that. So the backend
is a **raw byte pipe** (PTY bytes → `term.write`; xterm `onData` → PTY writer). That deleted
`ratatui`/`crossterm`/`tui-term`/`vt100`, all key-encoding (`keymap.rs`), mouse-selection,
manual scrollback, and the Kitty/Ctrl keybinding workarounds. Everything that makes mulpex
*mulpex* — the hook coordinator, MCP hub, persistence, file IPC — is UI-independent and was
reused unchanged.

## Layout (Cargo workspace + Svelte frontend)

```
crates/mulpex-core/   headless lib: hook, mcp, persist, config  (copied verbatim from the TUI)
crates/mulpex-helper/ bin: `hook <event>` / `mcp` dispatch → mulpex-core
src-tauri/            the Tauri app (Rust backend)
  src/pty.rs          Session = one claude on a PTY, streaming to a frontend Channel
  src/state.rs        Workspace = N open projects (Vec<Core> + active handle); each
                      Core = one project + sessions + its OWN scratch dir; reap/persist/hub-read
  src/commands.rs     #[tauri::command] surface (session cmds carry a projectHandle)
  src/hub.rs          200ms poll over ALL projects → emits handle-scoped hub-update /
                      session-exited / sessions-changed (+ projects-changed)
  src/menu.rs         native ⌘ menu; ids forwarded to the frontend as a `menu` event
  src/project.rs      recents + open-project set (~/.mulpex/recents.txt, open.txt)
  src/snapshot.rs     serde types shared w/ frontend (adds ProjectHandle, WorkspaceInfo)
src/                  Svelte/Vite frontend
  lib/terminals.ts    TerminalManager: one xterm per session keyed by (project,id),
                      alive-while-hidden across ALL projects, central resize
  lib/ipc.ts          typed command/event/channel wrappers
  lib/stores.ts       per-project state map + derived active-project projections (PTY bytes bypass)
  lib/components/*     ProjectTabBar, CommandPalette, TopBar, InstanceList, HubPanel,
                      TerminalPane/View, MessageReader, Rename…
```

## The helper (why it's a separate binary)

Child `claude` processes invoke `<helper> hook <event>` (from `settings.json`) and `<helper> mcp`
(from `mcp.json`) **by absolute path** — `claude`, not Tauri, spawns them. A `PreToolUse` hook
forks on every Read/Write/Edit/Bash and the MCP server is long-lived per instance, so the helper
must be tiny and fast to exec. It links only `mulpex-core` (~1.8 MB vs the ~29 MB app).

**Path resolution** (`lib.rs::resolve_helper_path`): `current_exe().parent().join("mulpex-helper")`
— works in `tauri dev` (`target/<profile>/`) and in the bundled `.app` (`Contents/MacOS/`). The
absolute path is substituted for `__MULPEX_BIN__` in the config templates when a project opens
(`state.rs::Core::open`).

### Bundling the helper (wired)

The dev flow needs nothing (helper sits beside the app in `target/`). For `tauri build`,
`mulpex-helper` ships as a **signed sidecar** so it lands in `Contents/MacOS/` *and is signed with
the bundle* — otherwise Gatekeeper SIGKILLs it and **every hook fails-open silently** (no
coordination, no error). This is **wired**: `bundle.externalBin` is `["binaries/mulpex-helper"]`
in `tauri.conf.json`, and `beforeBuildCommand` runs `scripts/bundle-helper.sh`, which builds the
helper in release and copies it to `src-tauri/binaries/mulpex-helper-<target-triple>` (the
suffix Tauri expects). Tauri strips the suffix, places it at `Contents/MacOS/mulpex-helper`, and
signs it with the app. Bundle `targets` are `["app", "dmg"]`, so `tauri build` produces both
`Mulpex.app` and `Mulpex_<version>_aarch64.dmg`. Verified: the built `.app` has the signed helper
beside the main binary.

## Data flow

- **PTY → frontend:** per-session `tauri::ipc::Channel<String>` carrying **base64** chunks
  (`pty.rs::OutputSink`, decoded in `terminals.ts`). Before the frontend attaches (restored
  sessions paint immediately on `--resume`), output is **buffered** and flushed on
  `attach_session` — race-free under the sink mutex.
- **frontend → PTY:** `send_bytes(id, data)` from xterm `onData` (UTF-8 encoded). The one manual
  key carve-out is **Shift+Enter / Option+Enter → `\x1b\r`** (`attachCustomKeyEventHandler`) —
  meta-Return, the sequence `/terminal-setup` installs for VS Code (`sendSequence "\x1b\r"`) and
  Terminal.app ("Use Option as Meta"). Claude accepts a bare `\n` too; ESC+CR is preferred only
  because it's the documented one. Both bytes must reach the PTY in **one write** (`send_bytes`
  → `write_all`): split across reads, Claude consumes the lone ESC and the CR then submits.
  `macOptionIsMeta` covers Option word-motions.

  **The handler must call `e.preventDefault()`** — this is the whole fix, not the byte choice.
  Returning `false` makes xterm bail out of `_keyDown` *before* setting `_keyDownHandled`, and it
  does not cancel the event on that path. The browser therefore still fires `keypress`, and
  xterm's `_keyPress` (which short-circuits only on `_keyDownHandled`) turns charCode 13 into a
  `\r`. So the PTY got our newline **and then a submit** — the newline was inserted and the
  message sent in the same keystroke, which is what "Shift+Enter sends the message" was. The
  `keypress` arm of the handler is a second line of defence. Option+Enter accidentally worked
  throughout, because `_keyPress` already ignores keys with `altKey` set.
- **hub state:** the 200 ms poll walks **every open project**, reads each one's scratch-dir files
  into a `HubSnapshot`, and emits a **handle-scoped** `hub-update {handle, snapshot}` on change;
  the frontend keys it into that project's slice, and the sidebar/hub panel are a reactive
  projection of the *active* project. Reaping (with `bounce_dead_inbox`, peer-list rewrite,
  persistence) is **single-sourced in the poll loop** — an explicit ⌘W close just `kill()`s and
  lets the next reap emit `session-exited {handle, id}`, identical to a self-exit.

## Multiple projects (Workspace)

Mulpex hosts **several projects at once in one window** and switches between them instantly. The
single `Core` is now one of N owned by a `Workspace` (`state.rs`: `Mutex<Workspace>` replacing
`Mutex<Option<Core>>`), each with a **monotonic `ProjectHandle`** (never reused after close, so a
stale reference resolves to a no-op) and its **own scratch dir** `temp/mulpex-<pid>/<handle>/`.

- **Hub isolation is free:** the hub is scoped entirely by `MULPEX_STATE_DIR`, passed per session
  in `pty.rs::Session::spawn`, so a per-project `state_dir` isolates each project's hub with no
  other change — instances only coordinate with peers in the *same* project. Instance ids stay
  per-project (each numbers 1,2,3…), so every session command carries a `projectHandle` and every
  hub event is handle-scoped; `(handle, id)` disambiguates.
- **Commands:** `bootstrap()` (replaces `current_project`) returns a `WorkspaceInfo { projects,
  active }`; `open_project` dedups canonically (re-activates if already open) and emits
  `projects-changed`; `close_project` tears the project down + re-picks the active neighbor;
  `switch_project` sets the active handle. Session cmds (`attach/create/close/rename/send_bytes/
  resize/focus_session`, `get_hub_snapshot`) all take `projectHandle`.
- **Frontend:** `stores.ts` holds a `Map<handle, ProjectState>` + `activeProjectHandle`; the
  classic `sessions/statuses/tasks/hub/activeId/project` stores are **derived read-only
  projections of the active project**, so the sidebar/hub components are unchanged. Navigation is a
  persistent **`ProjectTabBar`** (click / `+` open / ✕ close / unread badge) plus a **`⌘P`
  `CommandPalette`** fuzzy switcher (drag-and-drop no longer opens projects — see
  **Dropped paths** below). `TerminalManager` keys xterms by `(handle,id)` and keeps
  **every project's** terminals alive while hidden; exactly one is visible globally.
- **Persistence:** the open-project set is saved to `~/.mulpex/open.txt` (distinct from
  `recents.txt`) on open/close; on launch every project reopens (`--resume`).

## Keyboard

Native macOS menu accelerators (⌘T/⌘W/⌘R/⌘M/⌘1–9/⌘[ ⌘]/⌘O/⌘Q, plus **⌘⇧W** close project and
**⌘⇧] / ⌘⇧[** next/prev project) are intercepted by the menu before xterm; Claude never uses ⌘,
so there's zero collision. **⌘P** (the project quick-switcher) is *not* a menu accelerator — it's
handled in the webview (`svelte:window` keydown, `preventDefault` stops the print dialog).
Everything else (arrows, Ctrl+C, Esc, Shift+Enter) flows straight to the focused terminal.
Copy/paste/select-all are predefined Edit-menu items macOS routes to the focused xterm textarea.

## Dropped paths

Dropping a file or folder on the window puts its absolute path **at the focused session's prompt**
(`App.svelte::dropPaths`) — space-separated for a multi-drop, nothing submitted. It no longer opens
the drop as a project; that's ⌘O / the `+` tab / ⌘P. The one fallback is "nowhere to type": with no
active session (the picker, or a project at zero sessions) a drop still goes to
`openOrFocusProject`, and the backend rejects non-directories.

**The paths go over as one bracketed paste (`ESC[200~ … ESC[201~`), not as typed keystrokes** —
this is load-bearing, not incidental framing. Paste is the channel Claude Code inspects for
attachments: a **pasted** image path becomes an `[Image #N]` attachment the instance can actually
*see*, while the identical path *typed* stays inert text. That one byte-level difference is the
whole reason dropping a screenshot used to yield nothing but a string. One paste holding every
dropped path is the correct shape — Claude extracts each image and leaves non-images as text.

**Files and folders behave identically, deliberately** — this matches **Claude Code's own
drag-and-drop**, where dragging either into the terminal adds its path. The v0.4.0
drag-a-folder-to-open-a-project gesture was a Mulpex-only invention that shadowed the standard
behavior; a directory is a normal argument to hand Claude, and project-opening already has three
dedicated entry points. Don't "restore" the old gesture without asking — matching stock
Claude Code is the intent, not an oversight.

Two non-obvious constraints, both of which this had to be built around:

- **Tauri owns the drop.** `dragDropEnabled` defaults to **true**, so the webview converts the
  native OS drop into an `onDragDropEvent` and the DOM never fires `drop` — xterm cannot see it,
  so that handler is the only place a drop can be honoured. (xterm.js has no built-in
  path-insertion either; dragging a file to get its path is *emulator* behavior from
  Terminal.app/iTerm, so it had to be written by hand regardless of which layer got the event.)
  The event is also **window-wide**, not per-element: a drop on the sidebar or tab bar is
  indistinguishable from one on the terminal.
- **`escapePath` backslash-escapes; it does not quote.** Anything a shell would act on gets a
  `\`, which is what Terminal.app/iTerm insert on a drag — so the prompt reads
  `/Users/me/My\ File.csv`, matching stock Claude Code, rather than a quote-wrapped path.
  Characters ≥ U+0080 stay **bare**: real terminals don't escape unicode. The one exception is
  ANSI-C `$'…'` for control characters, which is a *PTY* concern rather than a shell one — a
  literal `\n` in a filename (legal on macOS) would submit the message halfway through the path,
  and `$'…'` keeps the wire bytes CR/LF-free while round-tripping to the real name.
- **The trailing space belongs INSIDE the paste markers.** It looks misplaced and isn't: a space
  written *after* `ESC[201~` **wipes the prompt** for any non-image path — the path renders and
  Claude then erases the line, presumably the keystroke racing its async paste handling. Images
  are unaffected by it, so testing only an image drop passes this bug straight through. Measured;
  don't "tidy" the space back outside.

The old behavior was a **silent** failure worth remembering as a pattern: the handler passed every
dropped path to `openOrFocusProject`, a file hit `state.rs`'s `bail!("not a directory")`, and that
message landed in `openError` — which renders *only inside the picker*, off-screen whenever a
project is open. Same shape as the Finder-launch `claude`-not-found bug: a real error with nowhere
to appear.

## Terminals kept alive while hidden

`TerminalPane` stacks the xterms of **every session across every open project** absolutely (keyed
`(handle,id)`); exactly one — the active project's active session — is `visibility: visible`, all
the rest `visibility: hidden` (**never** `display:none`, which would zero their size and break
`fit()`). Hidden terminals (including whole background *projects*) keep receiving `term.write()`,
so background Claudes keep rendering. Geometry is central: a `ResizeObserver` on the pane fits the
visible terminal, then applies the same `cols/rows` to every session + backend PTY (all PTYs share
one size, as the TUI did) — `refit` issues one `resize_session(handle,…)` per open project so
background projects aren't left at spawn size. **WebGL** is attached only to the one globally
visible terminal (browser caps live GL contexts) and disposed on blur / project switch.

## Lifecycle

- **Startup:** `lib.rs::setup()` reopens **every project in `open.txt`** (each builds its `Core` —
  scratch dir, config files, restore sessions — before the window paints; output buffers
  pre-attach). The frontend then calls `bootstrap()` → `WorkspaceInfo`, builds one xterm per
  session of each project, `attach_session`es each, and activates `active`. With no open projects
  it shows the picker (recents + `@tauri-apps/plugin-dialog` folder picker); opening one goes
  through `open_project(path)`.
- **No project ever auto-starts a `claude`.** `Core::open` restores what was worked on last time
  and stops there — with nothing to restore the project opens with **zero sessions** and waits for
  ⌘T. Because startup restore and `open_project` share that one path, a freshly added project
  behaves the same as a restored one. Zero sessions was already a supported state (⌘W on the last
  session produces it): `active` stays `0` and indexes nothing, `bootstrap_info` gives the frontend
  `activeSessionId: null`, `selectProject` calls `terminals.focus(handle, -1)` so no terminal is
  visible, and `TerminalPane` shows its "press ⌘T" empty state. Guarded by
  `state.rs::tests::open_with_nothing_to_restore_spawns_no_session`.
- **Teardown:** handled in `lib.rs` via `RunEvent` (managed-state `Drop` isn't guaranteed on
  exit). Window close → `app.exit(0)` → `ExitRequested` → `Workspace::teardown_all()` kills
  **every project's** process groups (`killpg` SIGHUP→SIGKILL→wait) **then** removes the whole
  scratch root. This is the "no orphaned claude" guarantee, now across all projects. `open.txt` is
  **not** touched on teardown, so the set survives to next launch.

## claude binary

Mulpex launches the **user's stock `claude`** (`pty.rs::claude_command`) — no byte-patching, no
re-signing. (The deprecated TUI shipped a `patch-claude-maxq.py` hack that raised the
`AskUserQuestion` caps to 10/10; that was intentionally dropped so instances behave exactly like a
plain `claude`. The matching "you may ask up to 10 questions/options" NOTE was removed from
`PLANNING_RULES` too — only the zero-assumptions planning discipline remains.)

### Finding it from a GUI launch (`claude_bin.rs`)

A bundle launched from **Finder inherits LaunchServices' environment, not a login shell's** —
`PATH` is the bare `/usr/bin:/bin:/usr/sbin:/sbin` and there is no `TERM`/`LANG`. `tauri dev`
hides this completely by inheriting the terminal's env, so both consequences only ever appeared
in the shipped `.app`:

- **`claude` was unfindable** (the installer puts it in `~/.local/bin`), so a bare
  `CommandBuilder::new("claude")` failed to spawn → `Core::open` errored → `open_project`
  returned `Err` → the frontend swallowed it in a `console.error`. Clicking a project did
  *nothing*, silently. `claude_bin::merged_path()` now rebuilds the real `PATH` — a
  `$SHELL -lic` probe (5 s timeout, killed on overrun; the only way to see nvm/asdf/volta) then a
  fallback list of known install dirs — resolves `claude` to an **absolute path**, and passes the
  same `PATH` to the child so *its* Bash tool finds `node`/`git`/Homebrew. Cached in a `OnceLock`,
  warmed on a background thread in `setup()` so nothing pays the probe inline.
- **Output was monochrome** — neither `portable_pty` nor `pty.rs` set `TERM`. The child talks to
  **xterm.js**, so we declare that emulator rather than inherit whatever started Mulpex:
  `TERM=xterm-256color` + `COLORTERM=truecolor`, plus `LANG` only when absent.

Failure is now visible rather than swallowed: a `claude_status` command backs a **picker banner**
when the CLI is missing, and `open_project` errors render inline in the picker.

## Hub listener — idle wake (A2)

By default a `claude` instance only reads its inbox when it takes a turn, so a message from a
peer sits unread while the instance is idle between the user's prompts. To make an idle instance
react on its own **without host-side stdin polling**, each instance runs the agentalk pattern
against the *local* hub:

- **Watcher:** the instance arms a **persistent `Monitor`** on a ~1 s poll of its own inbox dir
  (`$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID/`), emitting a `mulpex: N new hub message(s)`
  line only when new message files appear (seeded to the current count, so only post-arm
  arrivals fire). Each such line is a wake event the Claude Code runtime injects as a new turn —
  even while the instance is idle waiting for the user.
- **Arming (hook-driven, no injected prompt):** only the agent can arm its *own* Monitor, and a
  `--resume` restart kills the previous one — but instead of typing a visible bootstrap prompt
  into the PTY (which looked ugly/confusing on every spawn and resume), a normal instance now
  **starts completely clean** and arms its listener from the **`UserPromptSubmit` hook** on the
  user's first turn. The hook (`hook.rs::userpromptsubmit`) injects `ARM_LISTENER_NUDGE` as hidden
  `additionalContext` — a low-key "arm your listener quietly as part of this turn" reminder — but
  only while `listener_armed(ctx)` is false, i.e. while `state_dir/armed/<id>` is absent. The
  Monitor command in `HUB_RULES` **`touch`es that flag as its first action**, so the flag tracks
  the *real* Monitor: once armed the reminder stops; if arming was missed it re-injects next turn
  (self-healing). Because `state_dir` is fresh per Mulpex launch (hence per `--resume`), the flag
  is absent at startup, so restored instances re-arm on their first prompt. The full arming
  procedure + exact Monitor command still live in `HUB_RULES` (append-system-prompt, so the wake→act
  contract survives compaction). *(`hub_spawn` children are the one case that still gets an injected
  PTY prompt — their assigned task — via `pty.rs::spawn_prompt`; they arm the listener from the
  same hook on that first turn.)*
- **On wake (auto-act):** the instance calls `mcp__mulpex__hub_inbox`, acts on the message(s)
  autonomously, replies to the sender only when it adds value (no bare acks), and prefixes the
  self-triggered turn with a `⟳ hub message from #<sender> →` marker so the human can tell it
  wasn't their prompt. This coexists with the `userpromptsubmit` hook's unread-count nudge, which
  still covers the "notice on your next prompt" path.

## Spawning instances (`hub_spawn`)

An instance can create new task-seeded siblings — e.g. fetch a list of tickets and spawn one
instance per ticket. The MCP helper runs in a **separate process** from Tauri and can't create
sessions itself, so `hub_spawn` (`mcp.rs`) is a **file handshake** through the poll loop:

- **Request:** `hub_spawn({tasks: [...]})` writes `state_dir/spawn/<token>.json`
  (`{from, tasks, ts}`), capped at `MAX_SPAWN_PER_CALL` (8) so a 50-item list can't fork 50
  `claude`s at once, then **polls** for `<token>.done` (~6 s) to return the assigned ids.
- **Fulfilment:** the 200 ms poll loop calls `Core::process_spawn_requests()` (`state.rs`), which
  for each task spawns a session via `spawn_instance_with_task(parent_id, task)`, writes the ids
  to `<token>.done`, and — if anything spawned — emits `sessions-changed` so the frontend builds
  the new xterms (the existing reap path already republishes on removal; added sessions ride the
  same event via `TerminalPane`'s keyed `{#each}`).
- **Seeding + link:** the child's one-shot PTY prompt (`pty.rs::spawn_prompt`) is just the task:
  start it, then `hub_send` a summary back to the spawner when done (listener arming is *not* in
  this prompt — it comes from the `UserPromptSubmit` hook like every instance). Still
  `[mulpex:hub]`-sentinel-prefixed (skips the sidebar task-capture) and a single line (task
  whitespace collapsed). The child is **auto-named** `name_from_task(task)` so the sidebar labels
  it, and is **not** focused — the user stays on their pane while children appear. Recursion is
  inherent (children also have `hub_spawn`); only the per-call cap bounds a single call.

## Shared-tree guardrail

All instances **of a given project** share one working directory and one git checkout (not a
worktree per instance; separate projects are separate trees), so
a tree-wide git op by one (`reset --hard`, `checkout .`/`restore .`, `clean`, `stash`, branch
switch, `rebase`/`revert`) or any bulk-destructive command wipes every other instance's
uncommitted work. `HUB_RULES` (the append-system-prompt) tells each instance to treat these as
dangerous: check `hub_instances` first, coordinate via `hub_send` or ask the user if any peer is
live, and ask the user even when alone. This is **prompt-level guidance only** — no PreToolUse
hard block (that was considered; command-detection is heuristic and shell-bypassable).

## Verified so far

- Both crates + the Tauri app compile clean; `svelte-check` + `vite build` clean.
- The whole coordination hub works **end-to-end through `mulpex-helper`**: MCP `initialize` /
  `tools/list` (all 6 `hub_*` tools), `hub_send`→`hub_inbox` delivery + `messages.log`,
  `userpromptsubmit` task capture + peer snapshot, and `pretooluse` `O_EXCL` lock acquisition
  with a canonical-path + heartbeat token.
- **Run as a GUI and bundled.** `npm run tauri dev` and `tauri build` (→ signed `Mulpex.app`
  with the `mulpex-helper` sidecar inside `Contents/MacOS/`) both work; multi-instance spawn,
  focus-switch, resize, and session `--resume` verified in the window.
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

## Known open bug — scratch root leaks on quit

Measured while restarting for the v0.4.5 test: after `osascript -e 'tell application "Mulpex" to
quit'`, **every `claude` was dead (zero orphans, so the `killpg` half of `teardown_all()` ran) but
the whole `temp/mulpex-<pid>/` tree survived intact** — all 5 project state dirs, 88 entries. The
documented guarantee is kill-then-`remove_dir_all` of the scratch root, and only the first half
happened.

Only the **Apple Event** quit path was measured; ⌘Q and the window close button are untested here,
and the code routes window-close through `app.exit(0)` → `ExitRequested`, which an Apple Event quit
may bypass entirely. Impact is mild (abandoned temp dirs per launch, eventually reaped by macOS),
but it *is* a violated invariant. Worth checking all three quit paths against `RunEvent`.

## Last Synced Commit

`6367db7da469ceb58ee249bd8e946a26167806bc` — 2026-07-26
