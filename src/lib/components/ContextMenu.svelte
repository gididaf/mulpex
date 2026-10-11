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
   * It is deliberately *not* a general menu widget — no icons, no radio groups,
   * and submenus only one level deep (auto-compact's Off / 50 … 80 %, which as
   * five flat items read as noise). Two callers, one shape.
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
    /** A one-level submenu, opened by hover, → or Enter; the item gets a ›. */
    sub?: CtxItem[];
    /** Draw a ✓ before the label (a submenu's current choice). */
    checked?: boolean;
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

  /** The item whose submenu is open, and the highlighted row inside it (-1 =
   *  the submenu is shown but the keys still walk the main menu). */
  let openSub = $state(-1);
  let subCursor = $state(-1);
  let subEl: HTMLElement | undefined = $state();
  const subItems = $derived(openSub >= 0 ? (items[openSub]?.sub ?? []) : []);
  const subPickable = $derived(
    subItems.map((it, i) => (it.sep ? -1 : i)).filter((i) => i >= 0),
  );
  /** Flip the submenu to the left / up when it would leave the window. */
  let subFlip = $state({ left: false, up: 0 });
  $effect(() => {
    if (!subEl) return;
    subFlip = { left: false, up: 0 };
    tick().then(() => {
      if (!subEl) return;
      const r = subEl.getBoundingClientRect();
      subFlip = {
        left: r.right + 6 > window.innerWidth,
        up: Math.max(0, r.bottom + 6 - window.innerHeight),
      };
    });
  });

  function hover(i: number) {
    cursor = i;
    openSub = items[i]?.sub ? i : -1;
    subCursor = -1;
  }

  function choose(it: CtxItem, i?: number) {
    if (it.sep) return;
    if (it.sub && i != null) {
      openSub = i;
      subCursor = it.sub.findIndex((x) => !x.sep);
      return;
    }
    if (!it.run) return;
    onclose(it);
  }

  function walk(list: number[], from: number, step: number): number {
    if (!list.length) return -1;
    const at = list.indexOf(from);
    const next = at < 0 ? (step > 0 ? 0 : list.length - 1) : at + step;
    return list[((next % list.length) + list.length) % list.length];
  }

  /**
   * Bound to the window, not the menu, so the keys work even if something took
   * focus back; the menu still takes focus on open, which is what stops
   * keystrokes from reaching the terminal underneath. Every key handled here is
   * also stopped, so it can't reach App.svelte's global handler as well.
   */
  function onKey(e: KeyboardEvent) {
    // Inside an open submenu: its own arrows, and ← / Esc step back out.
    if (subCursor >= 0) {
      const handled = ["Escape", "ArrowLeft", "ArrowDown", "ArrowUp", "Enter", " "];
      if (!handled.includes(e.key)) return;
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape" || e.key === "ArrowLeft") {
        subCursor = -1;
        openSub = -1;
      } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        subCursor = walk(subPickable, subCursor, e.key === "ArrowDown" ? 1 : -1);
      } else {
        const it = subItems[subCursor];
        if (it) choose(it);
      }
      return;
    }
    if (e.key === "ArrowRight" && items[cursor]?.sub) {
      e.preventDefault();
      e.stopPropagation();
      choose(items[cursor], cursor);
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      // From "nothing highlighted", ↓ takes the first item and ↑ the last.
      cursor = walk(pickable, cursor, e.key === "ArrowDown" ? 1 : -1);
      openSub = -1;
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      e.stopPropagation();
      const it = items[cursor];
      if (it) choose(it, cursor);
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
      <div class="wrap">
        <button
          class="item"
          class:danger={it.danger}
          class:on={cursor === i}
          role="menuitem"
          aria-haspopup={it.sub ? "menu" : undefined}
          onpointerenter={() => hover(i)}
          onclick={() => choose(it, i)}
        >
          <span class="label">{it.label}</span>
          {#if it.hint}<span class="hint">{it.hint}</span>{/if}
          {#if it.sub}<span class="hint">›</span>{/if}
        </button>
        {#if it.sub && openSub === i}
          <div
            class="menu sub ready"
            class:left={subFlip.left}
            style:margin-top="-{subFlip.up}px"
            role="menu"
            bind:this={subEl}
          >
            {#each it.sub as sit, j}
              {#if sit.sep}
                <div class="sep" role="separator"></div>
              {:else}
                <button
                  class="item"
                  class:danger={sit.danger}
                  class:on={subCursor === j}
                  role="menuitem"
                  onpointerenter={() => (subCursor = j)}
                  onclick={() => choose(sit)}
                >
                  <span class="check">{sit.checked ? "✓" : ""}</span>
                  <span class="label">{sit.label}</span>
                  {#if sit.hint}<span class="hint">{sit.hint}</span>{/if}
                </button>
              {/if}
            {/each}
          </div>
        {/if}
      </div>
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
  .wrap {
    position: relative;
  }
  /* The submenu hangs off the right edge of its item (or the left, flipped). */
  .menu.sub {
    top: -0.3rem;
    left: 100%;
    margin-left: 0.15rem;
    position: absolute;
    min-width: 6rem;
  }
  .menu.sub.left {
    left: auto;
    right: 100%;
    margin-left: 0;
    margin-right: 0.15rem;
  }
  .sub .item {
    gap: 0.4rem;
  }
  .check {
    width: 0.8rem;
  }
  .sep {
    height: 1px;
    margin: 0.25rem 0.35rem;
    background: var(--border);
  }
</style>
