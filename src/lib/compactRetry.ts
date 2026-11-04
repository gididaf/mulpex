// Auto-compact's "one attempt per crossing" memory, kept free of the app's
// stores so `npm test` can drive it.
//
// After an attempt a claude is held back until its context reads below the
// rule's line again. That is right after a good compact (the ctx file still
// shows the old number until the statusline redraws), but after a FAILED one
// context never drops — nothing compacted it — so the claude was held back
// for good: viva-la-rts#1538 sat at 59% under a 50% rule for hours after one
// "can't find its input box". A failure now also releases after
// RETRY_AFTER_MS, and again after each further failure (no cap).

/** How long after a failed auto-compact the next attempt may start. */
export const RETRY_AFTER_MS = 10 * 60_000;

export class Attempts {
  /** key → when a retry is allowed (ms), or null: only a drop below the line releases it. */
  private held = new Map<string, number | null>();

  /** An attempt started: hold `k` until its context drops below the line. */
  started(k: string): void {
    this.held.set(k, null);
  }

  /** The attempt on `k` failed: it may also retry from `now + RETRY_AFTER_MS`.
   *  Returns that time, or null when `k` isn't held (a ⌘⇧K request, or one
   *  already released). */
  failed(k: string, now: number): number | null {
    if (!this.held.has(k)) return null;
    const at = now + RETRY_AFTER_MS;
    this.held.set(k, at);
    return at;
  }

  /** Whether `k` must not be auto-compacted now. */
  blocks(k: string, now: number): boolean {
    if (!this.held.has(k)) return false;
    const at = this.held.get(k);
    return at == null || now < at;
  }

  has(k: string): boolean {
    return this.held.has(k);
  }

  release(k: string): void {
    this.held.delete(k);
  }
}
