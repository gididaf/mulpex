<script lang="ts">
  // One claude's conversation, from its transcript (the Mac's `remote/chat.rs`
  // decides what is shown). Tool calls are one line each; tap to see the input
  // and the result, which arrives as a separate item and is joined by id here.
  import { tick } from "svelte";
  import { render } from "./markdown";
  import Dialog, { type DialogView } from "./Dialog.svelte";

  export type Item =
    | { k: "user"; text: string }
    | { k: "text"; text: string }
    | { k: "tool"; id: string; name: string; summary: string; detail: string }
    | { k: "result"; id: string; text: string; error: boolean }
    | { k: "sys"; text: string };

  let {
    title,
    status,
    items,
    gone,
    loading,
    onback,
    onsend,
    onkey,
    dialog,
    onanswer,
    onclose,
  }: {
    title: string;
    status: string | null;
    items: Item[];
    gone: boolean;
    loading: boolean;
    onback: () => void;
    /** Resolves with the Mac's answer: typed, or why not. */
    onsend: (text: string) => Promise<{ ok: boolean; why: string | null }>;
    onkey: (key: string) => void;
    /** The question or plan this claude is waiting on, if any. */
    dialog: DialogView | null;
    onanswer: (answer: unknown) => Promise<{ ok: boolean; why: string | null }>;
    /** Close this instance (the phone confirms first). */
    onclose: () => void;
  } = $props();

  let draft = $state("");
  let sending = $state(false);
  let error = $state<string | null>(null);
  let box: HTMLTextAreaElement | undefined = $state();

  async function send() {
    const text = draft.trim();
    if (!text || sending) return;
    sending = true;
    error = null;
    const r = await onsend(text);
    sending = false;
    if (r.ok) {
      draft = "";
      pinned = true;
      grow();
    } else error = r.why ?? "Not sent";
  }

  /** The box grows with its text, up to a third of the screen. */
  function grow() {
    if (!box) return;
    box.style.height = "auto";
    box.style.height = Math.min(box.scrollHeight, window.innerHeight / 3) + "px";
  }

  const keys: Array<[string, string]> = [
    ["esc", "Esc"],
    ["ctrl-c", "^C"],
    ["up", "↑"],
    ["down", "↓"],
    ["enter", "⏎"],
    ["tab", "Tab"],
    ["shift-tab", "⇧Tab"],
  ];

  const results = $derived.by(() => {
    const m = new Map<string, { text: string; error: boolean }>();
    for (const it of items) if (it.k === "result") m.set(it.id, it);
    return m;
  });
  const shown = $derived(items.filter((it) => it.k !== "result"));
  let open = $state(new Set<string>());

  function toggle(id: string) {
    const next = new Set(open);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    open = next;
  }

  // Follow the conversation while the user is at the bottom; leave them alone
  // when they've scrolled up to read.
  let scroller: HTMLDivElement | undefined = $state();
  let pinned = true;
  function onScroll() {
    if (!scroller) return;
    pinned = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  }
  $effect(() => {
    void shown.length;
    if (pinned) tick().then(() => scroller?.scrollTo({ top: scroller.scrollHeight }));
  });
</script>

<div class="chat">
  <header>
    <button class="back" onclick={onback} aria-label="Back">‹</button>
    <span class="title" dir="auto">{title}</span>
    {#if status}<span class="st {status}">{status === "needs" ? "needs you" : status}</span>{/if}
    <button class="close" onclick={onclose}>Close</button>
  </header>

  <div class="scroll" bind:this={scroller} onscroll={onScroll}>
    {#if loading}
      <div class="note">Loading…</div>
    {:else if gone}
      <div class="note">This claude is no longer running.</div>
    {:else if shown.length === 0}
      <div class="note">Nothing here yet.</div>
    {/if}
    {#each shown as it, i (i)}
      {#if it.k === "user"}
        <div class="bubble user">{@html render(it.text)}</div>
      {:else if it.k === "text"}
        <div class="bubble claude">{@html render(it.text)}</div>
      {:else if it.k === "sys"}
        <div class="sys" dir="auto">{it.text}</div>
      {:else if it.k === "tool"}
        {@const r = results.get(it.id)}
        <button class="tool" class:err={r?.error} onclick={() => toggle(it.id)}>
          <span class="name">{it.name}</span>
          <span class="sum" dir="auto">{it.summary}</span>
          <span class="mark">{r ? (r.error ? "✕" : "✓") : "…"}</span>
        </button>
        {#if open.has(it.id)}
          <pre class="detail" dir="ltr">{it.detail}</pre>
          {#if r}<pre class="detail result" class:err={r.error} dir="auto">{r.text || "(no output)"}</pre>{/if}
        {/if}
      {/if}
    {/each}
  </div>

  {#if dialog && !gone}
    <Dialog {dialog} {onanswer} />
  {/if}
  {#if !gone}
    <div class="composer">
      {#if error}<div class="error" dir="auto">{error}</div>{/if}
      <div class="keys">
        {#each keys as [k, label] (k)}
          <button onclick={() => onkey(k)}>{label}</button>
        {/each}
      </div>
      <div class="input">
        <textarea
          bind:this={box}
          bind:value={draft}
          oninput={grow}
          rows="1"
          dir="auto"
          placeholder="Message claude…"
        ></textarea>
        <button class="send" disabled={!draft.trim() || sending} onclick={send}>
          {sending ? "…" : "Send"}
        </button>
      </div>
    </div>
  {/if}
</div>

<style>
  .chat {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    padding-top: env(safe-area-inset-top);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid var(--border);
  }
  .close {
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--border, #30363d);
    border-radius: 6px;
    background: none;
    color: var(--red, #f85149);
    font: inherit;
    font-size: 0.8rem;
  }
  .back {
    font-size: 1.8rem;
    line-height: 1;
    padding: 0 0.5rem 0.2rem;
    background: none;
    border: 0;
    color: var(--accent);
  }
  .title {
    flex: 1;
    min-width: 0;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    unicode-bidi: plaintext;
  }
  .st {
    font-size: 0.8rem;
    color: var(--label);
  }
  .st.needs {
    color: var(--red);
  }
  .st.working {
    color: var(--yellow);
  }
  .scroll {
    flex: 1;
    overflow-y: auto;
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }
  .note {
    color: var(--label);
    text-align: center;
    padding: 2rem 0;
  }
  .bubble {
    max-width: 88%;
    padding: 0.5rem 0.75rem;
    border-radius: 14px;
    overflow-wrap: anywhere;
  }
  .bubble :global(p) {
    margin: 0.3rem 0;
    white-space: pre-wrap;
    unicode-bidi: plaintext;
  }
  .bubble :global(pre) {
    margin: 0.4rem 0;
    padding: 0.5rem;
    border-radius: 6px;
    background: var(--bg);
    overflow-x: auto;
    font-size: 0.8rem;
  }
  .bubble :global(code) {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.88em;
  }
  .bubble :global(a) {
    color: var(--accent);
  }
  .user {
    align-self: flex-end;
    background: var(--accent);
    color: #fff;
  }
  .user :global(pre) {
    background: rgba(0, 0, 0, 0.2);
  }
  .claude {
    align-self: flex-start;
    background: var(--bg-elev);
  }
  .sys {
    align-self: center;
    font-size: 0.78rem;
    color: var(--label);
    unicode-bidi: plaintext;
  }
  .tool {
    align-self: flex-start;
    max-width: 92%;
    display: flex;
    align-items: baseline;
    gap: 0.45rem;
    padding: 0.3rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.8rem;
    text-align: start;
  }
  .tool .name {
    color: var(--text);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .tool .sum {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tool .mark {
    color: var(--green);
  }
  .tool.err .mark {
    color: var(--red);
  }
  .detail {
    margin: 0;
    padding: 0.5rem;
    max-height: 40vh;
    overflow: auto;
    border-radius: 6px;
    background: var(--bg-elev);
    font-size: 0.75rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .detail.result {
    border-inline-start: 3px solid var(--green);
  }
  .composer {
    border-top: 1px solid var(--border);
    padding: 0.4rem 0.6rem calc(0.5rem + env(safe-area-inset-bottom));
    background: var(--bg);
  }
  .error {
    color: var(--red);
    font-size: 0.8rem;
    padding: 0.1rem 0.2rem 0.35rem;
    unicode-bidi: plaintext;
  }
  .keys {
    display: flex;
    gap: 0.3rem;
    overflow-x: auto;
    padding-bottom: 0.4rem;
    scrollbar-width: none;
  }
  .keys button {
    flex: 0 0 auto;
    min-width: 2.4rem;
    padding: 0.3rem 0.55rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elev);
    color: var(--text);
    font: inherit;
    font-size: 0.8rem;
  }
  .input {
    display: flex;
    align-items: flex-end;
    gap: 0.4rem;
  }
  textarea {
    flex: 1;
    min-width: 0;
    resize: none;
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--border);
    border-radius: 18px;
    background: var(--bg-elev);
    color: var(--text);
    font: inherit;
    font-size: 16px; /* under 16px, iOS zooms the page on focus */
    unicode-bidi: plaintext;
  }
  textarea:focus {
    outline: none;
    border-color: var(--accent);
  }
  .send {
    padding: 0.5rem 0.9rem;
    border: 0;
    border-radius: 18px;
    background: var(--accent);
    color: #fff;
    font: inherit;
    font-weight: 600;
  }
  .send:disabled {
    opacity: 0.4;
  }
  .detail.result.err {
    border-inline-start-color: var(--red);
  }
</style>
