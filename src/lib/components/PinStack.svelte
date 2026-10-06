<script lang="ts">
  // The active instance's pin (⌘⇧P, one at a time), floating over the top of its pane. Styled
  // as the terminal itself — same font, size, background and claude's colors —
  // so a pinned block reads exactly like the lines it was lifted from.
  //
  // Lives in `.pane-inner` beside the xterms, never inside one: xterm owns its
  // container's DOM after `open()`.
  import { activeProjectHandle, activeId, flashToast } from "../stores";
  import { pins, pinKey, removePin, togglePin } from "../pins";
  import type { PinRun } from "../pins";
  import { THEME, FONT_FAMILY, FONT_SIZE, LINE_HEIGHT, terminals } from "../terminals";

  const list = $derived(
    $activeProjectHandle != null && $activeId != null
      ? ($pins.get(pinKey($activeProjectHandle, $activeId)) ?? [])
      : [],
  );

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

{#if list.length && $activeProjectHandle != null && $activeId != null}
  {@const h = $activeProjectHandle}
  {@const id = $activeId}
  <div
    class="stack"
    style="--pin-bg:{THEME.background};--pin-fg:{THEME.foreground};font-family:{FONT_FAMILY};font-size:{FONT_SIZE}px;line-height:{LINE_HEIGHT};"
  >
    {#each list as pin (pin.uid)}
      <div class="pin">
        <div class="tools">
          <button title="Copy" onclick={() => copy(pin.text)}>⧉</button>
          <button
            title={pin.collapsed ? "Expand" : "Collapse"}
            onclick={() => act(() => togglePin(h, id, pin.uid))}>{pin.collapsed ? "▸" : "▾"}</button
          >
          <button title="Unpin" onclick={() => act(() => removePin(h, id, pin.uid))}>✕</button>
        </div>
        <div class="body">
          {#each pin.collapsed ? pin.lines.slice(0, 1) : pin.lines as line, i (i)}
            <div class="line">{#each line as r, j (j)}<span style={css(r)}>{r.t}</span>{/each}{#if !line.length}&#8203;{/if}</div>
          {/each}
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .stack {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    max-height: 40%;
    overflow-y: auto;
    z-index: 5; /* over the terminals, under the toast (10) */
    background: var(--pin-bg);
    color: var(--pin-fg);
    border-bottom: 1px solid var(--border);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.6);
  }
  .pin {
    position: relative;
    padding: 0.35rem 4.5rem 0.35rem 0.5rem;
  }
  .pin + .pin {
    border-top: 1px dashed var(--border);
  }
  .body {
    overflow-x: auto;
  }
  /* Same two RTL rules as the terminal rows (docs/rendering.md): a per-line
     base direction, and inline spans so the browser orders words in a run. */
  .line {
    white-space: pre;
    unicode-bidi: plaintext;
  }
  .line span {
    display: inline;
  }
  .tools {
    position: absolute;
    top: 0.25rem;
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
</style>
