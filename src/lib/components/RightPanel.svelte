<script lang="ts">
  // The side panel (⌘⇧L): a column right of the terminal with three fixed tabs,
  // Board · Pins · Explain. The Board is the project's; Pins and Explain show the
  // focused instance's content. Tabs never close — only the panel hides.
  //
  // A real column, so the terminal narrows instead of being covered. That
  // resizes every PTY (one geometry — docs/rendering.md), which is why it opens
  // and closes for the whole app, never per instance, and why a drag of its edge
  // only commits its width on release.
  import { activeProjectHandle, activeId } from "../stores";
  import { rightPanel, setPanelOpen, setTab, setPanelFrac, clampFrac, type PanelTab } from "../rightPanel";
  import { boards, unreadCount } from "../board";
  import { pins, pinKey } from "../pins";
  import { explains, explainKey, isStreaming } from "../explain";
  import { THEME, terminals } from "../terminals";
  import BoardTab from "./BoardTab.svelte";
  import PinsTab from "./PinsTab.svelte";
  import ExplainTab from "./ExplainTab.svelte";

  /** The row the panel shares with the terminal, to turn a drag into a fraction. */
  let { row }: { row: HTMLElement | undefined } = $props();

  const h = $derived($activeProjectHandle);
  const id = $derived($activeId);

  /** Cards a claude changed since the user last opened them. */
  const boardCount = $derived(h != null ? unreadCount($boards.get(h)) : 0);
  const pinCount = $derived(h != null && id != null ? ($pins.get(pinKey(h, id))?.length ?? 0) : 0);
  const ex = $derived(h != null && id != null ? $explains.get(explainKey(h, id)) : undefined);

  const TABS: { tab: PanelTab; label: string }[] = [
    { tab: "board", label: "Board" },
    { tab: "pins", label: "Pins" },
    { tab: "explain", label: "Explain" },
  ];

  function pick(tab: PanelTab) {
    setTab(tab);
    terminals.refocus();
  }

  // ── resize by dragging the left edge ─────────────────────────────────────
  // A ghost line follows the pointer; the width (and so every PTY's size)
  // changes once, on release.
  let ghostX: number | null = $state(null);

  function fracAt(clientX: number): number {
    if (!row) return 0.3;
    const r = row.getBoundingClientRect();
    return clampFrac((r.right - clientX) / r.width);
  }

  function ondown(e: PointerEvent) {
    if (e.button !== 0 || !row) return;
    e.preventDefault();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    ghostX = e.clientX;
  }
  function onmove(e: PointerEvent) {
    if (ghostX == null || !row) return;
    const r = row.getBoundingClientRect();
    ghostX = r.right - fracAt(e.clientX) * r.width;
  }
  function onup(e: PointerEvent) {
    if (ghostX == null) return;
    ghostX = null;
    setPanelFrac(fracAt(e.clientX));
    terminals.refocus();
  }
</script>

<aside class="panel" aria-label="Side panel" style="--pin-bg:{THEME.background};--pin-fg:{THEME.foreground};">
  <div
    class="grip"
    role="separator"
    aria-orientation="vertical"
    title="Drag to resize"
    onpointerdown={ondown}
    onpointermove={onmove}
    onpointerup={onup}
    onpointercancel={() => (ghostX = null)}
  ></div>
  <header>
    <div class="tabs" role="tablist">
      {#each TABS as t (t.tab)}
        <button
          role="tab"
          class="tab"
          class:on={$rightPanel.tab === t.tab}
          aria-selected={$rightPanel.tab === t.tab}
          onclick={() => pick(t.tab)}
        >
          {t.label}
          {#if t.tab === "board" && boardCount}<span class="count" title="Cards a claude updated since you last opened them">{boardCount}</span>{/if}
          {#if t.tab === "pins" && pinCount}<span class="count dim">{pinCount}</span>{/if}
          {#if t.tab === "explain" && ex}<span class="dot" class:live={isStreaming(ex)}></span>{/if}
        </button>
      {/each}
    </div>
    <button class="x" title="Hide panel (⌘⇧L)" onclick={() => { setPanelOpen(false); terminals.refocus(); }}>✕</button>
  </header>
  <div class="body">
    {#if $rightPanel.tab === "board"}
      <BoardTab />
    {:else if $rightPanel.tab === "pins"}
      <PinsTab />
    {:else}
      <ExplainTab />
    {/if}
  </div>
</aside>
{#if ghostX != null}
  <div class="ghost" style="left:{ghostX}px"></div>
{/if}

<style>
  .panel {
    position: relative;
    display: flex;
    flex-direction: column;
    min-width: 0;
    height: 100%;
    background: var(--bg-elev);
    color: var(--text);
    border-left: 1px solid var(--border);
  }
  .grip {
    position: absolute;
    top: 0;
    bottom: 0;
    left: -3px;
    width: 6px;
    cursor: col-resize;
    z-index: 2;
  }
  .grip:hover {
    background: var(--border-focus);
  }
  .ghost {
    position: fixed;
    top: 0;
    bottom: 0;
    width: 2px;
    margin-left: -1px;
    background: var(--accent);
    z-index: 50;
    pointer-events: none;
  }
  header {
    display: flex;
    align-items: stretch;
    justify-content: space-between;
    gap: 0.4rem;
    padding: 0 0.5rem 0 0.4rem;
    border-bottom: 1px solid var(--border);
  }
  .tabs {
    display: flex;
    min-width: 0;
    overflow: hidden;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    border: none;
    border-bottom: 2px solid transparent;
    background: none;
    color: var(--text-faint);
    font-size: 0.8rem;
    font-weight: 600;
    padding: 0.5rem 0.55rem 0.4rem;
    white-space: nowrap;
  }
  .tab:hover {
    color: var(--text-dim);
  }
  .tab.on {
    color: var(--text);
    border-bottom-color: var(--accent);
  }
  .count {
    min-width: 1.1rem;
    padding: 0 0.3rem;
    border-radius: 0.6rem;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.7rem;
    line-height: 1.1rem;
    text-align: center;
  }
  .count.dim {
    background: var(--border);
    color: var(--text-dim);
  }
  .dot {
    width: 0.4rem;
    height: 0.4rem;
    border-radius: 50%;
    background: var(--text-faint);
  }
  .dot.live {
    background: var(--dot-working);
  }
  .x {
    align-self: center;
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
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
</style>
