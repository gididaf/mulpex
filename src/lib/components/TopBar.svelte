<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { activeProject } from "../stores";
  import type { RemoteStatus } from "../ipc";
  import { remoteStatus } from "../ipc";

  // Remote Control's state rides at the right end while it's on, so a Mac
  // that can be driven from a phone always says so. Click to open its dialog.
  let { onremote }: { onremote: () => void } = $props();

  let remote = $state<RemoteStatus | null>(null);
  $effect(() => {
    remoteStatus().then((s) => (remote = s));
    const un = listen<RemoteStatus>("remote-changed", (e) => (remote = e.payload));
    return () => {
      un.then((f) => f());
    };
  });
</script>

<header class="top">
  <span class="label">project</span>
  <span class="name">{$activeProject?.name ?? ""}</span>
  <span class="path">{$activeProject?.dir ?? ""}</span>
  {#if remote?.enabled}
    <button class="remote {remote.state}" onclick={onremote} title="Remote Control (⌘⇧O)">
      <span class="dot"></span>
      Remote{#if remote.state === "connected"}
        · {remote.clients} {remote.clients === 1 ? "phone" : "phones"}{:else if remote.state === "error"}
        · offline{/if}
    </button>
  {/if}
</header>

<style>
  .top {
    grid-area: top;
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.35rem 0.75rem;
    background: var(--bg-elev);
    border-bottom: 1px solid var(--border);
    white-space: nowrap;
    overflow: hidden;
  }
  .label {
    color: var(--label);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  .name {
    font-weight: 600;
  }
  .path {
    color: var(--text-faint);
    font-size: 0.78rem;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
  }
  .remote {
    flex: 0 0 auto;
    align-self: center;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.1rem 0.55rem;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.75rem;
    cursor: pointer;
  }
  .dot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: #d29922;
  }
  .connected .dot {
    background: #3fb950;
  }
  .error .dot {
    background: #f85149;
  }
</style>
