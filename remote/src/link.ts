// The phone's connection to its Mac: pairing, the per-connection handshake,
// and sealed frames (`crypto.ts`; the Mac's side is `src-tauri/src/remote/`).
//
// The pairing link puts everything in the URL fragment —
// `#p=<host id>.<Mac public key>.<one-time secret>` — which the browser never
// sends to the relay. We read it once, strip it from the address bar, and keep
// what lasts (host id, the Mac's public key, this phone's own key) in
// localStorage. The one-time secret is used once and never stored.

import { b64, newKey, pairProof, phoneHandshake, publicOf, unb64, type Channel } from "./crypto";

type Pairing = {
  host: string;
  hostPub: string;
  dev: string;
  devSecret: string;
  /** Set once the Mac accepted this phone. */
  paired: boolean;
};

const STORE = "mulpex.pairing";

function load(): Pairing | null {
  try {
    return JSON.parse(localStorage.getItem(STORE) ?? "null");
  } catch {
    return null;
  }
}

function save(p: Pairing | null) {
  try {
    if (p) localStorage.setItem(STORE, JSON.stringify(p));
    else localStorage.removeItem(STORE);
  } catch {
    // Private mode: pairing then lasts only as long as this page.
  }
}

function deviceName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android";
  return "Browser";
}

export type LinkState =
  | "unpaired" // no pairing: scan the QR code
  | "connecting"
  | "pairing"
  | "online"
  | "mac-offline"
  | "offline";

export class Link {
  state: LinkState = "connecting";
  /** Why the Mac refused us, if it did. */
  refusal: string | null = null;

  private pairing: Pairing | null = load();
  /** The QR code's one-time secret, while a pairing is in flight. */
  private oneTime: Uint8Array | null = null;
  private ws: WebSocket | null = null;
  private ch: Channel | null = null;
  private eph: Uint8Array | null = null;
  private retry = 1000;

  constructor(
    private onState: (link: Link) => void,
    private onMessage: (msg: any) => void,
  ) {
    this.readFragment();
    if (!this.pairing) {
      this.state = "unpaired";
      return;
    }
    this.connect();
  }

  /** A fresh QR scan: remember the Mac, keep this phone's key if we had one. */
  private readFragment() {
    const m = location.hash.match(/^#p=([A-Za-z0-9-]+)\.([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)$/);
    if (!m) return;
    history.replaceState(null, "", location.pathname + location.search);
    const [, host, hostPub, secret] = m;
    const prev = this.pairing?.host === host ? this.pairing : null;
    this.pairing = {
      host,
      hostPub,
      dev: prev?.dev ?? b64(newKey().slice(0, 12)),
      devSecret: prev?.devSecret ?? b64(newKey()),
      paired: false,
    };
    this.oneTime = unb64(secret);
    save(this.pairing);
  }

  private set(state: LinkState) {
    this.state = state;
    this.onState(this);
  }

  private raw(data: unknown) {
    this.ws?.send(JSON.stringify({ data }));
  }

  /** Send a request to the Mac, sealed. Dropped if not connected. */
  send(msg: unknown) {
    if (!this.ch) return;
    this.raw({ t: "sealed", c: this.ch.seal(new TextEncoder().encode(JSON.stringify(msg))) });
  }

  private connect() {
    const p = this.pairing;
    if (!p) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${scheme}://${location.host}/ws/client?host=${encodeURIComponent(p.host)}`);
    this.ws = ws;
    this.ch = null;
    this.set("connecting");
    ws.onmessage = (e) => {
      let msg: any;
      try {
        msg = JSON.parse(e.data);
      } catch {
        return;
      }
      if (msg.event === "host") {
        this.ch = null;
        if (msg.online) this.begin();
        else this.set("mac-offline");
        return;
      }
      if (msg.data) this.onData(msg.data);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ch = null;
      if (this.state === "unpaired") return;
      this.set("offline");
      setTimeout(() => this.connect(), this.retry);
      this.retry = Math.min(this.retry * 2, 10000);
    };
  }

  /** The Mac is reachable: pair first if we have a fresh QR, then handshake. */
  private begin() {
    const p = this.pairing!;
    if (!p.paired && this.oneTime) {
      this.set("pairing");
      const dp = publicOf(unb64(p.devSecret));
      this.raw({
        t: "pair",
        dev: p.dev,
        dp: b64(dp),
        name: deviceName(),
        proof: b64(pairProof(this.oneTime, p.dev, dp)),
      });
      return;
    }
    this.eph = newKey();
    this.raw({ t: "hs", dev: p.dev, ep: b64(publicOf(this.eph)) });
  }

  private onData(d: any) {
    const p = this.pairing!;
    switch (d.t) {
      case "paired":
        p.paired = true;
        this.oneTime = null;
        save(p);
        this.begin();
        break;
      case "hs":
        this.ch = phoneHandshake(this.eph!, unb64(p.devSecret), unb64(p.hostPub), unb64(d.eh));
        this.eph = null;
        this.retry = 1000;
        this.refusal = null;
        this.set("online");
        this.send({ t: "hello" });
        break;
      case "sealed": {
        const plain = this.ch?.open(d.c);
        if (!plain) {
          // Out of step with the Mac: start this connection over.
          this.ws?.close();
          return;
        }
        this.onMessage(JSON.parse(new TextDecoder().decode(plain)));
        break;
      }
      case "denied":
        if (d.why === "bad frame") {
          this.ws?.close();
          return;
        }
        // Not paired, removed on the Mac, or a stale QR code.
        this.refusal = d.why;
        this.oneTime = null;
        this.pairing = null;
        save(null);
        this.set("unpaired");
        this.ws?.close();
        break;
    }
  }
}
