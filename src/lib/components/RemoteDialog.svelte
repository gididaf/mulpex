<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import type { RemoteStatus } from "../ipc";
  import { confirm } from "@tauri-apps/plugin-dialog";
  import { remoteRevokeDevice, remoteSetEnabled, remoteSetRelayUrl, remoteStatus } from "../ipc";

  // ⌘⇧O: turn Remote Control on or off and pair a phone (`remote.rs`). The
  // status is live — `remote-changed` fires on every connect, drop and join.
  let { onclose }: { onclose: () => void } = $props();

  let st = $state<RemoteStatus | null>(null);
  let url = $state("");
  let error = $state("");
  let busy = $state(false);
  let sheet: HTMLDivElement | undefined = $state();

  $effect(() => {
    remoteStatus().then((s) => {
      st = s;
      url = s.relay_url;
    });
    const un = listen<RemoteStatus>("remote-changed", (e) => (st = e.payload));
    // A pairing QR lasts 10 minutes; asking again replaces an expired one.
    const tick = setInterval(() => remoteStatus().then((s) => (st = s)), 30_000);
    sheet?.focus();
    return () => {
      clearInterval(tick);
      un.then((f) => f());
    };
  });

  async function revoke(id: string, name: string) {
    if (!(await confirm(`Remove ${name}? It will need a new QR scan to connect again.`, { title: "Remove phone", kind: "warning" })))
      return;
    st = await remoteRevokeDevice(id);
  }

  const since = (secs: number) => new Date(secs * 1000).toLocaleDateString();

  async function toggle() {
    if (!st || busy) return;
    busy = true;
    error = "";
    try {
      st = await remoteSetEnabled(!st.enabled);
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function saveUrl() {
    if (!st || url.trim() === st.relay_url) return;
    error = "";
    try {
      st = await remoteSetRelayUrl(url);
      url = st.relay_url;
    } catch (e) {
      error = String(e);
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }

  const words: Record<RemoteStatus["state"], string> = {
    off: "Off",
    connecting: "Connecting to the relay…",
    connected: "Connected",
    error: "Can't reach the relay",
  };
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={onclose}>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    bind:this={sheet}
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <div class="label">Remote Control</div>
    {#if st}
      <div class="row">
        <span class="dot {st.state}"></span>
        <span class="state">
          {words[st.state]}{#if st.state === "connected"}
            · {st.clients} {st.clients === 1 ? "phone" : "phones"}{/if}
        </span>
        <button class="toggle" class:on={st.enabled} disabled={busy} onclick={toggle}>
          {st.enabled ? "Turn off" : "Turn on"}
        </button>
      </div>
      {#if st.error}<div class="error">{st.error}</div>{/if}

      {#if st.enabled && st.qr_svg}
        <div class="qr">{@html st.qr_svg}</div>
        <div class="hint">
          To add a phone, scan this. Each code works once, for 10 minutes.
          {#if st.devices.length > 0}Phones already added just open the page again.{/if}
        </div>
      {/if}

      {#if st.devices.length > 0}
        <div class="label sub">Phones</div>
        {#each st.devices as d (d.id)}
          <div class="row device">
            <span class="dot {d.online ? 'connected' : ''}"></span>
            <span class="state">{d.name}{#if d.push} 🔔{/if} <span class="muted">· added {since(d.added)}</span></span>
            <button onclick={() => revoke(d.id, d.name)}>Remove</button>
          </div>
        {/each}
      {/if}

      <div class="label sub">Relay address</div>
      <div class="row">
        <input
          bind:value={url}
          spellcheck="false"
          onkeydown={(e) => {
            if (e.key === "Enter") saveUrl();
          }}
          onblur={saveUrl}
        />
      </div>
    {/if}
    {#if error}<div class="error">{error}</div>{/if}
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
    width: min(26rem, 92vw);
    padding: 1rem;
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 8px;
    outline: none;
  }
  .label {
    color: var(--label);
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    margin-bottom: 0.6rem;
  }
  .label.sub {
    margin-top: 1rem;
    margin-bottom: 0.4rem;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .state {
    flex: 1;
  }
  .dot {
    width: 0.6rem;
    height: 0.6rem;
    border-radius: 50%;
    background: var(--border);
  }
  .dot.connected {
    background: #3fb950;
  }
  .dot.connecting {
    background: #d29922;
  }
  .dot.error {
    background: #f85149;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    padding: 0.35rem 0.8rem;
    cursor: pointer;
  }
  button.on {
    border-color: var(--border-focus);
  }
  input {
    flex: 1;
    min-width: 0;
    padding: 0.45rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-family: ui-monospace, monospace;
  }
  input:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .qr {
    margin: 1rem auto 0.5rem;
    width: 220px;
    height: 220px;
    border-radius: 6px;
    overflow: hidden;
  }
  /* The SVG's own size grows with the link's length (more modules); it has a
     viewBox, so pinning it to the box scales the whole code to fit. */
  .qr :global(svg) {
    display: block;
    width: 100%;
    height: 100%;
  }
  .hint {
    text-align: center;
    color: var(--label);
    font-size: 0.85rem;
  }
  .device {
    margin-bottom: 0.35rem;
  }
  .muted {
    color: var(--label);
    font-size: 0.85rem;
  }
  .error {
    color: #f85149;
    font-size: 0.85rem;
    margin-top: 0.5rem;
    word-break: break-word;
  }
</style>
