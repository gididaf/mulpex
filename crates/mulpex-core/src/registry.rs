//! The workspace registry: what every *other* open project is, and the address
//! grammar that lets an instance name an instance inside one.
//!
//! ## Why this exists
//!
//! Until now the hub stopped at the project boundary, and not because anything
//! checked: an instance is handed exactly one `MULPEX_STATE_DIR` (`pty.rs`), every
//! hub path is `state_dir.join(…)`, and so another project's hub simply had no
//! name a child process could utter. Isolation by unreachability.
//!
//! Cross-project messaging needs the opposite — one place that says "these
//! projects are open, here is each one's state dir and who is live in it". Only
//! the app's poll loop can say that honestly (it is the sole context holding every
//! `Core` under one lock), so it publishes this file at the **state root**
//! (`temp/mulpex-<pid>/registry.json`, one level above each project's state dir)
//! and the helper reads it.
//!
//! Two properties fall out of that placement, both wanted:
//!
//! - The root is per *process* (`mulpex-<pid>`), so "reachable" means precisely
//!   "open in this Mulpex window". A second Mulpex is a separate universe, with no
//!   code required to make it so.
//! - A child knows its own `MULPEX_STATE_DIR = <root>/<handle>`, so it finds the
//!   registry with `parent()` and needs no new environment variable.
//!
//! Staleness is bounded by the poll cadence (~200 ms) — exactly the guarantee the
//! per-project `instances` file already gives, so validating a foreign recipient
//! here is no more of a race than validating a local one.
//!
//! ## The `#` collision
//!
//! Mulpex's fixed vocabulary writes an instance as `claude#3` / `term#5`, where
//! `#` separates *kind* from *number*. A cross-project address is `central-one#3`,
//! where the same `#` separates *project* from *number*. One character, two jobs.
//!
//! That is a trap rather than a wart, because `claude#3` is how the instance is
//! written everywhere else — in the sidebar, in `HUB_RULES`, in the user's own
//! prose — so a model will eventually put it in `to` meaning the local instance 3
//! and, under a naive grammar, be told there is no project called "claude". So the
//! kind words are resolved **before** any project lookup:
//!
//! - `claude#<n>` → the local instance `<n>`, identical to a bare `<n>`.
//! - `term#<n>` → refused, naming the tool that *can* reach a terminal. This
//!   promotes a rule `HUB_RULES` previously only stated into one that is enforced.
//!
//! The cost, accepted deliberately: a project literally named `claude` or `term`
//! cannot be addressed by its bare folder name. It stays reachable by path
//! qualifier (`dreamvps/claude#3`) or absolute path, which the error messages say.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Published by the poll loop at the state root, read by every helper.
pub const REGISTRY_FILE: &str = "registry.json";

/// The workspace root holding the registry, from an instance's own state dir
/// (`<root>/<handle>`).
pub fn state_root_of(state_dir: &Path) -> Option<&Path> {
    state_dir.parent()
}

/// One live claude instance in some project. Terminals are absent by design —
/// they are not hub peers, and `hub_send` must never be able to offer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceEntry {
    pub id: usize,
    pub status: String,
    pub task: String,
    pub name: Option<String>,
}

/// One open project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub handle: u64,
    /// Folder basename — the tab label, and the short form of an address.
    pub name: String,
    /// Canonical project directory. The stable identity; `name` can collide.
    pub dir: String,
    /// That project's `MULPEX_STATE_DIR`, i.e. where its `inbox/` lives.
    pub state_dir: String,
    pub instances: Vec<InstanceEntry>,
}

impl ProjectEntry {
    pub fn has_instance(&self, id: usize) -> bool {
        self.instances.iter().any(|i| i.id == id)
    }

    /// Is this the project rooted at `dir` — i.e. "is this me"?
    ///
    /// Must not be a string compare. The registry is written by the app and read
    /// by a helper whose `MULPEX_PROJECT_DIR` has been through
    /// `canonicalize` (`hook::Ctx::from_env`), and on macOS `/var/…` and
    /// `/private/var/…` are the same directory under two names — as are any
    /// symlinked project path and its target. Getting this wrong is quiet rather
    /// than loud: an instance would see its *own* project listed among the
    /// "other" ones and could address its peers two different ways. Caught by
    /// driving two real helpers, not by a unit test with hand-matched strings.
    pub fn is_dir(&self, dir: &Path) -> bool {
        same_dir(Path::new(&self.dir), dir)
    }

    /// How an instance in ANOTHER project must address `id` here.
    pub fn address(&self, id: usize) -> String {
        format!("{}#{id}", self.name)
    }

    pub fn inbox_dir(&self, id: usize) -> PathBuf {
        Path::new(&self.state_dir).join("inbox").join(id.to_string())
    }

    pub fn messages_log(&self) -> PathBuf {
        Path::new(&self.state_dir).join("messages.log")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    pub fn read(state_root: &Path) -> Registry {
        std::fs::read_to_string(state_root.join(REGISTRY_FILE))
            .ok()
            .map(|t| Registry::from_json(&t))
            .unwrap_or_default()
    }

    /// Read the registry from an instance's own state dir.
    pub fn read_for(state_dir: &Path) -> Registry {
        state_root_of(state_dir).map(Registry::read).unwrap_or_default()
    }

    /// Write only when the bytes actually change, so a quiet tick costs one
    /// string compare rather than a disk write every 200 ms. Temp+rename, so a
    /// helper reading concurrently never sees a half-written file.
    /// Returns whether anything was written.
    pub fn write_if_changed(state_root: &Path, reg: &Registry) -> bool {
        let path = state_root.join(REGISTRY_FILE);
        let text = reg.to_json();
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if existing == text {
                return false;
            }
        }
        if std::fs::create_dir_all(state_root).is_err() {
            return false;
        }
        let tmp = state_root.join(format!("{REGISTRY_FILE}.tmp"));
        if std::fs::write(&tmp, &text).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &path).is_ok()
    }

    pub fn to_json(&self) -> String {
        let projects: Vec<Value> = self
            .projects
            .iter()
            .map(|p| {
                let instances: Vec<Value> = p
                    .instances
                    .iter()
                    .map(|i| {
                        json!({
                            "id": i.id,
                            "status": i.status,
                            "task": i.task,
                            "name": i.name,
                        })
                    })
                    .collect();
                json!({
                    "handle": p.handle,
                    "name": p.name,
                    "dir": p.dir,
                    "state_dir": p.state_dir,
                    "instances": instances,
                })
            })
            .collect();
        json!({ "projects": projects }).to_string()
    }

    pub fn from_json(text: &str) -> Registry {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Registry::default();
        };
        let projects = v
            .get("projects")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let instances = p
                            .get("instances")
                            .and_then(|i| i.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|i| {
                                        Some(InstanceEntry {
                                            id: i.get("id")?.as_u64()? as usize,
                                            status: str_of(i, "status", "waiting"),
                                            task: str_of(i, "task", ""),
                                            name: i
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .map(str::to_string),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(ProjectEntry {
                            handle: p.get("handle").and_then(|h| h.as_u64()).unwrap_or(0),
                            name: str_of(p, "name", ""),
                            dir: p.get("dir")?.as_str()?.to_string(),
                            state_dir: p.get("state_dir")?.as_str()?.to_string(),
                            instances,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Registry { projects }
    }

    /// The entry for the project rooted at `dir`, i.e. "which one am I".
    pub fn project_for_dir(&self, dir: &Path) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.is_dir(dir))
    }

    /// Resolve a project qualifier from an address.
    ///
    /// Exact directory first, then a trailing path-component match — which covers
    /// the bare folder name (`cloud`) and any longer disambiguating tail
    /// (`dreamvps/cloud`) with one rule. Ambiguity is an error naming the
    /// candidates' full paths rather than a silent pick: sending a message to the
    /// wrong repository is a wrong answer, not an inconvenience.
    pub fn resolve(&self, qualifier: &str) -> Result<&ProjectEntry, String> {
        let q = qualifier.trim();
        if let Some(p) = self.projects.iter().find(|p| p.dir == q) {
            return Ok(p);
        }
        let want = components(q);
        if want.is_empty() {
            return Err(self.unknown_project(q));
        }
        let matches: Vec<&ProjectEntry> = self
            .projects
            .iter()
            .filter(|p| ends_with_components(&p.dir, &want))
            .collect();
        match matches.len() {
            0 => Err(self.unknown_project(q)),
            1 => Ok(matches[0]),
            _ => {
                let paths: Vec<String> = matches.iter().map(|p| p.dir.clone()).collect();
                Err(format!(
                    "\"{q}\" is ambiguous — {} open projects match: {}. Re-send with enough of \
                     the path to be unique (e.g. \"{}#<n>\") or with the full path.",
                    paths.len(),
                    paths.join(", "),
                    disambiguating_suffix(&paths),
                ))
            }
        }
    }

    fn unknown_project(&self, q: &str) -> String {
        if self.projects.is_empty() {
            return format!(
                "no open project matches \"{q}\" — no other project is open in Mulpex right now."
            );
        }
        let open: Vec<String> = self
            .projects
            .iter()
            .map(|p| format!("{} ({})", p.name, p.dir))
            .collect();
        format!(
            "no open project matches \"{q}\". Open projects: {}. Call \
             mcp__mulpex__hub_instances to see them with their instances.",
            open.join(", ")
        )
    }
}

fn str_of(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or(default)
        .to_string()
}

/// A message recipient, as written in `hub_send`'s `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// An instance in my own project: `"3"` or `"claude#3"`.
    Local(usize),
    /// Every other instance in my own project: `"all"`.
    LocalAll,
    /// An instance in another open project: `"central-one#3"`.
    Foreign { qualifier: String, id: usize },
}

const ADDRESS_HELP: &str = "Use a number (\"3\") or \"claude#3\" for an instance in THIS project, \
     \"<project>#<n>\" (e.g. \"central-one#3\") for one in another open project, or \"all\" to \
     reach every other instance in this project.";

/// Parse `hub_send`'s `to`. See the module docs for why the kind words are
/// resolved before any project lookup.
pub fn parse_address(raw: &str) -> Result<Address, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(format!("'to' is empty. {ADDRESS_HELP}"));
    }
    if t.eq_ignore_ascii_case("all") {
        return Ok(Address::LocalAll);
    }
    if let Ok(id) = t.parse::<usize>() {
        return Ok(Address::Local(id));
    }
    // Split on the LAST '#', so a path qualifier or an exotic folder name can
    // itself contain one.
    let Some((qual, rest)) = t.rsplit_once('#') else {
        return Err(format!("\"{t}\" is not a valid address. {ADDRESS_HELP}"));
    };
    let qual = qual.trim();
    let rest = rest.trim();

    if qual.eq_ignore_ascii_case("term") || qual.eq_ignore_ascii_case("terminal") {
        return Err(format!(
            "\"{t}\" is a terminal, not a hub instance — hub_send can never reach one. Use \
             mcp__mulpex__hub_terminal_send to type into it, or address a claude instance \
             instead."
        ));
    }
    if qual.eq_ignore_ascii_case("claude") && rest.eq_ignore_ascii_case("all") {
        return Ok(Address::LocalAll);
    }
    if rest.eq_ignore_ascii_case("all") {
        return Err(format!(
            "\"{t}\" is not allowed: a broadcast is always project-local, because a message is \
             mandatory reading for whoever gets it and one project must not be able to stall \
             another. Use to: \"all\" for every instance in YOUR project, or name a single \
             instance (e.g. \"{qual}#3\")."
        ));
    }
    let Ok(id) = rest.parse::<usize>() else {
        return Err(format!("\"{t}\" is not a valid address. {ADDRESS_HELP}"));
    };
    if qual.eq_ignore_ascii_case("claude") {
        return Ok(Address::Local(id));
    }
    if qual.is_empty() {
        return Err(format!("\"{t}\" is missing a project name. {ADDRESS_HELP}"));
    }
    Ok(Address::Foreign {
        qualifier: qual.to_string(),
        id,
    })
}

/// Two paths naming the same directory, allowing for symlinks and macOS's
/// `/var` → `/private/var`. Falls back to a literal compare when either path no
/// longer exists, which is the conservative answer: "not the same".
pub fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn components(s: &str) -> Vec<String> {
    s.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

fn ends_with_components(dir: &str, want: &[String]) -> bool {
    let have = components(dir);
    if want.is_empty() || want.len() > have.len() {
        return false;
    }
    have[have.len() - want.len()..]
        .iter()
        .zip(want)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// The shortest trailing path fragment that tells the first candidate apart from
/// the rest — so the ambiguity error can suggest something that actually works
/// rather than a generic "be more specific".
fn disambiguating_suffix(paths: &[String]) -> String {
    let first = components(&paths[0]);
    for take in 1..=first.len() {
        let want: Vec<String> = first[first.len() - take..].to_vec();
        let hits = paths
            .iter()
            .filter(|p| ends_with_components(p, &want))
            .count();
        if hits == 1 {
            return want.join("/");
        }
    }
    paths[0].clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(id: usize) -> InstanceEntry {
        InstanceEntry {
            id,
            status: "waiting".into(),
            task: String::new(),
            name: None,
        }
    }

    fn proj(name: &str, dir: &str, ids: &[usize]) -> ProjectEntry {
        ProjectEntry {
            handle: 1,
            name: name.into(),
            dir: dir.into(),
            state_dir: format!("/tmp/mulpex-1/{name}"),
            instances: ids.iter().copied().map(inst).collect(),
        }
    }

    fn two_projects() -> Registry {
        Registry {
            projects: vec![
                proj("cloud", "/Users/g/Code/dreamvps/cloud", &[1, 2]),
                proj("central-one", "/Users/g/Code/dreamvps/central-one", &[1]),
            ],
        }
    }

    // ---- address grammar, in the order the parser resolves it --------------

    #[test]
    fn all_is_a_project_local_broadcast() {
        assert_eq!(parse_address("all").unwrap(), Address::LocalAll);
        assert_eq!(parse_address(" ALL ").unwrap(), Address::LocalAll);
        // `claude#all` is the same thought spelled with the kind word.
        assert_eq!(parse_address("claude#all").unwrap(), Address::LocalAll);
    }

    #[test]
    fn a_bare_number_is_local() {
        assert_eq!(parse_address("3").unwrap(), Address::Local(3));
        assert_eq!(parse_address(" 12 ").unwrap(), Address::Local(12));
    }

    /// The trap the `#` collision creates: `claude#3` is how an instance is
    /// written everywhere, so it must mean the LOCAL instance 3 — never a lookup
    /// for a project named "claude".
    #[test]
    fn claude_hash_n_is_the_local_instance_not_a_project() {
        assert_eq!(parse_address("claude#3").unwrap(), Address::Local(3));
        assert_eq!(parse_address("Claude#3").unwrap(), Address::Local(3));
        assert_eq!(parse_address("claude # 3").unwrap(), Address::Local(3));
    }

    #[test]
    fn term_hash_n_is_refused_and_names_the_right_tool() {
        let err = parse_address("term#5").unwrap_err();
        assert!(err.contains("not a hub instance"), "{err}");
        assert!(err.contains("hub_terminal_send"), "{err}");
        assert!(parse_address("terminal#5").is_err());
    }

    #[test]
    fn a_qualified_address_is_foreign() {
        assert_eq!(
            parse_address("central-one#3").unwrap(),
            Address::Foreign {
                qualifier: "central-one".into(),
                id: 3
            }
        );
        assert_eq!(
            parse_address("dreamvps/cloud#2").unwrap(),
            Address::Foreign {
                qualifier: "dreamvps/cloud".into(),
                id: 2
            }
        );
    }

    #[test]
    fn a_cross_project_broadcast_is_refused_with_the_reason() {
        let err = parse_address("central-one#all").unwrap_err();
        assert!(err.contains("project-local"), "{err}");
        assert!(err.contains("mandatory reading"), "{err}");
    }

    #[test]
    fn nonsense_addresses_are_rejected() {
        assert!(parse_address("").is_err());
        assert!(parse_address("   ").is_err());
        assert!(parse_address("nope").is_err());
        assert!(parse_address("cloud#").is_err());
        assert!(parse_address("cloud#x").is_err());
        assert!(parse_address("#3").is_err());
    }

    // ---- project resolution ------------------------------------------------

    #[test]
    fn resolves_by_basename_path_suffix_and_full_path() {
        let r = two_projects();
        assert_eq!(r.resolve("cloud").unwrap().name, "cloud");
        assert_eq!(r.resolve("CLOUD").unwrap().name, "cloud");
        assert_eq!(r.resolve("dreamvps/cloud").unwrap().name, "cloud");
        assert_eq!(
            r.resolve("/Users/g/Code/dreamvps/central-one").unwrap().name,
            "central-one"
        );
    }

    #[test]
    fn an_ambiguous_name_errors_with_both_full_paths_and_a_usable_suggestion() {
        let r = Registry {
            projects: vec![
                proj("cloud", "/Users/g/Code/dreamvps/cloud", &[1]),
                proj("cloud", "/Users/g/Archive/cloud", &[1]),
            ],
        };
        let err = r.resolve("cloud").unwrap_err();
        assert!(err.contains("/Users/g/Code/dreamvps/cloud"), "{err}");
        assert!(err.contains("/Users/g/Archive/cloud"), "{err}");
        // And the suggestion it makes must itself resolve.
        assert!(err.contains("dreamvps/cloud#<n>"), "{err}");
        assert_eq!(r.resolve("dreamvps/cloud").unwrap().dir, "/Users/g/Code/dreamvps/cloud");
    }

    #[test]
    fn an_unknown_project_lists_what_is_open() {
        let err = two_projects().resolve("nope").unwrap_err();
        assert!(err.contains("cloud"), "{err}");
        assert!(err.contains("central-one"), "{err}");
        let err = Registry::default().resolve("nope").unwrap_err();
        assert!(err.contains("no other project is open"), "{err}");
    }

    #[test]
    fn a_project_matches_only_on_whole_components() {
        let r = two_projects();
        // "one" must not match ".../central-one" — that is a substring, not a
        // path component, and matching it would deliver to the wrong repo.
        assert!(r.resolve("one").is_err());
        assert!(r.resolve("oud").is_err());
    }

    // ---- the file ----------------------------------------------------------

    #[test]
    fn round_trips_and_writes_only_on_change() {
        let dir = std::env::temp_dir().join(format!("mulpex-reg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut reg = two_projects();
        reg.projects[0].instances[0].task = "auth refactor".into();
        reg.projects[0].instances[0].name = Some("auth".into());

        assert!(Registry::write_if_changed(&dir, &reg));
        assert_eq!(Registry::read(&dir), reg);
        // Same content again: no write, and the file is untouched.
        assert!(!Registry::write_if_changed(&dir, &reg));

        reg.projects[1].instances.push(inst(2));
        assert!(Registry::write_if_changed(&dir, &reg));
        assert_eq!(Registry::read(&dir), reg);

        // A missing or corrupt registry reads as empty rather than failing.
        assert_eq!(Registry::read(Path::new("/nonexistent-mulpex")), Registry::default());
        std::fs::write(dir.join(REGISTRY_FILE), "{{{").unwrap();
        assert_eq!(Registry::read(&dir), Registry::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn addresses_and_paths_are_derived_from_the_entry() {
        let p = proj("central-one", "/Users/g/Code/dreamvps/central-one", &[1, 3]);
        assert_eq!(p.address(3), "central-one#3");
        assert!(p.has_instance(3));
        assert!(!p.has_instance(2));
        assert_eq!(
            p.inbox_dir(3),
            Path::new("/tmp/mulpex-1/central-one/inbox/3")
        );
        assert_eq!(
            p.messages_log(),
            Path::new("/tmp/mulpex-1/central-one/messages.log")
        );
    }

    /// "Which project am I" cannot be a string compare. The app writes the dir it
    /// opened; the helper asks with the canonicalized one. On macOS that alone is
    /// enough to disagree (`/var` vs `/private/var`), and a symlinked project
    /// path disagrees everywhere. The visible symptom is an instance seeing its
    /// OWN project among the "other" ones — found by driving two real helpers.
    #[test]
    fn a_project_is_identified_through_symlinks_not_by_string() {
        let root = std::env::temp_dir().join(format!("mulpex-samedir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        let link = root.join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let p = ProjectEntry {
            handle: 1,
            name: "real".into(),
            dir: real.to_string_lossy().into_owned(),
            state_dir: root.join("1").to_string_lossy().into_owned(),
            instances: vec![],
        };
        assert!(p.is_dir(&real));
        assert!(p.is_dir(&link), "a symlinked project path is the same project");
        assert!(!p.is_dir(&root), "a different directory is still different");
        // Two paths that don't exist are NOT assumed equal.
        assert!(!same_dir(&root.join("ghost-a"), &root.join("ghost-b")));

        let reg = Registry { projects: vec![p] };
        assert!(reg.project_for_dir(&link).is_some());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `read_for` takes an instance's own state dir (`<root>/<handle>`) and finds
    /// the registry one level up — the reason no new env var was needed.
    #[test]
    fn read_for_climbs_from_a_project_state_dir() {
        let root = std::env::temp_dir().join(format!("mulpex-reg-for-{}", std::process::id()));
        let state_dir = root.join("7");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&state_dir).unwrap();
        Registry::write_if_changed(&root, &two_projects());
        assert_eq!(Registry::read_for(&state_dir), two_projects());
        let _ = std::fs::remove_dir_all(&root);
    }
}
