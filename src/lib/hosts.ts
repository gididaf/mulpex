// Remote Hosts: other Mulpexes this one drives (`src-tauri/src/remote/client.rs`,
// docs/remote-hosts.md). Their projects live in the same `projects` store as
// this Mulpex's own, under handles `slot · 2^32 + their handle` — so every
// (handle, id)-keyed thing in the frontend works for them unchanged — and the
// host switcher decides which host's projects the tab bar shows.
//
// This module is only the vocabulary: which host a handle belongs to, the
// hosts' connection states, and which host is in front. The lifecycle (pairing,
// building a host's tabs when it connects) is `remoteHosts.ts`.

import { invoke } from "@tauri-apps/api/core";
import { derived, get, writable } from "svelte/store";

/** Remote handles start here; slot 0 is this Mulpex. Mirrors `client.rs::base_of`. */
export const HOST_BASE = 2 ** 32;

export const slotOf = (handle: number): number => Math.floor(handle / HOST_BASE);
export const isRemote = (handle: number): boolean => handle >= HOST_BASE;

/** The geometry a host's PTYs run at, and who drives it. */
export interface HostGeo {
  cols: number;
  rows: number;
  /** The driving remote desktop's name, or null when the host's own window does. */
  owner: string | null;
  /** This Mulpex holds the lease. */
  mine: boolean;
}

/** Mirrors `client.rs::HostStatus`; also the `host-changed` payload. */
export interface HostStatus {
  slot: number;
  name: string;
  relay_url: string;
  enabled: boolean;
  state: "connecting" | "connected" | "offline" | "error" | "disconnected";
  error: string | null;
  boot: string | null;
  cached_boot: string | null;
  geo: HostGeo | null;
  layout: HostLayout | null;
}

/** What is cached of a host's tabs, to draw them greyed while unreachable. */
export interface HostLayout {
  projects: {
    handle: number;
    project_dir: string;
    project_name: string;
    sessions: import("./ipc").SessionInfo[];
  }[];
}

/** Every paired host, by slot. Empty for anyone who never paired one — then
 *  the host switcher is not drawn at all. */
export const hosts = writable<Map<number, HostStatus>>(new Map());

/** The host whose projects the tab bar shows: 0 is this Mulpex. */
export const activeHost = writable<number>(0);

export function hostName(slot: number): string {
  return slot === 0 ? "Local" : (get(hosts).get(slot)?.name ?? "remote");
}

export function hostOnline(slot: number): boolean {
  return slot === 0 || get(hosts).get(slot)?.state === "connected";
}

/** Live: whether the host in front is reachable (local always is). */
export const activeHostOnline = derived([hosts, activeHost], ([$h, $a]) =>
  $a === 0 ? true : $h.get($a)?.state === "connected",
);

/** Host side: the remote desktop driving THIS Mulpex's geometry right now
 *  (`geometry-changed`), or null when this window does. */
export const geometryDriver = writable<string | null>(null);

// ---- the client commands (`client.rs`) ----

export const hostList = () => invoke<HostStatus[]>("host_list");
export const hostPair = (link: string) => invoke<HostStatus>("host_pair", { link });
export const hostRemove = (host: number) => invoke<void>("host_remove", { host });
export const hostRename = (host: number, name: string) => invoke<void>("host_rename", { host, name });
export const hostConnect = (host: number, on: boolean) => invoke<void>("host_connect", { host, on });
export const hostCacheLayout = (host: number, boot: string | null, layout: HostLayout) =>
  invoke<void>("host_cache_layout", { host, boot, layout });
export const hostGeoClaim = (host: number, cols: number, rows: number) =>
  invoke<void>("host_geo", { host, cols, rows });
export const hostFrontendAction = (host: number, projectHandle: number, id: number, action: string) =>
  invoke<void>("host_frontend_action", { host, projectHandle, id, action });
export const hostUpload = (host: number, paths: string[]) => invoke<string[]>("host_upload", { host, paths });
export const hostClipboardImage = (host: number) => invoke<string | null>("host_clipboard_image", { host });
