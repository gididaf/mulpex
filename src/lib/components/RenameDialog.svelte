<script lang="ts">
  import { rename, sessions } from "../stores";
  import { renameSession } from "../ipc";
  import { terminals } from "../terminals";
  import { get } from "svelte/store";

  let value = $state(get(rename)?.value ?? "");
  let inputEl: HTMLInputElement | undefined = $state();

  $effect(() => {
    inputEl?.focus();
    inputEl?.select();
  });

  function cancel() {
    rename.set(null);
    terminals.refocus();
  }

  async function save() {
    const r = get(rename);
    if (!r) return;
    const name = value.trim();
    await renameSession(r.id, name);
    sessions.update((l) =>
      l.map((s) => (s.id === r.id ? { ...s, name: name || null } : s)),
    );
    rename.set(null);
    terminals.refocus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    }
  }
</script>

<div
  class="backdrop"
  role="button"
  tabindex="-1"
  onclick={cancel}
  onkeydown={() => {}}
>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Rename session"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <div class="label">Rename claude #{$rename?.id}</div>
    <input
      bind:this={inputEl}
      bind:value
      onkeydown={onKey}
      placeholder="name (empty clears)"
    />
    <div class="hint">Enter to save · Esc to cancel · empty restores the task line</div>
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
    width: min(24rem, 90vw);
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
  input {
    width: 100%;
    padding: 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
  }
  input:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  .hint {
    margin-top: 0.5rem;
    color: var(--text-faint);
    font-size: 0.74rem;
  }
</style>
