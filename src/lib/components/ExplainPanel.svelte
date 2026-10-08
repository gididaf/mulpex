<script lang="ts">
  import { tick } from "svelte";
  import { explains, explainKey, closeExplain, askFollowup, isStreaming } from "../explain";
  import { terminals } from "../terminals";
  import { renderMarkdown } from "../markdown";

  // ⌘E: the active claude's explanation, floating over the right of its pane.
  // An overlay, not a column: narrowing the terminal would resize every PTY in
  // the workspace (one geometry — docs/rendering.md) each time it opened, closed,
  // or you switched to an instance without a panel.
  let { handle, id }: { handle: number; id: number } = $props();

  const ex = $derived($explains.get(explainKey(handle, id)));
  const busy = $derived(ex ? isStreaming(ex) : false);

  let draft = $state("");
  let bodyEl: HTMLDivElement | undefined = $state();

  // Follow the answer down while it streams — unless the user scrolled up to
  // read something earlier.
  let pinned = true;
  function onscroll() {
    if (bodyEl) pinned = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 40;
  }
  $effect(() => {
    void ex?.turns.at(-1)?.text;
    if (pinned) tick().then(() => bodyEl?.scrollTo({ top: bodyEl.scrollHeight }));
  });

  function ask() {
    if (busy || !draft.trim()) return;
    askFollowup(handle, id, draft);
    draft = "";
    pinned = true;
  }

  // Enter sends, Shift+Enter is a new line; not while an IME is composing.
  function onDraftKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      ask();
    }
  }

  function close() {
    closeExplain(handle, id);
    terminals.refocus();
  }

  // Esc only while focus is in the panel: in the terminal, Esc belongs to claude.
  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  }
</script>

{#if ex}
  <div class="panel" role="dialog" tabindex="-1" {onkeydown} aria-label="Explanation">
    <header>
      <span class="title">Explain</span>
      <button class="x" title="Close (Esc)" onclick={close}>✕</button>
    </header>
    <blockquote dir="auto">{ex.quote}</blockquote>
    <div class="body" dir="rtl" bind:this={bodyEl} {onscroll}>
      {#each ex.turns as t, i (i)}
        {#if t.question}<div class="q" dir="auto">{t.question}</div>{/if}
        <!-- Sanitized in renderMarkdown. -->
        {#if t.text}<div class="text">{@html renderMarkdown(t.text)}</div>{/if}
        {#if t.state === "streaming" && !t.text}
          <div class="wait">{i === 0 ? "קורא את השיחה…" : "חושב…"}</div>
        {/if}
        {#if t.state === "error"}
          <div class="err" dir="auto">{t.error}</div>
        {/if}
      {/each}
    </div>
    <div class="ask">
      <textarea
        dir="auto"
        rows="2"
        placeholder="שאלה נוספת…"
        bind:value={draft}
        onkeydown={onDraftKey}
      ></textarea>
      <button class="send" disabled={busy || !draft.trim()} onclick={ask} title="Ask (Enter)">שאל</button>
    </div>
  </div>
{/if}

<style>
  .panel {
    position: absolute;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(40%, 34rem);
    min-width: min(20rem, 100%);
    z-index: 6;
    display: flex;
    flex-direction: column;
    background: var(--bg-elev);
    color: var(--text);
    border-left: 1px solid var(--border);
    box-shadow: -8px 0 24px rgba(0, 0, 0, 0.5);
    outline: none;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.45rem 0.6rem 0.45rem 0.9rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--text-dim);
  }
  .x {
    border: none;
    background: none;
    color: var(--text-dim);
    font-size: 0.9rem;
    padding: 0.1rem 0.35rem;
    border-radius: 4px;
  }
  .x:hover {
    background: var(--border);
    color: var(--text);
  }
  blockquote {
    margin: 0.7rem 0.9rem 0;
    padding: 0.4rem 0.6rem;
    max-height: 8rem;
    overflow-y: auto;
    border-inline-start: 3px solid var(--accent);
    background: var(--bg);
    color: var(--text-dim);
    font-size: 0.78rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .body {
    flex: 1;
    overflow-y: auto;
    padding: 0.8rem 0.9rem 1rem;
    font-size: 0.92rem;
    line-height: 1.6;
  }
  .text {
    overflow-wrap: anywhere;
  }
  /* Hard RTL on the body, not `auto`: a line that opens with an English term
     ("**CRDT**: …") must still read right-to-left. Inline code is isolated so an
     English snippet keeps its own order without reordering the Hebrew around it. */
  .text :global(p) {
    margin: 0 0 0.7rem;
  }
  .text :global(ul),
  .text :global(ol) {
    margin: 0 0 0.7rem;
    padding-inline-start: 1.3rem;
  }
  .text :global(li) {
    margin-bottom: 0.3rem;
  }
  .text :global(h1),
  .text :global(h2),
  .text :global(h3),
  .text :global(h4) {
    margin: 0.9rem 0 0.4rem;
    font-size: 0.98rem;
  }
  .text :global(strong) {
    color: var(--label);
  }
  .text :global(code) {
    direction: ltr;
    unicode-bidi: isolate;
    padding: 0.05rem 0.3rem;
    border-radius: 3px;
    background: var(--bg);
    font-size: 0.84em;
  }
  .text :global(pre) {
    direction: ltr;
    overflow-x: auto;
    padding: 0.5rem 0.6rem;
    border-radius: 4px;
    background: var(--bg);
  }
  .text :global(pre code) {
    padding: 0;
    background: none;
  }
  .text :global(blockquote) {
    margin: 0 0 0.7rem;
    padding-inline-start: 0.6rem;
    border-inline-start: 2px solid var(--border);
    color: var(--text-dim);
  }
  .q {
    margin: 0.4rem 0 0.7rem;
    padding: 0.35rem 0.6rem;
    border-radius: 6px;
    background: var(--bg);
    color: var(--accent);
    font-size: 0.88rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .ask {
    display: flex;
    gap: 0.4rem;
    align-items: flex-end;
    padding: 0.55rem 0.7rem;
    border-top: 1px solid var(--border);
  }
  textarea {
    flex: 1;
    resize: none;
    padding: 0.4rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 0.88rem;
  }
  textarea:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .send {
    padding: 0.4rem 0.8rem;
    border: 1px solid var(--accent);
    border-radius: 5px;
    background: none;
    color: var(--text);
    font-size: 0.85rem;
  }
  .send:disabled {
    opacity: 0.4;
    border-color: var(--border);
  }
  .send:not(:disabled):hover {
    background: var(--border);
  }
  .wait {
    color: var(--dot-working);
    font-size: 0.85rem;
  }
  .err {
    margin-top: 0.5rem;
    color: var(--warn, #e0a34a);
    font-size: 0.82rem;
    overflow-wrap: anywhere;
  }
</style>
