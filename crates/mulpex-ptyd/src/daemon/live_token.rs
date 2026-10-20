//! The Live Token proxy (docs/live-token.md), running in the daemon so a
//! claude's API calls keep flowing while the app restarts — they go to
//! `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>`, and that port has to
//! be served for as long as the claude lives, not as long as the app does.
//!
//! The app owns the settings and the dialog; it pushes the effective config
//! here (`configure`) on every connect and every change. Everything about how
//! a request is rewritten is unchanged from when this lived in the app.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

/// How often the refresh command runs on its own.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// A refresh command that takes longer than this is killed.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
/// A 401/429 within this long of the last refresh does not run another one —
/// a burst of failing requests shares one refresh.
const REFRESH_DEBOUNCE: Duration = Duration::from_secs(5);

/// What the app sends: its settings, plus the two things only the app can
/// work out (from the user's login environment).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// The file holding the current token. Empty = the feature is off.
    #[serde(default)]
    pub token_file: String,
    #[serde(default)]
    pub refresh_command: String,
    /// The API the claudes would have used without us.
    #[serde(default)]
    pub upstream: String,
    /// The refresh command's environment (`PATH` included).
    #[serde(default)]
    pub refresh_env: Vec<(String, String)>,
    /// Pasted tokens a claude can be pinned to (`assign`). The token file is
    /// the "default" one, used by every claude that is not pinned.
    #[serde(default)]
    pub tokens: Vec<NamedToken>,
    /// Route (`<project key>-<instance id>`, from the base URL's `/c/<route>`)
    /// → the id of the token in `tokens` that claude is pinned to.
    #[serde(default)]
    pub assign: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NamedToken {
    pub id: String,
    pub token: String,
}

impl Config {
    fn enabled(&self) -> bool {
        !self.token_file.trim().is_empty()
    }
}

static CONFIG: RwLock<Option<Config>> = RwLock::new(None);

fn config() -> Config {
    CONFIG.read().unwrap().clone().unwrap_or_default()
}

struct Proxy {
    port: u16,
    secret: String,
}

static PROXY: OnceLock<Proxy> = OnceLock::new();

/// Take the app's config; start the proxy if it turns the feature on. Returns
/// where it listens (`port`/`secret`, absent while it has never started).
pub fn configure(v: &Value) -> Result<Value, String> {
    let c: Config = serde_json::from_value(v.clone()).map_err(|e| format!("live token config: {e}"))?;
    let on = c.enabled();
    *CONFIG.write().unwrap() = Some(c);
    *TOKEN.lock().unwrap() = TokenCache::default();
    if on {
        ensure_started()?;
    }
    Ok(match PROXY.get() {
        // `routes`: this daemon understands `/c/<route>` (an older one would
        // forward it upstream as part of the path).
        Some(p) => json!({"running": true, "port": p.port, "secret": p.secret, "routes": true}),
        None => json!({"running": false}),
    })
}

/// `~/x` → `$HOME/x`. Anything else is used as written.
fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

pub fn read_token_file(path: &std::path::Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = text.trim();
    if token.starts_with("sk-ant-") {
        Ok(token.to_string())
    } else if token.is_empty() {
        Err(format!("{} is empty", path.display()))
    } else {
        Err(format!("{} does not hold an sk-ant- token", path.display()))
    }
}

/// The token file's last good contents, re-read only when its mtime or size
/// changes. A refresher replaces the file with `mv`, so a read can briefly see
/// it missing; the last good token is kept through that.
#[derive(Default)]
struct TokenCache {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
    token: Option<String>,
}

static TOKEN: LazyLock<Mutex<TokenCache>> = LazyLock::new(Default::default);

/// The token to put on the next request, or `None` to leave the claude's own.
fn current_token() -> Option<String> {
    let s = config();
    if !s.enabled() {
        return None;
    }
    let path = expand(s.token_file.trim());
    let mut c = TOKEN.lock().unwrap();
    if c.path != path {
        *c = TokenCache { path: path.clone(), ..Default::default() };
    }
    let stamp = std::fs::metadata(&path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    if stamp.is_some() && stamp != c.stamp {
        if let Ok(t) = read_token_file(&path) {
            c.token = Some(t);
            c.stamp = stamp;
        }
    }
    c.token.clone()
}

/// A short fingerprint for logs — never the token itself.
fn fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(token.as_bytes());
    d.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt
    })
}

/// Bind the listener (synchronously, so the port is known before the reply)
/// and start serving + the refresh loop. Idempotent.
fn ensure_started() -> Result<(), String> {
    if PROXY.get().is_some() {
        return Ok(());
    }
    let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("live token proxy: bind: {e}"))?;
    std_listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let port = std_listener.local_addr().map_err(|e| e.to_string())?.port();
    let mut raw = [0u8; 16];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut raw))
            .map_err(|e| format!("live token proxy: /dev/urandom: {e}"))?;
    }
    let secret: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    if PROXY.set(Proxy { port, secret }).is_err() {
        return Ok(());
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    runtime().spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[ptyd] live token proxy: {e}");
                return;
            }
        };
        serve(listener).await;
    });
    runtime().spawn(refresh_loop());
    Ok(())
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    })
}

async fn serve(listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, hyper::service::service_fn(handle))
                .await;
        });
    }
}

type Body = BoxBody<Bytes, std::io::Error>;

fn plain(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(
        Full::new(Bytes::from(msg.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *r.status_mut() = status;
    r
}

/// Headers that describe one hop, not the message.
pub fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

/// Strip `/<secret>` off a request path; `None` if it isn't there.
pub fn strip_secret<'a>(path_and_query: &'a str, secret: &str) -> Option<&'a str> {
    let rest = path_and_query.strip_prefix('/')?.strip_prefix(secret)?;
    if rest.is_empty() {
        Some("/")
    } else if rest.starts_with('/') || rest.starts_with('?') {
        Some(rest)
    } else {
        None
    }
}

/// Split an optional `/c/<route>` off the front of a (secret-stripped) path:
/// the claude that sent the request. Claudes spawned before routes existed
/// send none.
pub fn split_route(rest: &str) -> (Option<&str>, &str) {
    let Some(after) = rest.strip_prefix("/c/") else {
        return (None, rest);
    };
    let end = after.find(['/', '?']).unwrap_or(after.len());
    let route = &after[..end];
    if route.is_empty() {
        return (None, rest);
    }
    let tail = &after[end..];
    (Some(route), if tail.is_empty() { "/" } else { tail })
}

/// Which token a request gets: the one its claude is pinned to, else the
/// default (the token file). `key` names it in the stats: a pinned token's id,
/// or `default`.
struct Pick {
    key: String,
    token: Option<String>,
    pinned: bool,
}

fn pick(route: Option<&str>) -> Pick {
    let c = config();
    if let Some(id) = route.and_then(|r| c.assign.get(r)) {
        if let Some(t) = c.tokens.iter().find(|t| &t.id == id) {
            return Pick { key: t.id.clone(), token: Some(t.token.clone()), pinned: true };
        }
    }
    Pick { key: "default".into(), token: current_token(), pinned: false }
}

/// What the proxy last saw, for the dialog (`stats`). Per route: which token
/// its last request used and how that went. Per token: the account it belongs
/// to and the rate-limit headers of its last reply.
#[derive(Default)]
struct Stats {
    routes: HashMap<String, RouteStat>,
    tokens: HashMap<String, TokenStat>,
}

#[derive(Clone, serde::Serialize)]
struct RouteStat {
    token: String,
    at: u64,
    status: u16,
}

#[derive(Clone, Default, serde::Serialize)]
struct TokenStat {
    at: u64,
    status: u16,
    org: Option<String>,
    /// The default token's fingerprint (it changes when the file does).
    fp: Option<String>,
    /// Every `anthropic-ratelimit-unified-*` header of the last reply that had any.
    limits: BTreeMap<String, String>,
}

static STATS: LazyLock<Mutex<Stats>> = LazyLock::new(Default::default);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn record(route: Option<&str>, pick: &Pick, token: Option<&str>, r: &reqwest::Response) {
    let at = now_ms();
    let status = r.status().as_u16();
    let mut s = STATS.lock().unwrap();
    s.routes.insert(
        route.unwrap_or("").to_string(),
        RouteStat { token: pick.key.clone(), at, status },
    );
    let t = s.tokens.entry(pick.key.clone()).or_default();
    t.at = at;
    t.status = status;
    if !pick.pinned {
        t.fp = token.map(fingerprint);
    }
    let h = r.headers();
    if let Some(org) = h.get("anthropic-organization-id").and_then(|v| v.to_str().ok()) {
        t.org = Some(org.to_string());
    }
    let limits: BTreeMap<String, String> = h
        .iter()
        .filter_map(|(k, v)| {
            let k = k.as_str().strip_prefix("anthropic-ratelimit-unified-")?;
            Some((k.to_string(), v.to_str().ok()?.to_string()))
        })
        .collect();
    if !limits.is_empty() {
        t.limits = limits;
    }
}

/// The `live_token_stats` op: what [`record`] has seen. Route `""` is the
/// claudes that send no route.
pub fn stats() -> Value {
    let s = STATS.lock().unwrap();
    json!({"routes": s.routes, "tokens": s.tokens})
}

fn upstream_base() -> String {
    let u = config().upstream;
    let u = u.trim_end_matches('/');
    if u.is_empty() {
        "https://api.anthropic.com".to_string()
    } else {
        u.to_string()
    }
}

async fn handle(req: Request<Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
    let Some(p) = PROXY.get() else {
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE, "live token proxy not ready"));
    };
    let pq = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/").to_string();
    let Some(rest) = strip_secret(&pq, &p.secret) else {
        return Ok(plain(StatusCode::FORBIDDEN, "not a Mulpex claude"));
    };
    let (route, rest) = split_route(rest);
    let route = route.map(str::to_string);
    let url = format!("{}{}", upstream_base(), rest);
    let (parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return Ok(plain(StatusCode::BAD_REQUEST, &format!("read body: {e}"))),
    };
    // Only a request that already carries a bearer token (the OAuth path) is
    // rewritten; anything else (an API key, no auth) passes through as sent.
    let rewrite = parts.headers.contains_key(hyper::header::AUTHORIZATION);

    let mut chosen = if rewrite { Some(pick(route.as_deref())) } else { None };
    let token = chosen.as_ref().and_then(|p| p.token.clone());
    let mut used = token.clone();
    let mut resp = send(&parts, &url, &body, token.as_deref()).await;
    // A pinned token's failure goes back to its claude as is: pinned means
    // pinned. Only the default token is refreshed and retried.
    let pinned = chosen.as_ref().is_some_and(|p| p.pinned);
    if rewrite && !pinned && matches!(&resp, Ok(r) if r.status() == 401 || r.status() == 429) {
        // The account may have been switched (or run out): pull a fresh token
        // and, if it is a different one, try this request once more with it.
        refresh_now().await;
        let fresh = current_token();
        if fresh.is_some() && fresh != token {
            eprintln!(
                "[ptyd] live token: {} on {}, retrying with {}",
                resp.as_ref().map(|r| r.status().as_u16()).unwrap_or(0),
                token.as_deref().map(fingerprint).unwrap_or_default(),
                fresh.as_deref().map(fingerprint).unwrap_or_default(),
            );
            resp = send(&parts, &url, &body, fresh.as_deref()).await;
            used = fresh;
            if let Some(p) = chosen.as_mut() {
                p.token = used.clone();
            }
        }
    }
    if let (Some(p), Ok(r)) = (&chosen, &resp) {
        record(route.as_deref(), p, used.as_deref(), r);
    }
    Ok(match resp {
        Ok(r) => relay(r),
        Err(e) => plain(StatusCode::BAD_GATEWAY, &format!("live token proxy: {e}")),
    })
}

async fn send(
    parts: &hyper::http::request::Parts,
    url: &str,
    body: &Bytes,
    token: Option<&str>,
) -> reqwest::Result<reqwest::Response> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        if !hop_by_hop(k.as_str()) {
            headers.append(k.clone(), v.clone());
        }
    }
    if let Some(t) = token {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    client()
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(body.clone())
        .send()
        .await
}

/// Stream the upstream response back untouched (status, headers, body bytes —
/// no decompression, so `content-encoding`/`content-length` stay true).
fn relay(r: reqwest::Response) -> Response<Body> {
    let status = r.status();
    let mut out = Response::builder().status(status);
    for (k, v) in r.headers().iter() {
        let name = k.as_str();
        if name != "content-length" && hop_by_hop(name) {
            continue;
        }
        out = out.header(k, v);
    }
    use futures_util::TryStreamExt;
    let stream = r
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    out.body(BodyExt::boxed(StreamBody::new(stream)))
        .unwrap_or_else(|e| plain(StatusCode::BAD_GATEWAY, &e.to_string()))
}

/// When the refresh command last finished. Held across the run, so concurrent
/// callers wait for the one in flight instead of starting their own.
static LAST_REFRESH: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

async fn refresh_now() {
    let mut last = LAST_REFRESH.lock().await;
    if last.is_some_and(|t| t.elapsed() < REFRESH_DEBOUNCE) {
        return;
    }
    run_refresh_command().await;
    *last = Some(Instant::now());
}

async fn refresh_loop() {
    loop {
        tokio::time::sleep(REFRESH_INTERVAL).await;
        if config().enabled() {
            refresh_now().await;
        }
    }
}

async fn run_refresh_command() {
    let s = config();
    if !s.enabled() || s.refresh_command.trim().is_empty() {
        return;
    }
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(s.refresh_command.trim())
        .env_clear()
        .envs(s.refresh_env.iter().cloned())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ptyd] live token refresh: spawn: {e}");
            return;
        }
    };
    match tokio::time::timeout(REFRESH_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if !out.status.success() => {
            let err = String::from_utf8_lossy(&out.stderr);
            eprintln!("[ptyd] live token refresh: {} {}", out.status, err.trim());
        }
        Ok(Err(e)) => eprintln!("[ptyd] live token refresh: {e}"),
        Err(_) => eprintln!("[ptyd] live token refresh: timed out"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_prefix_is_required_and_stripped() {
        assert_eq!(strip_secret("/abc/v1/messages?beta=true", "abc"), Some("/v1/messages?beta=true"));
        assert_eq!(strip_secret("/abc", "abc"), Some("/"));
        assert_eq!(strip_secret("/abc?x=1", "abc"), Some("?x=1"));
        assert_eq!(strip_secret("/v1/messages", "abc"), None);
        assert_eq!(strip_secret("/abcd/v1/messages", "abc"), None);
    }

    #[test]
    fn a_route_is_split_off_the_front() {
        assert_eq!(split_route("/c/ab12-3/v1/messages?beta=true"), (Some("ab12-3"), "/v1/messages?beta=true"));
        assert_eq!(split_route("/c/ab12-3"), (Some("ab12-3"), "/"));
        assert_eq!(split_route("/c/ab12-3?x=1"), (Some("ab12-3"), "?x=1"));
        assert_eq!(split_route("/v1/messages"), (None, "/v1/messages"));
        assert_eq!(split_route("/c//v1"), (None, "/c//v1"));
    }

    #[test]
    fn a_pinned_route_gets_its_token_and_others_the_default() {
        *CONFIG.write().unwrap() = Some(Config {
            tokens: vec![NamedToken { id: "t1".into(), token: "sk-ant-one".into() }],
            assign: HashMap::from([
                ("p-1".to_string(), "t1".to_string()),
                ("p-2".to_string(), "gone".to_string()),
            ]),
            ..Default::default()
        });
        let p = pick(Some("p-1"));
        assert!(p.pinned && p.key == "t1" && p.token.as_deref() == Some("sk-ant-one"));
        // Pinned to a token that no longer exists, unpinned, no route: default
        // (off here, so no token: the claude's own passes through).
        for r in [Some("p-2"), Some("p-3"), None] {
            let p = pick(r);
            assert!(!p.pinned && p.key == "default" && p.token.is_none());
        }
        *CONFIG.write().unwrap() = None;
    }

    /// Against the real API: `LT_REAL_TOKEN_FILE=<token file> cargo test -p
    /// mulpex-ptyd real_api -- --ignored --nocapture`. A route pinned to a good
    /// token, one pinned to a bad token (its 401 must come back as is, not be
    /// retried with the default), an unpinned route and a route-less request.
    #[test]
    #[ignore]
    fn real_api_routes_pick_their_token() {
        let file = std::env::var("LT_REAL_TOKEN_FILE").expect("LT_REAL_TOKEN_FILE");
        let good = read_token_file(std::path::Path::new(&file)).unwrap();
        configure(&json!({
            "token_file": file,
            "tokens": [{"id": "good", "token": good}, {"id": "bad", "token": "sk-ant-oat01-nope"}],
            "assign": {"p-1": "good", "p-2": "bad"},
        }))
        .unwrap();
        let p = PROXY.get().unwrap();
        let base = format!("http://127.0.0.1:{}/{}", p.port, p.secret);
        let ask = |path: String| {
            runtime().block_on(async move {
                client()
                    .post(path)
                    .header("authorization", "Bearer sk-ant-oat01-spawn-time")
                    .header("anthropic-beta", "oauth-2025-04-20")
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .body(r#"{"model":"claude-haiku-5-5","messages":[{"role":"user","content":"hi"}]}"#)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let url = |route: &str| format!("{base}{route}/v1/messages/count_tokens");
        assert_eq!(ask(url("/c/p-1")), 200);
        assert_eq!(ask(url("/c/p-2")), 401);
        assert_eq!(ask(url("/c/p-3")), 200);
        assert_eq!(ask(url("")), 200);
        let s = stats();
        println!("{}", serde_json::to_string_pretty(&s).unwrap());
        assert_eq!(s["routes"]["p-1"]["token"], "good");
        assert_eq!(s["routes"]["p-2"]["token"], "bad");
        assert_eq!(s["routes"]["p-2"]["status"], 401);
        assert_eq!(s["routes"]["p-3"]["token"], "default");
        assert_eq!(s["routes"][""]["token"], "default");
        assert!(s["tokens"]["good"]["org"].is_string());
        assert_eq!(s["tokens"]["default"]["fp"], fingerprint(&good));
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        for h in ["host", "connection", "content-length", "transfer-encoding"] {
            assert!(hop_by_hop(h));
        }
        for h in ["authorization", "anthropic-beta", "content-type", "x-api-key"] {
            assert!(!hop_by_hop(h));
        }
    }

    #[test]
    fn token_file_must_hold_a_token() {
        let dir = std::env::temp_dir().join(format!("mpx-lt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("token");
        std::fs::write(&f, "sk-ant-oat01-abc\n").unwrap();
        assert_eq!(read_token_file(&f).unwrap(), "sk-ant-oat01-abc");
        std::fs::write(&f, "").unwrap();
        assert!(read_token_file(&f).is_err());
        std::fs::write(&f, "<html>").unwrap();
        assert!(read_token_file(&f).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
