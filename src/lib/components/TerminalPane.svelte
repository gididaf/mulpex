<script lang="ts">
  import { onMount } from "svelte";
  import { projects, activeProject, activeProjectHandle, activeId, saves, toast } from "../stores";
  import { terminals } from "../terminals";
  import TerminalView from "./TerminalView.svelte";
  import PinStack from "./PinStack.svelte";
  import ExplainPanel from "./ExplainPanel.svelte";
  import SaveOverlay from "./SaveOverlay.svelte";

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

  onMount(() => {
    // Refit whenever the pane's size changes: initial layout, window resize,
    // and anything that shifts the sidebar. All PTYs share this geometry.
    const ro = new ResizeObserver(() => terminals.refit());
    ro.observe(paneEl);
    return () => ro.disconnect();
  });
</script>

<div class="pane-inner" bind:this={paneEl}>
  {#each all as e (e.handle + " " + e.id)}
    <TerminalView handle={e.handle} id={e.id} kind={e.kind} exited={e.exited} />
  {/each}
  <PinStack />
  {#if $activeProjectHandle != null && $activeId != null}
    <ExplainPanel handle={$activeProjectHandle} id={$activeId} />
  {/if}
  {#if activeSave && $activeId != null}
    {@const id = $activeId}
    <SaveOverlay
      {id}
      status={activeSave}
      onclose={() => onsaveclose(id)}
      onretry={() => onsaveretry(id)}
    />
  {/if}
  {#if activeEmpty}
    <div class="empty">
      Nothing running — press ⌘T for a Claude instance, ⌘⇧T for a terminal
    </div>
  {/if}
  {#if $toast}
    <div class="toast" role="status">{$toast}</div>
  {/if}
</div>

<style>
  .pane-inner {
    position: relative;
    height: 100%;
    width: 100%;
    overflow: hidden;
  }
  .empty {
    display: grid;
    place-items: center;
    height: 100%;
    color: var(--text-faint);
    font-size: 0.9rem;
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
