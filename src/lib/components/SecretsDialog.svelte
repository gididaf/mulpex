<script lang="ts">
  import { confirm } from "@tauri-apps/plugin-dialog";
  import type { SavedSet, SecretRow } from "../ipc";
  import {
    secretsAttach,
    secretsCreateEphemeral,
    secretsDelete,
    secretsGet,
    secretsList,
    secretsSave,
    secretsUpdate,
  } from "../ipc";

  // ⌘K: KEY=VALUE rows → a 0600 .env for one claude (`secrets.rs`). The values
  // never leave this dialog except into that file; `onsent` gets the key names,
  // which is all that is typed into the prompt.
  let {
    handle,
    id,
    onclose,
    onsent,
  }: {
    handle: number;
    id: number;
    onclose: () => void;
    onsent: (keys: string[]) => void;
  } = $props();

  type Row = SecretRow & { shown: boolean };
  let rows = $state<Row[]>([{ key: "", value: "", shown: false }]);
  let error = $state("");
  let busy = $state(false);
  let sheet: HTMLDivElement | undefined = $state();
  /** Saved sets usable in this project; a click hands one over as is. */
  let saved = $state<SavedSet[]>([]);
  let keep = $state(false);
  let name = $state("");
  let projectOnly = $state(false);
  /** The saved set the rows are editing, or null for a new set. */
  let editing = $state<string | null>(null);

  const reload = (h: number) => secretsList(h).then((list) => (saved = list));
  $effect(() => {
    reload(handle);
  });

  const blankRows = (): Row[] => [{ key: "", value: "", shown: false }];

  async function startEdit(set: SavedSet) {
    error = "";
    try {
      const d = await secretsGet(set.name);
      rows = d.rows.map((r) => ({ ...r, shown: false }));
      projectOnly = d.project_only;
      editing = set.name;
      sheet?.querySelector<HTMLInputElement>("input.key")?.focus();
    } catch (e) {
      error = String(e);
    }
  }

  function stopEdit() {
    editing = null;
    rows = blankRows();
    projectOnly = false;
    error = "";
  }

  async function saveEdit(filled: SecretRow[]) {
    if (editing == null) return;
    await secretsUpdate(handle, editing, projectOnly, filled);
    stopEdit();
    await reload(handle);
  }

  async function remove(set: SavedSet) {
    const ok = await confirm(
      `Delete the saved set "${set.name}"?\n\nClaudes it was sent to lose it too.`,
      { title: "Delete Secrets", kind: "warning", okLabel: "Delete", cancelLabel: "Cancel" },
    );
    sheet?.focus();
    if (!ok) return;
    try {
      await secretsDelete(set.name);
      if (editing === set.name) stopEdit();
      await reload(handle);
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    sheet?.querySelector("input")?.focus();
  });

  function addRow() {
    rows.push({ key: "", value: "", shown: false });
  }

  function removeRow(i: number) {
    rows.splice(i, 1);
    if (rows.length === 0) addRow();
  }

  /** Enter and the primary button. `sendIt` false is "Save only": the set is
   *  stored and the form clears, ready for another, with nothing typed. */
  async function send(sendIt = true) {
    if (busy) return;
    // A row with neither half filled is just the spare one at the bottom.
    const filled = rows
      .filter((r) => r.key.trim() || r.value)
      .map((r) => ({ key: r.key.trim(), value: r.value }));
    if (editing == null && keep && !name.trim()) {
      error = "Name the set to save it";
      return;
    }
    busy = true;
    error = "";
    try {
      if (editing != null) {
        await saveEdit(filled);
      } else if (keep && !sendIt) {
        await secretsSave(handle, id, name.trim(), projectOnly, filled, false);
        rows = blankRows();
        name = "";
        projectOnly = false;
        keep = false;
        await reload(handle);
      } else if (keep) {
        onsent(await secretsSave(handle, id, name.trim(), projectOnly, filled, true));
      } else {
        await secretsCreateEphemeral(handle, id, filled);
        onsent(filled.map((r) => r.key));
      }
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function sendSaved(set: SavedSet) {
    if (busy) return;
    busy = true;
    error = "";
    try {
      onsent(await secretsAttach(handle, id, set.name));
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    // Enter on a focused button presses that button, not Send.
    if (e.key === "Enter" && !e.isComposing && !(e.target instanceof HTMLButtonElement)) {
      e.preventDefault();
      void send();
    } else if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    bind:this={sheet}
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Secrets"
    onclick={(e) => e.stopPropagation()}
    onkeydown={onKey}
  >
    <div class="label">Secrets for claude #{id}</div>
    {#if saved.length > 0}
      <div class="saved">
        {#each saved as set (set.name)}
          <div class="row">
            <button
              class="set"
              class:current={editing === set.name}
              disabled={busy || editing != null}
              title="Send to claude #{id}"
              onclick={() => void sendSaved(set)}
            >
              <span class="set-name">{set.name}</span>
              <span class="set-keys">{set.keys.join(", ")}</span>
              {#if set.project_only}<span class="set-tag">this project</span>{/if}
            </button>
            <button class="icon" title="Edit" disabled={busy} onclick={() => void startEdit(set)}
              >✎</button
            >
            <button class="icon" title="Delete" disabled={busy} onclick={() => void remove(set)}
              >🗑</button
            >
          </div>
        {/each}
      </div>
      <div class="sub">{editing != null ? `editing ${editing}` : "or new"}</div>
    {/if}
    {#each rows as row, i (i)}
      <div class="row">
        <input
          class="key"
          bind:value={row.key}
          placeholder="NAME"
          spellcheck="false"
          autocomplete="off"
        />
        <input
          class="value"
          type={row.shown ? "text" : "password"}
          bind:value={row.value}
          placeholder="value"
          spellcheck="false"
          autocomplete="off"
        />
        <button
          class="icon"
          title={row.shown ? "Hide" : "Show"}
          onclick={() => (row.shown = !row.shown)}>{row.shown ? "🙈" : "👁"}</button
        >
        <button class="icon" title="Remove" onclick={() => removeRow(i)}>✕</button>
      </div>
    {/each}
    <button class="add" onclick={addRow}>+ Add row</button>
    <div class="options">
      {#if editing != null}
        <label><input type="checkbox" bind:checked={projectOnly} /> This project only</label>
      {:else}
        <label><input type="checkbox" bind:checked={keep} /> Save for reuse</label>
      {/if}
      {#if editing == null && keep}
        <input class="name" bind:value={name} placeholder="name, e.g. prod-ssh" spellcheck="false" autocomplete="off" />
        <label><input type="checkbox" bind:checked={projectOnly} /> This project only</label>
      {/if}
    </div>
    {#if error}<div class="error">{error}</div>{/if}
    <div class="actions">
      {#if editing != null}
        <span class="hint">Enter to save · claudes holding it get the new values</span>
        <span class="buttons">
          <button class="plain" onclick={stopEdit}>Cancel</button>
          <button class="primary" disabled={busy} onclick={() => void send()}>Save</button>
        </span>
      {:else}
        <span class="hint"
          >Enter to send · Esc to cancel · {keep
            ? "kept until you delete it"
            : "deleted when this claude closes"}</span
        >
        {#if keep}
          <span class="buttons">
            <button class="plain" disabled={busy} onclick={() => void send(false)}>Save only</button>
            <button class="primary" disabled={busy} onclick={() => void send()}>Save & Send</button>
          </span>
        {:else}
          <button class="primary" disabled={busy} onclick={() => void send()}>Send</button>
        {/if}
      {/if}
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
  .row {
    display: flex;
    gap: 0.4rem;
    margin-bottom: 0.4rem;
  }
  input {
    min-width: 0;
    padding: 0.45rem 0.5rem;
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
  .key {
    flex: 0 0 38%;
    font-family: ui-monospace, monospace;
  }
  .value {
    flex: 1;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    cursor: pointer;
  }
  .icon {
    flex: 0 0 2rem;
    padding: 0;
  }
  .saved {
    display: flex;
    flex-direction: column;
  }
  .set {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.45rem 0.6rem;
    text-align: left;
  }
  .set:hover:not(:disabled) {
    border-color: var(--border-focus);
  }
  .set.current {
    border-color: var(--border-focus);
  }
  .buttons {
    display: flex;
    gap: 0.4rem;
  }
  .plain {
    padding: 0.4rem 0.8rem;
  }
  .set-name {
    font-weight: 600;
  }
  .set-keys {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-faint);
    font-family: ui-monospace, monospace;
    font-size: 0.8rem;
  }
  .set-tag {
    color: var(--label);
    font-size: 0.72rem;
  }
  .sub {
    margin: 0.6rem 0 0.4rem;
    color: var(--text-faint);
    font-size: 0.74rem;
  }
  .options {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.4rem 0.8rem;
    margin-top: 0.6rem;
    font-size: 0.82rem;
  }
  .options label {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    cursor: pointer;
  }
  .name {
    flex: 1;
    min-width: 8rem;
  }
  .add {
    padding: 0.3rem 0.6rem;
    font-size: 0.8rem;
    color: var(--text-faint);
  }
  .error {
    margin-top: 0.5rem;
    color: #e5534b;
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
