<!-- The Explainer: a right-hand column showing ONE Hebrew explanation — what the
     focused claude is saying in the turn on screen right now, as three fixed
     parts: what we are working on, what he did this turn, what he needs from
     you. THE HEADINGS ARE DRAWN HERE, not by the summarizer — it emits three
     marked lines (WORK/DID/NEED) and the app owns the wording and the type, so
     they cannot drift from one turn to the next. An entry whose output did not
     parse into three parts (and a question or a plan, which have their own
     shapes) falls back to its raw text rather than faking the structure.

     It is on demand and nothing else. ⌘⇧E opens this panel and asks in the same
     keystroke; closing it, switching rows, or sending the next prompt clears the
     answer and puts the column away (App.svelte owns all three). So there is no
     feed, no timestamps and no scrollback here: the transcript beside it is the
     history, and this is a glance at the present.

     A real grid column, not an overlay — showing it resizes the terminal pane,
     which the TerminalPane ResizeObserver propagates workspace-wide like any
     window resize. The entry text is hard dir="rtl" (see the markup comment).
     Nothing here touches the xterm CSS. -->
<script lang="ts">
  import { untrack } from "svelte";
  import {
    activeExplains,
    activeExplainBusy,
    explainDeciding,
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
  // At most one, by construction (the backend keeps a single entry per instance).
  const entry = $derived($activeExplains[0] ?? null);

  // The Hebrew headings. Order is fixed and matches the summarizer's contract:
  // the goal, then the turn, then the ask — widest scope first, and the thing
  // that might need you from last, where the eye lands.
  const PARTS = [
    { key: "work", title: "על מה אנחנו עובדים" },
    { key: "did", title: "מה עשיתי בסבב זה" },
    { key: "need", title: "מה אני צריך ממך" },
  ] as const;

  // The row's retry is in flight: the `ts` the entry had when the button was
  // pressed. The backend rewrites a retried entry in place and stamps it with
  // the retry's time, so a changed `ts` under the same `seq` is exactly "your
  // retry came back" — success or a fresh failure alike.
  let retrying = $state<{ seq: number; ts: number } | null>(null);

  $effect(() => {
    const e = $activeExplains[0] ?? null;
    untrack(() => {
      if (!retrying) return;
      // Only a *returned* entry clears the marker. An absent one means this row
      // is simply not on screen (the panel cleared, or another instance is
      // focused), and clearing on that would put the button back under a retry
      // that is still running.
      if (e && e.seq === retrying.seq && e.ts !== retrying.ts) retrying = null;
    });
  });

  async function retry(seq: number, ts: number, id: number) {
    const handle = $activeProjectHandle;
    if (handle === null) return;
    retrying = { seq, ts };
    let queued = false;
    try {
      queued = await retryExplain(handle, id, seq);
    } catch {
      queued = false;
    }
    // Nothing stashed under that seq any more (or the call failed): no update is
    // coming, so give the button back instead of spinning forever.
    if (!queued && retrying?.seq === seq) retrying = null;
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
  <!-- The content sits at the BOTTOM of the column, not the top: the claude's
       own latest words are at the bottom of the terminal beside it, and an
       explanation pinned to the top would mean looking up for one and down for
       the other. See the `.content` rule for why this is an auto margin and not
       `justify-content: flex-end`. -->
  <div class="body">
    <div class="content">
      {#if !cur}
        <div class="empty">no session focused</div>
      {:else if isShell}
        <div class="empty">terminals aren't explained</div>
      {:else if $explainDeciding || retrying || (!entry && $activeExplainBusy)}
        <div class="working" dir="rtl">מסביר…</div>
      {:else if !entry}
        <!-- Reachable when explain_now refused: no transcript yet, or the instance
             went away between the keypress and the answer. -->
        <div class="empty">nothing to explain yet</div>
      {:else}
        <article
          class:failed={!entry.ok}
          class:question={entry.kind === "question"}
          class:plan={entry.kind === "plan"}
        >
          {#if entry.kind === "question"}
            <div class="tag" dir="rtl">שאלה</div>
          {:else if entry.kind === "plan"}
            <div class="tag" dir="rtl">תוכנית</div>
          {/if}
          <!-- Hard rtl, NOT dir="auto": auto resolves from the first strong
               character, and our prompt keeps English identifiers verbatim, so
               a part often *begins* with one — auto then lays the whole Hebrew
               sentence out LTR (measured: scrambled line order, trailing period
               on the wrong side). The panel's language is Hebrew by contract;
               inline English sits correctly inside an RTL paragraph. -->
          {#if entry.sections}
            {#each PARTS as part (part.key)}
              <section>
                <h2 dir="rtl">{part.title}</h2>
                <div class="text" dir="rtl">{entry.sections[part.key]}</div>
              </section>
            {/each}
          {:else}
            <div class="text" dir="rtl">{entry.text}</div>
          {/if}
          {#if !entry.ok}
            <!-- A failed entry is the one place the panel is actionable: the
                 summarizer child died (most often on a transient the automatic
                 second attempt didn't outlive), and its input is still stashed
                 backend-side, so one click re-runs exactly it. -->
            <div class="actions">
              <button
                class="retry"
                dir="rtl"
                title="להריץ שוב את ההסבר"
                onclick={() => retry(entry.seq, entry.ts, entry.id)}
                >נסה שוב</button
              >
            </div>
          {/if}
        </article>
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
     explanation would lose its first lines with no way to reach them. An auto
     margin resolves to 0 the moment the content is taller than the box, so
     short content sits at the bottom and tall content scrolls normally from the
     top. Measured both ways (see docs/explainer.md). */
  .content {
    margin-top: auto;
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
    font-size: 0.7rem;
    text-align: start;
    margin-bottom: 0.3rem;
  }
  .plan .tag {
    color: var(--dot-ready);
  }
</style>
