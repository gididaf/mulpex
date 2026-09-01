<!-- The Explainer: a persistent right-hand column showing the focused
     instance's per-turn Hebrew explanations, oldest first — the newest entry is
     at the BOTTOM, so the feed grows downward the way the claude transcript
     beside it does. The backend store stays newest-first (its 50-entry cap is
     a prepend-and-truncate); the reversal is render-time only.
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
  // Newest last. `$activeExplains` is a fresh array per update (applyExplainFor
  // rebuilds it), but reverse() mutates — copy first.
  const ordered = $derived([...$activeExplains].reverse());

  let bodyEl = $state<HTMLDivElement | null>(null);
  // Chat-style stickiness: follow the newest entry only while the user is
  // already parked at the bottom. Deliberately NOT $state — the scroll effect
  // must not re-run just because this flipped.
  let stuck = true;

  function onScroll() {
    if (!bodyEl) return;
    // Slack for sub-pixel scroll positions and the in-flight "מסביר…" line.
    stuck = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 24;
  }

  // Switching rows shows a different feed from scratch — always start at its
  // newest end, whatever the scroll position on the previous row was.
  $effect(() => {
    void cur?.id;
    stuck = true;
  });

  // Runs after the DOM has the new entry, so scrollHeight is already grown.
  $effect(() => {
    void ordered.length;
    void $activeExplainBusy;
    const el = bodyEl;
    if (!el || !stuck) return;
    el.scrollTop = el.scrollHeight;
  });

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
  <div class="body" bind:this={bodyEl} onscroll={onScroll}>
    {#if !cur}
      <div class="empty">no session focused</div>
    {:else if isShell}
      <div class="empty">terminals aren't explained</div>
    {:else if ordered.length === 0 && !$activeExplainBusy}
      <div class="empty">nothing yet — explanations appear after each turn</div>
    {:else}
      {#each ordered as e, i (e.ts + "-" + i)}
        <article
          class:failed={!e.ok}
          class:question={e.kind === "question"}
          class:plan={e.kind === "plan"}
        >
          <div class="ts">
            {when(e.ts)}{#if e.kind === "question"}<span class="tag" dir="rtl"
                >שאלה</span
              >{:else if e.kind === "plan"}<span class="tag" dir="rtl">תוכנית</span
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
    <!-- The in-flight line lives at the bottom, where the entry it is producing
         will land. -->
    {#if cur && !isShell && $activeExplainBusy}
      <div class="working" dir="rtl">מסביר…</div>
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
    /* History recedes: only the newest entry (the last one, see below) reads at
       full strength until the pointer enters the panel. */
    opacity: 0.18;
    transition: opacity 0.15s ease;
  }
  /* :last-of-type, not :last-child — the "מסביר…" line is the last child while
     a summary is in flight. */
  article:last-of-type,
  .explainer:hover article {
    opacity: 1;
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
  /* A question or a plan entry: the claude is waiting on the user right now —
     accent edge on the reading (right) side, matching the panel's RTL. The
     colour is what separates them at a glance: cyan is a question to answer,
     green a finished plan waiting for "yes, execute" (the same green the
     sidebar uses for ready, and deliberately NOT the amber the in-flight dot
     owns). */
  .question,
  .plan {
    border-right: 2px solid var(--accent);
    padding-right: 0.5rem;
  }
  .plan {
    border-right-color: var(--dot-ready);
  }
  .tag {
    color: var(--accent);
    margin-left: 0.5rem;
    float: right;
  }
  .plan .tag {
    color: var(--dot-ready);
  }
</style>
