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

### `needs` means a pending question or plan, and nothing else

Red is the strongest thing the sidebar can say, so it is reserved for the two states where the
instance is genuinely holding something up and one keystroke from the user unblocks it:

- **`PreToolUse[AskUserQuestion]`** (`hook askq`)
- **`PreToolUse[ExitPlanMode]`** (`hook plan`) — a plan waiting for approval

Those are the **only** writers of `needs`. In particular the `Notification` hook is not: it used to
write `needs` for `permission_prompt` *and* for `idle_prompt`, and since `idle_prompt` fires 60 s
after **every** turn end, red was the resting state of a perfectly healthy instance. A signal that
is on most of the time is not a signal, which is what "the statuses work bad" meant. It now reports
what it actually knows — `working` while background work or a compaction is outstanding, `waiting`
otherwise — and the two rules that keep that honest are:

- **It never sets `needs`.** `notification_type` is no longer even read; both kinds get the same
  answer and the matcher is what limits which ones arrive.
- **It never clears `needs` either.** The plan-approval dialog fires its own `permission_prompt`
  ~6 s after `PreToolUse[ExitPlanMode]` (measured 2026-09-01), so a notification that wrote
  `waiting` unconditionally would flip the plan's red back to green while the dialog is still on
  screen. A status already reading `needs` is left exactly as it is — not even background work
  outranks it.

What still earns the hook its place is the downgrade: a turn that ended without a `Stop` (an
interrupt) leaves `working` behind, and the idle notification is the only event that then says
otherwise.

Red is cleared by anything that proves the instance moved on — `PostToolUse` (answering the
question or approving the plan) writes `working`, `UserPromptSubmit` the same, `Stop` writes
`waiting`. The one residual case is escaping the dialog and then doing nothing at all: the row
stays red until the next prompt. It is a true statement about a question that was never answered,
so it is left alone.

Everything keyed off `needs` narrows with it for free — the dock badge, the red tab badge and the
desktop banner (`attention.ts`, `stores.ts::needsCount`) all read the same word.

Pinned by `a_notification_never_lights_red_and_never_clears_it`, confirmed to fail with the
`needs`-preserving guard removed (`left: "waiting" right: "needs"` — the plan dialog going green
under itself).

### A turn that ends with background work is not idle

(Written when the idle notification still wrote `needs`, which is why it reads as a red-dot bug.
The `needs` half is now impossible — see above — but the `waiting`-vs-`working` distinction it
established is what the sidebar, the tab badges and the updater's busy guard still run on.)

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
- **`permission_prompt` no longer gets its own answer.** It used to be the one kind never
  suppressed, on the grounds that a permission request is a question whatever else is running —
  but with `--dangerously-skip-permissions` on, essentially the only thing that still fires it is
  the plan-approval dialog itself, ~6 s after `PreToolUse[ExitPlanMode]` (measured 2026-09-01).
  That dialog's red is already written by `hook plan`, so all the notification has to do is not
  undo it, which is the `needs`-preserving guard above. Both notification kinds now take the same
  branch. → [explainer.md](explainer.md)
- **`session_crons` is deliberately not counted.** A scheduled future run is not work in flight;
  between firings the instance really is idle and a prompt really is what it wants.
- A task entry with **no** `status` counts as running. The failure that matters is calling a busy
  instance idle, so the unknown case errs toward quiet.

### ...and a watcher is not work either — Mulpex's own listener broke all of this

The fix above then broke the thing it was protecting, everywhere. `HUB_RULES` tells **every**
instance to arm a Monitor on its inbox (["INCOMING MESSAGES"](hub.md), `pty.rs`), and
that Monitor is a `while true … sleep 1` loop. It arrives in `Stop`'s
`background_tasks` like any other task, so `background_work_running` was **permanently true**:
every instance in every project ended every turn `working`, `bg/<id>` was never cleared, and
because `working` is exactly what suppresses the idle notification, `needs` could never be
written again. No red dot, no tab badge, no dock badge, no banner — for anyone. Reported from the
field as a pane sitting idle at its prompt under *"Baked for 4m 0s · 1 monitor still running"*;
confirmed on the live scratch dir, where `bg/` held a flag for **all 14** instances of one project
and `registry.json` reported every instance in every project as `working`.

The listener is a **watcher**, not work in flight — the same judgement `session_crons` already
gets. **It is recognised by its own command line**, which is Mulpex's text: `HUB_RULES` dictates
it byte-for-byte, so `background_work_running` subtracts any `background_tasks` entry whose
`command` contains `hook::LISTENER_MARKER` (`$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID`). Measured
2026-09-16 on a real `claude` (`scratchpad/monprobe`), the entry is

```json
{"id":"bnxvw92ez","type":"shell","status":"running","description":"Mulpex hub inbox",
 "command":"INBOX=\"$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID\"; … prev=$cur; touch \"$ARMED/…\"; sleep 1; done"}
```

— **the whole command, verbatim, however long it is.** (An older note here recorded the command as
the bare `while true; do sleep 1; done`; that was the probe's own short command, not truncation.)

Consequences worth keeping:

- **The exemption is the command, not the tool.** Any *other* Monitor is ordinary work and still
  counts, exactly as a `run_in_background` shell does.
- **Real work alongside the listener still counts** — the verdict is per task, not per payload.
- **A `subagent` entry has no `command` at all**, so the match cannot mistake one for a listener.
- **It is retroactive.** An instance that armed its listener before this shipped goes green at its
  very next turn end, with no re-arm and no app restart.

#### What this replaced, and why the replacement is the better contract

Until 2026-09-16 the listener was recognised by **task id**, recorded in `monitors/<id>` by a
`PostToolUse` hook that read `tool_input.persistent: true` — the one event that could see the flag.
Then Claude Code **removed persistent Monitors**. Measured on a real `claude`, the flag moved from
`tool_input` to `tool_response` *and became permanently `false`*:

```json
"tool_input":  {"description":"Mulpex hub inbox","timeout_ms":600000,"command":"INBOX=…"}
"tool_response":{"taskId":"bp1jr29w8","timeoutMs":600000,"persistent":false}
```

So `note_persistent_monitor` returned early on every call, `monitors/` stayed empty (confirmed on
the user's live scratch dir, alongside a populated `armed/` and `bg/`), and **every instance was
stuck yellow with no way back short of an app restart** — the exact bug the id-recording was
written to fix, re-created from the other end. Nothing in this repo had changed.

Three things the command match does that the id handshake could not, and they are the reason it is
not merely the current fix:

- **One hook, no ordering.** No `monitors/<id>` file, no dependency on `PostToolUse` running before
  `Stop`, nothing to clear at `SessionStart`, and `posttooluse` no longer parses a payload on every
  single tool call.
- **It is retroactive** (above). The id version could not see a Monitor armed before it shipped, so
  every instance had to re-arm first — which, since the arm nudge rides on genuine user turns,
  meant "until the user happens to talk to it".
- **It depends only on a string Mulpex writes.** The id version depended on an optional parameter
  of someone else's tool. *Someone else's UI is not an interface* — and neither is someone else's
  optional field.

Pinned by `the_hub_listener_is_a_watcher_not_work_in_flight` (confirmed to fail with the
subtraction removed, on *"an instance idle at its prompt with only its hub listener running is NOT
working"*) and by `rules::hub_rules_carry_the_exact_arming_touch`, which asserts the marker and both
`touch`es survive in `HUB_RULES` — confirmed to fail when the heartbeat is dropped. Then driven
through the real `mulpex-helper` binary on the **verbatim captured payload**: Stop with only the
listener → `waiting` and no `bg` flag; idle_prompt after it → `waiting`; Stop with the listener plus
a real background shell → `working` and the flag set; idle_prompt after that → still `working`; Stop
with nothing → `waiting`. `monitors/` is no longer created at all.

### The listener expires, so `armed/<id>` is a heartbeat

Removing persistent Monitors broke a second thing, quieter and worse: **every monitor now expires**
(30 min at most). The hub listener is therefore not permanent, and when it stops, peer mail can no
longer wake an idle instance — the whole point of arming it.

The arm nudge is the only thing that ever gets one re-armed, and it was gated on `armed/<id>`
*existing*. The dead listener's flag sits there forever, so the nudge never came back and the
instance went deaf silently, with nothing anywhere saying so.

So `armed/<id>` is now a **heartbeat, not a flag**: the `HUB_RULES` command `touch`es it when it
starts *and again on every pass of its one-second loop*, and `hook::listener_armed` tests the
file's **mtime** against `LISTENER_HEARTBEAT_GRACE` (30 s) rather than its existence. A listener
that dies goes stale within seconds and the nudge returns by itself. `HUB_RULES` also tells the
instance to re-arm immediately when it is told its monitor expired, which is the fast path; the
heartbeat is the one that does not depend on the model noticing.

- **The grace is 30 s for a one-second heartbeat** — wide enough for a slept machine or a loaded
  box, still far inside the 30-minute expiry it exists to catch.
- **A clock that moved backwards reads as alive.** `elapsed()` errors there, and nudging an
  instance whose listener is probably fine is the worse failure.
- **`HUB_RULES` must not ask for `persistent` any more.** The Monitor tool's schema is
  `additionalProperties: false`, so passing it is now rejected outright — an instruction that would
  have made a literal-minded instance fail to arm at all. Asserted absent by the same rules test.

Pinned by `a_dead_listener_goes_stale_so_the_nudge_comes_back`, confirmed to fail with
`listener_armed` reverted to `.exists()`. Driven end to end afterwards: the `HUB_RULES` command run
as literal shell ticks the mtime every second (1789538468 → 1789538470 over two seconds), and
through the real `mulpex-helper`, a fresh heartbeat emits no arm nudge while a 120-s-old one and a
missing flag both do.

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
mid-compaction → stays `working`, SessionStart[compact] → `waiting`, idle_prompt after → `needs`
(`waiting` since red narrowed to questions and plans).

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

## The scratch dir is rebuilt before every spawn

Reported from the field on v0.8: a Mulpex that had been open since Aug 23 started failing **every**
new ⌘T on Aug 27, in several projects at once, with

```
Error: Settings file not found: /var/folders/…/T/mulpex-86054/2/settings.json
```

while the instances already running kept working perfectly.

`Core::open` wrote `settings.json` and `mcp.json` into `$TMPDIR/mulpex-<pid>/<handle>/` **once**,
and nothing ever touched them again. macOS deletes anything in the per-user temp dir it has not
seen touched in three days — `/System/Library/LaunchDaemons/com.apple.bsd.dirhelper.plist`,
`CLEAN_FILES_OLDER_THAN_DAYS = 3`, run daily at 03:35. Those two were the **only write-once files
in the whole tree**: the status and instance files are rewritten by the 200 ms poll, and every hub
subdirectory is `create_dir_all`ed by the writer that needs it (`hook.rs`, `mcp.rs`). So the purge
took exactly the two files that matter and left everything else looking healthy.

Running instances survived because `--settings` / `--mcp-config` are read **once, at spawn**. That
split is what made it read as "Mulpex broke overnight" rather than "a file is missing": nothing in
the UI changed, and only the next ⌘T failed. Quitting and relaunching fixed it (new pid → new
scratch dir → files rewritten), which is why it looked intermittent.

`write_state_dir` now owns the layout and `Core::ensure_state_dir` calls it before **every** spawn,
claude and terminal alike (`spawn_with`, `spawn_terminal`). It is idempotent — two small writes plus
a few `create_dir_all`s — and re-creates the state dir itself, so it repairs a partially purged tree
and a fully deleted one. Guarded by `a_purged_scratch_dir_is_rebuilt_before_a_spawn`.

Two things worth carrying:

- **A hand-repair puts the wrong path in.** The user's coworker had a `claude` rebuild the files
  from the templates and substituted `__MULPEX_BIN__` with `~/.local/bin/mulpex` — the *deprecated
  TUI* binary, not the app's `mulpex-helper` sidecar. `claude` then started fine, so the fix looked
  complete, while hooks and the MCP hub were pointed at the wrong program (and an unreachable
  helper **fails open silently** — see [packaging.md](packaging.md)). The unconditional rewrite
  corrects that on the next spawn.
- **This is the "write-once file in a scratch dir" shape, not a one-off.** Anything new that gets
  written to `state_dir` once at open and read later has the same three-day fuse. Put it in
  `write_state_dir`.

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

- **A debug build lives in `~/.mulpex-dev`, not `~/.mulpex`** (`mulpex_core::mulpex_home`, used
  by both the session stores and `project.rs`'s recents/open lists; `MULPEX_HOME` overrides
  either way). Added 2026-08-30 so `tauri dev` never reopens — or rewrites the stores of — the
  live app's projects while it runs alongside. Release-script assets (`~/.mulpex/signing`,
  `updater.key`) are not read by the app and stay on the literal path.
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

## Restarting an instance in place (⌘⇧R)

A `claude` reads its world exactly **once, at exec**: `CLAUDE_CODE_OAUTH_TOKEN` and the rest of the
login environment `claude_bin` reconstructed, the skills installed under `~/.claude`, the
`--settings` file. Rotate the token or install a skill and the running instance never hears about
it. ⌘W + ⌘T does pick it all up, but it hands back a *different* instance — new number (so every
`claude#N` written down elsewhere now points somewhere else), no name, empty inbox, and the
conversation to go and find.

`Core::restart_instance` (`commands::restart_session`, menu id `restart`) kills the child and spawns
a new one **on the same row** with the same `session_id` and `resume: true`. Everything that
identifies the instance survives because none of it lives in the process: the id and therefore its
hub address, its name and mute, its position in the sidebar, its `inbox/<id>` mail. It is the restore path from `Core::open`, aimed at one row while the app runs.

Five things it does that are not obvious:

- **It refuses rather than kills when there is nothing to resume.** An instance that has never had a
  prompt submitted has no transcript, so `--resume` would print `No conversation found` and exit in
  ~1.6 s — the row would come back dead, having killed a working claude to get there. `worked` is
  exactly that question (it latches when the first `UserPromptSubmit` hook writes the status file),
  so it is the guard, and the refusal happens before anything is touched.
- **`started`/`restored` are stamped BEFORE the kill.** If the respawn then fails, that is what makes
  `reap_dead` read the corpse as a *failed restore* — row kept with the reason in its own pane,
  `session_id` still written to the store. Stamped after a successful spawn instead, a failed
  respawn is an ordinary exit: the row is removed and the conversation's uuid goes with it.
- **The status file and `armed/<id>` are deleted.** Both are assertions about a process that no
  longer exists. A claude killed while it was waiting on the user would go on telling the sidebar,
  the dock badge and every peer that it `needs` something (removing the file reads as `waiting`,
  `mcp::status_of`'s default, which is the truth about a claude that is booting). `armed/` is worse:
  it tracks a live hub-listener Monitor, so left behind, the hook skips the arm nudge for good and
  the resumed instance is never woken by hub mail again — silently. The inbox, the task line and
  `named/<id>` are deliberately kept: they describe the *instance*, which is the thing being kept.
- **…and `resumed/<id>` is written, which is what lets it re-arm at all.** Clearing `armed/<id>`
  only creates the obligation; something still has to make the resumed child take a turn, and with
  no task on its command line the only thing that does is the "orphaned background task" wake its
  dead Monitor produces. The `UserPromptSubmit` hook **swallows that wake by default** — it is what
  made every instance open itself after an app update — so ⌘⇧R has to mark its own wake as wanted.
  The flag is consumed on read, so the *next* app restart of that instance is ordinary noise again.
  This is the one place the two `--resume` paths need opposite answers to the identical
  notification: an app launch builds a fresh `state_dir` (empty inbox, nothing to act on), while
  ⌘⇧R reuses the same one and keeps the mail. → [hub.md](hub.md)
- **The frontend keeps the xterm and rebinds it** (`terminals.reattach`): `reset()` to wipe the
  half-drawn alt screen the dead child left, then a fresh `Channel` to the new PTY (whose output the
  backend is buffering until something attaches). Rebuilding the terminal instead would put a new
  emulator at whatever size it defaulted to in front of a PTY that spawned at the shared geometry —
  the permanent-debris class in [rendering.md](rendering.md).

Confirmed before it kills anything (a native dialog — Enter restarts, Esc cancels), which ⌘W
deliberately is not: ⌘W closes a session you were looking at and meant to be rid of, while ⌘⇧R sits
one shift away from ⌘R (Rename) and kills a process that may be mid-turn. Reachable from the Session
menu, ⌘P and a row's right-click menu; the context-menu route aims at the row you clicked, not the
focused one. Terminals are excluded everywhere — a shell has no conversation to resume.

Guarded by `restarting_an_instance_with_nothing_to_resume_refuses_without_killing_it` and
`restarting_an_instance_keeps_its_row_and_clears_the_dead_childs_state`. **Not yet driven in the
real app** — the reattach half is unverified in the GUI; see [verification-log.md](verification-log.md).
