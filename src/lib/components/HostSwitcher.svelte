<script lang="ts">
  // The host switcher, at the left end of the tab bar: this Mulpex ("Local")
  // and every remote host paired to drive (`hosts.ts`). Only drawn once a host
  // is paired. Each button carries its host's red/green badges summed over all
  // its projects — so a claude on the VM waiting for you shows while you are
  // working locally — and a dot for the connection.
  import { activeHost, hosts, type HostStatus } from "../hosts";
  import { hostBadges } from "../stores";

  let {
    onselect,
    oncontext,
  }: {
    onselect: (slot: number) => void;
    oncontext: (e: MouseEvent, slot: number) => void;
  } = $props();

  const list = $derived([...$hosts.values()].sort((a, b) => a.slot - b.slot));

  const TITLE: Record<HostStatus["state"], string> = {
    connected: "connected",
    connecting: "connecting…",
    offline: "unreachable",
    error: "refused",
    disconnected: "disconnected",
  };
</script>

{#snippet badges(slot: number)}
  {@const b = $hostBadges.get(slot)}
  {#if b && b.ready > 0}<span class="badge ready" title="{b.ready} idle">{b.ready}</span>{/if}
  {#if b && b.needs > 0}<span class="badge needs" title="{b.needs} need you">{b.needs}</span>{/if}
{/snippet}

<div class="hosts" role="tablist" aria-label="Hosts">
  <button
    class="host"
    class:active={$activeHost === 0}
    title="This Mac"
    onclick={() => onselect(0)}
  >
    <span class="icon">●</span>
    <span class="name">Local</span>
    {@render badges(0)}
  </button>
  {#each list as h (h.slot)}
    <button
      class="host {h.state}"
      class:active={$activeHost === h.slot}
      title="{h.name} — {TITLE[h.state]}{h.error ? `: ${h.error}` : ''} (right-click for options)"
      onclick={() => onselect(h.slot)}
      oncontextmenu={(e) => {
        e.preventDefault();
        oncontext(e, h.slot);
      }}
    >
      <span class="dot"></span>
      <span class="name">◆ {h.name}</span>
      {@render badges(h.slot)}
    </button>
  {/each}
</div>

<style>
  .hosts {
    flex: none;
    display: flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0 0.4rem 0.25rem 0;
    margin-right: 0.3rem;
    border-right: 1px solid var(--border);
  }
  .host {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    padding: 0.15rem 0.5rem;
    background: none;
    border: 1px solid transparent;
    border-radius: 6px;
    color: var(--text-dim);
    font-size: 0.78rem;
    white-space: nowrap;
  }
  .host:hover {
    background: var(--bg-elev);
  }
  .host.active {
    border-color: var(--border-focus);
    background: var(--bg);
    color: var(--text);
    font-weight: 600;
  }
  .icon {
    font-size: 0.55rem;
    color: var(--text-faint);
  }
  .name {
    max-width: 9rem;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: var(--border);
  }
  .connected .dot {
    background: #3fb950;
  }
  .connecting .dot {
    background: #d29922;
  }
  .error .dot {
    background: #f85149;
  }
  .offline .name,
  .disconnected .name,
  .error .name {
    opacity: 0.6;
  }
  .badge {
    flex: none;
    min-width: 1.1rem;
    padding: 0 0.25rem;
    text-align: center;
    border-radius: 0.7rem;
    font-size: 0.66rem;
    font-weight: 700;
  }
  .badge.ready {
    background: var(--dot-ready);
    color: #0c2110;
  }
  .badge.needs {
    background: var(--dot-needs);
    color: #2a0a0d;
  }
</style>
