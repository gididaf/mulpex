<script lang="ts">
  // One shell terminal, live, as READABLE TEXT. The Mac streams the same bytes
  // its own xterm gets (`pty.rs::OutputSink::tap`). They are fed to an xterm
  // here that is never shown — it is only the emulator, kept at the PTY's exact
  // size (the desktop's one geometry, see docs/rendering.md) so cursor moves,
  // `\r` overwrites and colors all come out right. What is shown is its buffer
  // turned into lines: rows the desktop wrapped are joined back into one line
  // and re-wrapped at the phone's width, in a normal font, colors kept, each
  // line its own BiDi paragraph so Hebrew reads. Full-screen programs (htop,
  // vim) don't survive the re-wrap; that was the trade chosen for a phone.
  import { onMount, tick } from "svelte";
  import { Terminal, type IBufferCell } from "@xterm/xterm";

  let {
    title,
    gone,
    onback,
    oninput,
    onkey,
    onclose,
  }: {
    title: string;
    gone: boolean;
    onback: () => void;
    oninput: (data: string) => void;
    onkey: (key: string) => void;
    /** Close this instance (the phone confirms first). */
    onclose: () => void;
  } = $props();

  // The desktop's colors (src/lib/terminals.ts THEME), as the 16-color palette.
  const BASE16 = [
    "#1a1a1a", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#dcdcdc",
    "#5c6370", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff",
  ];
  const FG = "#e6e6e6";
  const BG = "#0d0d0f";
  /** How many lines are drawn: the end of the scrollback is what matters. */
  const MAX_LINES = 600;

  function palette(i: number): string {
    if (i < 16) return BASE16[i];
    if (i < 232) {
      const l = [0, 95, 135, 175, 215, 255];
      const n = i - 16;
      return `rgb(${l[Math.floor(n / 36)]},${l[Math.floor(n / 6) % 6]},${l[n % 6]})`;
    }
    const g = 8 + (i - 232) * 10;
    return `rgb(${g},${g},${g})`;
  }

  function color(isRGB: boolean, isPalette: boolean, value: number): string | null {
    if (isRGB) return `#${value.toString(16).padStart(6, "0")}`;
    if (isPalette) return palette(value);
    return null;
  }

  function esc(t: string): string {
    return t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function styleOf(c: IBufferCell, cursor: boolean): string {
    let fg = color(c.isFgRGB(), c.isFgPalette(), c.getFgColor());
    let bg = color(c.isBgRGB(), c.isBgPalette(), c.getBgColor());
    // Bold text in the first 8 colors is drawn bright, as most terminals do.
    if (c.isBold() && c.isFgPalette() && c.getFgColor() < 8) fg = BASE16[c.getFgColor() + 8];
    if (c.isInverse() || cursor) [fg, bg] = [bg ?? BG, fg ?? FG];
    let css = "";
    if (fg) css += `color:${fg};`;
    if (bg) css += `background:${bg};`;
    if (c.isBold()) css += "font-weight:600;";
    if (c.isItalic()) css += "font-style:italic;";
    if (c.isUnderline()) css += "text-decoration:underline;";
    if (c.isDim()) css += "opacity:.6;";
    return css;
  }

  let term: Terminal | null = null;
  let host: HTMLDivElement | undefined = $state();
  let scroller: HTMLDivElement | undefined = $state();
  let html = $state("");
  let line = $state("");
  let pinned = true;
  let queued = false;

  /** Turn the emulator's buffer into readable HTML lines. */
  function render() {
    queued = false;
    if (!term) return;
    const buf = term.buffer.active;
    const cell = buf.getNullCell();
    const curY = buf.baseY + buf.cursorY;
    const curX = buf.cursorX;
    // Find where the last MAX_LINES logical lines start: a wrapped row belongs
    // to the line above it.
    let start = buf.length - 1;
    let lines = 0;
    while (start > 0 && lines < MAX_LINES) {
      if (!buf.getLine(start)?.isWrapped) lines++;
      start--;
    }
    while (start > 0 && buf.getLine(start)?.isWrapped) start--;

    const out: string[] = [];
    let cur = "";
    let runStyle = "";
    let runText = "";
    const flush = () => {
      if (runText) cur += runStyle ? `<span style="${runStyle}">${esc(runText)}</span>` : esc(runText);
      runText = "";
    };
    for (let y = start; y < buf.length; y++) {
      const row = buf.getLine(y);
      if (!row) continue;
      if (!row.isWrapped && y !== start) {
        flush();
        out.push(cur);
        cur = "";
      }
      // Trailing blanks are layout, not text — except where the cursor sits.
      let end = row.translateToString(true).length;
      if (y === curY) end = Math.max(end, curX + 1);
      for (let x = 0; x < end; x++) {
        row.getCell(x, cell);
        if (cell.getWidth() === 0) continue;
        const st = styleOf(cell, y === curY && x === curX);
        if (st !== runStyle) {
          flush();
          runStyle = st;
        }
        runText += cell.getChars() || " ";
      }
      flush();
    }
    out.push(cur);
    // Blank lines at the very end (the unused screen below the prompt) go.
    while (out.length > 1 && out[out.length - 1] === "") out.pop();
    html = out.map((l) => `<div>${l || "&nbsp;"}</div>`).join("");
    if (pinned) tick().then(() => scroller?.scrollTo({ top: scroller.scrollHeight }));
  }

  function schedule() {
    if (queued) return;
    queued = true;
    requestAnimationFrame(render);
  }

  /** Start over at the PTY's size with its recent output (a fresh open). */
  export function reset(c: number, r: number, data: Uint8Array) {
    if (!term) return;
    term.reset();
    if (c > 0 && r > 0) term.resize(c, r);
    pinned = true;
    term.write(data, schedule);
  }

  export function write(c: number, r: number, data: Uint8Array) {
    if (!term) return;
    // A size we can't trust (0×0, or missing) keeps the current one.
    if (c > 0 && r > 0 && (c !== term.cols || r !== term.rows)) term.resize(c, r);
    term.write(data, schedule);
  }

  function onScroll() {
    if (!scroller) return;
    pinned = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;
  }

  onMount(() => {
    term = new Terminal({ cols: 80, rows: 24, scrollback: 5000, disableStdin: true, allowProposedApi: true });
    // Opened off-screen: only the emulator is used, never its drawing.
    term.open(host!);
    return () => {
      term?.dispose();
      term = null;
    };
  });

  function sendLine() {
    oninput(line + "\r");
    line = "";
  }

  const keys: Array<[string, string]> = [
    ["esc", "Esc"],
    ["ctrl-c", "^C"],
    ["ctrl-d", "^D"],
    ["tab", "Tab"],
    ["up", "↑"],
    ["down", "↓"],
    ["left", "←"],
    ["right", "→"],
    ["enter", "⏎"],
    ["ctrl-l", "^L"],
    ["ctrl-z", "^Z"],
  ];
</script>

<div class="term">
  <header>
    <button class="back" onclick={onback} aria-label="Back">‹</button>
    <span class="title" dir="auto">{title}</span>
    <button class="close" onclick={onclose}>Close</button>
  </header>
  <div class="hidden" bind:this={host}></div>
  <div class="viewport" bind:this={scroller} onscroll={onScroll}>
    <div class="text">{@html html}</div>
    {#if gone}<div class="gone">This terminal is closed.</div>{/if}
  </div>
  {#if !gone}
    <div class="composer">
      <div class="keys">
        {#each keys as [k, label] (k)}
          <button onclick={() => onkey(k)}>{label}</button>
        {/each}
      </div>
      <div class="input">
        <input
          bind:value={line}
          placeholder="Command…"
          autocapitalize="off"
          autocomplete="off"
          spellcheck="false"
          onkeydown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              sendLine();
            }
          }}
        />
        <button class="send" onclick={sendLine}>Run</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .term {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    padding-top: env(safe-area-inset-top);
    box-sizing: border-box;
    background: #0d0d0f;
    color: #e6e6e6;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid #30363d;
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
    color: #58a6ff;
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
  .hidden {
    position: absolute;
    left: -10000px;
    top: 0;
    width: 800px;
    visibility: hidden;
  }
  .viewport {
    flex: 1;
    overflow-y: auto;
    position: relative;
    padding: 0.5rem 0.6rem;
  }
  .text {
    font-family: ui-monospace, "SF Mono", Menlo, Monaco, monospace;
    font-size: 13px;
    line-height: 1.35;
  }
  .text :global(div) {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    unicode-bidi: plaintext;
  }
  .gone {
    padding: 1rem 0;
    text-align: center;
    color: #8b949e;
  }
  .composer {
    border-top: 1px solid #30363d;
    padding: 0.4rem 0.6rem calc(0.5rem + env(safe-area-inset-bottom));
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
    min-width: 2.3rem;
    padding: 0.3rem 0.5rem;
    border: 1px solid #30363d;
    border-radius: 6px;
    background: #161b22;
    color: #e6e6e6;
    font: inherit;
    font-size: 0.8rem;
  }
  .input {
    display: flex;
    gap: 0.4rem;
  }
  input {
    flex: 1;
    min-width: 0;
    padding: 0.5rem 0.6rem;
    border: 1px solid #30363d;
    border-radius: 8px;
    background: #161b22;
    color: #e6e6e6;
    font-family: ui-monospace, Menlo, monospace;
    font-size: 16px;
  }
  .send {
    padding: 0.5rem 0.9rem;
    border: 0;
    border-radius: 8px;
    background: #238636;
    color: #fff;
    font: inherit;
    font-weight: 600;
  }
</style>
