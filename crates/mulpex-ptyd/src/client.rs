//! The app's side of the daemon socket: request/reply by id, keystrokes, and a
//! reader thread that routes each session's output to whoever registered for it.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::proto;

/// What the daemon says about one session.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Output at this absolute offset since spawn.
    Output(u64, Vec<u8>),
    /// The PTY changed size here in the stream: (cols, rows).
    Resize(u16, u16),
    /// The child is gone.
    Exit,
    /// The daemon is gone, and every session with it.
    Lost,
}

pub trait Handler: Send + Sync {
    fn event(&self, ev: Event);
}

/// How long a request may take. A `kill` waits for its child, so generous.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    w: Mutex<UnixStream>,
    next_id: AtomicU64,
    waiters: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    handlers: Mutex<HashMap<u32, Arc<dyn Handler>>>,
    /// Events for a sid nobody has registered for yet — a freshly spawned
    /// session's first paint can arrive before the `spawn` reply is handled.
    pending: Mutex<HashMap<u32, Vec<Event>>>,
    alive: AtomicBool,
    /// The daemon's `hello` reply.
    pub hello: Value,
}

impl Client {
    /// Connect and say hello. The reply tells the caller which protocol the
    /// daemon speaks; whether that is one this app can drive is its decision.
    pub fn connect(sock: &Path, app: &str) -> std::io::Result<Arc<Client>> {
        let stream = UnixStream::connect(sock)?;
        let reader = stream.try_clone()?;
        let mut c = Client {
            w: Mutex::new(stream),
            next_id: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            hello: Value::Null,
        };
        // The hello is read inline, before the reader thread exists.
        {
            let mut w = c.w.lock().unwrap();
            proto::write_frame(&mut *w, proto::JSON, json!({"op": "hello", "id": 0, "proto": proto::PROTO, "app": app}).to_string().as_bytes())?;
        }
        let mut r = BufReader::new(reader);
        r.get_ref().set_read_timeout(Some(Duration::from_secs(5)))?;
        let hello = loop {
            match proto::read_frame(&mut r)? {
                Some((proto::JSON, p)) => {
                    let v: Value = serde_json::from_slice(&p).unwrap_or(Value::Null);
                    if v["id"] == 0 {
                        break v;
                    }
                }
                Some(_) => {}
                None => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon closed during hello")),
            }
        };
        r.get_ref().set_read_timeout(None)?;
        c.hello = hello;
        let c = Arc::new(c);
        {
            let c = Arc::clone(&c);
            std::thread::spawn(move || c.read_loop(r));
        }
        Ok(c)
    }

    pub fn proto(&self) -> u32 {
        self.hello["proto"].as_u64().unwrap_or(0) as u32
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn read_loop(self: Arc<Self>, mut r: BufReader<UnixStream>) {
        while let Ok(Some((kind, p))) = proto::read_frame(&mut r) {
            match kind {
                proto::OUTPUT => {
                    if let Some((sid, off, bytes)) = proto::parse_output(&p) {
                        self.dispatch(sid, Event::Output(off, bytes.to_vec()));
                    }
                }
                proto::JSON => {
                    let Ok(v) = serde_json::from_slice::<Value>(&p) else { continue };
                    if let Some(ev) = v["ev"].as_str() {
                        let sid = v["sid"].as_u64().unwrap_or(0) as u32;
                        match ev {
                            "exit" => self.dispatch(sid, Event::Exit),
                            "resize" => self.dispatch(
                                sid,
                                Event::Resize(v["cols"].as_u64().unwrap_or(80) as u16, v["rows"].as_u64().unwrap_or(24) as u16),
                            ),
                            _ => {}
                        }
                    } else if let Some(id) = v["id"].as_u64() {
                        if let Some(tx) = self.waiters.lock().unwrap().remove(&id) {
                            let _ = tx.send(v);
                        }
                    }
                }
                _ => {}
            }
        }
        // The daemon is gone. Every session it held is gone with it.
        self.alive.store(false, Ordering::Relaxed);
        self.waiters.lock().unwrap().clear();
        let hs: Vec<Arc<dyn Handler>> = self.handlers.lock().unwrap().drain().map(|(_, h)| h).collect();
        for h in hs {
            h.event(Event::Lost);
        }
    }

    fn dispatch(&self, sid: u32, ev: Event) {
        let h = self.handlers.lock().unwrap().get(&sid).cloned();
        match h {
            Some(h) => h.event(ev),
            None => {
                // Re-checked under `pending`'s lock: `register` takes the
                // handler in under the same lock, so an event either lands in
                // the backlog it drains or reaches the handler — never neither.
                let mut pend = self.pending.lock().unwrap();
                if let Some(h) = self.handlers.lock().unwrap().get(&sid).cloned() {
                    drop(pend);
                    h.event(ev);
                } else {
                    pend.entry(sid).or_default().push(ev);
                }
            }
        }
    }

    /// Route `sid`'s events to `h`, starting with any that arrived early.
    pub fn register(&self, sid: u32, h: Arc<dyn Handler>) {
        let mut pend = self.pending.lock().unwrap();
        for ev in pend.remove(&sid).unwrap_or_default() {
            h.event(ev);
        }
        self.handlers.lock().unwrap().insert(sid, h);
        drop(pend);
        if !self.is_alive() {
            if let Some(h) = self.handlers.lock().unwrap().remove(&sid) {
                h.event(Event::Lost);
            }
        }
    }

    pub fn unregister(&self, sid: u32) {
        self.handlers.lock().unwrap().remove(&sid);
        self.pending.lock().unwrap().remove(&sid);
    }

    /// One request, its reply. `Err` carries the daemon's error text, or why
    /// there was no reply.
    pub fn request(&self, mut v: Value) -> Result<Value, String> {
        if !self.is_alive() {
            return Err("the Mulpex PTY daemon is not running".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        v["id"] = json!(id);
        let (tx, rx) = mpsc::channel();
        self.waiters.lock().unwrap().insert(id, tx);
        let sent = {
            let mut w = self.w.lock().unwrap();
            w.write_all(&proto::json_frame(&v))
        };
        if let Err(e) = sent {
            self.waiters.lock().unwrap().remove(&id);
            return Err(format!("PTY daemon: {e}"));
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(r) if r["ok"] == true => Ok(r),
            Ok(r) => Err(r["error"].as_str().unwrap_or("PTY daemon refused").to_string()),
            Err(_) => {
                self.waiters.lock().unwrap().remove(&id);
                Err(format!("PTY daemon: no reply to {}", v["op"]))
            }
        }
    }

    /// Keystrokes. Both bytes of an ESC+CR must reach the PTY in one write
    /// (CLAUDE.md, Shift+Enter) — one frame here is one `write_all` there.
    pub fn send_input(&self, sid: u32, bytes: &[u8]) {
        let mut w = self.w.lock().unwrap();
        let _ = w.write_all(&proto::input_frame(sid, bytes));
    }

    /// Close the connection without asking anything of the daemon: its
    /// sessions keep running for the next app (a restart).
    pub fn close(&self) {
        let _ = self.w.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}
