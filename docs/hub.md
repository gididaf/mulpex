# The coordination hub

How instances find each other, wake each other, name themselves, message across projects, and spawn
siblings. Split across `crates/mulpex-core/src/{hook,mcp,registry,config}.rs` (what a child process
can do) and `src-tauri/src/{hub,state}.rs` (the 200 ms poll loop that fulfils file handshakes).

Shell terminals are in [shell-terminals.md](shell-terminals.md); remote peers in
[remote-peers.md](remote-peers.md).

Back to [CLAUDE.md](../CLAUDE.md).

## The doorbell — idle wake

A `claude` reads its inbox only when it takes a turn, so a peer's message sits unread while the
instance is idle between the user's prompts. **Nothing in Claude Code can start a turn in an idle
session** — no hook reaches one (hooks run only when the session already does something), MCP
channels deliver only to a session that is already live, and `SessionIdle` is requested but
unimplemented (anthropics/claude-code#94812). The one thing that *does* start a turn is a keystroke,
and Mulpex owns the PTY.

So the 200 ms poll (`Core::ring_doorbells`, `state.rs`) types one line into the instance:

```
<<<MPX>>> 1 new hub message(s)
```

- **The body is never typed**, only the trigger. `hub_send` still writes `inbox/<id>/<uuid>.json`
  and the instance still reads it with `hub_inbox`. Typing content into a TUI is what truncated a
  task at 1022 characters (see `hub_spawn` below); `crate::doorbell_line` is ~30.
- **One spelling.** `doorbell_line` generates both the line the poll loop types and the example
  `HUB_RULES` shows the instance (`__MULPEX_DOORBELL__` is substituted from it), so the two cannot
  drift — the failure that ended the previous design.
- **`hook::is_system_turn` recognises it** and puts it on the same side as a `<task-notification>`:
  no sidebar-task overwrite, and **no `userprompt/<id>` unmute mark**, or every peer message would
  undo a ⌘M. `peers_context` is still injected, deliberately — a doorbell turn is the one that most
  needs the unread count.
- **On wake** the instance calls `hub_inbox`, acts autonomously, replies only when it adds value,
  and prefixes the turn `⟳ hub message from <sender> →` so the human can tell it wasn't their
  prompt.

Measured (dev build, 2026-09-17): **12–82 ms** from the message landing to the doorbell being
typed — fast enough that it beats the gap between two messages, which is why a ring almost always
reports `1 message(s)` even under fan-in.

### The three gates, and what each one prevents

`state::should_ring` — all four conditions, each a way to do real damage:

| Gate | What ringing anyway would do |
| --- | --- |
| status is `waiting` | On `needs`, a dialog is on screen: the keystroke picks an option and the `\r` confirms it — Mulpex answers a question in the user's name. |
| …and not `working` | That turn reads its own inbox at its `Stop` (the unread-mail block). A doorbell only queues a redundant second turn. |
| the input box is empty | The doorbell ends in `\r`, so it submits the user's half-written prompt with our text stapled on. See below. |
| not already rung | At 200 ms, six messages arriving together ring six times before the instance drains the first. `rung` clears only when the inbox empties. |

### Knowing whether the input box is empty took three tries

Mulpex cannot read `claude`'s input buffer. It only sees the bytes it forwards, so it infers — and
the first two inferences both shipped to the user and both failed in his hands the same afternoon:

1. **A 3-second quiet timer.** Slow where it shouldn't be: a reply to the pane he had just typed in
   rang at **3051 ms**, against **82 ms** for an untouched one — and the round trip is the common
   case, so nearly every doorbell paid it. Worse, it protected nothing: anyone composing a prompt
   pauses for longer than a timer you would dare set, and the doorbell then fired straight into the
   draft. *"if for example i prepare a prompt in a claude but not send it yet, the doorbell will
   force send it?"* — yes, it would have.
2. **"Was the last key Return."** Safe, but it latched. Clicking into a pane, scrolling it or
   pressing an arrow key all arrive through xterm's `onData` exactly like a keystroke, so any of
   them marked the pane as drafting until the next Return. *"now they read my messages only after
   ENTER."*
3. **Counting what is in the box** (`pty::draft_len_after`). Typing adds, backspace removes,
   Return/Ctrl+C/Ctrl+U zero it, escape sequences change nothing, and UTF-8 *lead* bytes are counted
   so one Hebrew character is one character.

The lesson is (2): **an escape sequence is not a keystroke**, even though it arrives through the
same callback. And the count is resynced on the edge into `working` (`Core::turn_running`), because
a turn starting proves the box was emptied — without that, any over-count is permanent and silences
that pane for the session with nothing to say why.

Errors resolve toward "there is a draft". Being wrong that way makes mail wait behind the sidebar's
unread badge; being wrong the other way sends something the user never wrote.

### What the doorbell replaced, and why it was deleted

Until 2026-09-17 each instance armed a **`Monitor`** on its own inbox, running `"<helper>" listen`
(`mulpex-core/src/listen.rs`) — a 1 Hz poll printing `mulpex: N new hub message(s)`, which the
runtime injected as a turn. `hook.rs` nudged it to arm from `UserPromptSubmit` while
`armed/<id>`'s mtime was stale.

Two measured failures killed it, in order:

- **A command the model retypes, drifts.** The listener was a ~400-character shell one-liner, and a
  model copies whichever version is nearest in its context — its own previous `Monitor` call, not
  the system prompt. Measured 2026-09-16 from live transcripts: `warweb#65` armed a **superseded**
  copy **71 times across two days** and an app update, healing only when `/compact` finally dropped
  that call from its context; its sibling `warweb#74` never recovered. And the failure fed itself —
  the old command never refreshed `armed/<id>`, so the nudge fired every turn and stacked *another*
  Monitor each time. `warweb#65` ran three at once; one hub message woke it three times, which is
  how it was noticed. Moving the loop into a binary fixed that half.
- **Claude Code capped every Monitor at 30 minutes** (v2.1.271, 2026-09-14; `persistent: true` was
  removed, and the schema's `maximum: 3600000` is dead weight — a request for 3600000 comes back
  "expires in 30m", anthropics/claude-code#94553). So every instance woke twice an hour purely to
  re-arm. Each wake cost a full-context turn **and** an Explainer run, because `Stop` files
  `explainreq/<id>` unconditionally — ~96 model calls a day per instance, to say "nothing changed".
  Observed across `warweb#74` and `warweb#81` at exact 30-minute spacing.

Nothing about the second was tunable from this side: instant wake needs a live Monitor, a live
Monitor needs re-arming, and re-arming needs a model turn. The doorbell breaks that triangle by
moving the watch into the host, which also deletes the arm nudge, the `relisten` repair, the
heartbeat, and the whole class of bug where an instance must get a command exactly right.

**What survives the deletion**, and why:

- `listen.rs`, `command_is_hub_listener` and `pty::reap_orphaned_listeners` — **released builds are
  still running listeners right now**, and so is any instance that re-arms one out of its own
  history. They have to be recognised and killed; see the next section. The reaper matches
  `command_is_hub_listener` and deliberately **not** `command_is_watcher`, so it never touches
  agentalk's poll loop — the two matchers exist precisely so this one can stay narrow.
- `armed/`, `listeners/`, `relisten/` in the scratch tree: written by those legacy listeners, read
  by nothing. Removable once no released build is in the wild.
- `nudges_welcome` — it was written for the arm nudge but the naming nudge has the same requirement,
  and a doorbell counts as a system turn for exactly that reason.

**`background_tasks` is a `Stop`-only field** (measured, `claude` v2.1.273): present on `Stop`,
absent from `UserPromptSubmit` even 24 s after a Monitor was armed, from `PostToolUse`, and from the
idle notification. It no longer drives a repair, but it is still what `background_work_running`
reads, and the constraint bites anything that needs to know what is running.

### A listener outlives everything that is supposed to kill it

`Session::kill` `killpg`s the child's process group (`pty.rs`) and sweeps its controlling terminal
(`kill_tty_session`). **A hub listener escapes both.** Claude Code runs each background command in
its own process group with no controlling terminal — measured on live listeners: `PGID == pid`,
`SESS 0`, state `Ss` (no `+`). So it survives ⌘W, a crash and app teardown alike, and launchd
adopts it still spinning `sleep 1`. Six were found alive on one machine at once, the oldest from
the previous morning, belonging to a Mulpex that had already exited.

Mulpex no longer *starts* a listener, but this section is not historical — it got stronger. The
reaper now kills **every** hub listener it finds, not only the parentless ones, and runs once a
minute from the poll loop as well as at launch and teardown.

Dropping the `ppid == 1` condition was safe the moment nothing armed a listener, and it turned out
to be necessary the same day. `warweb#75` was spawned on 2026-09-17 with the new rules — its argv
contains `do NOT arm anything` — and it went on arming a Monitor every 30 minutes through the
night. Its transcript says why: **141 `Monitor` arm calls, the first on 2026-09-14.** Four days of
watching itself do a thing beats one sentence telling it not to, which is `warweb#65`'s 71-times
failure again at a larger number. Prose cannot revoke a habit; SIGKILL can.

The sweep runs *while the app is up* for the same reason: an instance that arms one at 03:00 would
otherwise keep waking itself until the user happened to restart Mulpex. A minute is the interval —
the walk reads every process's argv, which is far too expensive per tick and pointless faster,
since catching a stray a minute late costs one wake-up that was going to happen anyway.

Two things keep it safe. The command match is now the *only* thing between this and `kill -9` on
arbitrary pids, so it stays `command_is_hub_listener` — never `command_is_watcher`, which
deliberately also covers agentalk's poll loop and the user's `watchers.txt`. And the death is
quiet: Claude Code reports the killed Monitor as a "stopped, no completion record" wake, which
`orphaned_task_wake` already swallows.

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

> **Historical as to cause, current as to code.** The arm nudge this describes was deleted with the
> listener (2026-09-17), so the loop below can no longer start. `orphaned_task_wake` and the
> `resumed/<id>` exemption are **still live** and still needed — a killed Monitor from a released
> build still produces the wake, agentalk watchers produce it too, and the reasoning about what a
> `<task-notification>` *is* underpins `is_system_turn`, which the doorbell now depends on. Kept in
> full because the shape recurs and the measurement was expensive.

The arming nudge was self-healing by design, and for a while it healed a wound it was itself
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
description of what rides it.)

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
