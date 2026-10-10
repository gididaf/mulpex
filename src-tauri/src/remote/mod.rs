//! Remote Control (⌘⇧O): drive this Mulpex from a phone.
//!
//! The Mac connects OUT to a relay (`crates/mulpex-relay`) — it sits behind NAT,
//! so nothing can connect in — and the phone's web app connects to the same
//! relay. This side owns the connection: a plain thread with a blocking
//! websocket, like the rest of the backend, which polls a short read timeout so
//! it can also push whatever the hub loop published since the last pass.
//!
//! **Nothing readable crosses the relay.** A phone pairs once by scanning the
//! QR code, then every connection runs a handshake and every frame after it is
//! sealed (`crypto.rs`). The relay sees who talks to whom and how much, never
//! what. The only frames in the clear are the pairing and handshake messages,
//! which carry public keys and a proof, and refusals.
//!
//! What the phone sees is a *view*: every open project with its sidebar rows and
//! their status, built by `hub.rs` on each tick (`publish`) and sent only when it
//! changes. A phone that finishes its handshake and says `hello` gets the
//! current view at once.
//!
//! On/off survives a restart (`Config::enabled`): turned on and quit, Mulpex
//! comes back on — the user asked for that, because the point is being away
//! from the Mac. It is still a door into a machine whose claudes run with
//! `--dangerously-skip-permissions`, which is why the top bar shows it whenever
//! it is open.

mod chat;
mod crypto;
mod dialog;
mod push;

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::snapshot::{HubSnapshot, ProjectHandle, SessionInfo, SessionKind};
use crypto::{b64, Channel, Key32};

/// The hosted relay. A different one (a local `mulpex-relay` while
/// developing it) is set in the dialog.
const DEFAULT_RELAY: &str = "https://mulpex.dreamvps.com";
/// How long one read waits before the loop goes round to send.
const READ_TICK: Duration = Duration::from_millis(100);
/// Keeps an idle connection from being dropped by anything in between.
const PING_EVERY: Duration = Duration::from_secs(20);
/// How long a pairing QR code stays good. One use, then a fresh one.
const PAIR_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    /// Names this Mac's room on the relay. Phones join it by id.
    host_id: String,
    /// Where the relay lives, as the phone opens it: `http(s)://host[:port]`.
    relay_url: String,
    /// This Mac's long-lived X25519 key (base64url). Its public half is in the QR.
    #[serde(default)]
    host_secret: String,
    /// Proves to the relay that this Mac owns `host_id` (base64url). The relay
    /// keeps only a hash of it, and refuses anyone else claiming the room.
    #[serde(default)]
    relay_token: String,
    /// The Mac's P-256 key for signing Web Push (VAPID, `push.rs`), base64url.
    #[serde(default)]
    vapid_secret: String,
    /// Remote control was on when last left; turned back on at launch.
    #[serde(default)]
    enabled: bool,
}

/// A paired phone. Only its public key is kept.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// base64url X25519 public key.
    pub public: String,
    /// Unix seconds.
    pub added: u64,
    /// Where to send this phone notifications, once it allowed them.
    #[serde(default)]
    pub push: Option<push::Subscription>,
}

#[derive(Clone, Copy, PartialEq)]
enum Conn {
    Off,
    Connecting,
    Connected,
    Error,
}

impl Conn {
    fn word(self) -> &'static str {
        match self {
            Conn::Off => "off",
            Conn::Connecting => "connecting",
            Conn::Connected => "connected",
            Conn::Error => "error",
        }
    }
}

#[derive(Clone, Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    pub added: u64,
    /// Has a live, handshaken session right now.
    pub online: bool,
    /// Gets notifications.
    pub push: bool,
}

/// What the Remote Control dialog shows (`remote-changed` carries it too).
#[derive(Clone, Serialize)]
pub struct RemoteStatus {
    pub enabled: bool,
    pub relay_url: String,
    /// `off` / `connecting` / `connected` / `error`.
    pub state: String,
    /// The last connection failure, while `state` is `error`.
    pub error: Option<String>,
    /// Phones connected (handshaken) right now.
    pub clients: usize,
    /// What a NEW phone opens to pair; the QR code encodes exactly this. Empty
    /// while remote control is off.
    pub pair_url: String,
    pub qr_svg: String,
    pub devices: Vec<DeviceStatus>,
}

struct Pairing {
    secret: Key32,
    expires: Instant,
}

struct Inner {
    cfg: Config,
    devices: Vec<Device>,
    enabled: bool,
    /// Bumped on every on/off or URL change. A connection thread whose `epoch`
    /// is stale stops at its next pass, so at most one ever talks to the relay.
    epoch: u64,
    conn: Conn,
    error: Option<String>,
    /// Device ids with a live session, as the connection thread last reported.
    online: Vec<String>,
    pairing: Option<Pairing>,
    /// The last view `publish`ed, as the JSON sent, and a counter the connection
    /// compares against what it last sent.
    view: Option<String>,
    view_seq: u64,
    /// Typing requests handed to the desktop frontend, by request id, so its
    /// answer can find the phone that asked: (epoch, cid).
    pending: HashMap<String, (u64, u64)>,
    /// Sealed replies waiting for the connection thread: (epoch, cid, json).
    outbox: Vec<(u64, u64, String)>,
    app: Option<AppHandle>,
    /// Each claude's status in the last view ("handle:id" → word), to notice
    /// a change worth a notification. `None` until the first view after turning
    /// on, so whatever was already waiting then doesn't buzz the phone.
    last_status: Option<HashMap<String, String>>,
    /// What the phones' one summary notification last said (`summary_for`).
    summary: Summary,
    /// (device, handle, id): that phone has that claude's chat open with the
    /// app in front — no notification for it, they're looking at it.
    viewing: std::collections::HashSet<(String, u64, usize)>,
    /// `caffeinate`, holding off idle sleep while remote control is on.
    awake: Option<std::process::Child>,
}

static REMOTE: OnceLock<Mutex<Inner>> = OnceLock::new();

fn inner() -> &'static Mutex<Inner> {
    REMOTE.get_or_init(|| {
        Mutex::new(Inner {
            cfg: load_config(),
            devices: load_devices(),
            enabled: false,
            epoch: 0,
            conn: Conn::Off,
            error: None,
            online: Vec::new(),
            pairing: None,
            view: None,
            view_seq: 0,
            pending: HashMap::new(),
            outbox: Vec::new(),
            last_status: None,
            summary: Summary::default(),
            viewing: Default::default(),
            awake: None,
            app: None,
        })
    })
}

fn remote_dir() -> PathBuf {
    mulpex_core::mulpex_home().join("remote")
}

fn write_json(name: &str, value: &impl Serialize) {
    let dir = remote_dir();
    let _ = crate::secrets::private_dir(&dir);
    if let Ok(body) = serde_json::to_string_pretty(value) {
        let _ = crate::secrets::write_private(&dir.join(name), &body);
    }
}

/// Read the config, creating what's missing (a host id, keys, the hosted
/// relay) the first time.
fn load_config() -> Config {
    let read = std::fs::read_to_string(remote_dir().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok());
    let mut cfg = read.clone().unwrap_or_else(|| Config {
        host_id: mulpex_core::persist::new_uuid(),
        relay_url: DEFAULT_RELAY.into(),
        host_secret: String::new(),
        relay_token: String::new(),
        vapid_secret: String::new(),
        enabled: false,
    });
    let (vapid, fresh) = push::vapid_key(&cfg.vapid_secret);
    if fresh {
        cfg.vapid_secret = push::vapid_secret_b64(&vapid);
    }
    if crypto::unb64_32(&cfg.host_secret).is_none() {
        cfg.host_secret = b64(&crypto::random32());
    }
    if cfg.relay_token.is_empty() {
        cfg.relay_token = b64(&crypto::random32());
    }
    if read.map(|r| {
        r.host_secret != cfg.host_secret || r.relay_token != cfg.relay_token || r.vapid_secret != cfg.vapid_secret
    }) != Some(false)
    {
        write_json("config.json", &cfg);
    }
    cfg
}

fn load_devices() -> Vec<Device> {
    std::fs::read_to_string(remote_dir().join("devices.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn host_secret(cfg: &Config) -> Key32 {
    crypto::unb64_32(&cfg.host_secret).expect("load_config guarantees a valid key")
}

/// The pairing link. Everything a phone needs rides in the **fragment** (after
/// `#`), which a browser never sends to a server: the relay serves the page
/// without ever seeing the one-time secret or which Mac it belongs to.
fn pair_url(cfg: &Config, secret: &Key32) -> String {
    format!(
        "{}/#p={}.{}.{}",
        cfg.relay_url,
        cfg.host_id,
        b64(&crypto::public_of(&host_secret(cfg))),
        b64(secret)
    )
}

fn ws_url(cfg: &Config) -> String {
    let base = if let Some(rest) = cfg.relay_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = cfg.relay_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        cfg.relay_url.clone()
    };
    format!("{base}/ws/host?id={}&token={}", cfg.host_id, cfg.relay_token)
}

fn qr_svg(text: &str) -> String {
    use qrcode::render::svg;
    qrcode::QrCode::new(text.as_bytes())
        .map(|c| {
            c.render::<svg::Color>()
                .min_dimensions(220, 220)
                .dark_color(svg::Color("#000000"))
                .light_color(svg::Color("#ffffff"))
                .build()
        })
        .unwrap_or_default()
}

/// A fresh pairing whenever there is none or it has expired, while on.
fn ensure_pairing(g: &mut Inner) {
    if !g.enabled {
        g.pairing = None;
        return;
    }
    if g.pairing.as_ref().map_or(true, |p| p.expires <= Instant::now()) {
        g.pairing = Some(Pairing { secret: crypto::random32(), expires: Instant::now() + PAIR_TTL });
    }
}

fn status_of(g: &mut Inner) -> RemoteStatus {
    ensure_pairing(g);
    let url = g.pairing.as_ref().map(|p| pair_url(&g.cfg, &p.secret)).unwrap_or_default();
    RemoteStatus {
        enabled: g.enabled,
        relay_url: g.cfg.relay_url.clone(),
        state: g.conn.word().into(),
        error: g.error.clone(),
        clients: g.online.len(),
        qr_svg: if url.is_empty() { String::new() } else { qr_svg(&url) },
        pair_url: url,
        devices: g
            .devices
            .iter()
            .map(|d| DeviceStatus {
                id: d.id.clone(),
                name: d.name.clone(),
                added: d.added,
                online: g.online.contains(&d.id),
                push: d.push.is_some(),
            })
            .collect(),
    }
}

/// Tell the frontend. Built under the lock, emitted outside it.
fn emit_changed() {
    let (app, st) = {
        let mut g = inner().lock().unwrap();
        (g.app.clone(), status_of(&mut g))
    };
    if let Some(app) = app {
        let _ = app.emit("remote-changed", st);
    }
}

pub fn init(app: AppHandle) {
    let was_on = {
        let mut g = inner().lock().unwrap();
        g.app = Some(app);
        g.cfg.enabled
    };
    if was_on {
        set_enabled(true);
    }
}

/// Cheap enough for every hub tick: building a view is skipped while off.
pub fn is_enabled() -> bool {
    inner().lock().unwrap().enabled
}

/// The hub loop's latest picture of the workspace. Sent to the phones only when
/// it differs from the last one.
pub fn publish(view: Value) {
    let mut g = inner().lock().unwrap();
    if !g.enabled {
        return;
    }
    let s = view.to_string();
    if g.view.as_deref() != Some(s.as_str()) {
        g.view = Some(s);
        g.view_seq += 1;
    }
    let prev = g.last_status.take();
    let (statuses, note) = summary_for(&view, prev.as_ref(), &mut g.summary);
    g.last_status = Some(statuses);
    if let Some(note) = note {
        notify(&g, note);
    }
}

/// A phone opened the app: everything "done" has now been seen.
fn seen_done() {
    inner().lock().unwrap().summary.unseen.clear();
}

/// The one notification the phones keep: a summary of the claudes that need
/// you, and of those that finished since you last opened the app.
#[derive(Default)]
struct Summary {
    /// "handle:id" of claudes that went working → done and haven't been seen
    /// yet, newest first. Cleared when a phone opens the app (`seen_done`); a
    /// claude leaves it as soon as it's no longer done (you typed to it).
    unseen: Vec<String>,
    /// (needs, unseen) as last sent, so only a change sends again.
    last: (Vec<String>, Vec<String>),
}

/// How many claude lines the summary lists before "+N more".
const SUMMARY_LINES: usize = 6;

/// Every claude's status in `view`, and the summary notification to send, if
/// it changed. Counted: claudes the user started (no `parent`: a hub-spawned
/// worker reports to its spawner, not to the phone), never a muted one.
/// - **needs you** — every such claude on `needs` right now;
/// - **done** — those that went from working to finished and weren't seen yet.
/// It buzzes (`alert`) only when one is new on either list; a smaller number
/// is a silent edit. Nothing is sent when both reach zero — every push must
/// show a notification, or Android may show its own; the app clears the bar
/// when opened. Nothing at all on the first view (`prev` is `None`).
fn summary_for(
    view: &Value,
    prev: Option<&HashMap<String, String>>,
    sum: &mut Summary,
) -> (HashMap<String, String>, Option<Value>) {
    let mut now = HashMap::new();
    // key → (project, id, what), for the claudes that count.
    let mut counted: HashMap<String, (String, u64, String)> = HashMap::new();
    let mut needs = Vec::new();
    let mut alert = false;
    for p in view["projects"].as_array().into_iter().flatten() {
        for s in p["sessions"].as_array().into_iter().flatten() {
            let Some(status) = s["status"].as_str() else { continue };
            let key = format!("{}:{}", p["handle"], s["id"]);
            let before = prev.and_then(|m| m.get(&key)).map(String::as_str);
            now.insert(key.clone(), status.to_string());
            if s["muted"] == true || !s["parent"].is_null() {
                continue;
            }
            let what = s["name"].as_str().or(s["task"].as_str()).unwrap_or("").chars().take(80).collect();
            counted.insert(key.clone(), (p["name"].as_str().unwrap_or("").to_string(), s["id"].as_u64().unwrap_or(0), what));
            if prev.is_none() {
                continue;
            }
            if status == "needs" {
                alert |= before.is_some_and(|b| b != "needs");
                needs.push(key.clone());
            }
            if before == Some("working") && status == "waiting" && !sum.unseen.contains(&key) {
                sum.unseen.insert(0, key.clone());
                alert = true;
            }
        }
    }
    if prev.is_none() {
        sum.unseen.clear();
        return (now, None);
    }
    sum.unseen.retain(|k| counted.contains_key(k) && now.get(k).map(String::as_str) == Some("waiting"));
    let sig = (needs.clone(), sum.unseen.clone());
    if sig == sum.last {
        return (now, None);
    }
    sum.last = sig;
    if needs.is_empty() && sum.unseen.is_empty() {
        return (now, None);
    }
    let line = |mark: &str, k: &String| {
        let (project, id, what) = &counted[k];
        let what = if what.is_empty() { String::new() } else { format!(" · {what}") };
        format!("{mark} {project} · claude#{id}{what}")
    };
    let mut lines: Vec<String> =
        needs.iter().map(|k| line("●", k)).chain(sum.unseen.iter().map(|k| line("✓", k))).collect();
    if lines.len() > SUMMARY_LINES {
        let more = lines.len() - (SUMMARY_LINES - 1);
        lines.truncate(SUMMARY_LINES - 1);
        lines.push(format!("+{more} more"));
    }
    let mut title = Vec::new();
    if !needs.is_empty() {
        title.push(format!("{} need{} you", needs.len(), if needs.len() == 1 { "s" } else { "" }));
    }
    if !sum.unseen.is_empty() {
        title.push(format!("{} done", sum.unseen.len()));
    }
    let note = json!({
        "summary": true,
        "title": title.join(" · "),
        "body": lines.join("\n"),
        "alert": alert,
    });
    (now, Some(note))
}

/// Push the summary to every phone that allowed notifications, off the calling
/// thread. Not to a phone with a chat open in front — it's looking (the service
/// worker also skips any push while the app is in front). A subscription the
/// push service says is gone is forgotten.
fn notify(g: &Inner, note: Value) {
    let targets: Vec<(String, push::Subscription)> = g
        .devices
        .iter()
        .filter(|d| !g.viewing.iter().any(|(dev, _, _)| dev == &d.id))
        .filter_map(|d| d.push.clone().map(|p| (d.id.clone(), p)))
        .collect();
    if targets.is_empty() {
        return;
    }
    let (key, _) = push::vapid_key(&g.cfg.vapid_secret);
    std::thread::spawn(move || {
        for (device, sub) in &targets {
            let sent = push::send(&key, sub, note.to_string().as_bytes());
            if let push::Sent::Failed(why) = &sent {
                eprintln!("mulpex: push to a phone failed: {why}");
            }
            if sent == push::Sent::Gone {
                let mut g = inner().lock().unwrap();
                if let Some(d) = g.devices.iter_mut().find(|d| &d.id == device) {
                    d.push = None;
                }
                write_json("devices.json", &g.devices);
            }
        }
    });
}

/// One project as the phone draws it: its tab, and its sidebar rows.
pub fn project_view(
    handle: ProjectHandle,
    name: &str,
    state_dir: &std::path::Path,
    sessions: &[SessionInfo],
    snap: &HubSnapshot,
) -> Value {
    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let st = snap.statuses.iter().find(|e| e.id == s.id);
            let claude = s.kind == SessionKind::Claude;
            // The open question or plan, while the claude is waiting on it.
            let needs = claude && st.is_some_and(|e| e.status == crate::snapshot::Status::Needs);
            let dialog = if needs { dialog::read(state_dir, s.id) } else { None };
            json!({
                "dialog": dialog,
                "id": s.id,
                "kind": if claude { "claude" } else { "shell" },
                "name": s.name,
                "task": snap.tasks.iter().find(|t| t.id == s.id).map(|t| t.task.clone()),
                // A terminal has no status, and a failed claude is absent from
                // `statuses` on purpose — never default either of them to one.
                "status": if claude && s.failed.is_none() {
                    st.map(|e| e.status.word())
                } else {
                    None
                },
                "ctx_pct": st.and_then(|e| e.ctx_pct),
                "muted": s.muted,
                "parent": s.parent,
                "exited": s.exited,
                "failed": s.failed,
            })
        })
        .collect();
    json!({ "handle": handle, "name": name, "sessions": rows })
}

pub fn workspace_view(active: Option<ProjectHandle>, projects: Vec<Value>) -> Value {
    json!({ "t": "state", "active": active, "projects": projects })
}

fn start_thread(g: &mut Inner) {
    g.epoch += 1;
    g.conn = Conn::Connecting;
    g.online.clear();
    g.error = None;
    let epoch = g.epoch;
    std::thread::spawn(move || run(epoch));
}

/// Hold off idle sleep while remote control is on: a sleeping Mac answers no
/// phone. `-i` only — the display still sleeps, and closing the lid still
/// sleeps the Mac. `-w` ties it to this process, so it can't outlive Mulpex.
fn keep_awake() -> Option<std::process::Child> {
    std::process::Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

/// Turn remote control on or off.
pub fn set_enabled(on: bool) {
    {
        let mut g = inner().lock().unwrap();
        if on == g.enabled {
            return;
        }
        g.enabled = on;
        g.view = None;
        g.last_status = None;
        g.summary = Summary::default();
        if g.cfg.enabled != on {
            g.cfg.enabled = on;
            write_json("config.json", &g.cfg);
        }
        if on {
            start_thread(&mut g);
            g.awake = keep_awake();
        } else {
            g.epoch += 1;
            g.conn = Conn::Off;
            g.online.clear();
            g.error = None;
            if let Some(mut c) = g.awake.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
    emit_changed();
}

pub fn set_relay_url(url: &str) -> Result<(), String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("The relay address must start with http:// or https://".into());
    }
    {
        let mut g = inner().lock().unwrap();
        if g.cfg.relay_url == url {
            return Ok(());
        }
        g.cfg.relay_url = url;
        write_json("config.json", &g.cfg);
        if g.enabled {
            start_thread(&mut g);
        }
    }
    emit_changed();
    Ok(())
}

/// Forget a phone. Its live session, if any, is cut on the connection's next
/// pass; it can only come back by scanning a new QR code.
pub fn revoke(device: &str) {
    {
        let mut g = inner().lock().unwrap();
        g.devices.retain(|d| d.id != device);
        write_json("devices.json", &g.devices);
    }
    emit_changed();
}

/// Update the connection readout, unless this thread has been superseded.
fn set_conn(epoch: u64, conn: Conn, error: Option<String>) {
    {
        let mut g = inner().lock().unwrap();
        if g.epoch != epoch {
            return;
        }
        g.conn = conn;
        g.error = error;
        g.online.clear();
    }
    emit_changed();
}

fn current(epoch: u64) -> bool {
    inner().lock().unwrap().epoch == epoch
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// The connection thread: connect, serve, and reconnect with backoff until
/// remote control is turned off or re-pointed.
fn run(epoch: u64) {
    let mut backoff = 1u64;
    loop {
        let url = {
            let g = inner().lock().unwrap();
            if g.epoch != epoch {
                return;
            }
            ws_url(&g.cfg)
        };
        match tungstenite::connect(url.as_str()) {
            Ok((mut ws, _)) => {
                set_read_timeout(&mut ws);
                set_conn(epoch, Conn::Connected, None);
                backoff = 1;
                let res = Server::new(epoch).serve(&mut ws);
                let _ = ws.close(None);
                let _ = ws.flush();
                match res {
                    Ok(()) => return,
                    Err(e) => set_conn(epoch, Conn::Connecting, Some(e)),
                }
            }
            Err(e) => set_conn(epoch, Conn::Error, Some(connect_error(&e))),
        }
        for _ in 0..backoff * 10 {
            if !current(epoch) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(10);
    }
}

/// A refused room claim is the one failure the user can't diagnose from the
/// raw error, so say what it means.
fn connect_error(e: &tungstenite::Error) -> String {
    if let tungstenite::Error::Http(resp) = e {
        if resp.status().as_u16() == 403 {
            return "The relay refused this Mac's id (another Mac already owns it)".into();
        }
    }
    e.to_string()
}

fn set_read_timeout(ws: &mut Ws) {
    let t = Some(READ_TICK);
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(t);
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.get_mut().set_read_timeout(t);
        }
        _ => {}
    }
}

/// `data` is already JSON. Every frame goes to one phone: anything readable is
/// sealed per session, so there is nothing to broadcast.
fn send(ws: &mut Ws, to: u64, data: &Value) -> Result<(), String> {
    let text = json!({ "to": to, "data": data }).to_string();
    ws.send(Message::Text(text.into())).map_err(|e| e.to_string())
}

/// A handshaken phone on this connection.
struct Session {
    device: String,
    ch: Channel,
    /// The claude whose chat this phone has open, if any.
    watch: Option<Watch>,
    /// The terminal this phone has open, if any.
    term: Option<TermWatch>,
    /// The app is in front on the phone (it says so on every change).
    visible: bool,
}

/// One phone watching one shell terminal's output.
struct TermWatch {
    handle: ProjectHandle,
    id: usize,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
}

/// The most terminal output sent in one frame; the rest follows next pass.
const TERM_CHUNK: usize = 256 * 1024;

/// Terminal `id` of project `handle`: its recent output and a live tap, plus
/// the one geometry every PTY runs at (the phone's xterm must match it).
fn tap_terminal(handle: ProjectHandle, id: usize) -> Option<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>, (u16, u16))> {
    use tauri::Manager;
    #[cfg(test)]
    if tests::FAKE_TERM.lock().unwrap().0 {
        let (tx, rx) = std::sync::mpsc::channel();
        tests::FAKE_TERM.lock().unwrap().1 = Some(tx);
        return Some((b"\x1b[32mfake$\x1b[0m ".to_vec(), rx, (100, 30)));
    }
    let app = inner().lock().unwrap().app.clone()?;
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    let s = ws.project(handle)?.sessions.iter().find(|s| s.id == id)?;
    let (tail, rx) = s.tap()?;
    Some((tail, rx, ws.geometry))
}

fn geometry() -> (u16, u16) {
    use tauri::Manager;
    #[cfg(test)]
    if tests::FAKE_TERM.lock().unwrap().0 {
        return (100, 30);
    }
    let Some(app) = inner().lock().unwrap().app.clone() else { return (0, 0) };
    let state = app.state::<crate::state::AppState>();
    let g = state.ws.lock().unwrap().geometry;
    g
}

/// Type into a shell terminal. Never a claude: what reaches a claude goes
/// through the desktop's prompt checks (`on_type`).
fn write_terminal(handle: ProjectHandle, id: usize, bytes: &[u8]) -> bool {
    use tauri::Manager;
    #[cfg(test)]
    if let Some(tx) = &tests::FAKE_TERM.lock().unwrap().1 {
        // The fake shell echoes what it is sent.
        let _ = tx.send(format!("\r\nyou typed: {}\r\nfake$ ", String::from_utf8_lossy(bytes).trim_end()).into_bytes());
        return true;
    }
    let Some(app) = inner().lock().unwrap().app.clone() else { return false };
    let state = app.state::<crate::state::AppState>();
    let mut ws = state.ws.lock().unwrap();
    let Some(s) = ws.project_mut(handle).and_then(|c| c.session_mut(id)) else { return false };
    if s.kind != crate::pty::SessionKind::Shell {
        return false;
    }
    s.send(bytes);
    true
}

/// One phone following one claude's transcript.
struct Watch {
    handle: ProjectHandle,
    id: usize,
    tail: Option<chat::Tail>,
    /// When the transcript's path was last looked up. It can change under a
    /// running claude (an in-TUI `/resume`), so it is looked up again now and
    /// then rather than once.
    resolved: Instant,
    /// Told the phone this claude is gone; don't repeat it every pass.
    gone: bool,
}

/// The most a message is typed into a claude's input box. Measured on claude
/// 2.1.292: a bracketed paste of 939 characters arrived exact; the old
/// truncation was at 1022. Longer messages go through a file.
const MAX_TYPED: usize = 900;
/// The most a phone may send at all.
const MAX_MESSAGE: usize = 200_000;

/// Where a long phone message is kept for the claude to read: the user's own
/// words, so private (0600), and outside any repo.
fn write_long_message(text: &str) -> Result<PathBuf, String> {
    let dir = remote_dir().join("inbox");
    crate::secrets::private_dir(&dir)?;
    let path = dir.join(format!("{}.md", mulpex_core::persist::new_uuid()));
    crate::secrets::write_private(&path, text)?;
    Ok(path)
}

/// The desktop frontend's answer to a `remote-type`: queue it for the phone
/// that asked.
pub fn reply(rid: &str, ok: bool, why: Option<String>, id: Option<u64>) {
    let mut g = inner().lock().unwrap();
    if let Some((epoch, cid)) = g.pending.remove(rid) {
        let msg = json!({"t": "typed", "rid": rid, "ok": ok, "why": why, "id": id}).to_string();
        g.outbox.push((epoch, cid, msg));
    }
}

/// How often a followed claude's transcript path is looked up again.
const RESOLVE_EVERY: Duration = Duration::from_secs(2);

/// Project `handle`'s scratch dir, where the hooks leave each claude's state.
fn state_dir_of(handle: ProjectHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let app = inner().lock().unwrap().app.clone()?;
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    Some(ws.project(handle)?.state_dir.clone())
}

/// Where claude `id` of project `handle` writes its transcript, if it is a
/// live claude.
fn transcript_of(handle: ProjectHandle, id: usize) -> Option<PathBuf> {
    use tauri::Manager;
    #[cfg(test)]
    if let Some(p) = tests::TRANSCRIPT.lock().unwrap().clone() {
        return Some(p);
    }
    let app = inner().lock().unwrap().app.clone()?;
    let state = app.state::<crate::state::AppState>();
    let ws = state.ws.lock().unwrap();
    let core = ws.project(handle)?;
    let s = core.sessions.iter().find(|s| s.id == id && s.kind == crate::pty::SessionKind::Claude)?;
    Some(crate::saves::transcript_path(&core.project_dir, &s.session_id))
}

/// One relay connection's state.
struct Server {
    epoch: u64,
    sessions: HashMap<u64, Session>,
    sent_seq: u64,
}

impl Server {
    fn new(epoch: u64) -> Self {
        Server { epoch, sessions: HashMap::new(), sent_seq: 0 }
    }

    /// Serve one connection. `Ok` means stop for good (turned off or
    /// re-pointed); `Err` means the connection broke and the caller should
    /// reconnect.
    fn serve(&mut self, ws: &mut Ws) -> Result<(), String> {
        let mut last_ping = Instant::now();
        loop {
            let (seq, view, devices) = {
                let g = inner().lock().unwrap();
                if g.epoch != self.epoch {
                    return Ok(());
                }
                let ids: Vec<String> = g.devices.iter().map(|d| d.id.clone()).collect();
                (g.view_seq, g.view.clone(), ids)
            };
            // A revoked phone loses its session here, mid-connection.
            let revoked: Vec<u64> = self
                .sessions
                .iter()
                .filter(|(_, s)| !devices.contains(&s.device))
                .map(|(c, _)| *c)
                .collect();
            for cid in revoked {
                self.sessions.remove(&cid);
                send(ws, cid, &json!({"t": "denied", "why": "revoked"}))?;
                self.report();
            }
            if seq != self.sent_seq {
                if let Some(v) = &view {
                    let cids: Vec<u64> = self.sessions.keys().copied().collect();
                    for cid in cids {
                        self.send_sealed(ws, cid, v)?;
                    }
                }
                self.sent_seq = seq;
            }
            let watching: Vec<u64> = self
                .sessions
                .iter()
                .filter(|(_, s)| s.watch.is_some())
                .map(|(c, _)| *c)
                .collect();
            for cid in watching {
                self.poll_chat(ws, cid)?;
            }
            self.pump_terminals(ws)?;
            self.sync_viewing();
            let replies: Vec<(u64, String)> = {
                let mut g = inner().lock().unwrap();
                let epoch = self.epoch;
                let mine = g.outbox.iter().filter(|(e, ..)| *e == epoch).map(|(_, c, m)| (*c, m.clone())).collect();
                g.outbox.clear();
                mine
            };
            for (cid, msg) in replies {
                self.send_sealed(ws, cid, &msg)?;
            }
            if last_ping.elapsed() >= PING_EVERY {
                ws.send(Message::Ping(Default::default()))
                    .map_err(|e| e.to_string())?;
                last_ping = Instant::now();
            }
            match ws.read() {
                Ok(Message::Text(t)) => self.on_relay(ws, t.as_str())?,
                Ok(Message::Close(_)) => return Err("the relay closed the connection".into()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn send_sealed(&mut self, ws: &mut Ws, cid: u64, plain: &str) -> Result<(), String> {
        let Some(s) = self.sessions.get_mut(&cid) else { return Ok(()) };
        let c = s.ch.seal(plain.as_bytes());
        send(ws, cid, &json!({"t": "sealed", "c": c}))
    }

    /// Publish which devices are online, if that changed.
    /// Which claude each phone is looking at right now (chat open, app in
    /// front), for `notify` to leave alone.
    fn sync_viewing(&self) {
        let now: std::collections::HashSet<(String, u64, usize)> = self
            .sessions
            .values()
            .filter(|s| s.visible)
            .filter_map(|s| s.watch.as_ref().map(|w| (s.device.clone(), w.handle, w.id)))
            .collect();
        let mut g = inner().lock().unwrap();
        if g.epoch == self.epoch && g.viewing != now {
            g.viewing = now;
        }
    }

    fn report(&self) {
        let mut online: Vec<String> = self.sessions.values().map(|s| s.device.clone()).collect();
        online.sort();
        online.dedup();
        let changed = {
            let mut g = inner().lock().unwrap();
            if g.epoch != self.epoch || g.online == online {
                false
            } else {
                g.online = online;
                true
            }
        };
        if changed {
            emit_changed();
        }
    }

    /// One frame from the relay: a phone left, or sent something.
    fn on_relay(&mut self, ws: &mut Ws, text: &str) -> Result<(), String> {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return Ok(()) };
        if msg.get("event").and_then(Value::as_str) == Some("leave") {
            if let Some(cid) = msg.get("cid").and_then(Value::as_u64) {
                if self.sessions.remove(&cid).is_some() {
                    self.report();
                }
            }
            return Ok(());
        }
        let (Some(cid), Some(data)) = (msg.get("from").and_then(Value::as_u64), msg.get("data"))
        else {
            return Ok(());
        };
        let reply = match data.get("t").and_then(Value::as_str) {
            Some("pair") => Some(on_pair(data)),
            Some("hs") => Some(self.on_handshake(cid, data)),
            Some("sealed") => return self.on_sealed(ws, cid, data),
            _ => None,
        };
        if let Some(r) = reply {
            send(ws, cid, &r)?;
        }
        Ok(())
    }

    fn on_handshake(&mut self, cid: u64, data: &Value) -> Value {
        let str_of = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or("");
        let device = str_of("dev").to_string();
        let (secret, known) = {
            let g = inner().lock().unwrap();
            let known = g.devices.iter().find(|d| d.id == device).map(|d| d.public.clone());
            (host_secret(&g.cfg), known)
        };
        let Some(device_pub) = known.as_deref().and_then(crypto::unb64_32) else {
            return json!({"t": "denied", "why": "not paired"});
        };
        let Some(eph) = crypto::unb64_32(str_of("ep")) else {
            return json!({"t": "denied", "why": "bad handshake"});
        };
        let Some((eh, ch)) = crypto::host_handshake(&secret, &device_pub, &eph) else {
            return json!({"t": "denied", "why": "bad handshake"});
        };
        self.sessions.insert(cid, Session { device, ch, watch: None, term: None, visible: true });
        self.report();
        json!({"t": "hs", "eh": b64(&eh)})
    }

    fn on_sealed(&mut self, ws: &mut Ws, cid: u64, data: &Value) -> Result<(), String> {
        let c = data.get("c").and_then(Value::as_str).unwrap_or("");
        let plain = self.sessions.get_mut(&cid).and_then(|s| s.ch.open(c));
        let Some(plain) = plain else {
            // Forged, replayed, or no session: this connection is done.
            if self.sessions.remove(&cid).is_some() {
                self.report();
            }
            return send(ws, cid, &json!({"t": "denied", "why": "bad frame"}));
        };
        let Ok(req) = serde_json::from_slice::<Value>(&plain) else { return Ok(()) };
        match req.get("t").and_then(Value::as_str) {
            Some("hello") => {
                let view = inner().lock().unwrap().view.clone();
                if let Some(v) = view {
                    self.send_sealed(ws, cid, &v)?;
                }
            }
            // Open a claude's chat: history now, then whatever it appends.
            Some("open") => {
                let handle = req.get("handle").and_then(Value::as_u64);
                let id = req.get("id").and_then(Value::as_u64);
                if let (Some(s), Some(handle), Some(id)) = (self.sessions.get_mut(&cid), handle, id) {
                    s.watch = Some(Watch {
                        handle,
                        id: id as usize,
                        tail: None,
                        resolved: Instant::now(),
                        gone: false,
                    });
                }
                self.poll_chat(ws, cid)?;
            }
            Some("type") => self.on_type(ws, cid, &req)?,
            Some("answer") => self.on_answer(ws, cid, &req)?,
            // Notifications: the key to subscribe with, then the subscription.
            Some("push-key") => {
                let key = {
                    let g = inner().lock().unwrap();
                    push::vapid_public_b64(&push::vapid_key(&g.cfg.vapid_secret).0)
                };
                self.send_sealed(ws, cid, &json!({"t": "push-key", "key": key}).to_string())?;
            }
            Some(t @ ("push-sub" | "push-off")) => {
                let sub = if t == "push-sub" {
                    serde_json::from_value::<push::Subscription>(req["sub"].clone()).ok()
                } else {
                    None
                };
                // Only an https endpoint is a push service; nothing else gets posted to.
                let sub = sub.filter(|s| s.endpoint.starts_with("https://"));
                if let Some(device) = self.sessions.get(&cid).map(|s| s.device.clone()) {
                    {
                        let mut g = inner().lock().unwrap();
                        if let Some(d) = g.devices.iter_mut().find(|d| d.id == device) {
                            d.push = sub.clone();
                        }
                        write_json("devices.json", &g.devices);
                    }
                    emit_changed();
                }
                let ok = t == "push-off" || sub.is_some();
                self.send_sealed(ws, cid, &json!({"t": "push-state", "on": t == "push-sub" && ok}).to_string())?;
            }
            // Start a claude or a terminal, or close one. The desktop does it,
            // through the same paths as ⌘T / ⌘⇧T / ⌘W, and answers like typing.
            // (Not `close`: that one closes the phone's chat view.)
            Some(t @ ("new-instance" | "close-instance")) => {
                let action = if t == "new-instance" { "new" } else { "close" };
                let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect::<String>();
                let app = {
                    let mut g = inner().lock().unwrap();
                    g.pending.insert(rid.clone(), (self.epoch, cid));
                    g.app.clone()
                };
                #[cfg(test)]
                if app.is_none() {
                    // No desktop in tests: a new instance is #9, a close works.
                    reply(&rid, true, None, (action == "new").then_some(9));
                }
                if let Some(app) = app {
                    let _ = app.emit("remote-action", json!({
                        "action": action,
                        "handle": req.get("handle"),
                        "id": req.get("id"),
                        "kind": req.get("kind"),
                        "rid": rid,
                    }));
                }
            }
            // A drag on the phone: the project tabs (`handles`), or one
            // project's sidebar rows (`handle` + `ids`, top to bottom). The
            // desktop commits it through the same path as its own drag, and the
            // next view carries it back to every phone.
            Some("reorder") => {
                let app = inner().lock().unwrap().app.clone();
                if let Some(app) = app {
                    let _ = app.emit("remote-reorder", json!({
                        "handles": req.get("handles"),
                        "handle": req.get("handle"),
                        "ids": req.get("ids"),
                    }));
                }
            }
            Some("key") => {
                let app = inner().lock().unwrap().app.clone();
                if let Some(app) = app {
                    let _ = app.emit("remote-key", json!({
                        "handle": req.get("handle"),
                        "id": req.get("id"),
                        "key": req.get("key"),
                    }));
                }
            }
            // Open a terminal: its recent output now, then a live stream.
            Some("term-open") => {
                let handle = req.get("handle").and_then(Value::as_u64);
                let id = req.get("id").and_then(Value::as_u64).map(|i| i as usize);
                let (Some(handle), Some(id)) = (handle, id) else { return Ok(()) };
                let msg = match tap_terminal(handle, id) {
                    Some((tail, rx, (cols, rows))) => {
                        if let Some(s) = self.sessions.get_mut(&cid) {
                            s.term = Some(TermWatch { handle, id, rx });
                        }
                        json!({"t": "term", "handle": handle, "id": id, "reset": true,
                               "cols": cols, "rows": rows, "data": b64(&tail)})
                    }
                    None => json!({"t": "term", "handle": handle, "id": id, "gone": true}),
                };
                self.send_sealed(ws, cid, &msg.to_string())?;
            }
            Some("term-close") => {
                if let Some(s) = self.sessions.get_mut(&cid) {
                    s.term = None;
                }
            }
            Some("input") => {
                let handle = req.get("handle").and_then(Value::as_u64);
                let id = req.get("id").and_then(Value::as_u64).map(|i| i as usize);
                let data = req.get("data").and_then(Value::as_str).unwrap_or("");
                if let (Some(handle), Some(id)) = (handle, id) {
                    if !data.is_empty() && data.len() <= MAX_MESSAGE {
                        write_terminal(handle, id, data.as_bytes());
                    }
                }
            }
            Some("close") => {
                if let Some(s) = self.sessions.get_mut(&cid) {
                    s.watch = None;
                }
            }
            Some("visible") => {
                let on = req.get("on").and_then(Value::as_bool).unwrap_or(true);
                if let Some(s) = self.sessions.get_mut(&cid) {
                    s.visible = on;
                }
                if on {
                    seen_done();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Type a message into a claude. The desktop frontend does the typing,
    /// because only it can see the claude's input box and refuse to type onto a
    /// draft or into an open dialog (`promptbox.ts`); it answers through
    /// `remote_reply`, which lands in `outbox`.
    ///
    /// Anything over `MAX_TYPED` bytes is written to a file and the claude is
    /// handed a short line naming it: typing into the TUI silently truncated at
    /// about 1 KB before (see the root CLAUDE.md), and a file never does.
    fn on_type(&mut self, ws: &mut Ws, cid: u64, req: &Value) -> Result<(), String> {
        let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect::<String>();
        let text = req.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let (Some(handle), Some(id)) = (req.get("handle").and_then(Value::as_u64), req.get("id").and_then(Value::as_u64))
        else {
            return Ok(());
        };
        let fail = |why: &str| json!({"t": "typed", "rid": rid, "ok": false, "why": why}).to_string();
        if text.is_empty() {
            return self.send_sealed(ws, cid, &fail("Nothing to send"));
        }
        if text.len() > MAX_MESSAGE {
            return self.send_sealed(ws, cid, &fail("That message is too long"));
        }
        let typed = if text.len() > MAX_TYPED {
            match write_long_message(&text) {
                Ok(path) => format!(
                    "My message is in {} (sent from my phone, too long to type here). Read it and treat it as my prompt.",
                    path.display()
                ),
                Err(e) => return self.send_sealed(ws, cid, &fail(&e)),
            }
        } else {
            text
        };
        let app = {
            let mut g = inner().lock().unwrap();
            g.pending.insert(rid.clone(), (self.epoch, cid));
            g.app.clone()
        };
        match app {
            Some(app) => {
                let _ = app.emit("remote-type", json!({"handle": handle, "id": id, "text": typed, "rid": rid}));
                Ok(())
            }
            #[cfg(test)]
            None => {
                tests::TYPED.lock().unwrap().push((rid, typed));
                Ok(())
            }
            #[cfg(not(test))]
            None => self.send_sealed(ws, cid, &fail("Mulpex isn't ready")),
        }
    }

    /// Forward whatever each watched terminal printed since the last pass.
    fn pump_terminals(&mut self, ws: &mut Ws) -> Result<(), String> {
        let mut out: Vec<(u64, String)> = Vec::new();
        let mut geo = None;
        for (cid, s) in self.sessions.iter_mut() {
            let Some(t) = &s.term else { continue };
            let mut buf = Vec::new();
            let mut gone = false;
            loop {
                match t.rx.try_recv() {
                    Ok(b) => {
                        buf.extend_from_slice(&b);
                        if buf.len() >= TERM_CHUNK {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    // The terminal was closed.
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            let (handle, id) = (t.handle, t.id);
            if !buf.is_empty() {
                let (cols, rows) = *geo.get_or_insert_with(geometry);
                out.push((*cid, json!({"t": "term", "handle": handle, "id": id,
                    "cols": cols, "rows": rows, "data": b64(&buf)}).to_string()));
            }
            if gone {
                s.term = None;
                out.push((*cid, json!({"t": "term", "handle": handle, "id": id, "gone": true}).to_string()));
            }
        }
        for (cid, msg) in out {
            self.send_sealed(ws, cid, &msg)?;
        }
        Ok(())
    }

    /// Answer a claude's open dialog. The keys are worked out here, against the
    /// dialog as it is on disk now (`dialog.rs`); the desktop frontend plays
    /// them, after checking the claude is still waiting on it, and answers
    /// through `remote_reply` like typing does.
    fn on_answer(&mut self, ws: &mut Ws, cid: u64, req: &Value) -> Result<(), String> {
        let rid = req.get("rid").and_then(Value::as_str).unwrap_or("").chars().take(64).collect::<String>();
        let fail = |why: &str| json!({"t": "typed", "rid": rid, "ok": false, "why": why}).to_string();
        let (Some(handle), Some(id)) = (req.get("handle").and_then(Value::as_u64), req.get("id").and_then(Value::as_u64))
        else {
            return Ok(());
        };
        let id = id as usize;
        let Some(state_dir) = state_dir_of(handle) else {
            return self.send_sealed(ws, cid, &fail("That project isn't open"));
        };
        let chunks = match dialog::keys(&state_dir, id, req.get("answer").unwrap_or(&Value::Null)) {
            Ok(k) => k,
            Err(e) => return self.send_sealed(ws, cid, &fail(&e)),
        };
        let app = {
            let mut g = inner().lock().unwrap();
            g.pending.insert(rid.clone(), (self.epoch, cid));
            g.app.clone()
        };
        if let Some(app) = app {
            let _ = app.emit("remote-keys", json!({"handle": handle, "id": id, "chunks": chunks, "rid": rid}));
        }
        Ok(())
    }

    /// Bring one phone's open chat up to date: a full reload when the
    /// transcript is new to us (or was replaced), otherwise only what was
    /// appended since the last pass.
    fn poll_chat(&mut self, ws: &mut Ws, cid: u64) -> Result<(), String> {
        let Some(w) = self.sessions.get_mut(&cid).and_then(|s| s.watch.as_mut()) else {
            return Ok(());
        };
        let (handle, id) = (w.handle, w.id);
        let recheck = w.tail.is_none() || w.resolved.elapsed() >= RESOLVE_EVERY;
        let mut reload = None;
        if recheck {
            w.resolved = Instant::now();
            match transcript_of(handle, id) {
                None => {
                    if !w.gone {
                        w.gone = true;
                        w.tail = None;
                        let msg = json!({"t": "chat", "handle": handle, "id": id, "reset": true, "items": [], "gone": true});
                        return self.send_sealed(ws, cid, &msg.to_string());
                    }
                    return Ok(());
                }
                Some(path) if w.tail.as_ref().map(|t| &t.path) != Some(&path) => reload = Some(path),
                Some(_) => {}
            }
        }
        let appended = match (&mut w.tail, reload) {
            (_, Some(path)) => Err(path),
            (Some(t), None) => match t.read_new() {
                Some(items) => Ok(items),
                None => Err(t.path.clone()),
            },
            (None, None) => Ok(vec![]),
        };
        let msg = match appended {
            Ok(items) if items.is_empty() => return Ok(()),
            Ok(items) => json!({"t": "chat", "handle": handle, "id": id, "items": items}),
            Err(path) => {
                let (tail, items) = chat::Tail::open(path);
                w.tail = Some(tail);
                w.gone = false;
                json!({"t": "chat", "handle": handle, "id": id, "reset": true, "items": items})
            }
        };
        self.send_sealed(ws, cid, &msg.to_string())
    }
}

/// A new phone proving it scanned the current QR code. On success it is stored,
/// and the QR code changes so the same one can't pair a second phone.
fn on_pair(data: &Value) -> Value {
    let str_of = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or("");
    let device = str_of("dev");
    let name: String = str_of("name").chars().take(40).collect();
    let (Some(device_pub), Some(proof)) = (crypto::unb64_32(str_of("dp")), crypto::unb64(str_of("proof")))
    else {
        return json!({"t": "denied", "why": "bad pairing request"});
    };
    if device.is_empty() || device.len() > 64 {
        return json!({"t": "denied", "why": "bad pairing request"});
    }
    {
        let mut g = inner().lock().unwrap();
        let valid = g
            .pairing
            .as_ref()
            .filter(|p| p.expires > Instant::now())
            .is_some_and(|p| crypto::pair_proof_ok(&p.secret, device, &device_pub, &proof));
        if !valid {
            return json!({"t": "denied", "why": "This QR code expired or was already used. Open Remote Control on the Mac and scan the new one."});
        }
        g.pairing = None;
        g.devices.retain(|d| d.id != device);
        let added = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        g.devices.push(Device {
            id: device.to_string(),
            name: if name.is_empty() { "Phone".into() } else { name },
            public: b64(&device_pub),
            added,
            push: None,
        });
        write_json("devices.json", &g.devices);
    }
    emit_changed();
    json!({"t": "paired"})
}

#[tauri::command]
pub fn remote_status() -> RemoteStatus {
    status_of(&mut inner().lock().unwrap())
}

#[tauri::command]
pub fn remote_set_enabled(on: bool) -> RemoteStatus {
    set_enabled(on);
    remote_status()
}

#[tauri::command]
pub fn remote_set_relay_url(url: String) -> Result<RemoteStatus, String> {
    set_relay_url(&url)?;
    Ok(remote_status())
}

#[tauri::command]
pub fn remote_reply(rid: String, ok: bool, why: Option<String>, id: Option<u64>) {
    reply(&rid, ok, why, id);
}

#[tauri::command]
pub fn remote_revoke_device(id: String) -> RemoteStatus {
    revoke(&id);
    remote_status()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> Config {
        Config {
            host_id: "abc".into(),
            relay_url: url.into(),
            host_secret: b64(&[1u8; 32]),
            relay_token: "tok".into(),
            vapid_secret: String::new(),
            enabled: false,
        }
    }

    #[test]
    fn ws_url_follows_the_scheme() {
        assert_eq!(ws_url(&cfg("http://10.0.0.2:8787")), "ws://10.0.0.2:8787/ws/host?id=abc&token=tok");
        assert_eq!(
            ws_url(&cfg("https://mulpex.example.com")),
            "wss://mulpex.example.com/ws/host?id=abc&token=tok"
        );
    }

    #[test]
    fn the_pairing_secret_rides_in_the_fragment() {
        let url = pair_url(&cfg("https://r.example"), &[9u8; 32]);
        let (before, after) = url.split_once('#').unwrap();
        assert_eq!(before, "https://r.example/");
        assert!(after.starts_with("p=abc."));
        assert!(after.ends_with(&b64(&[9u8; 32])));
    }

    /// The real connection thread against a stand-in relay, playing a phone:
    /// pair with the QR secret, handshake, say hello, get the view sealed; get
    /// the next view too; a reused QR is refused; revoking cuts the session;
    /// turning off closes the socket. Nothing readable is ever sent.
    #[test]
    fn a_phone_pairs_handshakes_and_gets_sealed_views() {
        let home = std::env::temp_dir().join(format!("mulpex-remote-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("MULPEX_HOME", &home);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        set_relay_url(&format!("http://127.0.0.1:{port}/")).unwrap();
        set_enabled(true);
        let saved = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(home.join("remote/config.json")).unwrap()).unwrap()
        };
        assert_eq!(saved()["enabled"], true, "on survives a restart");
        publish(workspace_view(Some(1), vec![json!({"handle": 1})]));

        // What the phone gets out of the QR code.
        let st = remote_status();
        let frag = st.pair_url.split_once("#p=").unwrap().1.to_string();
        let parts: Vec<&str> = frag.split('.').collect();
        let host_pub = crypto::unb64_32(parts[1]).unwrap();
        let pair_secret = crypto::unb64_32(parts[2]).unwrap();

        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut ws = tungstenite::accept_hdr(tcp, |req: &tungstenite::handshake::server::Request, resp| {
            assert!(req.uri().query().unwrap().contains("token="));
            Ok(resp)
        })
        .unwrap();
        let read = |ws: &mut tungstenite::WebSocket<TcpStream>| loop {
            if let Message::Text(t) = ws.read().unwrap() {
                let v = serde_json::from_str::<Value>(t.as_str()).unwrap();
                assert_eq!(v["to"], 5);
                return v["data"].clone();
            }
        };
        let say = |ws: &mut tungstenite::WebSocket<TcpStream>, data: Value| {
            ws.send(Message::Text(json!({"from": 5, "data": data}).to_string().into())).unwrap();
        };

        // A handshake before pairing is refused.
        let dev = crypto::random32();
        let dp = crypto::public_of(&dev);
        say(&mut ws, json!({"t": "hs", "dev": "d1", "ep": b64(&crypto::public_of(&crypto::random32()))}));
        assert_eq!(read(&mut ws)["why"], "not paired");

        let proof = crypto::pair_proof(&pair_secret, "d1", &dp);
        let pair = json!({"t": "pair", "dev": "d1", "dp": b64(&dp), "name": "iPhone", "proof": b64(&proof)});
        say(&mut ws, pair.clone());
        assert_eq!(read(&mut ws)["t"], "paired");
        // The same QR can't pair again.
        say(&mut ws, pair);
        assert_eq!(read(&mut ws)["t"], "denied");

        let eph = crypto::random32();
        say(&mut ws, json!({"t": "hs", "dev": "d1", "ep": b64(&crypto::public_of(&eph))}));
        let hs = read(&mut ws);
        assert_eq!(hs["t"], "hs");
        let eh = crypto::unb64_32(hs["eh"].as_str().unwrap()).unwrap();
        let mut ch = crypto::tests_phone_channel(&eph, &dev, &host_pub, &eh);
        assert_eq!(remote_status().clients, 1);
        assert!(remote_status().devices[0].online);

        say(&mut ws, json!({"t": "sealed", "c": ch.seal(br#"{"t":"hello"}"#)}));
        let sealed = read(&mut ws);
        assert_eq!(sealed["t"], "sealed");
        let view: Value = serde_json::from_slice(&ch.open(sealed["c"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(view["active"], 1);

        publish(workspace_view(Some(2), vec![]));
        let sealed = read(&mut ws);
        let view: Value = serde_json::from_slice(&ch.open(sealed["c"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(view["active"], 2);

        revoke("d1");
        assert_eq!(read(&mut ws)["why"], "revoked");
        assert_eq!(remote_status().clients, 0);

        set_enabled(false);
        let closed = loop {
            match ws.read() {
                Ok(Message::Close(_)) | Err(_) => break true,
                Ok(_) => continue,
            }
        };
        assert!(closed);
        assert_eq!(remote_status().state, "off");
        assert!(remote_status().pair_url.is_empty());
        assert_eq!(saved()["enabled"], false, "and so does off");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A real host for driving the real phone page by hand or from a headless
    /// browser: `MULPEX_TEST_RELAY=http://127.0.0.1:18788 cargo test -p mulpex
    /// --lib live_host -- --ignored --nocapture`. Prints the pairing link, then
    /// serves a fake one-project workspace for a minute.
    /// Stands in for the workspace lookup, which needs a running app.
    pub(super) static TRANSCRIPT: Mutex<Option<PathBuf>> = Mutex::new(None);
    /// A stand-in shell for `live_host`: (enabled, its output feed).
    pub(super) static FAKE_TERM: Mutex<(bool, Option<std::sync::mpsc::Sender<Vec<u8>>>)> = Mutex::new((false, None));

    /// What would have been typed, while there is no desktop frontend.
    pub(super) static TYPED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    #[test]
    #[ignore]
    fn live_host() {
        let relay = std::env::var("MULPEX_TEST_RELAY").expect("MULPEX_TEST_RELAY");
        if let Ok(t) = std::env::var("MULPEX_TEST_TRANSCRIPT") {
            *TRANSCRIPT.lock().unwrap() = Some(PathBuf::from(t));
        }
        FAKE_TERM.lock().unwrap().0 = true;
        let home = std::env::temp_dir().join(format!("mulpex-remote-live-{}", std::process::id()));
        std::env::set_var("MULPEX_HOME", &home);
        set_relay_url(&relay).unwrap();
        set_enabled(true);
        if let Ok(out) = std::env::var("MULPEX_TEST_PAIR_OUT") {
            std::fs::write(out, remote_status().pair_url).unwrap();
        }
        println!("PAIR {}", remote_status().pair_url);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            let n = start.elapsed().as_secs();
            if let Some(tx) = &FAKE_TERM.lock().unwrap().1 {
                let long = "0123456789".repeat(15);
                let _ = tx.send(format!(
                    "\r\n\x1b[33mtick {n}\x1b[0m שלום עולם\r\n{long}\r\nprogress 10%\rprogress 99%\r\nfake$ "
                ).into_bytes());
            }
            // Play the desktop frontend: "type" each message by appending it to
            // the transcript as the human prompt it would become.
            for (rid, text) in TYPED.lock().unwrap().drain(..) {
                if let Some(t) = TRANSCRIPT.lock().unwrap().clone() {
                    use std::io::Write;
                    let line = json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": text}});
                    let mut f = std::fs::OpenOptions::new().append(true).open(t).unwrap();
                    writeln!(f, "{line}").unwrap();
                }
                reply(&rid, true, None, None);
            }
            publish(workspace_view(
                Some(1),
                vec![json!({"handle": 1, "name": "demo", "sessions": [
                    {"id": 1, "kind": "claude", "name": "שלום world", "task": null,
                     "status": if n >= 20 { "waiting" } else { "working" },
                     "ctx_pct": n, "muted": false, "parent": null, "exited": false, "failed": null},
                    {"id": 2, "kind": "shell", "name": null, "task": null, "status": null,
                     "ctx_pct": null, "muted": false, "parent": null, "exited": false, "failed": null},
                ]})],
            ));
            std::thread::sleep(Duration::from_millis(500));
        }
        set_enabled(false);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The one summary notification: who counts, when it buzzes, when it's a
    /// silent edit, and when nothing is sent.
    #[test]
    fn the_summary_notification() {
        // #1 started by the user, #2 muted, #3 hub-spawned, #4 user's, #5 shell-like.
        let view = |a: &str, b: &str, c: &str, d: &str| json!({"projects": [{"handle": 1, "name": "cloud", "sessions": [
            {"id": 1, "status": a, "muted": false, "parent": null, "name": "Fix billing", "task": null},
            {"id": 2, "status": b, "muted": true, "parent": null, "name": null, "task": "x"},
            {"id": 3, "status": c, "muted": false, "parent": 1, "name": null, "task": "y"},
            {"id": 4, "status": d, "muted": false, "parent": null, "name": null, "task": null},
            {"id": 5, "status": null, "muted": false, "parent": null, "name": null, "task": null},
        ]}]});
        let mut sum = Summary::default();
        let (s0, note) = summary_for(&view("working", "working", "working", "needs"), None, &mut sum);
        assert!(note.is_none(), "nothing on the first view, even for what already needs you");

        // #1 finishes, #4 is (still) on needs: the first summary, and it buzzes.
        let (s1, note) = summary_for(&view("waiting", "waiting", "waiting", "needs"), Some(&s0), &mut sum);
        let n = note.expect("a summary");
        assert_eq!(n["title"], "1 needs you · 1 done", "not muted #2, not spawned #3");
        assert_eq!(n["body"], "● cloud · claude#4\n✓ cloud · claude#1 · Fix billing");
        assert_eq!(n["alert"], true);

        let (s2, note) = summary_for(&view("waiting", "waiting", "waiting", "needs"), Some(&s1), &mut sum);
        assert!(note.is_none(), "no change, nothing sent");

        // #4 answered (on the Mac): a smaller number is a silent edit.
        let (s3, note) = summary_for(&view("waiting", "waiting", "waiting", "working"), Some(&s2), &mut sum);
        let n = note.expect("an edit");
        assert_eq!((n["title"].as_str(), n["alert"].as_bool()), (Some("1 done"), Some(false)));

        // Opening the app sees the done one; with both at zero nothing is sent.
        sum.unseen.clear();
        let (s4, note) = summary_for(&view("waiting", "waiting", "waiting", "working"), Some(&s3), &mut sum);
        assert!(note.is_none(), "nothing is pushed at zero");

        // #4 asks again: back, and buzzing.
        let (s5, note) = summary_for(&view("waiting", "waiting", "waiting", "needs"), Some(&s4), &mut sum);
        assert_eq!(note.expect("needs again")["alert"], true);

        // Typing to a done claude takes it off the done list.
        let (s6, _) = summary_for(&view("working", "waiting", "waiting", "needs"), Some(&s5), &mut sum);
        let (_, note) = summary_for(&view("waiting", "waiting", "waiting", "needs"), Some(&s6), &mut sum);
        assert_eq!(note.expect("done again")["title"], "1 needs you · 1 done");
        assert_eq!(sum.unseen, vec!["1:1".to_string()]);
    }

    #[test]
    fn a_long_summary_is_cut() {
        let sessions: Vec<Value> = (1..=9)
            .map(|i| json!({"id": i, "status": "working", "muted": false, "parent": null, "name": null, "task": null}))
            .collect();
        let done: Vec<Value> = (1..=9)
            .map(|i| json!({"id": i, "status": "waiting", "muted": false, "parent": null, "name": null, "task": null}))
            .collect();
        let mut sum = Summary::default();
        let (a, _) = summary_for(&json!({"projects": [{"handle": 1, "name": "p", "sessions": sessions}]}), None, &mut sum);
        let (_, n) = summary_for(&json!({"projects": [{"handle": 1, "name": "p", "sessions": done}]}), Some(&a), &mut sum);
        let n = n.unwrap();
        assert_eq!(n["title"], "9 done");
        let body = n["body"].as_str().unwrap();
        assert_eq!(body.lines().count(), SUMMARY_LINES);
        assert_eq!(body.lines().last(), Some("+4 more"));
    }

    #[test]
    fn a_failed_claude_and_a_terminal_have_no_status() {
        use crate::snapshot::{Status, StatusEntry};
        let mk = |id, kind, failed: Option<&str>| SessionInfo {
            id,
            name: None,
            muted: false,
            parent: None,
            collapsed: false,
            kind,
            exited: false,
            failed: failed.map(String::from),
        };
        let sessions = vec![
            mk(1, SessionKind::Claude, None),
            mk(2, SessionKind::Claude, Some("boom")),
            mk(3, SessionKind::Shell, None),
        ];
        let snap = HubSnapshot {
            statuses: vec![StatusEntry { id: 1, status: Status::Needs, watching: false, ctx_pct: Some(40) }],
            tasks: vec![],
            locks: vec![],
            waiting: vec![],
            messages: vec![],
            pending_messages: 0,
            pending: vec![],
        };
        let v = project_view(7, "p", std::path::Path::new("/nonexistent"), &sessions, &snap);
        assert_eq!(v["sessions"][0]["status"], "needs");
        assert_eq!(v["sessions"][0]["ctx_pct"], 40);
        assert!(v["sessions"][1]["status"].is_null());
        assert!(v["sessions"][2]["status"].is_null());
        assert_eq!(v["sessions"][2]["kind"], "shell");
    }
}
