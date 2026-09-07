//! `exec-spec` — how a task and a system prompt reach `claude` under tmux.
//!
//! **Why this exists at all.** The obvious spawn is
//! `tmux new-window -- claude --append-system-prompt <14 KB> <task>`. It does not
//! work: tmux refuses with `command too long` once the whole command passes
//! ~16,340 bytes (measured 2026-09-05: 16,300 ok, 16,350 fails). That is
//! libimsg's `MAX_IMSGSIZE` (16,384) capping the tmux *client → server* message,
//! not `ARG_MAX` (1 MB here) and not a per-argument limit — a single 16,000-byte
//! argument passes fine. `HUB_RULES` + `PLANNING_RULES` is already 14,034 bytes,
//! so the naive form sits ~2 KB from the wall before any task text and a
//! `hub_spawn` task would blow it.
//!
//! This is the same shape as the 1022-character TTYHOG truncation recorded in
//! `docs/hub.md`: a delivery channel with an undocumented limit. It at least
//! fails loudly rather than silently truncating — but the rule from that episode
//! stands, so we do not negotiate with the limit, we route around it.
//!
//! **The route.** The daemon writes the real argv to a file and the tmux command
//! line carries only the path to it. Measured: the tmux command line becomes 282
//! bytes *and stays constant whatever the payload*, and a 14,034-byte system
//! prompt plus a 6,000-byte task arrive byte-identical.
//!
//! `exec()` replaces the process image, so `#{pane_pid}` and
//! `#{pane_current_command}` describe `claude`, not this launcher — which is what
//! keeps the liveness and is-a-command-running probes honest.
//!
//! **Env.** `tmux new-window -e` can only *set* a variable, never remove one, and
//! a child that inherits `CLAUDE_CODE_CHILD_SESSION` silently stops saving its
//! transcript — breaking `--resume` at the *next* launch, not this one. So the
//! spec carries an explicit unset list and this launcher applies it.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Everything needed to become `claude`. Serialized as JSON because a 14 KB
/// system prompt full of quotes, backslashes and newlines has to survive the
/// round trip untouched, and `serde_json` already escapes it correctly.
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: String,
    /// Variables to set. tmux `-e` also sets these; carrying them here too means
    /// the spec alone fully describes the child, which is what makes a failed
    /// spawn reproducible by hand.
    pub env: Vec<(String, String)>,
    /// Variables to remove from the inherited environment. See the module note.
    pub unset: Vec<String>,
}

impl Spec {
    pub fn to_json(&self) -> String {
        let env: serde_json::Map<String, serde_json::Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        serde_json::json!({
            "argv": self.argv,
            "cwd": self.cwd,
            "env": env,
            "unset": self.unset,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Spec> {
        let v: serde_json::Value = serde_json::from_str(text).context("spec is not valid JSON")?;
        let argv: Vec<String> = v["argv"]
            .as_array()
            .context("spec has no argv array")?
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect();
        anyhow::ensure!(!argv.is_empty(), "spec argv is empty");
        let env = v["env"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, val)| (k.clone(), val.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let unset = v["unset"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Spec {
            argv,
            cwd: v["cwd"].as_str().unwrap_or_default().to_string(),
            env,
            unset,
        })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json())
            .with_context(|| format!("writing spec to {}", path.display()))
    }
}

/// Read the spec at `path` and *become* the program it names.
///
/// Returns only on failure — on success this process is gone. The error is
/// printed by the caller into the pane, where `remain-on-exit` keeps it readable.
pub fn exec(path: &Path) -> Result<std::convert::Infallible> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spec {}", path.display()))?;
    let spec = Spec::from_json(&text)?;

    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    if !spec.cwd.is_empty() {
        cmd.current_dir(&spec.cwd);
    }
    for name in &spec.unset {
        cmd.env_remove(name);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // `exec` never returns on success.
    Err(anyhow::Error::new(cmd.exec())
        .context(format!("exec {}", spec.argv[0])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload sizes that broke the naive spawn must survive the round trip
    /// byte-for-byte. 14 KB is the real `--append-system-prompt`; the special
    /// characters are the ones a quoting bug would eat.
    #[test]
    fn a_large_prompt_and_task_survive_the_spec_round_trip() {
        let sysprompt = "quotes ' \" ` $ \\ ; | & > < newline\n".repeat(500);
        let task = "Refactor the parser; keep `state.rs` intact.\n".repeat(200);
        assert!(sysprompt.len() > 14_000, "prompt should exceed the tmux cap");
        let spec = Spec {
            argv: vec![
                "/bin/claude".into(),
                "--append-system-prompt".into(),
                sysprompt.clone(),
                task.clone(),
            ],
            cwd: "/tmp".into(),
            env: vec![("MULPEX_INSTANCE_ID".into(), "3".into())],
            unset: vec!["CLAUDE_CODE_CHILD_SESSION".into()],
        };
        let back = Spec::from_json(&spec.to_json()).expect("round trip");
        assert_eq!(back.argv[2], sysprompt);
        assert_eq!(back.argv[3], task);
        assert_eq!(back.unset, vec!["CLAUDE_CODE_CHILD_SESSION".to_string()]);
        assert_eq!(back.env[0].1, "3");
    }

    #[test]
    fn an_empty_argv_is_rejected_rather_than_execed() {
        let json = r#"{"argv":[],"cwd":"/","env":{},"unset":[]}"#;
        assert!(Spec::from_json(json).is_err());
    }
}
