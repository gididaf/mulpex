<script lang="ts">
  // After a ⌘⇧Q Checkpoint & Quit: offer to wake every claude that stopped. A
  // fixed card like the update banner, so the restored panes stay readable.
  import { dismissResume, resumeTargets, wakeAll } from "../checkpoint";
  import type { CheckpointTarget } from "../ipc";

  let { onwoke }: { onwoke: (n: number) => void } = $props();

  const targets = $derived($resumeTargets as CheckpointTarget[]);
  const projects = $derived([...new Set(targets.map((t) => t.project))]);
</script>

{#if targets.length}
  <div class="card" role="dialog" aria-label="Wake checkpointed claudes">
    <span class="title">Mulpex quit at a checkpoint</span>
    <p class="note">
      {targets.length}
      {targets.length === 1 ? "claude" : "claudes"} stopped before it closed, in
      {projects.join(", ")}. Wake them to re-arm their monitors and carry on where they
      left off — each one decides itself whether anything is left to do.
    </p>
    <div class="actions">
      <button class="primary" onclick={async () => onwoke(await wakeAll())}>Wake all</button>
      <button class="ghost" onclick={dismissResume}>Not now</button>
    </div>
  </div>
{/if}

<style>
  .card {
    position: fixed;
    top: 3.2rem;
    right: 1rem;
    z-index: 60;
    width: 24rem;
    max-width: calc(100vw - 2rem);
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.75rem 0.85rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-left: 3px solid var(--dot-ready);
    border-radius: 6px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
    font-size: 0.8rem;
    color: var(--text);
  }
  .title {
    font-weight: 600;
  }
  .note {
    margin: 0;
    color: var(--text-dim);
    line-height: 1.4;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    justify-content: flex-end;
  }
  button {
    font: inherit;
    border-radius: 4px;
    padding: 0.25rem 0.7rem;
    cursor: pointer;
  }
  .primary {
    background: var(--dot-ready);
    border: 1px solid var(--dot-ready);
    color: #0b1f0b;
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
