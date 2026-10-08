# ⌘E Explain Selection

The user works in Hebrew and is not always following the technical detail. **Select the part you
don't understand in a claude's pane, press ⌘E**, and a side panel explains it in simple Hebrew. The
selection is only the target; the context is the whole conversation. A box under the answer takes
follow-up questions.

## History: why the last two designs went

- **The Explainer** (until 2026-09-28): a third column that summarized every turn through a
  headless Sonnet side conversation fed by the hooks. Gone; [verification-log.md](verification-log.md)
  keeps what was measured about it.
- **`/explain`** (2026-09-28 → 2026-10-08): a plugin skill, typed by ⌘E, that made the claude explain
  "my last response" in four fixed one-line parts. It did not help. "Last response" was often a
  monitor tick or a hub wake rather than the long answer the user meant, and four one-liners were too thin
  for a dense reply. The user asked for something they could *aim*. The skill is deleted, and
  `state_dir::write_state_dir` removes `plugin/skills/` from scratch dirs older builds wrote, or a
  claude would keep offering `/explain`. `promptbox.ts` stayed: ⌘K and Remote Control still read the
  input box with it.

## How it works

`App.svelte::explainSelection` → `terminals.selectionText` (the pin reader, `captureSelection` +
`plainText`, then clears the selection) → `lib/explain.ts::startExplain` → `explain_start` →
`src-tauri/src/explain.rs`.

- **A hidden fork, not a typed command.** `claude -p --model sonnet --resume <uuid> --fork-session
  --tools "" --setting-sources "" --strict-mcp-config`, in the project dir, with the env scrubbed the
  way ⌘S does it ([saves.md](saves.md)). The selection goes in the prompt (`explain_prompt.md`).
  Nothing is typed into the claude, so it works mid-turn and over an open question, and the
  conversation stays clean.
- **Streams.** `--output-format stream-json --verbose --include-partial-messages`; each `text_delta`
  becomes an `explain-progress {handle, id, reqId, kind: delta|done|error, text}` event. The
  frontend's `reqId` drops late events from a replaced request. **A resumed fork prints a stale
  `result` line first** (the old conversation's, `num_turns: 0`), so only the last one counts.
- **Gated on the transcript file**, not `worked`: `Core::explain_target` checks
  `saves::transcript_path(dir, session_id).is_file()`. A conversation opened with an in-TUI
  `/resume` has its whole history on disk before anyone prompts it, while `worked` only turns on at
  the first prompt. Gating on `worked` was the first build's bug: "Nothing to explain yet" over a
  full resumed conversation.
- **Claudes only, a selection required.** A toast otherwise ("Select some text first", "Explain
  works on claudes only").

## Follow-ups resume the fork; they do not re-fork

Measured on a 233 k-token conversation (2026-10-08):

| | cache read | cache written | cost | time |
| --- | --- | --- | --- | --- |
| first ⌘E | 0 | 233 k | $0.92–0.95 | 12–22 s |
| follow-up as a **re-fork**, exchange carried in a longer prompt | 8 k | 226 k | $0.91 | 15 s |
| follow-up as **`--resume <fork>`** | 467 k | 1.5 k | $0.12 | 7 s |

The prompt cache is consulted at the end of the previous request, and a re-fork's last message is
never that, so it pays the whole conversation again. So the first ask **keeps** its fork's
transcript (no `--no-session-persistence`), its uuid is taken from the child's `system/init` line
(`init_session`; it equals the `.jsonl` name, measured), and a follow-up runs `--resume <fork>`
with only the question in its prompt.

**Mulpex deletes that fork itself** (`explain::discard`). Without that, every ⌘E would leave a
conversation in the project's `claude --resume` list. Deleted on: panel ✕/Esc, a new ⌘E on the same
instance, instance exit, project close (`dropExplains` → `explain_close`), and app teardown
(`discard_all`, in `lib.rs::teardown`). The fork slot carries the request id that created it, so a
replaced request's late `init` line is deleted instead of recorded (`note_fork`). A crash can leave
one stray fork. That is the known residue.

## The panel (`ExplainPanel.svelte`)

- **An overlay on the right of the pane, not a column.** Narrowing the terminal would resize every PTY
  in the workspace ([rendering.md](rendering.md), one geometry) on each open, close, and switch
  between an instance with a panel and one without.
- **Per instance, memory only.** Kept while you switch away; gone on quit, instance or project
  close. A new ⌘E replaces the thread.
- **Esc closes it only while focus is inside it.** In the terminal, Esc belongs to claude.
- **Markdown via `marked`, sanitized with `dompurify`** (`lib/markdown.ts`). The answer can quote
  anything the conversation held, and this webview can invoke Tauri commands. Links and images are
  dropped and their text kept: a link click would navigate the app's only window away.
- **Hard `dir="rtl"` on the body**, not `auto`. A line that opens with an English term
  (`**CRDT**: …`) must still read right-to-left. Inline `code` is `unicode-bidi: isolate` so an
  English snippet doesn't reorder the Hebrew around it. Unlike the terminal, the prompt *allows* a
  key English term with a short explanation, which the user asked for. The panel is HTML, so the
  terminal's first-strong-character row rule ([rendering.md](rendering.md)) does not apply.
