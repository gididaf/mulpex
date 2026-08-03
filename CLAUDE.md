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
  lib/updater.ts      update check/download/apply + the cross-project busy-session count
  lib/components/*     ProjectTabBar, CommandPalette, TopBar, InstanceList, HubPanel,
                      TerminalPane/View, MessageReader, Rename, UpdateBanner…
scripts/release.sh    signed build → latest.json → gh release (see Auto-update)
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
  persistent **`ProjectTabBar`** (click / `+` open / ✕ close) plus a **`⌘P`
  `CommandPalette`** fuzzy switcher (drag-and-drop no longer opens projects — see
  **Dropped paths** below). `TerminalManager` keys xterms by `(handle,id)` and keeps
  **every project's** terminals alive while hidden; exactly one is visible globally.
- **Persistence:** the open-project set is saved to `~/.mulpex/open.txt` (distinct from
  `recents.txt`) on open/close; on launch every project reopens (`--resume`).

## Keyboard

Native macOS menu accelerators (⌘T/⌘W/⌘R/⌘M/⌘⇧M/⌘[ ⌘]/⌘O/⌘Q, plus **⌘⇧W** close project and
**⌘⇧] / ⌘⇧[** next/prev project) are intercepted by the menu before xterm; Claude never uses ⌘,
so there's zero collision. **⌘P** (the project quick-switcher) is *not* a menu accelerator — it's
handled in the webview (`svelte:window` keydown, `preventDefault` stops the print dialog).

**⌘M is Mute Session; the message reader moved to ⌘⇧M.** ⌘M has a *third* claimant nobody
declares: muda hard-binds `PredefinedMenuItem::minimize` to ⌘M and exposes no accelerator setter.
Two items on one key means the earlier menu silently wins (the app already ships that collision —
File ▸ Close Session and Window ▸ Close Window both claim ⌘W), which would leave the Window menu
advertising a ⌘M that mutes. So **Minimize is a custom item with no accelerator**, handled in
`App.svelte` via `getCurrentWindow().minimize()`. Don't "restore" the predefined one.

Mute is a **`CheckMenuItem`**, and muda flips a check item's own state *before* dispatching the
event — so the tick has to be pushed back from the frontend (`set_mute_menu_checked` →
`menu::set_mute_checked`, which walks one level into the Session submenu because `Menu::get` only
searches direct children). `App.svelte::syncMuteMenu` dedups that IPC call, since its trigger
re-runs on every 200 ms hub poll; the one `force` case is a ⌘M with no session focused, where muda
has ticked an item we never wrote and the dedup would otherwise never clear it.

**⌘1–⌘9 select the Nth open *project*, not the Nth session** (menu ids `project_<n>`, File menu).
Projects are the top-level thing you switch between — sessions have ⌘[ / ⌘] and the sidebar — and
⌘N-selects-a-tab is the browser/terminal convention the tab bar already implies. The index is the
`projects` Map's insertion order, which is exactly what `ProjectTabBar` renders, so ⌘3 is always
the third visible tab.

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

## Muted sessions (⌘M)

A muted instance **keeps running and keeps coordinating** — same PTY, same inbox, same peer list,
same `hub_instances` entry. Mute is purely a statement about how loudly the *sidebar* may talk
about it, and it's deliberately not a hub concept: nothing in `mulpex-core` knows the flag exists.
Concretely it: dims the row, **sinks it below the unmuted ones**, drops its status dot, its status
word and its ⏳, and removes it from **every attention count** — the tab's red `needs` badge, the
amber unread badge, and the hub-panel/status-strip unread readouts.

- **Ordering is one function**, `stores.ts::displayOrder` — a stable sort on `Number(muted)`, so
  each group keeps creation order and unmuting drops a session straight back where it came from.
  It feeds both the sidebar and ⌘[ / ⌘], so what you see is what you cycle. `TerminalPane` is
  unaffected (absolute stacking; order is meaningless there).
- **The unread badge needed a backend change.** `pending_messages` is one project-wide total, and
  "how much of this is mail for a muted instance" isn't answerable from a total — so the poll loop
  now also emits a per-recipient `pending: Vec<PendingEntry>` breakdown, and `unreadCount`
  subtracts the muted share. The **message log itself is untouched**: mute silences the count that
  pulls your eye, not the record of what happened.
- **Persisted per project**, alongside the custom name, as a third tab-separated field in
  `~/.mulpex/sessions/<key>.txt` (`<uuid>[\t<name>[\tmuted]]`). Both older formats still load — a
  bare uuid and a `<uuid>\t<name>` line — and a muted-but-unnamed instance writes the name column
  empty so the flag stays in field three. Covered by three `persist.rs` tests.
- **Muting never moves focus**, and the muted terminal stays visible and typeable. Mute means "stop
  shouting at me", not "I'm done with this one".
- **The 🔇 is not decoration.** A dimmed, dot-less, status-less row would otherwise read as *dead*
  rather than *silenced* — same failure the empty hub-panel sections had, an ambiguous readout that
  teaches the eye wrong. It's also the click target for muting a session **without focusing it**
  (unmuted rows show a 🔊 only on hover, so it stays reachable without adding noise).

**A new menu item is not wired until `lib.rs::is_forwarded` lists its id.** That function is an
*allowlist* — `on_menu_event` drops anything not in it — so a new item builds, appears in the menu,
shows its accelerator, and (for a `CheckMenuItem`) even ticks itself on click, while the frontend
never hears a thing. Nothing fails: no error, no log line, no compiler complaint. Both `mute` and
`minimize` shipped in that state and were caught only by driving the real app. This is the same
shape as the other silent failures in this file — a real event with nowhere to arrive.

## What a project tab shows

Name + **session count** (always, `0` included — "nothing running here" is information) + two
count badges, each for a different ask, so a colored pill is never ambiguous: **red =
sessions in `needs`** (a claude stopped to ask *you* something) and **amber = unread hub
messages**. Both hide at zero, and **both exclude muted sessions** (see above) — the plain session
count does not, because it says what's *here*, not what wants you. The needs count is the gap this closes —
a background project blocked on a question used to look identical to an idle one, findable only by
switching tabs, even though `ProjectState.statuses` had the answer all along. ⌘1–9 selects a tab
(see **Keyboard**).

**Tabs drag to reorder.** `ProjectTabBar` uses **pointer events, not HTML5 drag-and-drop** —
Tauri's webview drag-drop is enabled (App.svelte needs it to drop folders onto the window) and
intercepts drags before the DOM sees them; pointer capture also gives us the 4 px threshold that
keeps a click from registering as a drag. Dropping calls `reorderProjects` in `stores.ts` (rebuilds
the `Map`, since insertion order *is* tab order) and the `reorder_projects` command, which reorders
`Workspace::projects` and re-runs `persist_open()` — so the arrangement survives relaunch. Tab
order is also what ⌘1–9 index into, so a drag remaps them by design. Handles missing from the
submitted order are appended rather than dropped, so a stale caller can't make a project vanish.

## Attention: dock badge + notifications

`attention.ts` surfaces blocked claudes when you're *not* looking at Mulpex, both keyed off
`needs` (the status the `AskUserQuestion` / idle-prompt hooks write — see `config.rs`):

- **Dock badge** — `blockedTotal` (`stores.ts`) sums `needsCount` across *all* open projects and
  drives `setBadgeCount`. Zero must be passed as `undefined`, or the dock shows a literal "0".
- **Notification** — one silent banner per claude at the moment it becomes blocked, only when the
  window is unfocused. Clicking one raises the window and routes through the same select path as
  clicking a sidebar row, landing you on the pane with the question (the project handle + session
  id ride along in the notification's `extra`).

Three deliberate choices. It tracks `needs` and **not** `waiting`: `waiting` only means a turn
ended, which happens constantly and asks nothing of you — badging it would leave the dock lit
permanently and stop meaning "there is something to do". Muted sessions are excluded, matching the
tab badges. And the first sweep only *records* state (`primed`), because restored sessions can
already be in `needs` at launch and a burst of stale banners would bury the live one.

> `needs` fires less often than you'd guess: sessions run with `--dangerously-skip-permissions`, so
> the `permission_prompt` matcher is effectively dead and `needs` means AskUserQuestion or idle.

## Hub panel is anomaly-only

The sidebar's hub panel shows **Waiting** and **Locks** only when they're non-empty — no header,
no `none` placeholder — leaving **Messages** as the only permanent section. Locks release at *turn*
boundaries (`hook.rs::release_my_locks` on Stop), not per tool call, so with a single session in a
project the lock list is always that session's own files: a permanent three-section "none / none /
none" readout is noise that teaches the eye to skip the panel, so the one time contention *does*
happen you don't see it. Locks are additionally suppressed at `sessions.length <= 1` (nothing can
contend with you). Waiting sorts above Locks — the blocked instance is the headline, the lock is
the explanation — and contention is flagged a second time where you're already looking, as a ⏳ on
the blocked session's row in `InstanceList`. Don't "restore" the empty-state sections.

The sidebar splits **72% session list / 28% hub panel** (was 45/55 — messages are reference
material, the session list is what you steer with). Both rows scroll independently, which needs
`min-height: 0` on the grid items (`.sidebar > :global(*)` in `App.svelte`): without it a grid
item's *auto* minimum lets it grow past its track and the children's `overflow-y: auto` never
engages.

## Terminals kept alive while hidden

`TerminalPane` stacks the xterms of **every session across every open project** absolutely (keyed
`(handle,id)`); exactly one — the active project's active session — is `visibility: visible`, all
the rest `visibility: hidden` (**never** `display:none`, which would zero their size and break
`fit()`). Hidden terminals (including whole background *projects*) keep receiving `term.write()`,
so background Claudes keep rendering. Geometry is central: a `ResizeObserver` on the pane fits the
visible terminal, then applies the same `cols/rows` to every session + backend PTY (all PTYs share
one size, as the TUI did) — `refit` issues one `resize_session(handle,…)` per open project so
background projects aren't left at spawn size.

## RTL (Hebrew/Arabic) — two separate fixes, both load-bearing

Terminals use xterm's **DOM renderer**. The WebGL addon was removed for this fix and **must not
come back for speed** — it draws one glyph quad per cell, so column *n* always gets character *n* and
RTL text renders mirrored (Hebrew read backwards). The DOM renderer emits each styled run as a
`<span>` of real text and the **browser's own BiDi engine** reorders it for free. Measured, same
frame through the app's xterm 5.5.0 in headless Chrome: DOM → `שלום זאת בדיקה`, WebGL →
`הקידב תאז םולש` (the reported bug, reproduced). xterm has **no BiDi of its own** (`grep -c
"bidi\|rtl"` on `lib/xterm.js` is 0), so the browser is the only implementation available; a
`unicode-bidi: plaintext` CSS override on the rows changes nothing (already the default behavior),
and `direction: rtl` flips the box-drawing borders and is unusable. Dropping the addon also
deleted the GL-context juggling (attach-on-focus / dispose-on-blur, since browsers cap live
contexts) and cut ~100 kB from the JS bundle.

**That alone only fixed the letters.** Words still ran left-to-right, because xterm's DOM renderer
injects `.xterm-rows span { display: inline-block }` — and an inline-block is an **atomic** inline
box, which the BiDi algorithm treats as one opaque object. It can reorder text *inside* a span but
never *across* spans, and Claude Code colors words individually (`<span>Opus</span><span>
</span><span>5</span>…`), so a Hebrew sentence became one span per word: letters right, words
mirrored-wrong. `src/styles.css` overrides it with `display: inline !important` (`!important` is
required — the injected rule is more specific and lands in `<head>` later). Measured on a 7-span
row: `inline-block` → first word leftmost; `inline` → whole run mirrored, correct. `inline` rather
than `display: contents` on purpose — the span keeps its box, so background colors, the block
cursor and wide-char widths still paint (CJK/emoji x-positions verified byte-identical).

How this was found, for the next RTL bug: a Python `pty.fork()` harness drove a real `claude`,
typed the sentence keystroke-by-keystroke and captured the raw bytes; those bytes were replayed
into the app's own xterm in headless Chrome, and **each character's x-position was measured** with
`Range.getBoundingClientRect` rather than eyeballed — reading Hebrew out of a screenshot is
useless here, because transcribing it silently re-applies BiDi and hides which end is which.

Residual limit: the caret is still column-based, so it can sit visually off inside an RTL run.

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
  consumes the request file into a `pending_spawns` queue and then **drip-feeds** it — at most one
  child per tick, and no closer together than `SPAWN_STAGGER` (500 ms) — spawning each via
  `spawn_instance_with_task(parent_id, task)`. `<token>.done` is written only once the batch's
  last child is up, so the caller still gets every id in one reply. If anything spawned it emits
  `sessions-changed` so the frontend builds the new xterms (the existing reap path already
  republishes on removal; added sessions ride the same event via `TerminalPane`'s keyed
  `{#each}`). The drip-feed **never sleeps** — this runs on the shared poll loop, so blocking
  would stall every project's UI. Staggering exists because N simultaneous `claude` cold starts
  contend hard enough to blow any injection deadline (see below).
- **Seeding + link:** the child's one-shot PTY prompt (`pty.rs::spawn_prompt`) is just the task:
  start it, then `hub_send` a summary back to the spawner when done (listener arming is *not* in
  this prompt — it comes from the `UserPromptSubmit` hook like every instance). Still
  `[mulpex:hub]`-sentinel-prefixed (skips the sidebar task-capture) and a single line (task
  whitespace collapsed). The child is **auto-named** `name_from_task(task)` so the sidebar labels
  it, and is **not** focused — the user stays on their pane while children appear. Recursion is
  inherent (children also have `hub_spawn`); only the per-call cap bounds a single call.
- **Injection is verified, not fire-and-forget** (`pty.rs`). Typing the task in requires the
  child's input box to actually exist, and nothing about the PTY stream says so directly. The
  injector therefore: (1) waits for a *drawn input box* — `input_box_ready` looks for the rounded
  box chrome in a rolling `TAIL_CAP` tail of output — rather than for "painted then quiet", which
  a mid-startup lull while MCP servers load imitates perfectly; (2) types the prompt, then sends
  `\r` separately (a `\r` at the tail of a fast burst is treated as paste content, not a submit);
  (3) **verifies** via `turn_started`, which reads the child's own `state_dir/<id>` status file —
  the `UserPromptSubmit` hook writes `working` there the moment a prompt submits, so this is
  positive proof the text landed; (4) retries up to `INJECT_ATTEMPTS`, clearing the box with
  Ctrl-U first so partial attempts can't concatenate.

  **Why it's built this way:** the original injector waited on a quiet-window heuristic with an
  8 s hard cap, then typed regardless. That survived the one-child case but failed *every* child
  of a six-way spawn — six concurrent cold starts all exceeded 8 s, so each typed into a TUI with
  no input box and the bytes were dropped, leaving six idle instances with no task and no way to
  recover (their hub listeners arm from the first turn, which never came, so even `hub_send`
  couldn't reach them). Readiness detection alone would not be enough — it can always be wrong on
  a future `claude` whose chrome differs — which is why verification plus retry is the part that
  actually makes this robust, and why the readiness check can afford to key off TUI chrome.

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
- **RTL / Hebrew.** Both halves measured rather than eyeballed (see **RTL** above), then
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
  `ready` gate above.

## Auto-update

`tauri-plugin-updater` against the repo's GitHub releases. Checked at launch and every **6 h**
(`updater.ts::CHECK_INTERVAL_MS`), plus **Mulpex ▸ Check for Updates…** on demand; an available
version raises a fixed card (`UpdateBanner.svelte`) with **Update & Restart**.

- **The `.dmg` is not the update channel.** The updater consumes `Mulpex.app.tar.gz` + `.sig`
  (emitted by `bundle.createUpdaterArtifacts`), verifies the minisign signature against the
  `plugins.updater.pubkey` compiled into the app, and swaps the bundle in place. The DMG stays the
  first-install channel only. All four artifacts must land on the **same** GitHub release —
  `latest.json` is fetched from `/releases/latest/download/`, which resolves to the newest
  *published, non-prerelease* release, so a draft ships an update nobody can see.
- **`xattr -dr com.apple.quarantine` does not come back.** `com.apple.quarantine` is written by
  the *downloading* app (a browser, via LaunchServices); the updater fetches over the app's own
  HTTP client, so nothing sets the xattr and the extracted bundle inherits none. Gatekeeper's
  first-launch assessment only fires on quarantined bundles. One manual `xattr` on the first DMG
  install, never again. Ad-hoc signing (what `tauri build` does here — `Signature=adhoc`,
  `TeamIdentifier=not set`) stays fine: there is no cert continuity to break.
- **Restart goes through `AppHandle::request_restart`** (`commands::restart_app`), not
  `plugin-process`'s `relaunch` and **not `AppHandle::restart`**. The restart has to fire
  `ExitRequested`/`Exit` on the way out, because that is what runs teardown — otherwise every
  update orphans process groups and leaks a scratch dir. `relaunch()` doesn't fire them at all,
  and `restart()` only *sometimes* does: its own docs say that called **on the main thread** it
  "cannot guarantee the delivery of those events, so we skip them" and re-execs immediately.
  Whether a command body runs on the main thread is Tauri's scheduling choice — measured today it
  is *not*, and `restart()` did fire the events and did tear down correctly, so this is a bug that
  would have stayed invisible until a runtime upgrade moved the thread. `request_restart` always
  routes through `request_exit(RESTART_EXIT_CODE)`. The leak fix below is a prerequisite for all
  of this, not a nicety.
- **Busy guard.** `updater.ts::busySessionCount()` counts `working` (mid-turn) and `needs`
  (stopped on a question) across **every open project**, not just the visible one; non-zero parks
  the banner in a `confirming` state naming the count. `waiting` sessions don't count — `--resume`
  restores those intact.
- **Automatic checks are silent on failure; manual ones aren't.** A laptop on flaky wifi must not
  accumulate error banners nobody asked for, but a menu-item check that silently did nothing would
  read as broken. Same function, `manual` flag.
- **The banner is NOT gated on `ready`** — don't "tidy" it back inside that block. `ready` flips
  only after `bootstrap()` has walked every open project and built + attached an xterm for every
  session, serially, so with several projects restoring, the launch check finishes early and the
  card would sit invisible for as long as bootstrap took. That is exactly what the first user
  report of "the banner only appears if I click Check for Updates" was: the check had worked
  fine. Measured against the real endpoint, with zero projects the banner is up ~6 s after
  launch. The card is fixed-position and owns nothing bootstrap provides.
- **Shipped in v0.4.7**, which by construction had to be installed by hand — it is the release
  that *adds* the updater, so nothing older could deliver it. v0.4.8 was a deliberate no-op
  release published to exercise the real path end to end.
- **Releasing:** `npm run release` (`scripts/release.sh`) — preflights the key, the
  tauri.conf.json/Cargo.toml version agreement, a clean tree and an unused tag; builds; writes
  `latest.json`; `gh release create`s all four artifacts. `--dry-run` builds and writes the JSON
  without publishing.
- **The signing-key gotcha, which costs a full release compile to rediscover:** `tauri signer
  generate` prints `TAURI_SIGNING_PRIVATE_KEY_PATH`, but the v2 bundler reads **only**
  `TAURI_SIGNING_PRIVATE_KEY` (contents or path). With just the `_PATH` form set, the build runs to
  completion, emits the `.tar.gz`, and *then* dies with "A public key has been found, but no
  private key". Both `release.sh` and the `tauri:build` npm script export the key contents. The key
  lives at `~/.mulpex/updater.key` (0600, no password) and is **not** in the repo — lose it and no
  existing install can ever accept another update; the only recovery is a new keypair plus a manual
  DMG reinstall by every user.

## Teardown fires on TWO RunEvents (the fixed scratch-root leak)

`lib.rs` matches **`RunEvent::ExitRequested | RunEvent::Exit`**, and dropping either arm
re-opens a measured bug. `ExitRequested` is only reachable through `app.exit()` — i.e. the
window-close arm and `AppHandle::restart()`. **⌘Q and an Apple-Event `quit` never touch it:**
they go to Cocoa's `NSApplication terminate:`, which tao turns into `applicationWillTerminate:` →
`AppState::exit()` → `Event::LoopDestroyed`, and tauri-runtime-wry maps *that* to
`RunEvent::Exit`. Matching only `ExitRequested` meant teardown never ran on the two quit paths a
human actually uses.

The old diagnosis in this file inverted the evidence: it read "every `claude` was dead" as proof
the `killpg` half ran and only `remove_dir_all` failed. **Neither half ran.** The children died
from the PTY hangup when the process exited — which kills the foreground process group only, so
anything an instance had backgrounded was orphaned, exactly the case `killpg` exists for.

Measured before/after on an isolated `HOME` with one empty project (`scratchpad/measure-quit.sh`):
AppleScript quit and ⌘Q both **leaked** the whole `temp/mulpex-<pid>/` tree and both are now
**clean**; window-close was clean throughout; the Quit *menu item* is clean too. `teardown` is
idempotent, so window-close firing both events and running it twice is fine.

**Second layer: `Workspace::sweep_stale_state_roots()`**, run in `setup()` before the new root is
created. No shutdown hook can cover Force Quit, `kill -9`, a crash or a power loss, so each launch
also collects `temp/mulpex-<pid>` dirs whose pid is no longer alive (`libc::kill(pid, 0)`, with
`EPERM` counting as alive). It errs toward *keeping*: a recycled pid just defers that dir to a
later launch, whereas deleting a live Mulpex's root would break its running hub. This is what
finally collected the 12 dirs the old bug had accumulated on this machine.

## Last Synced Commit

`244a95f214821634e173db3b9de285c9159e3515` — 2026-07-27
