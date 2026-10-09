// Long-press drag to reorder the project tabs and sidebar rows. The order
// itself is the Mac's: a drop is sent there (`reorder`), committed like a
// desktop drag, and comes back in the next view.

// Before the order was the Mac's, the phone kept its own here. Gone.
try {
  localStorage.removeItem("mulpex.order");
} catch {
  // Blocked storage: nothing was kept either.
}

/** `list` with `from` moved into `to`'s slot. */
export function moveTo<T>(list: T[], from: T, to: T): T[] {
  const out = list.filter((k) => k !== from);
  const t = out.indexOf(to);
  const after = list.indexOf(from) < list.indexOf(to);
  out.splice(after ? t + 1 : t, 0, from);
  return out;
}

type DragOpts = {
  /** Which items can be dragged; each carries `data-key`. */
  selector: string;
  /** May `from` land on `to`? */
  canDrop: (from: string, to: string) => boolean;
  onDrop: (from: string, to: string) => void;
};

/**
 * Long-press (0.4 s) then drag to reorder, for touch. A tap still taps, and a
 * swipe before the press lands still scrolls. While dragging, the dragged item
 * has `data-drag="src"` and the slot it would take `data-drag="target"`.
 */
export function dragSort(node: HTMLElement, opts: DragOpts) {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let start: { x: number; y: number } | null = null;
  let src: HTMLElement | null = null;
  let target: HTMLElement | null = null;
  /** The press became a drag: swallow the click that follows it. */
  let swallow = false;

  const clearTarget = () => {
    if (target) delete target.dataset.drag;
    target = null;
  };
  const end = () => {
    if (timer) clearTimeout(timer);
    timer = null;
    start = null;
    if (src) delete src.dataset.drag;
    src = null;
    clearTarget();
  };

  const onStart = (e: TouchEvent) => {
    end();
    swallow = false;
    if (e.touches.length !== 1) return;
    const item = (e.target as HTMLElement).closest<HTMLElement>(opts.selector);
    if (!item || !node.contains(item)) return;
    const t = e.touches[0];
    start = { x: t.clientX, y: t.clientY };
    timer = setTimeout(() => {
      timer = null;
      src = item;
      src.dataset.drag = "src";
      swallow = true;
      navigator.vibrate?.(10);
    }, 400);
  };

  const onMove = (e: TouchEvent) => {
    if (!start) return;
    const t = e.touches[0];
    if (!src) {
      // Moved before the press landed: it's a scroll, not a drag.
      if (Math.hypot(t.clientX - start.x, t.clientY - start.y) > 8) end();
      return;
    }
    e.preventDefault();
    // Near the top or bottom edge, scroll the page so a long list can be reached.
    if (t.clientY < 60) window.scrollBy(0, -12);
    else if (t.clientY > innerHeight - 60) window.scrollBy(0, 12);
    const under = document.elementFromPoint(t.clientX, t.clientY)?.closest<HTMLElement>(opts.selector);
    const from = src.dataset.key!;
    const ok = under && under !== src && node.contains(under) && opts.canDrop(from, under.dataset.key!);
    if (ok ? under === target : !target) return;
    clearTarget();
    if (ok) {
      target = under!;
      target.dataset.drag = "target";
    }
  };

  const onEnd = () => {
    const from = src?.dataset.key;
    const to = target?.dataset.key;
    end();
    if (from && to) opts.onDrop(from, to);
  };

  const onClick = (e: MouseEvent) => {
    if (!swallow) return;
    swallow = false;
    e.stopPropagation();
    e.preventDefault();
  };
  // Android opens a context menu on a long press; iOS a callout.
  const onMenu = (e: Event) => {
    if (src || swallow) e.preventDefault();
  };

  node.addEventListener("touchstart", onStart, { passive: true });
  // Not passive: once dragging, the move must not also scroll the page.
  node.addEventListener("touchmove", onMove, { passive: false });
  node.addEventListener("touchend", onEnd);
  node.addEventListener("touchcancel", end);
  node.addEventListener("click", onClick, true);
  node.addEventListener("contextmenu", onMenu);
  return {
    update(o: DragOpts) {
      opts = o;
    },
    destroy() {
      end();
      node.removeEventListener("touchstart", onStart);
      node.removeEventListener("touchmove", onMove);
      node.removeEventListener("touchend", onEnd);
      node.removeEventListener("touchcancel", end);
      node.removeEventListener("click", onClick, true);
      node.removeEventListener("contextmenu", onMenu);
    },
  };
}
