<script lang="ts">
  import { liveTokenGet, liveTokenSet, type LiveTokenStatus } from "../ipc";

  // Mulpex ▸ Live Token… (`live_token.rs`): the token file every claude's API
  // requests take their token from, and the command that refreshes it. Both are
  // paths on the machine `host` names (0 = this one, else a remote host).
  let {
    host,
    hostLabel,
    onclose,
  }: {
    host: number;
    hostLabel: string | null;
    onclose: () => void;
  } = $props();

  let tokenFile = $state("");
  let refreshCommand = $state("");
  let status = $state<LiveTokenStatus | null>(null);
  let error = $state("");
  let busy = $state(false);
  let saved = $state(false);
  let sheet: HTMLDivElement | undefined = $state();

  $effect(() => {
    liveTokenGet(host)
      .then((s) => {
        status = s;
        tokenFile = s.token_file;
        refreshCommand = s.refresh_command;
      })
      .catch((e) => (error = String(e)));
  });

  $effect(() => {
    sheet?.querySelector("input")?.focus();
  });

  async function save() {
    if (busy) return;
    busy = true;
    error = "";
    try {
      status = await liveTokenSet(host, tokenFile, refreshCommand);
      tokenFile = status.token_file;
      refreshCommand = status.refresh_command;
      saved = true;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.isComposing && !(e.target instanceof HTMLButtonElement)) {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }

  const on = $derived((status?.token_file ?? "") !== "");
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    bind:this={sheet}
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Live Token"
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <div class="label">Live Token{hostLabel ? ` · ${hostLabel}` : ""}</div>
    <p class="about">
      Every claude takes its Claude token from this file on each request, so switching accounts
      needs no restart. Leave it empty to turn this off.
    </p>
    <label class="field">
      <span>Token file</span>
      <input
        bind:value={tokenFile}
        oninput={() => (saved = false)}
        placeholder="~/.claude-token-manager/token"
        spellcheck="false"
        autocomplete="off"
      />
    </label>
    <label class="field">
      <span>Refresh command <em>optional · runs every minute and on a 401/429</em></span>
      <input
        bind:value={refreshCommand}
        oninput={() => (saved = false)}
        placeholder="~/.claude-token-manager/refresh.sh"
        spellcheck="false"
        autocomplete="off"
      />
    </label>
    {#if error}
      <div class="error">{error}</div>
    {:else if on && status?.token_problem}
      <div class="error">{status.token_problem}</div>
    {:else if saved && on}
      <div class="ok">Saved. New claudes use it; restart running ones once (⌘⇧R).</div>
    {:else if saved}
      <div class="ok">Saved. Off for new claudes.</div>
    {/if}
    <div class="actions">
      <span class="hint">{on ? "On" : "Off"} · Enter to save · Esc to close</span>
      <span class="buttons">
        <button class="plain" onclick={onclose}>Close</button>
        <button class="primary" disabled={busy} onclick={() => void save()}>Save</button>
      </span>
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
    width: min(32rem, 92vw);
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
    margin-bottom: 0.5rem;
  }
  .about {
    margin: 0 0 0.75rem;
    color: var(--text-faint);
    font-size: 0.82rem;
    line-height: 1.4;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    margin-bottom: 0.6rem;
    font-size: 0.82rem;
  }
  .field em {
    color: var(--text-faint);
    font-style: normal;
    font-size: 0.74rem;
  }
  input {
    min-width: 0;
    padding: 0.45rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    font-family: ui-monospace, monospace;
  }
  input:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    cursor: pointer;
  }
  .buttons {
    display: flex;
    gap: 0.4rem;
  }
  .plain {
    padding: 0.4rem 0.8rem;
  }
  .error {
    margin-top: 0.25rem;
    color: #e5534b;
    font-size: 0.8rem;
  }
  .ok {
    margin-top: 0.25rem;
    color: var(--text-faint);
    font-size: 0.8rem;
  }
  .actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }
  .hint {
    color: var(--text-faint);
    font-size: 0.74rem;
  }
  .primary {
    padding: 0.4rem 1rem;
    border-color: var(--border-focus);
  }
  .primary:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
