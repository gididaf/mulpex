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
//! **The proxy runs in the PTY daemon** (`mulpex_ptyd::daemon::live_token`), not
//! here: a claude outlives an app restart, and so must the port it calls. This
//! module owns the settings and pushes them, with the bits only the app can
//! work out from the login environment, to the daemon (`push`).
//!
//! **Named tokens.** Beside the token file (the "default"), the user can paste
//! more tokens with a name (`live-tokens.json`, 0600) and pin any claude to one
//! (`live-token-claudes.json`). A claude names itself to the proxy through its
//! base URL — `…/<secret>/c/<route>`, `route` = `<project key>-<id>` — so a pin
//! changes on its next request, with no restart. Claudes spawned before routes
//! existed send none and always get the default.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::claude_bin;

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

/// Save new settings and hand them to the daemon's proxy.
pub fn set_settings(s: Settings) -> Result<(), String> {
    let s = s.normalized();
    let path = settings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    *SETTINGS.write().unwrap() = Some(s);
    push()
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
    /// The pasted tokens, without the token itself.
    pub tokens: Vec<TokenInfo>,
    /// The daemon's proxy knows which claude is calling. False for a daemon
    /// from before routes (it keeps running until the next real quit).
    pub routes: bool,
}

/// A pasted token, stored in `live-tokens.json` (0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    id: String,
    name: String,
    token: String,
    #[serde(default)]
    org: Option<String>,
}

/// What the frontend sees of a stored token: never the token, only its end.
#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub tail: String,
    pub org: Option<String>,
}

fn tokens_path() -> PathBuf {
    mulpex_core::mulpex_home().join("live-tokens.json")
}

static TOKENS: RwLock<Option<Vec<StoredToken>>> = RwLock::new(None);

fn tokens() -> Vec<StoredToken> {
    if let Some(t) = TOKENS.read().unwrap().as_ref() {
        return t.clone();
    }
    let t = std::fs::read_to_string(tokens_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<StoredToken>>(&t).ok())
        .unwrap_or_default();
    TOKENS.write().unwrap().get_or_insert(t).clone()
}

fn save_tokens(t: Vec<StoredToken>) -> Result<(), String> {
    let path = tokens_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?;
    crate::secrets::write_private(&path, &text)?;
    *TOKENS.write().unwrap() = Some(t);
    Ok(())
}

fn info(t: &StoredToken) -> TokenInfo {
    let tail: String = t.token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    TokenInfo { id: t.id.clone(), name: t.name.clone(), tail, org: t.org.clone() }
}

/// Which claude is pinned to which token, and which claudes were spawned with
/// a route at all (`live-token-claudes.json`). Keyed by route, which outlives
/// an app restart and a ⌘⇧R; ⌘W forgets it (`forget`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Claudes {
    #[serde(default)]
    assign: BTreeMap<String, String>,
    #[serde(default)]
    routed: BTreeSet<String>,
}

fn claudes_path() -> PathBuf {
    mulpex_core::mulpex_home().join("live-token-claudes.json")
}

static CLAUDES: Mutex<Option<Claudes>> = Mutex::new(None);

/// Read-modify-write the claudes file. Writes only when `f` changed something.
fn with_claudes<R>(f: impl FnOnce(&mut Claudes) -> R) -> R {
    let mut g = CLAUDES.lock().unwrap();
    let c = g.get_or_insert_with(|| {
        std::fs::read_to_string(claudes_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    });
    let before = c.clone();
    let r = f(c);
    if *c != before {
        let path = claudes_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(c) {
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!("[mulpex] live token: write {}: {e}", path.display());
            }
        }
    }
    r
}

/// The route `claude#id` of the project in `dir` names itself by.
pub fn route(dir: &Path, id: usize) -> String {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    format!("{}-{id}", crate::ptyd::project_key(&canon))
}

/// Add a pasted token. Checked against the API first (a free `count_tokens`):
/// a token the API refuses is not stored.
pub fn add_token(name: &str, token: &str) -> Result<(), String> {
    let name = name.trim();
    let token = token.trim();
    if name.is_empty() {
        return Err("Give the token a name".into());
    }
    if !token.starts_with("sk-ant-") {
        return Err("A Claude token starts with sk-ant-".into());
    }
    let mut all = tokens();
    if all.iter().any(|t| t.token == token) {
        return Err("This token is already in the list".into());
    }
    let org = probe(token)?;
    let mut raw = [0u8; 6];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut raw))
            .map_err(|e| format!("/dev/urandom: {e}"))?;
    }
    let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    all.push(StoredToken { id, name: name.to_string(), token: token.to_string(), org });
    save_tokens(all)?;
    push()
}

pub fn rename_token(id: &str, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the token a name".into());
    }
    let mut all = tokens();
    let t = all.iter_mut().find(|t| t.id == id).ok_or("No such token")?;
    t.name = name.to_string();
    save_tokens(all)
}

/// Delete a pasted token. Claudes pinned to it go back to the default.
pub fn delete_token(id: &str) -> Result<(), String> {
    let mut all = tokens();
    all.retain(|t| t.id != id);
    save_tokens(all)?;
    with_claudes(|c| c.assign.retain(|_, t| t != id));
    push()
}

/// Pin `route`'s claude to token `id`, or (None) back to the default.
pub fn assign(route: &str, id: Option<&str>) -> Result<(), String> {
    if let Some(id) = id {
        if !tokens().iter().any(|t| t.id == id) {
            return Err("No such token".into());
        }
    }
    with_claudes(|c| match id {
        Some(id) => c.assign.insert(route.to_string(), id.to_string()),
        None => c.assign.remove(route),
    });
    push()
}

/// The token `route`'s claude is pinned to, if any.
pub fn assigned(route: &str) -> Option<String> {
    with_claudes(|c| c.assign.get(route).cloned())
}

/// Whether `route`'s claude was spawned with a route (else it is a claude
/// from before, which always gets the default until a ⌘⇧R).
pub fn routed(route: &str) -> bool {
    with_claudes(|c| c.routed.contains(route))
}

/// ⌘W / `hub_close`: the instance is gone for good, so a later claude with the
/// same number starts on the default. Writes nothing when there was nothing.
pub fn forget(route: &str) {
    let changed = with_claudes(|c| c.assign.remove(route).is_some() | c.routed.remove(route));
    if changed {
        let _ = push();
    }
}

/// Check a token with a free `count_tokens` call. Returns the account's
/// organization id (a reply header), which tells two tokens of one account
/// apart from two accounts. The profile endpoint would give a name, but a
/// `claude setup-token` token lacks its `user:profile` scope (measured).
fn probe(token: &str) -> Result<Option<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(20)))
        .build()
        .into();
    let body = r#"{"model":"claude-haiku-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = agent
        .post(&format!("{}/v1/messages/count_tokens", upstream_base()))
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send(body)
        .map_err(|e| format!("Could not reach the API: {e}"))?;
    let org = resp
        .headers()
        .get("anthropic-organization-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match resp.status().as_u16() {
        401 | 403 => Err(format!("The API refused this token ({})", resp.status().as_u16())),
        _ => Ok(org),
    }
}

pub fn status() -> Status {
    let settings = settings();
    let token_problem = if settings.enabled() {
        match mulpex_ptyd::daemon::live_token::read_token_file(&expand(&settings.token_file)) {
            Ok(_) => None,
            Err(e) => Some(e),
        }
    } else {
        None
    };
    Status {
        settings,
        running: PROXY.lock().unwrap().is_some(),
        token_problem,
        tokens: tokens().iter().map(info).collect(),
        routes: ROUTES.load(Ordering::Relaxed),
    }
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

/// Where the daemon's proxy listens, once it has started.
static PROXY: Mutex<Option<(u16, String)>> = Mutex::new(None);
/// The daemon's proxy understands `/c/<route>` (said in its `configure` reply).
static ROUTES: AtomicBool = AtomicBool::new(false);

/// The API the claude would have used without us: the user's own
/// `ANTHROPIC_BASE_URL` if their environment sets one, else the real API.
fn upstream_base() -> String {
    claude_bin::forwarded_env()
        .into_iter()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .map(|(_, v)| v.trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string())
}

/// Hand the daemon the effective config, and learn where its proxy listens.
/// Called at startup (before any project restores, so restored claudes are
/// spawned pointing at it) and on every settings change.
pub fn push() -> Result<(), String> {
    let s = settings();
    let mut refresh_env = claude_bin::forwarded_env();
    refresh_env.retain(|(k, _)| k != "PATH");
    refresh_env.push(("PATH".to_string(), claude_bin::merged_path().to_string()));
    let config = json!({
        "token_file": s.token_file,
        "refresh_command": s.refresh_command,
        "upstream": upstream_base(),
        "refresh_env": refresh_env,
        "tokens": tokens().iter().map(|t| json!({"id": t.id, "token": t.token})).collect::<Vec<_>>(),
        "assign": with_claudes(|c| c.assign.clone()),
    });
    let r: Value = crate::ptyd::configure_live_token(config)?;
    if let (Some(port), Some(secret)) = (r["port"].as_u64(), r["secret"].as_str()) {
        *PROXY.lock().unwrap() = Some((port as u16, secret.to_string()));
    }
    ROUTES.store(r["routes"].as_bool() == Some(true), Ordering::Relaxed);
    Ok(())
}

/// What the daemon's proxy has seen (`routes`, `tokens`), or an empty object
/// from a daemon that predates it.
pub fn daemon_stats() -> Value {
    crate::ptyd::live_token_stats().unwrap_or_else(|_| json!({}))
}

/// The env a spawned claude gets on top of its usual one, or `None` when the
/// feature is off (then nothing about the spawn changes). `route` names the
/// claude to the proxy (see `route`); `None` gets the default token. A daemon
/// from before routes would forward `/c/…` upstream, so then it is left off.
pub fn spawn_env(route: Option<&str>) -> Option<Vec<(String, String)>> {
    if !settings().enabled() {
        return None;
    }
    let (port, secret) = PROXY.lock().unwrap().clone()?;
    let path = match route {
        Some(r) if ROUTES.load(Ordering::Relaxed) => format!("{secret}/c/{r}"),
        _ => secret,
    };
    let mut env = vec![(
        "ANTHROPIC_BASE_URL".to_string(),
        format!("http://127.0.0.1:{port}/{path}"),
    )];
    // Keep ToolSearch as it is against the real API, unless the user chose.
    if !claude_bin::forwarded_env().iter().any(|(k, _)| k == "ENABLE_TOOL_SEARCH") {
        env.push(("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()));
    }
    Some(env)
}

/// `spawn_env` for an interactive claude, which also records whether it got a
/// route — the dialog marks the ones that did not as "restart to control".
pub fn claude_spawn_env(route: &str) -> Option<Vec<(String, String)>> {
    let env = spawn_env(Some(route));
    let with_route = env
        .as_ref()
        .is_some_and(|e| e.iter().any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v.contains("/c/")));
    // Only a change is written, and a claude spawned with the feature off is
    // left alone when it was never recorded (tests spawn plenty of those).
    with_claudes(|c| {
        if with_route {
            c.routed.insert(route.to_string());
        } else {
            c.routed.remove(route);
        }
    });
    env
}

/// Push the config to the daemon. Called from `setup()` before any project
/// restores.
pub fn init() {
    if let Err(e) = push() {
        eprintln!("[mulpex] live token: {e}");
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
    fn a_route_is_project_key_and_id() {
        let r = route(Path::new("/nonexistent/proj"), 7);
        assert_eq!(r, format!("{}-7", crate::ptyd::project_key(Path::new("/nonexistent/proj"))));
    }

    #[test]
    fn claudes_file_parses_missing_fields_as_empty() {
        let c: Claudes = serde_json::from_str("{}").unwrap();
        assert_eq!(c, Claudes::default());
    }

    #[test]
    fn settings_parse_missing_fields_as_empty() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        let s: Settings = serde_json::from_str(r#"{"token_file":" ~/t "}"#).unwrap();
        assert_eq!(s.normalized().token_file, "~/t");
    }
}
