//! The "To Do" list: per claude, what it needs from the USER — an action they
//! must take, a decision (with choices to click), or a question.
//!
//! Two writers, never at once on the same layer:
//!
//! - **The claude** edits the BASE through `hub_todo` (`mcp.rs`), which
//!   rides the `termreq/` app-request channel — so every write happens here, in
//!   the app, under the `Workspace` lock. The helper only checks the call's shape.
//! - **The user** edits DRAFTS laid over the base (`Drafts`). The claude never
//!   sees a draft: `claude_view` is base-only. UPDATE folds the drafts into the
//!   base (`List::commit`) and the returned `Diff` becomes a hub message from
//!   "Mulpex" (`render_update`), so the claude can act without a tool call.
//!
//! Stored like the ⌘⇧P pins (`pins.rs`): `<mulpex home>/todos/<key>.json`, keyed
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
    /// The single note an item had before comments replaced it. Read only to
    /// migrate it into `comments` on load (`List::migrate`), never written.
    #[serde(default, skip_serializing)]
    pub note: Option<String>,
    /// The follow-up thread, both authors, oldest first. Append-only: it goes
    /// when the item does.
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// How many of `comments` the user has seen (opened the thread) — the
    /// claude's comments past it are unread. One count for every viewer.
    #[serde(default)]
    pub seen: usize,
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
    /// The instance this ask is really for — a worker whose need an orchestrator
    /// collected onto its own list, so the user has one list to read.
    #[serde(default, rename = "for")]
    pub for_id: Option<usize>,
    /// Set by the claude only when the user's attention is needed as soon as
    /// possible: shown on top, in red, and turns the row badge red.
    #[serde(default)]
    pub urgent: bool,
}

impl Item {
    /// A claude comment past what the user has seen.
    pub fn unread(&self) -> bool {
        self.comments.iter().skip(self.seen).any(|c| c.by == Author::Claude)
    }
}

/// One comment on an item's thread.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Comment {
    pub by: Author,
    pub text: String,
    pub at: u64,
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
    /// The user's reply, one per item, appended to the thread at UPDATE.
    /// (`note` is the name drafts were saved under before comments.)
    #[serde(default, alias = "note")]
    pub comment: Option<String>,
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
        self.done.is_none() && self.comment.is_none() && self.chosen.is_none() && !self.delete
    }
    /// Text the user typed or picked — the part that must never be dropped silently.
    fn has_words(&self) -> bool {
        self.comment.as_deref().is_some_and(|s| !s.trim().is_empty()) || self.chosen.is_some()
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
pub enum TodoEdit {
    Done { n: u32, done: bool },
    Comment { n: u32, text: String },
    /// The user opened item `n`'s thread: everything on it is now read. Not a
    /// draft — it changes the base at once.
    Seen { n: u32 },
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
}

/// What one UPDATE changed, for `render_update`.
#[derive(Default, Debug, PartialEq)]
pub struct Diff {
    pub done: Vec<String>,
    pub chosen: Vec<String>,
    pub comments: Vec<String>,
    pub added: Vec<String>,
    pub dismissed: Vec<String>,
}

impl Diff {
    fn is_empty(&self) -> bool {
        self.done.is_empty()
            && self.chosen.is_empty()
            && self.comments.is_empty()
            && self.added.is_empty()
            && self.dismissed.is_empty()
    }
}

/// One list's numbers for the sidebar badge: open items, the user's pending
/// drafts, items with an unread claude comment, and open urgent items.
#[derive(Default, Debug, PartialEq, Clone, Copy)]
pub struct Counts {
    pub open: u32,
    pub pending: u32,
    pub unread: u32,
    pub urgent: u32,
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
/// Details and comments are Markdown, and details are where long content goes
/// (QA steps, results, options compared) instead of the conversation.
const MAX_DETAIL: usize = 20_000;
const MAX_COMMENT: usize = 20_000;
/// In the list a claude gets back on every call, longer text is cut to this
/// and only the latest comments are shown — `list` with `id` reads one item in
/// full. Otherwise each call would re-send every long detail into its context.
const PREVIEW: usize = 300;
const PREVIEW_COMMENTS: usize = 3;
/// Per item. A thread this long has stopped being a follow-up.
const MAX_COMMENTS: usize = 100;
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

/// `for`: an instance number, `"7"` or `"claude#7"`; `null` clears.
fn for_of(v: Option<&Value>) -> Result<Option<Option<usize>>, String> {
    let Some(v) = v else { return Ok(None) };
    let n = match v {
        Value::Null => return Ok(Some(None)),
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_start_matches("claude#").trim_start_matches('#').parse().ok(),
        _ => None,
    };
    match n {
        Some(n) if n >= 1 => Ok(Some(Some(n as usize))),
        _ => Err("`for` must be an instance number, like 7 or \"claude#7\"".into()),
    }
}

/// `urgent`: a boolean, when given.
fn urgent_of(v: Option<&Value>) -> Result<Option<bool>, String> {
    match v {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err("`urgent` must be true or false".into()),
    }
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

fn label(n: u32, kind: Kind, title: &str, for_id: Option<usize>) -> String {
    match for_id {
        Some(f) => format!("#{n} [{}] (for claude#{f}) {title}", kind.word()),
        None => format!("#{n} [{}] {title}", kind.word()),
    }
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

    /// Take items `ids` off the list. What the user typed or picked on one is
    /// kept as an orphan draft, so it is still sent at UPDATE; a bare tick or
    /// dismiss on an item that no longer exists carries nothing worth sending.
    fn drop_items(&mut self, ids: &[u32]) {
        for n in ids {
            let Some(pos) = self.items.iter().position(|i| i.n == *n) else { continue };
            let item = self.items.remove(pos);
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
    }

    /// Bring a list saved by an older Mulpex up to date: done items used to stay
    /// until "Clear done", and an item used to carry one `note` instead of a
    /// thread (it becomes the user's first comment).
    fn migrate(&mut self) {
        let done: Vec<u32> = self.items.iter().filter(|i| i.status == Status::Done).map(|i| i.n).collect();
        self.drop_items(&done);
        for i in &mut self.items {
            if let Some(text) = i.note.take().filter(|t| !t.trim().is_empty()) {
                i.comments.insert(0, Comment { by: Author::User, text, at: i.updated });
                i.seen += 1;
            }
        }
    }

    fn open_count(&self) -> usize {
        self.items.iter().filter(|i| i.status == Status::Open).count()
    }

    /// Counts for the sidebar badge.
    pub fn counts(&self) -> Counts {
        Counts {
            open: self.open_count() as u32,
            pending: self.drafts.count() as u32,
            unread: self.items.iter().filter(|i| i.unread()).count() as u32,
            urgent: self.items.iter().filter(|i| i.urgent && i.status == Status::Open).count() as u32,
        }
    }

    /// The claude's change, from a `hub_todo` call. `list` is read-only and
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
                    let for_id = for_of(it.get("for"))?.flatten();
                    let urgent = urgent_of(it.get("urgent"))?.unwrap_or(false);
                    fresh.push((kind, title, detail, choices, for_id, urgent));
                }
                if self.open_count() + fresh.len() > MAX_OPEN {
                    return Err(format!(
                        "at most {MAX_OPEN} open items — mark some done or remove them first"
                    ));
                }
                let mut added = Vec::new();
                for (kind, title, detail, choices, for_id, urgent) in fresh {
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
                        comments: Vec::new(),
                        seen: 0,
                        chosen: None,
                        created: now,
                        updated: now,
                        rev: 0,
                        for_id,
                        urgent,
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
                let for_id = for_of(v.get("for"))?;
                let urgent = urgent_of(v.get("urgent"))?;
                if kind.is_none()
                    && title.is_none()
                    && detail.is_none()
                    && choices.is_none()
                    && for_id.is_none()
                    && urgent.is_none()
                {
                    return Err("`update` needs at least one of kind, title, detail, choices, for, urgent".into());
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
                if let Some(f) = for_id {
                    item.for_id = f;
                }
                if let Some(u) = urgent {
                    item.urgent = u;
                }
                item.updated = now;
                item.rev += 1;
                Ok(format!("updated #{n}"))
            }
            "comment" => {
                let ids = ids_of(v)?;
                let [n] = ids[..] else {
                    return Err("`comment` takes one `id`".into());
                };
                let text = v.get("text").and_then(|x| x.as_str()).map(str::trim).unwrap_or("");
                if text.is_empty() {
                    return Err("`comment` needs `text`".into());
                }
                let item = self.item_mut(n)?;
                if item.comments.len() >= MAX_COMMENTS {
                    return Err(format!("#{n} already has {MAX_COMMENTS} comments"));
                }
                item.comments.push(Comment { by: Author::Claude, text: cut(text, MAX_COMMENT), at: now });
                item.updated = now;
                Ok(format!("commented on #{n}"))
            }
            // A done item leaves the list at once — the user never has to clear it.
            "done" | "remove" => {
                let ids = ids_of(v)?;
                for n in &ids {
                    self.item_mut(*n)?;
                }
                self.drop_items(&ids);
                let list = ids.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                Ok(format!("{} {list}", if action == "done" { "done, removed" } else { "removed" }))
            }
            "list" => Ok(String::new()),
            "" => Err("missing `action` (list, add, update, comment, done, remove)".into()),
            other => Err(format!("unknown action {other:?} — use list, add, update, comment, done or remove")),
        }
    }

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

    pub fn apply_user(&mut self, edit: TodoEdit) -> Result<(), String> {
        match edit {
            TodoEdit::Done { n, done } => {
                let base_done = self.items.iter().find(|i| i.n == n).map(|i| i.status == Status::Done);
                let e = self.draft_mut(n)?;
                e.done = if base_done == Some(done) { None } else { Some(done) };
            }
            TodoEdit::Comment { n, text } => {
                let text = cut(text.trim_end(), MAX_COMMENT);
                let e = self.draft_mut(n)?;
                e.comment = if text.trim().is_empty() { None } else { Some(text) };
            }
            TodoEdit::Seen { n } => {
                let item = self.items.iter_mut().find(|i| i.n == n).ok_or(format!("#{n} is no longer on the list"))?;
                item.seen = item.comments.len();
            }
            TodoEdit::Choose { n, choice } => {
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
            TodoEdit::Dismiss { n, on } => {
                let e = self.draft_mut(n)?;
                if e.orphan.is_some() {
                    return Err(format!("#{n} was already removed — discard the draft instead"));
                }
                e.delete = on;
            }
            TodoEdit::Add { kind, title, detail } => {
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
            TodoEdit::EditAdded { tmp, kind, title, detail } => {
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
            TodoEdit::RemoveAdded { tmp } => self.drafts.added.retain(|a| a.tmp != tmp),
            TodoEdit::Discard { n } => {
                self.drafts.edits.remove(&n);
            }
            TodoEdit::DiscardAll => self.drafts = Drafts::default(),
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
                if let Some(c) = e.comment.as_deref().filter(|s| !s.trim().is_empty()) {
                    parts.push(format!("{c:?}"));
                }
                if !parts.is_empty() {
                    d.comments.push(format!(
                        "(re: #{n} [{}] {:?}, no longer on your list) — {}",
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
                d.dismissed.push(label(n, item.kind, &item.title, item.for_id));
                continue;
            }
            let item = &mut self.items[pos];
            let lbl = label(n, item.kind, &item.title, item.for_id);
            if let Some(c) = e.chosen {
                let gone = if item.choices.contains(&c) { "" } else { " (no longer one of your choices)" };
                d.chosen.push(format!("{lbl} → chose: {c:?}{gone}"));
                item.chosen = Some(c);
            }
            if let Some(text) = e.comment.filter(|t| !t.trim().is_empty()) {
                d.comments.push(format!("{lbl} — {text:?}"));
                item.comments.push(Comment { by: Author::User, text, at: now });
                // Writing a reply means the thread was read.
                item.seen = item.comments.len();
            }
            item.updated = now;
            // Checked by the user: reported, then off the list like any done item.
            if e.done == Some(true) {
                d.done.push(lbl);
                self.items.remove(pos);
            }
        }
        for a in drafts.added {
            self.next += 1;
            let n = self.next;
            let mut line = label(n, a.kind, &a.title, None);
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
                comments: Vec::new(),
                seen: 0,
                chosen: None,
                created: now,
                updated: now,
                rev: 0,
                for_id: None,
                urgent: false,
            });
        }
        (!d.is_empty()).then_some(d)
    }

    /// What the claude sees: the base only, never a draft. Every item is
    /// previewed (long text cut, latest comments only) except `full`, shown whole.
    pub fn claude_view(&self, full: Option<u32>) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .map(|i| {
                let whole = full == Some(i.n);
                let short = |t: &str| -> String {
                    let len = t.chars().count();
                    if whole || len <= PREVIEW {
                        t.to_string()
                    } else {
                        let head: String = t.chars().take(PREVIEW).collect();
                        format!("{head}… [{len} chars — action list with id {} for the full text]", i.n)
                    }
                };
                let mut o = json!({
                    "n": i.n,
                    "kind": i.kind,
                    "title": i.title,
                    "status": i.status,
                    "author": i.author,
                });
                let m = o.as_object_mut().unwrap();
                if let Some(d) = &i.detail {
                    m.insert("detail".into(), json!(short(d)));
                }
                if !i.choices.is_empty() {
                    m.insert("choices".into(), json!(i.choices));
                }
                if let Some(b) = i.done_by {
                    m.insert("done_by".into(), json!(b));
                }
                if !i.comments.is_empty() {
                    let skip = if whole { 0 } else { i.comments.len().saturating_sub(PREVIEW_COMMENTS) };
                    let shown: Vec<Value> =
                        i.comments[skip..].iter().map(|c| json!({"by": c.by, "text": short(&c.text)})).collect();
                    m.insert("comments".into(), json!(shown));
                    if skip > 0 {
                        m.insert("earlier_comments".into(), json!(format!("{skip} not shown — action list with id {}", i.n)));
                    }
                }
                if let Some(c) = &i.chosen {
                    m.insert("user_chose".into(), json!(c));
                }
                if let Some(f) = i.for_id {
                    m.insert("for".into(), json!(format!("claude#{f}")));
                }
                if i.urgent {
                    m.insert("urgent".into(), json!(true));
                }
                o
            })
            .collect();
        json!(items)
    }
}

/// The hub message an UPDATE sends. Self-contained on purpose: a claude started
/// before this feature existed has no `hub_todo` tool and no rule about it,
/// and must still be able to act on what the user said.
pub fn render_update(id: usize, d: &Diff) -> String {
    let mut out = format!(
        "The user updated your To Do list (claude#{id}) in Mulpex and pressed UPDATE. What changed:\n"
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
    section("Decisions made", &d.chosen);
    section("Comments from the user (answer on that item: hub_todo action \"comment\")", &d.comments);
    section("Added by the user (new requests for you)", &d.added);
    section("Dismissed by the user (gone from your list)", &d.dismissed);
    out.push_str(
        "Act on this now. Keep the list current with mcp__mulpex__hub_todo (mark items done — that removes them — \
         and add new items); answer comments on their item (action \"comment\"). Don't recap this \
         message or the list in your reply: the user sees the list in Mulpex, so just do the work. \
         Do NOT hub_send a reply.",
    );
    out
}

/// What a claude is told when the user turns its list on or off (context menu).
pub fn toggle_message(on: bool) -> String {
    if on {
        "The user turned ON your \"To Do\" list in Mulpex (the side panel). From now on, put \
         anything you need from them on it with mcp__mulpex__hub_todo — including anything \
         you are waiting on them for right now — and keep what is on the list out of your replies \
         (a one-line pointer like \"Added To Do #3\" at most). Mulpex reminds you of the details every turn. Do NOT hub_send a reply."
            .to_string()
    } else {
        "The user turned OFF your \"To Do\" list in Mulpex. Stop using mcp__mulpex__hub_todo \
         and ask them in your replies as usual. Do NOT hub_send a reply."
            .to_string()
    }
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
pub struct TodoSend {
    pub id: usize,
    pub state: String,
    pub why: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct TodoFile {
    dir: PathBuf,
    lists: BTreeMap<usize, List>,
}

/// The durable store, cached in memory — the poll loop reads counts every tick,
/// and the app is the only writer, so the file is written through, never re-read.
/// Interior mutability because `Core::persist_sessions` (which prunes) is `&self`.
pub struct TodoStore {
    path: PathBuf,
    project_dir: PathBuf,
    lists: Mutex<BTreeMap<usize, List>>,
    /// Ids whose list changed since the poll loop last emitted `todos-changed`.
    dirty: Mutex<BTreeSet<usize>>,
    /// Bumped on every change, so the phone's view can say "re-fetch" without
    /// carrying the lists themselves.
    rev: std::sync::atomic::AtomicU64,
}

impl TodoStore {
    /// The file beside `session_store_path` (`<home>/sessions/<key>.txt` →
    /// `<home>/todos/<key>.json`), loaded. Another project's file (a hash
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
        let path = home.join("todos").join(format!("{stem}.json"));
        // Before the rename from "For me" the list lived in `<home>/asks/`;
        // move it over once, so an upgrade keeps every list.
        let old = home.join("asks").join(format!("{stem}.json"));
        if !path.exists() && old.exists() {
            let _ = std::fs::create_dir_all(home.join("todos"));
            let _ = std::fs::rename(&old, &path);
        }
        let lists = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<TodoFile>(&t).ok())
            .filter(|f| f.dir == project_dir)
            .map(|f| f.lists)
            .map(|mut lists| {
                lists.values_mut().for_each(List::migrate);
                lists
            })
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

    pub fn counts(&self, id: usize) -> Counts {
        self.lists.lock().unwrap().get(&id).map(List::counts).unwrap_or_default()
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

    fn c3(c: Counts) -> (u32, u32, u32) {
        (c.open, c.pending, c.unread)
    }

    fn add(list: &mut List, items: Value) -> Result<String, String> {
        list.apply_claude(&json!({ "action": "add", "items": items }), 1)
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-todos-test-{}", mulpex_core::persist::new_uuid()));
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
        // Done takes the item off the list; there is nothing to reopen.
        l.apply_claude(&json!({"action":"done","id":[1,3]}), 3).unwrap();
        assert!(l.items.is_empty());
        assert!(l.apply_claude(&json!({"action":"reopen","id":1}), 4).is_err());
        add(&mut l, json!([{"kind":"action","title":"deploy the thing"}])).unwrap();
        // `for` tags a worker's ask an orchestrator collected.
        add(&mut l, json!([{"kind":"question","title":"token?","for":"claude#7"}])).unwrap();
        assert_eq!(l.items.last().unwrap().for_id, Some(7));
        assert!(l.claude_view(None).to_string().contains("\"for\":\"claude#7\""));
        assert!(add(&mut l, json!([{"kind":"question","title":"x","for":"bob"}])).is_err());
        // Duplicate open title is refused with a pointer.
        let e = add(&mut l, json!([{"kind":"action","title":"deploy the thing"}])).unwrap_err();
        assert!(e.contains("#4"), "{e}");
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
        l.apply_user(TodoEdit::Done { n: 1, done: true }).unwrap();
        l.apply_user(TodoEdit::Choose { n: 2, choice: Some("bar".into()) }).unwrap();
        l.apply_user(TodoEdit::Comment { n: 3, text: "5173 בבקשה".into() }).unwrap();
        l.apply_user(TodoEdit::Dismiss { n: 4, on: true }).unwrap();
        l.apply_user(TodoEdit::Add { kind: Kind::Action, title: "also bump version".into(), detail: Some("to 0.41".into()) }).unwrap();
        assert!(l.apply_user(TodoEdit::Choose { n: 2, choice: Some("baz".into()) }).is_err());
        assert_eq!(c3(l.counts()), (4, 5, 0));

        let view = l.claude_view(None).to_string();
        assert!(!view.contains("5173") && !view.contains("bump"), "{view}");

        let d = l.commit(9).unwrap();
        assert_eq!(d.done, vec!["#1 [action] run migration"]);
        assert_eq!(d.chosen, vec!["#2 [decision] pick a name → chose: \"bar\""]);
        assert_eq!(d.comments, vec!["#3 [question] מה הפורט? — \"5173 בבקשה\""]);
        assert_eq!(d.dismissed, vec!["#4 [action] obsolete"]);
        assert_eq!(d.added, vec!["#5 [action] also bump version\n    to 0.41"]);
        assert_eq!(l.drafts.count(), 0);
        assert!(l.items.iter().all(|i| i.n != 1), "a checked item leaves the list at UPDATE");
        let view = l.claude_view(None).to_string();
        assert!(view.contains("5173") && view.contains("\"user_chose\":\"bar\""), "{view}");
        assert!(view.contains("\"comments\":[{\"by\":\"user\""), "{view}");
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
        l.apply_user(TodoEdit::Done { n: 1, done: true }).unwrap();
        l.apply_user(TodoEdit::Done { n: 1, done: false }).unwrap();
        assert_eq!(l.drafts.count(), 0);
        l.apply_user(TodoEdit::Comment { n: 1, text: "".into() }).unwrap();
        assert_eq!(l.drafts.count(), 0);
    }

    #[test]
    fn removing_an_item_keeps_only_the_users_words() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"question","title":"q1"},{"kind":"action","title":"a2"}])).unwrap();
        l.apply_user(TodoEdit::Comment { n: 1, text: "my answer".into() }).unwrap();
        l.apply_user(TodoEdit::Done { n: 2, done: true }).unwrap();
        l.apply_claude(&json!({"action":"remove","id":[1,2]}), 2).unwrap();
        assert!(l.drafts.edits[&1].orphan.is_some());
        assert!(!l.drafts.edits.contains_key(&2), "a bare tick is dropped");
        let d = l.commit(3).unwrap();
        assert_eq!(d.comments, vec!["(re: #1 [question] \"q1\", no longer on your list) — \"my answer\""]);
    }

    #[test]
    fn a_thread_grows_from_both_sides_and_tracks_unread() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"question","title":"which port?"}])).unwrap();
        // The user's reply is a draft: the claude does not see it yet.
        l.apply_user(TodoEdit::Comment { n: 1, text: "5173".into() }).unwrap();
        assert!(!l.claude_view(None).to_string().contains("5173"));
        // One draft per item: writing again replaces it; blank clears it.
        l.apply_user(TodoEdit::Comment { n: 1, text: "5174".into() }).unwrap();
        assert_eq!(l.drafts.count(), 1);
        let d = l.commit(2).unwrap();
        assert_eq!(d.comments, vec!["#1 [question] which port? — \"5174\""]);
        assert!(render_update(3, &d).contains("action \"comment\""));
        assert_eq!(l.items[0].comments.len(), 1);
        assert_eq!(c3(l.counts()), (1, 0, 0), "your own comment is never unread");

        // The claude answers on the item: unread until the thread is opened.
        l.apply_claude(&json!({"action":"comment","id":1,"text":"taken — 5175?"}), 3).unwrap();
        assert_eq!(c3(l.counts()), (1, 0, 1));
        assert!(l.claude_view(None).to_string().contains("taken — 5175?"));
        l.apply_user(TodoEdit::Seen { n: 1 }).unwrap();
        assert_eq!(c3(l.counts()), (1, 0, 0));
        assert_eq!(l.drafts.count(), 0, "opening a thread is not a draft");

        // A reply marks the thread read too.
        l.apply_claude(&json!({"action":"comment","id":1,"text":"or 5176"}), 4).unwrap();
        l.apply_user(TodoEdit::Comment { n: 1, text: "5176".into() }).unwrap();
        l.commit(5).unwrap();
        assert_eq!(c3(l.counts()), (1, 0, 0));
        assert_eq!(
            l.items[0].comments.iter().map(|c| c.by).collect::<Vec<_>>(),
            vec![Author::User, Author::Claude, Author::Claude, Author::User]
        );

        // Bad shapes.
        assert!(l.apply_claude(&json!({"action":"comment","id":1}), 6).is_err());
        assert!(l.apply_claude(&json!({"action":"comment","id":[1,1],"text":"x"}), 6).is_err());
        assert!(l.apply_claude(&json!({"action":"comment","id":9,"text":"x"}), 6).is_err());
    }

    #[test]
    fn the_list_previews_long_text_and_one_id_reads_in_full() {
        let mut l = List::default();
        let long = "x".repeat(5000);
        add(&mut l, json!([{"kind":"action","title":"QA it","detail": long}])).unwrap();
        for k in 0..5 {
            l.apply_claude(&json!({"action":"comment","id":1,"text":format!("c{k}")}), 2).unwrap();
        }
        let v = l.claude_view(None).to_string();
        assert!(v.len() < 1500, "the preview carried the whole detail: {}", v.len());
        assert!(v.contains("5000 chars") && v.contains("2 not shown") && v.contains("c4") && !v.contains("\"c1\""));
        let v = l.claude_view(Some(1)).to_string();
        assert!(v.contains(&long) && v.contains("\"c0\"") && !v.contains("not shown"));
        // 20 000 characters fit; more are cut.
        add(&mut l, json!([{"kind":"action","title":"big","detail": "y".repeat(25_000)}])).unwrap();
        assert_eq!(l.items[1].detail.as_ref().unwrap().chars().count(), MAX_DETAIL);
    }

    #[test]
    fn urgent_is_set_and_cleared_by_the_claude_and_counted_for_the_badge() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"action","title":"a"},{"kind":"decision","title":"b","urgent":true}])).unwrap();
        assert!(!l.items[0].urgent && l.items[1].urgent);
        assert_eq!(l.counts().urgent, 1);
        assert!(l.claude_view(None).to_string().contains("\"urgent\":true"));
        l.apply_claude(&json!({"action":"update","id":1,"urgent":true}), 2).unwrap();
        l.apply_claude(&json!({"action":"update","id":2,"urgent":false}), 2).unwrap();
        assert!(l.items[0].urgent && !l.items[1].urgent);
        // Done takes it off the badge with the item.
        l.apply_claude(&json!({"action":"done","id":1}), 3).unwrap();
        assert_eq!(l.counts().urgent, 0);
        assert!(add(&mut l, json!([{"kind":"action","title":"c","urgent":"yes"}])).is_err());
        assert!(l.apply_claude(&json!({"action":"update","id":2,"urgent":1}), 4).is_err());
    }

    #[test]
    fn an_old_note_becomes_the_first_comment() {
        let mut l: List = serde_json::from_value(json!({
            "next": 1,
            "items": [{"n":1,"kind":"question","title":"q","status":"open","author":"claude",
                       "note":"my old answer","created":1,"updated":2}],
            "drafts": {"edits": {"1": {"note": "a draft"}}}
        }))
        .unwrap();
        l.migrate();
        assert_eq!(l.items[0].comments, vec![Comment { by: Author::User, text: "my old answer".into(), at: 2 }]);
        assert_eq!(c3(l.counts()), (1, 1, 0));
        assert_eq!(l.drafts.edits[&1].comment.as_deref(), Some("a draft"));
        assert!(!serde_json::to_string(&l).unwrap().contains("\"note\""), "never written back");
    }

    #[test]
    fn a_claude_change_marks_the_draft_stale_but_keeps_it() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"decision","title":"d","choices":["a","b"]}])).unwrap();
        l.apply_user(TodoEdit::Choose { n: 1, choice: Some("a".into()) }).unwrap();
        l.apply_claude(&json!({"action":"update","id":1,"choices":["b","c"]}), 2).unwrap();
        assert!(l.items[0].rev > l.drafts.edits[&1].base_rev);
        let d = l.commit(3).unwrap();
        assert!(d.chosen[0].contains("no longer one of your choices"), "{:?}", d.chosen);
    }

    #[test]
    fn done_by_the_claude_keeps_the_users_words() {
        let mut l = List::default();
        add(&mut l, json!([{"kind":"action","title":"a"},{"kind":"action","title":"b"}])).unwrap();
        l.apply_user(TodoEdit::Comment { n: 2, text: "thanks".into() }).unwrap();
        l.apply_claude(&json!({"action":"done","id":[1,2]}), 2).unwrap();
        assert!(l.items.is_empty());
        let d = l.commit(3).unwrap();
        assert_eq!(d.comments, vec!["(re: #2 [action] \"b\", no longer on your list) — \"thanks\""]);
    }

    #[test]
    fn done_items_saved_before_auto_clear_are_dropped_on_load() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        let s = TodoStore::beside(&sp, Path::new("/p"));
        s.mutate(3, |l| add(l, json!([{"kind":"action","title":"a"},{"kind":"action","title":"b"}]))).unwrap();
        s.mutate(3, |l| {
            l.items[0].status = Status::Done;
            Ok(())
        })
        .unwrap();
        let again = TodoStore::beside(&sp, Path::new("/p"));
        assert_eq!(again.get(3).unwrap().items.iter().map(|i| i.n).collect::<Vec<_>>(), vec![2]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn store_round_trips_prunes_and_ignores_other_projects() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        let s = TodoStore::beside(&sp, Path::new("/p"));
        s.mutate(3, |l| add(l, json!([{"kind":"action","title":"x"}]))).unwrap();
        s.mutate(3, |l| l.apply_user(TodoEdit::Comment { n: 1, text: "draft".into() })).unwrap();
        s.mutate(7, |l| add(l, json!([{"kind":"action","title":"y"}]))).unwrap();
        assert_eq!(s.take_dirty(), vec![3, 7]);
        // A failed op changes nothing and dirties nothing.
        assert!(s.mutate(3, |l| l.apply_claude(&json!({"action":"bogus"}), 1)).is_err());
        assert!(s.take_dirty().is_empty());

        let again = TodoStore::beside(&sp, Path::new("/p"));
        assert_eq!(again.get(3).unwrap().drafts.count(), 1, "drafts persist");
        assert!(TodoStore::beside(&sp, Path::new("/other")).all().is_empty());

        again.retain(&HashSet::from([7]));
        assert!(again.get(3).is_none());
        assert_eq!(again.take_dirty(), vec![3]);
        again.retain(&HashSet::new());
        assert!(!home.join("todos/proj-abc.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A list saved before the rename (`<home>/asks/`) is moved to `todos/`.
    #[test]
    fn a_list_saved_under_the_old_name_is_migrated() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        TodoStore::beside(&sp, Path::new("/p"))
            .mutate(3, |l| add(l, json!([{"kind":"action","title":"x"}])))
            .unwrap();
        std::fs::create_dir_all(home.join("asks")).unwrap();
        std::fs::rename(home.join("todos/proj-abc.json"), home.join("asks/proj-abc.json")).unwrap();

        let s = TodoStore::beside(&sp, Path::new("/p"));
        assert_eq!(s.get(3).unwrap().items.len(), 1, "the old list was not read");
        assert!(home.join("todos/proj-abc.json").exists());
        assert!(!home.join("asks/proj-abc.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
