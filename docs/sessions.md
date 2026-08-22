# Instances: launch, status, identity, persistence

How a `claude` instance is born, what its status word means, and how it is remembered across
restarts. Read this before touching `pty.rs::claude_command`, `claude_bin.rs`, `hook.rs`'s status
writes, `persist.rs`, or `state.rs::reap_dead`.

Back to [CLAUDE.md](../CLAUDE.md).

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

- **Every instance opened logged out** — the symptom is `Not logged in · Please run /login` in
  every pane at once, and it is the same fact one layer up: `PATH` was never the only casualty.
  `portable_pty` passes *our* environment through, and a Finder-launched bundle's environment is
  LaunchServices', which never sourced an rc file — so **nothing the user exports reaches the
  child**. For anyone authenticating by token (`CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY`
  exported from `.zshrc`, commonly by a refresher daemon, with **no `~/.claude/.credentials.json`
  on disk**) that variable *is* the entire credential, so every ⌘T instance opened logged out
  while the user's own terminal was fine.

  So the probe now harvests the **whole login environment** (`$SHELL -lic '… env -0 …'`, NUL-
  separated so a value containing a newline survives) and `pty.rs::base_env` forwards it to every
  child. What is *not* forwarded is `claude_bin::DENY`/`DENY_PREFIX`, each entry with its reason:
  `PATH` (replaced by the merged one), `TERM`/`COLORTERM` and the terminal-identity vars (the
  child talks to xterm.js, not to whatever ran the probe), `CLAUDE_CODE_CHILD_SESSION`/
  `ENTRYPOINT` (inheriting the child marker silently disables transcript saving and breaks
  `--resume`), `IS_SANDBOX` and `MULPEX_*` (hub identity is assigned per session — and if Mulpex
  was launched from inside a Mulpex claude, the login shell carries *that* instance's id), and the
  shell's own bookkeeping. `LANG` now comes from the login shell when it has one; the
  `en_US.UTF-8` fallback only fires when neither environment names one.

  **The environment is re-probed every `ENV_REFRESH_INTERVAL` (10 min) on the warm-up thread, and
  `PATH` deliberately does not follow.** Auth tokens rotate and Mulpex is left open for days, so a
  value pinned at launch eventually logs every *new* instance out — but `resolve_claude` hands out
  an absolute path derived from the first `PATH`, and a `PATH` disagreeing with the binary already
  chosen is worse than a stale one. A failed re-probe keeps the last good environment: a stale
  token beats no token.

  Two reasons this stayed invisible for so long, both the usual ones: a ⌘⇧T **terminal was never
  affected** (`$SHELL -l -i` sources the rc files itself, so the user's own shell always looked
  healthy), and it **cannot reproduce under `tauri dev`**, which inherits the launching terminal's
  environment. The pane also says `Please run /login`, which reads as a Claude Code problem rather
  than a Mulpex one.

Failure is now visible rather than swallowed: a `claude_status` command backs a **picker banner**
when the CLI is missing, and `open_project` errors render inline in the picker.

## Status words: what `needs`, `working` and `waiting` mean

The sidebar dot, the dock badge, the red tab badge and the updater's busy guard all read one
word per instance, written by the hooks in `mulpex-core`. Two things that look like idleness are
not, and each needed its own fix.

### `needs` must mean "needs YOU", not "its agent is still going"

An instance that launches a **background agent** (or a `run_in_background` shell) ends its turn and
is then woken later by a `<task-notification>`. Claude Code fires its `idle_prompt` notification
**60 s after every turn end regardless** — measured to the second on a real session: `Stop` at
11:58:56, `Notification` at 11:59:56, with the agent still live. The old settings template answered
that with a bare `printf needs`, so a row reading *"Waiting for 1 background agent to finish"* in
the pane showed **needs you** in the sidebar, plus a red tab badge, a dock badge and a desktop
banner. Every one of those means "go and answer this", and none of it was true.

The fix is that `Stop` is the only hook that can see the truth, so it records it:

- **`Stop`'s payload carries `background_tasks`** (and `session_crons`). Measured shapes, from
  `scratchpad/agentprobe.py` driving a real `claude` v2.1.234 on a PTY:
  `{"id":…,"type":"subagent","status":"running","description":…,"agent_type":…}` and
  `{"id":…,"type":"shell","status":"running","description":…,"command":…}`; the array is `[]` once
  everything has finished. **Both kinds count** — a background shell is no more "waiting for you"
  than a background agent is.
- **The notification's payload does not.** It carries `notification_type` and `message` and nothing
  else — measured, and the reason there is a flag on disk at all rather than one self-contained
  hook. `Stop` writes/clears `bg/<id>`; `hook::notification` reads it.
- `Stop` writes **`working`** instead of `waiting` while background work is outstanding, and the
  idle notification then leaves it there. `working` rather than a fourth status because everything
  keyed off it is already right: no dock badge, no red tab badge, and `updater.ts`'s busy guard
  keeps counting the instance as busy, so an auto-update cannot restart the app out from under a
  running agent.
- **`permission_prompt` is never suppressed** — a permission request is a question for the user
  whatever else is running — and `AskUserQuestion` never comes through here at all, since it writes
  `needs` from its own `PreToolUse` matcher. A genuinely blocked instance still shouts.
- **`session_crons` is deliberately not counted.** A scheduled future run is not work in flight;
  between firings the instance really is idle and a prompt really is what it wants.
- A task entry with **no** `status` counts as running. The failure that matters is calling a busy
  instance idle, so the unknown case errs toward quiet.

### Compaction is work too

Same shape, different silence. `/compact` **fires no `UserPromptSubmit`** — it is a local command,
not a prompt — so the status file simply keeps whatever the last turn left it, and the 60 s idle
notification then overwrites that with `needs` while the pane is still drawing *"Compacting
conversation… 39%"*. Measured on a real session: `PreCompact` 09:18:10 → `Notification{idle_prompt}`
09:19:10, to the second.

Compaction is also **invisible between its endpoints** — between `PreCompact` and the `SessionStart`
that ends it, nothing fires at all (09:24:19 → 09:24:53 on a real compaction). So both ends are
needed:

- **`PreCompact`** → `working`, and stash its `trigger` in `compacting/<id>`. The idle notification
  suppresses `needs` while that flag is present, exactly as it does for `bg/<id>`.
- **`SessionStart` with `source == "compact"`** → the compaction ended. `SessionStart` also fires for
  `startup`, `resume` and `clear`, and those must **not** touch a status the restore path already
  set, so the source is checked (`is_compaction_end`).
- **The `trigger` decides what the end means.** A manual `/compact` leaves the instance idle at its
  prompt → `waiting`. An **automatic** compaction interrupted a turn that then carries on → `working`,
  because a green "ready" dot in the middle of that turn is the same lie inverted.
- **`PreCompact` fires even when the compaction is then REFUSED** — "Not enough messages to compact",
  measured — and no `SessionStart` follows. So the flag is also cleared by `userpromptsubmit` and
  `stop`: any hook that proves the instance is doing something else. Worst case is one stale status
  word until the next prompt or turn end.

Pinned by `compaction_is_working_and_never_needs_you` (confirmed to fail with *"the 60 s idle
notification landed mid-compaction and claimed the user was needed"*) and
`only_a_compaction_session_start_touches_the_status`, then replayed through the real
`mulpex-helper` as the captured live sequence: Stop → `waiting`, PreCompact → `working`, idle_prompt
mid-compaction → stays `working`, SessionStart[compact] → `waiting`, idle_prompt after → `needs`.

`bg` and `compacting` are subdirs for the same reason `peers/` is: a bare integer at the state-dir
root is scanned as an instance status file (`mcp::live_ids`).

Pinned by `a_turn_that_ends_with_background_work_is_not_idle` and
`an_idle_prompt_is_only_needs_you_when_nothing_is_running`, both confirmed to fail when their half
of the fix is reverted, and both driven end to end through the real `mulpex-helper` binary
afterwards (agent → `working`, idle_prompt → stays `working`, permission_prompt → `needs`,
background shell → `working`, empty array → `waiting` and the flag gone, cron-only → `waiting`).

Banners come from **`tauri-plugin-notification`**, which needs *two* registrations to work — the
plugin in `lib.rs` **and** `notification:default` in `src-tauri/capabilities/default.json`. Miss
the capability and `sendNotification` is simply denied at runtime; the badge (a core window API)
keeps working, so the failure looks like "notifications are flaky", not "notifications are off".
Same allowlist shape as `lib.rs::is_forwarded` for menu ids.

## A session that failed to start is kept, not reaped

The general form of the TCC bug ([packaging.md](packaging.md)): any instance that dies before it
was ever usable used to
disappear silently. `reap_dead` now keeps a dead instance that died within `EARLY_DEATH_GRACE`
(10 s), marks it in `Core.failed` (id → reason), and writes the reason into its own pane via
`Session::notice` — under whatever the child managed to print, so `claude`'s own last words survive
above it. The row reads `⚠ … failed to start` and stays until ⌘W, exactly like an exited terminal.

- **The reason is re-derived at death**, so the TCC case reports the actionable folder message
  rather than a bare exit; otherwise it says only what is known.
- **A failed instance leaves the hub**: its status file is deleted and it is dropped from
  `live_instances()`, so `hub_send` can never offer a corpse as a peer. That also keeps it out of
  `statuses`, which is what excludes it from the dock badge, the tab badges and the updater's busy
  guard **for free** — the same exclusion a terminal gets.
- **`reap_dead`'s early-return had to change too.** It tests *removability*, and a kept-failed
  instance is not removable — so testing that alone returned before anything was marked, and the
  row then sat dead and unexplained until the grace lapsed and it was silently reaped. The original
  bug, delayed by ten seconds. The guard now tests "is there anything to do"
  (`is_removable || needs_failure_mark`), and the mark **latches** so both go false again
  afterwards; without that latch every 200 ms tick redoes the body's two disk writes. Guarded by
  `a_kept_failed_instance_does_not_make_every_poll_do_work`, confirmed to fail (on exactly that
  mtime assertion) with the latch removed.
- The ordinary case is untouched: an instance that ran and then exited is still removed
  (`an_instance_that_dies_after_the_grace_is_still_reaped`).

## An instance number is an identity, not a position

Reported from the field: three instances — **claude#2, claude#3, claude#15** — were closed for an
update and came back as **claude#1, claude#2, claude#3**, with the third row holding the
conversation that used to be claude#3 rather than claude#15. Nothing on screen said the numbers had
moved, so the obvious reading was "claude#3 resumed the wrong session". Two independent defects
compounded, and each is worth keeping written down.

**1. Ids were positional.** The store recorded `<uuid>[\t<name>[\tmuted]]` and nothing else, so
`Core::open` handed out `id = sessions.len() + 1`: every launch renumbered 2/3/15 → 1/2/3. That is
not a cosmetic relabel. The number is what the sidebar shows, what a person says out loud, and what
`hub_send` addresses (`claude#15`, `central-one#3`) — so after a restart every number named a
different conversation than it had the day before. The store now carries the id as a **fourth
positional column**, `<uuid>[\t<name>[\tmuted[\t<id>]]]`, read with `splitn(4, '\t')`, and a restore
reuses it. Gaps are kept (#2, #3, #15) because the gap is the truth; `next_id` continues from
`max + 1`, so a fresh ⌘T cannot collide with a restored instance or land in a hole.

`sessions.len() + 1` was wrong for a second reason that the field report also hit: **it does not
advance when a spawn fails**, so the next record silently took the failed one's number.

**2. A failed restore moved to the end of the list.** `sticky` — the records kept so a restore that
failed doesn't erase the session id (see the next section) — was *appended* by `persist_sessions`.
So one bad launch moved that conversation to the bottom of the sidebar, **permanently**, because the
next launch reads the new order back as the order. Combined with positional ids that reshuffles
which number holds which conversation, which is exactly how the third row ended up holding the
oldest session. `sticky` is now `Vec<(usize, SavedSession)>` — the row it occupied travels with the
record — and `persist_sessions` re-inserts it there, ascending so earlier records don't push later
ones past their slot.

Details worth not rediscovering:

- **Trailing empty columns are dropped on write**, so a store with no ids in it is written
  byte-identically to the old format — upgrading does not rewrite every project's file into
  something an older build would misread. Pinned by `a_store_with_no_ids_is_written_in_the_old_format`.
- **The columns are positional, so only *trailing* empties can go.** An unnamed unmuted instance
  with an id must still write both empty fields — `<uuid>\t\t\t15` — or the number is read back as
  the name. Pinned by `the_instance_number_round_trips_through_every_column_shape`.
- **A store written before this change has no ids and numbers sequentially, exactly as it used to.**
  So the first launch after upgrading still renumbers once; from the save that follows it, the
  numbers are stable.
- A duplicate or zero id (a hand-edited file) falls back to the next free number rather than
  colliding.
- **The frontend needed no change.** It keys everything by `(handle, id)` lookup and never does
  index arithmetic on the id, so non-contiguous ids just render as `claude #2`, `claude #3`,
  `claude #15`. `Core.active` is an index into `sessions`, not an id, and stays that way.

Both halves are pinned by tests confirmed to fail when reverted:
`a_restored_instance_keeps_the_number_the_user_knows_it_by` fails with
`left: [1, 2, 3] right: [2, 3, 15]` — the field report, verbatim — and
`a_failed_restore_is_written_back_where_it_sat` fails with *"a failed restore moved its conversation
to another row"*.

That second test needed one trick, and the reason is a trap. **Killing a restored session does not
reach the `sticky` path at all**: a session that dies within `EARLY_DEATH_GRACE` is deliberately
*kept* on screen rather than reaped, so nothing is ever removed and nothing goes sticky. The first
version of the test passed with the fix reverted for exactly that reason. It now ages the session's
`started` stamp past the grace, which makes `reap_dead` genuinely remove it while `restored` still
puts it inside `RESTORE_GRACE` — the real lost-restore path, driven in a tenth of a second. The
pre-existing `a_failed_restore_is_kept_visible_and_never_erases_the_record` has the same shape and
also never reaches `sticky`; it passes because the record survives in `sessions`, which is a
different guarantee than the one its name suggests.

## A failed restore must not erase the session record

`reap_dead` used to rewrite the store without any session it removed. That is right for a session
the user closed, and **catastrophic** for one that died because its restore failed: `claude
--resume <id>` prints `No conversation found with session ID: …` and exits in about **1.6 s**
(measured), the poll loop reaps it, the store is rewritten without the id — and now there is
nothing left to retry with and nothing to recover by hand. One bad restore turned into permanent
loss of the session.

So `Core` tracks `restored` (id → when it started) and, when a restored session dies inside
`RESTORE_GRACE` (120 s) without having been explicitly closed, keeps its `SavedSession` in
`sticky`, which `persist_sessions` merges back in — **at the row it held**, paired as
`(usize, SavedSession)`, because appending it silently moved that conversation to the bottom of the
sidebar for good (see **An instance number is an identity, not a position**). A restore that fails
once may well succeed next launch; if it never does, the user still has the id. Guarded by
`a_failed_restore_is_kept_visible_and_never_erases_the_record`, confirmed to fail with the `sticky` push
disabled.

**That test does not reach the `sticky` path, despite its name.** A session that dies within
`EARLY_DEATH_GRACE` is deliberately *kept* rather than reaped, so nothing is removed and nothing
goes sticky — the record survives simply because it is still in `sessions`, which is a different
guarantee. To exercise `sticky` at all, age the session's `started` stamp past that grace (what
`a_failed_restore_is_written_back_where_it_sat` does); killing it is not enough.

**The failure mode this protects against, and how to recognise it:** a `claude` that inherits
`CLAUDE_CODE_CHILD_SESSION` runs with **transcript saving off**. It behaves completely normally —
it just writes no transcript — so the breakage only shows up at the *next* launch, as an instance
that appears for a second, prints the red `No conversation found` line, and vanishes. That is why
`pty.rs` strips the marker (and `CLAUDE_CODE_ENTRYPOINT`), and why
`the_child_session_marker_is_stripped_from_spawned_sessions` pins it: if `env_remove` ever
silently stopped reaching the child, *every* session would become unrestorable with nothing in any
log to say why.

Verified along the way, all against the real CLI: `--session-id` is honoured in interactive mode
(not just `-p`); `--resume` appends to the same transcript rather than forking a new id; a 165 MB
transcript resumes fine, through Mulpex's full invocation; and quitting preserves the store.

**Note the tests take `env_guard()`** — `HOME` is process-global and the session store path is
derived from it, so the tests that repoint it must take turns or they race.

