<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { confirm } from "@tauri-apps/plugin-dialog";
  import { importStart, importState, importDiscard, importApply } from "../ipc";
  import type {
    ApplyReport,
    ImportDecision,
    ImportKind,
    ImportState,
    ProjectHandle,
  } from "../ipc";

  // File ▸ Import Docs…: the review list of `docs_import.rs`. The job lives in
  // the backend, so closing this window keeps it running and reopening shows
  // where it got to. Nothing is written until Apply, and only ticked rows are.

  let {
    handle,
    onclose,
    onapplied,
  }: {
    handle: ProjectHandle;
    onclose: () => void;
    onapplied: (r: ApplyReport) => void;
  } = $props();

  const KINDS: Array<[ImportKind, string]> = [
    ["save", "Save — move into mulpex/saves"],
    ["guide", "Guide — keep, add to ⌘L"],
    ["stale", "Stale — delete the file"],
    ["skip", "Skip — don't ask again"],
  ];

  let st = $state<ImportState | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  /** The user's edits per file, seeded from the sorter's answer when it lands. */
  type Edit = { tick: boolean; kind: ImportKind; title: string; description: string };
  let edits = $state<Record<string, Edit>>({});

  function absorb(next: ImportState | null) {
    st = next;
    for (const it of next?.items ?? []) {
      if (it.status === "done" && !edits[it.file] && it.kind) {
        edits[it.file] = {
          tick: true,
          kind: it.kind,
          title: it.title,
          description: it.description,
        };
      }
    }
  }

  onMount(() => {
    let un: (() => void) | undefined;
    void (async () => {
      try {
        absorb(await importStart(handle));
      } catch (e) {
        error = String(e);
      }
      un = await listen<number>("import-update", async (e) => {
        if (e.payload === handle) absorb(await importState(handle));
      });
    })();
    return () => un?.();
  });

  const done = $derived(st?.items.filter((i) => i.status !== "pending").length ?? 0);
  const total = $derived(st?.items.length ?? 0);
  const ticked = $derived(
    (st?.items ?? []).filter((i) => i.status === "done" && edits[i.file]?.tick),
  );

  async function apply() {
    if (!st || busy) return;
    const decisions: ImportDecision[] = ticked.map((i) => ({
      file: i.file,
      kind: edits[i.file].kind,
      slug: i.slug,
      title: edits[i.file].title,
      description: edits[i.file].description,
    }));
    const n = (k: ImportKind) => decisions.filter((d) => d.kind === k).length;
    const ok = await confirm(
      `Apply ${decisions.length} change(s)?\n\n` +
        `${n("save")} moved into mulpex/saves (originals deleted)\n` +
        `${n("guide")} added as guides\n` +
        `${n("stale")} deleted\n` +
        `${n("skip")} remembered as skip\n\n` +
        `Everything is committed as ONE commit (only these files), so it is easy to ` +
        `review or revert.`,
      { title: "Import Docs", kind: "warning", okLabel: "Apply", cancelLabel: "Cancel" },
    );
    if (!ok) return;
    busy = true;
    try {
      onapplied(await importApply(handle, decisions));
    } catch (e) {
      error = String(e);
      busy = false;
    }
  }

  async function discard() {
    const ok = await confirm("Throw this import away? Nothing has been written.", {
      title: "Discard Import",
      kind: "warning",
      okLabel: "Discard",
      cancelLabel: "Keep",
    });
    if (!ok) return;
    await importDiscard(handle);
    onclose();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Import docs"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <div class="head">
      <span class="label">Import Docs</span>
      {#if st && st.running}
        <span class="progress">Sorting {done}/{total}… you can close this and come back.</span>
      {:else if st}
        <span class="progress">{total} file(s) sorted. Review, then Apply.</span>
      {/if}
    </div>
    {#if error}
      <div class="error">{error}</div>
    {/if}
    <div class="list">
      {#if st == null}
        <div class="empty">Scanning…</div>
      {:else if st.items.length === 0}
        <div class="empty">Nothing to import — every markdown file here is already handled.</div>
      {:else}
        {#each st.items as it (it.file)}
          {@const ed = edits[it.file]}
          <div class="item" class:off={ed && !ed.tick}>
            <div class="top">
              {#if ed}
                <input type="checkbox" bind:checked={ed.tick} />
              {:else}
                <span class="spacer"></span>
              {/if}
              <span class="file">{it.file}</span>
              {#if it.status === "pending"}
                <span class="muted">sorting…</span>
              {:else if it.status === "error"}
                <span class="err" title={it.error ?? ""}>failed: {it.error}</span>
              {:else if ed}
                <select bind:value={ed.kind}>
                  {#each KINDS as [k, label] (k)}
                    <!-- The sorter's pick stays marked, so a slip of the picker
                         can be undone without re-sorting. -->
                    <option value={k}>{label}{it.kind === k ? "  (RECOMMENDED)" : ""}</option>
                  {/each}
                </select>
              {/if}
            </div>
            {#if ed}
              <div class="fields">
                <input class="title" dir="rtl" bind:value={ed.title} placeholder="כותרת" />
                <input class="desc" dir="rtl" bind:value={ed.description} placeholder="תיאור" />
                {#if it.reason}
                  <div class="reason">{it.reason}</div>
                {/if}
              </div>
            {/if}
          </div>
        {/each}
      {/if}
    </div>
    <div class="foot">
      <button onclick={discard} disabled={busy || st == null || total === 0}>Discard</button>
      <span class="grow"></span>
      <button onclick={onclose}>Close</button>
      <button
        class="primary"
        onclick={apply}
        disabled={busy || !st || st.running || ticked.length === 0}
        title={st?.running ? "Wait for sorting to finish" : ""}
      >
        Apply ({ticked.length})
      </button>
    </div>
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
    width: min(60rem, 95vw);
    max-height: 88vh;
    display: flex;
    flex-direction: column;
    padding: 1rem;
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 8px;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 0.75rem;
    margin-bottom: 0.5rem;
  }
  .label {
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .progress,
  .muted {
    color: var(--text-faint);
    font-size: 0.78rem;
  }
  .error,
  .err {
    color: var(--warn, #e0a34a);
    font-size: 0.78rem;
  }
  .err {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .list {
    overflow-y: auto;
    min-height: 4rem;
  }
  .empty {
    padding: 1rem 0.25rem;
    color: var(--text-faint);
    font-size: 0.85rem;
  }
  .item {
    padding: 0.5rem 0.4rem;
    border-bottom: 1px solid var(--border);
  }
  .item.off .fields,
  .item.off .file {
    opacity: 0.45;
  }
  .top {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  /* Drawn in CSS, not by macOS: WebKit painted native boxes that were checked
     while the window sat still in the inactive (grey) style, and only repainted
     them when the window lost and regained focus. `appearance: none` takes the
     native paint path out entirely. */
  .top input[type="checkbox"] {
    appearance: none;
    -webkit-appearance: none;
    flex: none;
    width: 13px;
    height: 13px;
    margin: 0;
    border: 1px solid var(--border);
    border-radius: 3px;
    background: var(--bg);
    display: grid;
    place-items: center;
    cursor: pointer;
  }
  .top input[type="checkbox"]:checked {
    background: var(--border-focus);
    border-color: var(--border-focus);
  }
  .top input[type="checkbox"]:checked::after {
    content: "";
    width: 3px;
    height: 7px;
    margin-top: -2px;
    border: solid var(--bg);
    border-width: 0 2px 2px 0;
    transform: rotate(45deg);
  }
  .spacer {
    width: 13px;
  }
  .file {
    flex: 1;
    min-width: 0;
    font-family: var(--mono, monospace);
    font-size: 0.78rem;
    color: var(--text-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  select,
  .fields input {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 5px;
    font: inherit;
    font-size: 0.8rem;
    padding: 0.2rem 0.35rem;
  }
  .fields {
    display: grid;
    grid-template-columns: 1fr 2fr;
    gap: 0.35rem;
    margin: 0.35rem 0 0 calc(13px + 0.5rem);
  }
  .fields .title {
    font-weight: 600;
  }
  .reason {
    grid-column: 1 / -1;
    color: var(--text-faint);
    font-size: 0.74rem;
  }
  .foot {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }
  .grow {
    flex: 1;
  }
  .foot button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 0.35rem 0.8rem;
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .foot button.primary {
    border-color: var(--border-focus);
  }
  .foot button:disabled {
    opacity: 0.45;
    cursor: default;
  }
</style>
