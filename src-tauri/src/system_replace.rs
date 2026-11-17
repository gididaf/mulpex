//! Mulpex ▸ System Text Replacements…: `--replace OLD NEW` pairs added to every
//! interactive `claude` this Mulpex starts. See docs/system-replace.md.
//!
//! `--replace` is NOT a Claude Code flag. It belongs to a launcher some users put
//! in front of `claude`, which rewrites the system text and then execs the real
//! binary. A plain `claude` rejects it, so the feature is off unless switched on,
//! and with it off not one argument changes.
//!
//! Only interactive claudes get the pairs (⌘T, restore, hub_spawn, ⌘⇧R, ⌘L) —
//! the hidden `-p` runs (save, explain, ⌘⇧B summary, handoff, Import Docs) do not.
//! A change applies on the next launch; running claudes keep what they started with.

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use mulpex_core::agent::Agent;

/// One OLD → NEW pair. Literal and case-sensitive; an empty `new` deletes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub old: String,
    #[serde(default)]
    pub new: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Saved as `<mulpex home>/system-replace.json` (the machine's own — on a remote
/// host's tab, the remote machine's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The master switch. Off (the default) = no `--replace` anywhere.
    #[serde(default)]
    pub enabled: bool,
    /// Applied in this order.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Settings {
    /// The extra argv, in order. Empty unless switched on.
    pub fn args(&self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        self.rules
            .iter()
            .filter(|r| r.enabled && !r.old.is_empty())
            .flat_map(|r| ["--replace".to_string(), r.old.clone(), r.new.clone()])
            .collect()
    }

    fn validate(&self) -> Result<(), String> {
        match self.rules.iter().position(|r| r.old.is_empty()) {
            Some(i) => Err(format!("rule {}: OLD must not be empty", i + 1)),
            None => Ok(()),
        }
    }
}

fn settings_path() -> PathBuf {
    mulpex_core::mulpex_home().join("system-replace.json")
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
        .unwrap_or_default();
    SETTINGS.write().unwrap().get_or_insert(s).clone()
}

/// Save new settings. Used from the next launch on.
pub fn set_settings(s: Settings) -> Result<Settings, String> {
    s.validate()?;
    let path = settings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    *SETTINGS.write().unwrap() = Some(s.clone());
    Ok(s)
}

/// What an interactive claude spawn adds right after the binary.
pub fn args() -> Vec<String> {
    settings().args()
}

/// The dialog's Test result.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub ok: bool,
    /// The argv tested, OLD/NEW shown only as their lengths.
    pub command: String,
    /// stdout + stderr, trimmed.
    pub output: String,
}

/// The subcommand Test runs behind the pairs. Not `--version` or `--help`: a
/// plain `claude` answers those before it parses options, so it would "accept"
/// `--replace` (measured, 2.1.326). `plugin list` parses them first — a plain
/// `claude` exits 1 with `unknown option '--replace'` — and is read-only and
/// quick (~2 s through the launcher).
const TEST_SUBCOMMAND: [&str; 2] = ["plugin", "list"];

/// Run `<claude> <the given pairs> plugin list` the way a spawn would find
/// `claude`, and report whether it accepted them. Tests the dialog's UNSAVED
/// rules (with at least one pair, ticked or not), so a user can check before
/// saving. Not a tty: on a hash mismatch the launcher prints its manual-recovery
/// hint instead of offering an interactive repair.
pub fn test(s: &Settings) -> Result<TestResult, String> {
    s.validate()?;
    let mut args = Settings { enabled: true, ..s.clone() }.args();
    if args.is_empty() {
        // Nothing ticked: still find out whether this `claude` takes the flag.
        args = ["--replace", "mulpex-test-old", "mulpex-test-new"].map(String::from).to_vec();
    }
    let bin = crate::headless::binary(Agent::Claude)?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let mut cmd = crate::headless::command(Agent::Claude, &home, None)?;
    cmd.args(&args).args(TEST_SUBCOMMAND);
    let command = format!(
        "{} --replace … ×{} {}",
        bin.display(),
        args.len() / 3,
        TEST_SUBCOMMAND.join(" ")
    );
    let (status, out, err) = crate::headless::run_child(cmd, "", Duration::from_secs(60))?;
    if status.success() {
        return Ok(TestResult { ok: true, command, output: "claude accepted --replace.".into() });
    }
    let output = [out.trim(), err.trim()].iter().filter(|t| !t.is_empty()).cloned().collect::<Vec<_>>().join("\n");
    Ok(TestResult { ok: false, command, output: if output.is_empty() { format!("{status}") } else { output } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(old: &str, new: &str, enabled: bool) -> Rule {
        Rule { old: old.into(), new: new.into(), enabled }
    }

    #[test]
    fn missing_file_fields_parse_as_off() {
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        let s: Settings = serde_json::from_str(r#"{"enabled":true,"rules":[{"old":"a"}]}"#).unwrap();
        assert_eq!(s.rules, vec![rule("a", "", true)]);
    }

    /// The coworker case: nothing configured, or configured and switched off,
    /// adds not one argument to their `claude`.
    #[test]
    fn off_adds_nothing() {
        assert!(Settings::default().args().is_empty());
        let s = Settings { enabled: false, rules: vec![rule("a", "b", true)] };
        assert!(s.args().is_empty());
    }

    #[test]
    fn enabled_rules_in_order_empty_new_kept() {
        let s = Settings {
            enabled: true,
            rules: vec![rule("a", "b", true), rule("skip", "x", false), rule("c\nd", "", true)],
        };
        assert_eq!(s.args(), ["--replace", "a", "b", "--replace", "c\nd", ""]);
    }

    /// Runs the real `claude` on this machine: `--ignored` only. Prints what the
    /// dialog's Test would show.
    #[test]
    #[ignore]
    fn test_button_against_the_installed_claude() {
        let r = test(&Settings { enabled: false, rules: vec![rule("a", "b", false)] }).unwrap();
        eprintln!("ok={} command={}\n{}", r.ok, r.command, r.output);
    }

    #[test]
    fn empty_old_is_refused() {
        let s = Settings { enabled: true, rules: vec![rule("a", "b", true), rule("", "x", false)] };
        assert_eq!(s.validate().unwrap_err(), "rule 2: OLD must not be empty");
    }
}
