//! Live Token: switch the Claude account every Mulpex claude uses **without
//! restarting any of them** (Mulpex ▸ Live Token…, docs/live-token.md).
//!
//! A `claude` reads `CLAUDE_CODE_OAUTH_TOKEN` once, at start. So when the token
//! in the user's token file changes (an account switch on their token manager),
//! every running claude keeps the old one until it is restarted. Off by default:
//! with no token file configured, nothing here runs and no spawn changes.
//!
//! When it is on, Mulpex runs a small HTTP proxy on `127.0.0.1` and every claude
//! it spawns gets `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>`. The
//! proxy re-reads the token file (only when its mtime changes) and puts the
//! current token on each request's `Authorization` header before forwarding to
//! the real API. The next request after a switch already uses the new account.
//!
//! What was measured (claude 2.1.301, 2026-10-16; docs/verification-log.md):
//! - `ANTHROPIC_UNIX_SOCKET` (the `claude ssh` path) does NOT work for this:
//!   inference over the socket is TLS to `api.anthropic.com`, so a proxy there
//!   can only tunnel, never rewrite a header.
//! - `ANTHROPIC_BASE_URL` does: `/v1/messages` arrives in plain HTTP with
//!   `Authorization: Bearer …` and the oauth beta header, and a token swapped in
//!   the file between two turns is used on the very next turn.
//! - `/api/oauth/*`, bootstrap and telemetry ignore the base URL and always go
//!   straight to `api.anthropic.com` with the env token. That is why the env
//!   token is left as it is (the real, spawn-time one) rather than replaced by a
//!   placeholder: a placeholder 401s those calls.
//! - A base URL that is not first-party turns ToolSearch's optimistic mode off;
//!   `ENABLE_TOOL_SEARCH=true` restores the first-party default.
//! - The base URL's path prefix is kept on every request, which is what lets the
//!   `<secret>` prefix restrict the proxy to the claudes Mulpex spawned.

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::claude_bin;

/// Where requests go when the user's own environment names no other API.
const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// How often the refresh command runs on its own.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// A refresh command that takes longer than this is killed.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
/// A 401/429 within this long of the last refresh does not run another one —
/// a burst of failing requests shares one refresh.
const REFRESH_DEBOUNCE: Duration = Duration::from_secs(5);

/// The two settings, saved as `<mulpex home>/live-token.json`. Both paths are
/// the machine's own (on a remote host's tab, the remote machine's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The file holding the current token. Empty = the feature is off.
    #[serde(default)]
    pub token_file: String,
    /// Optional shell command that rewrites `token_file` (run every minute, and
    /// at once on a 401 or 429).
    #[serde(default)]
    pub refresh_command: String,
}

impl Settings {
    pub fn enabled(&self) -> bool {
        !self.token_file.trim().is_empty()
    }

    fn normalized(self) -> Self {
        Settings {
            token_file: self.token_file.trim().to_string(),
            refresh_command: self.refresh_command.trim().to_string(),
        }
    }
}

fn settings_path() -> PathBuf {
    mulpex_core::mulpex_home().join("live-token.json")
}

static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);

/// The current settings (read from disk once, then kept in memory).
pub fn settings() -> Settings {
    if let Some(s) = SETTINGS.read().unwrap().as_ref() {
        return s.clone();
    }
    let s = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default()
        .normalized();
    SETTINGS.write().unwrap().get_or_insert(s).clone()
}

/// Save new settings and, if they turn the feature on, start the proxy.
pub fn set_settings(s: Settings) -> Result<(), String> {
    let s = s.normalized();
    let path = settings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    let on = s.enabled();
    *SETTINGS.write().unwrap() = Some(s);
    *TOKEN.lock().unwrap() = TokenCache::default();
    if on {
        ensure_started()?;
    }
    Ok(())
}

/// What the dialog shows: the settings plus whether the token file reads.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub settings: Settings,
    /// The proxy is listening (it stays up once started, even if turned off,
    /// so claudes already pointed at it keep working).
    pub running: bool,
    /// Why the token file cannot be used right now, if it can't.
    pub token_problem: Option<String>,
}

pub fn status() -> Status {
    let settings = settings();
    let token_problem = if settings.enabled() {
        match read_token_file(&expand(&settings.token_file)) {
            Ok(_) => None,
            Err(e) => Some(e),
        }
    } else {
        None
    };
    Status { settings, running: PROXY.get().is_some(), token_problem }
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

fn read_token_file(path: &std::path::Path) -> Result<String, String> {
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
    let s = settings();
    if !s.enabled() {
        return None;
    }
    let path = expand(&s.token_file);
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

struct Proxy {
    port: u16,
    secret: String,
}

static PROXY: OnceLock<Proxy> = OnceLock::new();

/// The env a spawned claude gets on top of its usual one, or `None` when the
/// feature is off (then nothing about the spawn changes).
pub fn spawn_env() -> Option<Vec<(String, String)>> {
    if !settings().enabled() {
        return None;
    }
    let p = PROXY.get()?;
    let mut env = vec![(
        "ANTHROPIC_BASE_URL".to_string(),
        format!("http://127.0.0.1:{}/{}", p.port, p.secret),
    )];
    // Keep ToolSearch as it is against the real API, unless the user chose.
    if !claude_bin::forwarded_env().iter().any(|(k, _)| k == "ENABLE_TOOL_SEARCH") {
        env.push(("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()));
    }
    Some(env)
}

/// Start the proxy if the feature is on. Called from `setup()` before any
/// project restores, so restored claudes are already pointed at it.
pub fn init() {
    if settings().enabled() {
        if let Err(e) = ensure_started() {
            eprintln!("[mulpex] live token: {e}");
        }
    }
}

/// Bind the listener (synchronously, so the port is known before any spawn)
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
        return Ok(()); // another thread won the race; its listener serves
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[mulpex] live token proxy: {e}");
                return;
            }
        };
        serve(listener).await;
    });
    tauri::async_runtime::spawn(refresh_loop());
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
fn hop_by_hop(name: &str) -> bool {
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
fn strip_secret<'a>(path_and_query: &'a str, secret: &str) -> Option<&'a str> {
    let rest = path_and_query.strip_prefix('/')?.strip_prefix(secret)?;
    if rest.is_empty() {
        Some("/")
    } else if rest.starts_with('/') || rest.starts_with('?') {
        Some(rest)
    } else {
        None
    }
}

/// The API the claude would have used without us: the user's own
/// `ANTHROPIC_BASE_URL` if their environment sets one, else the real API.
fn upstream_base() -> String {
    claude_bin::forwarded_env()
        .into_iter()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .map(|(_, v)| v.trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string())
}

async fn handle(req: Request<Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
    let Some(p) = PROXY.get() else {
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE, "live token proxy not ready"));
    };
    let pq = req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/").to_string();
    let Some(rest) = strip_secret(&pq, &p.secret) else {
        return Ok(plain(StatusCode::FORBIDDEN, "not a Mulpex claude"));
    };
    let url = format!("{}{}", upstream_base(), rest);
    let (parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return Ok(plain(StatusCode::BAD_REQUEST, &format!("read body: {e}"))),
    };
    // Only a request that already carries a bearer token (the OAuth path) is
    // rewritten; anything else (an API key, no auth) passes through as sent.
    let rewrite = parts.headers.contains_key(hyper::header::AUTHORIZATION);

    let token = if rewrite { current_token() } else { None };
    let mut resp = send(&parts, &url, &body, token.as_deref()).await;
    if rewrite && matches!(&resp, Ok(r) if r.status() == 401 || r.status() == 429) {
        // The account may have been switched (or run out): pull a fresh token
        // and, if it is a different one, try this request once more with it.
        refresh_now().await;
        let fresh = current_token();
        if fresh.is_some() && fresh != token {
            eprintln!(
                "[mulpex] live token: {} on {}, retrying with {}",
                resp.as_ref().map(|r| r.status().as_u16()).unwrap_or(0),
                token.as_deref().map(fingerprint).unwrap_or_default(),
                fresh.as_deref().map(fingerprint).unwrap_or_default(),
            );
            resp = send(&parts, &url, &body, fresh.as_deref()).await;
        }
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

/// Run the refresh command now, unless one finished within `REFRESH_DEBOUNCE`.
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
        if settings().enabled() {
            refresh_now().await;
        }
    }
}

async fn run_refresh_command() {
    let s = settings();
    if !s.enabled() || s.refresh_command.is_empty() {
        return;
    }
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&s.refresh_command)
        .env_clear()
        .envs(claude_bin::forwarded_env())
        .env("PATH", claude_bin::merged_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[mulpex] live token refresh: spawn: {e}");
            return;
        }
    };
    match tokio::time::timeout(REFRESH_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if !out.status.success() => {
            let err = String::from_utf8_lossy(&out.stderr);
            eprintln!("[mulpex] live token refresh: {} {}", out.status, err.trim());
        }
        Ok(Err(e)) => eprintln!("[mulpex] live token refresh: {e}"),
        Err(_) => eprintln!("[mulpex] live token refresh: timed out"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_a_token_file_is_set() {
        assert!(!Settings::default().enabled());
        assert!(!Settings { token_file: "  ".into(), refresh_command: "x".into() }.enabled());
        assert!(Settings { token_file: "~/t".into(), ..Default::default() }.enabled());
    }

    #[test]
    fn settings_parse_missing_fields_as_empty() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        let s: Settings = serde_json::from_str(r#"{"token_file":" ~/t "}"#).unwrap();
        assert_eq!(s.normalized().token_file, "~/t");
    }

    #[test]
    fn secret_prefix_is_required_and_stripped() {
        assert_eq!(strip_secret("/abc/v1/messages?beta=true", "abc"), Some("/v1/messages?beta=true"));
        assert_eq!(strip_secret("/abc", "abc"), Some("/"));
        assert_eq!(strip_secret("/abc?x=1", "abc"), Some("?x=1"));
        assert_eq!(strip_secret("/v1/messages", "abc"), None);
        assert_eq!(strip_secret("/abcd/v1/messages", "abc"), None);
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

    /// Manual end-to-end harness: serves the real proxy for `LT_SERVE_SECS`
    /// with `MULPEX_HOME`'s settings and writes the base URL a claude should
    /// use to `$MULPEX_HOME/base-url`. Drive a real `claude` against it.
    #[test]
    #[ignore]
    fn serve_for_manual_e2e() {
        let secs: u64 = std::env::var("LT_SERVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        init();
        let env = spawn_env().expect("settings in MULPEX_HOME must turn the feature on");
        let url = &env.iter().find(|(k, _)| k == "ANTHROPIC_BASE_URL").unwrap().1;
        std::fs::write(mulpex_core::mulpex_home().join("base-url"), url).unwrap();
        std::thread::sleep(Duration::from_secs(secs));
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
}
