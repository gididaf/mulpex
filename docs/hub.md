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
  self-triggered turn with a `⟳ hub message from <sender> →` marker so the human can tell it
  wasn't their prompt. This coexists with the `userpromptsubmit` hook's unread-count nudge, which
  still covers the "notice on your next prompt" path.

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
  injector therefore: (1) waits for a *drawn input box* — `input_box_ready` looks for that chrome
  in a rolling `TAIL_CAP` tail of output — rather than for "painted then quiet", which
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
  actually makes this robust.

  **But the readiness check cannot afford to be casually wrong, which these notes used to claim it
  could.** A false negative is not free: it costs the full `READY_TIMEOUT` (90 s) of a child
  sitting there with no task, which is long enough that its spawner gives up and concludes the
  task was lost. Measured in the field — see **A slow spawn must not look like a lost one**.
  `input_box_ready` therefore accepts **both** chrome styles claude v2.1.235 was observed drawing
  *on the same machine*: the rounded box (`╭` … `╰`) and a pair of plain `─` rules, either of them
  plus a `>`/`❯`. In the project the field report came from, the child emitted **zero** `╭`/`╰`
  and 408 `─`, so requiring the corners made the fast path a coin flip decided by the project.
  Pinned by `the_input_box_is_recognised_in_both_chrome_styles`, confirmed to fail (*"ruled input
  area not detected"*) with the corners-only rule restored.

### A slow spawn must not look like a lost one

Reported from the field: two consecutive `hub_spawn` calls each returned `ok: true` with a new id,
the instance appeared, and it sat there doing nothing. The spawner checked `hub_instances`, saw
`{"status":"waiting","task":""}`, concluded the seeding had silently dropped its 9,000-character
assignment, and re-sent the whole thing by hand with `hub_send`.

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
  injected prompt carries the `[mulpex:hub]` sentinel precisely so `hook::userpromptsubmit` skips
  capturing it. Every other row in the listing has a task, so the empty one reads as broken.
- **`ok: true`** was answered as soon as the child *process* existed. Creating a process is not
  delivering a task, and the two were minutes apart.

So the spawn path now publishes what it knows, and the API stops asserting what it doesn't:

- **`spawn_instance_with_task` seeds `tasks/<id>` with the assignment before the child exists**, so
  `hub_instances` shows what it was sent instead of `""`. (Removed again if the spawn itself
  fails, so a number handed to somebody else can't inherit it.)
- **The injector publishes its verdict to `spawning/<id>`** (`pty::spawn_delivery_path`):
  `pending` written synchronously by the spawn path, cleared on the `turn_started` proof, and
  `failed` when the retries are exhausted or the child dies during startup. That thread is the
  only thing in the system that ever knows the task was lost, and **returning quietly was the
  defect** — same shape as every other silent failure in these notes (**How this codebase fails**
  in [../CLAUDE.md](../CLAUDE.md)). A subdir for the usual reason:
  a bare integer at the state-dir root is scanned as a status file (`mcp::live_ids`).
- **`hub_instances` reports `task_delivery`** (`pending`/`failed`, absent once delivered) with a
  note saying what to do about it — wait, or `hub_send` it yourself. An instance nobody spawned
  carries no delivery claim at all.
- **`hub_spawn` waits for delivery** (`await_delivery`, capped at `SPAWN_DELIVERY_WAIT` = 60 s) and
  **`ok` now tracks the task, not the process**: `tasks_delivered` / `tasks_not_delivered_yet` /
  `tasks_never_delivered`, with `ok` true only when every task landed. The injector's own worst
  case is longer than that wait, which is exactly why an unresolved wait is reported as `pending`
  rather than rounded up to success; the no-response fallback reply is now `ok: false` too. The
  tool description says all of this, including that a brand-new instance showing `status: waiting`
  is *normal* while delivery is pending.

Pinned by `hub_instances_says_whether_a_spawned_task_actually_arrived` (three instances, none with
a status file, so they are indistinguishable without the marker) and
`hub_spawn_never_claims_ok_for_a_task_that_did_not_arrive`.

