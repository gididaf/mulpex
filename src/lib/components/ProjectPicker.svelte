<script lang="ts">
  let {
    recents,
    onpick,
    onopen,
  }: {
    recents: string[];
    onpick: () => void;
    onopen: (path: string) => void;
  } = $props();

  function base(p: string): string {
    return p.split("/").filter(Boolean).pop() ?? p;
  }
</script>

<div class="picker">
  <div class="card">
    <h1>Mulpex</h1>
    <p class="tag">Coordinated parallel Claude Code sessions</p>

    {#if recents.length}
      <div class="section">Recent projects</div>
      <ul class="recents">
        {#each recents as path (path)}
          <li>
            <button class="recent" onclick={() => onopen(path)}>
              <span class="name">{base(path)}</span>
              <span class="path">{path}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <button class="open" onclick={onpick}>Open Project…</button>
  </div>
</div>

<style>
  .picker {
    display: grid;
    place-items: center;
    height: 100%;
    background: var(--bg);
  }
  .card {
    width: min(30rem, 90vw);
    padding: 1.5rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-radius: 10px;
  }
  h1 {
    margin: 0;
    font-size: 1.6rem;
    letter-spacing: 0.02em;
  }
  .tag {
    margin: 0.25rem 0 1rem;
    color: var(--text-dim);
  }
  .section {
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    margin-bottom: 0.4rem;
  }
  .recents {
    list-style: none;
    margin: 0 0 1rem;
    padding: 0;
    max-height: 40vh;
    overflow-y: auto;
  }
  .recent {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    width: 100%;
    padding: 0.5rem 0.6rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    text-align: left;
  }
  .recent:hover {
    background: var(--bg-sidebar);
    border-color: var(--border);
  }
  .recent .name {
    font-weight: 600;
  }
  .recent .path {
    color: var(--text-faint);
    font-size: 0.75rem;
  }
  .open {
    width: 100%;
    padding: 0.6rem;
    background: var(--accent);
    color: #04252a;
    font-weight: 600;
    border: none;
    border-radius: 6px;
  }
  .open:hover {
    filter: brightness(1.08);
  }
</style>
