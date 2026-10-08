//! The Remote Control relay. A Mac running Mulpex connects OUT to it (a Mac sits
//! behind NAT, so nothing can connect in), phones connect to it too, and it
//! routes frames between the two. It also serves the phone's web app.
//!
//! It is deliberately dumb: it never looks inside `data`. Once pairing lands,
//! `data` is ciphertext the relay cannot read, so nothing here may come to depend
//! on it.
//!
//! One *room* per host id. The envelope, all JSON text frames:
//!   relay → host    {"event":"join","cid":N} | {"event":"leave","cid":N}
//!                   | {"from":N,"data":…}
//!   host → relay    {"to":N,"data":…}   (`to` absent or null = every client)
//!   relay → client  {"event":"host","online":bool} | {"data":…}
//!   client → relay  {"data":…}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tower_http::services::{ServeDir, ServeFile};

type Tx = UnboundedSender<Out>;

/// What a connection's writer task is told to do.
#[derive(Debug, PartialEq)]
enum Out {
    Text(String),
    /// A newer connection for the same host id took over this one.
    Close,
}

#[derive(Default)]
struct Room {
    host: Option<(u64, Tx)>,
    clients: HashMap<u64, Tx>,
}

#[derive(Default)]
struct Relay {
    rooms: Mutex<HashMap<String, Room>>,
    next: AtomicU64,
    /// Host id → SHA-256 of the token that first claimed it. Whoever holds the
    /// token owns the room; nobody else can open it as host. Only hashes are
    /// kept, so the file is worthless to someone who reads it.
    owners: Mutex<HashMap<String, String>>,
    /// Where `owners` is saved. `None` keeps it in memory only (tests).
    owners_file: Option<PathBuf>,
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Relay {
    fn with_owners_file(path: PathBuf) -> Self {
        let owners = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Relay { owners: Mutex::new(owners), owners_file: Some(path), ..Default::default() }
    }

    /// May this token open `room_id` as host? The first token to ask claims it.
    fn claim(&self, room_id: &str, token: &str) -> bool {
        if token.len() < 16 || token.len() > 128 {
            return false;
        }
        let hash = sha256_hex(token);
        let mut owners = self.owners.lock().unwrap();
        match owners.get(room_id) {
            Some(h) => *h == hash,
            None => {
                owners.insert(room_id.to_string(), hash);
                if let Some(path) = &self.owners_file {
                    if let Ok(body) = serde_json::to_string_pretty(&*owners) {
                        let tmp = path.with_extension("tmp");
                        if std::fs::write(&tmp, body).is_ok() {
                            let _ = std::fs::rename(&tmp, path);
                        }
                    }
                }
                true
            }
        }
    }

    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn host_joined(&self, room_id: &str, conn: u64, tx: Tx) {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.entry(room_id.to_string()).or_default();
        if let Some((_, old)) = room.host.replace((conn, tx.clone())) {
            let _ = old.send(Out::Close);
        }
        let online = json!({"event": "host", "online": true}).to_string();
        for (cid, ctx) in &room.clients {
            let _ = tx.send(Out::Text(json!({"event": "join", "cid": cid}).to_string()));
            let _ = ctx.send(Out::Text(online.clone()));
        }
    }

    fn host_left(&self, room_id: &str, conn: u64) {
        let mut rooms = self.rooms.lock().unwrap();
        let Some(room) = rooms.get_mut(room_id) else { return };
        // A replaced connection must not take its successor down with it.
        if room.host.as_ref().map(|(c, _)| *c) != Some(conn) {
            return;
        }
        room.host = None;
        let offline = json!({"event": "host", "online": false}).to_string();
        for ctx in room.clients.values() {
            let _ = ctx.send(Out::Text(offline.clone()));
        }
        if room.clients.is_empty() {
            rooms.remove(room_id);
        }
    }

    fn from_host(&self, room_id: &str, text: &str) {
        #[derive(Deserialize)]
        struct Env {
            to: Option<u64>,
            data: Value,
        }
        let Ok(env) = serde_json::from_str::<Env>(text) else { return };
        let out = json!({"data": env.data}).to_string();
        let rooms = self.rooms.lock().unwrap();
        let Some(room) = rooms.get(room_id) else { return };
        match env.to {
            Some(cid) => {
                if let Some(ctx) = room.clients.get(&cid) {
                    let _ = ctx.send(Out::Text(out));
                }
            }
            None => {
                for ctx in room.clients.values() {
                    let _ = ctx.send(Out::Text(out.clone()));
                }
            }
        }
    }

    fn client_joined(&self, room_id: &str, cid: u64, tx: Tx) {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.entry(room_id.to_string()).or_default();
        let online = room.host.is_some();
        let _ = tx.send(Out::Text(json!({"event": "host", "online": online}).to_string()));
        if let Some((_, htx)) = &room.host {
            let _ = htx.send(Out::Text(json!({"event": "join", "cid": cid}).to_string()));
        }
        room.clients.insert(cid, tx);
    }

    fn client_left(&self, room_id: &str, cid: u64) {
        let mut rooms = self.rooms.lock().unwrap();
        let Some(room) = rooms.get_mut(room_id) else { return };
        room.clients.remove(&cid);
        if let Some((_, htx)) = &room.host {
            let _ = htx.send(Out::Text(json!({"event": "leave", "cid": cid}).to_string()));
        } else if room.clients.is_empty() {
            rooms.remove(room_id);
        }
    }

    fn from_client(&self, room_id: &str, cid: u64, text: &str) {
        #[derive(Deserialize)]
        struct Env {
            data: Value,
        }
        let Ok(env) = serde_json::from_str::<Env>(text) else { return };
        let rooms = self.rooms.lock().unwrap();
        if let Some((_, htx)) = rooms.get(room_id).and_then(|r| r.host.as_ref()) {
            let out = json!({"from": cid, "data": env.data}).to_string();
            let _ = htx.send(Out::Text(out));
        }
    }
}

#[derive(Deserialize)]
struct HostQuery {
    id: String,
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct ClientQuery {
    host: String,
}

/// A host id is opaque to the relay, but bounded so a room key can't be abused.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

async fn host_ws(
    ws: WebSocketUpgrade,
    Query(q): Query<HostQuery>,
    State(relay): State<Arc<Relay>>,
) -> impl IntoResponse {
    if !valid_id(&q.id) {
        return (axum::http::StatusCode::BAD_REQUEST, "bad id").into_response();
    }
    if !relay.claim(&q.id, &q.token) {
        return (axum::http::StatusCode::FORBIDDEN, "this host id belongs to another Mac")
            .into_response();
    }
    ws.on_upgrade(move |socket| async move {
        let conn = relay.next_id();
        let (tx, rx) = unbounded_channel();
        relay.host_joined(&q.id, conn, tx);
        let r = relay.clone();
        let id = q.id.clone();
        pump(socket, rx, move |text| r.from_host(&id, &text)).await;
        relay.host_left(&q.id, conn);
    })
}

async fn client_ws(
    ws: WebSocketUpgrade,
    Query(q): Query<ClientQuery>,
    State(relay): State<Arc<Relay>>,
) -> impl IntoResponse {
    if !valid_id(&q.host) {
        return (axum::http::StatusCode::BAD_REQUEST, "bad host").into_response();
    }
    ws.on_upgrade(move |socket| async move {
        let cid = relay.next_id();
        let (tx, rx) = unbounded_channel();
        relay.client_joined(&q.host, cid, tx);
        let r = relay.clone();
        let id = q.host.clone();
        pump(socket, rx, move |text| r.from_client(&id, cid, &text)).await;
        relay.client_left(&q.host, cid);
    })
}

/// Run one connection: frames from the room go out, text frames that come in go
/// to `on_text`. Returns when either side ends.
async fn pump(
    socket: WebSocket,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Out>,
    on_text: impl Fn(String),
) {
    let (mut sink, mut stream) = socket.split();
    let mut writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Text(t) => {
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        break;
                    }
                }
                Out::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    });
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => on_text(t.to_string()),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = &mut writer => break,
        }
    }
    writer.abort();
}

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let listen: SocketAddr = arg("--listen")
        .unwrap_or_else(|| "0.0.0.0:8787".into())
        .parse()
        .expect("--listen must be host:port");
    let static_dir = PathBuf::from(arg("--static").unwrap_or_else(|| "remote/dist".into()));
    let index = static_dir.join("index.html");
    let data_dir = PathBuf::from(arg("--data").unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&data_dir).expect("--data dir");

    let relay = Arc::new(Relay::with_owners_file(data_dir.join("hosts.json")));
    let app = Router::new()
        .route("/ws/host", get(host_ws))
        .route("/ws/client", get(client_ws))
        .fallback_service(ServeDir::new(&static_dir).fallback(ServeFile::new(index)))
        .with_state(relay);

    let listener = tokio::net::TcpListener::bind(listen).await.expect("bind");
    eprintln!("mulpex-relay: listening on {listen}, serving {}", static_dir.display());
    axum::serve(listener, app).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn drain(rx: &mut UnboundedReceiver<Out>) -> Vec<Value> {
        let mut v = Vec::new();
        while let Ok(Out::Text(t)) = rx.try_recv() {
            v.push(serde_json::from_str(&t).unwrap());
        }
        v
    }

    #[test]
    fn routes_between_host_and_clients() {
        let r = Relay::default();
        let (htx, mut hrx) = unbounded_channel();
        let (c1tx, mut c1rx) = unbounded_channel();
        let (c2tx, mut c2rx) = unbounded_channel();
        r.client_joined("h", 10, c1tx);
        assert_eq!(drain(&mut c1rx), vec![json!({"event":"host","online":false})]);
        r.host_joined("h", 1, htx);
        assert_eq!(drain(&mut hrx), vec![json!({"event":"join","cid":10})]);
        assert_eq!(drain(&mut c1rx), vec![json!({"event":"host","online":true})]);
        r.client_joined("h", 11, c2tx);
        assert_eq!(drain(&mut hrx), vec![json!({"event":"join","cid":11})]);
        drain(&mut c2rx);

        r.from_client("h", 11, r#"{"data":{"t":"hello"}}"#);
        assert_eq!(drain(&mut hrx), vec![json!({"from":11,"data":{"t":"hello"}})]);

        r.from_host("h", r#"{"to":11,"data":"x"}"#);
        assert!(drain(&mut c1rx).is_empty());
        assert_eq!(drain(&mut c2rx), vec![json!({"data":"x"})]);

        r.from_host("h", r#"{"data":"y"}"#);
        assert_eq!(drain(&mut c1rx), vec![json!({"data":"y"})]);
        assert_eq!(drain(&mut c2rx), vec![json!({"data":"y"})]);

        r.client_left("h", 10);
        assert_eq!(drain(&mut hrx), vec![json!({"event":"leave","cid":10})]);
        r.host_left("h", 1);
        assert_eq!(drain(&mut c2rx), vec![json!({"event":"host","online":false})]);
    }

    #[test]
    fn rooms_are_isolated() {
        let r = Relay::default();
        let (htx, mut hrx) = unbounded_channel();
        let (ctx, mut crx) = unbounded_channel();
        r.host_joined("a", 1, htx);
        r.client_joined("b", 2, ctx);
        drain(&mut crx);
        r.from_host("a", r#"{"data":"secret"}"#);
        r.from_client("b", 2, r#"{"data":"hi"}"#);
        assert!(drain(&mut crx).is_empty());
        assert!(drain(&mut hrx).is_empty());
    }

    #[test]
    fn a_replaced_host_does_not_unregister_its_successor() {
        let r = Relay::default();
        let (old, mut old_rx) = unbounded_channel();
        let (new, _new_rx) = unbounded_channel();
        r.host_joined("h", 1, old);
        r.host_joined("h", 2, new);
        assert_eq!(old_rx.try_recv().unwrap(), Out::Close);
        r.host_left("h", 1);
        let (ctx, mut crx) = unbounded_channel();
        r.client_joined("h", 3, ctx);
        assert_eq!(drain(&mut crx), vec![json!({"event":"host","online":true})]);
    }

    #[test]
    fn the_first_token_owns_the_room() {
        let r = Relay::default();
        let a = "a".repeat(43);
        let b = "b".repeat(43);
        assert!(r.claim("h", &a));
        assert!(r.claim("h", &a));
        assert!(!r.claim("h", &b));
        assert!(r.claim("other", &b));
        assert!(!r.claim("x", "short"));
    }

    #[test]
    fn ownership_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("mulpex-relay-owners-{}.json", std::process::id()));
        let a = "a".repeat(43);
        assert!(Relay::with_owners_file(path.clone()).claim("h", &a));
        let r = Relay::with_owners_file(path.clone());
        assert!(!r.claim("h", &"b".repeat(43)));
        assert!(r.claim("h", &a));
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&a), "only the hash is stored");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ids_are_bounded() {
        assert!(valid_id("abc-123"));
        assert!(!valid_id(""));
        assert!(!valid_id("../x"));
        assert!(!valid_id(&"a".repeat(129)));
    }
}
