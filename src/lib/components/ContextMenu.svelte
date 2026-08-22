<script lang="ts">
  import { tick } from "svelte";

  /**
   * The sidebar's right-click menu.
   *
   * In-app rather than a native `Menu::popup`, for the reason CLAUDE.md calls
   * out under **How this codebase fails**: a native item is only wired once
   * `lib.rs::is_forwarded` lists its id, and one that isn't listed still builds,
   * still draws and still ticks itself while the frontend never hears a thing.
   * A menu whose items are per-row (and whose set changes with the row) would be
   * paying that trap on every entry. This one is plain DOM: what it renders is
   * what it runs.
   *
   * It is deliberately *not* a general menu widget — no submenus, no icons, no
   * radio groups. Two callers, one shape.
   */
  export interface CtxItem {
    /** A separator; every other field is ignored. */
    sep?: boolean;
    label?: string;
    /** Keyboard equivalent, shown only when the key really would do this — see
     *  App.svelte::openRowMenu. Absent means "no hint", never "no shortcut". */
    hint?: string;
    /** Destructive: rendered in the `needs` red. */
    danger?: boolean;
    run?: () => void;
  }

  let {
    x,
    y,
    items,
    onclose,
  }: {
    /** Viewport coordinates of the click that opened it. */
    x: number;
    y: number;
    items: CtxItem[];
    /** Called after the menu should disappear — with the chosen item, or with
     *  nothing when it was dismissed. The caller decides where focus goes. */
    onclose: (chosen?: CtxItem) => void;
  } = $props();

  let el: HTMLElement | undefined = $state();
  /** Highlighted row for arrow-key navigation; -1 until a key or the pointer
   *  picks one, so an opened menu has nothing pre-selected to fire on Enter. */
  let cursor = $state(-1);
  /** Indices of the real (non-separator) items, in order — what the arrows walk. */
  const pickable = $derived(
    items.map((it, i) => (it.sep ? -1 : i)).filter((i) => i >= 0),
  );

  // Position is applied after mount rather than as an initial value (hence the
  // 0,0 placeholder and the `ready` flag below), because it depends on the
  // rendered size: a menu opened near the bottom or right edge
  // has to flip back over the click point rather than run off-screen (the
  // window is small enough that this is the common case for the last row, not
  // an edge case).
  let pos = $state({ left: 0, top: 0, ready: false });
  $effect(() => {
    if (!el) return;
    const r = el.getBoundingClientRect();
    const pad = 6;
    const left =
      x + r.width + pad > window.innerWidth
        ? Math.max(pad, x - r.width)
        : x;
    const top =
      y + r.height + pad > window.innerHeight
        ? Math.max(pad, y - r.height)
        : y;
    pos = { left, top, ready: true };
    // Focus AFTER `ready` reaches the DOM: `focus()` on a `visibility: hidden`
    // element is silently ignored, and the menu is hidden until measured. Doing
    // it in the same run left the menu unfocused, which cost nothing visible —
    // it just meant every keystroke went on reaching the terminal underneath.
    // Measured in headless Chrome (document.activeElement stayed BODY).
    tick().then(() => el?.focus());
  });

  function choose(it: CtxItem) {
    if (it.sep || !it.run) return;
    onclose(it);
  }

  /**
   * Bound to the window, not the menu, so the keys work even if something took
   * focus back; the menu still takes focus on open, which is what stops
   * keystrokes from reaching the terminal underneath. Every key handled here is
   * also stopped, so it can't reach App.svelte's global handler as well.
   */
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      if (!pickable.length) return;
      const at = pickable.indexOf(cursor);
      const step = e.key === "ArrowDown" ? 1 : -1;
      // From "nothing highlighted", ↓ takes the first item and ↑ the last.
      const next = at < 0 ? (step > 0 ? 0 : pickable.length - 1) : at + step;
      cursor = pickable[((next % pickable.length) + pickable.length) % pickable.length];
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      e.stopPropagation();
      const it = items[cursor];
      if (it) choose(it);
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<!-- A full-window backdrop, so any click outside dismisses without every
     surface underneath needing an outside-click listener. `contextmenu` on it
     is swallowed too: a second right-click should move the menu's owner, not
     stack a second menu over the first. -->
<div
  class="backdrop"
  role="presentation"
  onpointerdown={() => onclose()}
  oncontextmenu={(e) => {
    e.preventDefault();
    onclose();
  }}
></div>

<div
  class="menu"
  class:ready={pos.ready}
  style:left="{pos.left}px"
  style:top="{pos.top}px"
  role="menu"
  tabindex="-1"
  bind:this={el}
>
  {#each items as it, i}
    {#if it.sep}
      <div class="sep" role="separator"></div>
    {:else}
      <button
        class="item"
        class:danger={it.danger}
        class:on={cursor === i}
        role="menuitem"
        onpointerenter={() => (cursor = i)}
        onclick={() => choose(it)}
      >
        <span class="label">{it.label}</span>
        {#if it.hint}<span class="hint">{it.hint}</span>{/if}
      </button>
    {/if}
  {/each}
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 60;
  }
  .menu {
    position: fixed;
    z-index: 61;
    min-width: 11rem;
    padding: 0.25rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
    /* Hidden for the one frame between mount and measurement, so the menu is
       never seen at the unflipped position. */
    visibility: hidden;
  }
  .menu.ready {
    visibility: visible;
  }
  .menu:focus {
    outline: none;
  }
  .item {
    display: flex;
    align-items: center;
    gap: 1.25rem;
    width: 100%;
    padding: 0.3rem 0.5rem;
    background: transparent;
    border: none;
    border-radius: 5px;
    color: var(--text);
    font-size: 0.82rem;
    text-align: left;
    white-space: nowrap;
  }
  /* One highlight, driven by `cursor` — the pointer sets it on enter, so hover
     and arrow-key selection can never both be lit at once. */
  .item.on {
    background: var(--border-focus);
  }
  .item.danger {
    color: var(--dot-needs);
  }
  .label {
    flex: 1;
  }
  .hint {
    color: var(--text-faint);
    font-size: 0.75rem;
  }
  .item.on .hint {
    color: var(--text-dim);
  }
  .sep {
    height: 1px;
    margin: 0.25rem 0.35rem;
    background: var(--border);
  }
</style>
