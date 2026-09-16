<script lang="ts">
  import {
    projects,
    activeProjectHandle,
    needsCount,
    readyCount,
    type ProjectHandle,
  } from "../stores";

  let {
    onselect,
    onclose,
    onadd,
    onreorder,
  }: {
    onselect: (h: ProjectHandle) => void;
    onclose: (h: ProjectHandle) => void;
    onadd: () => void;
    onreorder: (order: ProjectHandle[]) => void;
  } = $props();

  const list = $derived([...$projects.values()]);

  // ---- drag to reorder ----
  //
  // Pointer events, not HTML5 drag-and-drop: Tauri's webview-level drag-drop is
  // enabled (App.svelte relies on it for dropping folders onto the window), and
  // it intercepts drags before the DOM sees them. Pointer capture also gives us
  // the drag threshold below, which HTML5 DnD can't express.

  /** Tab elements by index, for hit-testing the pointer against their midpoints. */
  let tabEls: HTMLElement[] = [];
  /** Index being dragged, or null when idle. */
  let dragIdx = $state<number | null>(null);
  /** Index it would land on — also drives the drop indicator. */
  let overIdx = $state<number | null>(null);
  /** True only once the pointer has moved past the threshold. */
  let dragging = $state(false);
  let startX = 0;
  /** Swallow the click that ends a drag, so reordering never also switches tab. */
  let suppressClick = false;

  /** Which slot the pointer is over: first tab whose midpoint it hasn't passed. */
  function indexAt(x: number): number {
    for (let i = 0; i < list.length; i++) {
      const r = tabEls[i]?.getBoundingClientRect();
      if (r && x < r.left + r.width / 2) return i;
    }
    return list.length - 1;
  }

  function onPointerDown(e: PointerEvent, i: number) {
    if (e.button !== 0) return; // left button only — right-click may open a menu
    dragIdx = i;
    startX = e.clientX;
    dragging = false;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent) {
    if (dragIdx == null) return;
    // A few px of slop: a plain click always wiggles slightly, and treating that
    // as a drag would make tabs impossible to simply select.
    if (!dragging && Math.abs(e.clientX - startX) < 4) return;
    dragging = true;
    overIdx = indexAt(e.clientX);
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
    const order = list.map((p) => p.handle);
    const [h] = order.splice(from, 1);
    order.splice(to, 0, h);
    onreorder(order);
  }

  function selectUnlessDragged(h: ProjectHandle) {
    if (suppressClick) {
      suppressClick = false;
      return;
    }
    onselect(h);
  }
</script>

<div class="tabs">
  {#each list as p, i (p.handle)}
    <!-- Both badge counts exclude muted sessions (stores.ts) — muting is meant to
         quiet a project you're deliberately not watching, so a muted instance
         must not keep the tab lit up. -->
    {@const ready = readyCount(p)}
    {@const needs = needsCount(p)}
    <div
      class="tab"
      class:active={p.handle === $activeProjectHandle}
      class:dragging={dragging && dragIdx === i}
      class:drop-target={dragging && overIdx === i && dragIdx !== i}
      bind:this={tabEls[i]}
    >
      <button
        class="label"
        title={p.dir}
        onpointerdown={(e) => onPointerDown(e, i)}
        onpointermove={onPointerMove}
        onpointerup={onPointerUp}
        onpointercancel={onPointerUp}
        onclick={() => selectUnlessDragged(p.handle)}
      >
        <span class="name">{p.name}</span>
        <!-- Two badges, same colors as the sidebar dots, each hidden at zero:
             green = claudes done and idle, red = claudes stopped on a question.
             Working (amber) is deliberately unbadged, so a tab with everything
             busy shows no pill at all. -->
        {#if ready > 0}
          <span
            class="badge ready"
            title="{ready} idle session{ready === 1 ? '' : 's'}"
          >
            {ready}
          </span>
        {/if}
        {#if needs > 0}
          <span
            class="badge needs"
            title="{needs} session{needs === 1 ? '' : 's'} need{needs === 1
              ? 's'
              : ''} you"
          >
            {needs}
          </span>
        {/if}
      </button>
      <button
        class="x"
        aria-label="Close project"
        title="Close project (⌘⇧W)"
        onclick={(e) => {
          e.stopPropagation();
          onclose(p.handle);
        }}>✕</button
      >
    </div>
  {/each}
  <button class="add" aria-label="Open project" title="Open project (⌘O)" onclick={onadd}>+</button>
</div>

<style>
  .tabs {
    grid-area: tabs;
    display: flex;
    align-items: stretch;
    gap: 0.25rem;
    padding: 0.25rem 0.4rem 0;
    background: var(--bg-sidebar);
    border-bottom: 1px solid var(--border);
    overflow-x: auto;
    white-space: nowrap;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    padding: 0.2rem 0.35rem 0.2rem 0.5rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-bottom: none;
    border-radius: 6px 6px 0 0;
    max-width: 14rem;
  }
  .tab.active {
    border-color: var(--border-focus);
    background: var(--bg);
  }
  /* The tab under the cursor fades so it reads as "in hand"; the slot it would
     drop into gets an accent edge. Deliberately no motion — tabs reflowing under
     a moving cursor makes the target ambiguous. */
  .tab.dragging {
    opacity: 0.45;
  }
  .tab.drop-target {
    box-shadow: inset 2px 0 0 var(--border-focus);
  }
  .label {
    cursor: grab;
  }
  .tab.dragging .label {
    cursor: grabbing;
  }
  .label {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    background: none;
    border: none;
    padding: 0;
    color: var(--text-dim);
    font-size: 0.8rem;
    max-width: 12rem;
  }
  .tab.active .label {
    color: var(--text);
    font-weight: 600;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    flex: none;
    min-width: 1.1rem;
    padding: 0 0.25rem;
    text-align: center;
    border-radius: 0.7rem;
    font-size: 0.66rem;
    font-weight: 700;
  }
  /* The sidebar's dot colors, so a tab reads the same way a row does. Dark text
     on a saturated pill — white on either of these fails contrast. */
  .badge.ready {
    background: var(--dot-ready);
    color: #0c2110;
  }
  .badge.needs {
    background: var(--dot-needs);
    color: #2a0a0d;
  }
  .x {
    flex: none;
    background: none;
    border: none;
    color: var(--text-faint);
    font-size: 0.7rem;
    line-height: 1;
    padding: 0.15rem;
    border-radius: 4px;
  }
  .x:hover {
    color: var(--text);
    background: var(--border);
  }
  .add {
    flex: none;
    align-self: center;
    background: none;
    border: none;
    color: var(--text-dim);
    font-size: 1rem;
    line-height: 1;
    padding: 0.1rem 0.5rem;
    border-radius: 4px;
  }
  .add:hover {
    color: var(--text);
    background: var(--bg-elev);
  }
</style>
