# Codex as a second agent kind

A sidebar row runs a **claude**, a **codex** (OpenAI Codex CLI) or a **terminal**. Claudes and
codexes are both hub peers in one project, share one number space and are written `claude#3` /
`codex#4`. ⌘T opens a claude, ⌘⌥T a codex, and ⌘⌥S hands a row's work to the other agent
("Continue in Codex / Continue in Claude") — the reason this exists is to keep working when one
account hits its 5-hour or weekly limit.

**Rule:** agent-specific behaviour lives in one function per concern that takes an
`Agent` (`crates/mulpex-core/src/agent.rs`), never in an inline `if codex`. The Claude path must
stay byte-identical (golden tests on its argv and rules).

## Measured facts (codex-cli 0.149.1, macOS, 2026-10-21)

Driven on a real `codex` in tmux and with `codex exec`; nothing below is from documentation.

| # | Fact |
|---|---|
| S1 | Hooks go **per launch**: `-c 'hooks.<Event>=[{hooks=[{type="command",command="…",timeout=N}]}]'` plus `--dangerously-bypass-hook-trust`. Nothing is written to `~/.codex`. The user's own `~/.codex/hooks.json` hooks still run alongside. |
| S2 | Hook processes inherit the codex process env (`MULPEX_*` reach them). |
| S3 | The MCP server does **not** inherit env: it needs `-c 'mcp_servers.mulpex.env={MULPEX_INSTANCE_ID="7",…}'`. Model-visible tool names are `mcp__mulpex__hub_*` — identical to Claude's. |
| S4 | `-c developer_instructions="<json-escaped string>"` delivers rules text with quotes, newlines, `#` intact, obeyed. |
| S5 | Payloads (all carry `session_id`, `transcript_path` = the rollout file, `cwd`, `hook_event_name`, `model`, `permission_mode`; turn events add `turn_id`): **SessionStart** `source` (`startup`); **UserPromptSubmit** `prompt`; **PreToolUse/PostToolUse** `tool_name`, `tool_input`, `tool_use_id` (+`tool_response` on Post). Shell commands arrive as `tool_name:"Bash"`, `tool_input.command` (a string) — the existing bash guard works unchanged. Edits arrive as `tool_name:"apply_patch"`, `tool_input.command` = the patch text, with **absolute** paths in `*** Update File: /abs`, `*** Add File:`, `*** Delete File:` (`*** Move to:` for renames). **Stop** `stop_hook_active`, `last_assistant_message`. **PreCompact** `trigger` (`manual`/`auto`); **PostCompact**. MCP calls fire Pre/PostToolUse too. |
| S6 | `additionalContext` from UserPromptSubmit and PostToolUse reaches the model. |
| S7 | A PreToolUse hook that sleeps 6 s and then returns `permissionDecision:"deny"` + reason blocks the `apply_patch`; the model reports the reason. Locks work. |
| S8 | Stop returning `{"decision":"block","reason":…}` continues the turn; the next Stop has `stop_hook_active:true`. |
| S9 | `/compact` typed into an empty box compacts; PreCompact then PostCompact fire; the session id is unchanged; the screen shows `• Context compacted`. |
| S10 | The TUI fires **SessionStart only at the first prompt** (the rollout file is created then too). Before that Mulpex cannot know the session id. After `codex resume <id>` SessionStart again waits for the next prompt — the id is already known. |
| S11 | `codex resume <unknown id>` prints `ERROR: No saved session found with ID …` and **exits 1** — the existing failed-start path catches it. |
| S12 | `codex queue --thread <id> --message <text>` (≈0.1 s) **starts a turn in an idle TUI** on a PTY (UserPromptSubmit fires), **leaves an unsent draft in the box untouched**, and **while busy waits and runs as the next turn**. Works on a resumed TUI. **To a thread with no TUI open it still exits 0** ("Queued message …") — so a queue is only delivered when UserPromptSubmit sees it (the ack). |
| S15 | Empty composer: `ESC[1m›ESC[0m ESC[2mAsk Codex to do anything ESC[0m` (bold `›`, space, **dim** placeholder). A draft: bold `›`, space, normal text. The footer line under it shows `model effort · cwd`. |
| S17 | Context: rollout `event_msg` `token_count` → `info.last_token_usage.total_tokens / info.model_context_window`. The same event carries `rate_limits.primary/secondary.used_percent` (5 h / 7 d). |
| S18 | `codex exec fork <id> --ephemeral --skip-git-repo-check -c sandbox_mode="read-only" --json -o <file> -` (prompt on stdin; **stdin must be given or closed**, or exec waits for it) answers from the forked conversation, leaves the original rollout byte-identical and writes no new rollout. `--json` emits `thread.started{thread_id}`, `turn.started`, `item.completed{item:{type:"agent_message",text}}`, `turn.completed{usage}` — **no text deltas**, so Explain does not stream for codex. |
| S20 | Codex sets `CODEX_CI=1`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID` in commands it runs — stripped from every child we spawn (`mulpex_core::codex::CHILD_ENV_DENY`). |
| — | Startup shows a blocking "Update available" picker unless `-c check_for_update_on_startup=false`. |
| — | The real binary is `~/.local/bin/codex` (→ `~/.codex/packages/standalone/releases/<v>/bin/codex`). The user's shell aliases `codex` to add flags — never resolve through the alias. |
| — | A directory Codex has not been told to trust opens a blocking "Do you trust the contents of this directory?" picker — **even with the yolo flag**. The dotted `-c 'projects."<dir>".trust_level="trusted"'` does **not** skip it; the inline table `-c 'projects={"<dir>"={trust_level="trusted"}}'` does, for this launch only (`config.toml` untouched). Trust is keyed by the **git root** for a subdirectory or worktree, so `launch_args` lists both. |
| — | **Typing into codex:** one write of `text\r` does not submit — its paste-burst detection turns the CR into a newline and leaves a draft. Bracketed paste + CR in one write (`\e[200~text\e[201~\r`) submits. So `/compact` is sent that way, and Shift/Option+Enter's `\e\r` is a newline as in claude. History echoes of past prompts are bold+dim `›`; the live composer's `›` is bold, undimmed. |
| — | `codex delete <id>` refuses outside a TTY; `codex delete --force <id>` (Explain's fork cleanup). A failed `codex exec` reports on **stderr**, the reason being the **last `ERROR:` line** (after retry noise). |
| — | A turn that ends in an API error (OpenAI 429 after retries) or an interrupt fires **no Stop hook** — the app reads the rollout tail to bring such a row back to `waiting`. |
| — | `codex queue` → ack measured 8.2–8.5 s on a just-resumed codex (fast on a warm one), so the ack window is 15 s and a late ack still upgrades "unanswered" to "delivered". |
| S21 | Not installed on the Linux VM yet; a host without `codex` disables ⌘⌥T there with a toast. |

## Contracts between the parts

### Scratch-dir files (per project state dir)
| Path | Writer | Reader | Meaning |
|---|---|---|---|
| `agents/<id>` | app at spawn | helper (labels, `hub_instances` agent/address — the registry entry itself has no agent field), app | contents `codex`; absent ⇒ claude |
| `sessionid/<id>` | codex SessionStart/Stop/UserPromptSubmit hook (payload `session_id`) | app `reconcile_session_ids` | same file Claude uses |
| `transcript/<id>` | codex hooks (payload `transcript_path`) | app transcript resolver | absolute rollout path |
| `agentready/<id>` | codex SessionStart hook | app `wake_ready` | the codex has a thread id and hooks work |
| `wake/<id>.ack` | codex UserPromptSubmit when the prompt is a wake line | app wake ledger | delivery proof (mtime) |
| `wake/<id>.log` | app | humans | one line per ring attempt and outcome |
| `ctx/<id>` | codex Stop / PostToolUse (throttled) from the rollout | app (unchanged) | context % |
| `spawn/<token>.json` | helper `hub_spawn` | app | gains optional `"agent":"codex"`; missing ⇒ the spawner's own agent |

### Core API (`mulpex-core`)
- `agent::Agent { Claude, Codex }` — `word()`, `title()`, `parse()`, `other()`, `from_env()` (`MULPEX_AGENT`).
- `codex::launch_args(helper, state_dir, project_dir, id) -> Vec<String>` — every codex option (yolo, hook-trust bypass, `--no-alt-screen`, update check off, the `-c hooks.*`, `-c mcp_servers.mulpex.*` with env, `-c developer_instructions=`). The app appends `resume <uuid>` and/or the positional prompt.
- `codex::wake_line(n)` / `codex::is_wake_line(prompt)` — `mulpex: N new hub message(s)`, the text `listen.rs` prints for Claude.
- `codex::ctx_pct(rollout) -> Option<u8>`; `codex::CHILD_ENV_DENY`.
- `rules::system_prompt_for(helper, Agent)` — Claude ⇒ exactly `append_system_prompt(helper)` (golden hash). `rules::spawn_prompt_for(task, parent_agent)`, `rules::board_rule(agent)`.
- `remote::remote_launch_command_for(agent, …)`, `looks_like_tui`/`has_spinner_for(agent)` — `hub_remote_open` takes `agent`, default **claude** (codex may not be installed on the far box).
- `persist::SavedSession.agent` — 10th column, written only for codex.

### App commands / events (frontend ↔ backend)
- `create_session(projectHandle, agent?: "claude"|"codex")` — default claude.
- `agent_status() -> { claude: ClaudeStatus, codex: ClaudeStatus }` (same shape as `claude_status`, which stays).
- `continue_in_other_agent(projectHandle, id)` — the handoff.
- `wake_typed(projectHandle, id, ok: bool, reason: string)` — the frontend's answer to a `codex-wake {handle, id, line}` event (typed fallback, used only before a codex has a thread id).
- `StatusEntry.wake: { atMs, state: "rang"|"delivered"|"unanswered"|"refused", detail }?`
- Menu ids `new_codex_session` (⌘⌥T) and `continue_other_agent` (⌘⌥S).
- `SessionInfo.kind`: `"claude" | "codex" | "shell"`.
- Phone wire keeps `kind:"claude"` for every agent row and adds `agent: "claude"|"codex"` (an old cached PWA would open a non-"claude" row as a terminal).

## Wake (why it is not the reverted doorbell)
A claude is woken by its plugin-monitor listener. Codex has no monitors, so the **app** rings a
codex whose inbox grew while it is `waiting`: `codex queue --thread <sid> --message "mulpex: N new
hub message(s)"` — a declared CLI, not typing into a TUI, and it never touches a draft (S12). A ring
counts only when the codex's own UserPromptSubmit hook acks it (`wake/<id>.ack`); an unacked ring is
retried every 30 s and shown on the row as amber ⚠ — a ring that does not arrive is as visible as
one that does. Before the first prompt there is no thread id; then (only then) the frontend types
the line (bracketed paste + CR) into an **empty** box, and refuses visibly over a draft (`wake_typed`
is denied over Remote Hosts: two windows would type twice). A resumed codex has a thread id but no
`agentready` until its next prompt, so queue is used once the codex has been up ≥ 4 s.

## Documented gaps
No Read lock for codex (it reads through `Bash`, which the bash guard covers by path); no
AskUserQuestion/plan dialogs — `needs` comes from the rules' `[waiting for you]` last line;
no background-task tracking; Live Token is Claude-only; Explain does not stream; codex's own
subagents are invisible to the hub; tasks still ride argv (the `pkill -f` note in CLAUDE.md).
