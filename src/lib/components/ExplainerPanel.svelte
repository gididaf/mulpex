<!-- The Explainer: a persistent right-hand column showing the focused claude's
     per-turn Hebrew explanations, oldest first — the newest entry is at the
     BOTTOM, so the feed grows downward the way the claude transcript beside it
     does. The backend store stays newest-first (its 10-entry cap is a
     prepend-and-truncate, mirrored in stores.ts); the reversal is render-time
     only.

     Every entry has the same three fixed parts: what we are working on, what
     he did this turn, what he needs from you. THE HEADINGS ARE DRAWN HERE, not
     by the summarizer — it emits three marked lines (WORK/DID/NEED) and the app
     owns the wording and the type, so they cannot drift from one turn to the
     next. An entry whose output did not parse into three parts falls back to
     its raw text rather than faking the structure.

     History recedes: only the newest entry reads at full strength until the
     pointer enters the panel, and a short feed sits at the bottom of the
     column, next to the claude's own latest words.

     A real grid column, not an overlay — hiding it (⌘⇧E) resizes the terminal
     pane, which the TerminalPane ResizeObserver propagates workspace-wide like
     any window resize. The entry text is hard dir="rtl" (see the markup
     comment). Nothing here touches the xterm CSS. -->
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

  // The Hebrew headings. Order is fixed and matches the summarizer's contract:
  // the goal, then the turn, then the ask — widest scope first, and the thing
  // that might need you from last, where the eye lands.
  const PARTS = [
    { key: "work", title: "על מה אנחנו עובדים" },
    { key: "did", title: "מה עשיתי בסבב זה" },
    { key: "need", title: "מה אני צריך ממך" },
  ] as const;

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
       starts, instead of at the moment its replacement lands.

       The feed sits at the BOTTOM of the column, not the top: the claude's own
       latest words are at the bottom of the terminal beside it, and a short
       feed pinned to the top would mean looking up for one and down for the
       other. See the `.content` rule for why this is an auto margin and not
       `justify-content: flex-end`. -->
  <div
    class="body"
    class:explaining={cur && !isShell && $activeExplainBusy && !anyRetrying}
    bind:this={bodyEl}
    onscroll={onScroll}
  >
    <div class="content">
      {#if !cur}
        <div class="empty">no session focused</div>
      {:else if isShell}
        <div class="empty">terminals aren't explained</div>
      {:else if ordered.length === 0 && !$activeExplainBusy}
        <div class="empty">nothing yet — explanations appear after each turn</div>
      {:else}
        {#each ordered as e (e.seq)}
          <article
            class:failed={!e.ok}
            class:question={e.kind === "question"}
            class:plan={e.kind === "plan"}
          >
            <div class="ts">
              {when(e.ts)}
              {#if e.kind === "question"}
                <span class="tag" dir="rtl">שאלה</span>
              {:else if e.kind === "plan"}
                <span class="tag" dir="rtl">תוכנית</span>
              {/if}
            </div>
            <!-- Hard rtl, NOT dir="auto": auto resolves from the first strong
                 character, and our prompt keeps English identifiers verbatim,
                 so a part often *begins* with one — auto then lays the whole
                 Hebrew sentence out LTR (measured: scrambled line order,
                 trailing period on the wrong side). The panel's language is
                 Hebrew by contract; inline English sits correctly inside an RTL
                 paragraph. -->
            {#if retrying[e.seq]}
              <div class="text working" dir="rtl">מסביר…</div>
            {:else if e.sections}
              {#each PARTS as part (part.key)}
                <section>
                  <h2 dir="rtl">{part.title}</h2>
                  <div class="text" dir="rtl">{e.sections[part.key]}</div>
                </section>
              {/each}
            {:else}
              <div class="text" dir="rtl">{e.text}</div>
            {/if}
            {#if !e.ok && !retrying[e.seq]}
              <!-- A failed entry is the one place the panel is actionable: the
                   summarizer child died (most often on a transient the automatic
                   second attempt didn't outlive), and its input is still stashed
                   backend-side, so one click re-runs exactly it. -->
              <div class="actions">
                <button
                  class="retry"
                  dir="rtl"
                  title="להריץ שוב את ההסבר לתור הזה"
                  onclick={() => retry(e.seq, e.ts, e.id)}>נסה שוב</button
                >
              </div>
            {/if}
          </article>
        {/each}
      {/if}
      {#if cur && !isShell && $activeExplainBusy && !anyRetrying}
        <div class="working" dir="rtl">מסביר…</div>
      {/if}
    </div>
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
    /* `flex: 1` is load-bearing, not tidiness: without it the body is only as
       tall as its content (a flex item defaults to `flex: 0 1 auto`), so there
       is no free space for the auto margin below to consume and the content
       stays glued to the top. Measured — the first version of this bottom-pin
       changed nothing at all for exactly that reason. */
    flex: 1;
    overflow-y: auto;
    min-height: 0;
    padding: 0.5rem 0.8rem;
    display: flex;
    flex-direction: column;
  }
  /* Bottom-pinned, the way a chat log is. `margin-top: auto` and NOT
     `justify-content: flex-end`: in a scroll container, flex-end pushes
     overflow past the START edge, where it cannot be scrolled back to — a long
     feed would lose its first entries with no way to reach them. An auto
     margin resolves to 0 the moment the content is taller than the box, so a
     short feed sits at the bottom and a long one scrolls normally from the
     top. Measured both ways (see docs/explainer.md). */
  .content {
    margin-top: auto;
  }
  .empty {
    color: var(--text-faint);
    font-size: 0.8rem;
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
  /* While a new turn is being explained the entry above it is already history
     — it describes the turn before the one in progress. Same specificity as
     the hover rule below (0,3,1); source order is what lets hover win. */
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
  .text.working {
    padding: 0;
  }
  section + section {
    margin-top: 0.85rem;
  }
  /* The heading is a label, not a voice: small, dim, uppercase-ish spacing is
     wrong for Hebrew, so it leans on size and colour instead. text-align:start
     under dir="rtl" puts it on the reading edge, above its own sentence. */
  h2 {
    margin: 0 0 0.2rem;
    font-size: 0.72rem;
    font-weight: 500;
    color: var(--label);
    text-align: start;
  }
  .failed .text {
    color: var(--text-faint);
    font-style: italic;
  }
  /* The retry lives under its failed entry, hugging the RTL reading edge. */
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
  /* A question or a plan: the claude is waiting on the user right now — accent
     edge on the reading (right) side, matching the panel's RTL. The colour is
     what separates them at a glance: cyan is a question to answer, green a
     finished plan waiting for "yes, execute" (the same green the sidebar uses
     for ready, and deliberately NOT the amber the in-flight dot owns). */
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
