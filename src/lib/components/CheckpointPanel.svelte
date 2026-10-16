<script lang="ts">
  // ⌘⇧Q Checkpoint & Quit. A fixed card, NOT a modal: the sidebar and the
  // terminals stay usable underneath, because a claude asking a question can only
  // be moved by the user answering it there. Rows group by project; clicking one
  // opens that claude.
  import {
    cancelCheckpoint,
    checkpointMeta,
    checkpointRows,
    checkpointSummary,
    finishCheckpoint,
    type Row,
    type RowState,
  } from "../checkpoint";

  let { onfocus }: { onfocus: (handle: number, id: number) => void } = $props();

  const meta = $derived($checkpointMeta);
  const rows = $derived($checkpointRows as Row[]);
  const sum = $derived($checkpointSummary);

  // Project order as the backend sent it (tab order), rows in sidebar-id order.
  const groups = $derived.by(() => {
    const out: { project: string; rows: Row[] }[] = [];
    for (const r of rows) {
      const g = out.find((x) => x.project === r.project);
      if (g) g.rows.push(r);
      else out.push({ project: r.project, rows: [r] });
    }
    return out;
  });

  const LABEL: Record<RowState, string> = {
    exited: "exited",
    stopped: "stopped",
    needs: "needs you",
    stopping: "stopping…",
    busy: "finishing a step",
    delivering: "message sent",
    deaf: "can't hear",
  };

  const host = $derived(meta.hostName ?? "the host");
  const title = $derived(
    meta.mode === "update" ? "Checkpoint & Update" : meta.mode === "restart" ? `Checkpoint & restart ${host}` : "Checkpoint & Quit",
  );
  const finalLabel = $derived(
    meta.mode === "update" ? "Update & Restart" : meta.mode === "restart" ? `Restart ${host}` : "Quit Mulpex",
  );
  const anywayLabel = $derived(
    meta.mode === "update" ? "Update anyway" : meta.mode === "restart" ? "Restart anyway" : "Quit anyway",
  );
</script>

<div class="card" role="dialog" aria-label="Checkpoint and quit">
  <div class="head">
    <span class="title">{title}</span>
    <span class="count" class:all={sum.allDone}>{sum.done} / {sum.total} stopped</span>
  </div>
  <div class="bar">
    <div class="fill" style:width="{sum.total ? (sum.done / sum.total) * 100 : 100}%"></div>
  </div>

  {#if sum.total === 0}
    <p class="note">No claudes are running — nothing to stop.</p>
  {:else}
    <p class="note">
      {#if meta.mode === "restart"}
        Every claude on {host} was asked to stop at a safe point, note where it is, and stop
        its background jobs. When {host} is back, they wake by themselves.
      {:else}
        Every claude was asked to stop at a safe point, note where it is, and stop its
        background jobs. Next launch, Mulpex offers to wake them all.
      {/if}
    </p>
    <div class="list">
      {#each groups as g (g.project)}
        <div class="proj">{g.project}</div>
        {#each g.rows as r (`${r.handle}:${r.id}`)}
          <button class="row" class:stuck={r.stuck} onclick={() => onfocus(r.handle, r.id)}>
            <span class="dot {r.state}"></span>
            <span class="who">
              <span class="id">claude#{r.id}</span>
              {#if r.name}<span class="name">{r.name}</span>{/if}
            </span>
            <span class="state {r.state}">
              {LABEL[r.state]}{r.state === "stopped" && r.watching ? " · monitor" : ""}
            </span>
            {#if r.hint}<span class="hint">{r.hint}</span>{/if}
          </button>
        {/each}
      {/each}
    </div>
  {/if}

  {#if meta.error}<p class="error">{meta.error}</p>{/if}

  <div class="actions">
    {#if sum.allDone}
      <button class="primary" disabled={meta.leaving || meta.lost} onclick={finishCheckpoint}>
        {meta.leaving ? "Saving…" : finalLabel}
      </button>
    {:else if sum.onlyStuckLeft}
      <button class="danger" disabled={meta.leaving || meta.lost} onclick={finishCheckpoint}>
        {anywayLabel}
      </button>
    {:else}
      <span class="wait">Waiting for {sum.total - sum.done}…</span>
    {/if}
    <button class="ghost" disabled={meta.leaving} onclick={cancelCheckpoint}>Cancel</button>
  </div>
</div>

<style>
  .card {
    position: fixed;
    top: calc(3.2rem + var(--menubar-offset));
    right: 1rem;
    z-index: 60;
    width: 25rem;
    max-width: calc(100vw - 2rem);
    max-height: calc(100vh - 5rem);
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.75rem 0.85rem;
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-left: 3px solid var(--accent);
    border-radius: 6px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
    font-size: 0.8rem;
    color: var(--text);
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
  }
  .title {
    flex: 1;
    font-weight: 600;
  }
  .count {
    color: var(--text-dim);
    font-variant-numeric: tabular-nums;
  }
  .count.all {
    color: var(--dot-ready);
    font-weight: 600;
  }
  .bar {
    height: 4px;
    background: rgba(255, 255, 255, 0.08);
    border-radius: 2px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--dot-ready);
    transition: width 0.25s ease;
  }
  .note {
    margin: 0;
    color: var(--text-dim);
    line-height: 1.4;
  }
  .list {
    overflow-y: auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .proj {
    margin-top: 0.35rem;
    color: var(--label);
    font-weight: 600;
    font-size: 0.75rem;
  }
  .row {
    all: unset;
    display: grid;
    grid-template-columns: auto 1fr auto;
    align-items: center;
    column-gap: 0.5rem;
    padding: 0.3rem 0.4rem;
    border-radius: 4px;
    cursor: pointer;
  }
  .row:hover {
    background: rgba(255, 255, 255, 0.05);
  }
  .row.stuck {
    background: rgba(224, 108, 117, 0.08);
  }
  .dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--dot-working);
  }
  .dot.stopped,
  .dot.exited {
    background: var(--dot-ready);
  }
  .dot.needs,
  .dot.deaf {
    background: var(--dot-needs);
  }
  .dot.delivering {
    background: var(--text-faint);
  }
  .who {
    display: flex;
    gap: 0.4rem;
    min-width: 0;
    overflow: hidden;
  }
  .id {
    color: var(--text-dim);
    white-space: nowrap;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    unicode-bidi: plaintext;
  }
  .state {
    color: var(--text-dim);
    white-space: nowrap;
  }
  .state.stopped,
  .state.exited {
    color: var(--dot-ready);
  }
  .state.needs,
  .state.deaf {
    color: var(--dot-needs);
  }
  .hint {
    grid-column: 2 / 4;
    color: var(--text-dim);
    font-size: 0.75rem;
    line-height: 1.35;
  }
  .error {
    margin: 0;
    color: var(--dot-needs);
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    justify-content: flex-end;
  }
  .wait {
    flex: 1;
    color: var(--text-dim);
  }
  button.primary,
  button.danger,
  button.ghost {
    font: inherit;
    border-radius: 4px;
    padding: 0.25rem 0.7rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .primary {
    background: var(--dot-ready);
    border: 1px solid var(--dot-ready);
    color: #0b1f0b;
    font-weight: 600;
  }
  .danger {
    background: transparent;
    border: 1px solid var(--dot-needs);
    color: var(--dot-needs);
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
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
