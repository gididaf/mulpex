// End-to-end encryption, phone side. The Mac's half is
// `src-tauri/src/remote/crypto.rs`, which explains the scheme; the two are
// pinned together by its `matches_the_phone` test, whose expected values come
// from this file. Change one, change both.
//
// @noble rather than WebCrypto: `crypto.subtle` exists only on https pages, and
// this page is also served over plain http on a LAN. `getRandomValues` (which
// noble's randomBytes uses) is available everywhere.

import { x25519 } from "@noble/curves/ed25519.js";
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { randomBytes } from "@noble/hashes/utils.js";

const PROTO = new TextEncoder().encode("mulpex-remote-v1");

export function b64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function unb64(s: string): Uint8Array {
  const std = s.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(std + "=".repeat((4 - (std.length % 4)) % 4));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

export const newKey = (): Uint8Array => randomBytes(32);
export const publicOf = (secret: Uint8Array): Uint8Array => x25519.getPublicKey(secret);

export function pairProof(secret: Uint8Array, device: string, devicePub: Uint8Array): Uint8Array {
  return hmac(sha256, secret, concat(new TextEncoder().encode(device), devicePub));
}

function nonce(n: number): Uint8Array {
  const b = new Uint8Array(12);
  new DataView(b.buffer).setBigUint64(4, BigInt(n), true);
  return b;
}

export class Channel {
  private nSend = 0;
  private nRecv = 0;
  private send: Uint8Array;
  private recv: Uint8Array;
  constructor(send: Uint8Array, recv: Uint8Array) {
    this.send = send;
    this.recv = recv;
  }

  seal(plain: Uint8Array): string {
    const ct = chacha20poly1305(this.send, nonce(this.nSend)).encrypt(plain);
    this.nSend++;
    return b64(ct);
  }

  /** null on anything wrong (forged, replayed, out of order): drop the session. */
  open(sealed: string): Uint8Array | null {
    try {
      const pt = chacha20poly1305(this.recv, nonce(this.nRecv)).decrypt(unb64(sealed));
      this.nRecv++;
      return pt;
    } catch {
      return null;
    }
  }
}

/** Phone → Mac key and Mac → phone key. Exported for the test vectors. */
export function deriveKeys(
  eph: Uint8Array,
  device: Uint8Array,
  hostPub: Uint8Array,
  hostEph: Uint8Array,
): [Uint8Array, Uint8Array] {
  const ikm = concat(
    x25519.getSharedSecret(eph, hostEph),
    x25519.getSharedSecret(eph, hostPub),
    x25519.getSharedSecret(device, hostEph),
  );
  const okm = hkdf(sha256, ikm, PROTO, concat(PROTO, publicOf(eph), hostEph), 64);
  return [okm.slice(0, 32), okm.slice(32)];
}

/** The phone's half of a connection's handshake, once the Mac sent its
 *  ephemeral key back. */
export function phoneHandshake(
  eph: Uint8Array,
  device: Uint8Array,
  hostPub: Uint8Array,
  hostEph: Uint8Array,
): Channel {
  const [p2h, h2p] = deriveKeys(eph, device, hostPub, hostEph);
  return new Channel(p2h, h2p);
}
