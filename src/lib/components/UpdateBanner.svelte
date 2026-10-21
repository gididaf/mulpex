<script lang="ts">
  // A fixed card rather than a grid row on purpose: it has to render identically
  // over the project shell AND over the picker (no project open), and a grid row
  // would only exist in the former. Bottom-right keeps it clear of the tab bar
  // and the sidebar, both of which carry live state you may be reading.
  import {
    applyUpdate,
    cancelRemoteUpdates,
    dismissUpdate,
    updateState,
    type UpdateState,
  } from "../updater";
  import { startCheckpoint } from "../checkpoint";
  import { hosts, hostUpdateCancel, hostUpdateClear, type HostUpdate } from "../hosts";
  import { projects } from "../stores";
  import { addrIn } from "../agents";

  /** `claude#3` / `codex#3` for a row the host names by project + number. */
  const addrOf = (project: string, id: number) =>
    addrIn([...$projects.values()].find((x) => x.name === project)?.sessions ?? [], id);

  /** One line per remote host being updated. */
  function stepText(u: HostUpdate | null | undefined): string {
    if (!u) return "waiting…";
    switch (u.step) {
      case "sent":
        return "asking it to update…";
      case "checking":
        return "checking for the update…";
      case "stopping": {
        const n = u.pending.length;
        return n === 0
          ? "all claudes stopped"
          : `checkpointing — ${n} still working: ${u.pending
              .map((p) => (p.name ? `${p.name} (${addrOf(p.project, p.id)})` : addrOf(p.project, p.id)))
              .join(", ")}`;
      }
      case "downloading":
        return `downloading${u.percent != null ? ` — ${u.percent}%` : "…"}`;
      case "installing":
        return "installing…";
      case "restarting":
        return "restarting…";
      case "done":
        return `back on v${u.to}`;
      case "error":
        return `failed: ${u.error ?? "unknown error"}`;
      case "cancelled":
        return "cancelled";
    }
  }

  const s = $derived($updateState as UpdateState);
  // `checking` IS shown: only a manual check ever sets it (the periodic one stays
  // on `idle`), and a menu item that shows nothing while the network hangs reads
  // as broken — the same nowhere-to-appear failure as the old picker-only errors.
  const visible = $derived(s.phase !== "idle");

  /** Hosts updating on their own — right-click › Update to v… — rather than
   *  as the first half of this Mulpex's update (`remotes`, shown above). */
  const byHand = $derived(
    s.phase === "remotes" ? [] : [...$hosts.values()].filter((h) => h.update).sort((a, b) => a.slot - b.slot),
  );
</script>

<div class="stack">
{#each byHand as h (h.slot)}
  {@const u = h.update!}
  {@const ended = u.step === "done" || u.step === "error" || u.step === "cancelled"}
  <div class="card" class:error={u.step === "error"} role="status">
    <div class="row">
      <span class="title">Updating {h.name} to v{u.to}</span>
      {#if ended}
        <button class="ghost" onclick={() => void hostUpdateClear(h.slot)}>Dismiss</button>
      {:else}
        <button class="ghost" onclick={() => void hostUpdateCancel(h.slot)}>Cancel</button>
      {/if}
    </div>
    <p class="notes host">{stepText(u)}</p>
  </div>
{/each}

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
        >, but whatever they're doing right now is lost — unless you checkpoint first:
        each claude stops at a safe point, and after the update Mulpex offers to wake
        them all.
      </p>
      <div class="row">
        <!-- ⌘⇧Q's flow, ending in this update's restart instead of a quit. -->
        <button
          class="primary"
          onclick={() => {
            dismissUpdate();
            void startCheckpoint("update");
          }}
        >
          Checkpoint &amp; update
        </button>
        <button class="ghost" onclick={() => applyUpdate(true)}>
          Restart anyway
        </button>
        <button class="ghost" onclick={dismissUpdate}>Cancel</button>
      </div>
    {:else if s.phase === "remotes"}
      <div class="row">
        <span class="title">Updating remote hosts to {s.version} first</span>
        <button class="ghost" onclick={cancelRemoteUpdates}>Cancel</button>
      </div>
      {#each s.remotes as slot (slot)}
        {@const h = $hosts.get(slot)}
        <p class="notes host">
          <b>{h?.name ?? "remote host"}</b> — {stepText(h?.update)}
        </p>
      {/each}
      <p class="notes">
        Their claudes stop at a checkpoint and wake by themselves after the restart.
        This Mulpex updates once every host is back.
      </p>
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
</div>

<style>
  .stack {
    position: fixed;
    right: 1rem;
    bottom: 2.2rem;
    z-index: 50;
    max-width: 26rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .card {
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
  .host {
    color: var(--text);
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
