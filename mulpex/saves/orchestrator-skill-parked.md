---
title: "פיצ'ר orchestrator ל-Mulpex"
description: "נבנה ונבדק פעם אחת, הושהה לפי בקשה ושמור בצד."
author: "Gidi"
created: 2026-10-07
updated: 2026-10-07
---

## Goal
Add an `/orchestrator` feature to Mulpex. An orchestrator is a claude that does no work itself. It splits work into items (e.g. 10 Trello tickets), confirms the split with the user, `hub_spawn`s one claude per item and coordinates. Children must work **with the user directly** (AskUserQuestion, phases, approvals) so the orchestrator's context stays small. This is "part 1". The user named the later part "the QA part": a QA step, and probably a git worktree per child.

## Status
Parked by the user on 2026-10-07 "until future notice".
- The code is finished and was tested once in a real session.
- It is saved on branch `park/orchestrator` (commit `33db417`) and is NOT on `master`.
- Nothing is merged or released, and the master working tree was left clean.

## Context
- **Repo:** `/Users/gididaf/Documents/Code/utilities/mulpex`, a Tauri app hosting many Claude Code instances, with a coordination hub over MCP. Read the root `CLAUDE.md` and `docs/hub.md` first.
- **Plugin:** Mulpex generates a plugin (`<state_dir>/plugin`, loaded with `--plugin-dir`) on every spawn. Skills live there: `/explain` already existed, and `/orchestrator` is added beside it.
- **System prompt:** every claude gets `HUB_RULES` + `PLANNING_RULES` via `--append-system-prompt` (`crates/mulpex-core/src/rules.rs::append_system_prompt`).
- **Spawn prompt:** `hub_spawn` children start on `rules.rs::spawn_prompt(...)`, passed as argv.
- **The rules:** the user wants the rules from their global CLAUDE.md baked into Mulpex. Those rules are: zero assumptions, verified via AskUserQuestion; incremental phases, stopping after each for the user's QA.
- **The coworker:** the user named them as the reason for baking the rules in. Their words: "i have them in my global CLAUDE.md already but maybe my coworker who using Mulpex doesn't. the spawned claudes also need to respect this rule." So the coworker getting the new defaults is the intended effect, not a side effect.
- **Children work with the user:** "every claude should work with me completly i mean use the AskuserQuestion tool with me and everything else with me. the orchestrator can help them with single /agentalk for example or any single things that not depends on the specific session."
- **Lean orchestrator:** "i don't want to spam the orchestrator context window without really need so this is why i prefer the children to work against me as much as they can".
- **Other orchestrators:** other projects have hand-written orchestrator setups, e.g. cloudraw#9 with workers briefed from `~/.claude/plans/cloudraw-worker-rules.md` in "AUTONOMOUS MODE" (no AskUserQuestion, report only via the orchestrator). They are not this feature. They conflict with the new default child brief (see Open questions).

## Done
One commit, `33db417` "feat(hub): /orchestrator skill; hub_spawn children work with the user (PARKED)", on branch `park/orchestrator`.
- Its parent is `572f6ad` (chore: v0.31.0, master HEAD at parking time).
- It was created with `git commit-tree` from a temporary index, without switching branches.
- `git show --stat` shows 6 files, +105/-20.

**`crates/mulpex-core/src/config.rs`**
- New `ORCHESTRATOR_SKILL_MD`: frontmatter `name: orchestrator`, `disable-model-invocation: true`, uses `$ARGUMENTS`.
- What the skill tells the orchestrator to do:
  - No edits, commits or builds. Reading and fetching only, to write briefs.
  - Name its row via `hub_set_name`.
  - Confirm the split via AskUserQuestion, then write self-contained briefs and spawn at most 8 per call.
  - Tell the user which claude#N got which item, then end its turn. No polling, no reading panes.
  - Never answer for the user.
  - `hub_close` a child when its "done" line arrives, and retry on the next wake if refused mid-turn.
- Added AFTER the live test: never write briefs that switch off the child's rules ("don't ask", "no phases"), and never offer that as an option, unless the user asks in their own words, unprompted. The user requested this rule ("yes please") but never saw it run.
- `PLUGIN_MANIFEST_JSON` description now mentions /orchestrator.

**`crates/mulpex-core/src/state_dir.rs`**
- Writes `plugin/skills/orchestrator/SKILL.md`.
- Test `the_plugin_monitor_runs_the_listener_command_verbatim` asserts the skill's name, `disable-model-invocation` and `$ARGUMENTS`.

**`crates/mulpex-core/src/rules.rs`**
- `PLANNING_RULES` gains "Keep asking … until zero assumptions remain" and an INCREMENTAL PLANS paragraph: small phases, a testable milestone each, stop for the user's QA. This applies to EVERY Mulpex claude, the coworker's included.
- `spawn_prompt` now tells EVERY hub_spawn child to work WITH THE USER, not the spawner:
  - no questions or progress updates to the spawner;
  - message the spawner only for session-independent help;
  - when the user confirms the task is done, send ONE line: "done — <≤15 words>".
  - The sentinel prefix and single-line form are kept.
- The `HUB_RULES` hub_spawn entry is reworded to match.
- NOT changed: the `HUB_RULES` REMOTE CLAUDES paragraph (around `rules.rs:105`) and the `hub_remote_open` description (`mcp.rs:220`), which still say "works autonomously, signals you when done, blocked or needs an answer". This was never discussed; it was outside the scope considered, not a deliberate decision (see Open questions).

**`crates/mulpex-core/src/mcp.rs`**
- The `hub_spawn` tool description drops "autonomously" and describes the new child behavior.
- The success `note` says children send one "done" line.

**Docs**
- `docs/hub.md`: new child brief plus its history, and a new `/orchestrator` bullet after the nesting bullet.
- Root `CLAUDE.md`: the plugin paragraph mentions /orchestrator.

**Live test** (session `9f93385d-2cdc-4664-91ea-385cef724c54`, project `~/Documents/Code/test`, transcript under `~/.claude/projects/-Users-gididaf-Documents-Code-test/`):
- `/mulpex:orchestrator` named its row, asked the user to confirm, and spawned claude#8 and #9. Both tasks were verified as delivered.
- Both children sent "done — …". The orchestrator read both in one `hub_inbox` and called `hub_close [8,9]`: `closed [8,9]`, `refused []`.
- The user said "seems good".
- Caveat: the orchestrator's option offered children "no questions asked", and the user picked it, so this test did NOT exercise children asking the user directly.

## Left
1. When the user un-parks, first settle the Open questions with AskUserQuestion. Questions 1 and 2 can change what gets merged.
2. `git cherry-pick park/orchestrator` onto master.
   - Before that, check `hub_instances` and make sure no other instance in the project has uncommitted work in these files.
   - If master moved, resolve conflicts in `rules.rs`, `config.rs`, `mcp.rs`, `state_dir.rs`, `docs/hub.md` and `CLAUDE.md`.
3. Run `cargo test -p mulpex-core` and `cd src-tauri && cargo test --lib`. The spawn-prompt tests in `rules.rs` and `src-tauri/src/pty.rs` only check the sentinel, single-line form, `claude#2` and that the task is intact, so they pass with the new text.
4. Have the user QA in `npm run tauri dev` (it uses `~/.mulpex-dev`): ⌘T → `/orchestrator <task>`. This needs the user's own approval, since the post-test rule was never run. Check:
   - (a) no "children skip questions" option is offered;
   - (b) children ask the user directly (red row);
   - (c) a child's row actually closes after its "done" line (see Traps).
5. Ask the user before committing or releasing (`npm run release`).
6. Part 2, "the QA part" (not started): a QA step plus a worktree per child. Plan it with the user using AskUserQuestion and incremental phases.

## Decisions
- **How it starts:** the user types `/orchestrator [task]` in a ⌘T claude, and that claude becomes the orchestrator. Rejected: the skill spawning a new row.
- **Rule scope:** the incremental-phases rule goes into `PLANNING_RULES` for every Mulpex claude, not only orchestrated ones. The coworker was the stated reason.
- **No-work rule:** prompt-only. No PreToolUse block, consistent with the shared-tree guardrail.
- **Child brief:** changed for ALL hub_spawn children (the user picked "Change it for all spawns"). A per-call `interactive` flag was rejected. The conflict with cloudraw's autonomous workers was noticed only afterwards.
- **Report back:** a child sends one short line when done, not a summary, to keep the orchestrator's context small.
- **Before spawning:** the orchestrator always confirms the split first, and briefs are self-contained.
- **Closing:** done children are closed automatically, with no asking. The user's reasoning: once the user has QA'd with the child, there is no reason to nag about closing.
- **Phasing:** shipped as one phase (the user's choice).
- **Deferred:** worktrees/QA. The user: "it's belong to the later part - the QA part". This implies part 1 merges first, and the shared tree with its automatic file locks is accepted for part 1.
- **Parking:** on a branch rather than as a patch file or left uncommitted (the user's choice), so the next release can't ship it by accident.

## Traps
- **Restart needed:** running claudes pick up new rules and skills only after a restart (⌘⇧R), because the plugin and system prompt are read at spawn.
- **Skill name:** the skill shows up as `/mulpex:orchestrator`; plain `/orchestrator` resolves to it.
- **Closing can be refused:** `hub_close` refuses a mid-turn instance (`force: false` by default), and a child sends "done" from inside its own turn.
  - In the live test the closes succeeded. Most likely the children's turns had already ended: each child's last step after `hub_send` was a short reply, and the listener plus the orchestrator's own wake add delay. This was not measured.
  - If a close is refused, the skill says "retry on your next wake", but that child will not message again. The row can be left behind until something else wakes the orchestrator.
  - Possible fixes, none decided: tell the orchestrator to re-check with `hub_instances` before ending its turn, or close with `force` once the "done" line has arrived.
- **Flaky tests:** 3 tests in `crates/mulpex-core/src/persist.rs` (`save_load_round_trips_ids_names_and_mute`, `load_reads_pre_mute_name_lines`, `load_reads_legacy_bare_uuid_lines`) failed in about 1 of 3 full runs and pass alone. They look flaky and are unrelated to this work.
- **zsh:** `F="a b"; git add $F` does not word-split. Use an array.
- **Shared tree:** all instances of a project share one working tree. Never stash, checkout or reset without checking `hub_instances` and asking.
- **Autonomous setups break:** changing the default child brief affects orchestrator-style setups that want autonomous workers, like cloudraw. Their briefs would have to override it explicitly, and the new skill rule forbids `/orchestrator` from doing that unless the user asks.

## How to verify
- `git show --stat park/orchestrator` shows 6 files, +105/-20.
- At the time, `cargo test -p mulpex-core` (125 tests, apart from the flaky persist ones) and `cd src-tauri && cargo test --lib` (107 pass, 9 ignored) were green.
- After a dev launch, `<state_dir>/plugin/skills/orchestrator/SKILL.md` exists.
- Live QA follows Left step 4. Ground truth for what a child received is its session `.jsonl` transcript, not the pane.

## Open questions
1. **Every spawn or only some?** Should the "children work with the user" default apply to every `hub_spawn`, given that setups like cloudraw#9 run deliberately autonomous workers? The user approved "all spawns" before this conflict was raised. Re-ask before cherry-picking. The alternative is a per-call flag, which was rejected at the time.
2. **Remote claudes:** should `hub_remote_open` (the `HUB_RULES` REMOTE CLAUDES paragraph and the tool description) get the same treatment? It currently tells the remote claude to work autonomously and signal the spawner when it needs an answer. A remote claude lives in a terminal on another machine, so routing its questions to the user directly may not fit.
3. **Untested rule:** does the user accept the post-test "never switch off the child's rules" text without a new live QA? Their "seems good" came before it was written.
4. **Order of parts:** merge part 1 first, or build part 2 first? The user's "later part" wording suggests part 1 first, but this wasn't confirmed at un-park time. They also haven't said when to un-park.
5. **Refused closes:** should the orchestrator force-close or re-check after a "done" line, so no finished rows are left behind (see Traps)?
