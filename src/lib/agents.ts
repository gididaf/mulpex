// One home for what differs between a claude, a codex and a terminal in the
// sidebar's words. A claude and a codex are both *agents*: hub peers that share
// one number space in a project (docs/codex.md). Anything that used to ask
// `kind === "claude"` asks `isAgent` here, so a codex gets the same treatment
// unless a function below says otherwise.

import type { Agent, SessionKind } from "./ipc";

/** A claude or a codex — not a terminal. */
export const isAgent = (kind: SessionKind | undefined | null): kind is Agent =>
  kind === "claude" || kind === "codex";

/** The row's agent, or null for a terminal. */
export const agentOf = (kind: SessionKind | undefined | null): Agent | null =>
  isAgent(kind) ? kind : null;

/** The word the sidebar uses: `claude`, `codex`, `term`. */
export const kindWord = (kind: SessionKind | undefined | null): string =>
  kind === "shell" ? "term" : kind === "codex" ? "codex" : "claude";

/** The row label, with a space: `claude #1`, `codex #2`, `term #5`. */
export const rowLabel = (kind: SessionKind | undefined | null, id: number): string =>
  `${kindWord(kind)} #${id}`;

/** The written / address form, no space: `claude#1`, `codex#2`, `term#5`. */
export const addr = (kind: SessionKind | undefined | null, id: number): string =>
  `${kindWord(kind)}#${id}`;

/** "Claude" / "Codex", for menu items and messages. */
export const agentTitle = (agent: Agent): string => (agent === "codex" ? "Codex" : "Claude");

/** The agent a ⌘⌥S handoff moves this row to. */
export const otherAgent = (agent: Agent): Agent => (agent === "codex" ? "claude" : "codex");

/** The address of instance `id` among `list` — `claude#3` / `codex#3` by its
 *  kind. One no longer open has no known kind, so it is just `#3` (the number
 *  alone is what the hub resolves; guessing `claude` could name the wrong one). */
export const addrIn = (list: readonly { id: number; kind: SessionKind }[], id: number): string => {
  const s = list.find((x) => x.id === id);
  return s ? addr(s.kind, id) : `#${id}`;
};
