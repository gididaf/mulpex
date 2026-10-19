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
    Status { settings, running: PROXY.lock().unwrap().is_some(), token_problem }
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
    });
    let r: Value = crate::ptyd::configure_live_token(config)?;
    if let (Some(port), Some(secret)) = (r["port"].as_u64(), r["secret"].as_str()) {
        *PROXY.lock().unwrap() = Some((port as u16, secret.to_string()));
    }
    Ok(())
}

/// The env a spawned claude gets on top of its usual one, or `None` when the
/// feature is off (then nothing about the spawn changes).
pub fn spawn_env() -> Option<Vec<(String, String)>> {
    if !settings().enabled() {
        return None;
    }
    let (port, secret) = PROXY.lock().unwrap().clone()?;
    let mut env = vec![(
        "ANTHROPIC_BASE_URL".to_string(),
        format!("http://127.0.0.1:{port}/{secret}"),
    )];
    // Keep ToolSearch as it is against the real API, unless the user chose.
    if !claude_bin::forwarded_env().iter().any(|(k, _)| k == "ENABLE_TOOL_SEARCH") {
        env.push(("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()));
    }
    Some(env)
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
    fn settings_parse_missing_fields_as_empty() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        let s: Settings = serde_json::from_str(r#"{"token_file":" ~/t "}"#).unwrap();
        assert_eq!(s.normalized().token_file, "~/t");
    }
}
