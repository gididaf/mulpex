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
  key carve-out is **Shift+Enter → `\n`** (`attachCustomKeyEventHandler`); `macOptionIsMeta`
  covers Option word-motions.
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
  `CommandPalette`** fuzzy switcher. **Drag-and-drop** a folder onto the window opens it
  (`getCurrentWebview().onDragDropEvent`). `TerminalManager` keys xterms by `(handle,id)` and keeps
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
  scratch dir, config files, restore/spawn sessions — before the window paints; output buffers
  pre-attach). The frontend then calls `bootstrap()` → `WorkspaceInfo`, builds one xterm per
  session of each project, `attach_session`es each, and activates `active`. With no open projects
  it shows the picker (recents + `@tauri-apps/plugin-dialog` folder picker); opening one goes
  through `open_project(path)`.
- **Teardown:** handled in `lib.rs` via `RunEvent` (managed-state `Drop` isn't guaranteed on
  exit). Window close → `app.exit(0)` → `ExitRequested` → `Workspace::teardown_all()` kills
  **every project's** process groups (`killpg` SIGHUP→SIGKILL→wait) **then** removes the whole
  scratch root. This is the "no orphaned claude" guarantee, now across all projects. `open.txt` is
  **not** touched on teardown, so the set survives to next launch.

## claude binary

Mulpex launches the **user's stock `claude`**, resolved via `PATH` (`pty.rs::claude_command`) —
no byte-patching, no re-signing. (The deprecated TUI shipped a `patch-claude-maxq.py` hack that
raised the `AskUserQuestion` caps to 10/10; that was intentionally dropped so instances behave
exactly like a plain `claude`. The matching "you may ask up to 10 questions/options" NOTE was
removed from `PLANNING_RULES` too — only the zero-assumptions planning discipline remains.)

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

## Last Synced Commit

`bc903ca3ba1761280f973171b67511c24a799d09` — 2026-07-25
