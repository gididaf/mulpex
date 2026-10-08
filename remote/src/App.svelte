<script lang="ts">
  import { Link, type LinkState } from "./link";
  import Chat, { type Item } from "./Chat.svelte";
  import type { DialogView } from "./Dialog.svelte";
  import Term from "./Term.svelte";
  import { unb64 } from "./crypto";

  // Mulpex Remote Control — the phone side: the project tabs and the sidebar,
  // live, and a chat view per claude. The Mac pushes a `state` view (see
  // `src-tauri/src/remote/mod.rs::workspace_view`) whenever it changes, and
  // `chat` items for the claude that is open here.

  type Row = {
    id: number;
    kind: "claude" | "shell";
    name: string | null;
    task: string | null;
    status: "working" | "waiting" | "needs" | null;
    ctx_pct: number | null;
    muted: boolean;
    parent: number | null;
    exited: boolean;
    failed: string | null;
    /** The open question or plan, while the claude waits on it. */
    dialog: DialogView | null;
  };
  type Project = { handle: number; name: string; sessions: Row[] };
  type View = { t: "state"; active: number | null; projects: Project[] };

  /** The claude whose conversation is open, if any. */
  let chat = $state<{ handle: number; id: number; items: Item[]; gone: boolean; loading: boolean } | null>(
    null,
  );

  /** The terminal that is open, if any. */
  let term = $state<{ handle: number; id: number; gone: boolean } | null>(null);
  let termView: Term | undefined = $state();
  /** Frames that arrived before the terminal view mounted. */
  let termQueue: any[] = [];

  function feedTerm(msg: any) {
    if (!termView) {
      termQueue.push(msg);
      return;
    }
    if (msg.gone) {
      if (term) term.gone = true;
      return;
    }
    const data = unb64(msg.data ?? "");
    if (msg.reset) termView.reset(msg.cols, msg.rows, data);
    else termView.write(msg.cols, msg.rows, data);
  }

  $effect(() => {
    if (termView && termQueue.length) {
      const q = termQueue;
      termQueue = [];
      q.forEach(feedTerm);
    }
  });

  let view = $state<View | null>(null);
  let link = $state<LinkState>("connecting");
  let refusal = $state<string | null>(null);
  // The callback gets the link itself: it first fires from inside the
  // constructor, before `conn` is assigned.
  const conn = new Link(
    (l) => {
      link = l.state;
      refusal = l.refusal;
      if (l.state === "unpaired") view = null;
      // A new connection is a new session on the Mac: ask for the chat again.
      if (l.state === "online" && chat) {
        chat.loading = true;
        l.send({ t: "open", handle: chat.handle, id: chat.id });
      }
      if (l.state === "online" && term) l.send({ t: "term-open", handle: term.handle, id: term.id });
      // The Mac skips a notification for the claude open here only while
      // the app is actually in front; a minimized app can stay connected.
      if (l.state === "online") l.send({ t: "visible", on: document.visibilityState === "visible" });
    },
    (msg) => {
      if (msg.t === "state") {
        view = msg as View;
        openPending();
      } else if (msg.t === "push-key" || msg.t === "push-state") {
        pushWaiters.get(msg.t)?.(msg);
        pushWaiters.delete(msg.t);
      }
      else if (msg.t === "term" && term && msg.handle === term.handle && msg.id === term.id) feedTerm(msg);
      else if (msg.t === "typed") {
        typing.get(msg.rid)?.({ ok: msg.ok, why: msg.why ?? null, id: msg.id ?? null });
        typing.delete(msg.rid);
      }
      else if (msg.t === "chat" && chat && msg.handle === chat.handle && msg.id === chat.id) {
        chat.items = msg.reset ? msg.items : [...chat.items, ...msg.items];
        chat.gone = !!msg.gone;
        chat.loading = false;
      }
    },
  );
  link = conn.state;

  function openChat(handle: number, row: Row) {
    if (row.kind !== "claude") return openTerm(handle, row);
    chat = { handle, id: row.id, items: [], gone: false, loading: true };
    conn.send({ t: "open", handle, id: row.id });
    // So the phone's own back gesture closes the chat instead of the app.
    history.pushState({ chat: true }, "");
  }

  /** Typing requests waiting for the Mac's answer, by request id. */
  const typing = new Map<string, (r: Reply) => void>();

  type Reply = { ok: boolean; why: string | null; id?: number | null };

  /** A request the Mac answers (`typed`, matched by request id). */
  function request(msg: Record<string, unknown>, timeoutMs = 15_000): Promise<Reply> {
    if (link !== "online") return Promise.resolve({ ok: false, why: "Not connected to the Mac" });
    const rid = Math.random().toString(36).slice(2);
    return new Promise((resolve) => {
      typing.set(rid, resolve);
      conn.send({ ...msg, rid });
      setTimeout(() => {
        if (typing.delete(rid)) resolve({ ok: false, why: "No answer from the Mac" });
      }, timeoutMs);
    });
  }

  function sendText(text: string): Promise<Reply> {
    if (!chat) return Promise.resolve({ ok: false, why: "No claude open" });
    return request({ t: "type", handle: chat.handle, id: chat.id, text });
  }

  function sendAnswer(answer: unknown): Promise<Reply> {
    if (!chat) return Promise.resolve({ ok: false, why: "No claude open" });
    return request({ t: "answer", handle: chat.handle, id: chat.id, answer });
  }

  // ---- Notifications ------------------------------------------------------
  // The Mac sends them itself, encrypted to this browser (`remote/push.rs`).
  // Phones need the app installed for this: iOS only offers push to a Home
  // Screen app; Android Chrome works in the browser too.
  type PushState = "unsupported" | "off" | "on" | "busy";
  let push = $state<PushState>("off");
  let pushNote = $state<string | null>(null);
  const pushWaiters = new Map<string, (m: any) => void>();

  function askMac(t: string, msg: Record<string, unknown>): Promise<any> {
    const reply = t === "push-key" ? "push-key" : "push-state";
    return new Promise((resolve) => {
      pushWaiters.set(reply, resolve);
      conn.send({ t, ...msg });
      setTimeout(() => {
        if (pushWaiters.delete(reply)) resolve(null);
      }, 10_000);
    });
  }

  async function pushRegistration(): Promise<ServiceWorkerRegistration | null> {
    if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) return null;
    return navigator.serviceWorker.ready;
  }

  (async () => {
    const reg = await pushRegistration();
    if (!reg) push = "unsupported";
    else push = (await reg.pushManager.getSubscription()) && Notification.permission === "granted" ? "on" : "off";
  })();

  async function togglePush() {
    pushNote = null;
    const reg = await pushRegistration();
    if (!reg) {
      pushNote = /iPhone|iPad/.test(navigator.userAgent)
        ? "On iPhone, add Mulpex to the Home Screen first (Share → Add to Home Screen), then open it from there."
        : "This browser can't receive notifications.";
      return;
    }
    if (link !== "online") {
      pushNote = "Connect to the Mac first.";
      return;
    }
    push = "busy";
    try {
      if ((await reg.pushManager.getSubscription()) && Notification.permission === "granted") {
        await (await reg.pushManager.getSubscription())?.unsubscribe();
        await askMac("push-off", {});
        push = "off";
        return;
      }
      if ((await Notification.requestPermission()) !== "granted") {
        pushNote = "Notifications are blocked for this site in the browser settings.";
        push = "off";
        return;
      }
      const k = await askMac("push-key", {});
      if (!k?.key) throw new Error("No answer from the Mac");
      // A key from a different Mac (or an old one) can't be reused: start over.
      await (await reg.pushManager.getSubscription())?.unsubscribe();
      const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: k.key });
      const st = await askMac("push-sub", { sub: sub.toJSON() });
      if (!st?.on) throw new Error("The Mac didn't accept it");
      push = "on";
    } catch (e) {
      pushNote = String(e instanceof Error ? e.message : e);
      push = "off";
    }
  }

  /** A notification tap names a claude ("handle.id"): open it once the view
   *  has arrived. From the URL on a fresh start, by message when already open. */
  let pendingOpen: string | null = new URLSearchParams(location.search).get("open");
  if (pendingOpen) history.replaceState(null, "", location.pathname);
  navigator.serviceWorker?.addEventListener("message", (e) => {
    if (e.data?.open) {
      pendingOpen = e.data.open;
      openPending();
    }
  });
  function openPending() {
    if (!pendingOpen || !view) return;
    const [h, i] = pendingOpen.split(".").map(Number);
    pendingOpen = null;
    if (term || chat) history.back();
    setTimeout(() => {
      picked = h;
      openChat(h, { id: i, kind: "claude" } as Row);
    }, term || chat ? 300 : 0);
  }

  /** Start a claude or terminal in a project, then open it. The Mac adds the
   *  row without moving its own focus. */
  let starting = $state<string | null>(null);
  async function startInstance(handle: number, kind: "claude" | "shell") {
    starting = null;
    const r = await request({ t: "new-instance", handle, kind }, 20_000);
    if (!r.ok || r.id == null) {
      starting = r.why ?? "Couldn't start it";
      return;
    }
    openChat(handle, { id: r.id, kind } as Row);
  }

  async function closeInstance(handle: number, id: number, kind: "claude" | "shell") {
    const label = kind === "shell" ? `term#${id}` : `claude#${id}`;
    if (!confirm(`Close ${label}?`)) return;
    const r = await request({ t: "close-instance", handle, id });
    if (r.ok) history.back();
    else alert(r.why ?? "Couldn't close it");
  }

  function sendKey(key: string) {
    if (chat) conn.send({ t: "key", handle: chat.handle, id: chat.id, key });
  }

  function openTerm(handle: number, row: Row) {
    term = { handle, id: row.id, gone: false };
    termQueue = [];
    conn.send({ t: "term-open", handle, id: row.id });
    history.pushState({ term: true }, "");
  }

  function closeChat() {
    if (term) {
      term = null;
      termView = undefined;
      conn.send({ t: "term-close" });
    }
    if (!chat) return;
    chat = null;
    conn.send({ t: "close" });
  }

  window.addEventListener("popstate", closeChat);
  // A reload (pull-to-refresh, or Android killing the app in the background)
  // starts on the list, but the history still holds the entry an open chat or
  // terminal pushed: the first Back would pop it and do nothing visible. Drop it.
  if (history.state?.chat || history.state?.term) history.back();
  document.addEventListener("visibilitychange", () =>
    conn.send({ t: "visible", on: document.visibilityState === "visible" }),
  );

  const chatRow = $derived(
    chat ? view?.projects.find((p) => p.handle === chat!.handle)?.sessions.find((s) => s.id === chat!.id) : null,
  );

  const refusals: Record<string, string> = {
    "not paired": "This phone isn't paired with the Mac.",
    revoked: "This phone was removed on the Mac.",
  };
  /** The tab the phone is looking at; follows the Mac until the user picks one. */
  let picked = $state<number | null>(null);

  const shown = $derived.by(() => {
    if (!view) return null;
    const h = picked ?? view.active;
    return view.projects.find((p) => p.handle === h) ?? view.projects[0] ?? null;
  });

  /** Sidebar order: claudes (unmuted, then muted) above terminals, each child
   *  right under its parent. */
  function ordered(rows: Row[]): Array<{ row: Row; depth: number }> {
    const rank = (r: Row) => (r.kind === "shell" ? 2 : r.muted ? 1 : 0);
    const ids = new Set(rows.map((r) => r.id));
    const tops = rows
      .filter((r) => r.parent == null || !ids.has(r.parent))
      .sort((a, b) => rank(a) - rank(b));
    const out: Array<{ row: Row; depth: number }> = [];
    const walk = (r: Row, depth: number) => {
      out.push({ row: r, depth });
      for (const c of rows.filter((x) => x.parent === r.id)) walk(c, depth + 1);
    };
    for (const t of tops) walk(t, 0);
    return out;
  }

  function label(r: Row): string {
    return r.kind === "shell" ? `term#${r.id}` : `claude#${r.id}`;
  }

  function statusWord(r: Row): string {
    if (r.failed) return "failed";
    if (r.kind === "shell") return r.exited ? "exited" : "";
    return r.status === "needs" ? "needs you" : (r.status ?? "");
  }

  function needsCount(p: Project): number {
    return p.sessions.filter((s) => s.status === "needs" && !s.muted).length;
  }

</script>

<main>
  {#if link === "unpaired"}
    <div class="empty">
      {#if refusal}<p class="refusal">{refusals[refusal] ?? refusal}</p>{/if}
      Open Remote Control on your Mac (⌘⇧O) and scan its QR code.
    </div>
  {:else if term}
    <Term
      bind:this={termView}
      title={`term#${term.id}`}
      gone={term.gone}
      onback={() => history.back()}
      oninput={(data) => term && conn.send({ t: "input", handle: term.handle, id: term.id, data })}
      onclose={() => term && closeInstance(term.handle, term.id, "shell")}
      onkey={(key) => term && conn.send({ t: "key", handle: term.handle, id: term.id, key })}
    />
  {:else if chat}
    <Chat
      title={chatRow ? `${label(chatRow)}${chatRow.name || chatRow.task ? " · " + (chatRow.name ?? chatRow.task) : ""}` : `claude#${chat.id}`}
      status={chatRow?.status ?? null}
      items={chat.items}
      gone={chat.gone}
      loading={chat.loading}
      onback={() => history.back()}
      onsend={sendText}
      onclose={() => chat && closeInstance(chat.handle, chat.id, "claude")}
      onkey={sendKey}
      dialog={chatRow?.status === "needs" ? (chatRow.dialog ?? null) : null}
      onanswer={sendAnswer}
    />
  {:else}
    <header>
      <span class="title">Mulpex</span>
      <span class="link {link}">
        {link === "online"
          ? "connected · encrypted"
          : link === "mac-offline"
            ? "Mac is offline"
            : link === "pairing"
              ? "pairing…"
              : link === "offline"
                ? "reconnecting…"
                : "connecting…"}
      </span>
      {#if push !== "unsupported" || /iPhone|iPad/.test(navigator.userAgent)}
        <button class="bell" class:on={push === "on"} disabled={push === "busy"} onclick={togglePush}>
          {push === "on" ? "🔔 On" : "🔕 Notify me"}
        </button>
      {/if}
    </header>
    {#if pushNote}<div class="pushnote" dir="auto">{pushNote}</div>{/if}

    {#if view}
      <nav class="tabs">
        {#each view.projects as p (p.handle)}
          <button
            class="tab"
            class:active={shown?.handle === p.handle}
            onclick={() => (picked = p.handle)}
          >
            <span dir="auto">{p.name}</span>
            {#if needsCount(p) > 0}<span class="badge">{needsCount(p)}</span>{/if}
          </button>
        {/each}
      </nav>

      {#if shown}
        <div class="actions">
          <button onclick={() => startInstance(shown.handle, "claude")}>+ Claude</button>
          <button onclick={() => startInstance(shown.handle, "shell")}>+ Terminal</button>
          {#if starting}<span class="err" dir="auto">{starting}</span>{/if}
        </div>
        <ul class="rows">
          {#each ordered(shown.sessions) as { row, depth } (row.id)}
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
            <li
              class="row"
              class:muted={row.muted}
              class:tappable={true}
              style="padding-inline-start: {0.9 + depth * 1.1}rem"
              onclick={() => openChat(shown.handle, row)}
            >
              <span class="dot {row.failed ? 'failed' : (row.status ?? row.kind)}"></span>
              <div class="text">
                <div class="top">
                  <span class="id">{label(row)}</span>
                  <span class="st {row.status ?? ''}">{statusWord(row)}</span>
                  {#if row.ctx_pct != null}<span class="ctx">{row.ctx_pct}%</span>{/if}
                </div>
                {#if row.name || row.task}
                  <div class="name" dir="auto">{row.name ?? row.task}</div>
                {/if}
              </div>
            </li>
          {:else}
            <li class="none">No instances in this project.</li>
          {/each}
        </ul>
      {:else}
        <div class="empty">No projects are open on the Mac.</div>
      {/if}
    {:else if link === "mac-offline"}
      <div class="empty">Remote Control is off on the Mac, or the Mac can't reach the relay.</div>
    {:else}
      <div class="empty">Waiting for the Mac…</div>
    {/if}
  {/if}
</main>

<style>
  :global(:root) {
    --bg: #ffffff;
    --bg-elev: #f4f5f7;
    --text: #1f2328;
    --label: #656d76;
    --border: #d0d7de;
    --accent: #0969da;
    --green: #1a7f37;
    --yellow: #9a6700;
    --red: #cf222e;
    color-scheme: light dark;
  }
  @media (prefers-color-scheme: dark) {
    :global(:root) {
      --bg: #0d1117;
      --bg-elev: #161b22;
      --text: #e6edf3;
      --label: #8b949e;
      --border: #30363d;
      --accent: #58a6ff;
      --green: #3fb950;
      --yellow: #d29922;
      --red: #f85149;
    }
  }
  :global(html, body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font: 16px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    -webkit-text-size-adjust: 100%;
  }
  main {
    min-height: 100dvh;
    padding-top: env(safe-area-inset-top);
    padding-bottom: env(safe-area-inset-bottom);
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    font-weight: 600;
  }
  .link {
    font-size: 0.8rem;
    color: var(--label);
  }
  .link.online {
    color: var(--green);
  }
  .link.mac-offline {
    color: var(--red);
  }
  .tabs {
    display: flex;
    gap: 0.4rem;
    overflow-x: auto;
    padding: 0.6rem 1rem;
    border-bottom: 1px solid var(--border);
    scrollbar-width: none;
  }
  .tab {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.8rem;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg-elev);
    color: var(--text);
    font: inherit;
    font-size: 0.9rem;
  }
  .tab.active {
    border-color: var(--accent);
    color: var(--accent);
  }
  .badge {
    min-width: 1.2rem;
    padding: 0 0.3rem;
    border-radius: 999px;
    background: var(--red);
    color: #fff;
    font-size: 0.75rem;
    text-align: center;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .row {
    display: flex;
    align-items: flex-start;
    gap: 0.7rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .bell {
    margin-inline-start: 0.6rem;
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--border);
    border-radius: 999px;
    background: none;
    color: var(--label);
    font: inherit;
    font-size: 0.78rem;
  }
  .bell.on {
    color: var(--green);
    border-color: var(--green);
  }
  .pushnote {
    padding: 0.5rem 1rem;
    font-size: 0.85rem;
    color: var(--label);
    border-bottom: 1px solid var(--border);
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .actions button {
    padding: 0.4rem 0.8rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-elev);
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }
  .actions .err {
    color: var(--red);
    font-size: 0.8rem;
  }
  .row.tappable {
    cursor: pointer;
  }
  .row.tappable:active {
    background: var(--bg-elev);
  }
  .row.muted {
    opacity: 0.5;
  }
  .dot {
    flex: 0 0 auto;
    width: 0.65rem;
    height: 0.65rem;
    margin-top: 0.4rem;
    border-radius: 50%;
    background: var(--border);
  }
  .dot.waiting {
    background: var(--green);
  }
  .dot.working {
    background: var(--yellow);
  }
  .dot.needs,
  .dot.failed {
    background: var(--red);
  }
  .dot.shell {
    background: var(--label);
    border-radius: 2px;
  }
  .text {
    min-width: 0;
    flex: 1;
  }
  .top {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
  }
  .id {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.9rem;
  }
  .st {
    font-size: 0.8rem;
    color: var(--label);
  }
  .st.needs {
    color: var(--red);
  }
  .ctx {
    margin-inline-start: auto;
    font-size: 0.75rem;
    color: var(--label);
  }
  .name {
    margin-top: 0.15rem;
    font-size: 0.9rem;
    color: var(--label);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    unicode-bidi: plaintext;
  }
  .refusal {
    color: var(--red);
  }
  .none,
  .empty {
    padding: 2rem 1rem;
    color: var(--label);
    text-align: center;
  }
</style>
