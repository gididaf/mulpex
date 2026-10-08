<script lang="ts">
  // A claude's open question or plan, as buttons (the Mac's
  // `remote/dialog.rs` turns the answer into the keys claude's dialog takes).
  import { render } from "./markdown";

  export type Question = {
    question: string;
    header: string;
    multi: boolean;
    options: { label: string; description: string }[];
  };
  export type DialogView = { kind: "ask"; questions: Question[] } | { kind: "plan"; plan: string };
  type Result = { ok: boolean; why: string | null };

  let { dialog, onanswer }: { dialog: DialogView; onanswer: (answer: unknown) => Promise<Result> } =
    $props();

  // One entry per question: picked option indexes, and the typed answer.
  let picks = $state<number[][]>([]);
  let others = $state<string[]>([]);
  let feedback = $state("");
  let sending = $state(false);
  let error = $state<string | null>(null);
  let planOpen = $state(false);

  // A new dialog starts clean. Keyed on its content, not the object: the Mac
  // re-sends the whole view whenever anything changes (a ctx %, another row),
  // and that must not wipe picks made halfway through.
  const identity = $derived(JSON.stringify(dialog));
  $effect(() => {
    void identity;
    const d = JSON.parse(identity) as DialogView;
    const n = d.kind === "ask" ? d.questions.length : 0;
    picks = Array.from({ length: n }, () => []);
    others = Array.from({ length: n }, () => "");
    feedback = "";
    error = null;
    planOpen = false;
  });

  function pick(qi: number, oi: number, multi: boolean) {
    const cur = picks[qi] ?? [];
    if (multi) picks[qi] = cur.includes(oi) ? cur.filter((x) => x !== oi) : [...cur, oi];
    else {
      picks[qi] = [oi];
      others[qi] = "";
    }
  }

  function typed(qi: number, multi: boolean) {
    // In a single choice, a typed answer replaces the picked one.
    if (!multi && others[qi].trim()) picks[qi] = [];
  }

  const ready = $derived(
    dialog.kind === "ask" &&
      dialog.questions.every((_, i) => (picks[i]?.length ?? 0) > 0 || (others[i] ?? "").trim() !== ""),
  );

  async function send(answer: unknown) {
    if (sending) return;
    sending = true;
    error = null;
    const r = await onanswer(answer);
    sending = false;
    if (!r.ok) error = r.why ?? "Not sent";
  }

  function submitAsk() {
    if (dialog.kind !== "ask") return;
    send({
      answers: dialog.questions.map((_, i) => ({
        pick: picks[i] ?? [],
        other: (others[i] ?? "").trim() || null,
      })),
    });
  }
</script>

<div class="card">
  {#if dialog.kind === "ask"}
    {#each dialog.questions as q, qi (qi)}
      <div class="q">
        {#if q.header}<span class="chip" dir="auto">{q.header}</span>{/if}
        <div class="question" dir="auto">{q.question}</div>
        {#if q.multi}<div class="hint">Pick any</div>{/if}
        {#each q.options as o, oi (oi)}
          <button class="opt" class:on={picks[qi]?.includes(oi)} onclick={() => pick(qi, oi, q.multi)}>
            <span class="box">{q.multi ? (picks[qi]?.includes(oi) ? "☑" : "☐") : picks[qi]?.includes(oi) ? "●" : "○"}</span>
            <span class="txt">
              <span class="label" dir="auto">{o.label}</span>
              {#if o.description}<span class="desc" dir="auto">{o.description}</span>{/if}
            </span>
          </button>
        {/each}
        <input
          class="other"
          dir="auto"
          placeholder="Something else…"
          bind:value={others[qi]}
          oninput={() => typed(qi, q.multi)}
        />
      </div>
    {/each}
    <button class="primary" disabled={!ready || sending} onclick={submitAsk}>
      {sending ? "Sending…" : "Answer"}
    </button>
  {:else}
    <div class="question">Claude has a plan ready</div>
    <button class="toggle" onclick={() => (planOpen = !planOpen)}>{planOpen ? "Hide plan" : "Read the plan"}</button>
    {#if planOpen}<div class="plan">{@html render(dialog.plan)}</div>{/if}
    <div class="row">
      <button class="primary" disabled={sending} onclick={() => send({ choice: 1 })}>Approve</button>
      <button class="secondary" disabled={sending} onclick={() => send({ choice: 2 })}>Approve, review edits</button>
    </div>
    <div class="row">
      <input class="other" dir="auto" placeholder="Or tell Claude what to change…" bind:value={feedback} />
      <button class="secondary" disabled={!feedback.trim() || sending} onclick={() => send({ choice: 3, text: feedback })}>
        Send
      </button>
    </div>
  {/if}
  {#if error}<div class="error" dir="auto">{error}</div>{/if}
</div>

<style>
  .card {
    border-top: 2px solid var(--red);
    background: var(--bg-elev);
    padding: 0.6rem 0.75rem;
    max-height: 60dvh;
    overflow-y: auto;
  }
  .q {
    margin-bottom: 0.7rem;
  }
  .chip {
    display: inline-block;
    font-size: 0.7rem;
    padding: 0.05rem 0.45rem;
    border-radius: 999px;
    background: var(--border);
    color: var(--text);
    margin-bottom: 0.25rem;
  }
  .question {
    font-weight: 600;
    margin-bottom: 0.4rem;
    unicode-bidi: plaintext;
  }
  .hint {
    font-size: 0.75rem;
    color: var(--label);
    margin: -0.25rem 0 0.35rem;
  }
  .opt {
    display: flex;
    gap: 0.55rem;
    width: 100%;
    text-align: start;
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.3rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
  }
  .opt.on {
    border-color: var(--accent);
  }
  .box {
    color: var(--accent);
  }
  .txt {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .label {
    unicode-bidi: plaintext;
  }
  .desc {
    font-size: 0.78rem;
    color: var(--label);
    unicode-bidi: plaintext;
  }
  .other {
    flex: 1;
    width: 100%;
    box-sizing: border-box;
    min-width: 0;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 16px;
    unicode-bidi: plaintext;
  }
  .row {
    display: flex;
    gap: 0.4rem;
    margin-top: 0.45rem;
  }
  .primary,
  .secondary,
  .toggle {
    padding: 0.55rem 0.8rem;
    border-radius: 8px;
    font: inherit;
    font-weight: 600;
  }
  .primary {
    border: 0;
    background: var(--accent);
    color: #fff;
  }
  .q ~ .primary {
    width: 100%;
  }
  .secondary,
  .toggle {
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
  }
  .toggle {
    font-weight: 400;
    font-size: 0.85rem;
    padding: 0.3rem 0.6rem;
  }
  button:disabled {
    opacity: 0.45;
  }
  .plan {
    margin-top: 0.5rem;
    padding: 0.4rem 0.6rem;
    border-radius: 8px;
    background: var(--bg);
    max-height: 35dvh;
    overflow-y: auto;
    font-size: 0.88rem;
  }
  .plan :global(p) {
    margin: 0.3rem 0;
    white-space: pre-wrap;
    unicode-bidi: plaintext;
  }
  .plan :global(pre) {
    overflow-x: auto;
    font-size: 0.78rem;
  }
  .error {
    color: var(--red);
    font-size: 0.8rem;
    margin-top: 0.4rem;
    unicode-bidi: plaintext;
  }
</style>
