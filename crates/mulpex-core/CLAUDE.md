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
| `hook.rs`, `config.rs` | [../../docs/sessions.md](../../docs/sessions.md) — what each status word means and why `needs` must mean "needs YOU"; [../../docs/hub.md](../../docs/hub.md) — the doorbell contract (`is_system_turn`) and the naming nudge |
| `mcp.rs` (`hub_send`/`hub_spawn`/`hub_set_name`) | [../../docs/hub.md](../../docs/hub.md) |
| `mcp.rs` (`hub_terminal_*`), `termlog.rs` | [../../docs/shell-terminals.md](../../docs/shell-terminals.md) |
| `registry.rs` | [../../docs/hub.md](../../docs/hub.md) — the `<project>#<n>` grammar and its ordered parser |
| `remote.rs` | [../../docs/remote-peers.md](../../docs/remote-peers.md) |
| `persist.rs` | [../../docs/sessions.md](../../docs/sessions.md) — the store's positional columns |
| `rules.rs` (`HUB_RULES`, `PLANNING_RULES`, `spawn_prompt`, `DOORBELL_PREFIX`), `state_dir.rs` | [../../docs/hub.md](../../docs/hub.md), [../../docs/sessions.md](../../docs/sessions.md) — one copy, because the app writes them and the helper reads them back |
| `listen.rs` (`mulpex-helper listen`) | [../../docs/hub.md](../../docs/hub.md) — **superseded by the doorbell**; kept only because released builds still run listeners that must be recognised and reaped |

Traps that live in this crate specifically:

- **A bare integer filename at the state-dir root is scanned as an instance status file**
  (`mcp::live_ids`). Any new per-instance flag goes in a subdir — `bg/`, `watching/`, `compacting/`,
  `armed/`, `relisten/`, `pids/`, `listeners/`, `explainreq/`,
  `named/`, `namenudge/`, `spawning/`, `resumed/`, like `peers/` already does.
- **A `<task-notification>` turn is the runtime talking, not the user.** It is a real turn that
  fires `UserPromptSubmit` like any other, so anything the hook *asks the model to do* has to be
  gated on it (`nudges_welcome`) — the old arm nudge injected there made the instance start the very
  Monitor whose death causes the next one. A **doorbell** is the same kind of turn and is classed
  with it by `is_system_turn`. The peer snapshot is the deliberate exception: a hub wake *is* a
  task-notification, and is the turn that most needs the unread count. → [../../docs/hub.md](../../docs/hub.md)
- **`persist.rs`'s store columns are positional** (`<uuid>[\t<name>[\tmuted[\t<id>]]]`). Only
  *trailing* empties may be dropped, or the id is read back as the name.
- **`SessionStore::new` picks the home from the ambient `MULPEX_HOME`, so anything that is not the
  desktop app must not use it.** Leave that variable unset and the first write lands in the app's
  own `~/.mulpex/sessions/`, handing the same `--resume` uuid to two claudes — silent conversation
  corruption, produced by an *omission* rather than a mistake. `SessionStore::in_home` takes the
  home explicitly, which is what makes the bug unwritable. `mpx` was the caller that forced this
  and is gone; the hazard is in the ambient-home design, so the signature stays.
- **Never ask an instance to set something up that this side could do itself.** `HUB_RULES` used to
  carry a `Monitor` command the model retyped, and an instance re-armed a *superseded* copy 71 times
  across two days and an app update, because a model copies its own last `Monitor` call before it
  re-reads the system prompt. Moving the loop into `listen.rs` fixed the retyping; the **doorbell**
  finished it by leaving nothing to arm. `HUB_RULES` now only *describes* what a doorbell looks
  like. → [../../docs/hub.md](../../docs/hub.md)
- **`rules.rs` and `hook.rs` are two ends of one contract.** `HUB_RULES` tells the instance a line
  starting `<<<MPX>>>` is Mulpex ringing; `hook::is_system_turn` has to recognise the same line, or
  a doorbell overwrites the sidebar task and unmutes a ⌘M'd row. Both spellings come from
  `crate::doorbell_line` and `hub_rules_carry_the_doorbell_contract` asserts it, along with the
  absence of any surviving `__MULPEX_*__` placeholder and of anything still asking an instance to
  arm. → [../../docs/hub.md](../../docs/hub.md)
- **Only the `Stop` payload carries `background_tasks`** — not `UserPromptSubmit`, not
  `PostToolUse`, not the idle notification (measured, `claude` v2.1.273). Anything that needs to
  know what is running has to be decided in `stop` and handed forward on disk.
  → [../../docs/hub.md](../../docs/hub.md)
- **A background agent's tool calls fire `PostToolUse` in the PARENT session.** So does anything
  else the instance started; the hook cannot tell whose call it is answering except by `tool_name`.
  This is why `posttooluse` may not write `working` blindly: it ran ~once every two seconds over an
  `AskUserQuestion` dialog, and once the red was gone the `permission_prompt` that follows it turned
  the row green in front of an unanswered question. Only `DIALOG_TOOLS` may clear a `needs`.
  → [../../docs/sessions.md](../../docs/sessions.md)
- **The watcher list is generic; the listener matchers are not.** `command_is_watcher` (built-ins +
  `<mulpex home>/watchers.txt`) is what keeps a never-exiting background task from pinning a row
  yellow — agentalk's poll loop and events tail, legacy hub listeners, anything the user adds. But
  `pty::reap_orphaned_listeners` matches `command_is_hub_listener` alone, because it SIGKILLs, and
  must only ever do that to Mulpex's own listener. A watcher also writes `watching/<id>` separately
  from `bg/<id>`, because the status word and the updater's busy guard want opposite answers.
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
