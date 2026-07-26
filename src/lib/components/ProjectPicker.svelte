<script lang="ts">
  import type { ClaudeStatus } from "../ipc";

  let {
    recents,
    claude,
    error,
    onpick,
    onopen,
  }: {
    recents: string[];
    /** null until the startup probe answers. */
    claude: ClaudeStatus | null;
    /** Last open-project failure, shown inline instead of dying in the console. */
    error: string | null;
    onpick: () => void;
    onopen: (path: string) => void;
  } = $props();

  function base(p: string): string {
    return p.split("/").filter(Boolean).pop() ?? p;
  }

  let missingClaude = $derived(claude !== null && !claude.found);
</script>

<div class="picker">
  <div class="card">
    <h1>Mulpex</h1>
    <p class="tag">Coordinated parallel Claude Code sessions</p>

    {#if missingClaude}
      <div class="alert">
        <strong>Claude Code CLI not found</strong>
        <p>
          Mulpex runs your own <code>claude</code>, but no <code>claude</code>
          executable was found. Install it from
          <a href="https://code.claude.com" target="_blank" rel="noreferrer"
            >code.claude.com</a
          >, make sure <code>claude --version</code> works in Terminal, then
          restart Mulpex.
        </p>
        <details>
          <summary>Where Mulpex looked</summary>
          <pre>{claude?.searched_path.split(":").join("\n")}</pre>
        </details>
      </div>
    {:else if error}
      <div class="alert">
        <strong>Couldn't open that project</strong>
        <pre>{error}</pre>
      </div>
    {/if}

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
  .alert {
    margin-bottom: 1rem;
    padding: 0.7rem 0.8rem;
    background: color-mix(in srgb, var(--danger, #ff6b6b) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--danger, #ff6b6b) 45%, transparent);
    border-radius: 6px;
    font-size: 0.85rem;
  }
  .alert p {
    margin: 0.35rem 0 0;
    color: var(--text-dim);
    line-height: 1.45;
  }
  .alert code {
    font-size: 0.8em;
    padding: 0 0.2em;
    background: var(--bg-sidebar);
    border-radius: 3px;
  }
  .alert details {
    margin-top: 0.5rem;
    color: var(--text-faint);
  }
  .alert summary {
    cursor: pointer;
    font-size: 0.78rem;
  }
  .alert pre {
    margin: 0.4rem 0 0;
    max-height: 9rem;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-all;
    font-size: 0.72rem;
    color: var(--text-faint);
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
