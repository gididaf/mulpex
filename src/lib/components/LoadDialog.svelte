<script lang="ts">
  import { onMount } from "svelte";
  import { confirm } from "@tauri-apps/plugin-dialog";
  import {
    listSaves,
    deleteSave,
    loadSave,
    listGuides,
    deleteGuide,
    loadGuide,
  } from "../ipc";
  import type { LoadResult, GuideEntry, ProjectHandle, SaveEntry } from "../ipc";

  // ⌘L: two tabs over the active project's repo.
  // - Saves (`mulpex/saves/`): unfinished work. Enter starts a claude on one.
  // - Guides (`mulpex/guides/`): pointers to recurring-incident runbooks.
  //   Enter starts a claude that reads it and asks what the user needs.
  // Both lists are hard `dir="rtl"` — never `auto`, which a title starting with
  // an English term would flip to LTR (the Explainer's rule, docs/explainer.md).

  let {
    handle,
    onclose,
    onloaded,
  }: {
    handle: ProjectHandle;
    onclose: () => void;
    /** A claude was started (or, `existing`, found already open); the caller
     *  adds the row if new and focuses it. */
    onloaded: (r: LoadResult) => void;
  } = $props();

  type Tab = "saves" | "guides";
  let tab = $state<Tab>("saves");
  let saves = $state<SaveEntry[] | null>(null);
  let guides = $state<GuideEntry[] | null>(null);
  let error = $state<string | null>(null);
  let query = $state("");
  let sel = $state(0);
  let busy = $state(false);
  /** Saves: the row whose "continue or fresh?" choice is open, and which button
   *  (0 = continue, 1 = fresh) the keyboard is on. */
  let choosing = $state<string | null>(null);
  let pick = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();

  type Row = { file: string; title: string; description: string; meta: string; mark: string };

  const rows = $derived.by((): Row[] => {
    const all: Row[] =
      tab === "saves"
        ? (saves ?? []).map((e) => ({
            file: e.file,
            title: e.title,
            description: e.description,
            meta: `${e.updated || e.created}${e.author ? ` · ${e.author}` : ""}`,
            mark: e.continuable ? "↺" : "",
          }))
        : (guides ?? []).map((e) => ({
            file: e.file,
            title: e.title,
            description: e.description,
            meta: e.missing ? `${e.source} — missing` : e.source,
            mark: "",
          }));
    const q = query.trim().toLowerCase();
    if (!q) return all;
    return all.filter((r) =>
      [r.title, r.description, r.meta, r.file].some((f) => f.toLowerCase().includes(q)),
    );
  });
  const loaded = $derived(tab === "saves" ? saves : guides);

  $effect(() => {
    // Keep the selection on a real row as the filter narrows.
    if (sel >= rows.length) sel = Math.max(0, rows.length - 1);
  });

  async function refresh() {
    try {
      [saves, guides] = await Promise.all([listSaves(handle), listGuides(handle)]);
      error = null;
    } catch (e) {
      error = String(e);
    }
  }

  onMount(() => {
    inputEl?.focus();
    void refresh();
  });

  function switchTab(t: Tab) {
    if (tab === t) return;
    tab = t;
    sel = 0;
    choosing = null;
    inputEl?.focus();
  }

  /** Enter / click on a row. */
  async function choose(r: Row | undefined) {
    if (!r || busy) return;
    if (tab === "guides") {
      void runGuide(r.file);
      return;
    }
    // A save whose conversation still exists here asks first; any other loads
    // fresh straight away.
    if (saves?.find((e) => e.file === r.file)?.continuable) {
      choosing = r.file;
      pick = 0;
    } else {
      void loadSaveAs(r.file, "fresh");
    }
  }

  async function run(f: () => Promise<LoadResult>) {
    if (busy) return;
    busy = true;
    try {
      onloaded(await f());
    } catch (err) {
      error = String(err);
      busy = false;
      choosing = null;
      inputEl?.focus();
    }
  }
  const loadSaveAs = (file: string, mode: "fresh" | "continue") =>
    run(() => loadSave(handle, file, mode));
  const runGuide = (file: string) =>
    run(async () => ({ info: await loadGuide(handle, file), existing: false }));

  async function remove(r: Row) {
    const pb = guides?.find((e) => e.file === r.file);
    const ok = await confirm(
      tab === "saves"
        ? `Delete "${r.title}"?`
        : `Retire "${r.title}"?\n\nThis deletes the doc (${pb?.source}) and its guide ` +
            `entry. Git keeps the history.`,
      {
        title: tab === "saves" ? "Delete Save" : "Retire Guide",
        kind: "warning",
        okLabel: "Delete",
        cancelLabel: "Cancel",
      },
    );
    inputEl?.focus();
    if (!ok) return;
    try {
      await (tab === "saves" ? deleteSave(handle, r.file) : deleteGuide(handle, r.file));
    } catch (err) {
      error = String(err);
    }
    await refresh();
  }

  function onKey(e: KeyboardEvent) {
    if (choosing != null) {
      if (e.key === "Escape") {
        e.preventDefault();
        choosing = null;
      } else if (e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "Tab") {
        e.preventDefault();
        pick = 1 - pick;
      } else if (e.key === "Enter") {
        e.preventDefault();
        void loadSaveAs(choosing, pick === 0 ? "continue" : "fresh");
      } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
        e.preventDefault();
      }
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    } else if (e.key === "Tab") {
      e.preventDefault();
      switchTab(tab === "saves" ? "guides" : "saves");
    } else if (e.key === "Enter") {
      e.preventDefault();
      void choose(rows[sel]);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (rows.length) sel = (sel + 1) % rows.length;
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (rows.length) sel = (sel - 1 + rows.length) % rows.length;
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
    <div class="tabs">
      <button class:on={tab === "saves"} onclick={() => switchTab("saves")}>
        Saves{saves ? ` (${saves.length})` : ""}
      </button>
      <button class:on={tab === "guides"} onclick={() => switchTab("guides")}>
        Guides{guides ? ` (${guides.length})` : ""}
      </button>
    </div>
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
      {#if loaded == null}
        <div class="empty">…</div>
      {:else if loaded.length === 0}
        <div class="empty" dir="ltr">
          {tab === "saves"
            ? "No saves yet. Press ⌘S on a claude to save one."
            : "No guides yet."}
        </div>
      {:else if rows.length === 0}
        <div class="empty">אין תוצאות</div>
      {:else}
        {#each rows as r, i (r.file)}
          <div class="item" class:sel={i === sel}>
            <button
              class="main"
              onmouseenter={() => (sel = i)}
              onclick={() => void choose(r)}
              disabled={busy}
            >
              <div class="title">
                {r.title}
                {#if r.mark}
                  <span class="resumable" title="The original conversation is on this machine"
                    >{r.mark}</span
                  >
                {/if}
              </div>
              {#if r.description}
                <div class="desc">{r.description}</div>
              {/if}
              <div class="meta" dir="auto">{r.meta}</div>
            </button>
            <button class="trash" title="Delete" onclick={() => void remove(r)}>🗑</button>
          </div>
          {#if choosing === r.file}
            <!-- LTR: the buttons are app chrome, in English like the hints. -->
            <div class="choice" dir="ltr">
              <button
                class:on={pick === 0}
                onclick={() => void loadSaveAs(r.file, "continue")}
                disabled={busy}
              >
                {saves?.find((e) => e.file === r.file)?.open_as != null
                  ? `Go to claude #${saves?.find((e) => e.file === r.file)?.open_as}`
                  : "Continue conversation"}
              </button>
              <button
                class:on={pick === 1}
                onclick={() => void loadSaveAs(r.file, "fresh")}
                disabled={busy}
              >
                Start fresh from doc
              </button>
              <span class="choice-hint">←→ · Enter · Esc</span>
            </div>
          {/if}
        {/each}
      {/if}
    </div>
    <div class="hint">Enter to load · ↑↓ to choose · Tab switches tabs · Esc to close</div>
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
  .tabs {
    display: flex;
    gap: 0.25rem;
    margin-bottom: 0.5rem;
  }
  .tabs button {
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 0.2rem 0.5rem;
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    cursor: pointer;
  }
  .tabs button.on {
    color: var(--text);
    border-bottom-color: var(--border-focus);
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
