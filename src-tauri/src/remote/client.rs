//! Remote Hosts, the client side: this Mulpex driving another one — a *remote
//! host* (a Linux VM, another Mac) — whose projects then show up here as tabs,
//! behind the host switcher. The host side is `desktop.rs`; the design is in
//! `docs/remote-hosts.md`.
//!
//! This Mulpex pairs and connects to a host exactly as a phone does (same
//! relay, same pairing link, same handshake and sealed frames), then says
//! `hello` as a desktop and gets the host's own command surface, events and
//! terminal streams instead of a phone's view.
//!
//! **Handles.** A remote project is known here as `slot << 32 | host handle`:
//! `slot` numbers the paired hosts from 1, and 0 is this Mulpex. Local handles
//! count up from 1 and never get near 2^32, and the sum is exact in JS. So every
//! store, map and `(handle, id)` key in the frontend works unchanged for remote
//! projects — and this module is the one place a handle crosses the boundary:
//! lowered on the way out (`lower`), lifted on the way in (`lift`).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tungstenite::Message;

use super::crypto::{self, b64, unb64, unb64_32, Key32};
use super::{set_read_tick, Ws};

/// How long a read waits: keystrokes go out and echoes come in within it.
const TICK: Duration = Duration::from_millis(4);
const PING_EVERY: Duration = Duration::from_secs(20);
/// Acknowledge received output in steps this big (`desktop.rs` stops sending
/// when too far ahead of the acknowledgements).
const ACK_EVERY: u64 = 256 * 1024;
/// A command that takes longer than this is reported as failed.
const RPC_TIMEOUT: Duration = Duration::from_secs(120);
const UPLOAD_CHUNK: usize = 256 * 1024;
const MAX_UPLOAD: u64 = 100 * 1024 * 1024;
/// An update order with no progress back by now went to a host from before
/// remote updates existed: it ignores a message it doesn't know.
const UPDATE_ANSWER: Duration = Duration::from_secs(30);

pub fn base_of(slot: u32) -> u64 {
    (slot as u64) << 32
}

/// A paired host, as `<mulpex home>/remote/hosts.json` keeps it (0600: it holds
/// this Mulpex's private key for that host).
#[derive(Clone, Serialize, Deserialize)]
pub struct HostCfg {
    pub slot: u32,
    pub name: String,
    /// The user named it; the host's own name no longer replaces it.
    #[serde(default)]
    pub renamed: bool,
    pub relay_url: String,
    pub host_id: String,
    pub host_pub: String,
    pub device: String,
    pub device_secret: String,
    pub added: u64,
    /// Connect at launch / now. "Disconnect" turns it off until "Connect".
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The host's run id when last connected (`desktop::boot_id`).
    #[serde(default)]
    pub boot: Option<String>,
    /// The last layout seen — project and row names, no screen content — so
    /// the host's tabs can be drawn greyed while it is unreachable.
    #[serde(default)]
    pub layout: Option<Value>,
}

fn yes() -> bool {
    true
}

/// What the frontend sees of one host (`host-changed` carries it too).
#[derive(Clone, Serialize)]
pub struct HostStatus {
    pub slot: u32,
    pub name: String,
    pub relay_url: String,
    pub enabled: bool,
    /// `connecting` / `connected` / `offline` (the host isn't reachable) /
    /// `error` (refused: see `error`) / `disconnected` (turned off here).
    pub state: String,
    pub error: Option<String>,
    /// The host's run id on this connection (`desktop::boot_id`); the
    /// frontend rebuilds the host's tabs when it differs from `cached_boot`.
    pub boot: Option<String>,
    /// The run id the cached `layout` belongs to.
    pub cached_boot: Option<String>,
    pub geo: Option<Value>,
    pub layout: Option<Value>,
    /// The host's app version, from its `welcome` or `refused`.
    pub version: Option<String>,
    /// The host's wire fingerprint (`wire.rs`), from the same. `None`: a host
    /// from before it, which must update to talk to this one.
    pub wire: Option<String>,
    /// An update this Mulpex ordered on the host (`remote_update.rs` there).
    pub update: Option<HostUpdate>,
}

/// A remote-ordered update, as the host last reported it. `step`: `sent` (no
/// answer yet), then the host's `checking` / `stopping` / `downloading` /
/// `installing` / `restarting`, and `done` once it is back on `to` — or
/// `error` / `cancelled`.
#[derive(Clone, Serialize)]
pub struct HostUpdate {
    pub to: String,
    pub step: String,
    /// While `stopping`: the claudes there that have not stopped yet.
    pub pending: Value,
    pub percent: Option<u64>,
    pub error: Option<String>,
    #[serde(skip)]
    sent_at: Instant,
}

impl HostUpdate {
    fn new(to: &str) -> Self {
        HostUpdate { to: to.into(), step: "sent".into(), pending: json!([]), percent: None, error: None, sent_at: Instant::now() }
    }
    fn ended(&self) -> bool {
        matches!(self.step.as_str(), "done" | "error" | "cancelled")
    }
}

struct Conn {
    epoch: u64,
    state: String,
    error: Option<String>,
    tx: Option<Sender<String>>,
    /// The sealed link, from the handshake on — unlike `tx`, also while the
    /// host refused this version, which is when an update order must go out.
    link: Option<Sender<String>>,
    geo: Option<Value>,
    boot: Option<String>,
    version: Option<String>,
    wire: Option<String>,
    update: Option<HostUpdate>,
}

struct Inner {
    hosts: Vec<HostCfg>,
    conns: HashMap<u32, Conn>,
    pending: HashMap<String, Sender<Result<Value, String>>>,
    channels: HashMap<(u32, u64, usize), Channel<Value>>,
    app: Option<AppHandle>,
    epoch: u64,
}

static CLIENT: OnceLock<Mutex<Inner>> = OnceLock::new();

fn inner() -> &'static Mutex<Inner> {
    CLIENT.get_or_init(|| {
        Mutex::new(Inner {
            hosts: load_hosts(),
            conns: HashMap::new(),
            pending: HashMap::new(),
            channels: HashMap::new(),
            app: None,
            epoch: 0,
        })
    })
}

fn load_hosts() -> Vec<HostCfg> {
    std::fs::read_to_string(super::remote_dir().join("hosts.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_hosts(g: &Inner) {
    super::write_json("hosts.json", &g.hosts);
}

fn status_of(g: &Inner, h: &HostCfg) -> HostStatus {
    let c = g.conns.get(&h.slot);
    HostStatus {
        slot: h.slot,
        name: h.name.clone(),
        relay_url: h.relay_url.clone(),
        enabled: h.enabled,
        state: c.map(|c| c.state.clone()).unwrap_or_else(|| "disconnected".into()),
        error: c.and_then(|c| c.error.clone()),
        boot: c.and_then(|c| c.boot.clone()),
        cached_boot: h.boot.clone(),
        geo: c.and_then(|c| c.geo.clone()),
        layout: h.layout.clone(),
        version: c.and_then(|c| c.version.clone()),
        wire: c.and_then(|c| c.wire.clone()),
        update: c.and_then(|c| c.update.clone()),
    }
}

fn emit_host(slot: u32) {
    let (app, st) = {
        let g = inner().lock().unwrap();
        let st = g.hosts.iter().find(|h| h.slot == slot).map(|h| status_of(&g, h));
        (g.app.clone(), st)
    };
    if let (Some(app), Some(st)) = (app, st) {
        let _ = app.emit("host-changed", st);
    }
}

fn emit(name: &str, payload: Value) {
    let app = inner().lock().unwrap().app.clone();
    if let Some(app) = app {
        let _ = app.emit(name, payload);
    }
}

/// Connect every enabled host. At launch.
pub fn init(app: AppHandle) {
    let slots: Vec<u32> = {
        let mut g = inner().lock().unwrap();
        g.app = Some(app);
        g.hosts.iter().filter(|h| h.enabled).map(|h| h.slot).collect()
    };
    for slot in slots {
        start(slot, None);
    }
}

/// (Re)start the connection thread for `slot`; any older one stops.
fn start(slot: u32, pairing: Option<(Key32, Sender<Result<(), String>>)>) {
    let epoch = {
        let mut g = inner().lock().unwrap();
        g.epoch += 1;
        let epoch = g.epoch;
        g.conns.insert(
            slot,
            Conn {
                epoch,
                state: "connecting".into(),
                error: None,
                tx: None,
                link: None,
                geo: None,
                boot: None,
                version: None,
                wire: None,
                update: None,
            },
        );
        epoch
    };
    emit_host(slot);
    std::thread::spawn(move || run(slot, epoch, pairing));
}

fn stop(slot: u32) {
    let mut g = inner().lock().unwrap();
    g.epoch += 1;
    g.conns.remove(&slot);
}

fn current(slot: u32, epoch: u64) -> bool {
    inner().lock().unwrap().conns.get(&slot).is_some_and(|c| c.epoch == epoch)
}

fn set_state(slot: u32, epoch: u64, state: &str, error: Option<String>) {
    {
        let mut g = inner().lock().unwrap();
        let Some(c) = g.conns.get_mut(&slot).filter(|c| c.epoch == epoch) else { return };
        if c.state == state && c.error == error {
            return;
        }
        c.state = state.into();
        c.error = error;
        if state != "connected" {
            c.tx = None;
        }
    }
    // Commands waiting on a connection that just went: fail them now.
    if state != "connected" {
        fail_pending_of(slot);
    }
    emit_host(slot);
}

fn fail_pending_of(slot: u32) {
    let prefix = format!("{slot}-");
    let mut g = inner().lock().unwrap();
    let keys: Vec<String> = g.pending.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
    for k in keys {
        if let Some(tx) = g.pending.remove(&k) {
            let _ = tx.send(Err("the connection to the remote host dropped".into()));
        }
    }
}

fn ws_url(h: &HostCfg) -> String {
    let base = if let Some(rest) = h.relay_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = h.relay_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        h.relay_url.clone()
    };
    format!("{base}/ws/client?host={}", h.host_id)
}

/// Why a connection stopped: retry, or give up until the user acts.
enum End {
    Retry(String),
    Stop,
}

fn run(slot: u32, epoch: u64, mut pairing: Option<(Key32, Sender<Result<(), String>>)>) {
    let mut backoff = 1u64;
    loop {
        let Some(cfg) = inner().lock().unwrap().hosts.iter().find(|h| h.slot == slot).cloned() else { return };
        if !current(slot, epoch) {
            return;
        }
        match tungstenite::connect(ws_url(&cfg).as_str()) {
            Ok((mut ws, _)) => {
                set_read_tick(&mut ws, TICK);
                let end = serve(slot, epoch, &cfg, &mut ws, &mut pairing);
                let _ = ws.close(None);
                let _ = ws.flush();
                match end {
                    End::Stop => return,
                    End::Retry(why) => {
                        set_state(slot, epoch, "connecting", Some(why));
                        backoff = 1;
                    }
                }
            }
            Err(e) => {
                if let Some((_, done)) = pairing.take() {
                    let _ = done.send(Err(format!("Can't reach the relay: {e}")));
                    return;
                }
                set_state(slot, epoch, "offline", Some(format!("Can't reach the relay: {e}")));
            }
        }
        for _ in 0..backoff * 10 {
            if !current(slot, epoch) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(10);
    }
}

fn raw(ws: &mut Ws, data: Value) -> Result<(), String> {
    ws.send(Message::Text(json!({ "data": data }).to_string().into())).map_err(|e| e.to_string())
}

fn serve(
    slot: u32,
    epoch: u64,
    cfg: &HostCfg,
    ws: &mut Ws,
    pairing: &mut Option<(Key32, Sender<Result<(), String>>)>,
) -> End {
    let dev = unb64_32(&cfg.device_secret).unwrap_or([0; 32]);
    let host_pub = unb64_32(&cfg.host_pub).unwrap_or([0; 32]);
    let mut eph: Option<Key32> = None;
    let mut ch: Option<crypto::Channel> = None;
    let (tx, rx): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
    let mut last_ping = Instant::now();
    let mut received: u64 = 0;
    let mut acked: u64 = 0;
    let me = super::desktop::hostname();
    loop {
        if !current(slot, epoch) {
            return End::Stop;
        }
        if let Some(end) = update_unanswered(slot, epoch) {
            return end;
        }
        if let Some(c) = ch.as_mut() {
            while let Ok(plain) = rx.try_recv() {
                let sealed = c.seal(plain.as_bytes());
                if let Err(e) = raw(ws, json!({"t": "sealed", "c": sealed})) {
                    return End::Retry(e);
                }
            }
            if received - acked >= ACK_EVERY {
                let sealed = c.seal(json!({"t": "ack", "n": received - acked}).to_string().as_bytes());
                if let Err(e) = raw(ws, json!({"t": "sealed", "c": sealed})) {
                    return End::Retry(e);
                }
                acked = received;
            }
        }
        if last_ping.elapsed() >= PING_EVERY {
            if let Err(e) = ws.send(Message::Ping(Default::default())) {
                return End::Retry(e.to_string());
            }
            last_ping = Instant::now();
        }
        let text = match ws.read() {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) => return End::Retry("the relay closed the connection".into()),
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue
            }
            Err(e) => return End::Retry(e.to_string()),
        };
        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
        // The relay saying whether the host is in its room right now.
        if msg.get("event").and_then(Value::as_str) == Some("host") {
            ch = None;
            if msg.get("online") != Some(&Value::Bool(true)) {
                if let Some((_, done)) = pairing.take() {
                    let _ = done.send(Err(
                        "That Mulpex isn't connected to the relay — turn on Remote Control (⌘⇧O) there".into(),
                    ));
                    return End::Stop;
                }
                set_state(slot, epoch, "offline", Some("Remote Control is off there, or it's asleep".into()));
                continue;
            }
            if let Some((secret, _)) = pairing.as_ref() {
                let dp = crypto::public_of(&dev);
                let proof = crypto::pair_proof(secret, &cfg.device, &dp);
                let r = raw(ws, json!({"t": "pair", "dev": cfg.device, "dp": b64(&dp), "name": me,
                                       "proof": b64(&proof), "kind": "desktop"}));
                if let Err(e) = r {
                    return End::Retry(e);
                }
            } else {
                let e = crypto::random32();
                eph = Some(e);
                if let Err(e) = raw(ws, json!({"t": "hs", "dev": cfg.device, "ep": b64(&crypto::public_of(&e))})) {
                    return End::Retry(e);
                }
            }
            continue;
        }
        let Some(data) = msg.get("data") else { continue };
        match data.get("t").and_then(Value::as_str) {
            Some("paired") => {
                if let Some((_, done)) = pairing.take() {
                    let _ = done.send(Ok(()));
                }
                let e = crypto::random32();
                eph = Some(e);
                if let Err(e) = raw(ws, json!({"t": "hs", "dev": cfg.device, "ep": b64(&crypto::public_of(&e))})) {
                    return End::Retry(e);
                }
            }
            Some("denied") => {
                let why = data.get("why").and_then(Value::as_str).unwrap_or("refused").to_string();
                if let Some((_, done)) = pairing.take() {
                    let _ = done.send(Err(why));
                    return End::Stop;
                }
                if why == "bad frame" {
                    return End::Retry(why);
                }
                set_state(
                    slot,
                    epoch,
                    "error",
                    Some(format!("{} removed this Mac ({why}). Remove it here and pair again.", cfg.name)),
                );
                return End::Stop;
            }
            Some("hs") => {
                let (Some(e), Some(eh)) = (eph.take(), data.get("eh").and_then(Value::as_str).and_then(unb64_32))
                else {
                    continue;
                };
                let Some(mut c) = crypto::client_handshake(&e, &dev, &host_pub, &eh) else {
                    return End::Retry("bad handshake".into());
                };
                let hello = json!({"t": "hello", "kind": "desktop", "proto": super::desktop::PROTO,
                                   "app": env!("CARGO_PKG_VERSION"), "wire": env!("MULPEX_WIRE"), "name": me});
                if let Err(e) = raw(ws, json!({"t": "sealed", "c": c.seal(hello.to_string().as_bytes())})) {
                    return End::Retry(e);
                }
                ch = Some(c);
                received = 0;
                acked = 0;
                if let Some(c) = inner().lock().unwrap().conns.get_mut(&slot).filter(|c| c.epoch == epoch) {
                    c.link = Some(tx.clone());
                }
            }
            Some("sealed") => {
                let Some(c) = ch.as_mut() else { continue };
                let Some(plain) = data.get("c").and_then(Value::as_str).and_then(|s| c.open(s)) else {
                    return End::Retry("a frame failed to open".into());
                };
                received += plain.len() as u64;
                let Ok(m) = serde_json::from_slice::<Value>(&plain) else { continue };
                if let Some(end) = on_message(slot, epoch, &tx, m) {
                    return end;
                }
            }
            _ => {}
        }
    }
}

/// One message from the host. `Some` ends the connection.
fn on_message(slot: u32, epoch: u64, tx: &Sender<String>, m: Value) -> Option<End> {
    let base = base_of(slot);
    match m.get("t").and_then(Value::as_str).unwrap_or("") {
        "welcome" => {
            let boot = m.get("boot").and_then(Value::as_str).map(String::from);
            let name = m.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let subs: Vec<(u64, usize)> = {
                let mut g = inner().lock().unwrap();
                if let Some(h) = g.hosts.iter_mut().find(|h| h.slot == slot) {
                    if !h.renamed && !name.is_empty() {
                        h.name = name;
                    }
                }
                if let Some(c) = g.conns.get_mut(&slot).filter(|c| c.epoch == epoch) {
                    c.tx = Some(tx.clone());
                    c.geo = m.get("geo").cloned();
                    c.boot = boot;
                    c.version = m.get("app").and_then(Value::as_str).map(String::from);
                    c.wire = m.get("wire").and_then(Value::as_str).map(String::from);
                    // A host updated after refusing us comes back on our version.
                    if let Some(u) = c.update.as_mut().filter(|u| Some(&u.to) == c.version.as_ref()) {
                        u.step = "done".into();
                        u.pending = json!([]);
                    }
                }
                save_hosts(&g);
                g.channels.keys().filter(|k| k.0 == slot).map(|k| (k.1, k.2)).collect()
            };
            // The frontend compares this boot with the one its tabs were built
            // for, and rebuilds them when the host restarted.
            set_state(slot, epoch, "connected", None);
            // Re-mirror every pane still open here: each restarts from a snapshot.
            for (h, id) in subs {
                let _ = tx.send(json!({"t": "sub", "h": h, "id": id}).to_string());
            }
        }
        "refused" => return on_refused(slot, epoch, tx, &m),
        "update-progress" => return on_update_progress(slot, epoch, &m),
        // A phone's view: the host is a Mulpex from before Remote Hosts.
        "state" => {
            // A host that refused us still sends its phone view now and then
            // (one from before v0.48 always does). While an update we ordered
            // there runs, that is not "too old" — the update is the answer.
            if with_conn(slot, epoch, |c| c.update.as_ref().is_some_and(|u| !u.ended())).unwrap_or(false) {
                return None;
            }
            set_state(slot, epoch, "error", Some("That Mulpex is too old to be driven from here — update it".into()));
            return Some(End::Stop);
        }
        "res" | "typed" => {
            let rid = m.get("rid").and_then(Value::as_str).unwrap_or("");
            let key = format!("{slot}-{rid}");
            let tx = inner().lock().unwrap().pending.remove(&key);
            if let Some(tx) = tx {
                let ok = m.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let res = if ok {
                    let mut v = m.get("value").cloned().unwrap_or(Value::Null);
                    lift(&mut v, base);
                    Ok(v)
                } else {
                    Err(m.get("error").or(m.get("why")).and_then(Value::as_str).unwrap_or("failed").to_string())
                };
                let _ = tx.send(res);
            }
        }
        "ev" => {
            let name = m.get("name").and_then(Value::as_str).unwrap_or("");
            let mut payload = m.get("payload").cloned().unwrap_or(Value::Null);
            if name == "import-update" {
                if let Some(h) = payload.as_u64() {
                    payload = json!(h + base);
                }
            } else {
                lift(&mut payload, base);
            }
            if name == "projects-changed" {
                emit("host-projects-changed", json!({"slot": slot, "ws": payload}));
            } else if name == "checkpoint-changed" {
                // Unlike the rest it names no project, so say which host.
                emit("host-checkpoint-changed", json!({"slot": slot, "ev": payload}));
            } else if super::desktop::EVENT_NAMES.contains(&name) {
                emit(name, payload);
            }
        }
        "geo" => {
            {
                let mut g = inner().lock().unwrap();
                if let Some(c) = g.conns.get_mut(&slot) {
                    c.geo = Some(m.clone());
                }
            }
            emit("host-geo", json!({"slot": slot, "geo": m}));
        }
        t @ ("snap" | "o" | "r" | "gone") => {
            let h = m.get("h").and_then(Value::as_u64).unwrap_or(0);
            let id = m.get("id").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut out = json!({"t": t});
            match t {
                "snap" | "o" => {
                    let bytes = match (m.get("z").and_then(Value::as_str), m.get("d").and_then(Value::as_str)) {
                        (Some(z), _) => inflate(z),
                        (None, Some(d)) => unb64(d).unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    out["d"] = json!(b64std(&bytes));
                    if t == "snap" {
                        out["cols"] = m["cols"].clone();
                        out["rows"] = m["rows"].clone();
                        out["partial"] = m["partial"].clone();
                    }
                }
                "r" => {
                    out["cols"] = m["cols"].clone();
                    out["rows"] = m["rows"].clone();
                }
                _ => {}
            }
            let ch = inner().lock().unwrap().channels.get(&(slot, h, id)).cloned();
            if let Some(ch) = ch {
                let _ = ch.send(out);
            }
        }
        _ => {}
    }
    None
}

fn inflate(z: &str) -> Vec<u8> {
    let Some(raw) = unb64(z) else { return Vec::new() };
    let mut out = Vec::new();
    let _ = flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut out);
    out
}

/// Standard base64 for the frontend (`atob`), not the url-safe wire form.
fn b64std(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Every project handle in a host's answer or event, into this Mulpex's
/// numbering: a `handle` field anywhere, and a workspace's `active`.
pub fn lift(v: &mut Value, base: u64) {
    match v {
        Value::Object(o) => {
            let is_ws = o.contains_key("projects");
            for (k, x) in o.iter_mut() {
                if (k == "handle" || (is_ws && k == "active")) && x.is_u64() {
                    *x = json!(x.as_u64().unwrap() + base);
                } else {
                    lift(x, base);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| lift(x, base)),
        _ => {}
    }
}

/// The handles in a command's arguments, into the host's numbering.
pub fn lower(args: &mut Value, base: u64) {
    let down = |x: &mut Value| {
        if let Some(n) = x.as_u64() {
            if n >= base {
                *x = json!(n - base);
            }
        }
    };
    if let Some(h) = args.get_mut("projectHandle") {
        down(h);
    }
    if let Some(Value::Array(hs)) = args.get_mut("handles") {
        hs.iter_mut().for_each(down);
    }
    // `broadcast_send`'s picks: `[{handle, id}]`.
    if let Some(Value::Array(ts)) = args.get_mut("targets") {
        ts.iter_mut().filter_map(|t| t.get_mut("handle")).for_each(down);
    }
}

fn new_rid() -> String {
    b64(&crypto::random32()[..9])
}

/// `1.2.3` → `(1, 2, 3)`; anything else is not a version we can order.
fn version_of(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

fn with_conn<R>(slot: u32, epoch: u64, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
    inner().lock().unwrap().conns.get_mut(&slot).filter(|c| c.epoch == epoch).map(f)
}

/// The host refused this Mulpex's version. Older than us: order it to update
/// to ours and stay on the link to hear how it goes. Newer: it is done, if we
/// ordered that; otherwise this Mac is the one behind.
fn on_refused(slot: u32, epoch: u64, tx: &Sender<String>, m: &Value) -> Option<End> {
    let mine = env!("CARGO_PKG_VERSION");
    let theirs = m.get("app").and_then(Value::as_str).unwrap_or("?").to_string();
    let update = with_conn(slot, epoch, |c| {
        c.version = Some(theirs.clone());
        c.wire = m.get("wire").and_then(Value::as_str).map(String::from);
        c.update.clone()
    })
    .flatten();
    let older = matches!((version_of(&theirs), version_of(mine)), (Some(a), Some(b)) if a < b);
    if older {
        match update.filter(|u| u.to == mine) {
            // Ordered already (we are hearing this again after its restart, or
            // on a reconnect mid-run): keep listening — unless it ended.
            Some(u) if u.ended() => {
                let why = u.error.unwrap_or_else(|| "the update was cancelled".into());
                set_state(slot, epoch, "error", Some(format!("Updating that Mulpex to v{mine} failed: {why}")));
                return Some(End::Stop);
            }
            Some(_) => {}
            None => {
                with_conn(slot, epoch, |c| c.update = Some(HostUpdate::new(mine)));
                let _ = tx.send(json!({"t": "update", "to": mine}).to_string());
            }
        }
        set_state(slot, epoch, "updating", Some(format!("Updating that Mulpex from v{theirs} to v{mine}…")));
        return None;
    }
    if update.as_ref().is_some_and(|u| u.to == theirs) {
        with_conn(slot, epoch, |c| {
            if let Some(u) = c.update.as_mut() {
                u.step = "done".into();
                u.pending = json!([]);
            }
        });
        set_state(slot, epoch, "error", Some(format!("That Mulpex is on v{theirs} now — update this one to match")));
        return Some(End::Stop);
    }
    set_state(
        slot,
        epoch,
        "error",
        Some(format!("That Mulpex runs v{theirs}, this one v{mine} — update both to the same version")),
    );
    Some(End::Stop)
}

/// `update-progress` from the host.
fn on_update_progress(slot: u32, epoch: u64, m: &Value) -> Option<End> {
    let step = m.get("step").and_then(Value::as_str).unwrap_or("").to_string();
    let to = m.get("to").and_then(Value::as_str).unwrap_or("").to_string();
    let refused = with_conn(slot, epoch, |c| {
        let u = c.update.get_or_insert_with(|| HostUpdate::new(&to));
        u.step = step.clone();
        u.pending = m.get("pending").cloned().unwrap_or_else(|| json!([]));
        u.percent = m.get("percent").and_then(Value::as_u64);
        u.error = m.get("error").and_then(Value::as_str).map(String::from);
        c.state == "updating"
    })?;
    match step.as_str() {
        "restarting" => {
            emit_host(slot);
            Some(End::Retry("That Mulpex is restarting into the update".into()))
        }
        // A host that refused us has nothing else to offer on this link.
        "error" | "cancelled" if refused => {
            let why = m.get("error").and_then(Value::as_str).unwrap_or("the update was cancelled");
            set_state(slot, epoch, "error", Some(format!("Updating that Mulpex to v{to} failed: {why}")));
            Some(End::Stop)
        }
        _ => {
            emit_host(slot);
            None
        }
    }
}

/// An order nobody answered: the host predates remote updates.
fn update_unanswered(slot: u32, epoch: u64) -> Option<End> {
    let refused = with_conn(slot, epoch, |c| {
        let u = c.update.as_mut().filter(|u| u.step == "sent" && u.sent_at.elapsed() >= UPDATE_ANSWER)?;
        u.step = "error".into();
        u.error = Some("it didn't answer — it may be older than remote updates; update it by hand once".into());
        Some(c.state == "updating")
    })??;
    if refused {
        set_state(slot, epoch, "error", Some("That Mulpex didn't answer the update order — update it by hand once".into()));
        return Some(End::Stop);
    }
    emit_host(slot);
    None
}

/// Queue one message for the host. Fails when it isn't connected.
fn send_to(slot: u32, msg: Value) -> Result<(), String> {
    let g = inner().lock().unwrap();
    let c = g.conns.get(&slot).ok_or("that host is disconnected")?;
    let tx = c.tx.as_ref().ok_or_else(|| {
        let name = g.hosts.iter().find(|h| h.slot == slot).map(|h| h.name.as_str()).unwrap_or("the host");
        format!("{name} is offline")
    })?;
    tx.send(msg.to_string()).map_err(|_| "the connection to the remote host dropped".to_string())
}

/// Send a request carrying `rid`, and wait for its answer.
fn request(slot: u32, mut msg: Value) -> Result<Value, String> {
    let rid = new_rid();
    msg["rid"] = json!(rid);
    let (tx, rx) = std::sync::mpsc::channel();
    inner().lock().unwrap().pending.insert(format!("{slot}-{rid}"), tx);
    if let Err(e) = send_to(slot, msg) {
        inner().lock().unwrap().pending.remove(&format!("{slot}-{rid}"));
        return Err(e);
    }
    let res = rx.recv_timeout(RPC_TIMEOUT).unwrap_or_else(|_| Err("the remote host didn't answer".into()));
    inner().lock().unwrap().pending.remove(&format!("{slot}-{rid}"));
    res
}

/// Every file under `paths` (a folder: everything in it, under its own name),
/// as (path on this disk, relative name to upload as).
fn upload_list(paths: &[String]) -> Result<(Vec<(PathBuf, String)>, u64), String> {
    let mut out = Vec::new();
    let mut total = 0u64;
    fn walk(p: &std::path::Path, rel: String, out: &mut Vec<(PathBuf, String)>, total: &mut u64) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if meta.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(p).map_err(|e| e.to_string())?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                walk(&e.path(), format!("{rel}/{name}"), out, total)?;
            }
        } else if meta.is_file() {
            *total += meta.len();
            if *total > MAX_UPLOAD {
                return Err(format!("too big to upload (over {} MB)", MAX_UPLOAD / 1024 / 1024));
            }
            out.push((p.to_path_buf(), rel));
        }
        Ok(())
    }
    for p in paths {
        let path = PathBuf::from(p);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "upload".into());
        walk(&path, name, &mut out, &mut total)?;
    }
    Ok((out, total))
}

fn upload(slot: u32, paths: &[String]) -> Result<Vec<String>, String> {
    let (files, _) = upload_list(paths)?;
    let uid = new_rid().replace(['-', '_'], "x");
    for (path, rel) in &files {
        let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut buf = vec![0u8; UPLOAD_CHUNK];
        let mut sent_any = false;
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            let last = n < UPLOAD_CHUNK;
            if n == 0 && sent_any {
                // The file ended exactly on a chunk: close it with an empty one.
                send_to(slot, json!({"t": "up", "uid": uid, "path": rel, "data": "", "last": true}))?;
                break;
            }
            send_to(slot, json!({"t": "up", "uid": uid, "path": rel, "data": b64(&buf[..n]), "last": last}))?;
            sent_any = true;
            if last {
                break;
            }
        }
    }
    let v = request(slot, json!({"t": "up-done", "uid": uid}))?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// The clipboard's image as a PNG file, if it holds one (what Claude Code
/// itself reads on Ctrl+V — here it has to cross to the host first).
#[cfg(target_os = "macos")]
fn clipboard_png() -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("mulpex-clip-{}.png", new_rid().replace(['-', '_'], "x")));
    let script = format!(
        "set f to open for access POSIX file \"{}\" with write permission\n\
         try\n write (the clipboard as «class PNGf») to f\n close access f\n\
         on error\n close access f\n error \"no image\"\n end try",
        path.display()
    );
    let ok = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let sized = std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
    if ok && sized {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn clipboard_png() -> Option<PathBuf> {
    None
}

// ---- the command surface ----

#[tauri::command]
pub fn host_list() -> Vec<HostStatus> {
    let g = inner().lock().unwrap();
    g.hosts.iter().map(|h| status_of(&g, h)).collect()
}

/// Pair with a host from its link (the VM's ⌘⇧O ▸ Copy link):
/// `https://<relay>/#p=<host id>.<host key>.<one-time secret>`.
#[tauri::command]
pub async fn host_pair(link: String) -> Result<HostStatus, String> {
    tauri::async_runtime::spawn_blocking(move || pair(&link))
        .await
        .map_err(|e| e.to_string())?
}

fn parse_link(link: &str) -> Result<(String, String, String, Key32), String> {
    let link = link.trim();
    let (relay, frag) = link.split_once("/#p=").ok_or("That isn't a Mulpex pairing link")?;
    let parts: Vec<&str> = frag.split('.').collect();
    let [host, host_pub, secret] = parts[..] else { return Err("That pairing link is incomplete".into()) };
    let secret = unb64_32(secret).ok_or("That pairing link is damaged")?;
    unb64_32(host_pub).ok_or("That pairing link is damaged")?;
    if !(relay.starts_with("http://") || relay.starts_with("https://")) || host.is_empty() {
        return Err("That isn't a Mulpex pairing link".into());
    }
    Ok((relay.to_string(), host.to_string(), host_pub.to_string(), secret))
}

fn pair(link: &str) -> Result<HostStatus, String> {
    let (relay_url, host_id, host_pub, secret) = parse_link(link)?;
    let slot = {
        let mut g = inner().lock().unwrap();
        if g.hosts.iter().any(|h| h.host_id == host_id && h.enabled) {
            return Err("This Mac is already paired with that Mulpex".into());
        }
        g.hosts.retain(|h| h.host_id != host_id);
        let slot = g.hosts.iter().map(|h| h.slot).max().unwrap_or(0) + 1;
        g.hosts.push(HostCfg {
            slot,
            name: "remote".into(),
            renamed: false,
            relay_url,
            host_id,
            host_pub,
            device: b64(&crypto::random32()[..12]),
            device_secret: b64(&crypto::random32()),
            added: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            enabled: true,
            boot: None,
            layout: None,
        });
        slot
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    start(slot, Some((secret, done_tx)));
    let res = done_rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| Err("No answer from that Mulpex".into()));
    match res {
        Ok(()) => {
            let g = inner().lock().unwrap();
            save_hosts(&g);
            let h = g.hosts.iter().find(|h| h.slot == slot).ok_or("gone")?;
            Ok(status_of(&g, h))
        }
        Err(e) => {
            stop(slot);
            inner().lock().unwrap().hosts.retain(|h| h.slot != slot);
            Err(e)
        }
    }
}

#[tauri::command]
pub fn host_remove(host: u32) {
    stop(host);
    let mut g = inner().lock().unwrap();
    g.hosts.retain(|h| h.slot != host);
    g.channels.retain(|k, _| k.0 != host);
    save_hosts(&g);
}

#[tauri::command]
pub fn host_rename(host: u32, name: String) {
    {
        let mut g = inner().lock().unwrap();
        if let Some(h) = g.hosts.iter_mut().find(|h| h.slot == host) {
            let name: String = name.trim().chars().take(40).collect();
            h.renamed = !name.is_empty();
            if !name.is_empty() {
                h.name = name;
            }
        }
        save_hosts(&g);
    }
    emit_host(host);
}

/// Connect (`on`) or disconnect a host, and remember which.
#[tauri::command]
pub fn host_connect(host: u32, on: bool) {
    {
        let mut g = inner().lock().unwrap();
        if let Some(h) = g.hosts.iter_mut().find(|h| h.slot == host) {
            h.enabled = on;
        }
        save_hosts(&g);
    }
    if on {
        start(host, None);
    } else {
        stop(host);
        fail_pending_of(host);
        emit_host(host);
    }
}

/// Remember a host's layout (and run id), for drawing it while unreachable.
#[tauri::command]
pub fn host_cache_layout(host: u32, boot: Option<String>, layout: Value) {
    let mut g = inner().lock().unwrap();
    if let Some(h) = g.hosts.iter_mut().find(|h| h.slot == host) {
        h.layout = Some(layout);
        if boot.is_some() {
            h.boot = boot;
        }
    }
    save_hosts(&g);
}

/// Order a connected host to update to `version` (this Mac is about to update
/// to it). Progress arrives on `host-changed` as `update`.
#[tauri::command]
pub fn host_update(host: u32, version: String) -> Result<(), String> {
    {
        let mut g = inner().lock().unwrap();
        let c = g.conns.get_mut(&host).ok_or("that host is disconnected")?;
        let link = c.link.clone().ok_or("that host is offline")?;
        link.send(json!({"t": "update", "to": version}).to_string())
            .map_err(|_| "the connection to the remote host dropped".to_string())?;
        c.update = Some(HostUpdate::new(&version));
    }
    emit_host(host);
    Ok(())
}

/// Call off a host's update; its claudes are told to carry on. Too late once it
/// is installing — that host restarts anyway.
#[tauri::command]
pub fn host_update_cancel(host: u32) {
    let link = inner().lock().unwrap().conns.get(&host).and_then(|c| c.link.clone());
    if let Some(link) = link {
        let _ = link.send(json!({"t": "update-cancel"}).to_string());
    }
}

/// Forget a finished/failed update's progress, so the banner stops showing it.
#[tauri::command]
pub fn host_update_clear(host: u32) {
    if let Some(c) = inner().lock().unwrap().conns.get_mut(&host) {
        c.update = None;
    }
    emit_host(host);
}

/// Run one of the host's commands (`rpc.rs`) with the frontend's arguments.
#[tauri::command]
pub async fn host_invoke(host: u32, cmd: String, args: Value) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut args = args;
        lower(&mut args, base_of(host));
        request(host, json!({"t": "rpc", "cmd": cmd, "args": args}))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Mirror a host's session into this channel: a snapshot, then its stream.
#[tauri::command]
pub fn host_attach(host: u32, project_handle: u64, id: usize, channel: Channel<Value>) {
    let h = project_handle.saturating_sub(base_of(host));
    inner().lock().unwrap().channels.insert((host, h, id), channel);
    let _ = send_to(host, json!({"t": "sub", "h": h, "id": id}));
}

#[tauri::command]
pub fn host_detach(host: u32, project_handle: u64, id: usize) {
    let h = project_handle.saturating_sub(base_of(host));
    inner().lock().unwrap().channels.remove(&(host, h, id));
    let _ = send_to(host, json!({"t": "unsub", "h": h, "id": id}));
}

/// Keystrokes for a host's session. Dropped while it's unreachable.
#[tauri::command]
pub fn host_input(host: u32, project_handle: u64, id: usize, data: Vec<u8>) {
    let h = project_handle.saturating_sub(base_of(host));
    let _ = send_to(host, json!({"t": "in", "h": h, "id": id, "d": b64(&data)}));
}

/// Drive the host's geometry from this window.
#[tauri::command]
pub fn host_geo(host: u32, cols: u16, rows: u16) {
    let _ = send_to(host, json!({"t": "geo", "cols": cols, "rows": rows}));
}

/// Ask the host's own frontend to do something only it can (⌘⇧K).
#[tauri::command]
pub async fn host_frontend_action(host: u32, project_handle: u64, id: usize, action: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let h = project_handle.saturating_sub(base_of(host));
        request(host, json!({"t": "fa", "action": action, "h": h, "id": id})).map(|_| ())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Copy files or folders to the host; resolves to their paths there.
#[tauri::command]
pub async fn host_upload(host: u32, paths: Vec<String>) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || upload(host, &paths))
        .await
        .map_err(|e| e.to_string())?
}

/// The clipboard's image, copied to the host; its path there, or `None` when
/// the clipboard holds no image.
#[tauri::command]
pub async fn host_clipboard_image(host: u32) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(png) = clipboard_png() else { return Ok(None) };
        let res = upload(host, &[png.display().to_string()]);
        let _ = std::fs::remove_file(&png);
        res.map(|p| p.into_iter().next())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_cross_the_boundary_both_ways() {
        let base = base_of(2);
        let mut ws = json!({"projects": [{"handle": 1, "sessions": [{"id": 3}]}, {"handle": 4}], "active": 4, "cols": 80});
        lift(&mut ws, base);
        assert_eq!(ws["projects"][0]["handle"], base + 1);
        assert_eq!(ws["active"], base + 4);
        assert_eq!(ws["projects"][0]["sessions"][0]["id"], 3, "ids are per project, untouched");
        assert_eq!(ws["cols"], 80);
        let mut ev = json!({"handle": 7, "snapshot": {"statuses": []}});
        lift(&mut ev, base);
        assert_eq!(ev["handle"], base + 7);
        let mut active_only = json!({"active": 2});
        lift(&mut active_only, base);
        assert_eq!(active_only["active"], 2, "`active` alone is a session index, not a handle");

        let mut args = json!({"projectHandle": base + 5, "id": 1, "handles": [base + 2, base + 1]});
        lower(&mut args, base);
        assert_eq!(args, json!({"projectHandle": 5, "id": 1, "handles": [2, 1]}));
        let mut picks = json!({"message": "q", "targets": [{"handle": base + 3, "id": 2}]});
        lower(&mut picks, base);
        assert_eq!(picks, json!({"message": "q", "targets": [{"handle": 3, "id": 2}]}));
    }

    /// The order goes only to a host that is strictly older, by number — not
    /// by string, where "0.9.0" > "0.10.0".
    #[test]
    fn versions_compare_by_number() {
        assert!(version_of("0.9.0") < version_of("0.10.0"));
        assert_eq!(version_of("1.2.3"), Some((1, 2, 3)));
        assert_eq!(version_of("1.2"), None);
        assert_eq!(version_of("1.2.3.4"), None);
        assert_eq!(version_of("1.2.x"), None);
        assert_eq!(version_of("?"), None);
    }

    #[test]
    fn a_pairing_link_parses() {
        let k = b64(&[7u8; 32]);
        let (relay, host, _, secret) = parse_link(&format!("https://r.example/#p=abc-1.{k}.{k}")).unwrap();
        assert_eq!((relay.as_str(), host.as_str(), secret), ("https://r.example", "abc-1", [7u8; 32]));
        assert!(parse_link("https://r.example/").is_err());
        assert!(parse_link(&format!("https://r.example/#p=abc.{k}")).is_err());
    }

    /// The whole path for real: the relay binary, this module as the remote
    /// desktop, and the host side (`desktop.rs`, over a stand-in session) in one
    /// process — pairing from the link, a denied command, a mirrored session,
    /// typing, the geometry lease, an upload, and the keystroke echo time.
    /// `MULPEX_TEST_RELAY_BIN=target/debug/mulpex-relay cargo test -p mulpex
    /// --lib live_through_a_real_relay -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_through_a_real_relay() {
        use super::super::tests::{FAKE_GEO, FAKE_SINK, SERIAL};
        use std::sync::Arc;
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let bin = std::env::var("MULPEX_TEST_RELAY_BIN").expect("MULPEX_TEST_RELAY_BIN");
        let home = std::env::temp_dir().join(format!("mulpex-live-desk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("relay")).unwrap();
        std::env::set_var("MULPEX_HOME", &home);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
            }
        }
        let _relay = Kill(
            std::process::Command::new(bin)
                .args(["--listen", &format!("127.0.0.1:{port}"), "--static", "/nonexistent"])
                .args(["--data", &home.join("relay").display().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));

        *FAKE_GEO.lock().unwrap() = Some(((100, 30), None));
        let sink = Arc::new(crate::pty::OutputSink::new(false, (100, 30)));
        sink.push(b"hello");
        *FAKE_SINK.lock().unwrap() = Some(sink.clone());
        super::super::set_relay_url(&format!("http://127.0.0.1:{port}")).unwrap();
        super::super::set_enabled(true);
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !f() {
                assert!(t.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait("the host to reach the relay", &|| super::super::remote_status().state == "connected");

        let link = super::super::remote_status().pair_url;
        let st = pair(&link).expect("pairing");
        let slot = st.slot;
        wait("welcome", &|| inner().lock().unwrap().conns.get(&slot).is_some_and(|c| c.state == "connected"));
        assert!(pair(&link).is_err(), "a link pairs once");

        let denied = request(slot, json!({"t": "rpc", "cmd": "quit_app", "args": {}}));
        assert!(denied.unwrap_err().contains("can't be run"));

        let frames: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_frames = frames.clone();
        let ch: Channel<Value> = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(j) = body {
                sink_frames.lock().unwrap().push(serde_json::from_str(&j).unwrap());
            }
            Ok(())
        });
        host_attach(slot, base_of(slot) + 1, 1, ch);
        let text = |frames: &Arc<Mutex<Vec<Value>>>| -> String {
            frames
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f["t"] == "o" || f["t"] == "snap")
                .map(|f| {
                    use base64::Engine;
                    let b = base64::engine::general_purpose::STANDARD.decode(f["d"].as_str().unwrap()).unwrap();
                    String::from_utf8_lossy(&b).into_owned()
                })
                .collect()
        };
        wait("the snapshot", &|| text(&frames) == "hello");
        assert_eq!(frames.lock().unwrap()[0]["t"], "snap");

        let mut times = Vec::new();
        for i in 0..50 {
            let t = Instant::now();
            host_input(slot, base_of(slot) + 1, 1, format!("k{i}").into_bytes());
            let want = format!("<k{i}>");
            wait("an echo", &|| text(&frames).ends_with(&want));
            times.push(t.elapsed());
        }
        times.sort();
        println!("echo through the relay: p50 {:?}, p95 {:?}, max {:?}", times[25], times[47], times[49]);

        host_geo(slot, 140, 45);
        wait("the lease", &|| FAKE_GEO.lock().unwrap().as_ref().unwrap().0 == (140, 45));
        wait("geo back", &|| {
            inner().lock().unwrap().conns.get(&slot).and_then(|c| c.geo.clone()).is_some_and(|g| g["mine"] == true)
        });

        let up = std::env::temp_dir().join(format!("mulpex-up-src-{}", std::process::id()));
        std::fs::create_dir_all(up.join("d")).unwrap();
        std::fs::write(up.join("d/x.txt"), vec![b'z'; 600 * 1024]).unwrap();
        let there = upload(slot, &[up.join("d").display().to_string()]).expect("upload");
        assert_eq!(there.len(), 1);
        assert!(there[0].ends_with("/d"));
        assert_eq!(std::fs::read(std::path::Path::new(&there[0]).join("x.txt")).unwrap().len(), 600 * 1024);

        host_remove(slot);
        super::super::set_enabled(false);
        *FAKE_GEO.lock().unwrap() = None;
        *FAKE_SINK.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&up);
    }

    #[test]
    fn a_folder_uploads_under_its_own_name() {
        let dir = std::env::temp_dir().join(format!("mulpex-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shots/sub")).unwrap();
        std::fs::write(dir.join("shots/a.png"), b"a").unwrap();
        std::fs::write(dir.join("shots/sub/b.txt"), b"bb").unwrap();
        std::fs::write(dir.join("one.md"), b"c").unwrap();
        let (files, total) = upload_list(&[
            dir.join("shots").display().to_string(),
            dir.join("one.md").display().to_string(),
        ])
        .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.1.as_str()).collect();
        assert_eq!(rels, vec!["shots/a.png", "shots/sub/b.txt", "one.md"]);
        assert_eq!(total, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
