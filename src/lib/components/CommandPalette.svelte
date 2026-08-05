<script lang="ts">
  /**
   * ⌘P — the command palette: projects, the active project's sessions, and every
   * Mulpex command, in one fuzzy list.
   *
   * It started as a project quick-switcher, and grew into the app's discovery
   * surface: the status bar used to advertise all twelve accelerators, which grew
   * 1:1 with the feature set and silently clipped its own tail on a narrow window.
   * Now the bar shows the few keys that apply to what's focused plus a `⌘P all`
   * anchor, and *this* is where the rest lives. That also makes the accelerators
   * learnable by use — every row carries the key that does the same thing, so the
   * palette teaches you out of needing it.
   *
   * Actions dispatch by **menu id** through `App.svelte::handleMenu`, the same
   * string-keyed switch the native menu already feeds. There is no second
   * implementation of any command here, so the palette cannot drift from the menu.
   *
   * Sessions are the active project's only. A row for a background project's
   * session would have to switch project on the way, and ids repeat per project so
   * every row would need a disambiguating prefix — for a jump you can already make
   * with two keystrokes (⌘1–9 then click / ⌘[ ⌘]).
   */
  import {
    projects,
    activeProjectHandle,
    sessions,
    activeId,
    statuses,
    tasks,
    type ProjectHandle,
  } from "../stores";
  import type { Status } from "../ipc";
  import { terminals } from "../terminals";

  let {
    onproject,
    onsession,
    onaction,
    onclose,
  }: {
    onproject: (h: ProjectHandle) => void;
    onsession: (id: number) => void;
    /** A menu id for `handleMenu` — the palette runs no command itself. */
    onaction: (menuId: string) => void;
    onclose: () => void;
  } = $props();

  let query = $state("");
  let index = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();
  let rowEls: HTMLElement[] = [];

  $effect(() => {
    inputEl?.focus();
  });

  const DOT: Record<Status, string> = {
    working: "var(--dot-working)",
    waiting: "var(--dot-ready)",
    needs: "var(--dot-needs)",
  };
  const STATUS_LABEL: Record<Status, string> = {
    working: "working",
    waiting: "ready",
    needs: "needs you",
  };

  type Group = "Projects" | "Sessions" | "Actions";

  interface Row {
    key: string;
    group: Group;
    title: string;
    /** Second line: a path, a session's name/task, or a hint. */
    sub: string;
    /** Accelerator chip, or "" for a command that has no key. */
    keys: string;
    /** What the fuzzy query is matched against. */
    hay: string;
    /** Status colour (claude rows) — mutually exclusive with `sigil`. */
    dot?: string;
    /** The `$` marker a terminal row gets where an instance gets its dot. */
    sigil?: boolean;
    /** The project you're in / the session you're looking at. */
    current?: boolean;
    run: () => void;
  }

  /** Subsequence (fuzzy) match: every char of `q` appears in order in `text`. */
  function fuzzy(q: string, text: string): boolean {
    q = q.toLowerCase();
    text = text.toLowerCase();
    let i = 0;
    for (const ch of text) if (i < q.length && ch === q[i]) i++;
    return i === q.length;
  }

  function base(p: string): string {
    return p.split("/").filter(Boolean).pop() ?? p;
  }

  const rows = $derived.by<Row[]>(() => {
    const q = query.trim();
    const out: Row[] = [];

    for (const p of $projects.values()) {
      out.push({
        key: `p${p.handle}`,
        group: "Projects",
        title: p.name,
        sub: p.dir,
        keys: "",
        hay: `${p.name} ${p.dir}`,
        current: p.handle === $activeProjectHandle,
        run: () => onproject(p.handle),
      });
    }

    // `$sessions` is already in sidebar order (muted sunk to the bottom), so the
    // palette lists them the way the list you're looking at does.
    for (const s of $sessions) {
      const shell = s.kind === "shell";
      const st = $statuses.get(s.id) ?? "waiting";
      const task = shell ? "" : ($tasks.get(s.id) ?? "");
      const fallback = shell
        ? s.exited
          ? "exited"
          : "running"
        : s.muted
          ? "muted"
          : STATUS_LABEL[st];
      out.push({
        key: `s${s.id}`,
        group: "Sessions",
        title: shell ? `term #${s.id}` : `claude #${s.id}`,
        sub: s.name || task || fallback,
        keys: "",
        hay: `${shell ? "term" : "claude"} #${s.id} ${s.name} ${task}`,
        dot: shell || s.muted ? undefined : DOT[st],
        sigil: shell,
        current: s.id === $activeId,
        run: () => onsession(s.id),
      });
    }

    // Availability mirrors the menu's own logic in `handleMenu`: an action that
    // would resolve to a no-op simply isn't offered. Destructive ones are here
    // without a confirmation step, matching ⌘W / ⌘⇧W exactly — the palette is a
    // second door to the same command, not a different one.
    const cur = $sessions.find((s) => s.id === $activeId) ?? null;
    const isClaude = cur != null && cur.kind !== "shell";
    const acts: Array<[string, string, string, boolean]> = [
      ["new_session", "New Session", "⌘T", true],
      ["new_terminal", "New Terminal", "⌘⇧T", true],
      ["rename", "Rename Session…", "⌘R", cur != null],
      // Mute is meaningless for a terminal — it produces none of the signals mute
      // silences, and the backend refuses to record the flag (see InstanceList).
      ["mute", cur?.muted ? "Unmute Session" : "Mute Session", "⌘M", isClaude],
      ["close_session", "Close Session", "⌘W", cur != null],
      ["messages", "Messages", "⌘⇧M", true],
      ["next", "Next Session", "⌘]", $sessions.length > 1],
      ["prev", "Previous Session", "⌘[", $sessions.length > 1],
      ["open_project", "Open Project…", "⌘O", true],
      ["close_project", "Close Project", "⌘⇧W", true],
      ["next_project", "Next Project", "⌘⇧]", $projects.size > 1],
      ["prev_project", "Previous Project", "⌘⇧[", $projects.size > 1],
      // No accelerator by design, so the palette is its only keyboard route.
      ["check_updates", "Check for Updates…", "", true],
    ];
    for (const [menu, title, keys, show] of acts) {
      if (!show) continue;
      out.push({
        key: `a${menu}`,
        group: "Actions",
        title,
        // Carries over the switcher's old affordance: typing a name and landing
        // on "Open Project…" tells you what folder you're about to go looking for.
        sub: menu === "open_project" && q ? `pick a folder (“${base(q)}”)` : "",
        keys,
        hay: `${title} ${keys}`,
        run: () => onaction(menu),
      });
    }

    if (!q) return out;
    // "Open Project…" survives a query that doesn't match it, and this is not an
    // oversight: it's the one row that consumes the query as *input* rather than
    // as a filter, so typing the name of a folder that isn't open yet must still
    // leave you somewhere to go. The old switcher kept it outside the list for
    // exactly this reason, and the palette must not bottom out in a dead "no
    // matches" either.
    //
    // It goes on the END, though — where that switcher also put it. Left in its
    // natural Actions slot it outranks genuine matches, and since the highlight
    // starts at row 0, typing "close project" and hitting Enter opened the folder
    // picker. Measured: that is exactly what the first version of this did.
    const hit = out.filter((r) => fuzzy(q, r.hay));
    if (!hit.some((r) => r.key === "aopen_project")) {
      const fallback = out.find((r) => r.key === "aopen_project");
      if (fallback) hit.push(fallback);
    }
    return hit;
  });

  // Narrowing the list must put the highlight back on the best match, not leave
  // it wherever the previous query happened to park it.
  $effect(() => {
    void query;
    index = 0;
  });

  // The list is long enough now to scroll, so the highlight has to stay visible
  // under ↑/↓ — otherwise it walks off the bottom and the palette looks stuck.
  $effect(() => {
    rowEls[index]?.scrollIntoView({ block: "nearest" });
  });

  function choose(i: number) {
    rows[i]?.run();
  }

  function close() {
    onclose();
    terminals.refocus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      index = Math.min(index + 1, rows.length - 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      index = Math.max(index - 1, 0);
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(index);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }
</script>

<div class="backdrop" role="button" tabindex="-1" onclick={close} onkeydown={() => {}}>
  <div
    class="sheet"
    role="dialog"
    tabindex="-1"
    aria-label="Command palette"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <input
      bind:this={inputEl}
      bind:value={query}
      onkeydown={onKey}
      placeholder="Projects, sessions, commands…"
    />
    <ul class="results">
      {#each rows as r, i (r.key)}
        {#if i === 0 || rows[i - 1]?.group !== r.group}
          <li class="head">{r.group}</li>
        {/if}
        <li>
          <button
            class="row"
            class:active={i === index}
            class:current={r.current}
            bind:this={rowEls[i]}
            onmouseenter={() => (index = i)}
            onclick={() => choose(i)}
          >
            <span class="main">
              <span class="title">
                {#if r.dot}
                  <span class="dot" style:background={r.dot}></span>
                {:else if r.sigil}
                  <span class="sigil" aria-hidden="true">$</span>
                {/if}
                <span class="name">{r.title}</span>
              </span>
              {#if r.sub}<span class="sub">{r.sub}</span>{/if}
            </span>
            {#if r.keys}<kbd>{r.keys}</kbd>{/if}
          </button>
        </li>
      {/each}
    </ul>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: grid;
    justify-items: center;
    align-items: start;
    padding-top: 12vh;
  }
  .sheet {
    width: min(34rem, 92vw);
    background: var(--bg-elev);
    border: 1px solid var(--border-focus);
    border-radius: 10px;
    overflow: hidden;
  }
  input {
    width: 100%;
    padding: 0.7rem 0.9rem;
    background: var(--bg);
    color: var(--text);
    border: none;
    border-bottom: 1px solid var(--border);
    font: inherit;
    font-size: 0.95rem;
  }
  input:focus {
    outline: none;
  }
  .results {
    list-style: none;
    margin: 0;
    padding: 0.3rem;
    max-height: 50vh;
    overflow-y: auto;
  }
  .head {
    padding: 0.45rem 0.55rem 0.2rem;
    color: var(--text-faint);
    font-size: 0.68rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.4rem 0.55rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    text-align: left;
  }
  .row.active {
    background: var(--bg);
    border-color: var(--border-focus);
  }
  .main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
  }
  .title {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    max-width: 100%;
  }
  .name {
    font-weight: 600;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* The project you're in and the session you're looking at, so a list of
     near-identical rows says which one is already the answer. */
  .row.current .name {
    color: var(--accent);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  /* Occupies the dot's slot so terminal and instance rows line up. */
  .sigil {
    width: 8px;
    flex: none;
    text-align: center;
    color: var(--text-faint);
    font-size: 0.8rem;
    line-height: 1;
  }
  .sub {
    max-width: 100%;
    color: var(--text-faint);
    font-size: 0.75rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  kbd {
    flex: none;
    font: inherit;
    font-size: 0.75rem;
    color: var(--text);
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 3px;
    padding: 0 0.25rem;
  }
</style>
