<script lang="ts">
  // The side panel's Pins tab (⌘⇧P): the active instance's pins, oldest first.
  // Each pin is styled as the terminal itself — same font, size, background and
  // claude's colors — so it reads like the lines it was lifted from, but wrapped
  // to the panel's width. The column itself is `RightPanel.svelte`.
  import { tick } from "svelte";
  import { activeProjectHandle, activeId, sessions, flashToast } from "../stores";
  import { pins, pinKey, removePin, togglePin, clearPins } from "../pins";
  import type { PinRun } from "../pins";
  import { FONT_FAMILY, FONT_SIZE, LINE_HEIGHT, terminals } from "../terminals";

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
</script>

<div class="tab">
  <div class="bar">
    <span class="who">{label}</span>
    {#if list.length && h != null && id != null}
      {@const hh = h}
      {@const ii = id}
      <button class="clear" title="Remove all pins of {label}" onclick={() => act(() => clearPins(hh, ii))}
        >Clear all</button
      >
    {/if}
  </div>
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
</div>

<style>
  .tab {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  .bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    min-height: 1.9rem;
    padding: 0.2rem 0.6rem 0.2rem 0.9rem;
    border-bottom: 1px solid var(--border);
  }
  .who {
    font-size: 0.75rem;
    color: var(--text-faint);
  }
  .clear {
    border: none;
    background: none;
    color: var(--text-dim);
    padding: 0.1rem 0.35rem;
    border-radius: 4px;
    font-size: 0.75rem;
  }
  .clear:hover {
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
