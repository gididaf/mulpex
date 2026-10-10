// Swipe left/right between projects on the main screen. The page follows the
// finger, then slides out and the next project slides in; at the first or last
// project it only stretches a little and springs back.

type SwipeOpts = {
  /** Is there a project that way? -1 = previous (swipe right), +1 = next. */
  can: (dir: -1 | 1) => boolean;
  /** Switch to it — called while the page is off screen. */
  go: (dir: -1 | 1) => void;
};

/** Ignore touches this close to a screen edge: Android's back gesture. */
const EDGE = 24;
/** How far, as a share of the width, a slow drag must go to switch. */
const DISTANCE = 0.25;
/** Or how fast a flick must be, in px/ms. */
const FLICK = 0.4;
const SLIDE_MS = 180;

export function swipePages(node: HTMLElement, opts: SwipeOpts) {
  let start: { x: number; y: number; t: number } | null = null;
  /** Decided once the finger has moved: a horizontal swipe, or a scroll. */
  let axis: "x" | "y" | null = null;
  let dx = 0;
  let busy = false;

  const place = (x: number, ms = 0) => {
    node.style.transition = ms ? `transform ${ms}ms ease-out` : "";
    node.style.transform = x ? `translateX(${x}px)` : "";
  };
  const reset = () => {
    start = null;
    axis = null;
    dx = 0;
  };

  const onStart = (e: TouchEvent) => {
    reset();
    if (busy || e.touches.length !== 1) return;
    const t = e.touches[0];
    if (t.clientX < EDGE || t.clientX > innerWidth - EDGE) return;
    start = { x: t.clientX, y: t.clientY, t: e.timeStamp };
  };

  const onMove = (e: TouchEvent) => {
    if (!start) return;
    // A long-press row drag (order.ts) owns this touch.
    if (e.defaultPrevented && axis !== "x") {
      place(0);
      return reset();
    }
    const t = e.touches[0];
    const x = t.clientX - start.x;
    const y = t.clientY - start.y;
    if (!axis) {
      if (Math.hypot(x, y) < 10) return;
      axis = Math.abs(x) > Math.abs(y) * 1.5 ? "x" : "y";
    }
    if (axis !== "x") return;
    e.preventDefault();
    const dir = x < 0 ? 1 : -1;
    // No project that way: stretch a little, so the edge is felt.
    dx = opts.can(dir) ? x : x * 0.25;
    place(dx);
  };

  const onEnd = (e: TouchEvent) => {
    if (!start || axis !== "x") return reset();
    const dir: -1 | 1 = dx < 0 ? 1 : -1;
    const speed = Math.abs(dx) / Math.max(1, e.timeStamp - start.t);
    const w = node.clientWidth;
    reset();
    if (!opts.can(dir) || (Math.abs(dx) < w * DISTANCE && speed < FLICK)) {
      place(0, SLIDE_MS);
      return;
    }
    busy = true;
    place(-dir * w, SLIDE_MS);
    setTimeout(() => {
      opts.go(dir);
      place(dir * w);
      // Next frame: slide the new project in from the other side.
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          place(0, SLIDE_MS);
          setTimeout(() => (busy = false), SLIDE_MS);
        }),
      );
    }, SLIDE_MS);
  };

  node.addEventListener("touchstart", onStart, { passive: true });
  // Not passive: a horizontal swipe must not also scroll the page.
  node.addEventListener("touchmove", onMove, { passive: false });
  node.addEventListener("touchend", onEnd);
  const onCancel = () => {
    place(0, SLIDE_MS);
    reset();
  };
  node.addEventListener("touchcancel", onCancel);
  return {
    update(o: SwipeOpts) {
      opts = o;
    },
    destroy() {
      node.removeEventListener("touchstart", onStart);
      node.removeEventListener("touchmove", onMove);
      node.removeEventListener("touchend", onEnd);
      node.removeEventListener("touchcancel", onCancel);
    },
  };
}
