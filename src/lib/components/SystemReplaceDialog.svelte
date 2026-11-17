<script lang="ts">
  import {
    systemReplaceGet,
    systemReplaceSet,
    systemReplaceTest,
    type SystemReplaceRule,
    type SystemReplaceTestResult,
  } from "../ipc";

  // Mulpex ▸ System Text Replacements… (`system_replace.rs`): `--replace OLD NEW`
  // pairs added to every interactive claude this Mulpex starts, for a launcher in
  // front of `claude` that understands them. Off by default. Saved on the machine
  // `host` names (0 = this one, else a remote host).
  let {
    host,
    hostLabel,
    onclose,
  }: {
    host: number;
    hostLabel: string | null;
    onclose: () => void;
  } = $props();

  let enabled = $state(false);
  let rules = $state<SystemReplaceRule[]>([]);
  let loaded = $state(false);
  let error = $state("");
  let busy = $state(false);
  let saved = $state(false);
  let testing = $state(false);
  let test = $state<SystemReplaceTestResult | null>(null);

  $effect(() => {
    systemReplaceGet(host)
      .then((s) => {
        enabled = s.enabled;
        rules = s.rules.map((r) => ({ ...r }));
        loaded = true;
      })
      .catch((e) => (error = String(e)));
  });

  // Any edit makes "Saved" and the last test result stale.
  function touched() {
    saved = false;
    test = null;
  }

  const current = () => ({ enabled, rules: rules.map((r) => ({ ...r })) });
  const emptyOld = $derived(rules.findIndex((r) => r.old === ""));

  function add() {
    rules.push({ old: "", new: "", enabled: true });
    touched();
  }

  function move(i: number, d: number) {
    const j = i + d;
    if (j < 0 || j >= rules.length) return;
    [rules[i], rules[j]] = [rules[j], rules[i]];
    touched();
  }

  function remove(i: number) {
    rules.splice(i, 1);
    touched();
  }

  async function save() {
    if (busy || !loaded) return;
    busy = true;
    error = "";
    try {
      const s = await systemReplaceSet(host, current());
      enabled = s.enabled;
      rules = s.rules.map((r) => ({ ...r }));
      saved = true;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function runTest() {
    if (testing || !loaded) return;
    testing = true;
    error = "";
    test = null;
    try {
      test = await systemReplaceTest(host, current());
    } catch (e) {
      error = String(e);
    } finally {
      testing = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onclose();
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="backdrop" role="button" tabindex="-1" onclick={onclose} onkeydown={() => {}}>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="System Text Replacements"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <div class="label">System Text Replacements{hostLabel ? ` · ${hostLabel}` : ""}</div>
    <p class="about">
      Adds <code>--replace OLD NEW</code> for each ticked rule to every claude Mulpex starts (new, restored,
      spawned, restarted). Your <code>claude</code> must be a launcher that understands
      <code>--replace</code> — a plain <code>claude</code> refuses to start with it. Rules run in order;
      text is literal and case-sensitive; an empty NEW deletes. Running claudes keep their rules until
      restarted (⌘⇧R).
    </p>

    <label class="switch">
      <input type="checkbox" bind:checked={enabled} onchange={touched} />
      Enabled
    </label>

    <div class="rules" class:off={!enabled}>
      {#each rules as rule, i (i)}
        <div class="rule">
          <div class="head">
            <label class="tick">
              <input type="checkbox" bind:checked={rule.enabled} onchange={touched} />
              {i + 1}
            </label>
            <div class="btns">
              <button class="icon" title="Move up" disabled={i === 0} onclick={() => move(i, -1)}>↑</button>
              <button class="icon" title="Move down" disabled={i === rules.length - 1} onclick={() => move(i, 1)}
                >↓</button
              >
              <button class="icon x" title="Remove rule" onclick={() => remove(i)}>✕</button>
            </div>
          </div>
          <div class="pair" class:dim={!rule.enabled}>
            <span class="k">OLD</span>
            <textarea
              dir="auto"
              rows="3"
              spellcheck="false"
              class:bad={rule.old === ""}
              bind:value={rule.old}
              oninput={touched}
            ></textarea>
            <span class="k">NEW</span>
            <textarea
              dir="auto"
              rows="3"
              spellcheck="false"
              placeholder="(empty deletes OLD)"
              bind:value={rule.new}
              oninput={touched}
            ></textarea>
          </div>
        </div>
      {/each}
      <button class="plain addrule" onclick={add}>+ Add rule</button>
    </div>

    {#if emptyOld >= 0}
      <div class="error">Rule {emptyOld + 1}: OLD must not be empty.</div>
    {/if}
    {#if error}
      <div class="error">{error}</div>
    {/if}
    {#if test}
      <div class="test" class:good={test.ok}>
        <div class="mono cmd">{test.ok ? "✓" : "✕"} {test.command}</div>
        <pre class="mono">{test.output}</pre>
      </div>
    {/if}

    <div class="actions">
      <button class="plain" disabled={testing || !loaded || emptyOld >= 0} onclick={runTest}>
        {testing ? "Testing…" : "Test"}
      </button>
      <span class="hint">{saved ? "Saved · used from the next launch" : "Esc to close"}</span>
      <div class="end">
        <button class="plain" onclick={onclose}>Cancel</button>
        <button class="primary" disabled={busy || !loaded || emptyOld >= 0} onclick={save}>Save</button>
      </div>
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
  .about {
    margin: 0 0 0.6rem;
    color: var(--text-faint);
    font-size: 0.82rem;
    line-height: 1.4;
  }
  code,
  .mono {
    font-family: ui-monospace, monospace;
  }
  .switch,
  .tick {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .rules {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    margin-top: 0.7rem;
  }
  .rules.off .pair {
    opacity: 0.55;
  }
  .rule {
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 0.35rem;
  }
  .btns {
    display: flex;
    gap: 0.25rem;
  }
  .pair {
    display: grid;
    grid-template-columns: 2.6rem 1fr;
    gap: 0.35rem 0.5rem;
    align-items: start;
  }
  .pair.dim {
    opacity: 0.5;
  }
  .k {
    color: var(--label);
    font-size: 0.72rem;
    letter-spacing: 0.06em;
    padding-top: 0.45rem;
  }
  textarea {
    min-width: 0;
    resize: vertical;
    padding: 0.4rem 0.5rem;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-family: ui-monospace, monospace;
    font-size: 0.82rem;
    unicode-bidi: plaintext;
  }
  textarea:focus {
    outline: none;
    border-color: var(--border-focus);
  }
  textarea.bad {
    border-color: #e5534b;
  }
  button {
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    font: inherit;
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .plain {
    padding: 0.4rem 0.8rem;
  }
  .icon {
    padding: 0.1rem 0.45rem;
    font-size: 0.8rem;
  }
  .x:hover:not(:disabled) {
    color: #e5534b;
    border-color: #e5534b;
  }
  .addrule {
    align-self: flex-start;
  }
  .error {
    margin-top: 0.5rem;
    color: #e5534b;
    font-size: 0.8rem;
  }
  .test {
    margin-top: 0.7rem;
    padding: 0.45rem 0.6rem;
    border: 1px solid #e5534b;
    border-radius: 6px;
    font-size: 0.8rem;
  }
  .test.good {
    border-color: #3fb950;
  }
  .test .cmd {
    word-break: break-all;
  }
  .test pre {
    margin: 0.35rem 0 0;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 12rem;
    overflow-y: auto;
    color: var(--text-faint);
  }
  .actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    margin-top: 1rem;
  }
  .end {
    display: flex;
    gap: 0.4rem;
  }
  .hint {
    color: var(--text-faint);
    font-size: 0.74rem;
  }
  .primary {
    padding: 0.4rem 1rem;
    border-color: var(--border-focus);
  }
</style>
