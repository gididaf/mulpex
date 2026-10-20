//! The Board: one per project, Trello-like — columns the USER defines, cards
//! assigned to claudes, a comment thread and images on each card.
//!
//! Replaced the per-claude "To Do" list (`todos.rs`, 2026-10-20). The changes
//! that shaped it:
//!
//! - **One board per project**, not one list per claude. A card's `assignee`
//!   says whose it is; every claude sees the board, but `hub_board` returns only
//!   its own cards unless it asks for `scope: "all"` (an orchestrator).
//! - **Live board, UPDATE to tell**: every edit lands on the board at once (no
//!   drafts), and is queued for the card's assignee (`Outbox`) until the user
//!   presses UPDATE — then each claude gets ONE message from "Mulpex". (A 5 s
//!   timer after the last edit was tried first; the user wanted the trigger.)
//!   A change to an unassigned card tells nobody.
//! - **Generic columns.** Only the user edits them; each has an optional
//!   description, and `columns_text` is what every claude reads per turn — so
//!   "Waiting for QA: move here when you need me to test" is the user's workflow,
//!   not Mulpex's.
//! - **Archive, not auto-removal.** Archived cards never reach a claude's
//!   context (`claude_view` skips them, and cards in the last column show their
//!   title only) — the board grows, the context doesn't.
//! - **Images are attachments**, stored beside the board (`img/<id>`), never
//!   Markdown `<img>`: the sanitizer stays as strict as it was. A claude gets
//!   their absolute paths and Reads them; it attaches its own by path.
//!
//! The app is the only writer: claudes reach it through `termreq/` (`op:
//! "board"`), the panel/remote desktop/phone through commands.
//!
//! No field anywhere in here may be called `handle`: `remote/client.rs::lift`
//! rewrites every such key in a payload forwarded to a remote desktop.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    Claude,
    User,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Column {
    pub id: u32,
    pub name: String,
    /// What goes here, in the user's words — the rule claudes follow.
    #[serde(default)]
    pub desc: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Comment {
    /// Stable, so the user can edit or delete their comment until UPDATE sends
    /// it. Empty on comments saved before ids existed.
    #[serde(default)]
    pub id: String,
    pub by: Author,
    /// Which claude wrote it, when `by` is claude.
    #[serde(default)]
    pub who: Option<usize>,
    pub text: String,
    #[serde(default)]
    pub images: Vec<String>,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Card {
    /// Per project, monotonic, never reused — "#3" means one card forever.
    pub n: u32,
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// Images attached to the card itself (not to a comment).
    #[serde(default)]
    pub images: Vec<String>,
    pub col: u32,
    #[serde(default)]
    pub assignee: Option<usize>,
    /// Set and cleared by a claude only: pinned on top of its column, in red.
    #[serde(default)]
    pub urgent: bool,
    #[serde(default)]
    pub choices: Vec<String>,
    /// The user's pick, as its TEXT — survives the claude reordering choices.
    #[serde(default)]
    pub chosen: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub archived: bool,
    pub author: Author,
    /// The claude that created it, when `author` is claude.
    #[serde(default)]
    pub by: Option<usize>,
    /// A claude changed it since the user last opened it.
    #[serde(default)]
    pub unread: bool,
    pub created: u64,
    pub updated: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Board {
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub next: u32,
    #[serde(default)]
    pub next_col: u32,
    pub columns: Vec<Column>,
    /// Stored in display order: per column, urgent first (`normalize`).
    #[serde(default)]
    pub cards: Vec<Card>,
    /// Claudes' column changes waiting for the user's Approve / Reject.
    #[serde(default)]
    pub proposals: Vec<Proposal>,
    #[serde(default)]
    pub next_prop: u32,
}

/// A claude's change to the columns, as one all-or-nothing set: nothing moves
/// until the user approves it in the panel (`Board::decide`), and the claude is
/// told the answer at once — no UPDATE.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Proposal {
    pub id: u32,
    pub by: usize,
    #[serde(default)]
    pub why: String,
    pub changes: Vec<ColChange>,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ColChange {
    /// At the end, or before `before`.
    Add { name: String, desc: String, before: Option<ColRef> },
    Edit { col: ColRef, name: Option<String>, desc: Option<String> },
    /// To the end, or before `before`.
    Move { col: ColRef, before: Option<ColRef> },
    /// Its cards go to `to`, or are archived — as the user's own delete.
    Delete { col: ColRef, to: Option<ColRef> },
}

/// A column a change names: one on the board (by id, so the user renaming it
/// meanwhile doesn't break the proposal), or one an earlier `Add` of the same
/// proposal makes (by the name it is added with).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ColRef {
    Id(u32),
    New(String),
}

impl Default for Board {
    fn default() -> Self {
        let col = |id, name: &str, desc: &str| Column { id, name: name.into(), desc: desc.into() };
        Board {
            on: false,
            next: 0,
            next_col: 3,
            columns: vec![
                col(1, "To Do", "Not started yet."),
                col(2, "Doing", "Being worked on now. Move a card here when you start it."),
                col(3, "Done", "Finished. Move a card here once it is done."),
            ],
            cards: Vec::new(),
            proposals: Vec::new(),
            next_prop: 0,
        }
    }
}

/// One edit from the user (desktop panel, remote desktop, phone), tagged by `op`.
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BoardEdit {
    Add {
        title: String,
        #[serde(default)]
        detail: Option<String>,
        col: u32,
        #[serde(default)]
        assignee: Option<usize>,
        #[serde(default)]
        images: Vec<String>,
    },
    Edit {
        n: u32,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        images: Option<Vec<String>>,
    },
    /// Into column `col`, before card `before` (None = at the end).
    Move { n: u32, col: u32, #[serde(default)] before: Option<u32> },
    Assign { n: u32, #[serde(default)] to: Option<usize> },
    Choose { n: u32, #[serde(default)] choice: Option<String> },
    Comment { n: u32, text: String, #[serde(default)] images: Vec<String> },
    /// The user's own comment, while UPDATE has not sent it yet (`Core::board_edit` checks).
    CommentEdit { n: u32, id: String, text: String, #[serde(default)] images: Vec<String> },
    CommentDelete { n: u32, id: String },
    Archive { n: u32, on: bool },
    ArchiveColumn { col: u32 },
    /// Permanent. The user only.
    Delete { n: u32 },
    Seen { n: u32 },
    ColAdd { name: String, #[serde(default)] desc: String },
    ColEdit { col: u32, #[serde(default)] name: Option<String>, #[serde(default)] desc: Option<String> },
    ColMove { col: u32, #[serde(default)] before: Option<u32> },
    /// Its cards go to `to`, or are archived when `to` is None.
    ColDelete { col: u32, #[serde(default)] to: Option<u32> },
    /// A claude's column proposal (`Core::board_edit` → `Board::decide`).
    Approve { proposal: u32 },
    Reject { proposal: u32, #[serde(default)] reason: String },
}

/// What happened to a card, for the claude it concerns.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Change {
    Added,
    Moved,
    AssignedToYou,
    /// Taken off you and given to someone else (or nobody).
    Unassigned(Option<usize>),
    Edited,
    /// The message reads the comment's CURRENT text by `id` — it may have been
    /// edited or deleted before UPDATE.
    Comment {
        #[serde(default)]
        id: String,
        text: String,
        images: Vec<String>,
    },
    Chose(Option<String>),
    Archived(bool),
    Deleted,
}

/// One change for one claude.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub to: usize,
    pub n: u32,
    /// `None` = the user; `Some(c)` = claude#c did it.
    pub by: Option<usize>,
    pub change: Change,
    /// The card's title when the event happened — a deleted card has no other.
    pub title: String,
    /// The card as it was before the edit that made this event (`None` = it
    /// did not exist yet). The first one per card is the baseline UPDATE
    /// compares against, so a move there and back is no change.
    #[serde(default)]
    pub before: Option<Snap>,
}

/// What of a card a claude would be told about, to tell a real change from a
/// round trip.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snap {
    pub col: u32,
    pub assignee: Option<usize>,
    pub title: String,
    pub detail: Option<String>,
    pub images: Vec<String>,
    pub chosen: Option<String>,
    pub archived: bool,
}

impl Snap {
    fn of(c: &Card) -> Self {
        Snap {
            col: c.col,
            assignee: c.assignee,
            title: c.title.clone(),
            detail: c.detail.clone(),
            images: c.images.clone(),
            chosen: c.chosen.clone(),
            archived: c.archived,
        }
    }
}

/// Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cards on the board that are not archived. A board this full has stopped
/// being one.
pub const MAX_ACTIVE: usize = 300;
const MAX_COLUMNS: usize = 12;
const MAX_TITLE: usize = 200;
const MAX_COL_NAME: usize = 40;
const MAX_COL_DESC: usize = 500;
/// Changes in one column proposal, and proposals waiting at once.
const MAX_PROP_CHANGES: usize = 20;
const MAX_PROPOSALS: usize = 20;
/// Details and comments are Markdown; details are where long content goes.
const MAX_DETAIL: usize = 20_000;
const MAX_COMMENT: usize = 20_000;
const MAX_COMMENTS: usize = 200;
const MAX_CHOICES: usize = 8;
const MAX_CHOICE: usize = 120;
pub const MAX_IMAGES: usize = 10;
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// In the list a claude gets back, longer text is cut to this; `id` reads a
/// card in full. Otherwise every call re-sends every long detail.
const PREVIEW: usize = 300;

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

/// An instance number, `"7"` or `"claude#7"`; `null` = nobody. `Ok(None)` when absent.
fn instance_of(v: Option<&Value>, what: &str) -> Result<Option<Option<usize>>, String> {
    let Some(v) = v else { return Ok(None) };
    let n = match v {
        Value::Null => return Ok(Some(None)),
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_start_matches("claude#").trim_start_matches('#').parse().ok(),
        _ => None,
    };
    match n {
        Some(n) if n >= 1 => Ok(Some(Some(n as usize))),
        _ => Err(format!("`{what}` must be an instance number, like 7 or \"claude#7\", or null")),
    }
}

fn bool_of(v: Option<&Value>, what: &str) -> Result<Option<bool>, String> {
    match v {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{what}` must be true or false")),
    }
}

/// `id` as one number or an array of them.
fn ids_of(v: &Value) -> Result<Vec<u32>, String> {
    let one = |x: &Value| x.as_u64().map(|n| n as u32);
    match v.get("id") {
        Some(Value::Array(a)) if !a.is_empty() => {
            a.iter().map(|x| one(x).ok_or_else(|| "`id` must be card numbers".to_string())).collect()
        }
        Some(x) => one(x).map(|n| vec![n]).ok_or_else(|| "missing `id` (a card number, or an array)".into()),
        None => Err("missing `id` (a card number, or an array)".into()),
    }
}

fn one_id(v: &Value, action: &str) -> Result<u32, String> {
    let ids = ids_of(v)?;
    match ids[..] {
        [n] => Ok(n),
        _ => Err(format!("`{action}` takes one `id`")),
    }
}

/// `images`: absolute paths from a claude, resolved to stored ids by `import`.
fn images_of(
    v: Option<&Value>,
    import: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let arr = v.as_array().ok_or("`images` must be an array of absolute file paths")?;
    if arr.len() > MAX_IMAGES {
        return Err(format!("at most {MAX_IMAGES} images at once"));
    }
    arr.iter()
        .map(|p| p.as_str().ok_or_else(|| "`images` must be an array of absolute file paths".to_string()))
        .map(|p| p.and_then(|p| import(p)))
        .collect()
}

impl Board {
    pub fn col(&self, id: u32) -> Option<&Column> {
        self.columns.iter().find(|c| c.id == id)
    }

    fn col_name(&self, id: u32) -> String {
        self.col(id).map(|c| c.name.clone()).unwrap_or_else(|| "?".into())
    }

    fn last_col(&self) -> Option<u32> {
        self.columns.last().map(|c| c.id)
    }

    /// A column by id (number) or by name (case-insensitive).
    fn col_by(&self, v: &Value) -> Result<u32, String> {
        let names = || self.columns.iter().map(|c| format!("{:?}", c.name)).collect::<Vec<_>>().join(", ");
        if let Some(n) = v.as_u64() {
            if self.col(n as u32).is_some() {
                return Ok(n as u32);
            }
        }
        if let Some(s) = v.as_str() {
            let s = s.trim();
            if let Some(c) = self.columns.iter().find(|c| c.name.eq_ignore_ascii_case(s)) {
                return Ok(c.id);
            }
        }
        Err(format!("no column {v} — the columns are {}", names()))
    }

    fn card(&self, n: u32) -> Option<&Card> {
        self.cards.iter().find(|c| c.n == n)
    }

    fn card_mut(&mut self, n: u32) -> Result<&mut Card, String> {
        let next = self.next;
        self.cards.iter_mut().find(|c| c.n == n).ok_or_else(|| {
            if n >= 1 && n <= next {
                format!("#{n} is no longer on the board (deleted by the user)")
            } else {
                format!("#{n} is not on the board")
            }
        })
    }

    fn active_count(&self) -> usize {
        self.cards.iter().filter(|c| !c.archived).count()
    }

    /// Display order is storage order: within each column urgent cards first,
    /// otherwise stable. Every mutation ends here, so a position the panel
    /// draws is a position it can send back.
    fn normalize(&mut self) {
        let cards = std::mem::take(&mut self.cards);
        let (urgent, rest): (Vec<Card>, Vec<Card>) = cards.into_iter().partition(|c| c.urgent && !c.archived);
        self.cards = urgent;
        self.cards.extend(rest);
    }

    /// Put card `n` (already removed) into `col`: before `before`, at the top
    /// (`top`), or at the end.
    fn place(&mut self, mut card: Card, col: u32, before: Option<u32>, top: bool) {
        card.col = col;
        let at = match before {
            Some(b) => self.cards.iter().position(|c| c.n == b && c.col == col),
            None if top => self.cards.iter().position(|c| c.col == col),
            None => None,
        };
        match at {
            Some(i) => self.cards.insert(i, card),
            None => {
                // After the column's last card, so the stored order stays grouped.
                let last = self.cards.iter().rposition(|c| c.col == col);
                match last {
                    Some(i) => self.cards.insert(i + 1, card),
                    None => self.cards.push(card),
                }
            }
        }
    }

    fn take(&mut self, n: u32) -> Option<Card> {
        let i = self.cards.iter().position(|c| c.n == n)?;
        Some(self.cards.remove(i))
    }

    /// Events for the claudes a change to `card` concerns: its assignee, unless
    /// that is who did it.
    fn tell(card: &Card, by: Option<usize>, change: Change, out: &mut Vec<Event>) {
        if let Some(to) = card.assignee.filter(|a| Some(*a) != by) {
            out.push(Event { to, n: card.n, by, change, title: card.title.clone(), before: None });
        }
    }

    /// Counts for one claude's sidebar badge: its active cards outside the last
    /// column, those a claude changed since the user looked, and urgent ones.
    pub fn counts(&self, id: usize) -> Counts {
        let last = self.last_col();
        let mine = || self.cards.iter().filter(move |c| !c.archived && c.assignee == Some(id));
        Counts {
            open: mine().filter(|c| Some(c.col) != last).count() as u32,
            unread: mine().filter(|c| c.unread).count() as u32,
            urgent: mine().filter(|c| c.urgent).count() as u32,
        }
    }

    #[cfg(test)]
    pub fn unread_total(&self) -> u32 {
        self.cards.iter().filter(|c| !c.archived && c.unread).count() as u32
    }

    /// Every image id the board still refers to.
    pub fn images(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for c in &self.cards {
            out.extend(c.images.iter().cloned());
            for m in &c.comments {
                out.extend(m.images.iter().cloned());
            }
        }
        out
    }

    /// A claude's change, from a `hub_board` call (`list` is the caller's).
    /// `import` turns an image path into a stored id. Returns a one-line note
    /// and the events for OTHER claudes (an orchestrator assigning a worker).
    pub fn apply_claude(
        &mut self,
        from: usize,
        v: &Value,
        now: u64,
        import: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<(String, Vec<Event>), String> {
        let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
        let by = Some(from);
        let mut ev = Vec::new();
        let snaps = self.snaps();
        let note = match action {
            "add" => {
                let items = v
                    .get("items")
                    .and_then(|x| x.as_array())
                    .filter(|a| !a.is_empty())
                    .ok_or("`add` needs `items`: a non-empty array of {title, detail?, column?, assignee?, ...}")?;
                // Validate everything first: a batch is all or nothing.
                let mut fresh = Vec::new();
                for it in items {
                    let title = one_line(it.get("title").and_then(|x| x.as_str()).unwrap_or(""), MAX_TITLE);
                    if title.is_empty() {
                        return Err("every card needs a `title`".into());
                    }
                    let assignee = instance_of(it.get("assignee"), "assignee")?.unwrap_or(Some(from));
                    if let Some(dup) =
                        self.cards.iter().find(|c| !c.archived && c.title == title && c.assignee == assignee)
                    {
                        return Err(format!("#{} already says {title:?} — update it instead of adding a duplicate", dup.n));
                    }
                    let col = match it.get("column") {
                        Some(c) => self.col_by(c)?,
                        None => self.columns.first().map(|c| c.id).ok_or("the board has no columns")?,
                    };
                    let detail = clean_detail(it.get("detail").and_then(|x| x.as_str()));
                    let choices = clean_choices(it.get("choices"))?;
                    let urgent = bool_of(it.get("urgent"), "urgent")?.unwrap_or(false);
                    let images = images_of(it.get("images"), import)?;
                    fresh.push((title, detail, col, assignee, choices, urgent, images));
                }
                if self.active_count() + fresh.len() > MAX_ACTIVE {
                    return Err(format!("the board already has {MAX_ACTIVE} cards — archive some first"));
                }
                let mut added = Vec::new();
                for (title, detail, col, assignee, choices, urgent, images) in fresh {
                    self.next += 1;
                    let card = Card {
                        n: self.next,
                        title,
                        detail,
                        images,
                        col,
                        assignee,
                        urgent,
                        choices,
                        chosen: None,
                        comments: Vec::new(),
                        archived: false,
                        author: Author::Claude,
                        by,
                        unread: true,
                        created: now,
                        updated: now,
                    };
                    Self::tell(&card, by, Change::Added, &mut ev);
                    added.push(format!("#{}", card.n));
                    self.place(card, col, None, true);
                }
                format!("added {}", added.join(", "))
            }
            "update" => {
                let n = one_id(v, "update")?;
                let title = v
                    .get("title")
                    .and_then(|x| x.as_str())
                    .map(|t| one_line(t, MAX_TITLE))
                    .filter(|t| !t.is_empty());
                let detail = v.get("detail").map(|d| clean_detail(d.as_str()));
                let choices = v.get("choices").map(|c| clean_choices(Some(c))).transpose()?;
                let urgent = bool_of(v.get("urgent"), "urgent")?;
                let assignee = instance_of(v.get("assignee"), "assignee")?;
                let images = images_of(v.get("images"), import)?;
                if title.is_none()
                    && detail.is_none()
                    && choices.is_none()
                    && urgent.is_none()
                    && assignee.is_none()
                    && images.is_empty()
                {
                    return Err("`update` needs at least one of title, detail, choices, urgent, assignee, images".into());
                }
                let card = self.card_mut(n)?;
                if card.images.len() + images.len() > MAX_IMAGES {
                    return Err(format!("#{n} can hold at most {MAX_IMAGES} images"));
                }
                let before = card.assignee;
                if let Some(t) = title {
                    card.title = t;
                }
                if let Some(d) = detail {
                    card.detail = d;
                }
                if let Some(c) = choices {
                    if card.chosen.as_ref().is_some_and(|ch| !c.contains(ch)) {
                        card.chosen = None;
                    }
                    card.choices = c;
                }
                if let Some(u) = urgent {
                    card.urgent = u;
                }
                card.images.extend(images);
                card.updated = now;
                card.unread = true;
                let mut assigned = false;
                if let Some(a) = assignee {
                    if a != before {
                        card.assignee = a;
                        assigned = true;
                    }
                }
                let card = card.clone();
                if assigned {
                    Self::tell(&card, by, Change::AssignedToYou, &mut ev);
                    if let Some(old) = before.filter(|o| *o != from) {
                        ev.push(Event { to: old, n, by, change: Change::Unassigned(card.assignee), title: card.title.clone(), before: None });
                    }
                } else {
                    Self::tell(&card, by, Change::Edited, &mut ev);
                }
                format!("updated #{n}")
            }
            "move" => {
                let ids = ids_of(v)?;
                let col = self.col_by(v.get("column").ok_or("`move` needs `column` (its name)")?)?;
                for n in &ids {
                    self.card_mut(*n)?;
                }
                for n in &ids {
                    let mut card = self.take(*n).expect("checked above");
                    let moved = card.col != col || card.archived;
                    card.archived = false;
                    card.updated = now;
                    card.unread = true;
                    if moved {
                        Self::tell(&card, by, Change::Moved, &mut ev);
                    }
                    self.place(card, col, None, true);
                }
                let list = ids.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                format!("moved {list} to {:?}", self.col_name(col))
            }
            "comment" => {
                let n = one_id(v, "comment")?;
                let text = v.get("text").and_then(|x| x.as_str()).map(str::trim).unwrap_or("");
                let images = images_of(v.get("images"), import)?;
                if text.is_empty() && images.is_empty() {
                    return Err("`comment` needs `text` (and/or `images`)".into());
                }
                let card = self.card_mut(n)?;
                if card.comments.len() >= MAX_COMMENTS {
                    return Err(format!("#{n} already has {MAX_COMMENTS} comments"));
                }
                let text = cut(text, MAX_COMMENT);
                let id = mulpex_core::persist::new_uuid();
                card.comments.push(Comment { id: id.clone(), by: Author::Claude, who: by, text: text.clone(), images: images.clone(), at: now });
                card.updated = now;
                card.unread = true;
                let card = card.clone();
                Self::tell(&card, by, Change::Comment { id, text, images }, &mut ev);
                format!("commented on #{n}")
            }
            "archive" => {
                let ids = ids_of(v)?;
                for n in &ids {
                    self.card_mut(*n)?;
                }
                for n in &ids {
                    let card = self.card_mut(*n).expect("checked above");
                    if !card.archived {
                        card.archived = true;
                        card.updated = now;
                        let card = card.clone();
                        Self::tell(&card, by, Change::Archived(true), &mut ev);
                    }
                }
                let list = ids.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                format!("archived {list}")
            }
            "delete" | "remove" | "done" => {
                return Err(format!(
                    "there is no `{action}` — move the card to the column that fits (e.g. the last one), or `archive` it; only the user deletes cards"
                ))
            }
            "columns" => {
                let id = self.propose(from, v, now)?;
                format!(
                    "column proposal P{id} is waiting for the user — nothing changed yet. Mulpex messages you \
                     their Approve / Reject; carry on meanwhile"
                )
            }
            "list" => String::new(),
            "" => return Err("missing `action` (list, add, update, move, comment, archive, columns)".into()),
            other => {
                return Err(format!(
                    "unknown action {other:?} — use list, add, update, move, comment, archive or columns (to propose a column change)"
                ))
            }
        };
        self.normalize();
        Self::stamp(&mut ev, &snaps);
        Ok((note, ev))
    }

    fn snaps(&self) -> BTreeMap<u32, Snap> {
        self.cards.iter().map(|c| (c.n, Snap::of(c))).collect()
    }

    fn stamp(ev: &mut [Event], snaps: &BTreeMap<u32, Snap>) {
        for e in ev {
            e.before = snaps.get(&e.n).cloned();
        }
    }

    /// The user's edit. Returns the events for the claudes it concerns and the
    /// image ids that are no longer referenced (a deleted card's), for removal.
    pub fn apply_user(&mut self, edit: BoardEdit, now: u64) -> Result<(Vec<Event>, Vec<String>), String> {
        let mut ev = Vec::new();
        let mut gone_images = Vec::new();
        let snaps = self.snaps();
        match edit {
            BoardEdit::Add { title, detail, col, assignee, images } => {
                let title = one_line(&title, MAX_TITLE);
                if title.is_empty() {
                    return Err("a title is needed".into());
                }
                if self.col(col).is_none() {
                    return Err("that column is gone".into());
                }
                if images.len() > MAX_IMAGES {
                    return Err(format!("at most {MAX_IMAGES} images"));
                }
                if self.active_count() >= MAX_ACTIVE {
                    return Err(format!("the board already has {MAX_ACTIVE} cards — archive some first"));
                }
                self.next += 1;
                let card = Card {
                    n: self.next,
                    title,
                    detail: clean_detail(detail.as_deref()),
                    images,
                    col,
                    assignee,
                    urgent: false,
                    choices: Vec::new(),
                    chosen: None,
                    comments: Vec::new(),
                    archived: false,
                    author: Author::User,
                    by: None,
                    unread: false,
                    created: now,
                    updated: now,
                };
                Self::tell(&card, None, Change::Added, &mut ev);
                self.place(card, col, None, false);
            }
            BoardEdit::Edit { n, title, detail, images } => {
                let card = self.card_mut(n)?;
                let mut changed = false;
                if let Some(t) = title.map(|t| one_line(&t, MAX_TITLE)).filter(|t| !t.is_empty()) {
                    changed |= card.title != t;
                    card.title = t;
                }
                if let Some(d) = detail {
                    let d = clean_detail(Some(&d));
                    changed |= card.detail != d;
                    card.detail = d;
                }
                if let Some(imgs) = images {
                    if imgs.len() > MAX_IMAGES {
                        return Err(format!("at most {MAX_IMAGES} images"));
                    }
                    gone_images.extend(card.images.iter().filter(|i| !imgs.contains(i)).cloned());
                    changed |= card.images != imgs;
                    card.images = imgs;
                }
                if changed {
                    card.updated = now;
                    let card = card.clone();
                    Self::tell(&card, None, Change::Edited, &mut ev);
                }
            }
            BoardEdit::Move { n, col, before } => {
                if self.col(col).is_none() {
                    return Err("that column is gone".into());
                }
                let mut card = self.take(n).ok_or(format!("#{n} is no longer on the board"))?;
                let moved = card.col != col;
                if moved {
                    card.updated = now;
                    Self::tell(&card, None, Change::Moved, &mut ev);
                }
                self.place(card, col, before.filter(|b| *b != n), false);
            }
            BoardEdit::Assign { n, to } => {
                let card = self.card_mut(n)?;
                let before = card.assignee;
                if before != to {
                    card.assignee = to;
                    card.updated = now;
                    let card = card.clone();
                    Self::tell(&card, None, Change::AssignedToYou, &mut ev);
                    if let Some(old) = before {
                        ev.push(Event { to: old, n, by: None, change: Change::Unassigned(to), title: card.title.clone(), before: None });
                    }
                }
            }
            BoardEdit::Choose { n, choice } => {
                let card = self.card_mut(n)?;
                if let Some(c) = &choice {
                    if !card.choices.contains(c) {
                        return Err(format!("{c:?} is not one of #{n}'s choices"));
                    }
                }
                if card.chosen != choice {
                    card.chosen = choice.clone();
                    card.updated = now;
                    let card = card.clone();
                    Self::tell(&card, None, Change::Chose(choice), &mut ev);
                }
            }
            BoardEdit::Comment { n, text, images } => {
                let text = cut(text.trim(), MAX_COMMENT);
                if text.is_empty() && images.is_empty() {
                    return Err("write something or attach an image".into());
                }
                if images.len() > MAX_IMAGES {
                    return Err(format!("at most {MAX_IMAGES} images"));
                }
                let card = self.card_mut(n)?;
                if card.comments.len() >= MAX_COMMENTS {
                    return Err(format!("#{n} already has {MAX_COMMENTS} comments"));
                }
                let id = mulpex_core::persist::new_uuid();
                card.comments.push(Comment { id: id.clone(), by: Author::User, who: None, text: text.clone(), images: images.clone(), at: now });
                card.updated = now;
                // Writing on a card means it was read.
                card.unread = false;
                let card = card.clone();
                Self::tell(&card, None, Change::Comment { id, text, images }, &mut ev);
            }
            BoardEdit::CommentEdit { n, id, text, images } => {
                let text = cut(text.trim(), MAX_COMMENT);
                if text.is_empty() && images.is_empty() {
                    return Err("an empty comment — delete it instead".into());
                }
                if images.len() > MAX_IMAGES {
                    return Err(format!("at most {MAX_IMAGES} images"));
                }
                let card = self.card_mut(n)?;
                let c = card
                    .comments
                    .iter_mut()
                    .find(|c| c.id == id && !id.is_empty() && c.by == Author::User)
                    .ok_or("that comment is gone")?;
                gone_images.extend(c.images.iter().filter(|i| !images.contains(i)).cloned());
                c.text = text;
                c.images = images;
            }
            BoardEdit::CommentDelete { n, id } => {
                let card = self.card_mut(n)?;
                let i = card
                    .comments
                    .iter()
                    .position(|c| c.id == id && !id.is_empty() && c.by == Author::User)
                    .ok_or("that comment is gone")?;
                gone_images.extend(card.comments.remove(i).images);
            }
            BoardEdit::Archive { n, on } => {
                let card = self.card_mut(n)?;
                if card.archived != on {
                    card.archived = on;
                    card.updated = now;
                    if on {
                        card.unread = false;
                    }
                    let card = card.clone();
                    Self::tell(&card, None, Change::Archived(on), &mut ev);
                }
            }
            BoardEdit::ArchiveColumn { col } => {
                for card in self.cards.iter_mut().filter(|c| c.col == col && !c.archived) {
                    card.archived = true;
                    card.unread = false;
                    card.updated = now;
                    Self::tell(card, None, Change::Archived(true), &mut ev);
                }
            }
            BoardEdit::Delete { n } => {
                let card = self.take(n).ok_or(format!("#{n} is no longer on the board"))?;
                gone_images.extend(card.images.iter().cloned());
                for m in &card.comments {
                    gone_images.extend(m.images.iter().cloned());
                }
                // An archived card was already reported gone.
                if !card.archived {
                    Self::tell(&card, None, Change::Deleted, &mut ev);
                }
            }
            BoardEdit::Seen { n } => {
                self.card_mut(n)?.unread = false;
            }
            BoardEdit::ColAdd { name, desc } => {
                let name = one_line(&name, MAX_COL_NAME);
                if name.is_empty() {
                    return Err("a column needs a name".into());
                }
                if self.columns.len() >= MAX_COLUMNS {
                    return Err(format!("at most {MAX_COLUMNS} columns"));
                }
                self.next_col += 1;
                self.columns.push(Column { id: self.next_col, name, desc: cut(desc.trim(), MAX_COL_DESC) });
            }
            BoardEdit::ColEdit { col, name, desc } => {
                let c = self.columns.iter_mut().find(|c| c.id == col).ok_or("that column is gone")?;
                if let Some(n) = name.map(|n| one_line(&n, MAX_COL_NAME)).filter(|n| !n.is_empty()) {
                    c.name = n;
                }
                if let Some(d) = desc {
                    c.desc = cut(d.trim(), MAX_COL_DESC);
                }
            }
            BoardEdit::ColMove { col, before } => {
                let i = self.columns.iter().position(|c| c.id == col).ok_or("that column is gone")?;
                let c = self.columns.remove(i);
                let at = before.and_then(|b| self.columns.iter().position(|c| c.id == b)).unwrap_or(self.columns.len());
                self.columns.insert(at, c);
            }
            BoardEdit::Approve { .. } | BoardEdit::Reject { .. } => {
                return Err("a proposal is decided by `Board::decide`".into());
            }
            BoardEdit::ColDelete { col, to } => {
                if self.columns.len() <= 1 {
                    return Err("the board needs at least one column".into());
                }
                if self.col(col).is_none() {
                    return Err("that column is gone".into());
                }
                if let Some(t) = to {
                    if t == col || self.col(t).is_none() {
                        return Err("pick another column to move its cards to".into());
                    }
                }
                let ns: Vec<u32> = self.cards.iter().filter(|c| c.col == col).map(|c| c.n).collect();
                for n in ns {
                    let mut card = self.take(n).expect("listed above");
                    match to {
                        Some(t) => {
                            if !card.archived {
                                card.updated = now;
                                Self::tell(&card, None, Change::Moved, &mut ev);
                            }
                            self.place(card, t, None, false);
                        }
                        None => {
                            // Archived cards keep a column to come back to.
                            let first = self.columns.iter().find(|c| c.id != col).map(|c| c.id).unwrap();
                            if !card.archived {
                                card.archived = true;
                                card.updated = now;
                                Self::tell(&card, None, Change::Archived(true), &mut ev);
                            }
                            self.place(card, first, None, false);
                        }
                    }
                }
                self.columns.retain(|c| c.id != col);
            }
        }
        // Events carry the column the card ended up in only when rendered, so
        // fill nothing here; `normalize` keeps the display order.
        self.normalize();
        Self::stamp(&mut ev, &snaps);
        Ok((ev, gone_images))
    }

    /// `action: "columns"`: validate a claude's column changes by applying
    /// them to a copy, and queue them for the user. Names resolve as the
    /// changes go, so a later change may name a column an earlier one added
    /// or renamed. The proposal's id.
    fn propose(&mut self, from: usize, v: &Value, now: u64) -> Result<u32, String> {
        const SHAPE: &str = "`columns` needs `changes`: a non-empty array of {op: \"add\", name, desc?, before?} | \
            {op: \"edit\", column, name?, desc?} | {op: \"move\", column, before?} | {op: \"delete\", column, to?} \
            (columns by name; `before` omitted = at the end; `to` omitted = its cards are archived)";
        let items = v.get("changes").and_then(|x| x.as_array()).filter(|a| !a.is_empty()).ok_or(SHAPE)?;
        if items.len() > MAX_PROP_CHANGES {
            return Err(format!("at most {MAX_PROP_CHANGES} changes in one proposal"));
        }
        if self.proposals.len() >= MAX_PROPOSALS {
            return Err(format!("{MAX_PROPOSALS} column proposals already wait for the user — wait for their answer"));
        }
        let mut sim = self.clone();
        let mut news: Vec<(String, u32)> = Vec::new();
        let mut changes = Vec::new();
        for (i, it) in items.iter().enumerate() {
            let at = |e: String| format!("change {}: {e}", i + 1);
            let text = |k: &str| it.get(k).and_then(|x| x.as_str()).map(str::to_string);
            // A name on `sim` → a ref that survives until approval.
            let refer = |sim: &Board, news: &[(String, u32)], x: &Value| -> Result<ColRef, String> {
                let id = sim.col_by(x)?;
                Ok(match news.iter().find(|(_, n)| *n == id) {
                    Some((name, _)) => ColRef::New(name.clone()),
                    None => ColRef::Id(id),
                })
            };
            let opt = |sim: &Board, news: &[(String, u32)], k: &str| -> Result<Option<ColRef>, String> {
                match it.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(x) => refer(sim, news, x).map(Some),
                }
            };
            let need = |k: &str| it.get(k).filter(|x| !x.is_null()).ok_or_else(|| at(format!("needs `{k}`")));
            let ch = match it.get("op").and_then(|x| x.as_str()).unwrap_or("") {
                "add" => ColChange::Add {
                    name: one_line(&text("name").unwrap_or_default(), MAX_COL_NAME),
                    desc: cut(text("desc").unwrap_or_default().trim(), MAX_COL_DESC),
                    before: opt(&sim, &news, "before").map_err(at)?,
                },
                "edit" | "rename" => {
                    let name = text("name").map(|n| one_line(&n, MAX_COL_NAME)).filter(|n| !n.is_empty());
                    let desc = text("desc").map(|d| cut(d.trim(), MAX_COL_DESC));
                    if name.is_none() && desc.is_none() {
                        return Err(at("`edit` needs a new `name` and/or `desc`".into()));
                    }
                    ColChange::Edit { col: refer(&sim, &news, need("column")?).map_err(at)?, name, desc }
                }
                "move" => ColChange::Move {
                    col: refer(&sim, &news, need("column")?).map_err(at)?,
                    before: opt(&sim, &news, "before").map_err(at)?,
                },
                "delete" | "remove" => ColChange::Delete {
                    col: refer(&sim, &news, need("column")?).map_err(at)?,
                    to: opt(&sim, &news, "to").map_err(at)?,
                },
                _ => return Err(at(SHAPE.into())),
            };
            sim.apply_change(&ch, &mut news, now).map_err(at)?;
            changes.push(ch);
        }
        let mut seen = HashSet::new();
        if let Some(c) = sim.columns.iter().find(|c| !seen.insert(c.name.to_lowercase())) {
            return Err(format!("two columns would be called {:?}", c.name));
        }
        self.next_prop += 1;
        let why = cut(v.get("why").and_then(|x| x.as_str()).unwrap_or("").trim(), MAX_COL_DESC);
        self.proposals.push(Proposal { id: self.next_prop, by: from, why, changes, at: now });
        Ok(self.next_prop)
    }

    /// One change of a proposal, through the user's own column edits.
    fn apply_change(&mut self, ch: &ColChange, news: &mut Vec<(String, u32)>, now: u64) -> Result<Vec<Event>, String> {
        let id = |r: &ColRef| -> Result<u32, String> {
            match r {
                ColRef::Id(i) => self.col(*i).map(|c| c.id).ok_or_else(|| "a column it names is gone".into()),
                ColRef::New(n) => news.iter().find(|(m, _)| m == n).map(|(_, i)| *i).ok_or_else(|| format!("no new column {n:?}")),
            }
        };
        let edit = match ch {
            ColChange::Add { name, desc, before } => {
                let before = before.as_ref().map(&id).transpose()?;
                let (mut ev, _) = self.apply_user(BoardEdit::ColAdd { name: name.clone(), desc: desc.clone() }, now)?;
                let col = self.next_col;
                news.push((name.clone(), col));
                if before.is_some() {
                    ev.extend(self.apply_user(BoardEdit::ColMove { col, before }, now)?.0);
                }
                return Ok(ev);
            }
            ColChange::Edit { col, name, desc } => BoardEdit::ColEdit { col: id(col)?, name: name.clone(), desc: desc.clone() },
            ColChange::Move { col, before } => BoardEdit::ColMove { col: id(col)?, before: before.as_ref().map(&id).transpose()? },
            ColChange::Delete { col, to } => BoardEdit::ColDelete { col: id(col)?, to: to.as_ref().map(&id).transpose()? },
        };
        Ok(self.apply_user(edit, now)?.0)
    }

    /// The user's Approve / Reject of proposal `id`: all its changes or none —
    /// an approval that no longer fits the board (a column it names was
    /// deleted meanwhile) is refused and the proposal stays, to reject. Returns
    /// who proposed it, the message for them, and the cards' events.
    pub fn decide(&mut self, id: u32, approve: bool, reason: &str, now: u64) -> Result<(usize, String, Vec<Event>), String> {
        let i = self.proposals.iter().position(|p| p.id == id).ok_or("that proposal is gone")?;
        if !approve {
            let p = self.proposals.remove(i);
            let reason = reason.trim();
            let why = if reason.is_empty() { String::new() } else { format!(" Their reason: {reason}") };
            let msg = format!(
                "The user REJECTED your Board column proposal P{id} — the columns did not change.{why} \
                 Don't propose it again unless the user asks. Do NOT hub_send a reply."
            );
            return Ok((p.by, msg, Vec::new()));
        }
        let p = self.proposals[i].clone();
        let mut next = self.clone();
        next.proposals.remove(i);
        let mut news = Vec::new();
        let mut ev = Vec::new();
        for ch in &p.changes {
            ev.extend(next.apply_change(ch, &mut news, now).map_err(|e| format!("can't apply P{id}: {e} — reject it instead"))?);
        }
        *self = next;
        let msg = format!(
            "The user APPROVED your Board column proposal P{id}. The columns are now:\n{}Do NOT hub_send a reply.",
            self.columns_text()
        );
        Ok((p.by, msg, ev))
    }

    /// What a claude sees. By default only ITS cards that are not archived —
    /// the board can hold hundreds, and a claude's context must not. Cards in
    /// the last column show their title only. `scope: "all"` lists every
    /// active card, slimmer still; `id` shows one card (any) in full.
    pub fn claude_view(&self, from: usize, all: bool, full: Option<u32>, img_dir: &Path) -> Value {
        let last = self.last_col();
        let path = |id: &String| img_dir.join(id).display().to_string();
        let short = |t: &str, n: u32| -> String {
            let len = t.chars().count();
            if len <= PREVIEW {
                t.to_string()
            } else {
                let head: String = t.chars().take(PREVIEW).collect();
                format!("{head}… [{len} chars — list with id {n} for the full text]")
            }
        };
        let base = |c: &Card| -> Value {
            let mut o = json!({ "n": c.n, "title": c.title, "column": self.col_name(c.col) });
            let m = o.as_object_mut().unwrap();
            match c.assignee {
                Some(a) if a == from => {}
                Some(a) => {
                    m.insert("assignee".into(), json!(format!("claude#{a}")));
                }
                None => {
                    m.insert("assignee".into(), Value::Null);
                }
            }
            if c.urgent {
                m.insert("urgent".into(), json!(true));
            }
            o
        };
        if let Some(n) = full {
            let Some(c) = self.card(n) else { return json!({ "error": format!("#{n} is not on the board") }) };
            let mut o = base(c);
            let m = o.as_object_mut().unwrap();
            m.insert("author".into(), json!(c.author));
            if let Some(d) = &c.detail {
                m.insert("detail".into(), json!(d));
            }
            if !c.images.is_empty() {
                m.insert("images".into(), json!(c.images.iter().map(path).collect::<Vec<_>>()));
            }
            if !c.choices.is_empty() {
                m.insert("choices".into(), json!(c.choices));
            }
            if let Some(ch) = &c.chosen {
                m.insert("user_chose".into(), json!(ch));
            }
            if c.archived {
                m.insert("archived".into(), json!(true));
            }
            let comments: Vec<Value> = c
                .comments
                .iter()
                .map(|k| {
                    let mut o = json!({ "by": match (k.by, k.who) {
                        (Author::User, _) => "user".to_string(),
                        (Author::Claude, Some(w)) => format!("claude#{w}"),
                        (Author::Claude, None) => "claude".to_string(),
                    }, "text": k.text });
                    if !k.images.is_empty() {
                        o["images"] = json!(k.images.iter().map(path).collect::<Vec<_>>());
                    }
                    o
                })
                .collect();
            if !comments.is_empty() {
                m.insert("comments".into(), json!(comments));
            }
            return o;
        }
        let cards: Vec<Value> = self
            .cards
            .iter()
            .filter(|c| !c.archived && (all || c.assignee == Some(from)))
            .map(|c| {
                let mut o = base(c);
                let finished = Some(c.col) == last;
                if all || finished {
                    return o;
                }
                let m = o.as_object_mut().unwrap();
                if let Some(d) = &c.detail {
                    m.insert("detail".into(), json!(short(d, c.n)));
                }
                if !c.images.is_empty() {
                    m.insert("images".into(), json!(c.images.len()));
                }
                if !c.choices.is_empty() {
                    m.insert("choices".into(), json!(c.choices));
                }
                if let Some(ch) = &c.chosen {
                    m.insert("user_chose".into(), json!(ch));
                }
                if let Some(k) = c.comments.last() {
                    m.insert(
                        "last_comment".into(),
                        json!({ "by": if k.by == Author::User { "user" } else { "claude" }, "text": short(&k.text, c.n) }),
                    );
                    if c.comments.len() > 1 {
                        m.insert("comments".into(), json!(c.comments.len()));
                    }
                }
                o
            })
            .collect();
        json!({ "columns": self.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), "cards": cards })
    }

    /// The columns as the per-turn rule shows them.
    pub fn columns_text(&self) -> String {
        let mut out = String::new();
        for (i, c) in self.columns.iter().enumerate() {
            out.push_str(&format!("{}. {:?}", i + 1, c.name));
            if !c.desc.trim().is_empty() {
                out.push_str(" — ");
                out.push_str(c.desc.trim());
            }
            out.push('\n');
        }
        out
    }
}

/// One claude's badge numbers.
#[derive(Default, Debug, PartialEq, Clone, Copy)]
pub struct Counts {
    pub open: u32,
    pub unread: u32,
    pub urgent: u32,
}

/// What happened to one card, folded from its events: the latest of each kind
/// wins, and comments accumulate.
#[derive(Default, Debug, Clone, PartialEq)]
struct Folded {
    title: String,
    by: BTreeSet<Option<usize>>,
    added: bool,
    moved: bool,
    assigned: bool,
    unassigned: Option<Option<usize>>,
    edited: bool,
    /// (by, comment id, text, images) as posted; see `live_comments`.
    comments: Vec<(Option<usize>, String, String, Vec<String>)>,
    chose: Option<Option<String>>,
    /// Whether it was archived BEFORE these changes — so archive-then-restore says nothing.
    was_archived: Option<bool>,
    deleted: bool,
    /// The card before the FIRST of these changes: `Some(None)` = it is new.
    /// `None` only for events saved before the baseline existed.
    base: Option<Option<Snap>>,
}

fn fold(events: &[Event]) -> BTreeMap<u32, Folded> {
    let mut cards: BTreeMap<u32, Folded> = BTreeMap::new();
    for e in events {
        let f = cards.entry(e.n).or_default();
        f.base.get_or_insert_with(|| e.before.clone());
        f.title = e.title.clone();
        f.by.insert(e.by);
        match &e.change {
            Change::Added => f.added = true,
            Change::Moved => f.moved = true,
            Change::AssignedToYou => {
                f.assigned = true;
                f.unassigned = None;
            }
            Change::Unassigned(to) => {
                f.unassigned = Some(*to);
                f.assigned = false;
            }
            Change::Edited => f.edited = true,
            Change::Comment { id, text, images } => f.comments.push((e.by, id.clone(), text.clone(), images.clone())),
            Change::Chose(c) => f.chose = Some(c.clone()),
            Change::Archived(a) => {
                f.was_archived.get_or_insert(!a);
            }
            Change::Deleted => f.deleted = true,
        }
    }
    cards
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Sending {
    events: Vec<Event>,
    /// Since when it has been waiting for its claude to be able to hear it.
    /// Not saved: a relaunch restarts the wait.
    #[serde(skip)]
    since: Option<Instant>,
}

/// Changes waiting to be told, per claude, in two stages:
///
/// - `waiting` — the USER's changes, held until they press UPDATE (2026-10-20:
///   a 5 s timer after the last edit was the first design; the user asked for a
///   manual trigger instead, so a half-done round of edits is never sent).
/// - `sending` — released (by UPDATE, or a claude's change to another claude's
///   card, which goes on its own), delivered once the claude can hear it.
///
/// Saved with the board, so a quit keeps what was not sent yet.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Outbox {
    #[serde(default)]
    waiting: BTreeMap<usize, Vec<Event>>,
    #[serde(default)]
    sending: BTreeMap<usize, Sending>,
}

impl Outbox {
    /// `user`: the user's edits wait for UPDATE; a claude's go out on their own.
    pub fn push(&mut self, events: Vec<Event>, user: bool, now: Instant) {
        for e in events {
            if user {
                self.waiting.entry(e.to).or_default().push(e);
            } else {
                let s = self.sending.entry(e.to).or_default();
                s.since.get_or_insert(now);
                s.events.push(e);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty() && self.sending.is_empty()
    }

    /// The cards with a REAL change waiting for UPDATE, per claude — a move
    /// there and back, or a pick then un-pick, is none.
    pub fn pending(&self, board: &Board) -> BTreeMap<usize, Vec<u32>> {
        self.waiting
            .iter()
            .filter_map(|(id, evs)| {
                let ns: Vec<u32> = fold(evs)
                    .iter()
                    .filter(|(n, f)| net(*id, **n, f, board).is_some_and(|x| x.any()))
                    .map(|(n, _)| *n)
                    .collect();
                (!ns.is_empty()).then_some((*id, ns))
            })
            .collect()
    }

    /// The user's comments still waiting for UPDATE — the ones they may edit or delete.
    pub fn unsent_comments(&self) -> Vec<String> {
        self.waiting
            .values()
            .flatten()
            .filter(|e| e.by.is_none())
            .filter_map(|e| match &e.change {
                Change::Comment { id, .. } if !id.is_empty() => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Claudes with anything queued at all, real or not.
    pub fn waiting_ids(&self) -> Vec<usize> {
        self.waiting.keys().copied().collect()
    }

    pub fn sending_ids(&self) -> Vec<usize> {
        self.sending.keys().copied().collect()
    }

    /// UPDATE for `id`: its waiting changes go out. False if it had none —
    /// none that change anything are dropped, never sent.
    pub fn release(&mut self, id: usize, board: &Board, now: Instant) -> bool {
        let real = self.pending(board).contains_key(&id);
        let Some(evs) = self.waiting.remove(&id) else { return false };
        if !real {
            return false;
        }
        let s = self.sending.entry(id).or_default();
        s.since.get_or_insert(now);
        s.events.extend(evs);
        true
    }

    /// Forget every queue of a claude that is gone for good.
    pub fn forget(&mut self, keep: impl Fn(usize) -> bool) -> bool {
        let before = (self.waiting.len(), self.sending.len());
        self.waiting.retain(|id, _| keep(*id));
        self.sending.retain(|id, _| keep(*id));
        before != (self.waiting.len(), self.sending.len())
    }

    /// Claudes with released changes, and how long they have waited.
    pub fn due(&self, now: Instant) -> Vec<(usize, Duration)> {
        self.sending
            .iter()
            .map(|(id, s)| (*id, s.since.map(|t| now.duration_since(t)).unwrap_or_default()))
            .collect()
    }

    fn start_clocks(&mut self, now: Instant) {
        for s in self.sending.values_mut() {
            s.since.get_or_insert(now);
        }
    }

    /// Take `id`'s released changes and render the message, against the board
    /// as it is NOW (a card moved twice is reported where it ended). `None`
    /// when every change cancelled out.
    pub fn take(&mut self, id: usize, board: &Board, img_dir: &Path) -> Option<String> {
        let s = self.sending.remove(&id)?;
        render(id, &fold(&s.events), board, img_dir)
    }
}

fn who(by: Option<usize>) -> String {
    match by {
        None => "the user".into(),
        Some(c) => format!("claude#{c}"),
    }
}

/// What one card's folded changes add up to for claude `id`, against its
/// baseline. `None` when there is nothing to say (a card made and deleted
/// before UPDATE).
#[derive(Default)]
struct Net {
    deleted: bool,
    added: bool,
    assigned: bool,
    /// Taken off this claude; to whom.
    lost: Option<Option<usize>>,
    moved: bool,
    edited: bool,
    archived: Option<bool>,
    chose: Option<Option<String>>,
    comments: bool,
}

impl Net {
    fn any(&self) -> bool {
        self.deleted
            || self.added
            || self.assigned
            || self.lost.is_some()
            || self.moved
            || self.edited
            || self.archived.is_some()
            || self.chose.is_some()
            || self.comments
    }
}

/// The folded comments as they read NOW: an edited one with its new text, a
/// deleted one gone. (One without an id — saved before ids — as posted.)
fn live_comments(f: &Folded, card: Option<&Card>) -> Vec<(Option<usize>, String, Vec<String>)> {
    f.comments
        .iter()
        .filter_map(|(by, id, text, images)| {
            if id.is_empty() {
                return Some((*by, text.clone(), images.clone()));
            }
            let c = card?.comments.iter().find(|c| &c.id == id)?;
            Some((*by, c.text.clone(), c.images.clone()))
        })
        .collect()
}

fn net(id: usize, n: u32, f: &Folded, board: &Board) -> Option<Net> {
    let card = board.card(n);
    let mut x = Net { comments: !live_comments(f, card).is_empty(), ..Net::default() };
    match (&f.base, card) {
        // Made and deleted before UPDATE: nothing happened.
        (Some(None), None) => return None,
        // Never this claude's, before or after (handed over and back): nothing for it.
        (Some(Some(b)), None) if b.assignee != Some(id) => return None,
        (Some(Some(b)), Some(c)) if b.assignee != Some(id) && c.assignee != Some(id) => return None,
        (_, None) => x.deleted = true,
        (Some(None), Some(_)) => x.added = true,
        (Some(Some(b)), Some(c)) => {
            let (was, now) = (b.assignee == Some(id), c.assignee == Some(id));
            x.assigned = now && !was;
            if was && !now {
                x.lost = Some(c.assignee);
            }
            x.moved = c.col != b.col;
            x.edited = c.title != b.title || c.detail != b.detail || c.images != b.images;
            x.archived = (c.archived != b.archived).then_some(c.archived);
            x.chose = (c.chosen != b.chosen).then(|| c.chosen.clone());
        }
        // Saved before baselines existed: trust the folded flags.
        (None, Some(c)) => {
            x.added = f.added;
            x.assigned = f.assigned && c.assignee == Some(id);
            if f.unassigned.is_some() && c.assignee != Some(id) {
                x.lost = Some(c.assignee);
            }
            x.moved = f.moved;
            x.edited = f.edited;
            x.archived = f.was_archived.filter(|w| *w != c.archived).map(|_| c.archived);
            x.chose = f.chose.clone().filter(|ch| *ch == c.chosen);
        }
    }
    Some(x)
}

fn render(id: usize, cards: &BTreeMap<u32, Folded>, board: &Board, img_dir: &Path) -> Option<String> {
    let mut lines = Vec::new();
    for (n, f) in cards {
        let Some(x) = net(id, *n, f, board).filter(Net::any) else { continue };
        let mut parts: Vec<String> = Vec::new();
        let actors = f.by.iter().map(|b| who(*b)).collect::<Vec<_>>().join(" and ");
        let Some(card) = board.card(*n).filter(|_| !x.deleted) else {
            lines.push(format!("#{n} {:?} — deleted by {actors}. Drop it.", f.title));
            continue;
        };
        if let Some(to) = x.lost {
            lines.push(format!(
                "#{n} {:?} — no longer yours: {actors} gave it to {}. Stop working on it.",
                card.title,
                to.map(|t| format!("claude#{t}")).unwrap_or_else(|| "nobody".into())
            ));
            continue;
        }
        if x.added {
            parts.push(format!("new card for you from {actors}"));
        } else if x.assigned {
            parts.push("assigned to you".into());
        }
        if x.moved || x.added || x.assigned {
            parts.push(format!("now in {:?}", board.col_name(card.col)));
        }
        if let Some(a) = x.archived {
            parts.push(if a { "archived".into() } else { "restored from the archive".into() });
        }
        if x.edited {
            parts.push("title/detail/images edited".into());
        }
        if let Some(c) = &x.chose {
            parts.push(match c {
                Some(c) => format!("chose: {c:?}"),
                None => "cleared their choice".into(),
            });
        }
    let mut block = String::new();
        if !parts.is_empty() {
            block.push_str(&parts.join("; "));
        }
        for (by, text, images) in &live_comments(f, Some(card)) {
            block.push_str(&format!("\n    comment from {}:", who(*by)));
            for l in text.lines() {
                block.push_str("\n      ");
                block.push_str(l);
            }
            for i in images {
                block.push_str(&format!("\n      [image: {}]", img_dir.join(i).display()));
            }
        }
        // A card new to this claude carries what it says, so it can act at once.
        if x.added || x.edited || x.assigned {
            if let Some(d) = &card.detail {
                block.push_str("\n    detail:");
                for l in d.lines() {
                    block.push_str("\n      ");
                    block.push_str(l);
                }
            }
            for i in &card.images {
                block.push_str(&format!("\n    [image: {}]", img_dir.join(i).display()));
            }
        }
        if block.trim().is_empty() {
            continue;
        }
        lines.push(format!("#{n} {:?} — {}", card.title, block.trim_start()));
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = format!("Changes on the project's Board in Mulpex that concern you (claude#{id}):\n");
    for l in lines {
        out.push_str("  ");
        out.push_str(&l);
        out.push('\n');
    }
    out.push_str(
        "Read any [image: …] path with the Read tool. Act on this now. Keep the board current with \
         mcp__mulpex__hub_board (move cards as their columns describe; answer comments on their card). \
         Don't recap this message or the board in your reply — the user sees the board. \
         Do NOT hub_send a reply.",
    );
    Some(out)
}

/// What every claude is told when the user turns the board on or off.
pub fn toggle_message(on: bool, columns: &str) -> String {
    if on {
        format!(
            "The user turned ON the project's Board in Mulpex (the side panel). Use \
             mcp__mulpex__hub_board for what you work on with the user and what you need from them; \
             Mulpex reminds you of the rules every turn. The columns:\n{columns}Do NOT hub_send a reply."
        )
    } else {
        "The user turned OFF the project's Board in Mulpex. Stop using mcp__mulpex__hub_board and ask \
         them in your replies as usual. Do NOT hub_send a reply."
            .to_string()
    }
}

/// The image's type by its first bytes — never by its name.
pub fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}


/// An id the store hands out: `<uuid>.<ext>` and nothing else, so it can
/// never name a path outside the image dir.
fn valid_id(id: &str) -> bool {
    let Some((stem, ext)) = id.split_once('.') else { return false };
    !stem.is_empty()
        && stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && matches!(ext, "png" | "jpg" | "gif" | "webp")
}

#[derive(Serialize, Deserialize)]
struct BoardFile {
    dir: PathBuf,
    board: Board,
    #[serde(default)]
    outbox: Outbox,
}

/// The durable store, cached in memory; the app is the only writer.
pub struct BoardStore {
    path: PathBuf,
    img_dir: PathBuf,
    project_dir: PathBuf,
    board: Mutex<Board>,
    /// Lock order: `board`, then `outbox`.
    outbox: Mutex<Outbox>,
    /// Pasted images arriving in chunks, by upload token.
    uploads: Mutex<BTreeMap<String, Vec<u8>>>,
    dirty: std::sync::atomic::AtomicBool,
    /// Bumped on every change, so the phone's view can say "re-fetch".
    rev: std::sync::atomic::AtomicU64,
}

impl BoardStore {
    /// The board beside `session_store_path` (`<home>/sessions/<key>.txt` →
    /// `<home>/boards/<key>.json`, images in `<home>/boards/<key>/img/`).
    /// With none yet, the old per-claude To Do lists (`todos/<key>.json`) become
    /// its first cards; `todos_on` says which rows had their list on.
    pub fn beside(session_store_path: &Path, project_dir: &Path, todos_on: &HashSet<usize>) -> Self {
        let home = session_store_path.parent().and_then(Path::parent).unwrap_or_else(|| Path::new("."));
        let stem = session_store_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let path = home.join("boards").join(format!("{stem}.json"));
        let img_dir = home.join("boards").join(&stem).join("img");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<BoardFile>(&t).ok())
            .filter(|f| f.dir == project_dir)
            .map(|f| (f.board, f.outbox));
        let mut migrated = false;
        let (board, mut outbox) = match loaded {
            Some((b, o)) => (b, o),
            None => match legacy::migrate(home, &stem, project_dir, todos_on) {
                Some(b) => {
                    migrated = true;
                    (b, Outbox::default())
                }
                None => (Board::default(), Outbox::default()),
            },
        };
        outbox.start_clocks(Instant::now());
        let store = Self {
            path,
            img_dir,
            project_dir: project_dir.to_path_buf(),
            board: Mutex::new(board),
            outbox: Mutex::new(outbox),
            uploads: Mutex::new(BTreeMap::new()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            rev: std::sync::atomic::AtomicU64::new(0),
        };
        if migrated {
            store.save(&store.board.lock().unwrap(), &store.outbox.lock().unwrap());
            legacy::retire(home, &stem);
        }
        store.sweep_images();
        store
    }

    pub fn img_dir(&self) -> &Path {
        &self.img_dir
    }

    pub fn get(&self) -> Board {
        self.board.lock().unwrap().clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&Board) -> R) -> R {
        f(&self.board.lock().unwrap())
    }

    /// Run `f` on the board; written through and marked dirty only when it
    /// succeeds and something changed.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut Board) -> Result<R, String>) -> Result<R, String> {
        let mut board = self.board.lock().unwrap();
        let mut next = board.clone();
        let r = f(&mut next)?;
        if next != *board {
            *board = next;
            self.save(&board, &self.outbox.lock().unwrap());
            self.touch();
        }
        Ok(r)
    }

    /// The cards with a real change waiting for UPDATE, per claude.
    pub fn pending(&self) -> BTreeMap<usize, Vec<u32>> {
        let board = self.board.lock().unwrap();
        let ob = self.outbox.lock().unwrap();
        ob.pending(&board)
    }

    /// Read the outbox.
    pub fn with_outbox<R>(&self, f: impl FnOnce(&Outbox) -> R) -> R {
        f(&self.outbox.lock().unwrap())
    }

    /// Change the outbox (with the board, for rendering); saved and marked dirty.
    pub fn outbox<R>(&self, f: impl FnOnce(&mut Outbox, &Board) -> R) -> R {
        let board = self.board.lock().unwrap();
        let mut ob = self.outbox.lock().unwrap();
        let r = f(&mut ob, &board);
        self.save(&board, &ob);
        self.touch();
        r
    }

    pub fn touch(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        self.rev.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn rev(&self) -> u64 {
        self.rev.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    /// Store image bytes, checked by content. Returns its id.
    pub fn put_image(&self, bytes: &[u8]) -> Result<String, String> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!("an image can be at most {} MB", MAX_IMAGE_BYTES / 1024 / 1024));
        }
        let ext = image_ext(bytes).ok_or("only PNG, JPG, GIF and WebP images can be attached")?;
        let id = format!("{}.{ext}", mulpex_core::persist::new_uuid());
        std::fs::create_dir_all(&self.img_dir).map_err(|e| format!("can't save the image: {e}"))?;
        std::fs::write(self.img_dir.join(&id), bytes).map_err(|e| format!("can't save the image: {e}"))?;
        Ok(id)
    }

    /// Copy the image at `path` into the store (a claude's attachment, a file
    /// the user picked or dropped).
    pub fn import_image(&self, path: &str) -> Result<String, String> {
        let p = Path::new(path.trim());
        if !p.is_absolute() {
            return Err(format!("{path:?} is not an absolute path"));
        }
        let meta = std::fs::metadata(p).map_err(|e| format!("can't read {path:?}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{path:?} is not a file"));
        }
        if meta.len() as usize > MAX_IMAGE_BYTES {
            return Err(format!("{path:?} is over {} MB", MAX_IMAGE_BYTES / 1024 / 1024));
        }
        let bytes = std::fs::read(p).map_err(|e| format!("can't read {path:?}: {e}"))?;
        self.put_image(&bytes).map_err(|e| format!("{path:?}: {e}"))
    }

    /// One chunk of a pasted image (`board_image_put`).
    pub fn put_chunk(&self, upload: Option<&str>, data: &str, done: bool) -> Result<Value, String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|_| "that image could not be read".to_string())?;
        let mut ups = self.uploads.lock().unwrap();
        let token = match upload {
            Some(t) if ups.contains_key(t) => t.to_string(),
            Some(_) => return Err("that upload expired — attach the image again".into()),
            None => {
                // One paste at a time is the norm; a few stuck ones are dropped.
                while ups.len() >= 8 {
                    let k = ups.keys().next().cloned().unwrap();
                    ups.remove(&k);
                }
                let t = mulpex_core::persist::new_uuid();
                ups.insert(t.clone(), Vec::new());
                t
            }
        };
        let buf = ups.get_mut(&token).unwrap();
        buf.extend_from_slice(&bytes);
        if buf.len() > MAX_IMAGE_BYTES {
            ups.remove(&token);
            return Err(format!("an image can be at most {} MB", MAX_IMAGE_BYTES / 1024 / 1024));
        }
        if !done {
            return Ok(json!({ "upload": token }));
        }
        let all = ups.remove(&token).unwrap();
        drop(ups);
        Ok(json!({ "id": self.put_image(&all)? }))
    }

    /// Up to `CHUNK` bytes of image `id` from `offset`, base64 (`board_image_get`).
    pub fn get_chunk(&self, id: &str, offset: usize) -> Result<Value, String> {
        use base64::Engine;
        const CHUNK: usize = 3 * 1024 * 1024;
        let bytes = self.read_image(id)?;
        let from = offset.min(bytes.len());
        let to = (from + CHUNK).min(bytes.len());
        Ok(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes[from..to]),
            "total": bytes.len(),
        }))
    }

    pub fn read_image(&self, id: &str) -> Result<Vec<u8>, String> {
        if !valid_id(id) {
            return Err("not an image id".into());
        }
        std::fs::read(self.img_dir.join(id)).map_err(|_| "that image is gone".to_string())
    }

    pub fn has_image(&self, id: &str) -> bool {
        valid_id(id) && self.img_dir.join(id).is_file()
    }

    pub fn remove_images(&self, ids: &[String]) {
        let still = self.with(Board::images);
        for id in ids.iter().filter(|i| valid_id(i) && !still.contains(*i)) {
            let _ = std::fs::remove_file(self.img_dir.join(id));
        }
    }

    /// Images nothing refers to and older than a day: uploaded for a comment
    /// that was never sent, or left by a crash between copy and save.
    fn sweep_images(&self) {
        let Ok(rd) = std::fs::read_dir(&self.img_dir) else { return };
        let used = self.with(Board::images);
        let day = Duration::from_secs(24 * 3600);
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if old && !used.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Best-effort, atomic (tmp + rename).
    fn save(&self, board: &Board, outbox: &Outbox) {
        let Some(dir) = self.path.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let file = json!({ "dir": self.project_dir, "board": board, "outbox": outbox });
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, file.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// Reading the per-claude To Do lists the board replaced (`todos/<key>.json`,
/// and `asks/<key>.json` from before that rename). Only what a card keeps.
mod legacy {
    use super::*;

    #[derive(Deserialize)]
    struct File {
        dir: PathBuf,
        #[serde(default)]
        lists: BTreeMap<usize, List>,
    }
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        title: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        choices: Vec<String>,
        #[serde(default)]
        status: String,
        #[serde(default)]
        author: String,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        comments: Vec<OldComment>,
        #[serde(default)]
        seen: usize,
        #[serde(default)]
        chosen: Option<String>,
        #[serde(default)]
        created: u64,
        #[serde(default)]
        updated: u64,
        #[serde(default, rename = "for")]
        for_id: Option<usize>,
        #[serde(default)]
        urgent: bool,
    }
    #[derive(Deserialize)]
    struct OldComment {
        by: String,
        text: String,
        #[serde(default)]
        at: u64,
    }

    fn files(home: &Path, stem: &str) -> [PathBuf; 2] {
        [home.join("todos").join(format!("{stem}.json")), home.join("asks").join(format!("{stem}.json"))]
    }

    /// A board built from the old lists, or `None` when there was nothing.
    pub fn migrate(home: &Path, stem: &str, project_dir: &Path, todos_on: &HashSet<usize>) -> Option<Board> {
        let file = files(home, stem)
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<File>(&t).ok())
            .filter(|f| f.dir == project_dir);
        let lists = file.map(|f| f.lists).unwrap_or_default();
        if lists.values().all(|l| l.items.is_empty()) && todos_on.is_empty() {
            return None;
        }
        let mut b = Board { on: !todos_on.is_empty(), ..Board::default() };
        let first = b.columns[0].id;
        for (id, list) in lists {
            for it in list.items.into_iter().filter(|i| i.status != "done") {
                b.next += 1;
                let mut comments: Vec<Comment> = Vec::new();
                if let Some(t) = it.note.filter(|t| !t.trim().is_empty()) {
                    comments.push(Comment { id: String::new(), by: Author::User, who: None, text: t, images: vec![], at: it.updated });
                }
                for c in &it.comments {
                    let by = if c.by == "user" { Author::User } else { Author::Claude };
                    let who = (by == Author::Claude).then_some(id);
                    comments.push(Comment { id: String::new(), by, who, text: c.text.clone(), images: vec![], at: c.at });
                }
                let unread = it.comments.iter().skip(it.seen).any(|c| c.by != "user");
                // An ask an orchestrator collected for a worker stays on the
                // orchestrator, who relays — note who it was for.
                let title = match it.for_id {
                    Some(f) => format!("{} (for claude#{f})", it.title),
                    None => it.title,
                };
                b.cards.push(Card {
                    n: b.next,
                    title,
                    detail: it.detail,
                    images: vec![],
                    col: first,
                    assignee: Some(id),
                    urgent: it.urgent,
                    choices: it.choices,
                    chosen: it.chosen,
                    comments,
                    archived: false,
                    author: if it.author == "user" { Author::User } else { Author::Claude },
                    by: (it.author != "user").then_some(id),
                    unread,
                    created: it.created,
                    updated: it.updated,
                });
            }
        }
        b.normalize();
        Some(b)
    }

    /// Keep the old files, renamed, so a migration can be undone by hand.
    pub fn retire(home: &Path, stem: &str) {
        for p in files(home, stem) {
            if p.exists() {
                let _ = std::fs::rename(&p, p.with_extension("json.migrated"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_import(p: &str) -> Result<String, String> {
        Err(format!("unexpected import {p}"))
    }

    fn claude(b: &mut Board, from: usize, v: Value) -> Result<(String, Vec<Event>), String> {
        b.apply_claude(from, &v, 1, &mut no_import)
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mulpex-board-test-{}", mulpex_core::persist::new_uuid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn titles(b: &Board, col: u32) -> Vec<String> {
        b.cards.iter().filter(|c| c.col == col && !c.archived).map(|c| c.title.clone()).collect()
    }

    #[test]
    fn claude_adds_moves_and_numbers_never_reuse() {
        let mut b = Board::default();
        let (note, ev) = claude(&mut b, 3, json!({"action":"add","items":[{"title":"QA\n login"},{"title":"b","column":"doing"}]})).unwrap();
        assert_eq!(note, "added #1, #2");
        assert!(ev.is_empty(), "your own card tells nobody");
        assert_eq!(b.cards[0].title, "QA login");
        assert_eq!(b.card(2).unwrap().col, 2);
        assert_eq!(b.card(1).unwrap().assignee, Some(3));
        // Moving lands at the top of the column.
        claude(&mut b, 3, json!({"action":"move","id":1,"column":"Doing"})).unwrap();
        assert_eq!(titles(&b, 2), vec!["QA login", "b"]);
        // Only the user deletes, and only the user edits columns.
        assert!(claude(&mut b, 3, json!({"action":"delete","id":1})).unwrap_err().contains("archive"));
        assert!(claude(&mut b, 3, json!({"action":"col_add","name":"x"})).is_err());
        b.apply_user(BoardEdit::Delete { n: 2 }, 2).unwrap();
        assert!(claude(&mut b, 3, json!({"action":"update","id":2,"title":"x"})).unwrap_err().contains("no longer"));
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"c"}]})).unwrap();
        assert!(b.card(3).is_some());
        // Duplicate active title for the same assignee is refused with a pointer.
        assert!(claude(&mut b, 3, json!({"action":"add","items":[{"title":"c"}]})).unwrap_err().contains("#3"));
        // Bad column names say which exist.
        let e = claude(&mut b, 3, json!({"action":"move","id":3,"column":"QA"})).unwrap_err();
        assert!(e.contains("\"To Do\""), "{e}");
    }

    #[test]
    fn urgent_cards_are_pinned_on_top_of_their_column() {
        let mut b = Board::default();
        b.apply_user(BoardEdit::Add { title: "a".into(), detail: None, col: 1, assignee: Some(3), images: vec![] }, 1).unwrap();
        b.apply_user(BoardEdit::Add { title: "b".into(), detail: None, col: 1, assignee: Some(3), images: vec![] }, 1).unwrap();
        assert_eq!(titles(&b, 1), vec!["a", "b"], "the user's new card goes to the end");
        claude(&mut b, 3, json!({"action":"update","id":2,"urgent":true})).unwrap();
        assert_eq!(titles(&b, 1), vec!["b", "a"]);
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"c"}]})).unwrap();
        assert_eq!(titles(&b, 1), vec!["b", "c", "a"], "a claude's card lands on top, below urgent");
        // The user drags `a` before `c`.
        b.apply_user(BoardEdit::Move { n: 1, col: 1, before: Some(3) }, 2).unwrap();
        assert_eq!(titles(&b, 1), vec!["b", "a", "c"]);
    }

    #[test]
    fn user_changes_tell_the_assignee_only() {
        let mut b = Board::default();
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"QA it"}]})).unwrap();
        b.apply_user(BoardEdit::Add { title: "nobody's".into(), detail: None, col: 1, assignee: None, images: vec![] }, 1).unwrap();
        let (ev, _) = b.apply_user(BoardEdit::Move { n: 2, col: 2, before: None }, 2).unwrap();
        assert!(ev.is_empty(), "an unassigned card tells nobody");
        let (ev, _) = b.apply_user(BoardEdit::Move { n: 1, col: 3, before: None }, 2).unwrap();
        assert_eq!(ev[0].to, 3);
        assert_eq!(ev[0].change, Change::Moved);
        // Reassigning tells both sides.
        let (ev, _) = b.apply_user(BoardEdit::Assign { n: 1, to: Some(5) }, 3).unwrap();
        assert_eq!(ev.iter().map(|e| e.to).collect::<Vec<_>>(), vec![5, 3]);
        // A claude assigning a worker tells the worker, not itself.
        let (_, ev) = claude(&mut b, 3, json!({"action":"add","items":[{"title":"for worker","assignee":"claude#7","detail":"steps here"}]})).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].to, ev[0].by, &ev[0].change), (7, Some(3), &Change::Added));
        let mut ob = Outbox::default();
        ob.push(ev, false, Instant::now());
        assert!(ob.pending(&b).is_empty(), "a claude's change for another claude does not wait for UPDATE");
        let msg = ob.take(7, &b, Path::new("/img")).unwrap();
        assert!(msg.contains("new card for you from claude#3") && msg.contains("steps here"), "{msg}");
        // Handed an existing card: it gets the detail too.
        let (ev, _) = b.apply_user(BoardEdit::Assign { n: 1, to: Some(8) }, 4).unwrap();
        b.cards.iter_mut().find(|c| c.n == 1).unwrap().detail = Some("the QA steps".into());
        ob.push(ev, true, Instant::now());
        assert!(ob.release(8, &b, Instant::now()));
        let msg = ob.take(8, &b, Path::new("/img")).unwrap();
        assert!(msg.contains("assigned to you") && msg.contains("the QA steps"), "{msg}");
    }

    #[test]
    fn the_users_changes_wait_for_update_and_fold() {
        let mut b = Board::default();
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"QA it","choices":["A","B"]}]})).unwrap();
        let t0 = Instant::now();
        let mut ob = Outbox::default();
        let (e1, _) = b.apply_user(BoardEdit::Move { n: 1, col: 2, before: None }, 2).unwrap();
        ob.push(e1, true, t0);
        let (e2, _) = b.apply_user(BoardEdit::Move { n: 1, col: 3, before: None }, 2).unwrap();
        ob.push(e2, true, t0);
        let (e3, _) = b.apply_user(BoardEdit::Comment { n: 1, text: "עובד, תודה".into(), images: vec!["ab-12.png".into()] }, 3).unwrap();
        ob.push(e3, true, t0);
        let (e4, _) = b.apply_user(BoardEdit::Choose { n: 1, choice: Some("B".into()) }, 3).unwrap();
        ob.push(e4, true, t0);
        assert!(ob.due(t0 + Duration::from_secs(600)).is_empty(), "nothing goes before UPDATE, however long");
        assert_eq!(ob.pending(&b), BTreeMap::from([(3, vec![1])]));
        assert!(!ob.release(9, &b, t0), "nothing waiting for claude#9");
        assert!(ob.release(3, &b, t0));
        assert!(ob.pending(&b).is_empty() && ob.sending_ids() == vec![3]);
        assert_eq!(ob.due(t0).len(), 1);
        let msg = ob.take(3, &b, Path::new("/img")).unwrap();
        assert!(msg.contains("now in \"Done\"") && !msg.contains("Doing"), "{msg}");
        assert!(msg.contains("עובד, תודה") && msg.contains("[image: /img/ab-12.png]"), "{msg}");
        assert!(msg.contains("chose: \"B\""), "{msg}");
        assert!(msg.contains("Do NOT hub_send a reply"));
        assert!(ob.is_empty());
        // Archive then restore before UPDATE says nothing.
        let (e, _) = b.apply_user(BoardEdit::Archive { n: 1, on: true }, 4).unwrap();
        ob.push(e, true, t0);
        let (e, _) = b.apply_user(BoardEdit::Archive { n: 1, on: false }, 4).unwrap();
        ob.push(e, true, t0);
        assert!(ob.pending(&b).is_empty(), "archive then restore is no change");
        assert!(!ob.release(3, &b, t0), "nothing real to send");
        assert!(ob.take(3, &b, Path::new("/img")).is_none());
        // Moved there and back, picked then un-picked, reassigned and back: no change.
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"round trips","choices":["A","B"]}]})).unwrap();
        for e in [
            BoardEdit::Move { n: 2, col: 3, before: None },
            BoardEdit::Move { n: 2, col: 1, before: None },
            BoardEdit::Choose { n: 2, choice: Some("A".into()) },
            BoardEdit::Choose { n: 2, choice: None },
            BoardEdit::Assign { n: 2, to: Some(5) },
            BoardEdit::Assign { n: 2, to: Some(3) },
            BoardEdit::Edit { n: 2, title: Some("other".into()), detail: None, images: None },
            BoardEdit::Edit { n: 2, title: Some("round trips".into()), detail: None, images: None },
        ] {
            let (ev, _) = b.apply_user(e, 6).unwrap();
            ob.push(ev, true, t0);
        }
        assert!(ob.pending(&b).is_empty(), "{:?}", ob.pending(&b));
        // …then one real move makes it pending again, reported once, from the start.
        let (ev, _) = b.apply_user(BoardEdit::Move { n: 2, col: 2, before: None }, 7).unwrap();
        ob.push(ev, true, t0);
        assert_eq!(ob.pending(&b), BTreeMap::from([(3, vec![2])]));
        assert!(ob.release(3, &b, t0));
        let msg = ob.take(3, &b, Path::new("/img")).unwrap();
        assert!(msg.contains("now in \"Doing\"") && !msg.contains("chose") && !msg.contains("edited"), "{msg}");
        // A card made and deleted before UPDATE says nothing.
        let (ev, _) = b.apply_user(BoardEdit::Add { title: "oops".into(), detail: None, col: 1, assignee: Some(3), images: vec![] }, 8).unwrap();
        ob.push(ev, true, t0);
        let n = b.next;
        let (ev, _) = b.apply_user(BoardEdit::Delete { n }, 8).unwrap();
        ob.push(ev, true, t0);
        assert!(ob.pending(&b).is_empty());
        ob.forget(|_| false);
        // A closed claude's queue is forgotten.
        let (e, _) = b.apply_user(BoardEdit::Move { n: 1, col: 1, before: None }, 5).unwrap();
        ob.push(e, true, t0);
        assert!(ob.forget(|id| id != 3) && ob.is_empty());
    }

    /// The user's comment can be edited or deleted until UPDATE: the message
    /// carries the edited text, and a deleted one is no change at all.
    #[test]
    fn an_unsent_comment_can_be_edited_or_deleted() {
        let mut b = Board::default();
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"QA it"}]})).unwrap();
        let t0 = Instant::now();
        let mut ob = Outbox::default();
        let (ev, _) = b.apply_user(BoardEdit::Comment { n: 1, text: "typo".into(), images: vec!["aa-1.png".into()] }, 2).unwrap();
        ob.push(ev, true, t0);
        let id = ob.unsent_comments()[0].clone();
        let (ev, gone) = b.apply_user(BoardEdit::CommentEdit { n: 1, id: id.clone(), text: "fixed".into(), images: vec![] }, 3).unwrap();
        assert!(ev.is_empty() && gone == vec!["aa-1.png".to_string()]);
        assert!(ob.release(3, &b, t0));
        let msg = ob.take(3, &b, Path::new("/i")).unwrap();
        assert!(msg.contains("fixed") && !msg.contains("typo") && !msg.contains("aa-1"), "{msg}");
        // Deleted before UPDATE: nothing to send.
        let (ev, _) = b.apply_user(BoardEdit::Comment { n: 1, text: "never mind".into(), images: vec![] }, 4).unwrap();
        ob.push(ev, true, t0);
        let id = ob.unsent_comments()[0].clone();
        b.apply_user(BoardEdit::CommentDelete { n: 1, id }, 5).unwrap();
        assert!(ob.pending(&b).is_empty());
        assert_eq!(b.cards[0].comments.len(), 1);
        // A claude's comment can't be touched this way.
        claude(&mut b, 3, json!({"action":"comment","id":1,"text":"mine"})).unwrap();
        let cid = b.cards[0].comments[1].id.clone();
        assert!(b.apply_user(BoardEdit::CommentDelete { n: 1, id: cid }, 6).is_err());
    }

    /// What was not sent yet survives a relaunch.
    #[test]
    fn the_outbox_is_saved_with_the_board() {
        let home = tempdir();
        let sp = home.join("sessions").join("p.txt");
        let s = BoardStore::beside(&sp, Path::new("/p"), &HashSet::new());
        s.mutate(|b| b.apply_claude(3, &json!({"action":"add","items":[{"title":"x"}]}), 1, &mut no_import)).unwrap();
        let (ev, _) = s.mutate(|b| b.apply_user(BoardEdit::Comment { n: 1, text: "later".into(), images: vec![] }, 2)).unwrap();
        s.outbox(|o, _| o.push(ev, true, Instant::now()));
        let again = BoardStore::beside(&sp, Path::new("/p"), &HashSet::new());
        assert_eq!(again.pending(), BTreeMap::from([(3, vec![1])]));
        let msg = again.outbox(|o, b| {
            o.release(3, b, Instant::now());
            o.take(3, b, Path::new("/i"))
        });
        assert!(msg.unwrap().contains("later"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_claude_view_keeps_context_small() {
        let mut b = Board::default();
        let long = "x".repeat(5000);
        claude(&mut b, 3, json!({"action":"add","items":[
            {"title":"mine","detail": long},
            {"title":"finished","column":"Done","detail":"secret detail"},
            {"title":"theirs","assignee":4}
        ]})).unwrap();
        b.apply_user(BoardEdit::Add { title: "old".into(), detail: None, col: 1, assignee: Some(3), images: vec![] }, 1).unwrap();
        b.apply_user(BoardEdit::Archive { n: 4, on: true }, 1).unwrap();
        let v = b.claude_view(3, false, None, Path::new("/i")).to_string();
        assert!(v.len() < 1500 && v.contains("5000 chars"), "{v}");
        assert!(v.contains("finished") && !v.contains("secret detail"), "last column: title only: {v}");
        assert!(!v.contains("theirs") && !v.contains("\"old\""), "only mine, never archived: {v}");
        let all = b.claude_view(3, true, None, Path::new("/i")).to_string();
        assert!(all.contains("theirs") && all.contains("claude#4") && !all.contains("\"old\"") && !all.contains("xxxx"), "{all}");
        let one = b.claude_view(3, false, Some(1), Path::new("/i")).to_string();
        assert!(one.contains(&long));
    }

    #[test]
    fn columns_are_the_users_and_deleting_one_moves_or_archives_its_cards() {
        let mut b = Board::default();
        b.apply_user(BoardEdit::ColAdd { name: "Waiting for QA".into(), desc: "move here when I need to test".into() }, 1).unwrap();
        b.apply_user(BoardEdit::ColMove { col: 4, before: Some(3) }, 1).unwrap();
        assert_eq!(b.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["To Do", "Doing", "Waiting for QA", "Done"]);
        assert!(b.columns_text().contains("3. \"Waiting for QA\" — move here when I need to test"));
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"a","column":"waiting for qa"}]})).unwrap();
        let (ev, _) = b.apply_user(BoardEdit::ColDelete { col: 4, to: Some(3) }, 2).unwrap();
        assert_eq!(b.card(1).unwrap().col, 3);
        assert_eq!(ev[0].change, Change::Moved);
        b.apply_user(BoardEdit::ColDelete { col: 3, to: None }, 2).unwrap();
        assert!(b.card(1).unwrap().archived && b.col(b.card(1).unwrap().col).is_some());
        b.apply_user(BoardEdit::ColDelete { col: 2, to: None }, 2).unwrap();
        assert!(b.apply_user(BoardEdit::ColDelete { col: 1, to: None }, 2).is_err(), "the last column stays");
    }

    #[test]
    fn a_claudes_column_change_waits_for_the_user() {
        let mut b = Board::default();
        let names = |b: &Board| b.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        // Names resolve as the changes go: the move names the column the add makes.
        let (note, _) = claude(&mut b, 3, json!({"action":"columns","why":"QA piles up","changes":[
            {"op":"add","name":"QA","desc":"I test here"},
            {"op":"move","column":"qa","before":"Done"},
            {"op":"edit","column":"Doing","desc":"now"},
        ]})).unwrap();
        assert!(note.contains("P1"), "{note}");
        assert_eq!(names(&b), ["To Do", "Doing", "Done"], "nothing changes before approval");
        assert_eq!(b.proposals[0].changes[1], ColChange::Move { col: ColRef::New("QA".into()), before: Some(ColRef::Id(3)) });
        // The user renaming a column meanwhile doesn't break it: refs are ids.
        b.apply_user(BoardEdit::ColEdit { col: 3, name: Some("Shipped".into()), desc: None }, 2).unwrap();
        let (by, msg, _) = b.decide(1, true, "", 3).unwrap();
        assert_eq!(by, 3);
        assert_eq!(names(&b), ["To Do", "Doing", "QA", "Shipped"]);
        assert_eq!(b.columns[1].desc, "now");
        assert!(msg.contains("APPROVED") && msg.contains("\"QA\" — I test here"), "{msg}");
        assert!(b.proposals.is_empty());

        // Bad proposals are refused whole, at proposal time.
        for bad in [
            json!({"action":"columns","changes":[]}),
            json!({"action":"columns","changes":[{"op":"delete","column":"nope"}]}),
            json!({"action":"columns","changes":[{"op":"add","name":"qa"}]}),
            json!({"action":"columns","changes":[{"op":"edit","column":"QA"}]}),
            json!({"action":"columns","changes":[{"op":"delete","column":"QA","to":"QA"}]}),
        ] {
            assert!(claude(&mut b, 3, bad.clone()).is_err(), "{bad}");
        }
        assert!(b.proposals.is_empty());

        // Reject: nothing changes, the reason reaches the claude.
        claude(&mut b, 4, json!({"action":"columns","changes":[{"op":"delete","column":"QA"}]})).unwrap();
        let (by, msg, _) = b.decide(2, false, "I like it", 4).unwrap();
        assert_eq!(by, 4);
        assert!(msg.contains("REJECTED") && msg.contains("I like it"));
        assert_eq!(names(&b).len(), 4);

        // A delete moves its cards like the user's own, and tells their claude.
        claude(&mut b, 4, json!({"action":"add","items":[{"title":"a","column":"QA"}]})).unwrap();
        claude(&mut b, 4, json!({"action":"columns","changes":[{"op":"delete","column":"QA","to":"Doing"}]})).unwrap();
        // Approval re-checks the board: a column it names is gone → refused, kept.
        claude(&mut b, 4, json!({"action":"columns","changes":[{"op":"edit","column":"To Do","name":"Later"}]})).unwrap();
        b.apply_user(BoardEdit::ColDelete { col: 1, to: None }, 5).unwrap();
        assert!(b.decide(4, true, "", 6).unwrap_err().contains("gone"));
        assert_eq!(b.proposals.len(), 2);
        let (_, _, ev) = b.decide(3, true, "", 6).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].to, 4);
        assert_eq!(b.cards[0].col, 2);
        assert!(b.decide(3, true, "", 6).is_err(), "decided once");
    }

    #[test]
    fn unread_is_set_by_claudes_and_cleared_by_opening() {
        let mut b = Board::default();
        claude(&mut b, 3, json!({"action":"add","items":[{"title":"a"}]})).unwrap();
        assert_eq!(b.counts(3), Counts { open: 1, unread: 1, urgent: 0 });
        b.apply_user(BoardEdit::Seen { n: 1 }, 2).unwrap();
        assert_eq!(b.counts(3).unread, 0);
        claude(&mut b, 3, json!({"action":"comment","id":1,"text":"done?"})).unwrap();
        assert_eq!(b.unread_total(), 1);
        b.apply_user(BoardEdit::Comment { n: 1, text: "yes".into(), images: vec![] }, 3).unwrap();
        assert_eq!(b.unread_total(), 0, "replying means you read it");
        claude(&mut b, 3, json!({"action":"move","id":1,"column":"Done"})).unwrap();
        assert_eq!(b.counts(3).open, 0, "the last column is not open work");
    }

    #[test]
    fn images_are_checked_by_content_and_stored_by_id() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        let s = BoardStore::beside(&sp, Path::new("/p"), &HashSet::new());
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let id = s.put_image(&png).unwrap();
        assert!(id.ends_with(".png") && s.has_image(&id));
        assert_eq!(s.read_image(&id).unwrap(), png);
        assert!(s.put_image(b"<svg></svg>").is_err());
        assert!(s.put_image(&vec![0xFF; MAX_IMAGE_BYTES + 1]).is_err());
        assert!(s.read_image("../../etc/passwd").is_err());
        assert!(s.read_image("x/../y.png").is_err());
        let f = home.join("shot.png");
        std::fs::write(&f, png).unwrap();
        assert!(s.import_image(f.to_str().unwrap()).is_ok());
        assert!(s.import_image("relative.png").is_err());
        // A claude attaches by path.
        let mut imp = |p: &str| s.import_image(p);
        let mut b = Board::default();
        b.apply_claude(3, &json!({"action":"comment","id":1,"text":"x"}), 1, &mut imp).unwrap_err();
        b.apply_claude(3, &json!({"action":"add","items":[{"title":"t","images":[f.to_str().unwrap()]}]}), 1, &mut imp).unwrap();
        assert_eq!(b.cards[0].images.len(), 1);
        let v = b.claude_view(3, false, Some(1), s.img_dir()).to_string();
        assert!(v.contains(&s.img_dir().display().to_string()), "{v}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A big pasted image arrives in chunks and reads back in chunks, byte for byte.
    #[test]
    fn images_go_in_chunks_both_ways() {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let home = tempdir();
        let s = BoardStore::beside(&home.join("sessions").join("p.txt"), Path::new("/p"), &HashSet::new());
        let mut img = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        img.extend((0..7_000_000u32).map(|i| (i % 251) as u8));
        let parts: Vec<&[u8]> = img.chunks(2 * 1024 * 1024).collect();
        let mut upload: Option<String> = None;
        let mut id = None;
        for (i, part) in parts.iter().enumerate() {
            let r = s.put_chunk(upload.as_deref(), &b64(part), i == parts.len() - 1).unwrap();
            upload = r["upload"].as_str().map(String::from);
            id = r["id"].as_str().map(String::from);
        }
        let id = id.expect("the last chunk returns the id");
        let mut back = Vec::new();
        loop {
            let r = s.get_chunk(&id, back.len()).unwrap();
            let part = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap()).unwrap();
            assert!(part.len() <= 3 * 1024 * 1024);
            back.extend(part);
            if back.len() >= r["total"].as_u64().unwrap() as usize {
                break;
            }
        }
        assert_eq!(back, img);
        // Over 10 MB in all is refused, and an unknown upload says so.
        let big = vec![0u8; 6 * 1024 * 1024];
        let r = s.put_chunk(None, &b64(&big), false).unwrap();
        let t = r["upload"].as_str().unwrap().to_string();
        assert!(s.put_chunk(Some(&t), &b64(&big), true).is_err());
        assert!(s.put_chunk(Some("nope"), &b64(b"x"), true).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn store_round_trips_and_ignores_other_projects() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        let s = BoardStore::beside(&sp, Path::new("/p"), &HashSet::new());
        s.mutate(|b| b.apply_claude(3, &json!({"action":"add","items":[{"title":"x"}]}), 1, &mut no_import)).unwrap();
        assert!(s.take_dirty());
        assert!(s.mutate(|b| b.apply_claude(3, &json!({"action":"bogus"}), 1, &mut no_import)).is_err());
        assert!(!s.take_dirty(), "a failed op dirties nothing");
        let again = BoardStore::beside(&sp, Path::new("/p"), &HashSet::new());
        assert_eq!(again.get().cards.len(), 1);
        assert!(BoardStore::beside(&sp, Path::new("/other"), &HashSet::new()).get().cards.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn old_todo_lists_become_cards() {
        let home = tempdir();
        let sp = home.join("sessions").join("proj-abc.txt");
        std::fs::create_dir_all(home.join("todos")).unwrap();
        std::fs::write(
            home.join("todos/proj-abc.json"),
            json!({"dir": "/p", "lists": {
                "3": {"next": 3, "items": [
                    {"n":1,"kind":"decision","title":"A or B","choices":["A","B"],"status":"open","author":"claude",
                     "comments":[{"by":"user","text":"hmm","at":5},{"by":"claude","text":"well?","at":6}],"seen":1,
                     "created":1,"updated":6,"urgent":true},
                    {"n":2,"kind":"action","title":"gone","status":"done","author":"claude","created":1,"updated":1},
                    {"n":3,"kind":"question","title":"token?","status":"open","author":"claude","for":7,"created":1,"updated":1}
                ]},
                "4": {"items": [{"n":1,"kind":"action","title":"deploy","status":"open","author":"user","created":1,"updated":1}]}
            }})
            .to_string(),
        )
        .unwrap();
        let s = BoardStore::beside(&sp, Path::new("/p"), &HashSet::from([3]));
        let b = s.get();
        assert!(b.on, "a row had its list on");
        assert_eq!(b.cards.len(), 3, "done items are not carried over");
        let ab = b.cards.iter().find(|c| c.title == "A or B").unwrap();
        assert!(ab.urgent && ab.unread && ab.assignee == Some(3) && ab.comments.len() == 2);
        assert_eq!(ab.comments[1].who, Some(3));
        assert!(b.cards.iter().any(|c| c.title == "token? (for claude#7)"));
        assert_eq!(b.cards.iter().find(|c| c.title == "deploy").unwrap().author, Author::User);
        assert!(!home.join("todos/proj-abc.json").exists() && home.join("todos/proj-abc.json.migrated").exists());
        // Loaded from the board next time, not migrated again.
        assert_eq!(BoardStore::beside(&sp, Path::new("/p"), &HashSet::new()).get().cards.len(), 3);
        let _ = std::fs::remove_dir_all(&home);
    }
}
