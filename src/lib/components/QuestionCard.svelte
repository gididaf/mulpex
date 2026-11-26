<script lang="ts">
  // One async question card: AskUserQuestion's own dialog, drawn in the side
  // panel — options (radio or checkboxes), "Other", a preview beside the options
  // when any has one, per-question notes, one Submit for the whole card.
  import type { QCard } from "../ipc";
  import { answerCard, dismissCard, draftFor, forgetDraft, saveDraft, type QDraft } from "../questions";

  let { handle, card }: { handle: number; card: QCard } = $props();

  let qs: QDraft[] = $state([]);
  let lastRev = -1;
  $effect.pre(() => {
    if (card.rev !== lastRev) {
      lastRev = card.rev;
      qs = draftFor(handle, card).map((d) => ({ ...d, picks: [...d.picks] }));
    }
  });
  $effect(() => {
    saveDraft(handle, card, $state.snapshot(qs) as QDraft[]);
  });

  let busy = $state(false);

  const answered = (i: number) => {
    const d = qs[i];
    return !!d && (d.picks.length > 0 || (d.otherOn && d.other.trim() !== ""));
  };
  /** Each question's "Type something." box. */
  const boxes: (HTMLInputElement | null)[] = $state([]);
  const ready = $derived(card.questions.every((_, i) => answered(i)));

  // AskUserQuestion's rules: a single-answer question takes ONE option or the
  // typed answer, never both; a multi-answer one takes any options and/or it.
  function pick(i: number, label: string) {
    const d = qs[i];
    if (card.questions[i].multiSelect) {
      d.picks = d.picks.includes(label) ? d.picks.filter((p) => p !== label) : [...d.picks, label];
    } else {
      d.picks = [label];
      d.otherOn = false;
      d.other = "";
    }
  }
  /** Typing in the "Type something." row selects it; emptying it unselects. */
  function typed(i: number, text: string) {
    const d = qs[i];
    d.other = text;
    d.otherOn = text.trim() !== "";
    if (d.otherOn && !card.questions[i].multiSelect) d.picks = [];
  }
  /** The row's own radio/checkbox: on → focus the box; off (multi) → clear it. */
  function toggleOther(i: number, box: HTMLInputElement | null) {
    const d = qs[i];
    if (card.questions[i].multiSelect && d.otherOn) {
      d.otherOn = false;
      d.other = "";
      return;
    }
    box?.focus();
  }

  async function submit() {
    if (!ready || busy) return;
    busy = true;
    const answers = qs.map((d) => ({
      picks: d.picks,
      other: d.otherOn ? d.other : undefined,
      notes: d.notes.trim() || undefined,
    }));
    if (await answerCard(handle, card, answers)) forgetDraft(handle, card);
    busy = false;
  }
  async function dismiss() {
    busy = true;
    await dismissCard(handle, card);
    forgetDraft(handle, card);
    busy = false;
  }

  const previewOf = (i: number) => {
    const q = card.questions[i];
    const d = qs[i];
    const label = d?.shown ?? d?.picks[0] ?? q.options[0]?.label;
    return q.options.find((o) => o.label === label)?.preview ?? "";
  };
</script>

<div class="card">
  <div class="top">
    <span class="qid">{card.qid}</span>
    {#if card.rev > 1}<span class="edited" title="Claude changed this question">edited</span>{/if}
  </div>
  {#each card.questions as q, i (i)}
    {@const d = qs[i]}
    {@const hasPreview = q.options.some((o) => o.preview)}
    {#if d}
      <section class="q" dir={/[\u0590-\u05FF\u0600-\u06FF]/.test(q.question) ? "rtl" : "ltr"}>
        {#if q.header}<div class="hdr" dir="auto">{q.header}</div>{/if}
        <div class="text" dir="auto">{q.question}</div>
        <div class="body" class:split={hasPreview}>
          <div class="opts" role={q.multiSelect ? "group" : "radiogroup"}>
            {#each q.options as o (o.label)}
              <label
                class="opt"
                class:on={d.picks.includes(o.label)}
                onmouseenter={() => (d.shown = o.label)}
                onfocusin={() => (d.shown = o.label)}
              >
                <input
                  type={q.multiSelect ? "checkbox" : "radio"}
                  name="{card.qid}-{i}"
                  checked={d.picks.includes(o.label)}
                  onchange={() => pick(i, o.label)}
                />
                <span class="lbl">
                  <span class="l" dir="auto">{o.label}</span>
                  {#if o.description}<span class="desc" dir="auto">{o.description}</span>{/if}
                </span>
              </label>
            {/each}
            <div class="opt custom" class:on={d.otherOn}>
              <input
                type={q.multiSelect ? "checkbox" : "radio"}
                name="{card.qid}-{i}"
                checked={d.otherOn}
                aria-label="Type something"
                onclick={(e) => {
                  e.preventDefault();
                  toggleOther(i, boxes[i]);
                }}
              />
              <input
                class="other"
                dir="auto"
                placeholder="Type something."
                bind:this={boxes[i]}
                value={d.other}
                oninput={(e) => typed(i, (e.currentTarget as HTMLInputElement).value)}
              />
            </div>
          </div>
          {#if hasPreview}
            <pre class="preview" dir="auto">{previewOf(i)}</pre>
          {/if}
        </div>
        <input class="notes" dir="auto" placeholder="Notes (optional)" bind:value={d.notes} />
      </section>
    {/if}
  {/each}
  <div class="actions">
    <button class="dismiss" disabled={busy} title="Close without answering — claude uses its own judgment" onclick={dismiss}>Dismiss</button>
    <button class="submit" disabled={!ready || busy} onclick={submit}>Submit</button>
  </div>
</div>

<style>
  .card {
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 0.5rem 0.6rem;
    background: var(--bg);
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .top {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    font-size: 0.7rem;
    color: var(--text-faint);
  }
  .qid {
    font-family: ui-monospace, monospace;
  }
  .edited {
    padding: 0 0.3rem;
    border-radius: 0.5rem;
    background: var(--border);
    color: var(--text-dim);
  }
  .q {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .hdr {
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--accent);
    font-weight: 700;
  }
  .text {
    font-size: 0.85rem;
    font-weight: 600;
    white-space: pre-wrap;
    unicode-bidi: plaintext;
  }
  .body.split {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1.2fr);
    gap: 0.5rem;
  }
  .opts {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }
  .opt {
    display: flex;
    gap: 0.4rem;
    align-items: flex-start;
    padding: 0.25rem 0.35rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  .opt:hover {
    background: var(--bg-elev);
  }
  .opt.on {
    background: color-mix(in srgb, var(--accent) 18%, transparent);
  }
  .opt input {
    margin-top: 0.15rem;
  }
  .lbl {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .l,
  .desc {
    unicode-bidi: plaintext;
  }
  .desc {
    color: var(--text-dim);
    font-size: 0.74rem;
  }
  .preview {
    margin: 0;
    padding: 0.4rem;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg-elev);
    font-size: 0.72rem;
    overflow: auto;
    max-height: 18rem;
    white-space: pre;
    unicode-bidi: plaintext;
  }
  .opt.custom {
    align-items: center;
    cursor: default;
  }
  .opt.custom input[type="radio"],
  .opt.custom input[type="checkbox"] {
    margin-top: 0;
  }
  .other,
  .notes {
    width: 100%;
    box-sizing: border-box;
    background: var(--bg-elev);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0.3rem 0.4rem;
    font: inherit;
    font-size: 0.78rem;
    unicode-bidi: plaintext;
  }
  .opt.custom .other {
    flex: 1;
    width: auto;
    min-width: 0;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.4rem;
  }
  .actions button {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0.25rem 0.7rem;
    font-size: 0.78rem;
    background: none;
    color: var(--text-dim);
  }
  .actions .submit {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--bg);
    font-weight: 600;
  }
  .actions button:disabled {
    opacity: 0.45;
  }
</style>
