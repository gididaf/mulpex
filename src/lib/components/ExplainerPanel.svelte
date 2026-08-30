<!-- The Explainer: a persistent right-hand column showing the focused
     instance's per-turn Hebrew explanations, newest first (docs/explainer.md).
     A real grid column, not an overlay — toggling it (⌘⇧E) resizes the
     terminal pane, which the TerminalPane ResizeObserver propagates
     workspace-wide like any window resize. RTL is per-entry via dir="auto":
     Hebrew paragraphs read right-to-left while inline English identifiers sit
     correctly inside them. Nothing here touches the xterm CSS. -->
<script lang="ts">
  import {
    activeExplains,
    activeExplainBusy,
    sessions,
    activeId,
    showExplainer,
  } from "../stores";

  const cur = $derived($sessions.find((s) => s.id === $activeId) ?? null);
  const isShell = $derived(cur?.kind === "shell");
  const label = $derived(
    cur && !isShell ? (cur.name ?? `claude #${cur.id}`) : null,
  );

  function when(ts: number): string {
    return new Date(ts).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }
</script>

<aside class="explainer">
  <header>
    <span class="title">Explainer</span>
    {#if label}<span class="who">{label}</span>{/if}
    {#if $activeExplainBusy}<span class="busy" title="מסביר…"></span>{/if}
    <button
      class="x"
      onclick={() => showExplainer.set(false)}
      aria-label="Hide Explainer (⌘⇧E)">✕</button
    >
  </header>
  <div class="body">
    {#if cur && !isShell && $activeExplainBusy}
      <div class="working" dir="rtl">מסביר…</div>
    {/if}
    {#if !cur}
      <div class="empty">no session focused</div>
    {:else if isShell}
      <div class="empty">terminals aren't explained</div>
    {:else if $activeExplains.length === 0 && !$activeExplainBusy}
      <div class="empty">nothing yet — explanations appear after each turn</div>
    {:else}
      {#each $activeExplains as e, i (e.ts + "-" + i)}
        <article class:failed={!e.ok} class:question={e.kind === "question"}>
          <div class="ts">
            {when(e.ts)}{#if e.kind === "question"}<span class="qtag" dir="rtl"
                >שאלה</span
              >{/if}
          </div>
          <!-- Hard rtl, NOT dir="auto": auto resolves from the first strong
               character, and our prompt keeps English identifiers verbatim, so
               entries often *begin* with one — auto then lays the whole Hebrew
               paragraph out LTR (measured: scrambled line order, trailing period
               on the wrong side). The panel's language is Hebrew by contract;
               inline English sits correctly inside an RTL paragraph. -->
          <div class="text" dir="rtl">{e.text}</div>
        </article>
      {/each}
    {/if}
  </div>
</aside>

<style>
  .explainer {
    grid-area: explain;
    display: flex;
    flex-direction: column;
    min-height: 0;
    background: var(--bg-sidebar);
    border-left: 1px solid var(--border);
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 0.8rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    color: var(--label);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.72rem;
  }
  .who {
    color: var(--text-dim);
    font-size: 0.72rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .x {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--text-dim);
    font-size: 0.9rem;
    cursor: pointer;
  }
  /* The busy dot: amber like the sidebar's `working` dot — same vocabulary,
     "something is being produced for you". */
  .busy {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--dot-working);
    animation: explain-pulse 1.1s ease-in-out infinite;
  }
  .working {
    color: var(--text-faint);
    font-style: italic;
    font-size: 0.8rem;
    padding: 0.35rem 0;
    text-align: start;
    animation: explain-pulse 1.1s ease-in-out infinite;
  }
  @keyframes explain-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.3;
    }
  }
  .body {
    overflow-y: auto;
    min-height: 0;
    padding: 0.5rem 0.8rem;
  }
  article {
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--border);
  }
  article:last-child {
    border-bottom: none;
  }
  .ts {
    color: var(--text-faint);
    font-size: 0.7rem;
    margin-bottom: 0.3rem;
  }
  .text {
    /* dir="rtl" on the element (see the markup comment); text-align:start
       follows it, so text hugs the right edge. The taller line-height is for
       Hebrew at a small size. */
    text-align: start;
    white-space: pre-wrap;
    word-break: break-word;
    /* Two px above the house 0.82rem body size — Hebrew needs it at this width. */
    font-size: calc(0.82rem + 2px);
    line-height: 1.55;
    color: var(--text);
  }
  .failed .text {
    color: var(--text-faint);
    font-style: italic;
  }
  /* A question entry: the claude is waiting on the user right now — accent
     edge on the reading (right) side, matching the panel's RTL. */
  .question {
    border-right: 2px solid var(--accent);
    padding-right: 0.5rem;
  }
  .qtag {
    color: var(--accent);
    margin-left: 0.5rem;
    float: right;
  }
</style>
