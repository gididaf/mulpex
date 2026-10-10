<script lang="ts">
  // Shown at launch while Mulpex lacks Full Disk Access. Without it, a claude that
  // reads another app's data pops a macOS modal that blocks it until someone
  // clicks it on the Mac — invisible from Remote Control (src-tauri/src/fda.rs).
  // Top-right so it never stacks onto the update card (bottom-right).
  import { dismissFda, fdaState, openFda } from "../fda";
</script>

{#if $fdaState === "missing" || $fdaState === "requested"}
  <div class="card" role="status">
    {#if $fdaState === "missing"}
      <div class="row">
        <span class="title">Mulpex has no Full Disk Access</span>
        <button class="primary" onclick={openFda}>Open Settings</button>
        <button class="ghost" onclick={dismissFda}>✕</button>
      </div>
      <p class="notes">
        Without it, macOS can stop a claude with an "access data from other apps"
        dialog that only the Mac can answer.
      </p>
    {:else}
      <div class="row">
        <span class="title">Granted? Restart Mulpex to apply.</span>
        <button class="ghost" onclick={dismissFda}>✕</button>
      </div>
      <p class="notes">
        Turn on Mulpex in Full Disk Access. macOS applies it on the next launch.
      </p>
    {/if}
  </div>
{/if}

<style>
  .card {
    position: fixed;
    right: 1rem;
    top: 3rem;
    z-index: 50;
    max-width: 26rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.7rem 0.85rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-left: 3px solid var(--dot-needs);
    border-radius: 6px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
    font-size: 0.8rem;
    color: var(--text);
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
</style>
