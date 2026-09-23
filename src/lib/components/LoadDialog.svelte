<script lang="ts">
  import { onMount } from "svelte";
  import { confirm } from "@tauri-apps/plugin-dialog";
  import { listSaves, deleteSave, loadSave } from "../ipc";
  import type { LoadResult, ProjectHandle, SaveEntry } from "../ipc";

  // ⌘L: the saves of the active project's repo (`saves.rs::list`). Enter starts
  // a claude on the selected one. The saves' own text is Hebrew, so the list is
  // hard `dir="rtl"` — never `auto`, which a title starting with an English term
  // would flip to LTR (the Explainer's rule, docs/explainer.md).

  let {
    handle,
    onclose,
    onloaded,
  }: {
    handle: ProjectHandle;
    onclose: () => void;
    /** A claude was started on a save (or, `existing`, found already open); the
     *  caller adds the row if new and focuses it. */
    onloaded: (r: LoadResult) => void;
  } = $props();

  let entries = $state<SaveEntry[] | null>(null);
  let error = $state<string | null>(null);
  let query = $state("");
  let sel = $state(0);
  let busy = $state(false);
  /** The row whose "continue or fresh?" choice is open, and which of the two
   *  buttons (0 = continue, 1 = fresh) the keyboard is on. */
  let choosing = $state<string | null>(null);
  let pick = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();

  const shown = $derived.by(() => {
    const q = query.trim().toLowerCase();
    const all = entries ?? [];
    if (!q) return all;
    return all.filter((e) =>
      [e.title, e.description, e.author, e.file].some((f) => f.toLowerCase().includes(q)),
    );
  });

  $effect(() => {
    // Keep the selection on a real row as the filter narrows.
    if (sel >= shown.length) sel = Math.max(0, shown.length - 1);
  });

  async function refresh() {
    try {
      entries = await listSaves(handle);
      error = null;
    } catch (e) {
      error = String(e);
    }
  }

  onMount(() => {
    inputEl?.focus();
    void refresh();
  });

  /** Enter / click on a row: a save whose conversation still exists here asks
   *  first; any other loads fresh straight away. */
  function choose(e: SaveEntry | undefined) {
    if (!e || busy) return;
    if (e.continuable) {
      choosing = e.file;
      pick = 0;
    } else {
      void load(e, "fresh");
    }
  }

  async function load(e: SaveEntry, mode: "fresh" | "continue") {
    if (busy) return;
    busy = true;
    try {
      onloaded(await loadSave(handle, e.file, mode));
    } catch (err) {
      error = String(err);
      busy = false;
      choosing = null;
      inputEl?.focus();
    }
  }

  async function remove(e: SaveEntry) {
    const ok = await confirm(`Delete "${e.title}"?`, {
      title: "Delete Save",
      kind: "warning",
      okLabel: "Delete",
      cancelLabel: "Cancel",
    });
    inputEl?.focus();
    if (!ok) return;
    try {
      await deleteSave(handle, e.file);
    } catch (err) {
      error = String(err);
    }
    await refresh();
  }

  function onKey(e: KeyboardEvent) {
    if (choosing != null) {
      const entry = shown.find((x) => x.file === choosing);
      if (e.key === "Escape") {
        e.preventDefault();
        choosing = null;
      } else if (e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "Tab") {
        e.preventDefault();
        pick = 1 - pick;
      } else if (e.key === "Enter" && entry) {
        e.preventDefault();
        void load(entry, pick === 0 ? "continue" : "fresh");
      } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
        e.preventDefault();
      }
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(shown[sel]);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (shown.length) sel = (sel + 1) % shown.length;
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (shown.length) sel = (sel - 1 + shown.length) % shown.length;
    }
  }
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Load session"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <div class="label">Load Session</div>
    <input
      bind:this={inputEl}
      bind:value={query}
      oninput={() => (choosing = null)}
      onkeydown={onKey}
      dir="rtl"
      placeholder="חיפוש…"
    />
    {#if error}
      <div class="error">{error}</div>
    {/if}
    <div class="list" dir="rtl">
      {#if entries == null}
        <div class="empty">…</div>
      {:else if entries.length === 0}
        <div class="empty" dir="ltr">No saves yet. Press ⌘S on a claude to save one.</div>
      {:else if shown.length === 0}
        <div class="empty">אין תוצאות</div>
      {:else}
        {#each shown as e, i (e.file)}
          <div class="item" class:sel={i === sel}>
            <button
              class="main"
              onmouseenter={() => (sel = i)}
              onclick={() => choose(e)}
              disabled={busy}
            >
              <div class="title">
                {e.title}
                {#if e.continuable}
                  <span class="resumable" title="The original conversation is on this machine">↺</span>
                {/if}
              </div>
              {#if e.description}
                <div class="desc">{e.description}</div>
              {/if}
              <div class="meta">
                {e.updated || e.created}{e.author ? ` · ${e.author}` : ""}
              </div>
            </button>
            <button class="trash" title="Delete" onclick={() => void remove(e)}>🗑</button>
          </div>
          {#if choosing === e.file}
            <!-- LTR: the buttons are app chrome, in English like the hints. -->
            <div class="choice" dir="ltr">
              <button class:on={pick === 0} onclick={() => void load(e, "continue")} disabled={busy}>
                {e.open_as != null ? `Go to claude #${e.open_as}` : "Continue conversation"}
              </button>
              <button class:on={pick === 1} onclick={() => void load(e, "fresh")} disabled={busy}>
                Start fresh from doc
              </button>
              <span class="choice-hint">←→ · Enter · Esc</span>
            </div>
          {/if}
        {/each}
      {/if}
    </div>
    <div class="hint">Enter to load · ↑↓ to choose · Esc to close</div>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    place-items: center;
  }
  .sheet {
    width: min(34rem, 92vw);
    max-height: 80vh;
    display: flex;
    flex-direction: column;
    padding: 1rem;
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 8px;
  }
  .label {
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    margin-bottom: 0.5rem;
  }
  input {
    width: 100%;
    padding: 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
  }
  input:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .error {
    margin-top: 0.5rem;
    color: var(--warn, #e0a34a);
    font-size: 0.78rem;
  }
  .list {
    margin-top: 0.6rem;
    overflow-y: auto;
    min-height: 3rem;
  }
  .empty {
    padding: 1rem 0.25rem;
    color: var(--text-faint);
    font-size: 0.85rem;
  }
  .item {
    display: flex;
    align-items: flex-start;
    gap: 0.25rem;
    border-radius: 6px;
    border: 1px solid transparent;
  }
  .item.sel {
    background: var(--bg);
    border-color: var(--border-focus);
  }
  .main {
    flex: 1;
    min-width: 0;
    text-align: right;
    background: none;
    border: none;
    padding: 0.5rem 0.6rem;
    color: var(--text);
    cursor: pointer;
  }
  .title {
    font-weight: 600;
    font-size: 0.95rem;
  }
  .desc {
    margin-top: 2px;
    color: var(--text-dim);
    font-size: 0.85rem;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .meta {
    margin-top: 3px;
    color: var(--text-faint);
    font-size: 0.74rem;
  }
  .resumable {
    margin-inline-start: 0.3rem;
    color: var(--text-faint);
    font-weight: 400;
  }
  .choice {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0.6rem 0.55rem;
  }
  .choice button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 0.3rem 0.6rem;
    font: inherit;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .choice button.on {
    border-color: var(--border-focus);
    background: var(--bg-elev);
  }
  .choice-hint {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 0.72rem;
  }
  .trash {
    flex: none;
    margin: 0.45rem 0.35rem 0 0;
    background: none;
    border: none;
    border-radius: 4px;
    padding: 0.15rem 0.25rem;
    font-size: 0.8rem;
    opacity: 0;
    cursor: pointer;
  }
  .item:hover .trash,
  .item.sel .trash {
    opacity: 0.7;
  }
  .trash:hover {
    opacity: 1;
    background: var(--border);
  }
  .hint {
    margin-top: 0.5rem;
    color: var(--text-faint);
    font-size: 0.74rem;
  }
</style>
