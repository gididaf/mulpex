<script lang="ts">
  import type { SaveStatus } from "../stores";

  // ⌘S stopped this claude (`Core::begin_save`); its pane stays locked under
  // this card until the save ends and the user closes the row. Covering the
  // whole pane is also what keeps clicks off the dead xterm.
  let {
    id,
    status,
    onclose,
    onretry,
  }: {
    id: number;
    status: SaveStatus;
    onclose: () => void;
    onretry: () => void;
  } = $props();

  const STEP = { writing: "writing", checking: "checking", fixing: "fixing" } as const;
  const running = $derived(status.state in STEP);
</script>

<div class="lock" role="dialog" aria-label="Saving claude #{id}">
  <div class="card">
    {#if running}
      <div class="head">Saving claude #{id}…</div>
      <div class="step">{status.state}</div>
      <div class="note">This claude is stopped. You can close it once the save is done.</div>
    {:else if status.state === "done"}
      <div class="head ok">Saved ✓</div>
      {#if status.title}<div class="title" dir="rtl">{status.title}</div>{/if}
      <div class="note">Continue it any time from ⌘L.</div>
      <button class="btn primary" onclick={onclose}>Close</button>
    {:else}
      <div class="head err">Save failed</div>
      {#if status.detail}<div class="note">{status.detail}</div>{/if}
      <button class="btn primary" onclick={onretry}>Retry</button>
    {/if}
  </div>
</div>

<style>
  .lock {
    position: absolute;
    inset: 0;
    z-index: 5;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.6);
  }
  .card {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    max-width: min(28rem, calc(100% - 2rem));
    padding: 1.1rem 1.4rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elev);
    color: var(--text);
    text-align: center;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.6);
  }
  .head {
    font-size: 1rem;
    font-weight: 600;
  }
  .head.ok {
    color: var(--dot-ready);
  }
  .head.err {
    color: var(--warn, #e0a34a);
  }
  .step {
    color: var(--dot-working);
    font-size: 0.85rem;
  }
  .title {
    font-size: 0.95rem;
  }
  .note {
    color: var(--text-dim);
    font-size: 0.8rem;
    overflow-wrap: anywhere;
  }
  .btn {
    margin-top: 0.35rem;
    padding: 0.3rem 1.1rem;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: none;
    color: var(--text);
    font-size: 0.85rem;
  }
  .btn.primary {
    border-color: var(--accent);
  }
  .btn:hover {
    background: var(--border);
  }
</style>
