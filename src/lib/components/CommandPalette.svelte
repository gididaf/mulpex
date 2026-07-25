<script lang="ts">
  import { projects, type ProjectHandle } from "../stores";
  import { terminals } from "../terminals";

  let {
    onselect,
    onopennew,
    onclose,
  }: {
    onselect: (h: ProjectHandle) => void;
    onopennew: () => void;
    onclose: () => void;
  } = $props();

  let query = $state("");
  let index = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();

  $effect(() => {
    inputEl?.focus();
  });

  /** Subsequence (fuzzy) match: every char of `q` appears in order in `text`. */
  function fuzzy(q: string, text: string): boolean {
    q = q.toLowerCase();
    text = text.toLowerCase();
    let i = 0;
    for (const ch of text) if (i < q.length && ch === q[i]) i++;
    return i === q.length;
  }

  const list = $derived.by(() => {
    const all = [...$projects.values()];
    const q = query.trim();
    return q ? all.filter((p) => fuzzy(q, p.name + " " + p.dir)) : all;
  });

  // Keep the highlight in range as the filtered list shrinks/grows. The synthetic
  // "Open project…" row sits at index === list.length.
  $effect(() => {
    if (index > list.length) index = list.length;
  });

  function choose(i: number) {
    if (i === list.length) {
      onopennew();
      return;
    }
    const p = list[i];
    if (p) onselect(p.handle);
  }

  function close() {
    onclose();
    terminals.refocus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      index = Math.min(index + 1, list.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      index = Math.max(index - 1, 0);
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(index);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }

  function base(p: string): string {
    return p.split("/").filter(Boolean).pop() ?? p;
  }
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={close} onkeydown={() => {}}>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Switch project"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <input
      bind:this={inputEl}
      bind:value={query}
      onkeydown={onKey}
      placeholder="Switch project…"
    />
    <ul class="results">
      {#each list as p, i (p.handle)}
        <li>
          <button
            class="row"
            class:active={i === index}
            onmouseenter={() => (index = i)}
            onclick={() => choose(i)}
          >
            <span class="name">{p.name}</span>
            <span class="path">{p.dir}</span>
          </button>
        </li>
      {/each}
      <li>
        <button
          class="row open"
          class:active={index === list.length}
          onmouseenter={() => (index = list.length)}
          onclick={() => choose(list.length)}
        >
          <span class="name">Open project…</span>
          <span class="path">pick a folder{query.trim() ? ` (“${base(query.trim())}”)` : ""}</span>
        </button>
      </li>
    </ul>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    justify-items: center;
    align-items: start;
    padding-top: 12vh;
  }
  .sheet {
    width: min(34rem, 92vw);
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 10px;
    overflow: hidden;
  }
  input {
    width: 100%;
    padding: 0.7rem 0.9rem;
    background: var(--bg);
    color: var(--text);
    border: none;
    border-bottom: 1px solid var(--border);
    font: inherit;
    font-size: 0.95rem;
  }
  input:focus {
    outline: none;
  }
  .results {
    list-style: none;
    margin: 0;
    padding: 0.3rem;
    max-height: 50vh;
    overflow-y: auto;
  }
  .row {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    width: 100%;
    padding: 0.4rem 0.55rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    text-align: left;
  }
  .row.active {
    background: var(--bg);
    border-color: var(--border-focus);
  }
  .row .name {
    font-weight: 600;
    color: var(--text);
  }
  .row.open .name {
    color: var(--accent);
  }
  .row .path {
    color: var(--text-faint);
    font-size: 0.75rem;
  }
</style>
