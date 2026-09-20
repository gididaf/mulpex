# mulpex-core (the headless lib the helper links)

Root rules: [../../CLAUDE.md](../../CLAUDE.md). This crate is what a child `claude` process actually
executes — `hook <event>` on every tool call and turn boundary, and a long-lived `mcp` server. It
runs in a **different process from Tauri** and shares state with it only through files under
`$MULPEX_STATE_DIR`, which is why nearly everything here is a file handshake.

A change here therefore lands in **two processes**, and "the app still compiles" is only half a
test: the app writes this state and the helper reads it back, out of band, with no type system
between them. (It also used to serve a second host, `mpx`, deleted 2026-09-17 — the process split is
the reason that outlived it.)

| Module | Read first |
| --- | --- |
| `hook.rs`, `config.rs` | [../../docs/sessions.md](../../docs/sessions.md) — what each status word means and why `needs` must mean "needs YOU"; [../../docs/hub.md](../../docs/hub.md) — listener arming and naming nudges |
| `mcp.rs` (`hub_send`/`hub_spawn`/`hub_set_name`) | [../../docs/hub.md](../../docs/hub.md) |
| `mcp.rs` (`hub_terminal_*`), `termlog.rs` | [../../docs/shell-terminals.md](../../docs/shell-terminals.md) |
| `registry.rs` | [../../docs/hub.md](../../docs/hub.md) — the `<project>#<n>` grammar and its ordered parser |
| `remote.rs` | [../../docs/remote-peers.md](../../docs/remote-peers.md) |
| `persist.rs` | [../../docs/sessions.md](../../docs/sessions.md) — the store's positional columns |
| `rules.rs` (`HUB_RULES`, `PLANNING_RULES`, `spawn_prompt`), `state_dir.rs` | [../../docs/hub.md](../../docs/hub.md), [../../docs/sessions.md](../../docs/sessions.md) — one copy, because the app writes them and the helper reads them back |
| `listen.rs` (`mulpex-helper listen`) | [../../docs/hub.md](../../docs/hub.md) — why the listener is a binary, and why it has to notice its own `claude` dying |

Traps that live in this crate specifically:

- **A bare integer filename at the state-dir root is scanned as an instance status file**
  (`mcp::live_ids`). Any new per-instance flag goes in a subdir — `bg/`, `watching/`, `compacting/`,
  `armed/`, `relisten/`, `pids/`, `listeners/`, `explainreq/`,
  `named/`, `namenudge/`, `spawning/`, `resumed/`, `sessionid/`, `quietturn/`, like `peers/`
  already does.
- **A re-arm-only wake must leave no trace, and proving it *is* re-arm-only is the whole job.**
  `quietturn/<id>` is written when a `<task-notification>` turn starts and cleared by the first
  tool call that is not `is_listener_rearm` — so surviving to `Stop` is evidence, not a guess.
  Three handlers share it (`userpromptsubmit`, `posttooluse`, `stop`, plus `write_needs` for an
  escaped dialog, which fires no `PostToolUse` at all); change one and check the other three,
  because the failure mode is an explanation that silently never appears.
  → [../../docs/hub.md](../../docs/hub.md)
- **A `<task-notification>` turn is the runtime talking, not the user.** It is a real turn that
  fires `UserPromptSubmit` like any other, so anything the hook *asks the model to do* has to be
  gated on it (`nudges_welcome`) — an arm nudge injected there made the instance start the very
  Monitor whose death causes the next one. The peer snapshot is the deliberate exception: a hub
  wake *is* a task-notification. → [../../docs/hub.md](../../docs/hub.md)
- **`persist.rs`'s store columns are positional** (`<uuid>[\t<name>[\tmuted[\t<id>]]]`). Only
  *trailing* empties may be dropped, or the id is read back as the name.
- **`SessionStore::new` picks the home from the ambient `MULPEX_HOME`, so anything that is not the
  desktop app must not use it.** Leave that variable unset and the first write lands in the app's
  own `~/.mulpex/sessions/`, handing the same `--resume` uuid to two claudes — silent conversation
  corruption, from an omission rather than a mistake. `SessionStore::in_home` takes the home
  explicitly, which is what makes the bug unwritable. `mpx` was the caller that forced this and is
  gone; the hazard is in the ambient-home design, so the signature stays.
- **The listener is armed by the generated plugin monitor, and `HUB_RULES` must NOT name the
  command.** `state_dir::write_state_dir` writes `plugin/monitors/monitors.json` with
  `"<helper>" listen` (`__MULPEX_BIN__`-substituted like `settings.json` and `mcp.json`), and
  `pty.rs` spawns with `--plugin-dir` — Claude Code arms it, so no turn is spent and nothing
  expires. Putting the command back in the rules gives the instance a second listener and every
  hub message arrives twice; `rules::tests::hub_rules_leave_the_arming_to_mulpex` is the guard.
  `rules::listener_command` stays the one spelling shared by the monitor, the crash-only arm nudge
  and `hook::command_is_hub_listener`. It was a ~400-character shell loop once, and an instance
  re-armed a *superseded* copy 71 times across two days because a model copies its own last
  `Monitor` call before it re-reads the system prompt — so never move behaviour into that string
  either: anything the loop must do goes in `listen.rs`, which ships with the app.
  → [../../docs/hub.md](../../docs/hub.md)
- **`rules.rs` must stay byte-identical across hosts, not merely equivalent**, and `hook.rs` reads
  it from two ends: `command_is_hub_listener` recognises the command (which is what keeps a
  listener from counting as work in flight, and what the orphan reaper matches on), and the arm
  nudge repeats it verbatim. A character of drift strands every instance on yellow, or re-nudges it
  forever. `hub_rules_carry_the_exact_arming_command` asserts both, plus that no `__MULPEX_BIN__`
  placeholder survives into the prompt.
  → [../../docs/hub.md](../../docs/hub.md)
- **Only the `Stop` payload carries `background_tasks`** — not `UserPromptSubmit`, not
  `PostToolUse`, not the idle notification (measured, `claude` v2.1.273). Anything that needs to
  know what is running has to be decided in `stop` and handed forward on disk, which is what
  `relisten/<id>` is. → [../../docs/hub.md](../../docs/hub.md)
- **A background agent's tool calls fire `PostToolUse` in the PARENT session.** So does anything
  else the instance started; the hook cannot tell whose call it is answering except by `tool_name`.
  This is why `posttooluse` may not write `working` blindly: it ran ~once every two seconds over an
  `AskUserQuestion` dialog, and once the red was gone the `permission_prompt` that follows it turned
  the row green in front of an unanswered question. Only `DIALOG_TOOLS` may clear a `needs`.
  → [../../docs/sessions.md](../../docs/sessions.md)
- **The watcher list is generic; the listener matchers are not.** `command_is_watcher` (built-ins +
  `<mulpex home>/watchers.txt`) is what keeps a never-exiting background task from pinning a row
  yellow — the hub listener, agentalk's poll loop and events tail, anything the user adds. But
  `running_listener_ids`/`note_listener_needs_replacing` and `pty::reap_orphaned_listeners` still
  match `command_is_hub_listener` alone: those re-arm and kill things, and they must only ever do
  that to Mulpex's own listener. A watcher also writes `watching/<id>` separately from `bg/<id>`,
  because the status word and the updater's busy guard want opposite answers about it.
  → [../../docs/sessions.md](../../docs/sessions.md)
- **Don't report a default as a fact.** `status_of` returns `waiting` for a *missing* file, and that
  ambiguity once made a 91 s spawn stall indistinguishable from a lost task.
  → [../../docs/hub.md](../../docs/hub.md)
- **The hook is the only thing that sees what `claude` actually received.** Mulpex knows the prompt
  it sent, the child knows the prompt it got, and `hook::verify_spawn_delivery` is the single point
  where those meet — which is why a task truncated in transit was invisible for as long as it was.
  When you need to prove something about a child's input rather than assume it, that comparison is
  the only honest place to make it. → [../../docs/hub.md](../../docs/hub.md)
- **A task is delivered as an argv argument, never typed.** True for a locally spawned child
  (`pty.rs`) and for a remote peer (`remote::remote_launch_command`, base64'd). Typing capped it at
  1022 characters with no error anywhere. Only text for an ALREADY-RUNNING instance is typed, and
  that is still capped — keep it short.
  → [../../docs/hub.md](../../docs/hub.md), [../../docs/remote-peers.md](../../docs/remote-peers.md)
- **`HUB_RULES`/`config.rs` templates are `--append-system-prompt` text**, re-sent every turn, so
  they survive compaction — that is why contracts with instances live there and not in an injected
  prompt. Anything whose grammar is also parsed in code (the remote `<<<MPX …>>>` marker) has a test
  asserting the rules' own example parses, so the two halves cannot drift.
