//! The app ⇄ daemon wire: length-prefixed frames over a unix socket.
//!
//! ```text
//! u32 BE length (of everything after it) | u8 kind | payload
//! ```
//!
//! - `JSON` (0): a control message. Requests carry `"op"` and an `"id"` the
//!   reply echoes; events carry `"ev"`.
//! - `OUTPUT` (1, daemon → app): `u32 sid | u64 offset | bytes` — PTY output at
//!   that absolute offset in the session's stream since spawn.
//! - `INPUT` (2, app → daemon): `u32 sid | bytes` — keystrokes.
//!
//! **The frozen core.** `hello`, `list` and `shutdown` must keep their shape in
//! every future protocol: they are how an app that cannot talk to an older (or
//! newer) daemon still learns what it holds and shuts it down cleanly before
//! starting its own. Everything else may change with a `PROTO` bump.

use std::io::{self, Read, Write};

/// The protocol this build speaks. Bump it when a change would confuse the
/// other side; raise `PROTO_MIN` only when this app can no longer drive an
/// older daemon. A daemon fix that keeps the wire is NOT a bump — it ships
/// silently and takes over after the next real quit.
pub const PROTO: u32 = 1;
/// The oldest daemon protocol this app can still drive.
pub const PROTO_MIN: u32 = 1;

pub const JSON: u8 = 0;
pub const OUTPUT: u8 = 1;
pub const INPUT: u8 = 2;

/// A frame larger than this is a corrupt stream, not a message.
const MAX_FRAME: usize = 64 * 1024 * 1024;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode(kind, payload))
}

/// One frame as bytes, so a writer thread can send it in a single write.
pub fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

pub fn json_frame(v: &serde_json::Value) -> Vec<u8> {
    encode(JSON, v.to_string().as_bytes())
}

pub fn output_frame(sid: u32, offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + bytes.len());
    p.extend_from_slice(&sid.to_be_bytes());
    p.extend_from_slice(&offset.to_be_bytes());
    p.extend_from_slice(bytes);
    encode(OUTPUT, &p)
}

pub fn input_frame(sid: u32, bytes: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + bytes.len());
    p.extend_from_slice(&sid.to_be_bytes());
    p.extend_from_slice(bytes);
    encode(INPUT, &p)
}

/// Read one frame: `(kind, payload)`. `Ok(None)` on a clean EOF between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let kind = buf[0];
    buf.remove(0);
    Ok(Some((kind, buf)))
}

/// Split an `OUTPUT` payload into `(sid, offset, bytes)`.
pub fn parse_output(p: &[u8]) -> Option<(u32, u64, &[u8])> {
    if p.len() < 12 {
        return None;
    }
    let sid = u32::from_be_bytes(p[0..4].try_into().ok()?);
    let off = u64::from_be_bytes(p[4..12].try_into().ok()?);
    Some((sid, off, &p[12..]))
}

/// Split an `INPUT` payload into `(sid, bytes)`.
pub fn parse_input(p: &[u8]) -> Option<(u32, &[u8])> {
    if p.len() < 4 {
        return None;
    }
    Some((u32::from_be_bytes(p[0..4].try_into().ok()?), &p[4..]))
}

/// Whether an app that speaks `PROTO_MIN..=PROTO` can drive a daemon speaking
/// `daemon`.
pub fn compatible(daemon: u32) -> bool {
    (PROTO_MIN..=PROTO).contains(&daemon)
}

/// Whether an update whose release says its app drives daemons
/// `ptyd_min..=ptyd` can keep the running one (speaking `running`) — i.e.
/// whether the claudes survive installing it.
pub fn update_survives(running: u32, ptyd_min: u32, ptyd: u32) -> bool {
    ptyd_min <= running && running <= ptyd
}

/// The daemon's socket, lock and log, under the Mulpex home it serves.
pub fn socket_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("ptyd.sock")
}

pub fn lock_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("ptyd.lock")
}

pub fn dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join("ptyd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        buf.extend(json_frame(&serde_json::json!({"op": "list", "id": 3})));
        buf.extend(output_frame(7, 1 << 40, b"hello"));
        buf.extend(input_frame(9, b"\x1b\r"));
        let mut r = &buf[..];
        let (k, p) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(k, JSON);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&p).unwrap()["op"], "list");
        let (k, p) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(k, OUTPUT);
        assert_eq!(parse_output(&p).unwrap(), (7, 1 << 40, &b"hello"[..]));
        let (k, p) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(k, INPUT);
        assert_eq!(parse_input(&p).unwrap(), (9, &b"\x1b\r"[..]));
        assert!(read_frame(&mut r).unwrap().is_none(), "clean EOF");
    }

    /// The frozen core's bytes, as an app of any version sends them. Changing
    /// this fixture means an old app can no longer shut a new daemon down.
    #[test]
    fn the_frozen_core_is_frozen() {
        let hello = json_frame(&serde_json::json!({"op": "hello", "id": 1, "proto": 1}));
        let mut r = &hello[..];
        let (k, p) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(k, 0);
        let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
        assert_eq!((v["op"].as_str(), v["id"].as_u64()), (Some("hello"), Some(1)));
        assert_eq!(&hello[..5], &[0, 0, 0, (p.len() + 1) as u8, 0]);
    }

    #[test]
    fn survival_needs_the_running_daemon_in_the_new_apps_range() {
        assert!(update_survives(1, 1, 1));
        assert!(update_survives(1, 1, 2));
        assert!(!update_survives(1, 2, 2), "the new app dropped proto 1");
        assert!(!update_survives(3, 1, 2), "the running daemon is newer than the new app knows");
        assert!(compatible(PROTO) && compatible(PROTO_MIN));
        assert!(!compatible(PROTO + 1));
    }

    #[test]
    fn a_garbage_length_is_an_error_not_an_allocation() {
        let mut r = &[0xff, 0xff, 0xff, 0xff, 0][..];
        assert!(read_frame(&mut r).is_err());
        let mut r = &[0, 0, 0, 0][..];
        assert!(read_frame(&mut r).is_err());
    }
}
