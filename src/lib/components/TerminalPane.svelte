<script lang="ts">
  import { onMount } from "svelte";
  import { projects, activeProject, activeId, saves, toast } from "../stores";
  import { terminals } from "../terminals";
  import TerminalView from "./TerminalView.svelte";
  import RightPanel from "./RightPanel.svelte";
  import { rightPanel } from "../rightPanel";
  import SaveOverlay from "./SaveOverlay.svelte";
  import { activeHost, activeHostOnline, geometryDriver, hostName, hosts } from "../hosts";
  import { claimHost } from "../remoteHosts";
  import { geometryTakeBack } from "../ipc";

  let {
    onsaveclose,
    onsaveretry,
  }: {
    /** Close a row ⌘S holds, once its save is done. */
    onsaveclose: (id: number) => void;
    /** Re-run its failed save. */
    onsaveretry: (id: number) => void;
  } = $props();

  let paneEl: HTMLDivElement;
  let rowEl: HTMLDivElement | undefined = $state();
  const activeSave = $derived($activeId != null ? $saves.get($activeId) : undefined);

  // Every session across ALL open projects, so background projects' terminals
  // stay mounted (alive-while-hidden). Only the active project's active session
  // is visible; the rest are visibility:hidden but keep rendering.
  const all = $derived(
    [...$projects.values()].flatMap((p) =>
      p.sessions.map((s) => ({
        handle: p.handle,
        id: s.id,
        kind: s.kind,
        exited: s.exited,
      })),
    ),
  );
  const activeEmpty = $derived(($activeProject?.sessions.length ?? 0) === 0);
  // Remote Hosts: who sizes the panes in front, and whether their host is up.
  const hostSt = $derived($activeHost ? $hosts.get($activeHost) : undefined);
  const notMine = $derived(!!hostSt && hostSt.state === "connected" && !!hostSt.geo && !hostSt.geo.mine);

  onMount(() => {
    // Refit whenever the pane's size changes: initial layout, window resize,
    // and anything that shifts the sidebar. All PTYs share this geometry.
    const ro = new ResizeObserver(() => terminals.refit());
    ro.observe(paneEl);
    return () => ro.disconnect();
  });
</script>

<!-- The terminals and, when shown, the side panel (To Do · Pins · Explain)
     beside them. -->
<div class="pane-row" bind:this={rowEl}>
  <div class="pane-inner" bind:this={paneEl}>
    {#each all as e (e.handle + " " + e.id)}
      <TerminalView handle={e.handle} id={e.id} kind={e.kind} exited={e.exited} />
    {/each}
    {#if activeSave && $activeId != null}
      {@const id = $activeId}
      <SaveOverlay
        {id}
        status={activeSave}
        onclose={() => onsaveclose(id)}
        onretry={() => onsaveretry(id)}
      />
    {/if}
    {#if !$activeProject}
      <div class="empty">
        {#if !$activeHost}
          No projects open on this Mac — press ⌘O to open one
        {:else if $activeHostOnline}
          No projects open on ◆ {hostName($activeHost)} — press ⌘O to open one
        {:else}
          ◆ {hostName($activeHost)} is {hostSt?.state === "connecting" ? "connecting…" : "not reachable"}{hostSt?.error ? ` — ${hostSt.error}` : ""}
        {/if}
      </div>
    {:else if activeEmpty}
      <div class="empty">
        Nothing running — press ⌘T for a Claude instance, ⌘⇧T for a terminal
      </div>
    {/if}
    {#if $activeProject && $activeHost && !$activeHostOnline}
      <div class="banner">
        ◆ {hostName($activeHost)} is {hostSt?.state === "connecting" ? "reconnecting…" : "offline"} — these panes are frozen{hostSt?.error ? ` (${hostSt.error})` : ""}
      </div>
    {:else if notMine}
      <div class="banner">
        ◆ {hostName($activeHost)}'s panes are sized by {hostSt?.geo?.owner ?? "its own window"}
        <button onclick={() => claimHost($activeHost)}>Take over</button>
      </div>
    {:else if !$activeHost && $geometryDriver}
      <div class="banner">
        {$geometryDriver} is driving this Mulpex — panes are shown at its size
        <button onclick={() => geometryTakeBack()}>Take back</button>
      </div>
    {/if}
    {#if $toast}
      <div class="toast" role="status">{$toast}</div>
    {/if}
  </div>
  {#if $rightPanel.open}
    <div class="side-col" style="width:{$rightPanel.frac * 100}%">
      <RightPanel row={rowEl} />
    </div>
  {/if}
</div>

<style>
  .pane-row {
    position: relative;
    display: flex;
    height: 100%;
    width: 100%;
    overflow: hidden;
  }
  .pane-inner {
    position: relative;
    flex: 1;
    min-width: 0;
    height: 100%;
    overflow: hidden;
  }
  .side-col {
    flex: none;
    min-width: min(16rem, 50%);
    height: 100%;
  }
  .empty {
    display: grid;
    place-items: center;
    height: 100%;
    color: var(--text-faint);
    font-size: 0.9rem;
  }
  .banner {
    position: absolute;
    bottom: 0.6rem;
    left: 50%;
    transform: translateX(-50%);
    z-index: 9;
    display: flex;
    align-items: center;
    gap: 0.6rem;
    max-width: calc(100% - 2rem);
    padding: 0.35rem 0.5rem 0.35rem 0.8rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elev);
    color: var(--text-dim);
    font-size: 0.8rem;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.5);
    white-space: nowrap;
  }
  .banner button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border-focus);
    border-radius: 5px;
    font: inherit;
    padding: 0.15rem 0.6rem;
    cursor: pointer;
  }
  .toast {
    position: absolute;
    top: 0.75rem;
    left: 50%;
    transform: translateX(-50%);
    z-index: 10;
    max-width: calc(100% - 2rem);
    padding: 0.45rem 0.9rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elev);
    color: var(--text);
    font-size: 0.85rem;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.5);
    pointer-events: none;
  }
</style>
