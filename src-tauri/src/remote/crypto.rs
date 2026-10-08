//! End-to-end encryption between this Mac and a paired phone. The relay carries
//! every byte and must be able to read none of them.
//!
//! The phone (`remote/src/crypto.ts`) implements the same steps with @noble; the
//! two are pinned together by `tests::matches_the_phone`, whose expected values
//! were produced by the phone's code, not by this file.
//!
//! **Keys.** The Mac has a long-lived X25519 key (its public half rides in the
//! pairing QR). Each phone makes its own long-lived key when it pairs, and the
//! Mac stores the public half (`devices.json`).
//!
//! **Pairing.** The QR also carries a one-time secret, in the URL *fragment*, so
//! it never reaches the relay or any log. The phone proves it saw the QR with
//! `HMAC(secret, device id ‖ device public key)` — bound to its key, so a relay
//! that swapped the key would break the proof.
//!
//! **Each connection** runs a handshake with fresh ephemeral keys on both sides
//! and mixes three Diffie-Hellmans, as Noise KK does:
//!   DH(phone eph, Mac eph)     — fresh keys for this connection
//!   DH(phone eph, Mac static)  — only the real Mac can compute it
//!   DH(phone static, Mac eph)  — only the paired phone can compute it
//! HKDF-SHA256 turns them into one key per direction. Frames are
//! ChaCha20-Poly1305 with a counter nonce; a frame that is replayed, dropped or
//! reordered fails to open, and the connection is dropped.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// HKDF salt, and the prefix of its info. Changing it breaks every phone.
const PROTO: &[u8] = b"mulpex-remote-v1";

pub type Key32 = [u8; 32];

pub fn random32() -> Key32 {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    b
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).ok()
}

pub fn unb64_32(s: &str) -> Option<Key32> {
    unb64(s)?.try_into().ok()
}

pub fn public_of(secret: &Key32) -> Key32 {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

/// `None` for a low-order point, whose "shared" secret is a constant anyone knows.
fn dh(secret: &Key32, public: &Key32) -> Option<Key32> {
    let shared = StaticSecret::from(*secret).diffie_hellman(&PublicKey::from(*public));
    shared.was_contributory().then(|| shared.to_bytes())
}

/// What the phone computes; the Mac only ever checks one (`pair_proof_ok`).
#[cfg(test)]
pub fn pair_proof(secret: &Key32, device: &str, device_pub: &Key32) -> Key32 {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("any key length");
    mac.update(device.as_bytes());
    mac.update(device_pub);
    mac.finalize().into_bytes().into()
}

/// Constant-time, so the proof can't be found a byte at a time.
pub fn pair_proof_ok(secret: &Key32, device: &str, device_pub: &Key32, proof: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("any key length");
    mac.update(device.as_bytes());
    mac.update(device_pub);
    mac.verify_slice(proof).is_ok()
}

/// One direction-split session after a handshake.
pub struct Channel {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    n_send: u64,
    n_recv: u64,
}

fn nonce(n: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b
}

impl Channel {
    fn new(send: &Key32, recv: &Key32) -> Self {
        Channel {
            send: ChaCha20Poly1305::new(Key::from_slice(send)),
            recv: ChaCha20Poly1305::new(Key::from_slice(recv)),
            n_send: 0,
            n_recv: 0,
        }
    }

    /// Encrypt one frame, as base64url.
    pub fn seal(&mut self, plain: &[u8]) -> String {
        let ct = self
            .send
            .encrypt(Nonce::from_slice(&nonce(self.n_send)), plain)
            .expect("chacha20poly1305 cannot fail to encrypt");
        self.n_send += 1;
        b64(&ct)
    }

    /// Decrypt the next frame. `None` on anything wrong — forged, replayed, or
    /// out of order — and the caller must drop the session.
    pub fn open(&mut self, sealed: &str) -> Option<Vec<u8>> {
        let ct = unb64(sealed)?;
        let plain = self.recv.decrypt(Nonce::from_slice(&nonce(self.n_recv)), ct.as_slice()).ok()?;
        self.n_recv += 1;
        Some(plain)
    }
}

/// Phone → Mac key and Mac → phone key, from the three DHs.
fn derive(dhs: [Key32; 3], phone_eph: &Key32, host_eph: &Key32) -> (Key32, Key32) {
    let mut ikm = Vec::with_capacity(96);
    for d in &dhs {
        ikm.extend_from_slice(d);
    }
    let mut info = PROTO.to_vec();
    info.extend_from_slice(phone_eph);
    info.extend_from_slice(host_eph);
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(Some(PROTO), &ikm)
        .expand(&info, &mut okm)
        .expect("64 bytes is a valid HKDF length");
    let mut p2h = [0u8; 32];
    let mut h2p = [0u8; 32];
    p2h.copy_from_slice(&okm[..32]);
    h2p.copy_from_slice(&okm[32..]);
    (p2h, h2p)
}

/// The Mac's half of a handshake with a known phone. Returns the Mac's
/// ephemeral public key (sent back in the clear) and the session.
pub fn host_handshake(
    host_secret: &Key32,
    device_pub: &Key32,
    phone_eph: &Key32,
) -> Option<(Key32, Channel)> {
    host_handshake_with(&random32(), host_secret, device_pub, phone_eph)
}

fn host_handshake_with(
    eph_secret: &Key32,
    host_secret: &Key32,
    device_pub: &Key32,
    phone_eph: &Key32,
) -> Option<(Key32, Channel)> {
    let eph_pub = public_of(eph_secret);
    let dhs = [
        dh(eph_secret, phone_eph)?,
        dh(host_secret, phone_eph)?,
        dh(eph_secret, device_pub)?,
    ];
    let (p2h, h2p) = derive(dhs, phone_eph, &eph_pub);
    Some((eph_pub, Channel::new(&h2p, &p2h)))
}

/// The phone's half, for tests that play a phone against the Mac.
#[cfg(test)]
pub fn tests_phone_channel(eph: &Key32, dev: &Key32, host_pub: &Key32, host_eph: &Key32) -> Channel {
    let dhs = [dh(eph, host_eph).unwrap(), dh(eph, host_pub).unwrap(), dh(dev, host_eph).unwrap()];
    let (p2h, h2p) = derive(dhs, &public_of(eph), host_eph);
    Channel::new(&p2h, &h2p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    use super::tests_phone_channel as phone_side;

    #[test]
    fn both_ends_agree_and_frames_round_trip() {
        let host = random32();
        let dev = random32();
        let eph = random32();
        let (host_eph, mut mac) =
            host_handshake(&host, &public_of(&dev), &public_of(&eph)).unwrap();
        let mut phone = phone_side(&eph, &dev, &public_of(&host), &host_eph);
        for i in 0..3 {
            let msg = format!("hello {i}");
            assert_eq!(mac.open(&phone.seal(msg.as_bytes())).unwrap(), msg.as_bytes());
            assert_eq!(phone.open(&mac.seal(b"view")).unwrap(), b"view");
        }
    }

    #[test]
    fn a_replayed_or_forged_frame_fails() {
        let (host, dev, eph) = (random32(), random32(), random32());
        let (host_eph, mut mac) =
            host_handshake(&host, &public_of(&dev), &public_of(&eph)).unwrap();
        let mut phone = phone_side(&eph, &dev, &public_of(&host), &host_eph);
        let a = phone.seal(b"a");
        assert!(mac.open(&a).is_some());
        assert!(mac.open(&a).is_none(), "replay");
        let mut forged = unb64(&phone.seal(b"b")).unwrap();
        forged[0] ^= 1;
        assert!(mac.open(&b64(&forged)).is_none(), "forgery");
    }

    #[test]
    fn a_phone_without_the_paired_key_gets_garbage_keys() {
        let (host, dev, eph, impostor) = (random32(), random32(), random32(), random32());
        let (host_eph, mut mac) =
            host_handshake(&host, &public_of(&dev), &public_of(&eph)).unwrap();
        let mut phone = phone_side(&eph, &impostor, &public_of(&host), &host_eph);
        assert!(mac.open(&phone.seal(b"hi")).is_none());
    }

    #[test]
    fn a_low_order_point_is_refused() {
        assert!(host_handshake(&random32(), &random32(), &[0u8; 32]).is_none());
    }

    #[test]
    fn the_pairing_proof_is_bound_to_the_key() {
        let secret = random32();
        let dp = random32();
        let proof = pair_proof(&secret, "dev1", &dp);
        assert!(pair_proof_ok(&secret, "dev1", &dp, &proof));
        assert!(!pair_proof_ok(&secret, "dev1", &random32(), &proof));
        assert!(!pair_proof_ok(&secret, "dev2", &dp, &proof));
        assert!(!pair_proof_ok(&random32(), "dev1", &dp, &proof));
    }

    /// Fixed inputs; every expected value below was computed by the phone's
    /// `crypto.ts` (@noble), so this pins the two implementations together.
    #[test]
    fn matches_the_phone() {
        let k = |b: u8| [b; 32];
        let (host, dev, eph, host_eph_secret, secret) = (k(1), k(2), k(3), k(4), k(5));
        let (host_eph, mut mac) =
            host_handshake_with(&host_eph_secret, &host, &public_of(&dev), &public_of(&eph)).unwrap();
        assert_eq!(hex(&public_of(&host)), PHONE_HOST_PUB);
        assert_eq!(hex(&host_eph), PHONE_HOST_EPH);
        assert_eq!(hex(&pair_proof(&secret, "dev1", &public_of(&dev))), PHONE_PROOF);
        assert_eq!(mac.seal(b"hello"), PHONE_H2P_HELLO);
        assert_eq!(mac.open(PHONE_P2H_HI).unwrap(), b"hi");
    }

    const PHONE_HOST_PUB: &str = "a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a209";
    const PHONE_HOST_EPH: &str = "ac01b2209e86354fb853237b5de0f4fab13c7fcbf433a61c019369617fecf10b";
    const PHONE_PROOF: &str = "58de6e47c7872bd7e4c72304c85608cd47f731d8ad61fdd7ebf2c8b8fec05200";
    const PHONE_H2P_HELLO: &str = "gsfcFhjUtWi0jwHZ_llAV_9KHuKq";
    const PHONE_P2H_HI: &str = "cN0SgMW18jWkyDXoiQFaEw9o";
}
