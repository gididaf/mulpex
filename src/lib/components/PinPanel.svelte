<script lang="ts">
  // ⌘⇧P's pins panel: a column right of the terminal, listing the active
  // instance's pins, oldest first. Each pin is styled as the terminal itself —
  // same font, size, background and claude's colors — so it reads like the
  // lines it was lifted from, but wrapped to the panel's width.
  //
  // A real column, so the terminal narrows instead of being covered. That
  // resizes every PTY (one geometry — docs/rendering.md), which is why it opens
  // and closes for the whole app, never per instance, and why a drag of its edge
  // only commits its width on release.
  import { tick } from "svelte";
  import { activeProjectHandle, activeId, sessions, flashToast } from "../stores";
  import {
    pins,
    pinKey,
    removePin,
    togglePin,
    clearPins,
    setPanelOpen,
    setPanelFrac,
    clampFrac,
  } from "../pins";
  import type { PinRun } from "../pins";
  import { THEME, FONT_FAMILY, FONT_SIZE, LINE_HEIGHT, terminals } from "../terminals";

  /** The row the panel shares with the terminal, to turn a drag into a fraction. */
  let { row }: { row: HTMLElement | undefined } = $props();

  const h = $derived($activeProjectHandle);
  const id = $derived($activeId);
  const kind = $derived(id != null ? $sessions.find((s) => s.id === id)?.kind : undefined);
  const label = $derived(id == null ? "" : `${kind === "shell" ? "term" : "claude"}#${id}`);
  const list = $derived(h != null && id != null ? ($pins.get(pinKey(h, id)) ?? []) : []);

  let listEl: HTMLDivElement | undefined = $state();

  // Scroll to a new pin (the list grew), not on collapse or on switching instance.
  let lastKey = "";
  let lastLen = 0;
  $effect(() => {
    const key = h != null && id != null ? pinKey(h, id) : "";
    const len = list.length;
    const grew = key === lastKey && len > lastLen;
    lastKey = key;
    lastLen = len;
    if (grew) tick().then(() => listEl?.scrollTo({ top: listEl.scrollHeight, behavior: "smooth" }));
  });

  function css(r: PinRun): string {
    let s = "";
    if (r.fg) s += `color:${r.fg};`;
    if (r.bg) s += `background:${r.bg};`;
    if (r.b) s += "font-weight:bold;";
    if (r.d) s += "opacity:0.5;";
    if (r.i) s += "font-style:italic;";
    if (r.u || r.s)
      s += `text-decoration:${[r.u && "underline", r.s && "line-through"].filter(Boolean).join(" ")};`;
    return s;
  }

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      flashToast("Copied");
    } catch {
      flashToast("Couldn't copy");
    }
    terminals.refocus();
  }

  function act(f: () => void) {
    f();
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

<aside
  class="panel"
  aria-label="Pins"
  style="--pin-bg:{THEME.background};--pin-fg:{THEME.foreground};"
>
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
    <span class="title">Pins{#if label}<span class="who"> · {label}</span>{/if}</span>
    <span class="hdr-tools">
      {#if list.length && h != null && id != null}
        {@const hh = h}
        {@const ii = id}
        <button class="clear" title="Remove all pins of {label}" onclick={() => act(() => clearPins(hh, ii))}
          >Clear all</button
        >
      {/if}
      <button class="x" title="Close panel (⌘⇧P with nothing selected)" onclick={() => act(() => setPanelOpen(false))}
        >✕</button
      >
    </span>
  </header>
  <div class="list" bind:this={listEl}>
    {#if list.length && h != null && id != null}
      {@const hh = h}
      {@const ii = id}
      <div class="pins" style="font-family:{FONT_FAMILY};font-size:{FONT_SIZE}px;line-height:{LINE_HEIGHT};">
        {#each list as pin (pin.uid)}
          <div class="pin">
            <div class="tools">
              <button title="Copy" onclick={() => copy(pin.text)}>⧉</button>
              <button
                title={pin.collapsed ? "Expand" : "Collapse"}
                onclick={() => act(() => togglePin(hh, ii, pin.uid))}>{pin.collapsed ? "▸" : "▾"}</button
              >
              <button title="Unpin" onclick={() => act(() => removePin(hh, ii, pin.uid))}>✕</button>
            </div>
            <div class="body">
              {#each pin.collapsed ? pin.lines.slice(0, 1) : pin.lines as line, i (i)}
                <div class="line" class:clip={pin.collapsed}>{#each line as r, j (j)}<span style={css(r)}>{r.t}</span>{/each}{#if !line.length}&#8203;{/if}</div>
              {/each}
            </div>
          </div>
        {/each}
      </div>
    {:else}
      <div class="empty">
        {#if id == null}
          No instance selected.
        {:else if kind === "shell"}
          Pins work on claudes only.
        {:else}
          No pins for {label}.<br />Select text in the conversation and press ⌘⇧P.
        {/if}
      </div>
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
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.45rem 0.6rem 0.45rem 0.9rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--text-dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .who {
    font-weight: 400;
    color: var(--text-faint);
  }
  .hdr-tools {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    flex-shrink: 0;
  }
  .clear,
  .x {
    border: none;
    background: none;
    color: var(--text-dim);
    padding: 0.1rem 0.35rem;
    border-radius: 4px;
  }
  .clear {
    font-size: 0.75rem;
  }
  .x {
    font-size: 0.9rem;
  }
  .clear:hover,
  .x:hover {
    background: var(--border);
    color: var(--text);
  }
  .list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .pins {
    background: var(--pin-bg);
    color: var(--pin-fg);
  }
  .pin {
    position: relative;
    padding: 0.4rem 4.2rem 0.4rem 0.6rem;
  }
  .pin + .pin {
    border-top: 1px dashed var(--border);
  }
  /* Wrapped to the panel, and the same two RTL rules as the terminal rows
     (docs/rendering.md): a per-line base direction, and inline spans so the
     browser orders words in a run. */
  .line {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .line.clip {
    white-space: pre;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .line span {
    display: inline;
  }
  .tools {
    position: absolute;
    top: 0.3rem;
    right: 0.4rem;
    display: flex;
    gap: 0.15rem;
  }
  .tools button {
    background: transparent;
    border: none;
    color: var(--text-faint);
    font: inherit;
    cursor: pointer;
    padding: 0 0.3rem;
    border-radius: 4px;
  }
  .tools button:hover {
    color: var(--pin-fg);
    background: rgba(255, 255, 255, 0.08);
  }
  .empty {
    padding: 1.2rem 0.9rem;
    color: var(--text-faint);
    font-size: 0.85rem;
    line-height: 1.5;
  }
</style>
