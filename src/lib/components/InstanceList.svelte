<script lang="ts">
  import {
    sessions,
    statuses,
    tasks,
    activeId,
    hub,
    clampToGroup,
    dragOrder,
  } from "../stores";
  import type { Status } from "../ipc";

  let {
    onselect,
    onmute,
    onreorder,
  }: {
    onselect: (id: number) => void;
    /** Toggle mute for any row, without selecting it first. */
    onmute: (id: number, muted: boolean) => void;
    /** Commit a new top-to-bottom order (session ids) after a drag. */
    onreorder: (ids: number[]) => void;
  } = $props();

  const DOT: Record<Status, string> = {
    working: "var(--dot-working)",
    waiting: "var(--dot-ready)",
    needs: "var(--dot-needs)",
  };
  const LABEL: Record<Status, string> = {
    working: "working",
    waiting: "ready",
    needs: "needs you",
  };

  function statusOf(id: number): Status {
    return $statuses.get(id) ?? "waiting";
  }
  function waitOn(id: number): number | null {
    const w = $hub?.waiting.find((x) => x.id === id);
    return w ? w.holder : null;
  }
  /** How a row names itself. Instances and terminals draw from one id space, so
   *  the number alone is unambiguous — the word says which kind it is. */
  const label = (s: { id: number; kind: string }) =>
    s.kind === "shell" ? `term #${s.id}` : `claude #${s.id}`;

  // ---- drag to reorder ----
  //
  // The same mechanism as ProjectTabBar, turned onto the Y axis: pointer events
  // rather than HTML5 drag-and-drop, because Tauri's webview-level drag-drop is
  // enabled (App.svelte relies on it for dropped paths) and intercepts drags
  // before the DOM sees them. Pointer capture also gives us the threshold below,
  // which HTML5 DnD can't express.
  //
  // Terminals drag like instances — one list, one behavior. Their position just
  // isn't persisted, because terminals themselves aren't (see CLAUDE.md).

  /** Row elements by index, for hit-testing the pointer against their midpoints. */
  let rowEls: HTMLElement[] = [];
  /** Index being dragged, or null when idle. */
  let dragIdx = $state<number | null>(null);
  /** Index it would land on — also drives the drop indicator. */
  let overIdx = $state<number | null>(null);
  /** True only once the pointer has moved past the threshold. */
  let dragging = $state(false);
  let startY = 0;
  /** Swallow the click that ends a drag, so reordering never also selects. */
  let suppressClick = false;

  /** Which slot the pointer is over: first row whose midpoint it hasn't passed. */
  function indexAt(y: number): number {
    for (let i = 0; i < $sessions.length; i++) {
      const r = rowEls[i]?.getBoundingClientRect();
      if (r && y < r.top + r.height / 2) return i;
    }
    return $sessions.length - 1;
  }

  function onPointerDown(e: PointerEvent, i: number) {
    if (e.button !== 0) return; // left button only — right-click may open a menu
    dragIdx = i;
    startY = e.clientY;
    dragging = false;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent) {
    if (dragIdx == null) return;
    // A few px of slop: a plain click always wiggles slightly, and treating that
    // as a drag would make rows impossible to simply select.
    if (!dragging && Math.abs(e.clientY - startY) < 4) return;
    dragging = true;
    // Clamped, not raw: the indicator must only ever appear where the row can
    // actually land (see stores.ts::clampToGroup).
    overIdx = clampToGroup($sessions, dragIdx, indexAt(e.clientY));
  }

  function onPointerUp() {
    const from = dragIdx;
    const to = overIdx;
    const moved = dragging;
    dragIdx = null;
    overIdx = null;
    dragging = false;
    if (!moved) return;
    suppressClick = true;
    if (from == null || to == null || from === to) return;
    onreorder(dragOrder($sessions, from, to));
  }

  function selectUnlessDragged(id: number) {
    if (suppressClick) {
      suppressClick = false;
      return;
    }
    onselect(id);
  }
</script>

<!-- `$sessions` is already in display order (muted sunk to the bottom, creation
     order preserved within each group) — see stores.ts::displayOrder. -->
<div class="list">
  {#each $sessions as s, i (s.id)}
    {@const st = statusOf(s.id)}
    <!-- A muted row drops every attention signal — dot, status word, ⏳ — and
         carries 🔇 instead, so "dimmed" reads as "you silenced this" rather than
         "this one died". The mute toggle is a sibling of the select button, not
         nested inside it (a button inside a button is invalid HTML). -->
    {@const shell = s.kind === "shell"}
    <div
      class="row"
      class:active={s.id === $activeId}
      class:muted={s.muted}
      class:shell
      class:dragging={dragging && dragIdx === i}
      class:drop-target={dragging && overIdx === i && dragIdx !== i}
      bind:this={rowEls[i]}
    >
      <button
        class="body"
        onpointerdown={(e) => onPointerDown(e, i)}
        onpointermove={onPointerMove}
        onpointerup={onPointerUp}
        onpointercancel={onPointerUp}
        onclick={() => selectUnlessDragged(s.id)}
      >
        <div class="head">
          <!-- A terminal has no hub status to report, so it gets a $ marker
               where an instance gets its status dot. Leaving the dot out
               entirely would read as "this row is broken"; a green "ready" dot
               (the default for an unknown id) would be a lie. -->
          <!-- A failed instance is absent from `statuses`, so `st` here is the
               unknown-id default — "ready". Painting that would say a session
               that never started is waiting for you, so the failure has to win
               over the status dot rather than sit beside it. -->
          {#if shell}
            <span class="sigil" aria-hidden="true">$</span>
          {:else if s.failed}
            <span class="sigil failed" aria-hidden="true">⚠</span>
          {:else if !s.muted}
            <span class="dot" style:background={DOT[st]}></span>
          {/if}
          <span class="id">{label(s)}</span>
          {#if !shell && !s.failed && !s.muted && waitOn(s.id) != null}
            <span class="wait" title="waiting on #{waitOn(s.id)}">⏳</span>
          {/if}
          {#if shell}
            <span class="st">{s.exited ? "exited" : "running"}</span>
          {:else if s.failed}
            <span class="st failed" title={s.failed}>failed to start</span>
          {:else if !s.muted}
            <span class="st">{LABEL[st]}</span>
          {/if}
        </div>
        {#if s.name}
          <div class="name">{s.name}</div>
        {:else if !shell && $tasks.get(s.id)}
          <div class="task">{$tasks.get(s.id)}</div>
        {/if}
      </button>
      <!-- Mute is meaningless for a terminal: it produces none of the signals
           mute silences, and the backend refuses to record the flag. -->
      {#if !shell}
        <button
          class="mute"
          class:on={s.muted}
          aria-pressed={s.muted}
          title="{s.muted ? 'Unmute' : 'Mute'} claude #{s.id}{s.id === $activeId
            ? ' (⌘M)'
            : ''}"
          onclick={() => onmute(s.id, !s.muted)}
        >
          {s.muted ? "🔇" : "🔊"}
        </button>
      {/if}
    </div>
  {/each}
</div>

<style>
  .list {
    overflow-y: auto;
    padding: 0.35rem;
    border-bottom: 1px solid var(--border);
  }
  .row {
    display: flex;
    align-items: flex-start;
    gap: 0.15rem;
    width: 100%;
    padding: 0.4rem 0.35rem 0.4rem 0.5rem;
    margin-bottom: 0.25rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
  }
  .row:hover {
    background: var(--bg-elev);
  }
  .row.active {
    border-color: var(--border-focus);
    background: var(--bg-elev);
  }
  /* The row under the cursor fades so it reads as "in hand"; the slot it would
     drop into gets an accent edge. Deliberately no motion — rows reflowing under
     a moving cursor makes the target ambiguous. Mirrors ProjectTabBar, with the
     accent on the top edge because this list runs vertically. */
  .row.dragging {
    opacity: 0.45;
  }
  .row.drop-target {
    box-shadow: inset 0 2px 0 var(--border-focus);
  }
  .body {
    cursor: grab;
  }
  .row.dragging .body {
    cursor: grabbing;
  }
  .body {
    flex: 1;
    min-width: 0;
    display: block;
    text-align: left;
    background: transparent;
    border: none;
    padding: 0;
  }
  /* Dim the content, not the 🔇 — the marker has to stay legible to explain the
     dimming. */
  .row.muted .body {
    opacity: 0.45;
  }
  .mute {
    flex: none;
    background: none;
    border: none;
    padding: 0.1rem 0.15rem;
    border-radius: 4px;
    font-size: 0.72rem;
    line-height: 1;
    /* Unmuted rows keep the toggle invisible until you look at them: a 🔊 on
       every row is noise, but it has to be reachable without selecting first. */
    opacity: 0;
  }
  .row:hover .mute,
  .mute.on,
  .mute:focus-visible {
    opacity: 1;
  }
  .mute:hover {
    background: var(--border);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  /* Occupies the dot's slot so terminal and instance rows line up. */
  .sigil {
    width: 8px;
    flex: none;
    text-align: center;
    color: var(--text-faint);
    font-size: 0.8rem;
    line-height: 1;
  }
  .id {
    font-weight: 600;
  }
  .row.shell .id {
    color: var(--text-faint);
    font-weight: 500;
  }
  .wait {
    font-size: 0.8rem;
  }
  .st {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 0.72rem;
  }
  /* A failed row is the one case that should catch the eye without being
     confused for "needs you": it is not asking a question, it is reporting that
     nothing is there. Warning colour rather than the red `needs` uses. */
  .failed {
    color: var(--warn, #e0a34a);
  }
  .name {
    margin-top: 2px;
    color: var(--text);
    font-weight: 600;
    font-size: 0.8rem;
  }
  .task {
    margin-top: 2px;
    color: var(--text-dim);
    font-size: 0.78rem;
    font-style: italic;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
</style>
