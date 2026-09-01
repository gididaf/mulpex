# The Explainer — the Hebrew turn-summary panel

A persistent right-hand column: after each claude turn ends — and while a question or a plan sits
on screen waiting for you — a headless Sonnet call produces a
very short, very simple **Hebrew** explanation of what that claude said (English identifiers
kept verbatim), and the panel shows a per-instance feed of them, **oldest first — newest at the bottom**,
the way the claude transcript beside it reads. Built 2026-08-30;
every claim below marked *measured* was driven on a real session or transcript that day.

## The pipeline

```
claude turn ends
  → Stop hook writes transcript_path → <state_dir>/explainreq/<id>      (hook.rs::write_explain_request)
  (a pending AskUserQuestion → explainq/<id>; a pending ExitPlanMode plan → explainplan/<id>)
  → 200ms poll consumes-and-deletes (state.rs::take_explain_requests, namereq-style)
  → explainer.rs worker queue (2 threads, per-instance latest-wins coalescing)
      read transcript JSONL → extract the turn's assistant text → claude -p --model sonnet
  → in-memory ExplainStore (per (handle,id), newest first, 50-entry cap)   ← storage order
  → `explain-update {handle, id, entry}` event → stores.ts::applyExplainFor → ExplainerPanel
```

- **The Stop hook only writes one small file.** It blocks the claude's turn end, so the Sonnet
  call must never run there; everything slow happens on `explainer.rs`'s own worker threads,
  and the poll loop only pays a dir read + a queue push.
- `EXPLAINREQ_DIR` and `explain_request_path` live in `mulpex-core`'s lib.rs next to the
  namereq constants — a contract between two processes, kept in one place. The subdir is in
  `write_state_dir`'s list (three-day `$TMPDIR` fuse) and holds bare-integer *files inside a
  subdir*, which is what keeps `mcp::live_ids`' root scan blind to them.
- A blocked Stop (unread hub mail) writes nothing — the turn continues; the eventual real Stop
  writes the request, so a turn is explained exactly once. A turn ending with background work
  still running IS explained (its text is complete; the later `<task-notification>` turn gets
  its own entry). *Measured through the real helper binary, both paths.*

## Questions are explained too (`AskUserQuestion`)

The `PreToolUse[AskUserQuestion]` matcher — which used to be an inline `printf needs` — now runs
`<helper> hook askq`: it still writes the `needs` status word (that job must never be lost), and
also hands the payload's `tool_input` (the `questions` array with options) to the app via
`explainq/<id>`. The poll drains it exactly like `explainreq/` (shared
`Core::drain_request_dir`), and the worker runs it under a **separate question prompt**: what is
being asked, one short Hebrew line per option, `(Recommended)` called out. Entries carry
`kind: "question"` and render with an accent edge + a "שאלה" tag, so the panel explains the
question **while it sits on screen waiting** — the turn's own explanation still arrives later
when the turn really ends.

Coalescing is **per kind**: a queued question job is never replaced by a turn job for the same
instance (or vice versa) — `std::mem::discriminant` on the job input. A payload with no usable
`questions` array is skipped with a log line, same no-silent-branch rule as turns.

## Plans are explained too (`ExitPlanMode`)

The `PreToolUse[ExitPlanMode]` matcher runs `<helper> hook plan`: it writes the `needs` status
word and hands the payload's `tool_input` to the app via `explainplan/<id>`, drained by the poll
loop through the same `Core::drain_request_dir`. Entries carry `kind: "plan"`, and the panel
gives them a **green** accent edge and a `תוכנית` tag — the green the sidebar uses for *ready*,
because the dialog literally asks "ready to code?", and deliberately not the amber the in-flight
dot owns.

**The prompt is the whole point of the feature.** A plan is long, structured and technical by
construction — headings, absolute paths, code fences — and reproducing any of that in the panel
would defeat it. `PLAN_PROMPT` demands **one sentence**: what he intends to do, no steps, no
lists, no file names. The user chose this shape over "goal + 3 bullets" and over the
question-mode line-per-section form.

Everything measured on a real `claude` v2.1.252 on a PTY, 2026-09-01 (`scratchpad/probe`):

- **`tool_input` is `{plan, planFilePath}`** — `plan` is the plan as markdown. `planFilePath`
  points at the same text under `~/.claude/plans/`; it is deliberately ignored (one source, and
  no file read on this path).
- **The hook fires ~6 s *before* the approval dialog paints** (hook 09:47:18.2, dialog +
  `Notification{permission_prompt}` 09:47:24.2). With the summarizer's 6–13 s that puts the
  Hebrew line on screen a few seconds after the plan — not half a minute.
- **`Stop` does not fire while the plan waits**, so a plan gets no turn explanation and there is
  no overlap to dedup. After you approve, the turn carries on and its eventual `Stop` is
  explained normally — the user chose to leave both rather than suppress the follow-up.
- **The `needs` write is redundant belt-and-braces.** `Notification{permission_prompt}` already
  becomes `needs` via `hook::notify_status`, 6 s later; writing it in the plan hook makes the
  dot immediate and survives a change to that notification type.
- **The plan is head-capped at 12 KB** (`MAX_PLAN_BYTES`), the opposite end from a turn's
  tail-cap: a plan opens with its goal and descends into detail, and only the opening matters
  to a one-line summary.

### Plan mode is not reachable the way you would guess

`--dangerously-skip-permissions` (which every Mulpex claude runs with) **silently overrides**
`--permission-mode plan`: every hook payload still reads `permission_mode: bypassPermissions`,
no `ExitPlanMode` is ever called, and Claude just writes a plan as prose. Plan mode is reached
**only by shift+tab**, four presses from bypass — measured cycle: bypass → auto → manual →
accept edits → **plan**. Two probe runs found nothing before this was understood; if you are
driving a plan on a PTY, this is the trap.

## Why a dedicated event, not a HubSnapshot field

`HubSnapshot` is rebuilt and PartialEq-compared for every project every 200ms. A growing feed of
Hebrew paragraphs would inflate every idle compare, and any new entry would re-emit the entire
snapshot including all history. Summaries also arrive asynchronously, seconds after the turn —
the worker already knows exactly what changed, so it emits `explain-update` itself. The
`get_explains` command exists only for the initial paint (bootstrap / dev hot-reload).

A second event, **`explain-pending {handle, id, active}`**, drives the panel's busy indicator
(pulsing amber dot + "מסביר…" line while a summary is being produced for the focused instance).
It fires on 0↔1+ transitions of a per-instance job **count** — a count, not a bool, because a
running job plus a freshly queued one must not go idle when only the first finishes. Every path
through a job (summary, failure entry, skip) decrements it; `forget` clears it, and a late
finish after forget stays silent. Not persisted and not in `get_explains` — after a dev
hot-reload an in-flight job just shows no dot, which is the acceptable direction of error.

## The turn-end race (the expensive lesson)

**Claude Code appends the turn's final assistant entry to the transcript *around* the moment the
Stop hook fires — sometimes after.** Measured: final text entry at 11:55:15.346, Stop status
write in the same second; the first read then sees the turn's boundary and tool traffic but no
text. The first implementation skipped an empty extraction *silently*, which presented as "the
whole feature doesn't work" and cost a debugging round — the classic "a real event with nowhere
to arrive". Two rules came out of it:

- `extract_turn_text_settled`: wait 400ms, then retry while the turn reads as empty (~2.5s
  total). Still empty after that → a genuinely textless turn (interrupt, tool-calls-only) →
  skipped **with a log line**. No silent branch anywhere in `process()`.
- The retry is only for *empty* results. A partial read (mid-turn text present, final message
  still in flight) is theoretically possible in that same sub-second window; the initial 400ms
  delay is what shrinks it to noise.

## Turn extraction (measured transcript shapes)

Boundary = the **last real human prompt**: a `type:"user"` entry, `isMeta`/`isSidechain` absent,
whose content is a string or a text/image list with **no `tool_result` block** (tool results
arrive as `type:"user"`!). Then take every `text` block of every non-sidechain `assistant` entry
after it; join; keep the ~24KB **tail** (conclusions live at the end), `[…truncated…]` marker
when cut.

- A `<command-name>` entry (`/clear`, `/sync-docs`…) is a plain string user entry, **not**
  `isMeta` — and it *is* a valid boundary: a slash command that runs a real turn starts it, and
  keeping it as one is what stops the previous turn's text leaking in. Pinned by test.
- `<task-notification>` turns arrive as plain string user entries and count as boundaries —
  a notification-triggered turn gets its own explanation. Deliberate.
- Transcript noise (`attachment`, `ai-title`, `mode`, `cost-state`…) falls through the filters.

## The summarizer child

```
<resolve_claude()> -p --setting-sources "" --model sonnet \
    --no-session-persistence --tools "" --strict-mcp-config \
    --system-prompt "<HEBREW_PROMPT>"          # turn text via stdin
```

- **NOT `--bare`**: measured, `--bare -p` cannot see the subscription OAuth token and dies with
  "Not logged in". `--setting-sources ""` is the working equivalent (skips user/project
  settings, hooks, plugins); `--tools ""` + `--strict-mcp-config` = no tools, no MCP;
  `--no-session-persistence` leaves nothing in `~/.claude/projects` (verified).
- `env_clear()` + `claude_bin::forwarded_env()` + `PATH = merged_path()`: the deny-list strips
  `MULPEX_*` / `CLAUDE_CODE_CHILD_SESSION` / `CLAUDE_CODE_ENTRYPOINT`, so the child has no hub
  identity — and under `--setting-sources ""` it runs no hooks anyway. cwd = the project's
  scratch dir (neutral; no CLAUDE.md pickup).
- Both pipes are drained on their own threads **before** the timeout wait — a child blocked on
  a full pipe while the parent only `try_wait`s reads as a timeout (the classic self-inflicted
  deadlock). Timeout 90s → kill + `wait()` (reap) + failure entry.
- Measured latency 6–13s per turn; three concurrent calls fine.
- Failures become `ExplainEntry { ok: false, text: "ההסבר נכשל (exit N / timeout / …)" }` —
  rendered dim, never retried, never silently dropped. Say what you know.

## Decisions (confirmed with the user)

| Decision | Choice | Why |
| --- | --- | --- |
| Scope | every claude, every project, every turn | switching rows always shows fresh feed |
| Muted claudes | still explained | mute is presentational; skipping punches holes in history |
| Terminals | never | shells run no hooks — structurally impossible, and `take_explain_requests` double-checks |
| Persistence | in-memory, per run | the transcript is the archive; the panel is a glance |
| Instance exits / project closes | feed dropped (`forget` / `forget_project`) | a dead row is unreachable in the UI |
| Feed cap | 50/instance, backend-capped | bounded memory, no UI jank |
| Panel | real third grid column, visible by default, ⌘⇧E toggles | toggling refits every PTY workspace-wide — same class as a window resize (one geometry) |
| Empty turn | skip, log, no Sonnet call | a call on nothing would invent something |
| Plan explanation | ONE sentence, the goal only | a plan is all technical detail; the terminal beside it holds the detail |
| Plan + the turn that follows | both explained, no dedup | the post-approval turn is usually the part worth reading |
| Plan attention | `needs` written by the hook too | immediate dot, independent of the notification type |
| Feed order | oldest first, newest at the bottom, chat-sticky scroll | aligns with the claude transcript beside it |
| Old entries | dimmed to 0.18, full opacity on panel hover (newest always full) | the panel is a glance; the backlog stays reachable |

## Reading order and the dimming (the panel's own UI rules)

- **Newest is at the bottom.** The store stays newest-first — its 50-entry cap is a
  prepend-and-truncate, and `applyExplainFor` prepends — so the reversal is **render-time only**
  (`ordered = [...$activeExplains].reverse()` in `ExplainerPanel.svelte`). Don't "fix" this by
  flipping the store: the cap would then have to truncate the head.
  Side benefit: in the oldest-first array an existing entry's index no longer shifts when a new
  one arrives, so the `{#each}` key (`ts + "-" + i`) is finally stable and old rows stop being
  recreated on every update.
- **The "מסביר…" line moved to the bottom too**, where the entry it is producing will land. It is
  the `.body`'s last *child*, which is why the newest-entry rule is `article:last-of-type` and
  not `:last-child`.
- **Scroll is chat-sticky:** follow the newest entry only while the user is parked at the bottom
  (`scrollHeight - scrollTop - clientHeight < 24`, slack for sub-pixel positions and the busy
  line). Scrolled up to read history → stay put. `stuck` is a plain `let`, **not** `$state`: the
  scroll effect must not re-run just because it flipped. Switching rows resets it — a different
  feed always opens at its newest end.
- **History recedes.** Every entry sits at `opacity: 0.18`; the newest one, and *all* of them
  while the pointer is over the panel (`.explainer:hover article`), read at full strength. The
  panel is a glance at what the claude just said — the backlog is there when you go looking for
  it, and quiet the rest of the time.

## RTL in the panel

The entry text is **hard `dir="rtl"`, not `dir="auto"`** — auto resolves from the first strong
character, and the prompt's own rules keep English identifiers verbatim, so entries often *begin*
with one; auto then lays the whole Hebrew paragraph out LTR (measured: scrambled line order,
period on the wrong side). Inline English sits correctly inside an RTL paragraph. None of this
touches the xterm CSS — the `.xterm-rows span` rule stays sacred (docs/rendering.md).

## The Hebrew prompt

Lives as `HEBREW_PROMPT` in `explainer.rs` (turns), `QUESTION_PROMPT` (pending questions) and
`PLAN_PROMPT` (pending plans — see above; it is the strictest of the three, one sentence):
one to three sentences of dead-simple Hebrew, English terms verbatim, no preamble/headers/
bullets, open with what the claude needs from the user when it's waiting on a decision, state
failures directly, add nothing not in the text. Tone approved by the user on four real turns
(mulpex + dreamvps samples) before any code was written.

**All three speak in the first person, as the claude himself** (2026-09-01): "בדקתי… ועכשיו אני
צריך ממך לאשר", never "הוא בדק". Third person had a second failure mode beyond the tone the user
disliked: with nobody pinned to "אני", the summarizer regularly handed the *user* the claude's own
work — "אתה ממתין לשני agents נוספים" — which is exactly backwards. The prompts therefore pin both
pronouns at once: **אני = the claude, אתה/ממך/שלך = the human, always**, plus "גוף ראשון יחיד" so
Sonnet doesn't drift into the editorial "נציג/נעשה".

The person change had to be a *minimal* diff. A first draft opened with "תסכם… כאילו Claude עצמו
מספר מה עשה", and on a real 1.3 KB turn Sonnet answered with five paragraphs where the old prompt
gave one — "מספר מה עשה" reads as retell, not summarize. The shipped version keeps the original two
opening lines verbatim, adds the person as a clause plus two rules, and re-states brevity as
"זה סיכום, לא שכתוב: משפט אחד עד שלושה, רק העיקר. פסקה אחת". Re-measured on three real
dreamvps turns + a real `AskUserQuestion` payload + a real plan: one paragraph each, first person
throughout, options still one-per-`- ` line, `(מומלץ)` still only where the input marked it.

Two hardenings, both measured on real multi-question output and re-verified after the fix:

- **"No markdown symbols" must be explicit.** The panel renders plain text; without the rule,
  Sonnet wrapped question headers in `**…**` and the user saw literal asterisks.
- **The Recommended rule must be one-directional.** "Point out the recommended option" alone
  made Sonnet *invent* a `(מומלץ)` on an unmarked option — the rule now says mark only an
  explicit `(Recommended)`, and never hint at a preference otherwise. Verified both ways: no
  marker → zero recommendation language; marked input → exactly that option flagged.
