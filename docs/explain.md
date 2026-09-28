# `/explain` and ⌘E

The user works in Hebrew and is not always following the technical detail. `/explain` makes the
claude **itself** explain where things stand, in very simple Hebrew with no technical words, in its
own pane, as an ordinary turn. ⌘E types it for you.

It replaced the **Explainer** (2026-09-28): a third column that summarized every turn, pending
question and pending plan through a headless `claude -p --model sonnet` side conversation, fed by
the hooks writing `explainreq/<id>`. That design is gone — `explainer.rs`, `ExplainerPanel.svelte`,
⌘⇧E and the request dir were all deleted; git history has them, and
[verification-log.md](verification-log.md) keeps what was measured about them as history. What
replaced it is on demand, uses the real conversation instead of a transcript excerpt, and costs no
model call unless asked.

## The skill

`config::EXPLAIN_SKILL_MD`, written to `<state_dir>/plugin/skills/explain/SKILL.md` by
`state_dir::write_state_dir` — the same generated `--plugin-dir` plugin that carries the hub
listener monitor ([hub.md](hub.md)), rewritten before every spawn for the same three-day-fuse
reason. Nothing is installed. A claude picks the skill up at start, so an instance that was already
running gets it only after a restart (⌘⇧R).

- **The plugin is named `mulpex`**, and that name is the skill's namespace: Claude Code lists it as
  `/mulpex:explain (explain)`. Typing plain **`/explain` works** — Claude Code resolves it to
  `/mulpex:explain` (measured on 2.1.283, both typed and sent as one write with the Enter).
- **`disable-model-invocation: true`**: only the user runs it. A claude that decided on its own to
  explain itself would be noise.
- **No tools.** The skill tells the claude to explain only from what is already in the conversation.
- **Four fixed parts, Hebrew bold headings:** מה עשיתי / למה / מה אני צריך ממך (or "כלום כרגע") /
  איפה אנחנו. Words after the command set the focus, same structure.
- **No English at all, and every line starts with a Hebrew word.** That is a rendering rule, not
  taste: the pane picks each row's direction from its first strong character
  (`unicode-bidi: plaintext`, [rendering.md](rendering.md)), so a row opening with an English word
  flips to LTR, and English inside a Hebrew row reorders the words around it.

## ⌘E

Menu item `explain` (Session menu, `Cmd+E`, and in ⌘P), handled by `App.svelte::explainInstance`,
which writes `/explain\r` to the focused claude **in one `send_bytes`** — text and Enter together,
like Shift+Enter's two bytes. That is typing into the TUI, which [hub.md](hub.md) warns against for
anything long; nine characters is fine, and nothing longer should ever go this way.

Mid-turn is allowed: Claude Code queues it ("Press up to edit queued messages") and runs it when the
turn ends (measured). It is **refused, with a toast over the terminal** (`flashToast`, rendered by
`TerminalPane.svelte`), when typing would land somewhere else:

- **status `needs`** — a question or plan box is open, and `/explain` would be typed into it;
- **a draft in the input box** — the command would be glued onto the user's text;
- **no input box on screen** — claude still starting, or something unrecognized.

### Reading the input box (`promptbox.ts`)

There is no declared interface for "is the box empty", so it is read off the xterm buffer. Measured
on claude 2.1.283 (`tmux capture-pane -e`, then the same bytes replayed through `@xterm/headless`
5.5.0 — the build Mulpex uses — at idle, draft, multi-line draft, busy, busy with a draft, after a
turn, and with an `AskUserQuestion` open; all read correctly):

- The box is a **`❯` row with a `───` rule row directly above it**, closed by another rule row.
  Past prompts are echoed in the history as `❯` rows too, which is why the rule above is required
  and the scan runs bottom-up.
- The character after `❯` is **U+00A0 (NO-BREAK SPACE)**, not a space. Matching `"❯ "` found
  nothing at all — every state read as "none" until this was measured.
- **Everything claude puts in an empty box is dim (SGR 2)**: the `Try "…"` placeholder, the
  next-prompt suggestion, `Press up to edit queued messages`. Typed text is not. So the box is
  empty exactly when every cell after `❯` up to the closing rule is blank or dim.
- A question dialog replaces the box (its `❯ 1. Red` cursor row has no rule directly above), so it
  reads as "none" even without the `needs` check.

If a Claude Code update changes that drawing, the symptom is ⌘E refusing with "No prompt to type
into right now" — safe, visible, and the place to look.
