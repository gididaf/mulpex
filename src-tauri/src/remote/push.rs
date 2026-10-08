//! Web Push, sent by the Mac itself straight to a paired phone's push service
//! (Google's for Chrome, Apple's for Safari) — not through the relay, which
//! therefore never sees a notification.
//!
//! Two standards, both implemented here because they are small:
//! - **VAPID** (RFC 8292): the Mac signs a short JWT with its own P-256 key, so
//!   the push service accepts the message as coming from the app the phone
//!   subscribed to. The public half is what the phone subscribes with.
//! - **Message encryption** (RFC 8291, `aes128gcm`): the payload is encrypted to
//!   the browser's own key, so the push service carries it without being able
//!   to read it.
//!
//! The payload is only ever "claude#N in <project> needs you" and its task
//! line — nothing from the conversation.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::crypto::{b64, random32, unb64};

/// A browser's push subscription, as `PushSubscription.toJSON()` gives it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubKeys,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubKeys {
    /// The browser's P-256 public key, uncompressed (65 bytes), base64url.
    pub p256dh: String,
    /// 16 random bytes, base64url.
    pub auth: String,
}

/// The Mac's VAPID key from its stored scalar; a fresh one if that is missing
/// or bad.
pub fn vapid_key(stored: &str) -> (SecretKey, bool) {
    if let Some(k) = unb64(stored).and_then(|b| SecretKey::from_slice(&b).ok()) {
        return (k, false);
    }
    loop {
        if let Ok(k) = SecretKey::from_slice(&random32()) {
            return (k, true);
        }
    }
}

pub fn vapid_secret_b64(k: &SecretKey) -> String {
    b64(&k.to_bytes())
}

/// What the phone subscribes with (`applicationServerKey`): the uncompressed
/// public point, base64url.
pub fn vapid_public_b64(k: &SecretKey) -> String {
    b64(k.public_key().to_encoded_point(false).as_bytes())
}

/// `scheme://host[:port]` of a push endpoint — the JWT's audience.
fn origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(format!("{scheme}://{host}"))
}

/// The `Authorization` header value for one push.
pub fn vapid_header(key: &SecretKey, endpoint: &str, now: u64) -> Option<String> {
    let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = serde_json::json!({
        "aud": origin(endpoint)?,
        "exp": now + 12 * 3600,
        "sub": "mailto:remote@mulpex.app",
    });
    let unsigned = format!("{header}.{}", b64(claims.to_string().as_bytes()));
    let sig: Signature = SigningKey::from(key).sign(unsigned.as_bytes());
    Some(format!("vapid t={unsigned}.{}, k={}", b64(&sig.to_bytes()), vapid_public_b64(key)))
}

/// RFC 8291 `aes128gcm`: the whole request body. `eph` and `salt` are random
/// in real use and fixed in tests.
pub fn encrypt(sub: &Subscription, payload: &[u8]) -> Option<Vec<u8>> {
    let eph = loop {
        if let Ok(k) = SecretKey::from_slice(&random32()) {
            break k;
        }
    };
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&random32()[..16]);
    encrypt_with(sub, payload, &eph, &salt)
}

fn encrypt_with(sub: &Subscription, payload: &[u8], eph: &SecretKey, salt: &[u8; 16]) -> Option<Vec<u8>> {
    let ua_pub_bytes = unb64(&sub.keys.p256dh)?;
    let auth = unb64(&sub.keys.auth)?;
    let ua_pub = PublicKey::from_sec1_bytes(&ua_pub_bytes).ok()?;
    let as_pub = eph.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(eph.to_nonzero_scalar(), ua_pub.as_affine());

    // IKM = HKDF(auth, ecdh, "WebPush: info\0" ‖ ua_public ‖ as_public)
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(&ua_pub_bytes);
    info.extend_from_slice(as_pub.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes()).expand(&info, &mut ikm).ok()?;

    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).ok()?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).ok()?;

    // One record: the payload, then the last-record delimiter.
    let mut plain = payload.to_vec();
    plain.push(2);
    let ct = Aes128Gcm::new_from_slice(&cek).ok()?.encrypt(Nonce::from_slice(&nonce), plain.as_slice()).ok()?;

    let mut body = Vec::with_capacity(86 + ct.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_pub.as_bytes().len() as u8);
    body.extend_from_slice(as_pub.as_bytes());
    body.extend_from_slice(&ct);
    Some(body)
}

/// What became of one push.
#[derive(Debug, PartialEq)]
pub enum Sent {
    Ok,
    /// The subscription is dead (unsubscribed, or the app was removed): forget it.
    Gone,
    Failed(String),
}

/// Send one notification. Blocking; call it off the connection thread.
pub fn send(key: &SecretKey, sub: &Subscription, payload: &[u8]) -> Sent {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (Some(auth), Some(body)) = (vapid_header(key, &sub.endpoint, now), encrypt(sub, payload)) else {
        return Sent::Failed("bad subscription".into());
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into();
    match agent
        .post(&sub.endpoint)
        .header("Authorization", &auth)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", "600")
        .header("Urgency", "high")
        .send(&body[..])
    {
        Ok(resp) => match resp.status().as_u16() {
            200..=299 => Sent::Ok,
            404 | 410 => Sent::Gone,
            s => Sent::Failed(format!("push service answered {s}")),
        },
        Err(e) => Sent::Failed(e.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    pub(crate) fn browser() -> (SecretKey, Subscription) {
        let ua = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let sub = Subscription {
            endpoint: "https://push.example.net/send/abc".into(),
            keys: SubKeys {
                p256dh: b64(ua.public_key().to_encoded_point(false).as_bytes()),
                auth: b64(&[9u8; 16]),
            },
        };
        (ua, sub)
    }

    #[test]
    fn the_vapid_jwt_verifies_against_the_key_the_phone_subscribes_with() {
        let (key, fresh) = vapid_key("");
        assert!(fresh);
        assert_eq!(vapid_key(&vapid_secret_b64(&key)).0.to_bytes(), key.to_bytes());
        let h = vapid_header(&key, "https://fcm.googleapis.com/fcm/send/xyz", 1_000).unwrap();
        let t = h.strip_prefix("vapid t=").unwrap().split(", k=").next().unwrap();
        let (unsigned, sig) = t.rsplit_once('.').unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&unb64(unsigned.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        let k = unb64(h.split(", k=").nth(1).unwrap()).unwrap();
        let vk = VerifyingKey::from_sec1_bytes(&k).unwrap();
        let sig = Signature::from_slice(&unb64(sig).unwrap()).unwrap();
        assert!(vk.verify(unsigned.as_bytes(), &sig).is_ok());
    }

    #[test]
    fn the_body_has_the_rfc_8291_header() {
        let (_, sub) = browser();
        let body = encrypt(&sub, b"hi").unwrap();
        assert_eq!(&body[16..20], &4096u32.to_be_bytes());
        assert_eq!(body[20], 65);
        assert_eq!(body[21], 0x04, "an uncompressed point");
        // 16 salt + 4 rs + 1 idlen + 65 key + (2 + 1 delimiter + 16 tag)
        assert_eq!(body.len(), 86 + 19);
    }

    /// Prints a fixed vector for an independent decryptor (`http_ece`, in the
    /// phase's verification script): `cargo test -p mulpex --lib push_vector
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn push_vector() {
        let (ua, sub) = browser();
        let eph = SecretKey::from_slice(&[5u8; 32]).unwrap();
        let body = encrypt_with(&sub, "claude#3 needs you · שלום".as_bytes(), &eph, &[1u8; 16]).unwrap();
        println!("UA_PRIVATE {}", b64(&ua.to_bytes()));
        println!("UA_PUBLIC {}", sub.keys.p256dh);
        println!("AUTH {}", sub.keys.auth);
        println!("BODY {}", b64(&body));
    }
}
