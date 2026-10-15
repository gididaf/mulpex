//! Remote Hosts, the host side: another Mulpex — a *remote desktop* — mirroring
//! and driving this one, over the same relay, pairing and sealed frames a phone
//! uses (`mod.rs`). The client side is `client.rs`; the design is in
//! `docs/remote-hosts.md`.
//!
//! A phone gets a *view* built for a phone. A remote desktop gets this Mulpex
//! itself:
//! - **commands** (`rpc`): the same `commands.rs` functions this window's
//!   frontend calls, behind the allowlist in `rpc.rs`;
//! - **events** (`ev`): every event this window's frontend hears that a remote
//!   frontend needs too, forwarded as emitted (`EVENT_NAMES`);
//! - **terminal streams** (`sub` → `snap`, then `o`/`r`): each session's exact
//!   screen, then every byte and resize after it, so the remote xterm is a
//!   mirror of the one here;
//! - **keystrokes** (`in`), straight to the PTY, like this window's xterm;
//! - **the geometry lease** (`geo`): the remote window's size drives every PTY
//!   while it holds the lease; this window follows, letterboxed, until "Take
//!   back".
//!
//! Everything for one remote desktop travels on its one sealed channel, so the
//! order things happened in here is the order they arrive in there — which is
//! what makes "snapshot, then the bytes after it" exact.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use tauri::{Emitter, Listener, Manager};

use super::{b64, inner, Server, Ws};
use crate::pty::{DesktopTap, TapMsg};
use crate::snapshot::ProjectHandle;

/// Bumped when a remote desktop and this host could no longer understand each
/// other at all. Separately, both sides must run the **same app version**:
/// every tunnelled command's signature is part of this protocol.
pub const PROTO: u64 = 1;

/// Events a remote frontend needs to hear exactly as this one does. Handles in
/// them are this host's; the client lifts them into its own namespace.
pub const EVENT_NAMES: &[&str] = &[
    "hub-update",
    "sessions-changed",
    "session-exited",
    "session-restarted",
    "projects-changed",
    "save-progress",
    "explain-progress",
    "import-update",
    "pins-changed",
    "todos-changed",
];

/// The most terminal output in one frame.
const CHUNK: usize = 64 * 1024;
/// Stop streaming to a remote desktop this far ahead of what it has confirmed
/// receiving (`ack`): the relay buffers without limit, and so would we.
const MAX_UNACKED: u64 = 4 * 1024 * 1024;
/// A session whose unsent backlog grows past this is re-snapshotted instead of
/// caught up byte by byte — memory stays bounded and the result is the same.
const MAX_BACKLOG: usize = 8 * 1024 * 1024;
/// How long this window's xterm gets to serialize a screen before the raw ring
/// is sent instead (a minimized window's webview can be throttled).
const SNAPSHOT_WAIT: Duration = Duration::from_secs(3);
/// The most one remote desktop may upload in one go.
const MAX_UPLOAD: u64 = 100 * 1024 * 1024;
/// Uploads are kept this long, then pruned.
const UPLOAD_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// A random id for this run of the app. A remote desktop that sees it change
/// knows every project handle it holds may now name something else.
pub fn boot_id() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| b64(&super::crypto::random32()[..12]))
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = if ok { String::from_utf8_lossy(&buf[..end]).into_owned() } else { String::new() };
    let name = name.trim_end_matches(".local").to_string();
    if name.is_empty() { "Mulpex".into() } else { name }
}

/// One connected remote desktop.
pub(super) struct Desk {
    pub name: String,
    subs: HashMap<(ProjectHandle, usize), Sub>,
    sent_geo: Option<(u16, u16, Option<String>)>,
    ev_seq: u64,
    unacked: u64,
    uploads: HashMap<String, Upload>,
}

enum Sub {
    /// Waiting for this window's xterm to serialize the screen (`rid`).
    Pending { rid: String, tap: DesktopTap, since: Instant },
    Live { rx: Receiver<TapMsg>, skip: u64, backlog: VecDeque<TapMsg>, bytes: usize },
}

struct Upload {
    dir: PathBuf,
    roots: Vec<String>,
    bytes: u64,
    open: Option<(String, std::fs::File)>,
}

impl Desk {
    pub fn new(name: String) -> Self {
        Desk {
            name,
            subs: HashMap::new(),
            sent_geo: None,
            ev_seq: EVENTS.lock().unwrap().0,
            unacked: 0,
            uploads: HashMap::new(),
        }
    }
}

// ---- events this window hears, kept for the remote desktops ----

/// (last seq, the recent events as ready-to-seal JSON).
static EVENTS: Mutex<(u64, VecDeque<(u64, String)>)> = Mutex::new((0, VecDeque::new()));
/// Only recorded while a remote desktop is connected.
static DESKTOPS: AtomicBool = AtomicBool::new(false);

/// Listen for every forwarded event, once, at startup.
pub fn init_events(app: &tauri::AppHandle) {
    for name in EVENT_NAMES {
        app.listen_any(*name, move |e| {
            if !DESKTOPS.load(Ordering::Relaxed) {
                return;
            }
            let payload: Value = serde_json::from_str(e.payload()).unwrap_or(Value::Null);
            let msg = json!({"t": "ev", "name": name, "payload": payload}).to_string();
            let mut ev = EVENTS.lock().unwrap();
            ev.0 += 1;
            let seq = ev.0;
            ev.1.push_back((seq, msg));
            while ev.1.len() > 4000 {
                ev.1.pop_front();
            }
        });
    }
}

// ---- this window's xterm snapshots ----

pub(super) struct SnapReply {
    data: Option<String>,
    offset: u64,
    cols: u16,
    rows: u16,
}

static SNAP_REPLIES: Mutex<Option<HashMap<String, SnapReply>>> = Mutex::new(None);

pub fn snapshot_reply(rid: &str, data: Option<String>, offset: u64, cols: u16, rows: u16) {
    SNAP_REPLIES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(rid.to_string(), SnapReply { data, offset, cols, rows });
}

fn take_snap(rid: &str) -> Option<SnapReply> {
    SNAP_REPLIES.lock().unwrap().as_mut()?.remove(rid)
}

// ---- helpers that need the app ----

fn app() -> Option<tauri::AppHandle> {
    inner().lock().unwrap().app.clone()
}

fn tap_session(h: ProjectHandle, id: usize) -> Option<DesktopTap> {
    #[cfg(test)]
    if let Some(t) = super::tests::fake_desktop_tap(h, id) {
        return Some(t);
    }
    let app = app()?;
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    let s = ws.project(h)?.sessions.iter().find(|s| s.id == id)?;
    Some(s.desktop_tap())
}

fn geometry_and_owner() -> ((u16, u16), Option<(String, String)>) {
    #[cfg(test)]
    if let Some(g) = super::tests::FAKE_GEO.lock().unwrap().clone() {
        return g;
    }
    let Some(app) = app() else { return ((0, 0), None) };
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    (ws.geometry, ws.geometry_owner.clone())
}

/// A remote desktop claims the geometry: every PTY goes to its size.
fn claim_geometry(device: &str, name: &str, cols: u16, rows: u16) {
    if cols < 2 || rows < 2 {
        return;
    }
    #[cfg(test)]
    {
        let mut g = super::tests::FAKE_GEO.lock().unwrap();
        if g.is_some() {
            *g = Some(((cols, rows), Some((device.into(), name.into()))));
            return;
        }
    }
    let Some(app) = app() else { return };
    let geo = {
        let state = app.state::<crate::state::AppState>();
        let mut ws = state.ws.lock().unwrap();
        ws.geometry_owner = Some((device.to_string(), name.to_string()));
        ws.resize_all(cols, rows);
        ws.geometry
    };
    crate::commands::emit_geometry(&app, geo, Some(name.to_string()));
}

/// The remote desktop holding the lease went away: this window drives again.
fn release_geometry(device: &str) {
    #[cfg(test)]
    {
        let mut g = super::tests::FAKE_GEO.lock().unwrap();
        if let Some((geo, Some((d, _)))) = g.clone() {
            if d == device {
                *g = Some((geo, None));
            }
            return;
        }
    }
    let Some(app) = app() else { return };
    let geo = {
        let state = app.state::<crate::state::AppState>();
        let mut ws = state.ws.lock().unwrap();
        if ws.geometry_owner.as_ref().map(|o| o.0.as_str()) != Some(device) {
            return;
        }
        ws.geometry_owner = None;
        ws.geometry
    };
    crate::commands::emit_geometry(&app, geo, None);
}

fn write_input(h: ProjectHandle, id: usize, bytes: &[u8]) {
    #[cfg(test)]
    if super::tests::fake_input(h, id, bytes) {
        return;
    }
    let Some(app) = app() else { return };
    let state = app.state::<crate::state::AppState>();
    let mut ws = state.ws.lock().unwrap();
    if let Some(s) = ws.project_mut(h).and_then(|c| c.session_mut(id)) {
        s.send(bytes);
    }
}

/// Nudge session `id` into a full repaint: one row smaller, then back. Both
/// resizes are markers in its stream, so the mirror follows them in order.
fn repaint(h: ProjectHandle, id: usize) {
    let Some(app) = app() else { return };
    std::thread::spawn(move || {
        let state = app.state::<crate::state::AppState>();
        let (cols, rows) = state.ws.lock().unwrap().geometry;
        if rows < 3 {
            return;
        }
        let resize = |r: u16| {
            if let Some(s) = state.ws.lock().unwrap().project_mut(h).and_then(|c| c.session_mut(id)) {
                s.resize(r, cols);
            }
        };
        resize(rows - 1);
        std::thread::sleep(Duration::from_millis(150));
        resize(rows);
    });
}

fn session_exists(h: ProjectHandle, id: usize) -> bool {
    #[cfg(test)]
    if super::tests::FAKE_GEO.lock().unwrap().is_some() {
        return false;
    }
    let Some(app) = app() else { return false };
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    ws.project(h).is_some_and(|c| c.sessions.iter().any(|s| s.id == id))
}

// ---- composing a stream ----

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

/// One terminal frame: bytes (`o`) or a resize (`r`) of session `id` in `h`.
/// Bytes over 2 KB go deflated (`z`), which roughly halves a screenful.
fn frame(h: ProjectHandle, id: usize, msg: &TapMsg) -> String {
    match msg {
        TapMsg::Bytes(b) if b.len() > 2048 => {
            json!({"t": "o", "h": h, "id": id, "z": b64(&deflate(b))}).to_string()
        }
        TapMsg::Bytes(b) => json!({"t": "o", "h": h, "id": id, "d": b64(b)}).to_string(),
        TapMsg::Resize(c, r) => json!({"t": "r", "h": h, "id": id, "cols": c, "rows": r}).to_string(),
    }
}

/// `ring` (starting at absolute offset `start`) from offset `from` on, as
/// bytes and resizes in stream order. A resize recorded at exactly `from`
/// is included: resizing to a size already held is a no-op.
fn ring_from(tap: &DesktopTap, from: u64) -> Vec<TapMsg> {
    let mut out = Vec::new();
    let mut at = from.max(tap.start);
    for &(off, c, r) in tap.resizes.iter().filter(|r| r.0 >= from) {
        if off > at {
            let a = (at - tap.start) as usize;
            let b = (off - tap.start) as usize;
            out.push(TapMsg::Bytes(tap.ring[a..b].to_vec()));
            at = off;
        }
        out.push(TapMsg::Resize(c, r));
    }
    let a = (at - tap.start) as usize;
    if a < tap.ring.len() {
        out.push(TapMsg::Bytes(tap.ring[a..].to_vec()));
    }
    out
}

/// The opening of a session's stream, from a tap and (if this window's xterm
/// answered) its serialized screen at stream offset `q`:
/// - the screen, then whatever the ring holds past `q`, when it does;
/// - the screen, then the live stream minus the `q - total` bytes it already
///   shows, when the xterm got ahead of the tap;
/// - the ring replayed from the PTY's spawn size, when it still holds
///   everything since spawn — exact without any xterm;
/// - otherwise the ring alone, marked `partial` (it may show debris).
///
/// Returns (the snapshot frame, the frames after it, bytes to skip live).
fn compose(
    h: ProjectHandle,
    id: usize,
    tap: &DesktopTap,
    snap: Option<SnapReply>,
    geo: (u16, u16),
) -> (String, Vec<TapMsg>, u64) {
    let snap_frame = |cols: u16, rows: u16, data: &[u8], partial: bool| {
        json!({"t": "snap", "h": h, "id": id, "cols": cols, "rows": rows,
               "z": b64(&deflate(data)), "partial": partial})
        .to_string()
    };
    if let Some(SnapReply { data: Some(data), offset: q, cols, rows }) = snap {
        if q >= tap.start {
            let first = snap_frame(cols, rows, data.as_bytes(), false);
            return if q <= tap.total {
                (first, ring_from(tap, q), 0)
            } else {
                (first, Vec::new(), q - tap.total)
            };
        }
    }
    if tap.start == 0 {
        return (snap_frame(tap.init.0, tap.init.1, b"", false), ring_from(tap, 0), 0);
    }
    (snap_frame(geo.0, geo.1, b"", true), ring_from(tap, tap.start), 0)
}

/// Strip an uploaded path to safe relative components: no root, no `..`, no
/// control characters. `None` when nothing is left.
fn safe_rel(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        if let Component::Normal(s) = c {
            let s: String = s.to_string_lossy().chars().filter(|c| !c.is_control()).collect();
            let s = s.trim();
            if !s.is_empty() && s != "." && s != ".." {
                out.push(s);
            }
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn uploads_dir() -> PathBuf {
    super::remote_dir().join("uploads")
}

/// Forget uploads older than `UPLOAD_TTL`.
fn prune_uploads() {
    let Ok(entries) = std::fs::read_dir(uploads_dir()) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > UPLOAD_TTL);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

impl Server {
    /// Mark whether any remote desktop is connected (events are recorded only
    /// then), and how long a read may wait: a desktop's keystrokes and echoes
    /// must not sit in a 100 ms tick.
    pub(super) fn desktops_online(&self) -> bool {
        let any = self.sessions.values().any(|s| s.desk.is_some());
        DESKTOPS.store(any, Ordering::Relaxed);
        any
    }

    /// A `hello` from a remote desktop: same version, or refused with ours.
    pub(super) fn desk_hello(&mut self, ws: &mut Ws, cid: u64, req: &Value) -> Result<(), String> {
        let mine = env!("CARGO_PKG_VERSION");
        let proto = req.get("proto").and_then(Value::as_u64);
        let theirs = req.get("app").and_then(Value::as_str).unwrap_or("");
        if proto != Some(PROTO) || theirs != mine {
            let msg = json!({"t": "refused", "why": "version", "app": mine, "proto": PROTO});
            return self.send_sealed(ws, cid, &msg.to_string());
        }
        let name: String = req.get("name").and_then(Value::as_str).unwrap_or("Mac").chars().take(60).collect();
        let Some(s) = self.sessions.get_mut(&cid) else { return Ok(()) };
        s.desk = Some(Desk::new(name));
        let device = s.device.clone();
        self.desktops_online();
        let ((cols, rows), owner) = geometry_and_owner();
        let msg = json!({
            "t": "welcome", "proto": PROTO, "app": mine, "boot": boot_id(), "name": hostname(),
            "device": device, "geo": {"cols": cols, "rows": rows, "owner": owner.map(|o| o.1)},
        });
        self.send_sealed(ws, cid, &msg.to_string())
    }

    /// The session left (or was revoked): give back the geometry if it held it.
    pub(super) fn desk_gone(&mut self, device: &str) {
        let still = self.sessions.values().any(|s| s.desk.is_some() && s.device == device);
        if !still {
            release_geometry(device);
        }
        self.desktops_online();
    }

    /// One request from a remote desktop. `false`: not a desktop message.
    pub(super) fn desk_request(&mut self, ws: &mut Ws, cid: u64, t: &str, req: &Value) -> Result<bool, String> {
        let Some(s) = self.sessions.get_mut(&cid) else { return Ok(true) };
        let device = s.device.clone();
        let Some(desk) = s.desk.as_mut() else { return Ok(false) };
        let h = req.get("h").and_then(Value::as_u64);
        let id = req.get("id").and_then(Value::as_u64).map(|i| i as usize);
        match t {
            "rpc" => {
                let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect::<String>();
                let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
                let args = req.get("args").cloned().unwrap_or(Value::Null);
                let epoch = self.epoch;
                std::thread::spawn(move || {
                    let res = match app() {
                        _ if !super::rpc::ALLOWED.contains(&cmd.as_str()) => {
                            Err(format!("`{cmd}` can't be run from a remote Mulpex"))
                        }
                        Some(app) => super::rpc::dispatch(&app, &cmd, &args),
                        None => Err("Mulpex isn't ready".into()),
                    };
                    let msg = match res {
                        Ok(v) => json!({"t": "res", "rid": rid, "ok": true, "value": v}),
                        Err(e) => json!({"t": "res", "rid": rid, "ok": false, "error": e}),
                    };
                    inner().lock().unwrap().outbox.push((epoch, cid, msg.to_string()));
                });
            }
            "in" => {
                let data = req.get("d").and_then(Value::as_str).and_then(super::crypto::unb64);
                if let (Some(h), Some(id), Some(data)) = (h, id, data) {
                    write_input(h, id, &data);
                }
            }
            "sub" => {
                if let (Some(h), Some(id)) = (h, id) {
                    desk.subs.remove(&(h, id));
                    if let Some(msg) = subscribe(desk, h, id) {
                        return self.send_sealed(ws, cid, &msg).map(|_| true);
                    }
                }
            }
            "unsub" => {
                if let (Some(h), Some(id)) = (h, id) {
                    desk.subs.remove(&(h, id));
                }
            }
            "geo" => {
                let cols = req.get("cols").and_then(Value::as_u64).unwrap_or(0) as u16;
                let rows = req.get("rows").and_then(Value::as_u64).unwrap_or(0) as u16;
                let name = desk.name.clone();
                claim_geometry(&device, &name, cols, rows);
            }
            "ack" => {
                let n = req.get("n").and_then(Value::as_u64).unwrap_or(0);
                desk.unacked = desk.unacked.saturating_sub(n);
            }
            "up" => {
                let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").to_string();
                if let Err(e) = upload_chunk(desk, req) {
                    let uid = req.get("uid").and_then(Value::as_str).unwrap_or("");
                    if let Some(u) = desk.uploads.remove(uid) {
                        let _ = std::fs::remove_dir_all(&u.dir);
                    }
                    if !rid.is_empty() {
                        let msg = json!({"t": "res", "rid": rid, "ok": false, "error": e});
                        return self.send_sealed(ws, cid, &msg.to_string()).map(|_| true);
                    }
                }
            }
            "up-done" => {
                let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").to_string();
                let uid = req.get("uid").and_then(Value::as_str).unwrap_or("");
                let msg = match desk.uploads.remove(uid) {
                    Some(u) => {
                        let paths: Vec<String> =
                            u.roots.iter().map(|r| u.dir.join(r).display().to_string()).collect();
                        json!({"t": "res", "rid": rid, "ok": true, "value": paths})
                    }
                    None => json!({"t": "res", "rid": rid, "ok": false, "error": "nothing was uploaded"}),
                };
                return self.send_sealed(ws, cid, &msg.to_string()).map(|_| true);
            }
            // An action this window's frontend owns (⌘⇧K's state machine):
            // asked for like a phone's, answered through `remote_reply`.
            "fa" => {
                let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect::<String>();
                let action = req.get("action").and_then(Value::as_str).unwrap_or("");
                if action != "compact" {
                    return Ok(true);
                }
                let epoch = self.epoch;
                let app = {
                    let mut g = inner().lock().unwrap();
                    g.pending.insert(rid.clone(), (epoch, cid));
                    g.app.clone()
                };
                if let Some(app) = app {
                    let _ = app.emit(
                        "remote-action",
                        json!({"action": "compact", "handle": h, "id": id, "kind": null, "rid": rid}),
                    );
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Each pass: forwarded events, the geometry when it changed, snapshots
    /// that came back (or timed out), and every subscribed session's output —
    /// as far as the remote desktop's acknowledgements allow.
    pub(super) fn pump_desktops(&mut self, ws: &mut Ws) -> Result<(), String> {
        let cids: Vec<u64> = self.sessions.iter().filter(|(_, s)| s.desk.is_some()).map(|(c, _)| *c).collect();
        if cids.is_empty() {
            return Ok(());
        }
        // Only what some remote desktop hasn't been sent yet; what all of them
        // have is dropped from the queue.
        let seen = |s: &Server| -> Vec<u64> {
            s.sessions.values().filter_map(|x| x.desk.as_ref().map(|d| d.ev_seq)).collect()
        };
        let low = seen(self).into_iter().min().unwrap_or(0);
        let events: Vec<(u64, String)> = {
            let mut ev = EVENTS.lock().unwrap();
            while ev.1.front().is_some_and(|(s, _)| *s <= low) {
                ev.1.pop_front();
            }
            ev.1.iter().cloned().collect()
        };
        let (geo, owner) = geometry_and_owner();
        for cid in cids {
            let mut out: Vec<String> = Vec::new();
            {
                let Some(s) = self.sessions.get_mut(&cid) else { continue };
                let device = s.device.clone();
                let Some(desk) = s.desk.as_mut() else { continue };
                for (seq, msg) in &events {
                    if *seq > desk.ev_seq {
                        out.push(msg.clone());
                        desk.ev_seq = *seq;
                    }
                }
                let g = (geo.0, geo.1, owner.as_ref().map(|o| o.1.clone()));
                if desk.sent_geo.as_ref() != Some(&g) {
                    let mine = owner.as_ref().is_some_and(|o| o.0 == device);
                    out.push(
                        json!({"t": "geo", "cols": g.0, "rows": g.1, "owner": g.2, "mine": mine}).to_string(),
                    );
                    desk.sent_geo = Some(g);
                }
                pump_subs(desk, geo, &mut out);
            }
            for msg in out {
                if let Some(desk) = self.sessions.get_mut(&cid).and_then(|s| s.desk.as_mut()) {
                    desk.unacked += msg.len() as u64;
                }
                self.send_sealed(ws, cid, &msg)?;
            }
        }
        Ok(())
    }
}

/// Start mirroring session `id`: a raw replay when the ring still holds it
/// all, else ask this window's xterm for its screen. `Some(frame)` to send now
/// — `gone` when there is no such session.
fn subscribe(desk: &mut Desk, h: ProjectHandle, id: usize) -> Option<String> {
    let Some(tap) = tap_session(h, id) else {
        return Some(json!({"t": "gone", "h": h, "id": id}).to_string());
    };
    let rid = b64(&super::crypto::random32()[..12]);
    if tap.start > 0 {
        if let Some(app) = app() {
            let _ = app.emit("host-serialize", json!({"rid": rid, "handle": h, "id": id}));
        }
    }
    // A tap whose ring holds everything since spawn needs no snapshot: the
    // next pass replays it (`compose`).
    desk.subs.insert((h, id), Sub::Pending { rid, tap, since: Instant::now() });
    None
}

fn pump_subs(desk: &mut Desk, geo: (u16, u16), out: &mut Vec<String>) {
    let keys: Vec<(ProjectHandle, usize)> = desk.subs.keys().copied().collect();
    for (h, id) in keys {
        // A snapshot that came back, or the wait for one that ran out.
        let promote = match desk.subs.get(&(h, id)) {
            Some(Sub::Pending { rid, tap, since }) => {
                let reply = take_snap(rid);
                if reply.is_none() && tap.start > 0 && since.elapsed() < SNAPSHOT_WAIT {
                    continue;
                }
                Some(reply)
            }
            Some(Sub::Live { .. }) => None,
            None => continue,
        };
        if let Some(reply) = promote {
            let Some(Sub::Pending { tap, .. }) = desk.subs.remove(&(h, id)) else { continue };
            // Which way a mirror started (`MULPEX_DEBUG_REMOTE`): the ring alone,
            // or this window's screen — and how big it was.
            if std::env::var_os("MULPEX_DEBUG_REMOTE").is_some() {
                let r = reply.as_ref().map(|r| (r.data.as_ref().map(|d| d.len()), r.offset, r.cols, r.rows));
                eprintln!("mulpex: mirror h={h} id={id} ring={}..{} screen={r:?}", tap.start, tap.total);
            }
            let (first, rest, skip) = compose(h, id, &tap, reply, geo);
            // Only the tail of the output, and no screen to put under it: make
            // the program repaint, so the mirror shows its whole screen again
            // (claude redraws everything on SIGWINCH). Happens when this
            // window's webview couldn't answer — hidden and throttled.
            if first.contains("\"partial\":true") {
                repaint(h, id);
            }
            out.push(first);
            let bytes = rest.iter().map(msg_len).sum();
            desk.subs.insert((h, id), Sub::Live { rx: tap.rx, skip, backlog: rest.into(), bytes });
        }
        let Some(Sub::Live { rx, skip, backlog, bytes }) = desk.subs.get_mut(&(h, id)) else { continue };
        let mut gone = false;
        loop {
            match rx.try_recv() {
                Ok(TapMsg::Bytes(mut b)) => {
                    if *skip > 0 {
                        let n = (*skip).min(b.len() as u64) as usize;
                        b.drain(..n);
                        *skip -= n as u64;
                        if b.is_empty() {
                            continue;
                        }
                    }
                    *bytes += b.len();
                    // Coalesce into the last frame while it is small.
                    if let Some(TapMsg::Bytes(last)) = backlog.back_mut() {
                        if last.len() + b.len() <= CHUNK {
                            last.extend_from_slice(&b);
                            continue;
                        }
                    }
                    backlog.push_back(TapMsg::Bytes(b));
                }
                Ok(m @ TapMsg::Resize(..)) => {
                    if *skip == 0 {
                        backlog.push_back(m);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
        if *bytes > MAX_BACKLOG {
            // Too far behind to catch up byte by byte: start this one over.
            desk.subs.remove(&(h, id));
            if let Some(msg) = subscribe(desk, h, id) {
                out.push(msg);
            }
            continue;
        }
        while desk.unacked + out.iter().map(|m| m.len() as u64).sum::<u64>() < MAX_UNACKED {
            let Some(m) = backlog.pop_front() else { break };
            *bytes = bytes.saturating_sub(msg_len(&m));
            match m {
                TapMsg::Bytes(b) if b.len() > CHUNK => {
                    for c in b.chunks(CHUNK) {
                        out.push(frame(h, id, &TapMsg::Bytes(c.to_vec())));
                    }
                }
                m => out.push(frame(h, id, &m)),
            }
        }
        if gone && backlog.is_empty() {
            desk.subs.remove(&(h, id));
            // ⌘⇧R replaced the process behind the row: mirror the new one.
            if session_exists(h, id) {
                if let Some(msg) = subscribe(desk, h, id) {
                    out.push(msg);
                }
            } else {
                out.push(json!({"t": "gone", "h": h, "id": id}).to_string());
            }
        }
    }
}

fn msg_len(m: &TapMsg) -> usize {
    match m {
        TapMsg::Bytes(b) => b.len(),
        TapMsg::Resize(..) => 0,
    }
}

/// One piece of an upload: a file's bytes at `path` (relative; a dropped
/// folder arrives as many files under one root), `last` closing the file.
fn upload_chunk(desk: &mut Desk, req: &Value) -> Result<(), String> {
    let uid: String = req.get("uid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("bad upload".into());
    }
    let rel = req.get("path").and_then(Value::as_str).and_then(safe_rel).ok_or("bad file name")?;
    let data = req.get("data").and_then(Value::as_str).and_then(super::crypto::unb64).unwrap_or_default();
    let last = req.get("last").and_then(Value::as_bool).unwrap_or(false);
    if !desk.uploads.contains_key(&uid) {
        prune_uploads();
        let dir = uploads_dir().join(&uid);
        crate::secrets::private_dir(&uploads_dir())?;
        crate::secrets::private_dir(&dir)?;
        desk.uploads.insert(uid.clone(), Upload { dir, roots: Vec::new(), bytes: 0, open: None });
    }
    let u = desk.uploads.get_mut(&uid).expect("just inserted");
    u.bytes += data.len() as u64;
    if u.bytes > MAX_UPLOAD {
        return Err(format!("too big to upload (over {} MB)", MAX_UPLOAD / 1024 / 1024));
    }
    let key = rel.display().to_string();
    let root = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
    if !u.roots.contains(&root) {
        u.roots.push(root);
    }
    if u.open.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
        let path = u.dir.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        u.open = Some((key, f));
    }
    if let Some((_, f)) = u.open.as_mut() {
        f.write_all(&data).map_err(|e| e.to_string())?;
    }
    if last {
        u.open = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap(start: u64, ring: &[u8], resizes: Vec<(u64, u16, u16)>) -> DesktopTap {
        DesktopTap {
            start,
            ring: ring.to_vec(),
            resizes,
            init: (100, 30),
            total: start + ring.len() as u64,
            rx: std::sync::mpsc::channel().1,
        }
    }

    fn snap(data: &str, offset: u64) -> Option<SnapReply> {
        Some(SnapReply { data: Some(data.into()), offset, cols: 120, rows: 40 })
    }

    fn inflate(z: &str) -> Vec<u8> {
        use std::io::Read;
        let raw = super::super::crypto::unb64(z).unwrap();
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn a_snapshot_inside_the_ring_is_followed_by_the_bytes_after_it() {
        let t = tap(10, b"abcdefgh", vec![(14, 90, 20)]);
        let (first, rest, skip) = compose(1, 2, &t, snap("SCREEN", 12), (80, 24));
        let f: Value = serde_json::from_str(&first).unwrap();
        assert_eq!((f["cols"].as_u64(), f["partial"].as_bool()), (Some(120), Some(false)));
        assert_eq!(inflate(f["z"].as_str().unwrap()), b"SCREEN");
        assert_eq!(
            rest,
            vec![TapMsg::Bytes(b"cd".to_vec()), TapMsg::Resize(90, 20), TapMsg::Bytes(b"efgh".to_vec())]
        );
        assert_eq!(skip, 0);
    }

    #[test]
    fn a_snapshot_ahead_of_the_tap_skips_what_it_already_shows() {
        let t = tap(0, b"abc", vec![]);
        let (_, rest, skip) = compose(1, 2, &t, snap("S", 7), (80, 24));
        assert!(rest.is_empty());
        assert_eq!(skip, 4);
    }

    #[test]
    fn a_whole_ring_replays_from_the_spawn_size() {
        let t = tap(0, b"hello", vec![(2, 90, 20)]);
        let (first, rest, _) = compose(1, 2, &t, None, (80, 24));
        let f: Value = serde_json::from_str(&first).unwrap();
        assert_eq!((f["cols"].as_u64(), f["rows"].as_u64()), (Some(100), Some(30)));
        assert_eq!(
            rest,
            vec![TapMsg::Bytes(b"he".to_vec()), TapMsg::Resize(90, 20), TapMsg::Bytes(b"llo".to_vec())]
        );
    }

    #[test]
    fn without_a_snapshot_a_rolled_ring_is_partial() {
        let t = tap(50, b"xyz", vec![]);
        let (first, rest, _) = compose(1, 2, &t, snap("S", 10), (80, 24));
        let f: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(f["partial"], true, "the snapshot fell out of the ring");
        assert_eq!((f["cols"].as_u64(), f["rows"].as_u64()), (Some(80), Some(24)));
        assert_eq!(rest, vec![TapMsg::Bytes(b"xyz".to_vec())]);
    }

    #[test]
    fn upload_paths_stay_inside() {
        assert_eq!(safe_rel("../../etc/passwd"), Some(PathBuf::from("etc/passwd")));
        assert_eq!(safe_rel("/abs/a.png"), Some(PathBuf::from("abs/a.png")));
        assert_eq!(safe_rel("dir/./b\n.txt"), Some(PathBuf::from("dir/b.txt")));
        assert_eq!(safe_rel(".."), None);
        assert_eq!(safe_rel(""), None);
    }

    #[test]
    fn frames_deflate_big_output() {
        let small: Value = serde_json::from_str(&frame(3, 4, &TapMsg::Bytes(b"hi".to_vec()))).unwrap();
        assert_eq!(small["d"], b64(b"hi"));
        let big = vec![b'a'; 10_000];
        let f: Value = serde_json::from_str(&frame(3, 4, &TapMsg::Bytes(big.clone()))).unwrap();
        assert!(f["z"].as_str().unwrap().len() < 1000);
        assert_eq!(inflate(f["z"].as_str().unwrap()), big);
        let r: Value = serde_json::from_str(&frame(3, 4, &TapMsg::Resize(9, 8))).unwrap();
        assert_eq!((r["t"].as_str(), r["cols"].as_u64()), (Some("r"), Some(9)));
    }
}
