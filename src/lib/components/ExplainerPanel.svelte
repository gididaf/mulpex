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
  import { untrack } from "svelte";
  import {
    activeExplains,
    activeExplainBusy,
    activeProjectHandle,
    sessions,
    activeId,
    showExplainer,
  } from "../stores";
  import { retryExplain } from "../ipc";

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

  // Rows with a retry in flight: entry `seq` → the `ts` it had when the button
  // was pressed. The backend replaces a retried row in place and stamps it with
  // the retry's time, so a changed `ts` under the same `seq` is exactly "your
  // retry came back" — success or a fresh failure alike.
  let retrying = $state<Record<number, number>>({});
  // Only retries of rows in the feed on screen suppress the bottom in-flight
  // line — a retry left running on another instance says nothing about this one.
  const anyRetrying = $derived($activeExplains.some((e) => retrying[e.seq]));

  $effect(() => {
    const live = $activeExplains;
    untrack(() => {
      const next = { ...retrying };
      let changed = false;
      for (const key of Object.keys(next)) {
        const seq = Number(key);
        const e = live.find((x) => x.seq === seq);
        // Only a *returned* row clears the marker. A seq that is simply absent
        // belongs to another instance's feed (the user switched rows mid-retry)
        // and must survive, or coming back would show the failure again with
        // its button while the retry is still running.
        if (e && e.ts !== next[seq]) {
          delete next[seq];
          changed = true;
        }
      }
      if (changed) retrying = next;
    });
  });

  async function retry(seq: number, ts: number, id: number) {
    const handle = $activeProjectHandle;
    if (handle === null) return;
    retrying = { ...retrying, [seq]: ts };
    let queued = false;
    try {
      queued = await retryExplain(handle, id, seq);
    } catch {
      queued = false;
    }
    // Nothing stashed under that seq any more (or the call failed): no update is
    // coming, so give the button back instead of spinning forever.
    if (!queued) {
      const next = { ...retrying };
      delete next[seq];
      retrying = next;
    }
  }

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
  <!-- `explaining` is what dims the previous entry the MOMENT a new summary
       starts, instead of at the moment its replacement lands. -->
  <div
    class="body"
    class:explaining={cur && !isShell && $activeExplainBusy && !anyRetrying}
    bind:this={bodyEl}
    onscroll={onScroll}
  >
    {#if !cur}
      <div class="empty">no session focused</div>
    {:else if isShell}
      <div class="empty">terminals aren't explained</div>
    {:else if ordered.length === 0 && !$activeExplainBusy}
      <div class="empty">nothing yet — explanations appear after each turn</div>
    {:else}
      <!-- Keyed by seq: a retry rewrites its row in place (same seq, new ts), and
           keying on ts would tear the article down and rebuild it instead. -->
      {#each ordered as e (e.seq)}
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
          {#if retrying[e.seq]}
            <div class="text working" dir="rtl">מסביר…</div>
          {:else}
            <div class="text" dir="rtl">{e.text}</div>
            {#if !e.ok}
              <!-- A failed row is the one place the panel is actionable: the
                   summarizer child died (most often on a transient the automatic
                   second attempt didn't outlive), and the turn's text is still
                   stashed backend-side, so one click re-runs exactly it. -->
              <div class="actions">
                <button
                  class="retry"
                  dir="rtl"
                  title="להריץ שוב את ההסבר לתור הזה"
                  onclick={() => retry(e.seq, e.ts, e.id)}>נסה שוב</button
                >
              </div>
            {/if}
          {/if}
        </article>
      {/each}
    {/if}
    <!-- The in-flight line lives at the bottom, where the entry it is producing
         will land. -->
    {#if cur && !isShell && $activeExplainBusy && !anyRetrying}
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
  article:last-of-type {
    opacity: 1;
  }
  /* While a new turn is being explained the entry above it is already history,
     so it recedes right away — the panel shouldn't keep pointing at the old
     summary for the seconds it takes the new one to arrive. Same specificity
     game as below: (0,3,1) here, (0,3,1) later for hover, so hover wins on
     source order. */
  .body.explaining article:last-of-type {
    opacity: 0.18;
  }
  /* Pointer in the panel reads the whole history at full strength, in flight
     or not. */
  .explainer:hover .body article {
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
  /* The retry lives under its failed row, hugging the RTL reading edge. */
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: 0.35rem;
  }
  .retry {
    background: none;
    border: 1px solid var(--border);
    border-radius: 3px;
    color: var(--text-dim);
    font: inherit;
    font-size: 0.72rem;
    padding: 0.1rem 0.45rem;
    cursor: pointer;
  }
  .retry:hover {
    color: var(--text);
    border-color: var(--text-dim);
  }
  /* The in-place "מסביר…" reuses the bottom line's look, minus its padding —
     it stands in for the entry text, inside a row that already has its own. */
  .text.working {
    padding: 0;
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
