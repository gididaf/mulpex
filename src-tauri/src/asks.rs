//! The "For me" list: per claude, what it needs from the USER — an action they
//! must take, a decision (with choices to click), or a question.
//!
//! Two writers, never at once on the same layer:
//!
//! - **The claude** edits the BASE through `hub_user_asks` (`mcp.rs`), which
//!   rides the `termreq/` app-request channel — so every write happens here, in
//!   the app, under the `Workspace` lock. The helper only checks the call's shape.
//! - **The user** edits DRAFTS laid over the base (`Drafts`). The claude never
//!   sees a draft: `claude_view` is base-only. UPDATE folds the drafts into the
//!   base (`List::commit`) and the returned `Diff` becomes a hub message from
//!   "Mulpex" (`render_update`), so the claude can act without a tool call.
//!
//! Stored like the ⌘⇧P pins (`pins.rs`): `<mulpex home>/asks/<key>.json`, keyed
//! by instance NUMBER, pruned by `Core::persist_sessions` — so ⌘W loses a list
//! and a quit keeps it. Drafts are stored too, so the desktop, a remote desktop
//! and the phone all see the same "pending" state and any of them can send it.
//!
//! No field anywhere in here may be called `handle`: `remote/client.rs::lift`
//! rewrites every such key in a payload forwarded to a remote desktop.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Action,
    Decision,
    Question,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Action => "action",
            Kind::Decision => "decision",
            Kind::Question => "question",
        }
    }
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "action" => Ok(Kind::Action),
            "decision" => Ok(Kind::Decision),
            "question" => Ok(Kind::Question),
            other => Err(format!("unknown kind {other:?} — use action, decision or question")),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Done,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    Claude,
    User,
}

/// One item as the claude has seen it (or was last sent it).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Item {
    /// Per claude, monotonic, never reused — so "#3" means one thing for the
    /// life of the list, even after #3 was removed.
    pub n: u32,
    pub kind: Kind,
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub choices: Vec<String>,
    pub status: Status,
    #[serde(default)]
    pub done_by: Option<Author>,
    pub author: Author,
    /// The user's committed answer/note.
    #[serde(default)]
    pub note: Option<String>,
    /// The choice the user committed, as its TEXT — survives the claude
    /// reordering its choices.
    #[serde(default)]
    pub chosen: Option<String>,
    pub created: u64,
    pub updated: u64,
    /// Bumped on every change the claude makes, so a draft can tell it was
    /// written against an older version of the item.
    #[serde(default)]
    pub rev: u32,
}

/// The title and kind of an item the claude removed while the user had typed
/// something on it — kept so that text is still sent, with context.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Orphan {
    pub kind: Kind,
    pub title: String,
}

/// A draft laid over base item `n`. Every field is "what the user wants it to
/// become"; an absent field changes nothing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Edit {
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub chosen: Option<String>,
    #[serde(default)]
    pub delete: bool,
    #[serde(default)]
    pub base_rev: u32,
    #[serde(default)]
    pub orphan: Option<Orphan>,
}

impl Edit {
    fn is_empty(&self) -> bool {
        self.done.is_none() && self.note.is_none() && self.chosen.is_none() && !self.delete
    }
    /// Text the user typed or picked — the part that must never be dropped silently.
    fn has_words(&self) -> bool {
        self.note.as_deref().is_some_and(|s| !s.trim().is_empty()) || self.chosen.is_some()
    }
}

/// A new item the user wrote: a request to the claude, sent at UPDATE.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Added {
    pub tmp: String,
    pub kind: Kind,
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Drafts {
    #[serde(default)]
    pub edits: BTreeMap<u32, Edit>,
    #[serde(default)]
    pub added: Vec<Added>,
}

impl Drafts {
    pub fn count(&self) -> usize {
        self.edits.len() + self.added.len()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct List {
    #[serde(default)]
    pub next: u32,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub drafts: Drafts,
}

/// One edit from the user (desktop panel, remote desktop, phone), tagged by `op`.
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AskEdit {
    Done { n: u32, done: bool },
    Note { n: u32, note: String },
    Choose { n: u32, choice: Option<String> },
    Dismiss { n: u32, on: bool },
    Add { kind: Kind, title: String, #[serde(default)] detail: Option<String> },
    EditAdded {
        tmp: String,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        detail: Option<String>,
    },
    RemoveAdded { tmp: String },
    Discard { n: u32 },
    DiscardAll,
    ClearDone,
}

/// What one UPDATE changed, for `render_update`.
#[derive(Default, Debug, PartialEq)]
pub struct Diff {
    pub done: Vec<String>,
    pub reopened: Vec<String>,
    pub chosen: Vec<String>,
    pub notes: Vec<String>,
    pub added: Vec<String>,
    pub dismissed: Vec<String>,
}

impl Diff {
    fn is_empty(&self) -> bool {
        self.done.is_empty()
            && self.reopened.is_empty()
            && self.chosen.is_empty()
            && self.notes.is_empty()
            && self.added.is_empty()
            && self.dismissed.is_empty()
    }
}

/// Unix seconds, for the items' timestamps.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub const MAX_OPEN: usize = 50;
const MAX_TITLE: usize = 200;
const MAX_DETAIL: usize = 4000;
const MAX_NOTE: usize = 4000;
const MAX_CHOICES: usize = 8;
const MAX_CHOICE: usize = 120;

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// One line: newlines and runs of whitespace collapse to a single space.
fn one_line(s: &str, max: usize) -> String {
    cut(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn clean_detail(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(|s| cut(s, MAX_DETAIL))
}

fn clean_choices(v: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let arr = v.as_array().ok_or("`choices` must be an array of strings")?;
    let mut out: Vec<String> = Vec::new();
    for c in arr {
        let c = c.as_str().ok_or("`choices` must be an array of strings")?;
        let c = one_line(c, MAX_CHOICE);
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    }
    if out.len() > MAX_CHOICES {
        return Err(format!("at most {MAX_CHOICES} choices"));
    }
    Ok(out)
}

/// `id` as one number or an array of them.
fn ids_of(v: &Value) -> Result<Vec<u32>, String> {
    let one = |x: &Value| x.as_u64().map(|n| n as u32);
    match v.get("id") {
        Some(Value::Array(a)) if !a.is_empty() => {
            a.iter().map(|x| one(x).ok_or_else(|| "`id` must be item numbers".to_string())).collect()
        }
        Some(x) => one(x).map(|n| vec![n]).ok_or_else(|| "missing `id` (an item number, or an array)".into()),
        None => Err("missing `id` (an item number, or an array)".into()),
    }
}

fn label(n: u32, kind: Kind, title: &str) -> String {
    format!("#{n} [{}] {title}", kind.word())
}

impl List {
    fn item_mut(&mut self, n: u32) -> Result<&mut Item, String> {
        let next = self.next;
        self.items.iter_mut().find(|i| i.n == n).ok_or_else(|| {
            if n >= 1 && n <= next {
                format!("#{n} is no longer on your list (removed, or cleared by the user)")
            } else {
                format!("#{n} is not on your list")
            }
        })
    }

    fn open_count(&self) -> usize {
        self.items.iter().filter(|i| i.status == Status::Open).count()
    }

    /// Counts for the sidebar badge: open base items, and pending drafts.
    pub fn counts(&self) -> (u32, u32) {
        (self.open_count() as u32, self.drafts.count() as u32)
    }

    /// The claude's change, from a `hub_user_asks` call. `list` is read-only and
    /// handled by the caller. Returns a one-line note on success.
    pub fn apply_claude(&mut self, v: &Value, now: u64) -> Result<String, String> {
        let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
        match action {
            "add" => {
                let items = v
                    .get("items")
                    .and_then(|x| x.as_array())
                    .filter(|a| !a.is_empty())
                    .ok_or("`add` needs `items`: a non-empty array of {kind, title, detail?, choices?}")?;
                // Validate everything before touching anything: a batch is all or nothing.
                let mut fresh = Vec::new();
                for it in items {
                    let kind = Kind::parse(it.get("kind").and_then(|x| x.as_str()).unwrap_or("action"))?;
                    let title = one_line(it.get("title").and_then(|x| x.as_str()).unwrap_or(""), MAX_TITLE);
                    if title.is_empty() {
                        return Err("every item needs a `title`".into());
                    }
                    if let Some(dup) = self
                        .items
                        .iter()
                        .find(|i| i.status == Status::Open && i.title == title)
                    {
                        return Err(format!(
                            "#{} already says {title:?} — update it instead of adding a duplicate",
                            dup.n
                        ));
                    }
                    let detail = clean_detail(it.get("detail").and_then(|x| x.as_str()));
                    let choices = clean_choices(it.get("choices"))?;
                    fresh.push((kind, title, detail, choices));
                }
                if self.open_count() + fresh.len() > MAX_OPEN {
                    return Err(format!(
                        "at most {MAX_OPEN} open items — mark some done or remove them first"
                    ));
                }
                let mut added = Vec::new();
                for (kind, title, detail, choices) in fresh {
                    self.next += 1;
                    let n = self.next;
                    self.items.push(Item {
                        n,
                        kind,
                        title,
                        detail,
                        choices,
                        status: Status::Open,
                        done_by: None,
                        author: Author::Claude,
                        note: None,
                        chosen: None,
                        created: now,
                        updated: now,
                        rev: 0,
                    });
                    added.push(format!("#{n}"));
                }
                Ok(format!("added {}", added.join(", ")))
            }
            "update" => {
                let ids = ids_of(v)?;
                let [n] = ids[..] else {
                    return Err("`update` takes one `id`".into());
                };
                let kind = v.get("kind").and_then(|x| x.as_str()).map(Kind::parse).transpose()?;
                let title = v
                    .get("title")
                    .and_then(|x| x.as_str())
                    .map(|t| one_line(t, MAX_TITLE))
                    .filter(|t| !t.is_empty());
                let detail = v.get("detail").map(|d| clean_detail(d.as_str()));
                let choices = v.get("choices").map(|c| clean_choices(Some(c))).transpose()?;
                if kind.is_none() && title.is_none() && detail.is_none() && choices.is_none() {
                    return Err("`update` needs at least one of kind, title, detail, choices".into());
                }
                let item = self.item_mut(n)?;
                if let Some(k) = kind {
                    item.kind = k;
                }
                if let Some(t) = title {
                    item.title = t;
                }
                if let Some(d) = detail {
                    item.detail = d;
                }
                if let Some(c) = choices {
                    item.choices = c;
                }
                item.updated = now;
                item.rev += 1;
                Ok(format!("updated #{n}"))
            }
            "done" | "reopen" => {
                let ids = ids_of(v)?;
                for n in &ids {
                    self.item_mut(*n)?;
                }
                let reopen = action == "reopen";
                if reopen && self.open_count() + ids.len() > MAX_OPEN {
                    return Err(format!("at most {MAX_OPEN} open items"));
                }
                for n in &ids {
                    let item = self.item_mut(*n)?;
                    if reopen {
                        item.status = Status::Open;
                        item.done_by = None;
                    } else {
                        item.status = Status::Done;
                        item.done_by = Some(Author::Claude);
                    }
                    item.updated = now;
                    item.rev += 1;
                }
                let list = ids.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                Ok(format!("{} {list}", if reopen { "reopened" } else { "marked done" }))
            }
            "remove" => {
                let ids = ids_of(v)?;
                for n in &ids {
                    self.item_mut(*n)?;
                }
                for n in &ids {
                    let Some(pos) = self.items.iter().position(|i| i.n == *n) else { continue };
                    let item = self.items.remove(pos);
                    // Keep only what the user typed; a bare tick/delete on an item
                    // that no longer exists carries nothing worth sending.
                    match self.drafts.edits.remove(n) {
                        Some(mut e) if e.has_words() => {
                            e.done = None;
                            e.delete = false;
                            e.orphan = Some(Orphan { kind: item.kind, title: item.title });
                            self.drafts.edits.insert(*n, e);
                        }
                        _ => {}
                    }
                }
                let list = ids.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                Ok(format!("removed {list}"))
            }
            "list" => Ok(String::new()),
            "" => Err("missing `action` (list, add, update, done, reopen, remove)".into()),
            other => Err(format!("unknown action {other:?} — use list, add, update, done, reopen or remove")),
        }
    }

    /// The user's edit to the drafts (or, for `clear_done`, straight to the base).
    /// The draft on item `n`, started if there is none. Must name a live item,
    /// or an orphan draft already kept for a removed one.
    fn draft_mut(&mut self, n: u32) -> Result<&mut Edit, String> {
        let rev = self.items.iter().find(|i| i.n == n).map(|i| i.rev);
        let orphan = self.drafts.edits.get(&n).is_some_and(|e| e.orphan.is_some());
        if rev.is_none() && !orphan {
            return Err(format!("#{n} is no longer on the list"));
        }
        Ok(self.drafts.edits.entry(n).or_insert_with(|| Edit {
            base_rev: rev.unwrap_or(0),
            ..Edit::default()
        }))
    }

    pub fn apply_user(&mut self, edit: AskEdit) -> Result<(), String> {
        match edit {
            AskEdit::Done { n, done } => {
                let base_done = self.items.iter().find(|i| i.n == n).map(|i| i.status == Status::Done);
                let e = self.draft_mut(n)?;
                e.done = if base_done == Some(done) { None } else { Some(done) };
            }
            AskEdit::Note { n, note } => {
                let base = self.items.iter().find(|i| i.n == n).and_then(|i| i.note.clone()).unwrap_or_default();
                let note = cut(note.trim_end(), MAX_NOTE);
                let e = self.draft_mut(n)?;
                e.note = if note == base { None } else { Some(note) };
            }
            AskEdit::Choose { n, choice } => {
                let item = self.items.iter().find(|i| i.n == n).cloned();
                if let (Some(c), Some(item)) = (&choice, &item) {
                    if !item.choices.contains(c) {
                        return Err(format!("{c:?} is not one of #{n}'s choices"));
                    }
                }
                let base = item.and_then(|i| i.chosen);
                let e = self.draft_mut(n)?;
                e.chosen = if choice == base { None } else { choice };
            }
            AskEdit::Dismiss { n, on } => {
                let e = self.draft_mut(n)?;
                if e.orphan.is_some() {
                    return Err(format!("#{n} was already removed — discard the draft instead"));
                }
                e.delete = on;
            }
            AskEdit::Add { kind, title, detail } => {
                let title = one_line(&title, MAX_TITLE);
                if title.is_empty() {
                    return Err("a title is needed".into());
                }
                self.drafts.added.push(Added {
                    tmp: mulpex_core::persist::new_uuid(),
                    kind,
                    title,
                    detail: clean_detail(detail.as_deref()),
                });
            }
            AskEdit::EditAdded { tmp, kind, title, detail } => {
                let a = self
                    .drafts
                    .added
                    .iter_mut()
                    .find(|a| a.tmp == tmp)
                    .ok_or("that new item is gone")?;
                if let Some(k) = kind {
                    a.kind = k;
                }
                if let Some(t) = title.map(|t| one_line(&t, MAX_TITLE)).filter(|t| !t.is_empty()) {
                    a.title = t;
                }
                if let Some(d) = detail {
                    a.detail = clean_detail(Some(&d));
                }
            }
            AskEdit::RemoveAdded { tmp } => self.drafts.added.retain(|a| a.tmp != tmp),
            AskEdit::Discard { n } => {
                self.drafts.edits.remove(&n);
            }
            AskEdit::DiscardAll => self.drafts = Drafts::default(),
            AskEdit::ClearDone => {
                let drafted: HashSet<u32> = self.drafts.edits.keys().copied().collect();
                self.items.retain(|i| i.status != Status::Done || drafted.contains(&i.n));
            }
        }
        // A draft that no longer changes anything is not pending.
        self.drafts.edits.retain(|_, e| !e.is_empty());
        Ok(())
    }

    /// Fold the drafts into the base. `None` when nothing changes (and nothing
    /// should be sent).
    pub fn commit(&mut self, now: u64) -> Option<Diff> {
        let drafts = std::mem::take(&mut self.drafts);
        let mut d = Diff::default();
        for (n, e) in drafts.edits {
            if let Some(o) = &e.orphan {
                let mut parts = Vec::new();
                if let Some(c) = &e.chosen {
                    parts.push(format!("chose {c:?}"));
                }
                if let Some(note) = e.note.as_deref().filter(|s| !s.trim().is_empty()) {
                    parts.push(format!("{note:?}"));
                }
                if !parts.is_empty() {
                    d.notes.push(format!(
                        "(re: #{n} [{}] {:?}, which you had removed) — {}",
                        o.kind.word(),
                        o.title,
                        parts.join("; ")
                    ));
                }
                continue;
            }
            let Some(pos) = self.items.iter().position(|i| i.n == n) else { continue };
            if e.delete {
                let item = self.items.remove(pos);
                d.dismissed.push(label(n, item.kind, &item.title));
                continue;
            }
            let item = &mut self.items[pos];
            let lbl = label(n, item.kind, &item.title);
            match e.done {
                Some(true) if item.status != Status::Done => {
                    item.status = Status::Done;
                    item.done_by = Some(Author::User);
                    d.done.push(lbl.clone());
                }
                Some(false) if item.status != Status::Open => {
                    item.status = Status::Open;
                    item.done_by = None;
                    d.reopened.push(lbl.clone());
                }
                _ => {}
            }
            if let Some(c) = e.chosen {
                let gone = if item.choices.contains(&c) { "" } else { " (no longer one of your choices)" };
                d.chosen.push(format!("{lbl} → chose: {c:?}{gone}"));
                item.chosen = Some(c);
            }
            if let Some(note) = e.note {
                if note.trim().is_empty() {
                    item.note = None;
                } else {
                    d.notes.push(format!("{lbl} — {note:?}"));
                    item.note = Some(note);
                }
            }
            item.updated = now;
        }
        for a in drafts.added {
            self.next += 1;
            let n = self.next;
            let mut line = label(n, a.kind, &a.title);
            if let Some(det) = &a.detail {
                for l in det.lines() {
                    line.push_str("\n    ");
                    line.push_str(l);
                }
            }
            d.added.push(line);
            self.items.push(Item {
                n,
                kind: a.kind,
                title: a.title,
                detail: a.detail,
                choices: Vec::new(),
                status: Status::Open,
                done_by: None,
                author: Author::User,
                note: None,
                chosen: None,
                created: now,
                updated: now,
                rev: 0,
            });
        }
        (!d.is_empty()).then_some(d)
    }

    /// What the claude sees: the base only, never a draft.
    pub fn claude_view(&self) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .map(|i| {
                let mut o = json!({
                    "n": i.n,
                    "kind": i.kind,
                    "title": i.title,
                    "status": i.status,
                    "author": i.author,
                });
                let m = o.as_object_mut().unwrap();
                if let Some(d) = &i.detail {
                    m.insert("detail".into(), json!(d));
                }
                if !i.choices.is_empty() {
                    m.insert("choices".into(), json!(i.choices));
                }
                if let Some(b) = i.done_by {
                    m.insert("done_by".into(), json!(b));
                }
                if let Some(n) = &i.note {
                    m.insert("user_note".into(), json!(n));
                }
                if let Some(c) = &i.chosen {
                    m.insert("user_chose".into(), json!(c));
                }
                o
            })
            .collect();
        json!(items)
    }
}

/// The hub message an UPDATE sends. Self-contained on purpose: a claude started
/// before this feature existed has no `hub_user_asks` tool and no rule about it,
/// and must still be able to act on what the user said.
pub fn render_update(id: usize, d: &Diff) -> String {
    let mut out = format!(
        "The user updated your For-me list (claude#{id}) in Mulpex and pressed UPDATE. What changed:\n"
    );
    let mut section = |title: &str, lines: &[String]| {
        if lines.is_empty() {
            return;
        }
        out.push_str(title);
        out.push_str(":\n");
        for l in lines {
            out.push_str("  ");
            out.push_str(l);
            out.push('\n');
        }
    };
    section("Done (checked by the user)", &d.done);
    section("Reopened by the user", &d.reopened);
    section("Decisions made", &d.chosen);
    section("Notes from the user", &d.notes);
    section("Added by the user (new requests for you)", &d.added);
    section("Dismissed by the user (gone from your list)", &d.dismissed);
    out.push_str(
        "Act on this now. Keep the list current with mcp__mulpex__hub_user_asks (mark items done, \
         add new asks). Do NOT hub_send a reply — the list and your normal reply are where you answer.",
    );
    out
}

/// A list plus whether an UPDATE is waiting to be delivered — what the panel draws.
#[derive(Serialize)]
pub struct ListView<'a> {
    #[serde(flatten)]
    pub list: &'a List,
    pub sending: bool,
}

/// What UPDATE did for one claude: `queued` (delivered once it can hear it) or
/// `refused`, with why.
#[derive(Serialize)]
pub struct AskSend {
    pub id: usize,
    pub state: String,
    pub why: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct AskFile {
    dir: PathBuf,
    lists: BTreeMap<usize, List>,
}

/// The durable store, cached in memory — the poll loop reads counts every tick,
/// and the app is the only writer, so the file is written through, never re-read.
/// Interior mutability because `Core::persist_sessions` (which prunes) is `&self`.
pub struct AskStore {
    path: PathBuf,
    project_dir: PathBuf,
    lists: Mutex<BTreeMap<usize, List>>,
    /// Ids whose list changed since the poll loop last emitted `asks-changed`.
    dirty: Mutex<BTreeSet<usize>>,
    /// Bumped on every change, so the phone's view can say "re-fetch" without
    /// carrying the lists themselves.
    rev: std::sync::atomic::AtomicU64,
}

impl AskStore {
    /// The file beside `session_store_path` (`<home>/sessions/<key>.txt` →
    /// `<home>/asks/<key>.json`), loaded. Another project's file (a hash
    /// collision) reads as empty, as `pins.rs` does.
    pub fn beside(session_store_path: &Path, project_dir: &Path) -> Self {
        let home = session_store_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        let stem = session_store_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = home.join("asks").join(format!("{stem}.json"));
        let lists = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<AskFile>(&t).ok())
            .filter(|f| f.dir == project_dir)
            .map(|f| f.lists)
            .unwrap_or_default();
        Self {
            path,
            project_dir: project_dir.to_path_buf(),
            lists: Mutex::new(lists),
            dirty: Mutex::new(BTreeSet::new()),
            rev: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn get(&self, id: usize) -> Option<List> {
        self.lists.lock().unwrap().get(&id).cloned()
    }

    pub fn all(&self) -> BTreeMap<usize, List> {
        self.lists.lock().unwrap().clone()
    }

    pub fn counts(&self, id: usize) -> (u32, u32) {
        self.lists.lock().unwrap().get(&id).map(List::counts).unwrap_or((0, 0))
    }

    /// Run `f` on `id`'s list (created if missing). Written through and marked
    /// dirty only when `f` succeeds and the list actually changed.
    pub fn mutate<R>(
        &self,
        id: usize,
        f: impl FnOnce(&mut List) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut lists = self.lists.lock().unwrap();
        let before = lists.get(&id).cloned();
        let mut list = before.clone().unwrap_or_default();
        let r = f(&mut list)?;
        if Some(&list) != before.as_ref() {
            if list == List::default() {
                lists.remove(&id);
            } else {
                lists.insert(id, list);
            }
            self.save(&lists);
            self.touch(id);
        }
        Ok(r)
    }

    /// Drop the list of every instance not in `ids`. Writes only on a change.
    pub fn retain(&self, ids: &HashSet<usize>) {
        let mut lists = self.lists.lock().unwrap();
        let gone: Vec<usize> = lists.keys().filter(|id| !ids.contains(id)).copied().collect();
        if gone.is_empty() {
            return;
        }
        for id in &gone {
            lists.remove(id);
        }
        self.save(&lists);
        for id in gone {
            self.touch(id);
        }
    }

    fn touch(&self, id: usize) {
        self.dirty.lock().unwrap().insert(id);
        self.rev.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn rev(&self) -> u64 {
        self.rev.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn take_dirty(&self) -> Vec<usize> {
        std::mem::take(&mut *self.dirty.lock().unwrap()).into_iter().collect()
    }

    pub fn mark_dirty(&self, id: usize) {
        self.touch(id);
    }

    /// Best-effort, atomic (tmp + rename); an empty store removes the file.
    fn save(&self, lists: &BTreeMap<usize, List>) {
        if lists.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        let Some(dir) = self.path.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let file = json!({ "dir": self.project_dir, "lists": lists });
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, file.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(list: &mut List, items: Value) -> Result<String, String> {
        list.apply_claude(&json!({ "action": "add", "items": items }), 1)
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-asks-test-{}", mulpex_core::persist::new_uuid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn claude_ops_number_monotonically_and_never_reuse() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"action","title":"deploy\n the  thing"},{"kind":"question","title":"which db?"}])).unwrap();
        assert_eq!(l.items[0].title, "deploy the thing");
        assert_eq!(l.items.iter().map(|i| i.n).collect::<Vec<_>>(), vec![1, 2]);
        l.apply_claude(&json!({"action":"remove","id":2}), 2).unwrap();
        add(&mut l, json!([{"kind":"decision","title":"A or B","choices":["A","B","A"]}])).unwrap();
        assert_eq!(l.items[1].n, 3);
        assert_eq!(l.items[1].choices, vec!["A", "B"]);
        // Removed numbers say so rather than "not on your list".
        let e = l.apply_claude(&json!({"action":"done","id":2}), 3).unwrap_err();
        assert!(e.contains("no longer"), "{e}");
        // Done keeps the item, greyed; reopen brings it back.
        l.apply_claude(&json!({"action":"done","id":[1,3]}), 3).unwrap();
        assert_eq!(l.counts(), (0, 0));
        l.apply_claude(&json!({"action":"reopen","id":1}), 4).unwrap();
        assert_eq!(l.counts(), (1, 0));
        // Duplicate open title is refused with a pointer.
        let e = add(&mut l, json!([{"kind":"action","title":"deploy the thing"}])).unwrap_err();
        assert!(e.contains("#1"), "{e}");
        // Bad shapes are errors, not panics.
        assert!(l.apply_claude(&json!({"action":"update","id":1}), 5).is_err());
        assert!(l.apply_claude(&json!({"action":"nope"}), 5).is_err());
        assert!(add(&mut l, json!([{"kind":"weird","title":"x"}])).is_err());
    }

    #[test]
    fn open_limit_is_enforced() {
        let mut l = List::default();
        let many: Vec<Value> = (0..MAX_OPEN).map(|i| json!({"kind":"action","title":format!("t{i}")})).collect();
        add(&mut l, json!(many)).unwrap();
        assert!(add(&mut l, json!([{"kind":"action","title":"one more"}])).is_err());
    }

    #[test]
    fn drafts_are_hidden_from_the_claude_until_commit() {
        let mut l = List::default();
        add(&mut l, json!([
            {"kind":"action","title":"run migration"},
            {"kind":"decision","title":"pick a name","choices":["foo","bar"]},
            {"kind":"question","title":"מה הפורט?"},
            {"kind":"action","title":"obsolete"}
        ])).unwrap();
        l.apply_user(AskEdit::Done { n: 1, done: true }).unwrap();
        l.apply_user(AskEdit::Choose { n: 2, choice: Some("bar".into()) }).unwrap();
        l.apply_user(AskEdit::Note { n: 3, note: "5173 בבקשה".into() }).unwrap();
        l.apply_user(AskEdit::Dismiss { n: 4, on: true }).unwrap();
        l.apply_user(AskEdit::Add { kind: Kind::Action, title: "also bump version".into(), detail: Some("to 0.41".into()) }).unwrap();
        assert!(l.apply_user(AskEdit::Choose { n: 2, choice: Some("baz".into()) }).is_err());
        assert_eq!(l.counts(), (4, 5));

        let view = l.claude_view().to_string();
        assert!(!view.contains("5173") && !view.contains("bump"), "{view}");

        let d = l.commit(9).unwrap();
        assert_eq!(d.done, vec!["#1 [action] run migration"]);
        assert_eq!(d.chosen, vec!["#2 [decision] pick a name → chose: \"bar\""]);
        assert_eq!(d.notes, vec!["#3 [question] מה הפורט? — \"5173 בבקשה\""]);
        assert_eq!(d.dismissed, vec!["#4 [action] obsolete"]);
        assert_eq!(d.added, vec!["#5 [action] also bump version\n    to 0.41"]);
        assert_eq!(l.drafts.count(), 0);
        let view = l.claude_view().to_string();
        assert!(view.contains("5173") && view.contains("\"user_chose\":\"bar\""), "{view}");
        // Nothing pending → nothing to send.
        assert!(l.commit(10).is_none());

        let msg = render_update(3, &d);
        assert!(msg.contains("claude#3") && msg.contains("מה הפורט?"));
        assert!(msg.contains("Do NOT hub_send a reply"));
        assert!(!msg.contains("Reopened"), "empty sections are left out");
    }

    #[test]
    fn a_draft_that_changes_nothing_is_not_pending() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"action","title":"x"}])).unwrap();
        l.apply_user(AskEdit::Done { n: 1, done: true }).unwrap();
        l.apply_user(AskEdit::Done { n: 1, done: false }).unwrap();
        assert_eq!(l.drafts.count(), 0);
        l.apply_user(AskEdit::Note { n: 1, note: "".into() }).unwrap();
        assert_eq!(l.drafts.count(), 0);
    }

    #[test]
    fn removing_an_item_keeps_only_the_users_words() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"question","title":"q1"},{"kind":"action","title":"a2"}])).unwrap();
        l.apply_user(AskEdit::Note { n: 1, note: "my answer".into() }).unwrap();
        l.apply_user(AskEdit::Done { n: 2, done: true }).unwrap();
        l.apply_claude(&json!({"action":"remove","id":[1,2]}), 2).unwrap();
        assert!(l.drafts.edits[&1].orphan.is_some());
        assert!(!l.drafts.edits.contains_key(&2), "a bare tick is dropped");
        let d = l.commit(3).unwrap();
        assert_eq!(d.notes, vec!["(re: #1 [question] \"q1\", which you had removed) — \"my answer\""]);
    }

    #[test]
    fn a_claude_change_marks_the_draft_stale_but_keeps_it() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"decision","title":"d","choices":["a","b"]}])).unwrap();
        l.apply_user(AskEdit::Choose { n: 1, choice: Some("a".into()) }).unwrap();
        l.apply_claude(&json!({"action":"update","id":1,"choices":["b","c"]}), 2).unwrap();
        assert!(l.items[0].rev > l.drafts.edits[&1].base_rev);
        let d = l.commit(3).unwrap();
        assert!(d.chosen[0].contains("no longer one of your choices"), "{:?}", d.chosen);
    }

    #[test]
    fn clear_done_spares_drafted_items() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"action","title":"a"},{"kind":"action","title":"b"}])).unwrap();
        l.apply_claude(&json!({"action":"done","id":[1,2]}), 2).unwrap();
        l.apply_user(AskEdit::Note { n: 2, note: "thanks".into() }).unwrap();
        l.apply_user(AskEdit::ClearDone).unwrap();
        assert_eq!(l.items.iter().map(|i| i.n).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn store_round_trips_prunes_and_ignores_other_projects() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        let s = AskStore::beside(&sp, Path::new("/p"));
        s.mutate(3, |l| add(l, json!([{"kind":"action","title":"x"}]))).unwrap();
        s.mutate(3, |l| l.apply_user(AskEdit::Note { n: 1, note: "draft".into() })).unwrap();
        s.mutate(7, |l| add(l, json!([{"kind":"action","title":"y"}]))).unwrap();
        assert_eq!(s.take_dirty(), vec![3, 7]);
        // A failed op changes nothing and dirties nothing.
        assert!(s.mutate(3, |l| l.apply_claude(&json!({"action":"bogus"}), 1)).is_err());
        assert!(s.take_dirty().is_empty());

        let again = AskStore::beside(&sp, Path::new("/p"));
        assert_eq!(again.get(3).unwrap().drafts.count(), 1, "drafts persist");
        assert!(AskStore::beside(&sp, Path::new("/other")).all().is_empty());

        again.retain(&HashSet::from([7]));
        assert!(again.get(3).is_none());
        assert_eq!(again.take_dirty(), vec![3]);
        again.retain(&HashSet::new());
        assert!(!home.join("asks/proj-abc.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
