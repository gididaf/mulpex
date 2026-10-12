<script lang="ts">
  // File ▸ Connect to Remote Mulpex…: pair this Mulpex with another one, from
  // the link its ⌘⇧O dialog copies. The other Mulpex's projects then show up
  // behind its button in the host switcher (`remoteHosts.ts`).
  import { pairHost } from "../remoteHosts";
  import type { HostStatus } from "../hosts";

  let { onclose, onpaired }: { onclose: () => void; onpaired: (st: HostStatus) => void } = $props();

  let link = $state("");
  let busy = $state(false);
  let error = $state("");
  let input: HTMLTextAreaElement | undefined = $state();

  $effect(() => {
    input?.focus();
  });

  async function connect() {
    if (busy || !link.trim()) return;
    busy = true;
    error = "";
    try {
      onpaired(await pairHost(link.trim()));
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    } else if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void connect();
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={onclose}>
  <div class="sheet" role="dialog" tabindex="-1" aria-label="Connect to Remote Mulpex" onclick={(e) => e.stopPropagation()}>
    <div class="label">Connect to Remote Mulpex</div>
    <ol class="steps">
      <li>On the other machine, open Remote Control (⌘⇧O) and turn it on.</li>
      <li>Click <b>Copy link</b> there, and paste it here. A link works once, for 10 minutes.</li>
    </ol>
    <textarea
      bind:this={input}
      bind:value={link}
      rows="3"
      spellcheck="false"
      placeholder="https://mulpex.dreamvps.com/#p=…"
      onkeydown={onKey}
      disabled={busy}
    ></textarea>
    {#if error}<div class="error">{error}</div>{/if}
    <div class="row">
      <span class="muted">{busy ? "Pairing…" : ""}</span>
      <button onclick={onclose}>Cancel</button>
      <button class="primary" disabled={busy || !link.trim()} onclick={connect}>Connect</button>
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
    z-index: 50;
  }
  .sheet {
    width: min(30rem, 92vw);
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
    margin-bottom: 0.6rem;
  }
  .steps {
    margin: 0 0 0.75rem;
    padding-left: 1.2rem;
    color: var(--text-dim);
    font-size: 0.88rem;
    line-height: 1.5;
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    padding: 0.45rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
    resize: none;
    word-break: break-all;
  }
  textarea:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }
  .muted {
    flex: 1;
    color: var(--label);
    font-size: 0.85rem;
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
  button.primary {
    border-color: var(--border-focus);
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .error {
    color: #f85149;
    font-size: 0.85rem;
    margin-top: 0.5rem;
    word-break: break-word;
  }
</style>
