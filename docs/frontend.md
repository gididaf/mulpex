# Frontend behavior (Svelte UI)

Sidebar, tabs, badges, drops, mute, drag-reorder, attention. Read this before changing anything in
`src/lib/components/`, `stores.ts`, `attention.ts`, or `App.svelte`.

Rendering invariants (geometry, RTL) live in [rendering.md](rendering.md).

Back to [CLAUDE.md](../CLAUDE.md).

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
  each group keeps its base order (creation order, or whatever a drag arranged — see **Sessions
  drag to reorder**) and unmuting drops a session straight back where it came from. It feeds both
  the sidebar and ⌘[ / ⌘], so what you see is what you cycle. `TerminalPane` is unaffected
  (absolute stacking; order is meaningless there).
- **The unread badge needed a backend change.** `pending_messages` is one project-wide total, and
  "how much of this is mail for a muted instance" isn't answerable from a total — so the poll loop
  now also emits a per-recipient `pending: Vec<PendingEntry>` breakdown, and `unreadCount`
  subtracts the muted share. The **message log itself is untouched**: mute silences the count that
  pulls your eye, not the record of what happened.
- **Persisted per project**, alongside the custom name and the instance number, as the third
  tab-separated field in `~/.mulpex/sessions/<key>.txt`
  (`<uuid>[\t<name>[\tmuted[\t<id>]]]` — see the store's positional columns in
  [sessions.md](sessions.md)).
  Every older format still loads — a bare uuid, a `<uuid>\t<name>` line, and a three-column
  pre-id line — and because the columns are positional a muted-but-unnamed instance writes the
  name column empty so the flag stays in field three. Covered by five `persist.rs` tests.
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
shape as the other silent failures in these notes — a real event with nowhere to arrive (see
**How this codebase fails** in [../CLAUDE.md](../CLAUDE.md)).

## Sessions drag to reorder

Sidebar rows drag vertically exactly as project tabs drag horizontally — same mechanism
(**pointer events, not HTML5 drag-and-drop**, because Tauri's webview drag-drop is enabled for
dropped paths and intercepts DOM drags; pointer capture also gives the 4 px threshold that keeps a
click from registering as a drag), same `suppressClick` so a drag never also selects the row, same
visuals (dragged row fades, target slot gets an accent edge — on the *top* edge here, since the
list runs vertically). Terminals drag like instances: one list, one behavior.

- **Manual order and muted-sinking are composed, not alternatives.** `p.sessions` is the *base*
  order a drag rewrites; the sidebar renders `displayOrder(p.sessions)` on top of it, so muted rows
  still sink. That means a drop **across the mute boundary could never stick** — the row would
  visibly snap back on release. So drops are **clamped to the dragged row's own group**
  (`stores.ts::clampToGroup`): dragging a muted row to the top lands it at the top of the *muted*
  block. Clamping keeps the drop indicator honest and keeps the emitted order already-grouped,
  which is the invariant that makes `displayOrder` of it the identity — i.e. the frontend's
  optimistic repaint is exactly what the backend echoes back. The math lives in `stores.ts` next to
  `displayOrder` (`clampToGroup` / `dragOrder`) rather than in the component, because it is a
  consequence of that sort and the two rules have to stay in one file.
- **Persisted, via the backend.** `reorder_sessions(handle, ids)` → `Core::reorder_sessions`
  rearranges the `sessions` vec, which *is* the persisted order (`persist_sessions` walks it), so a
  drag survives relaunch like a tab drag does. Terminals have no position after a restart because
  terminals themselves aren't persisted.
- **`Core.active` is an index into that vec**, so it must be re-derived from the focused session's
  *id* after a reorder — carrying the index across would silently focus whichever session slid into
  that slot. Guarded by `reordering_sessions_keeps_focus_and_never_drops_one`, which also pins the
  never-drop contract (ids the caller omitted are appended, unknown ids ignored) shared with
  `Workspace::reorder_projects`.
- Sidebar order is what ⌘[ / ⌘] cycle, so a drag remaps those too — the same "what you see is what
  you cycle" rule the muted sort already follows.

## What a project tab shows

Name + **session count** (always, `0` included — "nothing running here" is information) + two
count badges, each for a different ask, so a colored pill is never ambiguous: **red =
sessions in `needs`** (a claude stopped to ask *you* something) and **amber = unread hub
messages**. Both hide at zero, and **both exclude muted sessions** (see above) — the plain session
count does not, because it says what's *here*, not what wants you. By the same rule the plain count
includes **terminals**, while the badges exclude them for free (a terminal has no status entry and
no inbox). The needs count is the gap this closes —
a background project blocked on a question used to look identical to an idle one, findable only by
switching tabs, even though `ProjectState.statuses` had the answer all along. ⌘1–9 selects a tab
(see **Keyboard** in [../CLAUDE.md](../CLAUDE.md)).

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
> the `permission_prompt` matcher is effectively dead and `needs` means AskUserQuestion, or idle
> **with nothing of its own still running** — see **`needs` must mean "needs YOU"** in
> [sessions.md](sessions.md).

## Hub panel is Messages only

`HubPanel.svelte` renders **Messages** and nothing else. It used to show **Waiting** and **Locks**
above it, anomaly-only (rendered only when non-empty — no header, no `none` placeholder), and both
were removed at the user's request: neither is something you steer with. Locks release at *turn*
boundaries (`hook.rs::release_my_locks` on Stop), not per tool call, so with a single session in a
project the lock list was always that session's own files — which is why it was already suppressed
at `sessions.length <= 1`, and why it was never worth the panel row. Don't "restore" either
section without asking.

Contention is still visible, in the place you're already looking: the ⏳ on the blocked session's
row in `InstanceList`, which reads `$hub.waiting` directly. So the **backend snapshot still carries
`locks` and `waiting`** (`ipc.ts::HubSnapshot`, `snapshot.rs`) — this was a UI-only removal, and
stripping the fields would break that ⏳.

The sidebar splits **72% session list / 28% hub panel** (was 45/55 — messages are reference
material, the session list is what you steer with). Both rows scroll independently, which needs
`min-height: 0` on the grid items (`.sidebar > :global(*)` in `App.svelte`): without it a grid
item's *auto* minimum lets it grow past its track and the children's `overflow-y: auto` never
engages.

