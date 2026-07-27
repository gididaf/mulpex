<script lang="ts">
  // A fixed card rather than a grid row on purpose: it has to render identically
  // over the project shell AND over the picker (no project open), and a grid row
  // would only exist in the former. Bottom-right keeps it clear of the tab bar
  // and the sidebar, both of which carry live state you may be reading.
  import {
    applyUpdate,
    dismissUpdate,
    updateState,
    type UpdateState,
  } from "../updater";

  const s = $derived($updateState as UpdateState);
  // `checking` IS shown: only a manual check ever sets it (the periodic one stays
  // on `idle`), and a menu item that shows nothing while the network hangs reads
  // as broken — the same nowhere-to-appear failure as the old picker-only errors.
  const visible = $derived(s.phase !== "idle");
</script>

{#if visible}
  <div class="card" class:error={s.phase === "error"} role="status">
    {#if s.phase === "checking"}
      <div class="row">
        <span class="title">Checking for updates…</span>
      </div>
    {:else if s.phase === "available"}
      <div class="row">
        <span class="title">Mulpex {s.version} is available</span>
        <button class="primary" onclick={() => applyUpdate()}>
          Update &amp; Restart
        </button>
        <button class="ghost" onclick={dismissUpdate}>Later</button>
      </div>
      {#if s.notes}
        <p class="notes">{s.notes}</p>
      {/if}
    {:else if s.phase === "confirming"}
      <div class="row">
        <span class="title">
          {s.busy}
          {s.busy === 1 ? "session is" : "sessions are"} mid-turn
        </span>
      </div>
      <p class="notes">
        Updating restarts every session. They come back with <code>--resume</code
        >, but whatever they're doing right now is lost.
      </p>
      <div class="row">
        <button class="primary" onclick={() => applyUpdate(true)}>
          Restart anyway
        </button>
        <button class="ghost" onclick={dismissUpdate}>Cancel</button>
      </div>
    {:else if s.phase === "installing"}
      <div class="row">
        <span class="title">
          Installing {s.version}{s.percent != null ? ` — ${s.percent}%` : "…"}
        </span>
      </div>
      <div class="bar"><div class="fill" style:width="{s.percent ?? 0}%"></div></div>
    {:else if s.phase === "uptodate"}
      <div class="row">
        <span class="title">Mulpex is up to date</span>
      </div>
    {:else if s.phase === "error"}
      <div class="row">
        <span class="title">Update failed</span>
        <button class="ghost" onclick={dismissUpdate}>Dismiss</button>
      </div>
      <p class="notes">{s.error}</p>
    {/if}
  </div>
{/if}

<style>
  .card {
    position: fixed;
    right: 1rem;
    bottom: 2.2rem;
    z-index: 50;
    max-width: 26rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.7rem 0.85rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-left: 3px solid var(--accent);
    border-radius: 6px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
    font-size: 0.8rem;
    color: var(--text);
  }
  .card.error {
    border-left-color: var(--dot-needs);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .title {
    flex: 1;
    font-weight: 600;
  }
  .notes {
    margin: 0;
    color: var(--text-dim);
    line-height: 1.4;
    /* Release notes are arbitrary length; never let one push the card off-screen. */
    max-height: 6rem;
    overflow-y: auto;
    white-space: pre-wrap;
  }
  code {
    background: rgba(255, 255, 255, 0.07);
    border-radius: 3px;
    padding: 0 0.2rem;
  }
  button {
    font: inherit;
    border-radius: 4px;
    padding: 0.2rem 0.6rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .primary {
    background: var(--accent);
    border: 1px solid var(--accent);
    color: #06232a;
    font-weight: 600;
  }
  .ghost {
    background: transparent;
    border: 1px solid var(--border);
    color: var(--text-dim);
  }
  .ghost:hover {
    color: var(--text);
    border-color: var(--text-dim);
  }
  .bar {
    height: 4px;
    background: rgba(255, 255, 255, 0.08);
    border-radius: 2px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.15s linear;
  }
</style>
