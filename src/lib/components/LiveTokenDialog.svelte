<script lang="ts">
  import {
    liveTokenGet,
    liveTokenSet,
    liveTokenAddToken,
    liveTokenRenameToken,
    liveTokenDeleteToken,
    liveTokenAssign,
    liveTokenClaudes,
    type LiveTokenStatus,
    type LiveTokenClaudes,
    type LiveTokenStat,
    type LiveTokenLast,
  } from "../ipc";
  import { projects } from "../stores";

  // Mulpex ▸ Live Token… (`live_token.rs`): the token file every claude's API
  // requests take their token from by default, the command that refreshes it,
  // the pasted tokens a claude can be pinned to instead, and which claude uses
  // which. Paths are on the machine `host` names (0 = this one, else a remote
  // host).
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
  let usage = $state<LiveTokenClaudes | null>(null);
  let error = $state("");
  let busy = $state(false);
  let saved = $state(false);
  let sheet: HTMLDivElement | undefined = $state();

  let newName = $state("");
  let newToken = $state("");
  let adding = $state(false);
  let addError = $state("");
  /** The token row being renamed, and its draft. */
  let renaming = $state<string | null>(null);
  let renameDraft = $state("");
  /** The token row asking "delete?". */
  let deleting = $state<string | null>(null);
  /** Re-render "5 s ago" without waiting for a poll. */
  let now = $state(Date.now());

  $effect(() => {
    liveTokenGet(host)
      .then((s) => {
        status = s;
        tokenFile = s.token_file;
        refreshCommand = s.refresh_command;
      })
      .catch((e) => (error = String(e)));
  });

  // The claudes table follows the proxy while the dialog is open.
  $effect(() => {
    let live = true;
    const poll = () =>
      liveTokenClaudes(host)
        .then((u) => {
          if (live) usage = u;
        })
        .catch(() => {});
    void poll();
    const t = setInterval(() => {
      now = Date.now();
      void poll();
    }, 2000);
    return () => {
      live = false;
      clearInterval(t);
    };
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

  async function add() {
    if (adding) return;
    adding = true;
    addError = "";
    try {
      status = await liveTokenAddToken(host, newName, newToken);
      newName = "";
      newToken = "";
    } catch (e) {
      addError = String(e);
    } finally {
      adding = false;
    }
  }

  async function commitRename(id: string) {
    try {
      status = await liveTokenRenameToken(host, id, renameDraft);
      renaming = null;
    } catch (e) {
      error = String(e);
    }
  }

  async function remove(id: string) {
    try {
      status = await liveTokenDeleteToken(host, id);
      deleting = null;
      usage = await liveTokenClaudes(host);
    } catch (e) {
      error = String(e);
    }
  }

  async function assign(handle: number, id: number, tokenId: string | null) {
    try {
      await liveTokenAssign(handle, id, tokenId);
      usage = await liveTokenClaudes(host);
    } catch (e) {
      error = String(e);
    }
  }

  // On the window, not the sheet: a click that removes the focused button
  // (Delete, Keep) drops focus to the body, out of the sheet's reach.
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      if (renaming) renaming = null;
      else if (deleting) deleting = null;
      else onclose();
    }
  }

  /** Enter in one of a section's fields runs that section's button. */
  function enter(run: () => void) {
    return (e: KeyboardEvent) => {
      if (e.key === "Enter" && !e.isComposing) {
        e.preventDefault();
        run();
      }
    };
  }

  const on = $derived((status?.token_file ?? "") !== "");

  function tokenName(id: string): string {
    if (id === "default") return "Default";
    return status?.tokens.find((t) => t.id === id)?.name ?? "deleted token";
  }

  function ago(at: number): string {
    const s = Math.max(0, Math.round((now - at) / 1000));
    if (s < 60) return `${s}s ago`;
    if (s < 3600) return `${Math.round(s / 60)}m ago`;
    if (s < 86400) return `${Math.round(s / 3600)}h ago`;
    return `${Math.round(s / 86400)}d ago`;
  }

  /** The rate-limit windows a token's last reply reported, as "5h 42%". The
   *  headers are `anthropic-ratelimit-unified-<window>-utilization`, a 0–1
   *  fraction (measured — see docs/live-token.md). */
  function limits(st: LiveTokenStat | undefined): { win: string; pct: number; reset: string }[] {
    if (!st) return [];
    return Object.entries(st.limits)
      .filter(([k]) => k.endsWith("-utilization"))
      .map(([k, v]) => {
        const win = k.slice(0, -"-utilization".length);
        const at = Number(st.limits[`${win}-reset`]);
        const reset = Number.isFinite(at) && at > 0 ? `resets ${new Date(at * 1000).toLocaleString()}` : "";
        return { win, pct: Math.round(Number(v) * 100), reset };
      })
      .filter((l) => Number.isFinite(l.pct));
  }

  const short = (org: string | null | undefined) => (org ? org.slice(0, 8) : "");

  /** Live Token is Claude-only (a codex uses its own account, docs/codex.md):
   *  a codex row is never listed, whatever an older backend reports. */
  const claudeRows = $derived(
    (usage?.claudes ?? []).filter(
      (c) => $projects.get(c.handle)?.sessions.find((s) => s.id === c.id)?.kind !== "codex",
    ),
  );

  /** Who uses a token: the claudes pinned to it. */
  function users(id: string): string {
    return claudeRows
      .filter((c) => c.token === id)
      .map((c) => `${c.project} claude#${c.id}`)
      .join(", ");
  }

  const failed = (l: LiveTokenLast | null | undefined) => !!l && l.status >= 400;
</script>

{#snippet usageCells(st: LiveTokenStat | undefined, org: string | null | undefined)}
  <span class="mono faint" title={org ?? st?.org ?? ""}>{short(org ?? st?.org)}</span>
  <span class="limits">
    {#each limits(st) as l (l.win)}
      <span class="limit" class:hot={l.pct >= 80} title={l.reset}>{l.win} {l.pct}%</span>
    {:else}
      <span class="faint">—</span>
    {/each}
  </span>
  <span class="faint" class:bad={st && st.status >= 400}>
    {st ? `${ago(st.at)}${st.status >= 400 ? ` · ${st.status}` : ""}` : "not used yet"}
  </span>
{/snippet}

<svelte:window onkeydown={onKey} />

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    bind:this={sheet}
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Live Token"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <div class="label">Live Token{hostLabel ? ` · ${hostLabel}` : ""}</div>
    <p class="about">
      Every claude takes its Claude token from the proxy on each request, so switching accounts
      needs no restart. By default that is the token file below; a claude can be pinned to a pasted
      token instead. Leave the file empty to turn this off.
    </p>

    <div class="section">Default</div>
    <label class="field">
      <span>Token file</span>
      <input
        bind:value={tokenFile}
        oninput={() => (saved = false)}
        onkeydown={enter(() => void save())}
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
        onkeydown={enter(() => void save())}
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
    <div class="row-end">
      <button class="primary" disabled={busy} onclick={() => void save()}>Save</button>
    </div>

    {#if on}
      {#if status && !status.routes}
        <div class="warn">
          Per-claude tokens start after Mulpex quits once (⌘Q): the proxy running now is from an
          older version. Until then every claude uses the default.
        </div>
      {/if}

      <div class="section">Tokens</div>
      <div class="tokens">
        <div class="trow">
          <span class="name">Default <span class="faint">token file</span></span>
          <span class="mono faint">{usage?.tokens.default?.fp ? `#${usage.tokens.default.fp.slice(0, 6)}` : ""}</span>
          {@render usageCells(usage?.tokens.default, null)}
          <span></span>
        </div>
        {#each status?.tokens ?? [] as t (t.id)}
          <div class="trow">
            {#if renaming === t.id}
              <input
                class="rename"
                bind:value={renameDraft}
                onkeydown={enter(() => void commitRename(t.id))}
                spellcheck="false"
              />
            {:else}
              <button
                class="name link"
                title="Rename"
                onclick={() => {
                  renaming = t.id;
                  renameDraft = t.name;
                }}>{t.name}</button
              >
            {/if}
            <span class="mono faint">…{t.tail}</span>
            {@render usageCells(usage?.tokens[t.id], t.org)}
            {#if deleting === t.id}
              <span class="confirm">
                <button class="danger" onclick={() => void remove(t.id)}>Delete</button>
                <button class="plain" onclick={() => (deleting = null)}>Keep</button>
              </span>
            {:else}
              <button class="x" title="Delete" onclick={() => (deleting = t.id)}>✕</button>
            {/if}
          </div>
          {#if deleting === t.id}
            <div class="ok sub">
              {users(t.id) ? `Used by ${users(t.id)} — they move to Default.` : "No claude uses it."}
            </div>
          {/if}
        {/each}
      </div>
      <div class="add">
        <input
          bind:value={newName}
          onkeydown={enter(() => void add())}
          placeholder="Name"
          spellcheck="false"
          autocomplete="off"
        />
        <input
          class="mono"
          type="password"
          bind:value={newToken}
          onkeydown={enter(() => void add())}
          placeholder="sk-ant-… (claude setup-token)"
          spellcheck="false"
          autocomplete="off"
        />
        <button class="primary" disabled={adding} onclick={() => void add()}
          >{adding ? "Checking…" : "Add"}</button
        >
      </div>
      {#if addError}
        <div class="error">{addError}</div>
      {/if}

      <div class="section">Claudes</div>
      <div class="claudes">
        {#each claudeRows as c (`${c.handle}:${c.id}`)}
          <div class="crow">
            <span class="who">
              <span class="faint">{c.project}</span> claude#{c.id}{#if c.label}<span class="faint">
                  · {c.label}</span
                >{/if}
            </span>
            <select
              value={c.token ?? ""}
              onchange={(e) => void assign(c.handle, c.id, e.currentTarget.value || null)}
            >
              <option value="">Default</option>
              {#each status?.tokens ?? [] as t (t.id)}
                <option value={t.id}>{t.name}</option>
              {/each}
            </select>
            <span class="last" class:bad={failed(c.last)}>
              {#if !c.routed}
                <span class="warnText">restart once (⌘⇧R) to control</span>
              {:else if c.last}
                {tokenName(c.last.token)} · {ago(c.last.at)} · {c.last.status}
              {:else}
                <span class="faint">no request yet</span>
              {/if}
            </span>
          </div>
        {:else}
          <div class="faint">No claude is running.</div>
        {/each}
      </div>
    {/if}

    <div class="actions">
      <span class="hint">{on ? "On" : "Off"} · Esc to close</span>
      <button class="plain" onclick={onclose}>Close</button>
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
    width: min(46rem, 94vw);
    max-height: 88vh;
    overflow-y: auto;
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
  .section {
    color: var(--label);
    font-size: 0.74rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    margin: 1rem 0 0.45rem;
    padding-top: 0.6rem;
    border-top: 1px solid var(--border);
  }
  .about {
    margin: 0 0 0.25rem;
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
  input,
  select {
    min-width: 0;
    padding: 0.45rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
  }
  .field input,
  .mono {
    font-family: ui-monospace, monospace;
  }
  input:focus,
  select:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  select {
    padding: 0.25rem 0.4rem;
    font-size: 0.8rem;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    cursor: pointer;
  }
  .plain {
    padding: 0.4rem 0.8rem;
  }
  .error,
  .bad {
    color: #e5534b;
  }
  .error {
    margin-top: 0.25rem;
    font-size: 0.8rem;
  }
  .ok {
    margin-top: 0.25rem;
    color: var(--text-faint);
    font-size: 0.8rem;
  }
  .ok.sub {
    margin: 0 0 0.3rem;
    text-align: right;
  }
  .warn {
    margin-top: 0.75rem;
    padding: 0.45rem 0.6rem;
    border: 1px solid #c69026;
    border-radius: 6px;
    color: #c69026;
    font-size: 0.8rem;
    line-height: 1.4;
  }
  .warnText {
    color: #c69026;
  }
  .faint {
    color: var(--text-faint);
  }
  .row-end {
    display: flex;
    justify-content: flex-end;
  }
  .tokens,
  .claudes {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.82rem;
  }
  .trow {
    display: grid;
    grid-template-columns: minmax(7rem, 1.4fr) 4.5rem 5rem minmax(7rem, 1.4fr) minmax(5rem, 1fr) 4.5rem;
    align-items: center;
    gap: 0.5rem;
  }
  .trow > span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .name {
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .link {
    border: none;
    background: none;
    padding: 0;
  }
  .link:hover {
    text-decoration: underline;
  }
  .rename {
    padding: 0.2rem 0.4rem;
  }
  .limits {
    display: flex;
    gap: 0.4rem;
  }
  .limit.hot {
    color: #c69026;
  }
  .x {
    justify-self: end;
    border: none;
    background: none;
    color: var(--text-faint);
  }
  .x:hover {
    color: #e5534b;
  }
  .confirm {
    display: flex;
    gap: 0.25rem;
    justify-self: end;
  }
  .confirm button {
    padding: 0.15rem 0.45rem;
    font-size: 0.76rem;
  }
  .danger {
    border-color: #e5534b;
    color: #e5534b;
  }
  .add {
    display: grid;
    grid-template-columns: 1fr 2fr auto;
    gap: 0.4rem;
    margin-top: 0.6rem;
  }
  .crow {
    display: grid;
    grid-template-columns: minmax(0, 1.6fr) 9rem minmax(0, 1.4fr);
    align-items: center;
    gap: 0.5rem;
  }
  .who,
  .last {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    margin-top: 1rem;
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
