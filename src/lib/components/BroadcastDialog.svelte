<script lang="ts">
  // ⌘⇧B Ask All Claudes: one question to every claude in every project of the
  // host in front (`broadcast.rs`, `lib/broadcast.ts`). Compose view while no
  // round is open; then a row per claude and the Sonnet summary. Closing keeps
  // the round going.
  import { broadcastTargets, type BroadcastRowState, type BroadcastTarget } from "../ipc";
  import { endBroadcast, noSummary, resumeBroadcast, rounds, sendBroadcast, summarize, summaries } from "../broadcast";
  import { hostName } from "../hosts";
  import { renderMarkdown, renderMarkdownAuto } from "../markdown";
  import { onMount } from "svelte";

  let {
    host,
    onclose,
    onfocus,
  }: { host: number; onclose: () => void; onfocus: (handle: number, id: number) => void } = $props();

  const key = (h: number, id: number) => `${h}:${id}`;

  let message = $state("");
  let targets = $state<BroadcastTarget[]>([]);
  let picked = $state<Set<string>>(new Set());
  let error = $state("");
  let busy = $state(false);
  let open = $state<Set<string>>(new Set());
  let sheet: HTMLDivElement | undefined = $state();
  let box: HTMLTextAreaElement | undefined = $state();

  const r = $derived($rounds.get(host) ?? null);
  const sum = $derived($summaries.get(host) ?? noSummary);

  async function loadTargets() {
    try {
      targets = await broadcastTargets(host);
    } catch (e) {
      targets = [];
      error = String(e);
    }
    picked = new Set(targets.map((t) => key(t.handle, t.id)));
  }

  onMount(() => {
    resumeBroadcast(host);
    void loadTargets();
  });
  $effect(() => {
    if (!r) box?.focus();
    else sheet?.focus();
  });

  function group<T extends { project: string }>(list: T[]) {
    const out: { project: string; rows: T[] }[] = [];
    for (const x of list) {
      const g = out.find((g) => g.project === x.project);
      if (g) g.rows.push(x);
      else out.push({ project: x.project, rows: [x] });
    }
    return out;
  }

  const allPicked = $derived(targets.length > 0 && picked.size === targets.length);

  function toggle(k: string) {
    const next = new Set(picked);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    picked = next;
  }

  function toggleAll() {
    picked = allPicked ? new Set() : new Set(targets.map((t) => key(t.handle, t.id)));
  }

  async function send() {
    if (busy) return;
    error = "";
    const list = targets.filter((t) => picked.has(key(t.handle, t.id))).map((t) => ({ handle: t.handle, id: t.id }));
    if (!message.trim()) return void (error = "Write a question first");
    if (list.length === 0) return void (error = "Pick at least one claude");
    busy = true;
    try {
      await sendBroadcast(host, message, list);
      open = new Set();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function newQuestion() {
    error = "";
    try {
      await endBroadcast(host);
    } catch (e) {
      return void (error = String(e));
    }
    message = "";
    await loadTargets();
  }

  function toggleOpen(k: string) {
    const next = new Set(open);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    open = next;
  }

  const LABEL: Record<BroadcastRowState, string> = {
    sent: "sent",
    busy: "working on it",
    read: "read…",
    replied: "replied",
    fallback: "replied · no tool",
    noanswer: "no answer",
    gone: "closed",
  };
  const FINAL = new Set<BroadcastRowState>(["replied", "fallback", "noanswer", "gone"]);
  const done = $derived(r ? r.rows.filter((x) => FINAL.has(x.state)).length : 0);
  const answers = $derived(r ? r.rows.filter((x) => x.reply != null).length : 0);
  // The summary is written in the question's language, but its bullets open with
  // an English `project · claude#N`, so per-block `dir="auto"` would read a Hebrew
  // summary as LTR. One direction for the whole thing, from the question's first letter.
  const summaryDir = $derived(/^[^A-Za-z֐-׿]*[֐-׿]/.test(r?.message ?? "") ? "rtl" : "ltr");

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !r) {
      e.preventDefault();
      void send();
    }
  }
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    bind:this={sheet}
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Ask All Claudes"
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <div class="head">
      <span class="label">Ask All Claudes{host ? ` · ${hostName(host)}` : ""}</span>
      {#if r}<span class="count" class:all={r.complete}>{done} / {r.rows.length} answered</span>{/if}
    </div>

    {#if !r}
      <textarea
        bind:this={box}
        bind:value={message}
        dir="auto"
        rows="4"
        placeholder="Ask every claude something… (⌘↩ to send)"
      ></textarea>
      {#if targets.length === 0}
        <p class="note">No claudes are running.</p>
      {:else}
        <label class="all">
          <input type="checkbox" checked={allPicked} onchange={toggleAll} />
          All claudes ({picked.size} / {targets.length})
        </label>
        <div class="list">
          {#each group(targets) as g (g.project)}
            <div class="proj">{g.project}</div>
            {#each g.rows as t (key(t.handle, t.id))}
              <label class="row pick">
                <input type="checkbox" checked={picked.has(key(t.handle, t.id))} onchange={() => toggle(key(t.handle, t.id))} />
                <span class="dot {t.status}"></span>
                <span class="id">claude#{t.id}</span>
                {#if t.name}<span class="name" dir="auto">{t.name}</span>{/if}
              </label>
            {/each}
          {/each}
        </div>
      {/if}
    {:else}
      <div class="question" dir="auto">{r.message}</div>
      <div class="bar"><div class="fill" style:width="{(done / Math.max(r.rows.length, 1)) * 100}%"></div></div>
      <div class="list">
        {#each group(r.rows) as g (g.project)}
          <div class="proj">{g.project}</div>
          {#each g.rows as x (key(x.handle, x.id))}
            {@const k = key(x.handle, x.id)}
            <div class="row">
              <button class="who" disabled={!x.reply} onclick={() => toggleOpen(k)}>
                <span class="caret">{x.reply ? (open.has(k) ? "▾" : "▸") : " "}</span>
                <span class="id">claude#{x.id}</span>
                {#if x.name}<span class="name" dir="auto">{x.name}</span>{/if}
              </button>
              <span class="state {x.state}">{LABEL[x.state as BroadcastRowState]}</span>
              <button class="go" title="Open this claude" onclick={() => onfocus(x.handle, x.id)}>↗</button>
            </div>
            {#if x.reply && open.has(k)}
              <div class="reply md">{@html renderMarkdownAuto(x.reply)}</div>
            {/if}
          {/each}
        {/each}
      </div>

      {#if sum.state !== "idle"}
        <div class="summary">
          <div class="sublabel">Summary{sum.state === "streaming" ? "…" : ""}</div>
          <!-- Sanitized in renderMarkdown. -->
          {#if sum.text}<div class="md" dir={summaryDir}>{@html renderMarkdown(sum.text)}</div>{/if}
          {#if sum.state === "error"}<p class="error">{sum.error}</p>{/if}
        </div>
      {/if}
    {/if}

    {#if error}<p class="error">{error}</p>{/if}

    <div class="actions">
      {#if !r}
        <button class="ghost" onclick={onclose}>Close</button>
        <button class="primary" disabled={busy || targets.length === 0} onclick={() => void send()}>
          {busy ? "Sending…" : "Send"}
        </button>
      {:else}
        <button class="ghost" onclick={() => void newQuestion()}>New question</button>
        <span class="spacer"></span>
        {#if !r.complete}
          <button disabled={answers === 0 || sum.state === "streaming"} onclick={() => void summarize(host)}>Summarize now</button>
        {:else if sum.state === "error" || sum.state === "done"}
          <button disabled={answers === 0} onclick={() => void summarize(host)}>Summarize again</button>
        {/if}
        <button class="primary" onclick={onclose}>Close</button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 70;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
  }
  .sheet {
    width: min(40rem, 92vw);
    max-height: 86vh;
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    padding: 1rem;
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 8px;
    font-size: 0.85rem;
    color: var(--text);
    outline: none;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
  }
  .label {
    flex: 1;
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .count {
    color: var(--text-dim);
    font-variant-numeric: tabular-nums;
  }
  .count.all {
    color: var(--dot-ready);
    font-weight: 600;
  }
  textarea {
    resize: vertical;
    padding: 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
  }
  textarea:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .question {
    padding: 0.45rem 0.6rem;
    border-left: 3px solid var(--accent);
    background: var(--bg);
    border-radius: 4px;
    white-space: pre-wrap;
    max-height: 6rem;
    overflow: auto;
  }
  .bar {
    height: 4px;
    background: rgba(255, 255, 255, 0.08);
    border-radius: 2px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--dot-ready);
    transition: width 0.25s ease;
  }
  .all {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--text-dim);
  }
  .list {
    flex: 1 1 auto;
    min-height: 3rem;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  .proj {
    margin-top: 0.35rem;
    color: var(--text-dim);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.15rem 0.25rem;
    border-radius: 4px;
  }
  .row:hover {
    background: rgba(255, 255, 255, 0.04);
  }
  .pick {
    cursor: pointer;
  }
  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--dot-ready);
    flex: none;
  }
  .dot.working {
    background: var(--dot-working);
  }
  .dot.needs {
    background: var(--dot-needs);
  }
  .id {
    font-family: ui-monospace, monospace;
  }
  .name {
    color: var(--text-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    cursor: pointer;
    padding: 0.35rem 0.8rem;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .who {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0;
    border: none;
    background: none;
    text-align: start;
  }
  .who:disabled {
    opacity: 1;
  }
  .caret {
    width: 0.8rem;
    color: var(--text-dim);
  }
  .state {
    color: var(--text-dim);
    font-size: 0.78rem;
    white-space: nowrap;
  }
  .state.replied,
  .state.fallback {
    color: var(--dot-ready);
  }
  .state.busy,
  .state.read {
    color: var(--dot-working);
  }
  .state.noanswer,
  .state.gone {
    color: var(--dot-needs);
  }
  .go {
    padding: 0 0.4rem;
    border: none;
    background: none;
    color: var(--text-dim);
  }
  .reply {
    margin: 0 0 0.3rem 1.5rem;
    padding: 0.4rem 0.6rem;
    background: var(--bg);
    border-radius: 4px;
    max-height: 14rem;
    overflow: auto;
  }
  .summary {
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--border);
    border-left: 3px solid var(--dot-ready);
    border-radius: 6px;
    max-height: 40vh;
    overflow: auto;
  }
  .sublabel {
    color: var(--label);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    margin-bottom: 0.3rem;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol) {
    margin: 0.25rem 0;
  }
  .md :global(ul),
  .md :global(ol) {
    padding-inline-start: 1.2rem;
  }
  .md :global(code) {
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
  }
  .md :global(pre) {
    overflow-x: auto;
  }
  .note {
    color: var(--text-dim);
  }
  .error {
    color: #e5534b;
    font-size: 0.8rem;
    margin: 0;
  }
  .actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 0.5rem;
  }
  .spacer {
    flex: 1;
  }
  .primary {
    border-color: var(--border-focus);
  }
  .ghost {
    background: none;
  }
</style>
