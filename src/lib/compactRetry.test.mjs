// `npm test`: auto-compact's retry memory (compactRetry.ts).

import { test } from "node:test";
import assert from "node:assert/strict";
import { Attempts, RETRY_AFTER_MS } from "./compactRetry.ts";

const T = 1_000_000;

test("a claude never tried is not blocked", () => {
  assert.equal(new Attempts().blocks("1:7", T), false);
});

test("after a good attempt only a release unblocks it, however long it waits", () => {
  const a = new Attempts();
  a.started("1:7");
  assert.equal(a.blocks("1:7", T + 24 * 3600_000), true);
  a.release("1:7");
  assert.equal(a.blocks("1:7", T), false);
});

test("a failed attempt retries after the cooldown, and again after each failure", () => {
  const a = new Attempts();
  a.started("1:7");
  assert.equal(a.failed("1:7", T), T + RETRY_AFTER_MS);
  assert.equal(a.blocks("1:7", T + RETRY_AFTER_MS - 1), true);
  assert.equal(a.blocks("1:7", T + RETRY_AFTER_MS), false);
  // The retry starts and fails too: no cap, another cooldown.
  const t2 = T + RETRY_AFTER_MS;
  a.started("1:7");
  assert.equal(a.blocks("1:7", t2 + 5 * RETRY_AFTER_MS), true, "a started retry waits for its outcome");
  a.failed("1:7", t2);
  assert.equal(a.blocks("1:7", t2 + RETRY_AFTER_MS), false);
});

test("a failure on a claude no rule is holding (a ⌘⇧K request) schedules nothing", () => {
  const a = new Attempts();
  assert.equal(a.failed("1:7", T), null);
  assert.equal(a.blocks("1:7", T), false);
});

test("a drop below the line still releases a failed one at once", () => {
  const a = new Attempts();
  a.started("1:7");
  a.failed("1:7", T);
  a.release("1:7");
  assert.equal(a.has("1:7"), false);
  assert.equal(a.blocks("1:7", T), false);
});

test("claudes are tracked separately", () => {
  const a = new Attempts();
  a.started("1:7");
  a.failed("1:7", T);
  a.started("1:8");
  assert.equal(a.blocks("1:7", T + RETRY_AFTER_MS), false);
  assert.equal(a.blocks("1:8", T + RETRY_AFTER_MS), true);
});
