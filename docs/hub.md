# The coordination hub

How instances find each other, wake each other, name themselves, message across projects, and spawn
siblings. Split across `crates/mulpex-core/src/{hook,mcp,registry,config}.rs` (what a child process
can do) and `src-tauri/src/{hub,state}.rs` (the 200 ms poll loop that fulfils file handshakes).

Shell terminals are in [shell-terminals.md](shell-terminals.md); remote peers in
[remote-peers.md](remote-peers.md).

Back to [CLAUDE.md](../CLAUDE.md).

## Hub listener — idle wake (A2)

By default a `claude` instance only reads its inbox when it takes a turn, so a message from a
peer sits unread while the instance is idle between the user's prompts. To make an idle instance
react on its own **without host-side stdin polling**, each instance runs the agentalk pattern
against the *local* hub:

- **Watcher:** the instance arms a `Monitor` running **`"<helper>" listen`** (`mulpex-core`'s
  `listen.rs`, reached through `mulpex-helper`), a ~1 s poll of its own inbox dir
  (`$MULPEX_STATE_DIR/inbox/$MULPEX_INSTANCE_ID/`), emitting a `mulpex: N new hub message(s)`
  line only when new message files appear (seeded to the current count, so only post-arm
  arrivals fire). Each such line is a wake event the Claude Code runtime injects as a new turn —
  even while the instance is idle waiting for the user. It was a shell one-liner until 2026-09-16;
  see [the command is a binary now](#the-command-is-a-binary-now-and-the-old-one-is-why) for why
  that had to stop.
- **Arming (hook-driven, no injected prompt):** only the agent can arm its *own* Monitor, and a
  `--resume` restart kills the previous one — but instead of typing a visible bootstrap prompt
  into the PTY (which looked ugly/confusing on every spawn and resume), a normal instance now
  **starts completely clean** and arms its listener from the **`UserPromptSubmit` hook** on the
  user's first turn. The hook (`hook.rs::userpromptsubmit`) injects `ARM_LISTENER_NUDGE` as hidden
  `additionalContext` — a low-key "arm your listener quietly as part of this turn" reminder — but
  only while `listener_armed(ctx)` is false, i.e. while `state_dir/armed/<id>`'s mtime is stale.
  The listener **rewrites that flag on every pass of its loop**, so it tracks a *live* Monitor:
  once armed the reminder stops; if arming was missed, or the Monitor expired, it re-injects next
  turn (self-healing). Because `state_dir` is fresh per Mulpex launch (hence per `--resume`), the
  flag is absent at startup, so restored instances re-arm on their first prompt. The full arming
  procedure + exact Monitor command live in `HUB_RULES` (append-system-prompt, so the wake→act
  contract survives compaction), and the nudge **repeats that command verbatim** rather than
  pointing at it. *(`hub_spawn` children are the one case that still gets an injected
  PTY prompt — their assigned task — via `mulpex-core`'s `rules.rs::spawn_prompt`; they arm the
  listener from the same hook on that first turn.)*
- **On wake (auto-act):** the instance calls `mcp__mulpex__hub_inbox`, acts on the message(s)
  autonomously, replies to the sender only when it adds value (no bare acks), and prefixes the
  self-triggered turn with a `⟳ hub message from <sender> →` marker so the human can tell it
  wasn't their prompt. This coexists with the `userpromptsubmit` hook's unread-count nudge, which
  still covers the "notice on your next prompt" path.

### The listener is not permanent, and `armed/<id>` is a heartbeat

Claude Code removed persistent Monitors (measured 2026-09-16), so **every hub listener expires** —
30 minutes at most. A listener that has stopped cannot wake an idle instance, which is the only
reason it exists.

The arm nudge is what gets one re-armed, and it reads `armed/<id>`. That file is now written by the
listener *on every pass of its one-second loop*, not once at startup, and `hook::listener_armed`
tests its **mtime** (grace: 30 s) instead of its existence — so a dead listener goes stale within
seconds and the nudge comes back on its own. `HUB_RULES` separately tells the instance to re-arm the
moment it is told its monitor expired; that is the fast path, the heartbeat is the one that does not
depend on the model noticing.

`HUB_RULES` must also no longer ask for `persistent: true`: the Monitor schema is
`additionalProperties: false`, so passing it is rejected and a literal-minded instance would fail to
arm at all. The full story, including what the same change did to the sidebar's yellow dot, is in
[sessions.md](sessions.md#the-listener-expires-so-armedid-is-a-heartbeat).

### A re-arm is not news: `quietturn/<id>`

The expiry is not ours to fix. What *was* ours is what it cost to watch.

Read off the live tool schema, 2026-09-19: `timeout_ms` declares `maximum: 3600000` and the
description says *"Deadlines above 1800000ms are capped to 1800000ms"* — ask for the advertised
maximum and the tool answers `expires in 30m`. There is no parameter that opts out; `persistent` is
gone from the schema entirely. So every instance is woken twice an hour, forever, by an event it can
only answer by doing the same thing again.

Each of those wakes used to cost two visible things, neither of them Anthropic's doing:

- the sidebar dot flipped to `working` and back, because `userpromptsubmit` writes `working` before
  it knows what kind of turn this is;
- `Stop` handed the turn to the Explainer, which spent a Sonnet call writing a Hebrew paragraph
  explaining that a watchdog had been restarted.

At five instances that is ~480 model calls a day, none of which say anything. `quietturn/<id>` is
the fix, and its whole design is about *earning* the right to hide a turn:

| hook | what it does |
| --- | --- |
| `userpromptsubmit` | on a `<task-notification>`, restore the status it just overwrote and mark the turn a candidate |
| `posttooluse` | `is_listener_rearm` → return, touching nothing. Anything else → clear the mark, write `working`, carry on |
| `write_needs` (`askq`/`plan`) | clear the mark — an **escaped** dialog fires no `PostToolUse`, so this is the only place that catches it |
| `stop` | mark still there → skip `write_explain_request`. Either way, clear it |

**Surviving to `Stop` is evidence, not a guess** — the mark is cleared by the *first* call that is
not the re-arm, so a wake that reads its inbox and acts on mail is explained exactly as before. And
`is_listener_rearm` matches on the **command** via `command_is_hub_listener`, not on the tool name:
a `Monitor` watching a deploy log is real work and stays visible.

Both defaults point the same way, deliberately. An unparseable payload reads as *not* the re-arm,
and a turn that is not clearly a system turn is never a candidate. Being wrong in that direction
costs one explanation nobody needed; being wrong in the other hides a turn that did something, and
the symptom — an explanation that never appears — is indistinguishable from a quiet instance.

### The command is a binary now, and the old one is why

Adding that in-loop `touch` is what exposed the real problem: **the listener command was
transcribed by the model, and a model copies whichever version is nearest in its context — which is
its own previous `Monitor` call, not the system prompt.**

Measured on live instances, 2026-09-16, from their own transcripts:

- `warweb#65` armed the listener **72 times** across two days. Arms #1–#71 all carried the
  pre-in-loop-`touch` command. It went straight through the app update that changed the text, and
  only picked up the current version at arm #72 — immediately after a `/compact` dropped the old
  call from its context. Its sibling `warweb#74` never recovered at all.
- `cloudraw#3` is the control: it compacted earlier in the day and has armed the current command
  ever since.

The consequence is a feedback loop, not just a stale string. No in-loop `touch` ⇒ `armed/<id>` never
refreshes ⇒ `listener_armed` is false every turn ⇒ the arm nudge fires every turn ⇒ **another
Monitor stacks on each one**. `warweb#65` was seen running three at once; one hub message woke it
three times, which is how the whole thing was noticed.

So the command stopped being prose to retype and became **`"<helper>" listen`** — one line naming a
binary:

- The loop's behaviour ships with the app. Changing it never asks a model to retype anything, and a
  session running last week's binary gets this week's logic at its next re-arm.
- The helper lives inside the `.app`, so the **three-day `$TMPDIR` fuse** cannot reach it. A script
  in the scratch dir would have been unrunnable for an instance open over a long weekend — at
  exactly the moment it needed to re-arm.
- `hook::command_is_hub_listener` is one matcher used by everything that has to recognise a
  listener (the `Stop` hook's working/idle count, and the orphan reaper). Two spellings is how one
  of them silently stops recognising the other.

**Recognising the old form is permanent, not transitional.** A session that was already running
keeps re-emitting the shell one-liner out of its history for as long as it lives, so
`command_is_hub_listener` matches both forms. For those sessions there is a repair path: only the
`Stop` payload carries `background_tasks` (see below), so `Stop` writes `relisten/<id>` when it sees
a listener that is running yet not refreshing the heartbeat — or more than one — and the next arm
nudge names those task ids and tells the instance to `TaskStop` them and arm one current listener.

**`background_tasks` is a `Stop`-only field.** Measured against a real `claude` v2.1.273 with
dumping hooks: it is present on `Stop`, and absent from `UserPromptSubmit` (even on a prompt taken
24 s after a Monitor was armed), from `PostToolUse`, and from the idle notification. That is the
whole reason the repair is a two-hook file handshake instead of a check inside `listener_armed`.

### A listener outlives everything that is supposed to kill it

`Session::kill` `killpg`s the child's process group (`pty.rs`) and sweeps its controlling terminal
(`kill_tty_session`). **A hub listener escapes both.** Claude Code runs each background command in
its own process group with no controlling terminal — measured on live listeners: `PGID == pid`,
`SESS 0`, state `Ss` (no `+`). So it survives ⌘W, a crash and app teardown alike, and launchd
adopts it still spinning `sleep 1`. Six were found alive on one machine at once, the oldest from
the previous morning, belonging to a Mulpex that had already exited. `mulpex-cli/src/sweep.rs`
named this hole; nothing acted on it.

Two halves close it, and both are needed:

- **`listen.rs` exits by itself** when `$MULPEX_STATE_DIR` disappears (app quit removes the whole
  scratch root; project close removes the project's) or when the `claude` in `pids/<id>` — written
  by `pty.rs` right after spawn — is gone. It also takes a `listeners/<id>` pid lock, so a second
  listener for the same instance stands down instead of doubling every wake-up.
- **`pty::reap_orphaned_listeners`** SIGKILLs the ones that predate that, at launch (next to
  `sweep_stale_state_roots`) and at teardown. It keys on **`ppid == 1` plus the listener mark**,
  *not* on a dead scratch root: a legacy listener's argv spells its state dir as the literal
  `$MULPEX_STATE_DIR`, and `KERN_PROCARGS2` does not return the environment that would expand it
  (measured — a real orphan's blob came back 756 bytes, argv only, no `MULPEX_*` anywhere). A
  listener serving a live instance is a child of that `claude` and is therefore never matched.

One more trap found while writing that: asking `KERN_PROCARGS2` for its buffer size with a null
`oldp` answers **32 bytes** — enough for `sleep 60` and nothing else. The buffer has to be
`KERN_ARGMAX`, or every command line comes back truncated and the sweep matches nothing while
looking like it works.

### The nudge that fed itself: why instances opened by themselves after an update

The arming nudge is self-healing by design, and for a while it healed a wound it was itself
inflicting. Every Mulpex update made idle instances — including ones in *other* projects the user
wasn't looking at — wake up and take a turn nobody asked for.

The loop, traced end-to-end in a live transcript (`bvpgm5lxl` → `bjwjmo0kw`, 2026-09-03) rather
than reasoned about:

1. Teardown `killpg`s each `claude`, so the listener's **Monitor dies with no completion record**.
   (The `claude` dies; the listener *process* does not — see
   [A listener outlives everything that is supposed to kill it](#a-listener-outlives-everything-that-is-supposed-to-kill-it).
   From Claude Code's point of view the task is gone either way, which is what produces the wake.)
2. On the next launch the resumed session is handed a synthetic
   `<task-notification><status>stopped</status>… No completion record was found …` prompt. **That
   is a real turn** — not a notice. It is what "the session opened by itself" actually was.
3. `UserPromptSubmit` fires on it (measured: the injected turn carries `origin.kind:
   "task-notification"`, `promptSource: "system"`, and a `hook_additional_context` attachment), so
   `ARM_LISTENER_NUDGE` rides in.
4. The instance dutifully arms a fresh Monitor — **the orphan that repeats this at the
   next update.**

The nudge manufactured the artifact that caused the next nudge, so it never settled. Note where the
miss was: `userpromptsubmit` *already* recognised `<task-notification>` (to keep event turns from
overwriting the sidebar task); the nudges below it just weren't gated on the same fact. A
discriminator that exists but is only half-applied is the cheapest kind of bug to keep.

Two halves fix it, and both matter:

- **The nudges are asked of the user's turns only** (`nudges_welcome`). The **peer snapshot is
  deliberately not gated** — a hub wake *is* a `<task-notification>`, and it is the very turn that
  most needs the unread-mail count. Verified: mail planted in `inbox/<id>/` still arrives on a
  Monitor-event wake with the arm nudge suppressed.
- **The orphan wake is blocked outright** (`orphaned_task_wake` → `{"decision":"block"}`), so no
  turn happens at all. Measured on a real PTY session: `num_turns: 0`, zero output tokens, no
  assistant row, `preventContinuation: true`.

The match is narrow on purpose: `stopped` **and** the "No completion record was found" wording.
That shape is *provably* stale — it describes a task belonging to a process this app already killed
— so it can never be actionable. A completion, a failure, a Monitor the user stopped, and a hub
wake all pass through, and `the_restart_wake_is_swallowed_but_every_other_notification_is_not`
asserts each one still does. Blocking is also why the hook now **restores the prior status**: a
blocked turn never runs, so no `Stop` hook follows to clear the `working` it just wrote.

**⌘⇧R is the exception, and inverting it would have been a silent regression.** Restart-in-place
reuses the *same* `state_dir`, deliberately keeps the inbox and clears `armed/<id>`
(`Core::restart_instance`) — so there, that same wake is the **only** path back to an armed
listener and a drained inbox. It is exempted by a one-shot `resumed/<id>` flag
(`resumed_in_place_path`), consumed on read so the *next* app restart is ordinary noise again. The
exemption must also re-open the nudge gate, not just let the turn through: gating on `system_turn`
alone bought a turn that still never re-armed — the exact failure the exemption exists to prevent,
and invisible except by reading the hook's stdout. That is what `nudges_welcome` is a named
function for.

**What is deliberately not fixed.** Claude Code renders a blocked prompt as a warning block that
also dumps the original prompt; `suppressOutput` and an empty `reason` were both measured not to
remove it (an empty reason just becomes "Blocked by hook"), so `ORPHAN_WAKE_BLOCK_REASON` is
written to explain the notice the user will see anyway. The only way to remove it entirely would be
to forge a completion record in Claude Code's private task dir — an undeclared interface, which is
the dependency this repo has already been burned by twice.

Also settled while diagnosing this, against two plausible-sounding suggestions:

- **`sweep_stale_state_roots` already works.** Of 59 `mulpex-*` dirs in `$TMPDIR`, 58 were our own
  **test fixtures** (`mulpex-oldfmt-*`, `mulpex-argvspawn-*`, `mulpex-vtgrid-test-*`…), which it
  skips by design because they don't parse as a pid; the single real dir was the *live* app's.
- **Making `armed/<id>` survive restarts would be a bug, not a fix.** The Monitor genuinely is dead
  after a restart. A surviving flag means the instance believes it is armed when it isn't, and it
  is never woken by hub mail again — with nothing anywhere to say so.

## Naming a row (`hub_set_name`), and the two backstops behind it

An instance labels its own sidebar row with `hub_set_name`, which is a **fire-and-forget file
handshake** (`mcp.rs` writes `namereq/<id>`, the poll loop's `Core::process_name_requests` turns it
into a real, persisted rename — exactly as if the user had pressed ⌘R). It is asked for by the
`UserPromptSubmit` hook's `AUTO_NAME_NUDGE`, the same self-healing shape as the listener-arming
nudge: injected as hidden `additionalContext` while `named/<id>` is absent, and the flag is written
by **`hub_set_name` itself** rather than when the rename lands, so a *refused* request (the user
already named the row) also stops the asking. ⌘R sets `manual_names`, and the user's label always
wins — `name_verdict` is the one place that rule lives.

**A nudge is a request, and a model can drop it.** Measured on a live instance, 2026-08-10:
claude#6 opened its turn with *"I'll start by arming the hub listener and naming this instance"*,
armed the Monitor (`armed/6` written — proof the nudge landed), then worked for three minutes and
never called `hub_set_name`. `named/6` was absent and `namereq/` empty afterwards, so nothing was
refused and nothing was lost in transit; the instruction was simply displaced by the actual task.
The row meanwhile showed the user's raw prompt — a wall of pasted JSX — and **nothing would ask
again until the user's next prompt**, which for a long turn is a long time. Diagnosis came from the
live scratch dir (`armed/` present, `named/` absent, `namereq/` empty), which distinguishes
"never called" from "called and refused" in one listing; the tool itself was confirmed present in
the *installed* helper's `tools/list` before blaming the model.

So naming now has the same two-layer treatment hub mail has:

- **A mid-turn nudge** (`hook.rs::name_nudge_due`, fired from `posttooluse` alongside the mail and
  departed-peer notes). At `NAME_NUDGE_AFTER_TOOLS` (3) tool calls into a turn, an unnamed instance
  is reminded again — late enough that it knows what the session is about, early enough that the
  row is labelled while the work is still running. Deduped per **turn** via a `namenudge/<id>`
  count that `userpromptsubmit` clears, so it arrives once, not on every tool call. The counter is
  a subdir for the same reason `peers/` is: bare-integer filenames at the state-dir root are
  scanned as instance status files (`mcp::live_ids` falls back to exactly that).
- **A provisional label** (`state.rs::apply_fallback_names`, called from the poll loop). An
  instance that reaches a turn boundary (`waiting`/`needs`) still unnamed gets `name_from_task` of
  its own captured task — the same label a `hub_spawn` child is auto-named with. Only at a turn
  boundary (mid-turn the task is whatever was typed first), only into an empty label, and only
  once.

**The provisional label lives in its own map, `Core.fallback_names`, and that is the load-bearing
part.** Putting it in `names` would look identical on screen and be wrong twice over: `names` is
what `persist_sessions` writes, and a name coming back from the store is treated as the **user's**
(`manual_names` is seeded from the restored names on `Core::open`, since a persisted name can't be
told from a hand-typed one) — so a machine-made guess would harden into a user label on the next
launch and `name_verdict` would then refuse the instance's own `hub_set_name` **forever**. It also
must not count as `current` for `name_verdict`. So it is a display-only overlay: `display_name`
prefers the real name, `process_name_requests` and `rename` drop the guess, and `named/<id>` is
*not* written — the instance keeps being nudged and can still replace it. Pinned by
`an_unnamed_instance_gets_a_provisional_name_that_never_persists`, whose store assertion goes
through a real `persist_sessions()` (asserting only "we didn't persist right now" passes even with
the bug) and was confirmed to fail when the label is inserted into `names`.

`.name` in `InstanceList.svelte` is line-clamped to 2 like `.task`, because a name is no longer
always 2-5 words.

## Cross-project messaging (`<project>#<n>`)

`hub_send` / `hub_inbox` / `hub_instances` reach **every project open in this Mulpex window**.
Everything else stays strictly per-project: locks, `hub_spawn`, terminals, mute, the badges.

Before this, isolation was not a check — it was **unreachability**. An instance is handed exactly
one `MULPEX_STATE_DIR` (`pty.rs`), every hub path is `state_dir.join(…)`, so another project's hub
had no name a child process could utter. The feature is therefore mostly about *naming*.

- **The registry** (`mulpex-core/src/registry.rs`, published at the state ROOT as
  `temp/mulpex-<pid>/registry.json`) is the one file that says what the other projects are, where
  each one's state dir is, and who is live in it. The **poll loop writes it** (`hub.rs`) because it
  is the only context holding every `Core` under one lock; `Core::registry_entry` builds an entry
  from the snapshot it already computed, so it costs no extra disk reads and inherits `statuses`'
  membership rule — shells and failed-to-start instances are absent, exactly as they are locally.
  Written only when the bytes change, temp+rename.
  - The root is per **process** (`mulpex-<pid>`), so "reachable" means precisely "open in this
    Mulpex window" — a second Mulpex is a separate universe, with no code to make it so.
  - A child knows its own `MULPEX_STATE_DIR = <root>/<handle>`, so it finds the registry with
    `parent()`. **No new env var was needed**, which is why nothing in `pty.rs` changed.
  - Staleness is ≤200 ms — the same guarantee the per-project `instances` file already gives, so
    validating a foreign recipient is no more of a race than validating a local one.
- **Sending is a direct write into the target's own `inbox/<id>/`** (`mcp.rs::send_foreign`), not a
  brokered handshake like `hub_spawn`'s. That is the whole reason the feature is small: **nothing
  downstream changed.** Their 1 Hz listener Monitor wakes them, the `PostToolUse` nudge and the
  blocking `Stop` hook count it, `hub_snapshot`'s `pending` counts it, their amber tab badge lights
  — every one of those paths already existed, because the message lands where a local one would.
  The `<token>.done` handshake was rejected on latency: `hub_send` must feel instant.
- **Provenance rides on extra keys**, `from_project` / `from_project_dir`, the same way the
  remote-claude wake already rides on `from_terminal`. `take_inbox` reads only `ts`/`body` plus
  whatever `sender_label` decides, so the reader needed no change.
- **`sender_label` returns an address you can reply to** — `claude#2`, `cloud#2`,
  `term#4 (remote claude)`. The reply address is never assembled or guessed.
- **Both sides' `messages.log` get the line.** Logging only where `hub_send` ran would leave the
  recipient's unread badge climbing with nothing in its reader to explain it. `MsgEntry.from` is
  therefore a `String` address, not a `usize` — there was nowhere to put the project. The log lives
  in a per-pid scratch dir, so the format change costs no compatibility.

### The `#` collision — the trap, and why the parser is ordered

`claude#3` separates *kind* from *number*; `central-one#3` separates *project* from *number*. One
character, two jobs. That is a trap rather than a wart: `claude#3` is how an instance is written
everywhere else, so a model will eventually put it in `to` meaning the local instance 3. So
`registry::parse_address` resolves the kind words **before** any project lookup — `claude#<n>` is
the local instance, `term#<n>` is refused naming `hub_terminal_send` (promoting a rule `HUB_RULES`
only stated into one that is enforced), everything else is a project qualifier. Cost, accepted:
a project literally named `claude` or `term` needs a path qualifier (`dreamvps/claude#3`); the
errors say so.

Resolution is exact-dir, then **whole trailing path components** (`cloud`, `dreamvps/cloud`) — a
substring match would let `one` hit `central-one`. An ambiguous name is an error listing the
candidates' full paths **plus a suffix that actually disambiguates**; sending to the wrong
repository is a wrong answer, not an inconvenience.

### Two things that had to change, both quiet failures

- **`bounce_dead_inbox` had to learn `from_project_dir`.** A foreign message's bare `from` is an id
  in *another* project's numbering, so bouncing on the number alone hands a stranger's undelivered
  mail to whichever local instance shares it — mis-delivery, not a dropped message. It now resolves
  the sender through the registry and bounces back across the boundary; a sender whose project has
  closed is dropped rather than guessed at. Pinned by
  `a_foreign_bounce_goes_back_across_the_boundary_not_to_the_local_same_number`, which is
  non-tautological by construction (project B has a *live* `claude#2`, exactly where the old code
  put it) and was confirmed to fail with that message.
- **"Which project am I" cannot be a string compare** (`registry::same_dir`). The app writes the dir
  it opened; the helper asks with the canonicalized one, and on macOS `/var` vs `/private/var` is
  enough to disagree — a symlinked project path disagrees everywhere. The symptom is an instance
  seeing its **own** project among the "other" ones. **Found by driving two real helper processes,
  not by a unit test**, whose hand-matched strings agreed by construction.

### What deliberately did NOT change

`to: "all"` is still project-local — a message is mandatory reading, so one project must not be
able to stall another. Cross-project mail raises the recipient project's **amber tab badge only**:
no dock badge, no notification, keeping the documented rule that the dock badge means "a claude is
blocked on YOU". `hub_spawn` still only creates instances in its own project, terminals are still
un-driveable across the boundary, and locks are meaningless between separate checkouts. `HUB_RULES`
says all of this, and says the thing that matters most for correctness: an instance over there is a
**different repository and working tree**, so anything sent to it must be self-contained.

## Spawning instances (`hub_spawn`)

An instance can create new task-seeded siblings — e.g. fetch a list of tickets and spawn one
instance per ticket. The MCP helper runs in a **separate process** from Tauri and can't create
sessions itself, so `hub_spawn` (`mcp.rs`) is a **file handshake** through the poll loop:

- **Request:** `hub_spawn({tasks: [...]})` writes `state_dir/spawn/<token>.json`
  (`{from, tasks, ts}`), capped at `MAX_SPAWN_PER_CALL` (8) so a 50-item list can't fork 50
  `claude`s at once, then **polls** for `<token>.done` (~6 s) to return the assigned ids. That
  window and `SPAWN_STAGGER` below are coupled: a full 8-task batch spends ~3.5 s in stagger
  alone, so raising the stagger (or the cap) without raising the poll window turns a big batch
  into the "spawn requested, call `hub_instances` in a moment" fallback reply — correct, but the
  ids no longer come back in-line.
- **Fulfilment:** the 200 ms poll loop calls `Core::process_spawn_requests()` (`state.rs`), which
  consumes the request file into a `pending_spawns` queue and then **drip-feeds** it — at most one
  child per tick, and no closer together than `SPAWN_STAGGER` (500 ms) — spawning each via
  `spawn_instance_with_task(parent_id, task)`. `<token>.done` is written only once the batch's
  last child is up, so the caller still gets every id in one reply. If anything spawned it emits
  `sessions-changed` so the frontend builds the new xterms (the existing reap path already
  republishes on removal; added sessions ride the same event via `TerminalPane`'s keyed
  `{#each}`). The drip-feed **never sleeps** — this runs on the shared poll loop, so blocking
  would stall every project's UI. Staggering exists because N simultaneous `claude` cold starts
  contend hard enough that a cold start can take tens of seconds (which used to blow the
  injection deadline; it now only delays the first turn).
- **Seeding + link:** the child's first prompt (`rules.rs::spawn_prompt` in `mulpex-core`, passed as argv) is just the task:
  start it, then `hub_send` a summary back to the spawner when done (listener arming is *not* in
  this prompt — it comes from the `UserPromptSubmit` hook like every instance). Still
  `[mulpex:hub]`-sentinel-prefixed (skips the sidebar task-capture) and a single line (task
  whitespace collapsed). The child is **auto-named** `name_from_task(task)` so the sidebar labels
  it, and is **not** focused — the user stays on their pane while children appear. Recursion is
  inherent (children also have `hub_spawn`); only the per-call cap bounds a single call.
- **The task goes on the child's COMMAND LINE, and must never be typed into its TUI again**
  (`pty.rs`). `claude` takes an initial prompt as a positional argv argument, so `spawn_prompt`'s
  text is handed over at `exec` time: no readiness detection, no retries, no submit key, and
  nothing that can race the TUI. Verified end-to-end against a real `claude` v2.1.252 — 3,000 and
  12,000-character prompts arrive **byte-exact**, argv coexists with `--session-id`/`--settings`/
  `--mcp-config`, the `UserPromptSubmit` hook fires with the whole text, and the child can call
  `mcp__mulpex__hub_instances` **on its first turn** (so MCP servers are up before the prompt runs
  — measured against the real `mulpex-helper mcp`, not assumed).

  **Why the typing path is gone.** It silently truncated every task over ~1 KB. Driving a real
  `claude` v2.1.252 on a PTY exactly the way `pty.rs` did — one `write_all` of the whole prompt,
  400 ms, then a separate `\r` — and reading ground truth from the child's own transcript
  `.jsonl` rather than off the screen:

  | sent | received |
  | --- | --- |
  | 1200 | 1022 |
  | 1998 | 1022 |
  | 3000 | 1022 |
  | 6000 | 1022 |

  1022 is the macOS tty input-queue size (1024) minus two. `claude` collapses a fast burst into a
  `[Pasted text #1]` placeholder and submits only the first chunk it read; the rest is dropped,
  cutting mid-word. **Neither of the two obvious suspects was at fault**, and both were ruled out
  by measurement before the TUI was: nothing in Mulpex caps the string (the chain
  `hub_spawn` → `spawn/<uuid>.json` → poll loop → spawn spec was traced and grepped), and the
  kernel loses nothing either (600–12,000-byte writes into a raw-mode PTY whose slave reads late
  all arrive in full — the master write simply blocks).

  **This contradicts the note below**, which recorded 2 k / 8 k / 9 k-char tasks landing intact on
  claude **v2.1.235**, and a 10,213-character prompt reaching a child's transcript whole. Both
  measurements stand; the difference is the `claude` version. Treat that as the lesson rather than
  as a discrepancy to resolve: **the TUI is someone else's UI, and delivering data through it is a
  contract that can be withdrawn by an upgrade you did not make.** argv is an interface; typing
  into a text box is not.

  The old machinery is deleted, not disabled: `input_box_ready` and its two chrome styles,
  `RULE_RUN`, the rolling `TAIL_CAP` output tail, `READY_FALLBACK`, the 90 s `READY_TIMEOUT`,
  `INJECT_ATTEMPTS`, `VERIFY_WINDOW` and the Ctrl-U retry loop. What survives is one **delivery
  watchdog** thread whose only job is the case the hook cannot cover: a child that never reaches a
  first turn at all. Everything else about delivery is now answered by the child itself.

### A slow spawn must not look like a lost one

Reported from the field: two consecutive `hub_spawn` calls each returned `ok: true` with a new id,
the instance appeared, and it sat there doing nothing. The spawner checked `hub_instances`, saw
`{"status":"waiting","task":""}`, concluded the seeding had silently dropped its 9,000-character
assignment, and re-sent the whole thing by hand with `hub_send`.

*(Historical — this is the v2.1.235 investigation that produced the readiness machinery. That
machinery is gone; the reasoning about what a spawner can and cannot read is why the delivery
verdicts above exist, and still applies.)*

**Nothing was ever dropped.** The child's own transcript has the injected prompt arriving intact,
all 10,213 characters of it, **91 s after the spawn** — i.e. at the `READY_TIMEOUT` ceiling, the
readiness bug above. The spawner's manual `hub_send` went out 19 s *before* the real task landed.
Length and content were ruled out by measurement first, driving a real `claude` on a PTY: 50 /
2 k / 8 k / 9 k-char tasks and an 8.5 k Hebrew one (15.5 KB of UTF-8, with backticks, `===`
headings and `--flags`) **all land on attempt 1**. The PTY itself only starts discarding above
~25 KB, and `write_all` blocks rather than truncating below that.

What made a slow spawn *unfalsifiable* is that every signal a spawner can read says exactly what a
lost task would say:

- **`status: waiting`** is `mcp::status_of`'s **default for a missing status file** — and a child
  that has not taken a turn yet has no status file. It is not reporting idleness; it is reporting
  ignorance, in the same word.
- **`task: ""`** is what a spawn child shows for its *whole life*, not just at startup: the
  prompt carries the `[mulpex:hub]` sentinel precisely so `hook::userpromptsubmit` skips capturing
  it (that same sentinel is now what routes the prompt into the delivery check). Every other row in the listing has a task, so the empty one reads as broken.
- **`ok: true`** was answered as soon as the child *process* existed. Creating a process is not
  delivering a task, and the two were minutes apart.

So the spawn path now publishes what it knows, and the API stops asserting what it doesn't:

- **`spawn_instance_with_task` seeds `tasks/<id>` with the assignment before the child exists**, so
  `hub_instances` shows what it was sent instead of `""`. (Removed again if the spawn itself
  fails, so a number handed to somebody else can't inherit it.)
- **Delivery publishes a verdict to `spawning/<id>`** (`mulpex_core::spawn_delivery_path`):
  `pending` written synchronously by the spawn path, then one of three outcomes. Returning
  quietly was the original defect — same shape as every other silent failure in these notes
  (**How this codebase fails** in [../CLAUDE.md](../CLAUDE.md)). A subdir for the usual reason: a
  bare integer at the state-dir root is scanned as a status file (`mcp::live_ids`).
- **The child rules on its own delivery, because it is the only one who can.** Mulpex knows what
  it sent; `claude` knows what it got; **only the `UserPromptSubmit` hook sees both**. So the
  spawn path writes the exact prompt to `spawning/<id>.expected`
  (`mulpex_core::spawn_expected_path`) and `hook::verify_spawn_delivery` compares it with the
  prompt that actually arrived: equal clears the verdict (delivered, *and verified so*), unequal
  writes **`partial`**. The watchdog in `pty.rs` never overrules a verdict the hook reached — it
  only writes `failed` for a child that never began a turn, or died before one.

  **`partial` is the state this whole mechanism exists for.** `failed` is loud by nature: the
  instance sits there doing nothing. A mangled brief is the opposite — the instance is *working*,
  looks healthy, and produces confident output about the wrong task. Under the typing path that
  case was completely invisible: a turn had genuinely started, so every signal the app could read
  said success, and the spawner was told `ok: true` while its child worked from the first
  kilobyte of a 6 KB brief. Delivery is argv now and cannot truncate; the check stays anyway,
  because it is what keeps the *next* delivery mechanism honest.
- **`hub_instances` reports `task_delivery`** (`pending`/`failed`/`partial`, absent once
  delivered and verified) with a note saying what to do about it — wait, `hub_send` it yourself,
  or (for `partial`) interrupt the instance, re-send, and report the bug. An instance nobody
  spawned carries no delivery claim at all.
- **`hub_spawn` waits for delivery** (`await_delivery`, capped at `SPAWN_DELIVERY_WAIT` = 60 s) and
  **`ok` tracks the task, not the process**: `tasks_delivered` / `tasks_not_delivered_yet` /
  `tasks_never_delivered` / `tasks_delivered_mangled`, with `ok` true only when every task landed
  intact. `partial` is a **terminal** verdict, deliberately not folded into `pending` — rolling
  the worst outcome into the most benign one is the exact error this section is about. An
  unresolved wait is still reported as `pending` rather than rounded up to success; the
  no-response fallback reply is `ok: false` too. The tool description says all of this, including
  that a brand-new instance showing `status: waiting` is *normal* while delivery is pending.

Pinned by `hub_instances_says_whether_a_spawned_task_actually_arrived` (three instances, none with
a status file, so they are indistinguishable without the marker),
`hub_spawn_never_claims_ok_for_a_task_that_did_not_arrive` (all four verdicts, including a
`partial` that must not read as pending), `hook::a_mangled_spawn_task_is_caught_by_the_child_itself`
(the 1022-character cut, checked by the real comparison the child runs) and
`pty::a_long_task_reaches_the_child_whole`.

The one that actually guards the mechanism is
`state::a_spawned_child_receives_its_whole_task_on_its_command_line`: it runs the real production
path (`spawn_instance_with_task` → `Session::spawn` → `CommandBuilder` → `exec`) against a stub
`claude` on `PATH` that records its argv, and asserts a 6,000-character task arrives whole. A unit
test on the prompt builder cannot catch a return to typing — only reading the child's real argv
can. It is `#[ignore]`d because it must own the process (`claude_bin`'s `merged_path` and
`resolve_claude` are `OnceLock`s, so the stub has to be resolved before any other test resolves the
real binary); run it alone:

```
cargo test --lib -- --ignored --exact \
  state::tests::a_spawned_child_receives_its_whole_task_on_its_command_line
```

Confirmed non-vacuous: with the `cmd.arg(prompt)` line removed it fails, reporting the
append-system-prompt as the last argument instead of the task.


## Closing instances (`hub_close`)

The inverse of `hub_spawn`, and it exists for the same workflow: an orchestrator that fans six
workers out could create them but not clean them up, so a finished fan-out left six idle rows for
the user to close by hand. It rides the **same request channel** `hub_terminal_close` uses —
`state_dir/termreq/<token>.json` → poll loop → `<token>.done` — because the helper is a separate
process and cannot touch a PTY. (`mcp::terminal_request` is now `app_request` for that reason; the
directory keeps its old name, which is a wire format three processes agree on rather than a
description of what rides it. The `mpx` daemon reads the same dir and applies the same op in
`mulpex-cli/src/terminals.rs`.)

**Two halves of the decision, on two sides, on purpose.**

- **The helper decides caller errors**, and any one of them aborts the whole call with nothing
  queued: a malformed address, `"all"`, a `<project>#<n>` from another project, and closing
  yourself. `close_targets` is split out and pure precisely so those can be tested without a
  Mulpex on the other end — a request file written on the way to a refusal would be applied later
  by a poll loop that never sees the reasoning. `"40"` and `"claude#40"` dedup to one id, or the
  second attempt would come back as "no claude#40 is open" — a refusal reported for a close that
  worked.
- **The app decides everything about live state**, per id, and reports it: not open, is a terminal
  (pointed at `hub_terminal_close`), or busy. A partly-refused batch is still `ok: true` at the
  request layer — the refusals are a *report*. Only `hub_close` itself turns an **all**-refused
  batch into a tool error, where nothing the caller asked for happened. Five workers must still
  close when the sixth is mid-turn.

**Self-close is refused, not deferred.** Killing the caller means the MCP call never returns: the
instance is gone before it can say what it did, and whatever it was mid-way through reporting goes
with it. An orchestrator closes its workers; a worker that has reported is closed by whoever
spawned it.

**Busy is `mulpex_core::close_busy_reason`, shared by both frontends** — a policy about when it is
safe to kill someone's work must not exist in two copies that can drift. It checks the **delivery
mark before the status**, and that order is the whole point: a just-spawned instance whose task is
still in flight has written no status file, and a *missing* status file reads as `waiting` — the
same word an idle instance gets (the `status_of` default trap again). By status alone, the moment a
worker is least safe to close is indistinguishable from the moment it is most safe; `spawning/<id>`
is what separates them. `needs` is deliberately **not** busy: an instance blocked on the user is
going nowhere until someone answers it, which is exactly when closing it is the right call.
`force: true` skips the check.

Closing is `Core::close` — the same path ⌘W takes — so a closed instance's file locks release, its
undelivered mail bounces back to its senders, and `closing` is what makes the killed claude
*removable* by the next reap rather than kept as a failed start. Nothing prunes the store on
either frontend: both derive it from the live session list every tick.

Pinned by `mcp::hub_close_refuses_self_all_and_other_projects`,
`mcp::hub_close_dedups_the_same_instance_written_two_ways`,
`a_spawn_still_in_flight_is_busy_even_though_it_looks_idle` (the `waiting`-vs-in-flight case that
motivates the check order) and `state::close_instance_closes_idle_claudes_and_refuses_the_rest`,
which drives the whole handshake against real sessions: one idle claude closes, a busy one is
refused *and left alive*, a terminal is refused with the tool that does close it, an unknown id is
refused, and `force: true` then closes the busy one.

**Not verified: the `mpx` (tmux) side.** It compiles and mirrors the desktop arm, and it shares
`close_busy_reason`, but no test drives it — `mulpex-cli` has no tmux fixture, and this was not
run against a live daemon.
