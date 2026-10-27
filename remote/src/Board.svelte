<script module lang="ts">
  // Mirrors the desktop's `src/lib/ipc.ts` Board types (`src-tauri/src/board.rs`).
  export type BoardColumn = { id: number; name: string; desc: string };
  export type BoardComment = { id: string; by: "claude" | "user"; who: number | null; text: string; images: string[]; at: number };
  export type BoardCard = {
    n: number;
    title: string;
    detail: string | null;
    images: string[];
    col: number;
    assignee: number | null;
    urgent: boolean;
    choices: string[];
    chosen: string | null;
    comments: BoardComment[];
    archived: boolean;
    author: "claude" | "user";
    by: number | null;
    unread: boolean;
  };
  /** `pending`: the user's changes waiting for UPDATE, claude id → card numbers. */
  export type Board = {
    on: boolean;
    columns: BoardColumn[];
    cards: BoardCard[];
    pending: Record<string, number[]>;
    /** The user's comments UPDATE has not sent yet — still editable. */
    unsent: string[];
    sending: number[];
  };

  type Reply = { ok: boolean; why: string | null; value?: any };
  type Req = (msg: Record<string, unknown>, timeoutMs?: number) => Promise<Reply>;

  /** Blob URLs by `"<handle>:<id>"` — an id names immutable bytes. */
  const urls = new Map<string, Promise<string>>();
  const MIME: Record<string, string> = { png: "image/png", jpg: "image/jpeg", gif: "image/gif", webp: "image/webp" };

  function fromB64(b64: string): Uint8Array {
    const s = atob(b64);
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  export function toB64(bytes: Uint8Array): string {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    return btoa(s);
  }

  export function imageUrl(req: Req, handle: number, id: string): Promise<string> {
    const key = `${handle}:${id}`;
    let p = urls.get(key);
    if (!p) {
      p = (async () => {
        const parts: Uint8Array[] = [];
        let got = 0;
        for (;;) {
          const r = await req({ t: "board-image-get", handle, id, offset: got }, 60_000);
          if (!r.ok || !r.value) throw new Error(r.why ?? "image gone");
          const b = fromB64(r.value.data);
          parts.push(b);
          got += b.length;
          if (got >= r.value.total || b.length === 0) break;
        }
        return URL.createObjectURL(new Blob(parts as BlobPart[], { type: MIME[id.split(".").pop() ?? ""] }));
      })();
      p.catch(() => urls.delete(key));
      urls.set(key, p);
    }
    return p;
  }

  const CHUNK = 2 * 1024 * 1024;
  export const MAX_BYTES = 10 * 1024 * 1024;
  export const MAX_IMAGES = 10;

  /** One picked image → its board id (chunked: a whole one would not fit a relay frame). */
  export async function uploadImage(req: Req, handle: number, file: File): Promise<string> {
    if (file.size > MAX_BYTES) throw new Error(`${file.name}: over 10 MB`);
    const bytes = new Uint8Array(await file.arrayBuffer());
    let upload: string | null = null;
    for (let off = 0; ; off += CHUNK) {
      const end = Math.min(off + CHUNK, bytes.length);
      const done = end >= bytes.length;
      const r = await req({ t: "board-image-put", handle, data: toB64(bytes.subarray(off, end)), upload, done }, 60_000);
      if (!r.ok) throw new Error(r.why ?? "upload failed");
      if (done) return r.value?.id;
      upload = r.value?.upload ?? null;
    }
  }
</script>

<script lang="ts">
  // The phone's Board: the project's columns as swipeable pages (one per screen
  // width), its cards, and a card screen with comments and images. The desktop
  // edits the columns; here the user moves, assigns, comments and adds cards.
  // Every edit is live — the Mac tells the card's claude a few seconds later.
  import { renderRich } from "./richmd";
  import { dragSort } from "./order";

  let {
    handle,
    board,
    claudes,
    focus,
    loading,
    cardOpen = $bindable(false),
    onback,
    req,
    onrefresh,
  }: {
    handle: number;
    board: Board | null;
    claudes: { id: number; name: string | null }[];
    focus: number | null;
    loading: boolean;
    cardOpen?: boolean;
    onback: () => void;
    req: Req;
    onrefresh: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally
  let all = $state(true);
  let error = $state<string | null>(null);
  let page = $state(0);
  let pagesEl: HTMLElement | undefined = $state();
  let openN = $state<number | null>(null);
  let lightbox = $state<{ ids: string[]; i: number } | null>(null);

  const card = $derived(cardOpen && openN != null ? board?.cards.find((c) => c.n === openN) : undefined);
  $effect(() => {
    if (!cardOpen) openN = null;
  });

  const shown = (col: number) =>
    board?.cards.filter((c) => c.col === col && !c.archived && (all || c.assignee == null || c.assignee === focus)) ?? [];

  async function edit(e: Record<string, unknown>): Promise<boolean> {
    error = null;
    const r = await req({ t: "board-edit", handle, edit: e });
    if (!r.ok) error = r.why ?? "Couldn't do that";
    else onrefresh();
    return r.ok;
  }

  // UPDATE: every edit is on the board at once, but a claude hears about it
  // only when the user sends — one message per claude (shared with the desktop).
  const pendingSet = $derived(new Set(Object.values(board?.pending ?? {}).flat()));
  const pendingClaudes = $derived(Object.keys(board?.pending ?? {}).map(Number));
  let updating = $state(false);
  async function update() {
    if (updating) return;
    updating = true;
    error = null;
    const r = await req({ t: "board-update", handle });
    const refused: string[] = r.value?.refused ?? [];
    if (!r.ok) error = r.why ?? "Couldn't send";
    else if (refused.length) error = refused.join(" · ");
    updating = false;
    onrefresh();
  }

  // Editing an unsent comment of the user's (until UPDATE sends it).
  let editing = $state<{ n: number; id: string; text: string; images: string[] } | null>(null);
  async function saveEdit() {
    if (!editing) return;
    const e = editing;
    const ok =
      !e.text.trim() && !e.images.length
        ? await edit({ op: "comment_delete", n: e.n, id: e.id })
        : await edit({ op: "comment_edit", n: e.n, id: e.id, text: e.text, images: [...e.images] });
    if (ok) editing = null;
  }
  $effect(() => {
    if (editing && !(board?.unsent ?? []).includes(editing.id)) editing = null;
  });

  async function turnOn() {
    error = null;
    const r = await req({ t: "board-on", handle, on: true });
    if (!r.ok) error = r.why ?? "Couldn't turn it on";
    onrefresh();
  }

  function goPage(i: number) {
    page = i;
    pagesEl?.children[i]?.scrollIntoView({ behavior: "smooth", inline: "start", block: "nearest" });
  }
  function onScroll() {
    if (!pagesEl) return;
    page = Math.round(pagesEl.scrollLeft / Math.max(1, pagesEl.clientWidth));
  }

  function openCard(c: BoardCard) {
    openN = c.n;
    cardOpen = true;
    history.pushState({ card: true }, "");
  }
  // A claude changes the open card: it is read as it arrives.
  $effect(() => {
    if (card?.unread) void edit({ op: "seen", n: card.n });
  });

  // ── adding ─────────────────────────────────────────────────────────────────
  let addTitle = $state("");
  let adding = $state<number | null>(null);
  async function add(col: number) {
    if (!addTitle.trim()) return;
    if (await edit({ op: "add", title: addTitle, col, assignee: focus })) {
      addTitle = "";
      adding = null;
    }
  }

  // ── reorder within a column (long-press) ────────────────────────────────────
  function drop(col: number, from: string, to: string) {
    const list = shown(col);
    const fi = list.findIndex((c) => String(c.n) === from);
    const ti = list.findIndex((c) => String(c.n) === to);
    if (fi < 0 || ti < 0) return;
    // Down: land after `to` (before the one after it); up: before `to`.
    const before = fi < ti ? (list[ti + 1]?.n ?? null) : list[ti].n;
    void edit({ op: "move", n: Number(from), col, before });
  }

  // ── card screen ─────────────────────────────────────────────────────────────
  let text = $state("");
  let pending = $state<string[]>([]);
  let busy = $state(false);
  let fileEl: HTMLInputElement | undefined = $state();
  let cardFileEl: HTMLInputElement | undefined = $state();

  async function attach(files: FileList | null, toCard: boolean) {
    if (!files?.length || !card) return;
    const list = [...files];
    const have = toCard ? card.images.length : pending.length;
    if (have + list.length > MAX_IMAGES) {
      error = `At most ${MAX_IMAGES} images`;
      return;
    }
    busy = true;
    error = null;
    try {
      const ids: string[] = [];
      for (const f of list) ids.push(await uploadImage(req, handle, f));
      if (toCard) await edit({ op: "edit", n: card.n, images: [...card.images, ...ids] });
      else pending = [...pending, ...ids];
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
      if (fileEl) fileEl.value = "";
      if (cardFileEl) cardFileEl.value = "";
    }
  }

  async function send() {
    if (!card || busy || (!text.trim() && !pending.length)) return;
    busy = true;
    const ok = await edit({ op: "comment", n: card.n, text, images: pending });
    busy = false;
    if (ok) {
      text = "";
      pending = [];
    }
  }

  const who = (c: BoardComment) => (c.by === "user" ? "you" : c.who != null ? `claude#${c.who}` : "claude");
  const imgCount = (c: BoardCard) => c.images.length + c.comments.reduce((n, m) => n + m.images.length, 0);
</script>

{#snippet thumbs(ids: string[], onremove?: (id: string) => void)}
  {#if ids.length}
    <div class="thumbs">
      {#each ids as id, i (id)}
        <div class="thumb">
          {#await imageUrl(req, handle, id)}
            <div class="ph"></div>
          {:then url}
            <button class="img" onclick={() => (lightbox = { ids, i })}><img src={url} alt="" /></button>
          {:catch}
            <div class="ph">?</div>
          {/await}
          {#if onremove}<button class="rm" aria-label="Remove" onclick={() => onremove(id)}>✕</button>{/if}
        </div>
      {/each}
    </div>
  {/if}
{/snippet}

<div class="board">
  {#if card}
    <header>
      <button class="back" onclick={() => history.back()} aria-label="Back">‹</button>
      <span class="title">#{card.n}</span>
      {#if card.urgent}<span class="urgent-tag">URGENT</span>{/if}
      <span class="grow"></span>
      {#if card.archived}
        <button class="hbtn" onclick={() => edit({ op: "archive", n: card.n, on: false })}>Restore</button>
      {:else}
        <button class="hbtn" onclick={async () => { if (await edit({ op: "archive", n: card!.n, on: true })) history.back(); }}>Archive</button>
      {/if}
    </header>
    <div class="scroll pad">
      <div class="ctitle big" dir="auto">{card.title}</div>
      <div class="fields">
        <label>
          <span>Column</span>
          <select value={card.col} onchange={(e) => edit({ op: "move", n: card!.n, col: Number((e.currentTarget as HTMLSelectElement).value), before: null })}>
            {#each board?.columns ?? [] as c (c.id)}<option value={c.id}>{c.name}</option>{/each}
          </select>
        </label>
        <label>
          <span>Assignee</span>
          <select value={card.assignee ?? ""} onchange={(e) => { const v = (e.currentTarget as HTMLSelectElement).value; edit({ op: "assign", n: card!.n, to: v === "" ? null : Number(v) }); }}>
            <option value="">Nobody</option>
            {#each claudes as c (c.id)}<option value={c.id}>claude#{c.id}</option>{/each}
            {#if card.assignee != null && !claudes.some((c) => c.id === card!.assignee)}<option value={card.assignee}>claude#{card.assignee} (closed)</option>{/if}
          </select>
        </label>
      </div>
      {#if card.detail}
        <!-- Sanitized in renderRich. -->
        <div class="md">{@html renderRich(card.detail)}</div>
      {/if}
      {@render thumbs(card.images)}
      <button class="link" disabled={busy} onclick={() => cardFileEl?.click()}>📎 Add image to the card</button>
      <input bind:this={cardFileEl} type="file" accept="image/png,image/jpeg,image/gif,image/webp" multiple hidden onchange={(e) => attach((e.currentTarget as HTMLInputElement).files, true)} />
      {#if card.choices.length}
        <div class="choices">
          {#each card.choices as c (c)}
            <button class="chip" class:on={card.chosen === c} dir="auto" onclick={() => edit({ op: "choose", n: card!.n, choice: card!.chosen === c ? null : c })}>{c}</button>
          {/each}
        </div>
      {/if}
      <div class="sec">Comments{card.comments.length ? ` · ${card.comments.length}` : ""}</div>
      {#each card.comments as c, ci (c.id || ci)}
        {@const mine = c.by === "user" && !!c.id && (board?.unsent ?? []).includes(c.id)}
        <div class="comment" class:claude={c.by === "claude"} class:unsent={mine}>
          <span class="who">{who(c)}{#if mine} · <span class="unsent-tag">not sent</span>{/if}</span>
          {#if editing?.id === c.id}
            <textarea dir="auto" rows="3" bind:value={editing.text}></textarea>
            {@render thumbs(editing.images, (id) => editing && (editing.images = editing.images.filter((i) => i !== id)))}
            <div class="ctools">
              <button class="link" onclick={() => (editing = null)}>Cancel</button>
              <button class="link" onclick={saveEdit}>Save</button>
            </div>
          {:else}
            <!-- Sanitized in renderRich. -->
            {#if c.text}<div class="md">{@html renderRich(c.text)}</div>{/if}
            {@render thumbs(c.images)}
            {#if mine}
              <div class="ctools">
                <button class="link" onclick={() => (editing = { n: card.n, id: c.id, text: c.text, images: [...c.images] })}>Edit</button>
                <button class="link" onclick={() => edit({ op: "comment_delete", n: card.n, id: c.id })}>Delete</button>
              </div>
            {/if}
          {/if}
        </div>
      {/each}
    </div>
    <div class="composer">
      {#if error}<div class="err" dir="auto">{error}</div>{/if}
      {@render thumbs(pending, (id) => (pending = pending.filter((p) => p !== id)))}
      <textarea dir="auto" rows="2" placeholder={card.assignee != null ? `Comment to claude#${card.assignee}…` : "Comment… (nobody is told)"} bind:value={text}></textarea>
      <div class="row">
        <button class="link" disabled={busy} onclick={() => fileEl?.click()}>📎 Image</button>
        <input bind:this={fileEl} type="file" accept="image/png,image/jpeg,image/gif,image/webp" multiple hidden onchange={(e) => attach((e.currentTarget as HTMLInputElement).files, false)} />
        {#if busy}<span class="dim">uploading…</span>{/if}
        <span class="grow"></span>
        <button class="send" disabled={busy || (!text.trim() && !pending.length)} onclick={send}>Comment</button>
      </div>
    </div>
  {:else}
    <header>
      <button class="back" onclick={onback} aria-label="Back">‹</button>
      <span class="title">Board</span>
      {#if board?.on}
        <div class="seg">
          {#if focus != null}<button class:on={!all} onclick={() => (all = false)}>claude#{focus}</button>{/if}
          <button class:on={all} onclick={() => (all = true)}>All</button>
        </div>
      {/if}
    </header>
    {#if loading && !board}
      <div class="empty">Loading…</div>
    {:else if !board || !board.on}
      <div class="empty">
        The Board is off for this project.
        <p>Turn it on and every claude here can put cards on it and talk to you on them.</p>
        <button class="send" onclick={turnOn}>Turn on the Board</button>
        {#if error}<div class="err" dir="auto">{error}</div>{/if}
      </div>
    {:else}
      <nav class="tabs">
        {#each board.columns as c, i (c.id)}
          <button class:on={page === i} dir="auto" onclick={() => goPage(i)}>{c.name} <span class="n">{shown(c.id).length}</span></button>
        {/each}
      </nav>
      {#if error}<div class="err bar" dir="auto">{error}</div>{/if}
      <div class="pages" bind:this={pagesEl} onscroll={onScroll}>
        {#each board.columns as col (col.id)}
          <section class="page">
            {#if col.desc}<div class="desc" dir="auto">{col.desc}</div>{/if}
            <ul use:dragSort={{ selector: ".cardrow", canDrop: () => true, onDrop: (a, b) => drop(col.id, a, b) }}>
              {#each shown(col.id) as c (c.n)}
                <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
                <li class="cardrow" data-key={c.n} class:urgent={c.urgent} onclick={() => openCard(c)}>
                  <div class="ctitle" dir="auto">{c.title}</div>
                  <div class="meta">
                    <span>#{c.n}</span>
                    {#if c.urgent}<span class="urgent-tag">URGENT</span>{/if}
                    {#if c.assignee != null}<span class="chipw" class:me={c.assignee === focus}>claude#{c.assignee}</span>{/if}
                    {#if c.choices.length}<span>{c.chosen ? "◆✓" : "◆"}</span>{/if}
                    {#if c.comments.length}<span>💬{c.comments.length}</span>{/if}
                    {#if imgCount(c)}<span>🖼{imgCount(c)}</span>{/if}
                    {#if pendingSet.has(c.n)}<span class="pend">pending</span>{/if}
                    {#if c.unread}<span class="udot"></span>{/if}
                  </div>
                </li>
              {:else}
                <li class="none">No cards.</li>
              {/each}
            </ul>
            {#if adding === col.id}
              <div class="add">
                <!-- svelte-ignore a11y_autofocus -->
                <input dir="auto" placeholder="Card title" bind:value={addTitle} autofocus onkeydown={(e) => { if (e.key === "Enter") add(col.id); }} />
                <div class="row">
                  <span class="dim">{focus != null ? `for claude#${focus}` : "unassigned"}</span>
                  <span class="grow"></span>
                  <button class="link" onclick={() => (adding = null)}>Cancel</button>
                  <button class="send" disabled={!addTitle.trim()} onclick={() => add(col.id)}>Add</button>
                </div>
              </div>
            {:else}
              <button class="addbtn" onclick={() => { adding = col.id; addTitle = ""; }}>+ Add a card</button>
            {/if}
          </section>
        {/each}
      </div>
      <footer>
        <span class="fstat">
          {#if pendingSet.size}{pendingSet.size} card{pendingSet.size === 1 ? "" : "s"} not sent{:else if board.sending.length}sending to {board.sending.map((s) => `claude#${s}`).join(", ")}…{:else}up to date{/if}
        </span>
        <button class="update" disabled={!pendingClaudes.length || updating} onclick={update}>
          {pendingClaudes.length === 1 ? `UPDATE claude#${pendingClaudes[0]}` : pendingClaudes.length > 1 ? `UPDATE · ${pendingClaudes.length} claudes` : "UPDATE"}
        </button>
      </footer>
    {/if}
  {/if}
</div>

{#if lightbox}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="lb" onclick={() => (lightbox = null)}>
    {#await imageUrl(req, handle, lightbox.ids[lightbox.i]) then url}<img src={url} alt="" />{/await}
    {#if lightbox.ids.length > 1}
      <div class="lbnav">
        <button onclick={(e) => { e.stopPropagation(); lightbox!.i = (lightbox!.i - 1 + lightbox!.ids.length) % lightbox!.ids.length; }}>‹</button>
        <span>{lightbox.i + 1} / {lightbox.ids.length}</span>
        <button onclick={(e) => { e.stopPropagation(); lightbox!.i = (lightbox!.i + 1) % lightbox!.ids.length; }}>›</button>
      </div>
    {/if}
  </div>
{/if}

<style>
  .board {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    padding-top: env(safe-area-inset-top);
    padding-bottom: env(safe-area-inset-bottom);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 0.8rem;
    border-bottom: 1px solid var(--border);
  }
  .back {
    border: 0;
    background: none;
    color: var(--accent);
    font-size: 1.6rem;
    line-height: 1;
    padding: 0 0.4rem 0 0;
  }
  .title {
    font-weight: 600;
  }
  .grow {
    flex: 1;
  }
  .hbtn {
    border: 1px solid var(--border);
    background: none;
    color: var(--text);
    font: inherit;
    font-size: 0.8rem;
    padding: 0.25rem 0.6rem;
    border-radius: 8px;
  }
  .seg {
    margin-inline-start: auto;
    display: flex;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .seg button {
    border: 0;
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.8rem;
    padding: 0.3rem 0.6rem;
  }
  .seg button.on {
    background: var(--bg-elev);
    color: var(--text);
  }
  .empty {
    padding: 2rem 1rem;
    color: var(--label);
    text-align: center;
  }
  .empty p {
    font-size: 0.9rem;
  }
  .tabs {
    display: flex;
    gap: 0.3rem;
    overflow-x: auto;
    padding: 0.4rem 0.6rem;
    border-bottom: 1px solid var(--border);
    scrollbar-width: none;
  }
  .tabs button {
    flex: none;
    border: 1px solid var(--border);
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.82rem;
    padding: 0.25rem 0.65rem;
    border-radius: 999px;
    unicode-bidi: plaintext;
  }
  .tabs button.on {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--bg);
  }
  .tabs .n {
    opacity: 0.7;
    font-size: 0.75rem;
  }
  .pages {
    flex: 1;
    display: flex;
    overflow-x: auto;
    overflow-y: hidden;
    scroll-snap-type: x mandatory;
    scrollbar-width: none;
  }
  .page {
    flex: 0 0 100%;
    scroll-snap-align: start;
    overflow-y: auto;
    padding: 0.6rem 0.8rem 1rem;
    box-sizing: border-box;
  }
  .desc {
    font-size: 0.8rem;
    color: var(--label);
    margin-bottom: 0.5rem;
    unicode-bidi: plaintext;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .cardrow {
    padding: 0.6rem 0.7rem;
    margin-bottom: 0.5rem;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-elev);
    -webkit-user-select: none;
    user-select: none;
  }
  .cardrow.urgent {
    border-inline-start: 4px solid var(--red);
  }
  .cardrow:global([data-drag="src"]) {
    opacity: 0.4;
  }
  .cardrow:global([data-drag="target"]) {
    box-shadow: 0 -3px 0 var(--accent);
  }
  .none {
    color: var(--label);
    font-size: 0.85rem;
    padding: 0.5rem 0;
  }
  .ctitle {
    font-size: 0.95rem;
    unicode-bidi: plaintext;
    overflow-wrap: anywhere;
  }
  .ctitle.big {
    font-size: 1.1rem;
    font-weight: 600;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.3rem;
    font-size: 0.75rem;
    color: var(--label);
  }
  .chipw {
    padding: 0 0.35rem;
    border-radius: 4px;
    background: var(--border);
    color: var(--text);
  }
  .chipw.me {
    background: var(--accent);
    color: var(--bg);
  }
  .urgent-tag {
    padding: 0 0.3rem;
    border-radius: 4px;
    background: var(--red);
    color: var(--on-red);
    font-size: 0.68rem;
    font-weight: 700;
  }
  .udot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--accent);
    margin-inline-start: auto;
  }
  .addbtn {
    width: 100%;
    border: 1px dashed var(--border);
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.85rem;
    padding: 0.5rem;
    border-radius: 10px;
  }
  .add input,
  textarea,
  select {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    font: inherit;
    font-size: 16px;
    unicode-bidi: plaintext;
  }
  select {
    padding: 0.35rem 0.4rem;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.4rem;
  }
  .dim {
    color: var(--label);
    font-size: 0.8rem;
  }
  .link {
    border: 0;
    background: none;
    color: var(--accent);
    font: inherit;
    font-size: 0.85rem;
    padding: 0.3rem 0;
  }
  .send {
    border: 0;
    border-radius: 8px;
    background: var(--accent);
    color: var(--bg);
    font: inherit;
    font-size: 0.85rem;
    font-weight: 600;
    padding: 0.4rem 0.9rem;
  }
  .send:disabled,
  .link:disabled {
    opacity: 0.45;
  }
  .err {
    color: var(--red);
    font-size: 0.82rem;
    margin: 0.3rem 0;
  }
  .err.bar {
    padding: 0 0.8rem;
  }
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.4rem 0.8rem calc(0.4rem + env(safe-area-inset-bottom));
    border-top: 1px solid var(--border);
    font-size: 0.75rem;
    color: var(--label);
  }
  .update {
    border: none;
    border-radius: 8px;
    background: var(--accent);
    color: var(--bg);
    font: inherit;
    font-weight: 700;
    font-size: 0.85rem;
    padding: 0.45rem 0.9rem;
    white-space: nowrap;
  }
  .update:disabled {
    background: var(--border);
    color: var(--label);
  }
  .pend {
    padding: 0 0.3rem;
    border-radius: 4px;
    border: 1px solid var(--accent);
    color: var(--accent);
    font-size: 0.7rem;
  }
  .scroll {
    flex: 1;
    overflow-y: auto;
  }
  .pad {
    padding: 0.8rem;
  }
  .fields {
    display: flex;
    gap: 0.5rem;
    margin: 0.6rem 0;
  }
  .fields label {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-size: 0.72rem;
    color: var(--label);
  }
  .md {
    font-size: 0.9rem;
    overflow-wrap: anywhere;
  }
  .md :global(pre),
  .md :global(code) {
    direction: ltr;
    unicode-bidi: isolate;
  }
  .md :global(pre) {
    overflow-x: auto;
    padding: 0.4rem;
    border-radius: 6px;
    background: var(--bg-elev);
  }
  .md :global(table) {
    display: block;
    overflow-x: auto;
    border-collapse: collapse;
  }
  .md :global(:is(td, th)) {
    border: 1px solid var(--border);
    padding: 0.15rem 0.4rem;
  }
  .choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin: 0.6rem 0;
  }
  .chip {
    border: 1px solid var(--border);
    background: none;
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
    padding: 0.3rem 0.8rem;
    border-radius: 999px;
    unicode-bidi: plaintext;
  }
  .chip.on {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--bg);
  }
  .sec {
    margin: 1rem 0 0.4rem;
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--label);
    text-transform: uppercase;
  }
  .comment {
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.4rem;
    border-radius: 8px;
    border-inline-start: 3px solid var(--border);
    background: var(--bg-elev);
  }
  .comment.unsent {
    border-inline-start-color: var(--accent);
  }
  .unsent-tag {
    color: var(--accent);
  }
  .ctools {
    display: flex;
    justify-content: flex-end;
    gap: 1rem;
  }
  .comment textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
  }
  .comment.claude {
    border-inline-start-color: var(--yellow);
  }
  .who {
    display: block;
    font-size: 0.72rem;
    color: var(--label);
  }
  .composer {
    padding: 0.5rem 0.8rem;
    border-top: 1px solid var(--border);
  }
  .thumbs {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin: 0.4rem 0;
  }
  .thumb {
    position: relative;
    width: 4rem;
    height: 4rem;
  }
  .img,
  .ph {
    display: grid;
    place-items: center;
    width: 100%;
    height: 100%;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg-elev);
    overflow: hidden;
    color: var(--label);
  }
  .img img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .rm {
    position: absolute;
    top: -0.4rem;
    right: -0.4rem;
    width: 1.3rem;
    height: 1.3rem;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 50%;
    background: var(--bg);
    color: var(--text);
    font-size: 0.7rem;
  }
  .lb {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.9);
  }
  .lb img {
    max-width: 100vw;
    max-height: 85dvh;
    object-fit: contain;
  }
  .lbnav {
    position: absolute;
    bottom: calc(1rem + env(safe-area-inset-bottom));
    display: flex;
    align-items: center;
    gap: 1.2rem;
    color: #ddd;
  }
  .lbnav button {
    border: 0;
    background: none;
    color: #fff;
    font-size: 2rem;
    padding: 0 0.6rem;
  }
</style>
