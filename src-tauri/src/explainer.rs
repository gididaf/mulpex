//! The Explainer: after every turn, a cheap Sonnet call produces a very short,
//! very simple **Hebrew** explanation of what the claude just said, for the
//! right-hand Explainer panel.
//!
//! The trigger is a file the hooks write: `Stop` (a finished turn), `askq` and
//! `plan` (a dialog the claude stopped to show) each drop the session's
//! transcript path into `explainreq/<id>` — the last two with a `dialog`
//! marker — and the 200 ms poll loop drains it into `submit`. Everything after
//! that happens on this module's own worker threads: read the transcript,
//! extract the current turn (prose, a pending question or a pending plan —
//! decided from the transcript itself), run `claude -p --model sonnet`
//! headless, store the result and emit `explain-update`.
//!
//! **A short feed per instance**, `MAX_ENTRIES` deep, oldest dropped
//! completely; the frontend mirrors the cap. `get_explains` hands the whole
//! feed to a fresh webview.
//!
//! The summarizer child is a plain process, not a PTY session, and must stay a
//! nobody: `env_clear()` + `claude_bin::forwarded_env()` (whose deny-list
//! strips `MULPEX_*` / `CLAUDE_CODE_CHILD_SESSION` / `CLAUDE_CODE_ENTRYPOINT`)
//! keeps it off the hub, and `--setting-sources "" --tools "" --strict-mcp-config
//! --no-session-persistence` keeps it cheap, quiet and diskless. NOT `--bare`:
//! measured 2026-08-30, `--bare -p` cannot see the subscription OAuth token and
//! dies with "Not logged in".

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::claude_bin;
use crate::snapshot::{
    ExplainEntry, ExplainKind, ExplainPending, ExplainSections, ExplainUpdate, ProjectHandle,
};

/// The system prompt of the summarizer. Hebrew because the panel is for reading
/// at a glance; English identifiers stay as-is because translating them is how
/// you lose track of what the claude actually touched. Tone/length approved on
/// real turns (probe0, 2026-08-30).
///
/// **First person, always.** The panel is Claude talking to the user about his
/// own turn — "בדקתי… ועכשיו אני צריך ממך" — not a narrator describing "הוא".
/// A third-person summarizer also drifts into calling the *user* the one who
/// did the work ("אתה ממתין לשני agents"), which is exactly backwards; pinning
/// אני to the claude and אתה to the human kills both problems at once.
/// Three questions, three lines, in this order — see [`SECTION_KEYS`]. The
/// **keys are ASCII** and the answers are Hebrew: a Hebrew key would have to
/// survive the model's own RTL handling before the parser ever saw it, and
/// there is nothing to gain from betting on that. The panel draws the Hebrew
/// headings itself, so nothing in this output is ever shown as a label.
const HEBREW_PROMPT: &str = "אתה \"המסביר\" של Mulpex. תקבל את ההודעות האחרונות של המשתמש, ואחריהן את הטקסט ש-Claude Code כתב בתור העבודה הנוכחי.\n\
ענה בעברית פשוטה מאוד, בגוף ראשון יחיד — כאילו Claude עצמו מדבר אל המשתמש — בדיוק שלוש שורות, בפורמט הזה ובסדר הזה:\n\
WORK: <על מה אנחנו עובדים>\n\
DID: <מה עשיתי בסבב הזה>\n\
NEED: <מה אני צריך מהמשתמש עכשיו>\n\
כללים:\n\
- בדיוק שלוש שורות. כל שורה מתחילה במילת המפתח באנגלית ואז נקודתיים. אין שום טקסט לפני השורה הראשונה או אחרי השלישית.\n\
- כל שורה היא משפט אחד קצר של עד 15 מילים, כמו שמסבירים לחבר שלא מבין בתכנות. קצר ופשוט לפני הכול.\n\
- אם עשיתי או מצאתי כמה דברים — תגיד רק את הדבר החשוב ביותר, וּותר על כל השאר. עדיף שורה חסרה מדי מאשר שורה עמוסה.\n\
- בלי נקודה-פסיק, בלי סוגריים, ובלי לחבר כמה דברים ב\"וגם\" או בפסיקים. משפט אחד, רעיון אחד.\n\
- WORK: המטרה הגדולה שאנחנו עובדים עליה עכשיו, לפי ההודעות של המשתמש — לא מה שעשיתי הרגע.\n\
- DID: מה שינסתי, בדקתי או תיקנתי בסבב הזה. רק העיקר, בלי רשימה ובלי לפרט שלבים.\n\
- NEED: אם אני מחכה להחלטה, לתשובה או לבדיקה — תגיד בדיוק מה. אם אני לא צריך כלום — כתוב בדיוק: כלום, אפשר להמשיך.\n\
- גוף ראשון תמיד: בדקתי, תיקנתי, אני צריך ממך. אף פעם לא \"הוא\", ואף פעם לא Claude בגוף שלישי.\n\
- אתה, אותך, ממך, שלך — מתייחסים אך ורק למשתמש האנושי. לעולם לא לעבודה שאני עצמי עשיתי.\n\
- שמות קבצים, פקודות, מונחים ושמות של functions/branches נשארים באנגלית בדיוק כמו שהם — בלי לתרגם ובלי לתעתק. סביב זה, מילים פשוטות.\n\
- בלי להיכנס לפרטים טכניים ובלי להסביר איך משהו עובד מבפנים. מה, לא איך.\n\
- טקסט פשוט בלבד — בלי סימוני markdown: בלי **, בלי #, בלי `backticks`, בלי מקפים בתחילת שורה.\n\
- אם נכשלתי או נתקעתי — אמור את זה ישירות ב-DID.\n\
- אל תוסיף שום דבר שלא מופיע בטקסט שקיבלת.";

/// The three line keys, in order. `parse_sections` requires **all three** — a
/// partial parse renders as raw text rather than as a heading with nothing
/// under it, because an empty part the user cannot tell from a missing one is
/// exactly the "default that reads as an assertion" this codebase keeps paying
/// for.
const SECTION_KEYS: [&str; 3] = ["WORK", "DID", "NEED"];

/// The question-mode prompt: Claude stopped mid-turn on `AskUserQuestion`.
///
/// Same three parts as a turn — the question is simply what goes in `NEED`, so
/// the panel never changes shape — with the options as their own lines under
/// it. `DID` is the part this exists to protect: the panel used to explain the
/// question and **throw the turn away**, so it read as a claude who had done no
/// work and immediately started asking. The rule is stated twice here (do it,
/// and what to write if there genuinely is nothing) because that is the failure
/// the user reported.
const QUESTION_PROMPT: &str = "אתה \"המסביר\" של Mulpex. תקבל את ההודעות האחרונות של המשתמש, את מה ש-Claude Code כתב בתור הנוכחי, ואחריהם את השאלה שהוא עצר לשאול לפני שהוא ממשיך.\n\
ענה בעברית פשוטה מאוד, בגוף ראשון יחיד — כאילו Claude עצמו מדבר אל המשתמש — בפורמט הזה ובסדר הזה:\n\
WORK: <על מה אנחנו עובדים>\n\
DID: <מה עשיתי בסבב הזה, לפני שעצרתי לשאול>\n\
NEED: <מה אני צריך שתחליט, ואחריו כל אפשרות בשורה משלה>\n\
כללים:\n\
- שלוש מילות המפתח, בסדר הזה. אין שום טקסט לפני WORK ואין כלום אחרי האפשרות האחרונה.\n\
- WORK ו-DID: משפט אחד קצר כל אחד, עד 15 מילים. בלי נקודה-פסיק ובלי לחבר כמה דברים ב\"וגם\".\n\
- DID הוא הדבר החשוב: מה בדקתי, מצאתי או עשיתי בסבב הזה לפני שעצרתי לשאול. אל תדלג עליו ואל תכתוב בו את השאלה.\n\
- אם באמת לא עשיתי כלום בסבב הזה חוץ מלשאול — כתוב ב-DID בדיוק: עוד לא עשיתי כלום בסבב הזה.\n\
- NEED: שורה ראשונה קצרה — מה אני צריך שתחליט. אחריה שורה לכל אפשרות שמתחילה ב\"- \", עד 12 מילים: שם האפשרות, מקף, ובמילים פשוטות מה היא אומרת.\n\
- אם יש כמה שאלות — כל שאלה בשורה משלה בתוך NEED, והאפשרויות שלה בשורות שמתחתיה. תשמור על כולן קצרות מאוד.\n\
- אם כתוב באפשרות (Recommended) — סמן אותה בסוף השורה שלה במילה (מומלץ), בעברית. אל תשאיר (Recommended) באנגלית.\n\
- אם לא כתוב (Recommended) באף אפשרות — אל תסמן שום אפשרות, ואל תרמוז מה עדיף.\n\
- גוף ראשון תמיד: בדקתי, מצאתי, אני צריך ממך. אף פעם לא \"הוא\", ואף פעם לא Claude בגוף שלישי.\n\
- אתה, אותך, ממך, שלך — מתייחסים אך ורק למשתמש האנושי. לעולם לא לעבודה שאני עצמי עשיתי.\n\
- שמות קבצים, פקודות, מונחים ושמות של functions/branches נשארים באנגלית בדיוק כמו שהם — בלי לתרגם ובלי לתעתק.\n\
- בלי להיכנס לפרטים טכניים. מה, לא איך.\n\
- טקסט פשוט בלבד — בלי סימוני markdown: בלי **, בלי #, בלי `backticks`. המקפים מותרים רק בתחילת שורת אפשרות.\n\
- אל תוסיף שום דבר שלא מופיע בטקסט שקיבלת.";

/// The plan-mode prompt: Claude finished planning and the "ready to code?"
/// dialog is on screen.
///
/// Same three parts as a turn and a question. `NEED` is the approval plus the
/// plan's main steps, one row each — the layout a question's options already
/// use, so the panel reads the same way whatever it is showing. It is still
/// **not** a copy of the plan: a plan is long, structured and technical by
/// construction (headings, absolute paths, code fences) and reproducing any of
/// that here would defeat the panel. At most four rows, the big moves only.
const PLAN_PROMPT: &str = "אתה \"המסביר\" של Mulpex. תקבל את ההודעות האחרונות של המשתמש, את מה ש-Claude Code כתב בתור הנוכחי, ואחריהם את התוכנית שהוא סיים לכתוב ומחכה לאישור שלך עליה.\n\
ענה בעברית פשוטה מאוד, בגוף ראשון יחיד — כאילו Claude עצמו מדבר אל המשתמש — בפורמט הזה ובסדר הזה:\n\
WORK: <על מה אנחנו עובדים>\n\
DID: <מה בדקתי ומצאתי בסבב הזה, לפני שכתבתי את התוכנית>\n\
NEED: <שאני צריך אישור להתחיל, ואחריו השלבים העיקריים כל אחד בשורה משלו>\n\
כללים:\n\
- שלוש מילות המפתח, בסדר הזה. אין שום טקסט לפני WORK ואין כלום אחרי השורה האחרונה.\n\
- WORK ו-DID: משפט אחד קצר כל אחד, עד 15 מילים. בלי נקודה-פסיק ובלי לחבר כמה דברים ב\"וגם\".\n\
- DID: מה קראתי, בדקתי או מצאתי בסבב הזה לפני שתכננתי. אל תכתוב בו את התוכנית עצמה.\n\
- אם באמת רק תכננתי ולא נגעתי בכלום — כתוב ב-DID בדיוק: רק תיכננתי, עוד לא נגעתי בכלום.\n\
- NEED: שורה ראשונה קצרה שאומרת שאני מחכה לאישור שלך כדי להתחיל. אחריה עד ארבע שורות שמתחילות ב\"- \", כל אחת עד 12 מילים: מה אני מתכוון לעשות, בגדול.\n\
- אם בתוכנית יש יותר מארבעה שלבים — אחד את הקטנים ותן רק את המהלכים הגדולים. אל תעתיק את התוכנית ואל תפרט שלבים קטנים.\n\
- בשורות האלה: בלי שמות קבצים, בלי פקודות ובלי פרטים טכניים — רק מה משתנה, במילים פשוטות. מונח באנגלית רק אם באמת אי אפשר בלעדיו.\n\
- גוף ראשון תמיד: בדקתי, מצאתי, אני מתכוון. אף פעם לא \"הוא\", ואף פעם לא Claude בגוף שלישי.\n\
- אתה, אותך, ממך, שלך — מתייחסים אך ורק למשתמש האנושי. לעולם לא לעבודה שאני עצמי עשיתי.\n\
- טקסט פשוט בלבד — בלי סימוני markdown: בלי **, בלי #, בלי `backticks`. המקפים מותרים רק בתחילת שורת שלב.\n\
- אל תוסיף שום דבר שלא כתוב בתוכנית, ואל תחווה דעה אם היא טובה.";

/// Feed cap per instance: a short history, not an archive — the transcript
/// beside the panel is the archive. The oldest entry is dropped **completely**
/// when the cap is passed (`push_entry` truncates the row and prunes its retry
/// stash with it), and the frontend mirrors the same cap
/// (`stores.ts::MAX_EXPLAIN_ENTRIES`), so memory per instance is bounded on
/// both sides.
const MAX_ENTRIES: usize = 10;

/// Cap on the turn text handed to Sonnet, keeping the **tail** — the end of a
/// turn is where conclusions live. Byte-based, trimmed to a char boundary.
const MAX_TURN_BYTES: usize = 24_000;

/// How much of the user's own recent prompts rides along with the turn, and how
/// many of them. This is what makes the `WORK:` line an answer rather than a
/// guess: the goal lives in what the user asked for, and a claude mid-task
/// rarely restates it.
///
/// Deliberately a *window*, not the session. Measured 2026-09-16 over the real
/// transcripts on this machine: every prompt of a long session runs to ~50k
/// tokens and the whole conversation to ~137k — for a first line that gets no
/// better, on a keypress the user is waiting on, and dragging in prompts from
/// before a `/clear` that are about something else entirely.
const MAX_PROMPTS_BYTES: usize = 4_000;
const RECENT_PROMPTS: usize = 6;

/// Cap on the plan handed to Sonnet, keeping the **head** — the opposite end
/// from a turn. A plan opens with its goal and descends into steps and file
/// lists, and the goal is the only part the one-line summary needs.
const MAX_PLAN_BYTES: usize = 12_000;

/// Summarizer wall-clock budget. Measured runs take 6–13 s; anything past this
/// is a hang, and the turn gets a failure entry rather than a stuck queue.
const SUMMARIZER_TIMEOUT: Duration = Duration::from_secs(90);

/// Two turns routinely end together (test the plural); more workers than this
/// just races more Sonnet calls for a panel the user reads one row at a time.
const WORKERS: usize = 2;

/// Every summarizer call gets one automatic second attempt after this pause,
/// and only a second failure reaches the panel. Measured 2026-09-03: `claude -p`
/// exits 1 on a transient — a 401/429 from the API, a CLI self-update swapping
/// the install out from under the child — as readily as on anything permanent,
/// and the user's only recourse for those was a failure row they couldn't act
/// on. Cheap: it costs an extra call only on a call that already failed.
const AUTO_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Claude Code flushes a turn's final assistant entry to the JSONL a beat after
/// the turn visibly ends (measured 2026-08-30: the entry landed at 11:55:15.346,
/// within the same second as the turn's end), so a request written by the
/// `Stop` hook can be drained while the transcript's last word hasn't arrived.
/// The read is therefore tried **immediately** and retried while it comes back
/// empty — or, for a request an `askq`/`plan` hook wrote, while the transcript
/// still shows no pending dialog (the same race, one entry later). Still empty
/// after ~1s is a turn with genuinely nothing said yet, and says so.
const EXTRACT_DELAY: Duration = Duration::from_millis(250);
const EXTRACT_RETRIES: usize = 4;

/// What the panel says when a turn ended with nothing said in it — an
/// interrupted turn, or one whose final entry never flushed. Not a failure and
/// not a summary: no Sonnet call is made, because a call on nothing invents
/// something. An entry rather than a silent skip, because a silent skip is
/// exactly what this feature's most expensive bug looked like from the outside.
const NOTHING_YET: &str = "עדיין אין מה להסביר";

/// What a job explains.
enum Input {
    /// One request the hooks wrote: read this instance's transcript and explain
    /// the current turn. Whether that turn is prose, a pending `AskUserQuestion`
    /// or a pending `ExitPlanMode` plan is decided **from the transcript**
    /// (`read_turn`) — all three live in the same file, so the request has one
    /// input, not three. `expect_dialog` is the `askq`/`plan` marker: the
    /// reader waits for the dialog entry rather than settling for the prose
    /// before it (see `read_turn_settled`).
    Now { transcript: PathBuf, expect_dialog: bool },
    /// A manual retry of the failed entry `seq` (the panel's "נסה שוב").
    ///
    /// It carries the *already-extracted* summarizer input, stashed when the
    /// entry failed — it does not go back to the transcript. By the time the
    /// user clicks, that claude may have finished two more turns, and
    /// `read_turn` reads the LAST one: re-extracting would silently explain a
    /// different turn under the failed row.
    Retry { seq: u64, kind: ExplainKind, prompt: &'static str, text: String },
}

struct Job {
    handle: ProjectHandle,
    id: usize,
    input: Input,
    /// The project's scratch dir — a neutral cwd for the summarizer child.
    cwd: PathBuf,
}

struct Inner {
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    /// `(handle, id)` → that instance's feed, newest first.
    store: Mutex<HashMap<(ProjectHandle, usize), Vec<ExplainEntry>>>,
    /// `(handle, id)` → jobs queued or running for it — the panel's busy dot.
    /// A count, not a bool: a running job plus a freshly queued one must not go
    /// idle when only the first finishes. `explain-pending` fires on the 0↔1+
    /// transitions only.
    pending: Mutex<HashMap<(ProjectHandle, usize), u32>>,
    /// `seq` of a **failed** entry → everything needed to run its summarizer
    /// call again. Written when a failure entry is pushed, dropped when the
    /// retry succeeds, when the entry falls off the end of its feed, and when
    /// the instance or project is forgotten — so it can only ever hold inputs
    /// for failure rows currently on screen.
    retries: Mutex<HashMap<u64, Retryable>>,
}

/// The stashed input of one failed entry: exactly the arguments
/// [`run_summarizer`] took, plus who it belongs to.
struct Retryable {
    handle: ProjectHandle,
    id: usize,
    kind: ExplainKind,
    prompt: &'static str,
    text: String,
    cwd: PathBuf,
}

static INNER: OnceLock<Arc<Inner>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();

fn inner() -> &'static Arc<Inner> {
    INNER.get_or_init(|| {
        Arc::new(Inner {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            store: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            retries: Mutex::new(HashMap::new()),
        })
    })
}

/// Start the worker threads. Called once from `hub::start`; the handle is what
/// lets a worker emit `explain-update` the moment its summary lands.
pub fn init(app: AppHandle) {
    if APP.set(app).is_err() {
        return; // already running
    }
    for _ in 0..WORKERS {
        let inner = inner().clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut q = inner.queue.lock().unwrap();
                loop {
                    if let Some(job) = q.pop_front() {
                        break job;
                    }
                    q = inner.wake.wait(q).unwrap();
                }
            };
            let (handle, id) = (job.handle, job.id);
            process(&inner, job);
            // Every path through process — entry, failure entry, skip — ends
            // the job; the busy dot must never outlive it.
            finish_pending(&inner, handle, id);
        });
    }
}

/// Queue one request the hooks wrote (`explainreq/<id>`, drained by the poll
/// loop). The body is the file as written: line 1 the transcript path, an
/// optional line 2 `dialog` when `askq`/`plan` wrote it. Latest-wins per
/// instance for jobs not yet running: if a queued job for the same
/// `(handle, id)` exists it is replaced — its transcript is the same file, and
/// the newer request supersedes it exactly the way `explainreq/<id>` overwrites
/// between polls. A job already *running* is not touched; the new one queues
/// behind it.
pub fn submit(handle: ProjectHandle, id: usize, body: String, cwd: PathBuf) {
    let mut lines = body.lines();
    let Some(path) = lines.next().map(str::trim).filter(|p| !p.is_empty()) else {
        return;
    };
    let expect_dialog = lines.any(|l| l.trim() == "dialog");
    enqueue(handle, id, Input::Now { transcript: PathBuf::from(path), expect_dialog }, cwd);
}

fn enqueue(handle: ProjectHandle, id: usize, input: Input, cwd: PathBuf) {
    let kind = std::mem::discriminant(&input);
    // Latest-wins is per *kind*, and a Retry has no "latest": each one names a
    // different failed row, so two of them must both run. Coalescing them would
    // leave one failure row spinning on its busy dot forever.
    let coalesce = !matches!(input, Input::Retry { .. });
    let job = Job { handle, id, input, cwd };
    let inner = inner();
    let mut q = inner.queue.lock().unwrap();
    let replaced = if let Some(existing) = q.iter_mut().find(|j| {
        coalesce && j.handle == handle && j.id == id && std::mem::discriminant(&j.input) == kind
    }) {
        *existing = job;
        true
    } else {
        q.push_back(job);
        false
    };
    drop(q);
    inner.wake.notify_one();
    // A replaced job was already counted; only a genuinely new one raises the
    // pending count (and lights the busy dot on the 0→1 edge).
    if !replaced {
        let mut p = inner.pending.lock().unwrap();
        let c = p.entry((handle, id)).or_insert(0);
        *c += 1;
        let lit = *c == 1;
        drop(p);
        if lit {
            emit_pending(handle, id, true);
        }
    }
}

/// One job ended (summary, failure, or skip): drop its pending count, and dark
/// the busy dot on the 1→0 edge. An entry already cleared by `forget` stays
/// cleared — no negative counts, no spurious re-emit.
fn finish_pending(inner: &Inner, handle: ProjectHandle, id: usize) {
    let mut p = inner.pending.lock().unwrap();
    let mut dark = false;
    if let Some(c) = p.get_mut(&(handle, id)) {
        *c = c.saturating_sub(1);
        if *c == 0 {
            p.remove(&(handle, id));
            dark = true;
        }
    }
    drop(p);
    if dark {
        emit_pending(handle, id, false);
    }
}

fn emit_pending(handle: ProjectHandle, id: usize, active: bool) {
    if let Some(app) = APP.get() {
        let _ = app.emit("explain-pending", ExplainPending { handle, id, active });
    }
}

/// Drop one instance's feed (its row is gone from the sidebar, so the feed is
/// unreachable) and any queued job for it. A job already mid-run may still
/// finish and deposit one zombie entry; it is invisible (the frontend keys the
/// panel by live rows) and freed with the project.
pub fn forget(handle: ProjectHandle, id: usize) {
    let inner = inner();
    inner.queue.lock().unwrap().retain(|j| !(j.handle == handle && j.id == id));
    inner.store.lock().unwrap().remove(&(handle, id));
    inner.retries.lock().unwrap().retain(|_, r| !(r.handle == handle && r.id == id));
    // The row is gone; so is its busy dot. A still-running job's own
    // finish_pending later finds no entry and stays silent.
    if inner.pending.lock().unwrap().remove(&(handle, id)).is_some() {
        emit_pending(handle, id, false);
    }
}

/// Drop everything a closed project accumulated.
pub fn forget_project(handle: ProjectHandle) {
    let inner = inner();
    inner.queue.lock().unwrap().retain(|j| j.handle != handle);
    inner.store.lock().unwrap().retain(|(h, _), _| *h != handle);
    inner.pending.lock().unwrap().retain(|(h, _), _| *h != handle);
    inner.retries.lock().unwrap().retain(|_, r| r.handle != handle);
}

/// The whole feed of one project, for the frontend's initial paint (bootstrap /
/// dev hot-reload). Per instance newest-first; instances in arbitrary order —
/// the frontend groups by `entry.id` anyway.
pub fn feed(handle: ProjectHandle) -> Vec<ExplainEntry> {
    let store = inner().store.lock().unwrap();
    let mut out = Vec::new();
    for ((h, _), entries) in store.iter() {
        if *h == handle {
            out.extend(entries.iter().cloned());
        }
    }
    out
}

/// Process-wide entry ids. Monotonic and never reused, so a `seq` the frontend
/// is holding can only ever mean the entry it came from — the retry address has
/// to survive a feed that reorders and truncates under it.
fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One job, end to end. A turn with nothing said in it yet gets the
/// [`NOTHING_YET`] entry rather than a Sonnet call on nothing — and rather than
/// a silent skip, which is what this feature's most expensive bug looked like
/// from the outside (the panel simply never filled in).
fn process(inner: &Inner, job: Job) {
    // `replace` is the seq of the failed row this job is redoing, if any: a
    // retry lands *in place of* its failure rather than as a second row.
    let (kind, prompt, text, replace) = match &job.input {
        Input::Now { transcript, expect_dialog } => match read_turn_settled(transcript, *expect_dialog) {
            Some(turn) => (turn.kind, turn.prompt, turn.text, None),
            None => {
                eprintln!(
                    "[explainer] project {} claude#{}: nothing said in {}",
                    job.handle,
                    job.id,
                    transcript.display()
                );
                push_and_emit(
                    inner,
                    job.handle,
                    job.id,
                    ExplainEntry {
                        id: job.id,
                        ts: now_ms(),
                        text: NOTHING_YET.to_string(),
                        sections: None,
                        ok: true,
                        kind: ExplainKind::Turn,
                        seq: next_seq(),
                    },
                    false,
                );
                return;
            }
        },
        Input::Retry { seq, kind, prompt, text } => (*kind, *prompt, text.clone(), Some(*seq)),
    };
    let seq = replace.unwrap_or_else(next_seq);
    let entry = match run_summarizer_twice(prompt, &text, &job.cwd) {
        Ok(summary) => {
            // It worked: the stashed input has no reader left.
            inner.retries.lock().unwrap().remove(&seq);
            // All three kinds share the shape now: a question is a turn whose
            // `NEED` carries the options, a plan one whose `NEED` carries the
            // steps. The panel therefore reads the same way whatever it shows.
            let sections = parse_sections(&summary);
            if sections.is_none() {
                eprintln!(
                    "[explainer] project {} claude#{}: three-part parse failed; showing raw output",
                    job.handle, job.id
                );
            }
            ExplainEntry { id: job.id, ts: now_ms(), text: summary, sections, ok: true, kind, seq }
        }
        Err(why) => {
            // Stash (or refresh) what the panel's retry button will re-run.
            let stash = Retryable {
                handle: job.handle,
                id: job.id,
                kind,
                prompt,
                text: text.clone(),
                cwd: job.cwd.clone(),
            };
            inner.retries.lock().unwrap().insert(seq, stash);
            ExplainEntry {
                id: job.id,
                ts: now_ms(),
                text: format!("ההסבר נכשל ({why})"),
                sections: None,
                ok: false,
                kind,
                seq,
            }
        }
    };
    // Dev-visible trace (stderr): the one place a summary — or its failure — can
    // be seen before the panel exists, and after it exists the place to check
    // when the panel shows nothing.
    eprintln!("[explainer] project {} claude#{}: {}", job.handle, job.id, entry.text);
    push_and_emit(inner, job.handle, job.id, entry, replace.is_some());
}

/// Store one finished entry and push it to the panel. `replace` puts a retry's
/// result back in its failed row's slot; otherwise the entry goes to the front
/// of this instance's feed, and the oldest falls off the end.
fn push_and_emit(
    inner: &Inner,
    handle: ProjectHandle,
    id: usize,
    entry: ExplainEntry,
    replace: bool,
) {
    if replace {
        replace_entry(inner, handle, id, entry.clone());
    } else {
        push_entry(inner, handle, id, entry.clone());
    }
    if let Some(app) = APP.get() {
        let _ = app.emit("explain-update", ExplainUpdate { handle, id, entry });
    }
}

/// Re-run the failed entry `seq`: the panel's "נסה שוב". Returns false if there
/// is nothing stashed under it (the entry aged out of its feed, or its instance
/// is gone) — the caller is a click, and a click that finds nothing must not
/// fabricate a job.
///
/// The stash is *kept*, not taken: if this attempt fails too, `process` writes
/// it straight back under the same seq and the button lives on.
pub fn retry(handle: ProjectHandle, id: usize, seq: u64) -> bool {
    let inner = inner();
    let (job, cwd) = {
        let retries = inner.retries.lock().unwrap();
        match retries.get(&seq) {
            // The (handle, id) check is not paranoia: seq comes from the
            // frontend, and a stale panel could address another row's entry.
            Some(r) if r.handle == handle && r.id == id => (
                Input::Retry { seq, kind: r.kind, prompt: r.prompt, text: r.text.clone() },
                r.cwd.clone(),
            ),
            _ => return false,
        }
    };
    enqueue(handle, id, job, cwd);
    true
}

/// Append to one instance's feed: newest first, capped. Entries pushed off the
/// end take their stashed retry input with them — the row is unreachable, so
/// nothing can ever ask for it again.
fn push_entry(inner: &Inner, handle: ProjectHandle, id: usize, entry: ExplainEntry) {
    let mut store = inner.store.lock().unwrap();
    let feed = store.entry((handle, id)).or_default();
    feed.insert(0, entry);
    let dropped: Vec<u64> = feed.iter().skip(MAX_ENTRIES).map(|e| e.seq).collect();
    feed.truncate(MAX_ENTRIES);
    drop(store);
    let mut retries = inner.retries.lock().unwrap();
    for seq in dropped {
        retries.remove(&seq);
    }
}

/// Overwrite the entry carrying `entry.seq` in place — a retry's result takes
/// the failed row's position in the feed, not a new one at the top. If the row
/// is gone (its instance was forgotten mid-run) the result is dropped: the
/// alternative, prepending it, would resurrect a feed the user closed.
fn replace_entry(inner: &Inner, handle: ProjectHandle, id: usize, entry: ExplainEntry) {
    let mut store = inner.store.lock().unwrap();
    if let Some(feed) = store.get_mut(&(handle, id)) {
        if let Some(slot) = feed.iter_mut().find(|e| e.seq == entry.seq) {
            *slot = entry;
        }
    }
}

/// [`run_summarizer`] with one automatic second attempt (see
/// [`AUTO_RETRY_DELAY`]). Only the second failure reaches the panel; the first
/// is logged, because "it failed twice for the same reason" and "it failed for
/// two different reasons" are different bugs and the entry shows only one.
fn run_summarizer_twice(prompt: &str, turn: &str, cwd: &Path) -> Result<String, String> {
    match run_summarizer(prompt, turn, cwd) {
        Ok(text) => Ok(text),
        Err(first) => {
            eprintln!("[explainer] attempt 1 failed ({first}); retrying once");
            std::thread::sleep(AUTO_RETRY_DELAY);
            run_summarizer(prompt, turn, cwd)
        }
    }
}

/// Run the headless Sonnet call, input text on stdin, `prompt` choosing the
/// turn or question persona. See the module docs for why each flag is there and
/// why `--bare` is not.
fn run_summarizer(prompt: &str, turn: &str, cwd: &Path) -> Result<String, String> {
    let claude = claude_bin::resolve_claude().ok_or("claude not found")?;
    let mut child = Command::new(claude)
        .args([
            "-p",
            "--setting-sources",
            "",
            "--model",
            "sonnet",
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--system-prompt",
            prompt,
        ])
        .env_clear()
        .envs(claude_bin::forwarded_env())
        .env("PATH", claude_bin::merged_path())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    // Feed stdin and close it, or `-p` waits for EOF forever.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(turn.as_bytes());
    }
    // Drain both pipes on their own threads: Sonnet's output fits a pipe buffer
    // today, but a child blocked on a full pipe while we only `try_wait` would
    // read as a "timeout" — the classic self-inflicted deadlock.
    let stdout = child.stdout.take();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut f) = stdout {
            use std::io::Read as _;
            let _ = f.read_to_string(&mut s);
        }
        s
    });
    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut f) = stderr {
            use std::io::Read as _;
            let _ = f.read_to_string(&mut s);
        }
        s
    });

    let deadline = std::time::Instant::now() + SUMMARIZER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait(); // reap; a zombie would pin the pid
                return Err(format!("timeout after {}s", SUMMARIZER_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();

    if !status.success() {
        return Err(failure_reason(status.code(), &out, &err));
    }
    let text = out.trim();
    if text.is_empty() {
        return Err("empty output".into());
    }
    Ok(text.to_string())
}

/// What the panel says a dead summarizer died of.
///
/// **`claude -p` reports its own failures on stdout, not stderr** — measured
/// 2026-09-03: a bad OAuth token exits 1 with "Failed to authenticate. API
/// Error: 401 OAuth access token is invalid." on stdout and an *empty* stderr.
/// Reading stderr alone is what left a real failure entry saying "ההסבר נכשל
/// (exit 1)" — the exit code with the reason thrown away, a code reporting
/// ignorance in the same breath as a diagnosis. So: prefer stderr, which
/// carries the more specific message when there is one, and fall back to
/// stdout.
fn failure_reason(code: Option<i32>, out: &str, err: &str) -> String {
    let code = code.map_or("killed".to_string(), |c| format!("exit {c}"));
    match first_line(err).or_else(|| first_line(out)) {
        Some(reason) => format!("{code}: {reason}"),
        None => code,
    }
}

/// First non-blank line of a child's output, trimmed and length-capped — the
/// panel row is two inches wide and an API error can be a paragraph.
fn first_line(s: &str) -> Option<String> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    })
}

/// Is this transcript entry a real human prompt — a turn boundary? Measured on
/// real transcripts (probe0): tool results also arrive as `type:"user"` but
/// carry a `tool_result` block; local-command caveats carry `isMeta:true`; a
/// sidechain's entries carry `isSidechain:true`. A `<task-notification>` turn
/// arrives as a plain string user entry and deliberately counts — a
/// notification-triggered turn gets its own explanation.
fn is_real_user_prompt(e: &Value) -> bool {
    if e.get("type").and_then(Value::as_str) != Some("user")
        || e.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        || e.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
    {
        return false;
    }
    match e.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let mut prompt_like = false;
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_result") => return false,
                    Some("text") | Some("image") => prompt_like = true,
                    _ => {}
                }
            }
            prompt_like
        }
        _ => false,
    }
}

/// Flatten an `AskUserQuestion` `tool_input` into plain text for the
/// summarizer: each question with its options and descriptions, `(Recommended)`
/// labels kept verbatim so the prompt's rule can point at them. `None` when the
/// payload has no usable questions.
fn question_text(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let mut out = String::new();
    for (i, q) in v.get("questions")?.as_array()?.iter().enumerate() {
        let question = q.get("question").and_then(Value::as_str).unwrap_or("");
        if question.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("Question {}: {question}\n", i + 1));
        if q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false) {
            out.push_str("(multiple answers allowed)\n");
        }
        out.push_str("Options:\n");
        for o in q.get("options").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let label = o.get("label").and_then(Value::as_str).unwrap_or("");
            let desc = o.get("description").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("- {label}: {desc}\n"));
        }
    }
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The plan out of an `ExitPlanMode` `tool_input`: its `plan` field, markdown
/// and all — Sonnet reads markdown fine, and stripping it would only cost the
/// structure that says which line is the goal. Head-capped (see
/// `MAX_PLAN_BYTES`). `None` when the payload carries no usable plan; the
/// payload also has a `planFilePath` pointing at the same text on disk, which
/// is deliberately ignored: one source, and no file read on this path.
fn plan_text(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let plan = v.get("plan")?.as_str()?.trim();
    if plan.is_empty() {
        return None;
    }
    if plan.len() <= MAX_PLAN_BYTES {
        return Some(plan.to_string());
    }
    let mut cut = MAX_PLAN_BYTES;
    while !plan.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}\n[…truncated…]", &plan[..cut]))
}

/// What one ⌘⇧E found in the transcript, and which persona explains it.
struct Turn {
    kind: ExplainKind,
    prompt: &'static str,
    text: String,
}

/// [`read_turn`] with the transcript-flush race absorbed. Tried **immediately**
/// and retried only while the turn reads as *unsettled*: empty, which is the
/// state the race produces after a `Stop`; or — when `expect_dialog`, i.e. the
/// request came from `askq`/`plan` — a plain turn, because the hook fired for
/// a dialog and the transcript that shows none has not caught up yet. A plain
/// turn summarized in that window would have been the *wrong* explanation
/// with no second chance (`Stop` does not fire while the dialog waits). After
/// the last attempt whatever was read is used: a plain turn is still better
/// than nothing, and the log line says which it was. Runs on a worker thread;
/// the waiting costs nobody anything.
fn read_turn_settled(path: &Path, expect_dialog: bool) -> Option<Turn> {
    let mut last = None;
    for attempt in 0..=EXTRACT_RETRIES {
        if attempt > 0 {
            std::thread::sleep(EXTRACT_DELAY);
        }
        match read_turn(path) {
            Some(turn) if expect_dialog && matches!(turn.kind, ExplainKind::Turn) => {
                last = Some(turn);
            }
            Some(turn) => return Some(turn),
            None => {}
        }
    }
    if last.is_some() {
        eprintln!(
            "[explainer] {}: a dialog was announced but never showed in the transcript; explaining the prose",
            path.display()
        );
    }
    last
}

/// The current turn out of a transcript, and what kind of turn it is.
///
/// A claude waiting on the user is the case the panel exists for, so it is
/// checked first: an `AskUserQuestion` or `ExitPlanMode` `tool_use` that has no
/// `tool_result` yet *is* the current turn, whatever prose came before it, and
/// it is explained with its own persona. Both payloads are already in the
/// transcript when the dialog is on screen (measured 2026-09-16: the assistant
/// entry carrying the `questions` array is written before the tool runs), which
/// is what lets one keypress cover all three cases from one file.
fn read_turn(path: &Path) -> Option<Turn> {
    let entries = read_entries(path)?;
    if let Some((name, input)) = pending_ask(&entries) {
        let json = input.to_string();
        if name == "AskUserQuestion" {
            if let Some(question) = question_text(&json) {
                // **The turn's own prose rides along with the question.** It
                // used to be dropped, and the panel then showed a claude who
                // asks without ever having done anything — which is exactly how
                // it read on screen. What he found before stopping is usually
                // the reason the question is being asked at all.
                let said = turn_text(&entries).unwrap_or_default();
                let body = format!(
                    "{said}\n\n=== THE QUESTION I STOPPED TO ASK ===\n{question}"
                );
                return Some(Turn {
                    kind: ExplainKind::Question,
                    prompt: QUESTION_PROMPT,
                    text: with_recent_prompts(&entries, &body),
                });
            }
        } else if let Some(plan) = plan_text(&json) {
            // The prose before the plan is the research that justifies it, and
            // it is what `DID` is made of — same reason the question case
            // carries it. Without it a plan turn reads as pure intent.
            let said = turn_text(&entries).unwrap_or_default();
            let body = format!("{said}\n\n=== THE PLAN I AM WAITING FOR APPROVAL ON ===\n{plan}");
            return Some(Turn {
                kind: ExplainKind::Plan,
                prompt: PLAN_PROMPT,
                text: with_recent_prompts(&entries, &body),
            });
        }
    }
    let text = turn_text(&entries)?;
    Some(Turn {
        kind: ExplainKind::Turn,
        prompt: HEBREW_PROMPT,
        text: with_recent_prompts(&entries, &text),
    })
}

/// The summarizer's input for a turn: the user's recent prompts, then this
/// turn's text. Labelled in ASCII — these are structural markers in a payload
/// that is otherwise Hebrew and English mixed, and they have to stay
/// unmistakable in a bidirectional blob.
fn with_recent_prompts(entries: &[Value], turn: &str) -> String {
    let prompts = recent_prompts(entries);
    if prompts.is_empty() {
        return turn.to_string();
    }
    format!("=== RECENT USER MESSAGES (oldest first) ===\n{prompts}\n\n=== MY CURRENT TURN ===\n{turn}")
}

/// The last few things the **user** actually said, oldest first.
///
/// `<task-notification>` turns are dropped: they are the runtime waking the
/// instance, not the user stating a goal, and a `WORK:` line built from one
/// would describe a background job instead of the work. A `<command-name>`
/// entry stays — a slash command is an instruction the user gave.
fn recent_prompts(entries: &[Value]) -> String {
    let mut picked: Vec<String> = Vec::new();
    for e in entries.iter().rev() {
        if picked.len() == RECENT_PROMPTS {
            break;
        }
        if !is_real_user_prompt(e) {
            continue;
        }
        let text = prompt_text(e);
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.starts_with("<task-notification") {
            continue;
        }
        picked.push(trimmed.to_string());
    }
    picked.reverse();
    let joined = picked.join("\n---\n");
    // Tail-capped: of a window of prompts, the recent end is the current goal.
    tail_cap(joined, MAX_PROMPTS_BYTES)
}

/// One user entry's text, whether its content is a bare string or a block list.
fn prompt_text(e: &Value) -> String {
    match e.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Keep the last `max` bytes, cut on a char boundary, marked when cut. The text
/// here is Hebrew, so every char is multibyte and a naive slice panics.
fn tail_cap(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[…truncated…]\n{}", &s[cut..])
}

/// Split the summarizer's three lines into the parts the panel draws headings
/// for. `None` unless **all three** keys are present with something after them
/// — see [`SECTION_KEYS`].
///
/// A value may wrap onto following lines (the model is asked for one line each
/// and mostly obliges, but a long `DID:` occasionally doesn't), so anything
/// before the next key belongs to the key above it.
fn parse_sections(out: &str) -> Option<ExplainSections> {
    let mut found: [Option<String>; 3] = [None, None, None];
    let mut current: Option<usize> = None;
    // Enough context to tell a *wrapped sentence* from a *new line of content*:
    // only a plain line directly under another plain line is a wrap. A list
    // item, the line after a list item (the next question's header), and
    // anything after a blank line all start a row of their own. Measured on real
    // multi-question output — without the "after a list item" case, question 2's
    // header was glued onto the tail of question 1's last option.
    let mut prev_listy = false;
    let mut blank_since = false;
    for line in out.lines() {
        let line = line.trim();
        let key = SECTION_KEYS.iter().position(|k| {
            line.strip_prefix(*k).and_then(|r| r.trim_start().strip_prefix(':')).is_some()
        });
        match key {
            Some(i) => {
                let value = line[SECTION_KEYS[i].len()..].trim_start();
                let value = value.strip_prefix(':').unwrap_or(value).trim();
                found[i] = Some(value.to_string());
                current = Some(i);
                prev_listy = false;
                blank_since = false;
            }
            None if line.is_empty() => blank_since = true,
            None => {
                let listy = line.starts_with('-');
                if let Some(i) = current {
                    let slot = found[i].get_or_insert_with(String::new);
                    if !slot.is_empty() {
                        // `.text` is `white-space: pre-wrap`, so a newline here
                        // is what puts a question's options one per row.
                        let wrap = !listy && !prev_listy && !blank_since;
                        slot.push(if wrap { ' ' } else { '\n' });
                    }
                    slot.push_str(line);
                }
                prev_listy = listy;
                blank_since = false;
            }
        }
    }
    let [work, did, need] = found;
    let (work, did, need) = (work?, did?, need?);
    if work.is_empty() || did.is_empty() || need.is_empty() {
        return None;
    }
    Some(ExplainSections { work, did, need })
}

/// The transcript as parsed JSONL entries; unparseable lines (a half-flushed
/// last line, most often) are dropped rather than failing the read.
fn read_entries(path: &Path) -> Option<Vec<Value>> {
    let raw = std::fs::read_to_string(path).ok()?;
    Some(
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    )
}

/// An `AskUserQuestion` / `ExitPlanMode` call still waiting for its answer:
/// the last such `tool_use` in the transcript, if no `tool_result` for its id
/// has come back. The answered ones are ordinary history — the question the
/// user already dismissed is not what ⌘⇧E is asking about.
fn pending_ask(entries: &[Value]) -> Option<(&str, &Value)> {
    let mut found: Option<(&str, &Value, &str)> = None;
    let mut answered: Vec<&str> = Vec::new();
    for e in entries {
        let Some(Value::Array(blocks)) = e.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        if e.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                    if name == "AskUserQuestion" || name == "ExitPlanMode" {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        if let Some(input) = b.get("input") {
                            found = Some((name, input, id));
                        }
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                        answered.push(id);
                    }
                }
                _ => {}
            }
        }
    }
    let (name, input, id) = found?;
    (!answered.contains(&id)).then_some((name, input))
}

/// The current turn's assistant text: every `text` block of every
/// non-sidechain assistant entry after the last real user prompt, joined. Tail-
/// capped at `MAX_TURN_BYTES`. `None` when there is nothing to explain.
fn turn_text(entries: &[Value]) -> Option<String> {
    let boundary = entries.iter().rposition(is_real_user_prompt)?;
    let mut texts: Vec<&str> = Vec::new();
    for e in &entries[boundary + 1..] {
        if e.get("type").and_then(Value::as_str) != Some("assistant")
            || e.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
        {
            continue;
        }
        let Some(Value::Array(blocks)) = e.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        texts.push(t);
                    }
                }
            }
        }
    }
    let joined = texts.join("\n\n");
    if joined.trim().is_empty() {
        return None;
    }
    Some(tail_cap(joined, MAX_TURN_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(tag: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mulpex-explain-{tag}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    /// The prose half of a transcript read, the way the tests used to call it
    /// before `read_turn` grew the pending-question branch in front of it.
    fn extract_turn_text(path: &Path) -> Option<String> {
        turn_text(&read_entries(path)?)
    }

    /// The measured `ExitPlanMode` tool_input shape (a real `claude` v2.1.252 on
    /// a PTY, 2026-09-01): the plan comes through as markdown, `planFilePath` is
    /// ignored, and anything without a real plan is a skip rather than a Sonnet
    /// call on nothing.
    #[test]
    fn plan_payloads_yield_the_plan_or_nothing() {
        // `r##`: a plan is markdown, so the JSON contains `"#` — which would
        // close a plain `r#""#` literal early.
        let json = r##"{"plan":"# Add a comment\n\nInsert one line at the top.",
            "planFilePath":"/Users/x/.claude/plans/plan-abc.md"}"##;
        let text = plan_text(json).unwrap();
        assert!(text.starts_with("# Add a comment"), "markdown is kept; sonnet reads it fine");
        assert!(!text.contains("planFilePath"), "the file path is not part of the plan");

        assert!(plan_text(r#"{"planFilePath":"/x.md"}"#).is_none(), "no plan field → skip");
        assert!(plan_text(r#"{"plan":"   "}"#).is_none(), "a blank plan → skip");
        assert!(plan_text("not json").is_none(), "an unparseable payload → skip");
    }

    /// A long plan is cut from the END, the opposite of a turn: a plan opens
    /// with its goal and descends into steps and file lists, and the one-line
    /// summary only needs the opening. The cut lands on a char boundary — a
    /// plan is full of Hebrew and box-drawing characters in practice.
    #[test]
    fn a_long_plan_keeps_its_head_not_its_tail() {
        let plan = format!("GOAL FIRST. {}END LAST.", "מטרה ועוד פרטים. ".repeat(2000));
        assert!(plan.len() > MAX_PLAN_BYTES, "the fixture has to actually be too long");
        let json = serde_json::json!({ "plan": plan }).to_string();
        let text = plan_text(&json).unwrap();
        assert!(text.starts_with("GOAL FIRST."), "the head survived");
        assert!(!text.contains("END LAST."), "the tail was cut");
        assert!(text.ends_with("[…truncated…]"), "and says so");
        assert!(text.len() < plan.len());
    }

    /// The shape measured on real transcripts (probe0): the boundary is the last
    /// *human* prompt — a `tool_result` "user" entry is mid-turn plumbing, a
    /// meta entry is a caveat, a sidechain belongs to a subagent — and only this
    /// turn's non-sidechain assistant text survives.
    #[test]
    fn extraction_finds_the_last_real_turn_and_only_its_text() {
        let path = write_transcript("turn", &[
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OLD TURN"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"SUBAGENT"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part one"}]}}"#,
            r#"{"type":"ai-title","title":"noise"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part two"}]}}"#,
        ]);
        assert_eq!(extract_turn_text(&path).as_deref(), Some("part one\n\npart two"));
        let _ = std::fs::remove_file(&path);
    }

    /// The flush race, reproduced: the `Stop` request is drained while the
    /// transcript holds the turn's boundary and tool traffic but not yet its
    /// final text — Claude Code appends that entry a beat later (measured: same
    /// second as the turn's end). The settled reader must pick it up instead of
    /// reporting an empty turn at the user.
    #[test]
    fn extraction_waits_out_the_transcript_flush_race() {
        let path = write_transcript("race", &[
            r#"{"type":"user","message":{"content":"question"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
        ]);
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let mut f = std::fs::OpenOptions::new().append(true).open(&writer_path).unwrap();
            f.write_all(
                b"\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"late answer\"}]}}",
            )
            .unwrap();
        });
        let turn = read_turn_settled(&path, false).unwrap();
        assert!(turn.text.ends_with("late answer"), "got: {}", turn.text);
        assert!(matches!(turn.kind, ExplainKind::Turn));
        writer.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// The same race one entry later, for a request an `askq` hook wrote: the
    /// hook fires for the dialog, but the transcript the worker opens may hold
    /// only the prose before it. Without the `dialog` marker that prose reads as
    /// a complete turn and gets explained as one — the wrong explanation, and
    /// the only one (`Stop` does not fire while the dialog waits). With it the
    /// reader keeps going until the question entry lands.
    #[test]
    fn a_dialog_request_waits_for_the_dialog_entry() {
        let path = write_transcript("dialog-race", &[
            r#"{"type":"user","message":{"content":"go"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"בדקתי את הקוד"}]}}"#,
        ]);
        // Without the marker this is a settled plain turn, immediately.
        let plain = read_turn_settled(&path, false).unwrap();
        assert!(matches!(plain.kind, ExplainKind::Turn));

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let mut f = std::fs::OpenOptions::new().append(true).open(&writer_path).unwrap();
            f.write_all(
                br#"
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"AskUserQuestion","input":{"questions":[{"question":"Which way?","options":[{"label":"A","description":"first"}]}]}}]}}"#,
            )
            .unwrap();
        });
        let turn = read_turn_settled(&path, true).unwrap();
        assert!(matches!(turn.kind, ExplainKind::Question), "waited for the dialog, not the prose");
        assert!(turn.text.contains("Which way?"));
        writer.join().unwrap();

        // A dialog that never shows up: the prose is used after the wait rather
        // than nothing at all.
        let path = write_transcript("dialog-never", &[
            r#"{"type":"user","message":{"content":"go"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"רק טקסט"}]}}"#,
        ]);
        let turn = read_turn_settled(&path, true).unwrap();
        assert!(matches!(turn.kind, ExplainKind::Turn));
        assert!(turn.text.contains("רק טקסט"));
        let _ = std::fs::remove_file(&path);
    }

    /// Measured 2026-08-30: a local command (`/clear`) lands as a plain string
    /// user entry — NOT meta — shaped `<command-name>…`. It must stay a valid
    /// boundary: a slash command that runs a real turn (`/sync-docs`) starts it,
    /// and text from before the command must never leak into that turn.
    #[test]
    fn a_local_command_entry_is_a_turn_boundary() {
        let path = write_transcript("cmd", &[
            r#"{"type":"user","message":{"content":"earlier prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"EARLIER TURN"}]}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/sync-docs</command-name>\n<command-message>sync-docs</command-message>"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>Caveat: …</local-command-caveat>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the command turn"}]}}"#,
        ]);
        assert_eq!(extract_turn_text(&path).as_deref(), Some("the command turn"));
        let _ = std::fs::remove_file(&path);
    }

    /// Measured on a real session: an interrupt lands as a plain string user
    /// entry with nothing after it. Nothing to explain → no Sonnet call at all;
    /// `process` turns this `None` into the `NOTHING_YET` entry.
    #[test]
    fn a_turn_with_no_assistant_text_reads_as_empty() {
        let path = write_transcript("empty", &[
            r#"{"type":"user","message":{"content":"do things"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#,
        ]);
        assert_eq!(extract_turn_text(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    /// The cap keeps the tail — conclusions live at the end of a turn — and
    /// must cut on a char boundary (the text is Hebrew, every char multibyte).
    #[test]
    fn oversized_turns_keep_the_tail_marked_truncated() {
        let big = "א".repeat(MAX_TURN_BYTES); // 2 bytes per char → over the cap
        let line = format!(
            r#"{{"type":"user","message":{{"content":"p"}}}}
{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{big}END"}}]}}}}"#
        );
        let path = write_transcript("cap", &[&line]);
        let got = extract_turn_text(&path).unwrap();
        assert!(got.starts_with("[…truncated…]\n"));
        assert!(got.ends_with("END"));
        assert!(got.len() <= MAX_TURN_BYTES + "[…truncated…]\n".len() + 4);
        let _ = std::fs::remove_file(&path);
    }

    /// The store holds **one** entry per instance — the current turn, nothing
    /// before it — and is dropped per instance on clear/reap and wholesale on
    /// project close. Exercises the real public surface against the module
    /// singleton — handles in the 990xx range so parallel tests can't collide
    /// (`init` is never called in tests, so no worker drains the queue behind
    /// our back).
    #[test]
    fn the_feed_is_newest_first_capped_and_forgettable() {
        const H: ProjectHandle = 99001;
        const H2: ProjectHandle = 99002;
        let entry = |id: usize, n: u64| ExplainEntry {
            id,
            ts: n,
            text: format!("e{n}"),
            sections: None,
            ok: true,
            kind: ExplainKind::Turn,
            seq: next_seq(),
        };
        for n in 0..(MAX_ENTRIES as u64 + 5) {
            push_entry(inner(), H, 1, entry(1, n));
        }
        push_entry(inner(), H, 2, entry(2, 0));
        push_entry(inner(), H2, 1, entry(1, 0));

        let mine: Vec<_> = feed(H);
        let rows: Vec<_> = mine.iter().filter(|e| e.id == 1).collect();
        assert_eq!(rows.len(), MAX_ENTRIES, "capped");
        assert_eq!(rows[0].ts, MAX_ENTRIES as u64 + 4, "newest first");
        assert!(
            rows.iter().all(|e| e.ts >= 5),
            "the oldest rows are gone completely, not merely hidden"
        );

        forget(H, 1);
        assert!(feed(H).iter().all(|e| e.id != 1));
        assert!(feed(H).iter().any(|e| e.id == 2), "other instance survives");
        forget_project(H);
        assert!(feed(H).is_empty());
        assert_eq!(feed(H2).len(), 1, "other project survives");
        forget_project(H2);
    }

    /// A retry rewrites the failed row in place and takes its stash with it the
    /// moment the row is gone — replaced by a newer entry, or cleared. Drives
    /// the real singleton the way the panel does — stash, retry, replace —
    /// without spawning a summarizer.
    #[test]
    fn a_retry_rewrites_its_row_in_place_and_its_stash_dies_with_the_row() {
        const H: ProjectHandle = 99005;
        let stash = |id: usize, seq: u64| {
            let r = Retryable {
                handle: H,
                id,
                kind: ExplainKind::Turn,
                prompt: HEBREW_PROMPT,
                text: "the turn".into(),
                cwd: PathBuf::from("/tmp"),
            };
            inner().retries.lock().unwrap().insert(seq, r);
        };
        let failed = |id: usize, seq: u64| ExplainEntry {
            id,
            ts: 1,
            text: "ההסבר נכשל (exit 1)".into(),
            sections: None,
            ok: false,
            kind: ExplainKind::Turn,
            seq,
        };

        // One failed row on screen, and a failed row on a second instance —
        // two retries in flight must both run.
        let (a, b) = (next_seq(), next_seq());
        stash(1, a);
        stash(2, b);
        push_entry(inner(), H, 1, failed(1, a));
        push_entry(inner(), H, 2, failed(2, b));

        assert!(!retry(H, 1, next_seq()), "an unknown seq queues nothing");
        assert!(!retry(H, 2, a), "another instance cannot retry this row");
        assert!(retry(H, 1, a));
        assert!(retry(H, 2, b), "a second retry is not coalesced away by the first");
        {
            let q = inner().queue.lock().unwrap();
            assert_eq!(q.iter().filter(|j| j.handle == H).count(), 2);
        }
        assert!(retry(H, 1, a), "the stash survives, so the row stays retryable");

        // The result rewrites the failed row rather than arriving beside it.
        let ok = ExplainEntry {
            id: 1,
            ts: 9,
            text: "הסבר".into(),
            sections: None,
            ok: true,
            kind: ExplainKind::Turn,
            seq: a,
        };
        replace_entry(inner(), H, 1, ok);
        let f: Vec<_> = feed(H).into_iter().filter(|e| e.id == 1).collect();
        assert_eq!(f.len(), 1, "rewritten, not prepended");
        assert!(f[0].ok && f[0].seq == a);

        // Enough newer entries push the failed row off the end of the feed, and
        // the stash goes with it — nothing can address it any more.
        for _ in 0..MAX_ENTRIES {
            push_entry(inner(), H, 2, failed(2, next_seq()));
        }
        assert!(!retry(H, 2, b), "an evicted row is no longer retryable");
        assert!(inner().retries.lock().unwrap().get(&b).is_none());

        forget_project(H);
        assert!(inner().retries.lock().unwrap().values().all(|r| r.handle != H));
    }

    /// One keypress, three kinds, one file. A question or a plan still waiting
    /// for the user IS the current turn and wins over the prose before it —
    /// that is the whole reason the panel is worth opening mid-dialog. The
    /// shape is the measured one (2026-09-16): the assistant entry carrying the
    /// `questions` array is in the transcript while the dialog is on screen.
    #[test]
    fn a_waiting_dialog_outranks_the_prose_before_it() {
        // (And carries that prose with it — see the two `find` assertions.)
        // One line: a transcript is JSONL, and `write_transcript` joins with \n.
        const ASKED: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"בדקתי את הקוד"},{"type":"tool_use","id":"toolu_1","name":"AskUserQuestion","input":{"questions":[{"question":"Which way?","options":[{"label":"A","description":"first"}]}]}}]}}"#;
        let path = write_transcript("pending-q", &[
            r#"{"type":"user","message":{"content":"go"}}"#,
            ASKED,
        ]);
        let turn = read_turn(&path).unwrap();
        assert!(matches!(turn.kind, ExplainKind::Question), "the question is the turn");
        assert!(turn.text.contains("Question 1: Which way?"));
        // The regression the user reported: the work before the question was
        // dropped, so the panel showed a claude who asks having done nothing.
        assert!(turn.text.contains("בדקתי את הקוד"), "the turn's own work rides along");
        assert!(
            turn.text.find("בדקתי את הקוד") < turn.text.find("Question 1"),
            "what I did comes before what I'm asking"
        );

        // Answered, it is history again: the turn's own text is what's left.
        let path = write_transcript("answered-q", &[
            r#"{"type":"user","message":{"content":"go"}}"#,
            ASKED,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"A"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ממשיך עם A"}]}}"#,
        ]);
        let turn = read_turn(&path).unwrap();
        assert!(matches!(turn.kind, ExplainKind::Turn), "an answered question is history");
        assert!(turn.text.contains("ממשיך עם A"));

        let path = write_transcript("pending-plan", &[
            r#"{"type":"user","message":{"content":"plan it"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"קראתי את הקוד"}]}}"#,
            // `r##`: the plan is markdown, so the JSON contains `"#`.
            r##"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"ExitPlanMode","input":{"plan":"# Do the thing"}}]}}"##,
        ]);
        let turn = read_turn(&path).unwrap();
        assert!(matches!(turn.kind, ExplainKind::Plan));
        assert!(turn.text.contains("# Do the thing"));
        // Same rule as a question: the research that justifies the plan is what
        // `DID` is made of, so it must reach the summarizer with the plan.
        assert!(turn.text.contains("קראתי את הקוד"), "the work behind the plan rides along");
        assert!(
            turn.text.find("קראתי את הקוד") < turn.text.find("# Do the thing"),
            "what I did comes before what I'm proposing"
        );
    }

    /// The three-part contract, from both ends. All three keys or nothing — a
    /// heading with nothing under it is a default reading as an assertion, and
    /// the panel would rather show the raw sentence than invent a structure.
    #[test]
    fn three_lines_parse_and_anything_less_does_not() {
        let s = parse_sections(
            "WORK: משנים את ההתנהגות של ההסבר\nDID: ביטלתי את ההפעלה האוטומטית\nNEED: כלום, אפשר להמשיך",
        )
        .unwrap();
        assert_eq!(s.work, "משנים את ההתנהגות של ההסבר");
        assert_eq!(s.did, "ביטלתי את ההפעלה האוטומטית");
        assert_eq!(s.need, "כלום, אפשר להמשיך");

        // A wrapped value belongs to the key above it; blank lines are noise.
        let s = parse_sections("WORK: א\n\nDID: התחלה\nהמשך של אותה שורה\nNEED: ג\n").unwrap();
        assert_eq!(s.did, "התחלה המשך של אותה שורה");

        // Tolerated sloppiness: whitespace before the colon, and stray indent.
        assert!(parse_sections("  WORK : א\n  DID: ב\n  NEED: ג").is_some());

        // A question's NEED is the ask, then one line per option: option lines
        // keep their own row (the panel is `pre-wrap`), a wrapped sentence does
        // not. This is what lets a question use the same three parts as a turn.
        let s = parse_sections(
            "WORK: א\nDID: ב\nNEED: להחליט איפה\n- בתוך NEED — אותו מבנה (מומלץ)\n- בחלק נפרד — הפרדה ברורה",
        )
        .unwrap();
        assert_eq!(s.need.lines().count(), 3, "the ask plus one row per option");
        assert!(s.need.lines().nth(1).unwrap().starts_with("- בתוך NEED"));

        // The real thing: three questions in one NEED, as the live summarizer
        // actually emitted them (2026-09-16). Every header and every option has
        // to keep its own row — the first version of this parser glued
        // question 2's header onto question 1's last option.
        let s = parse_sections(
            "WORK: א\nDID: ב\n\nNEED: איך לשלב שאלה ממתינה\n             - בתוך החלק השלישי (מומלץ) - אותו מבנה תמיד\n             - חלק רביעי נפרד - הפרדה ברורה\n\n             כמה פירוט להראות על כל אפשרות\n             - תווית ועוד כמה מילים (מומלץ) - שורה קצרה\n             - תווית בלבד - הכי קצר",
        )
        .unwrap();
        let rows: Vec<&str> = s.need.lines().collect();
        assert_eq!(rows.len(), 6, "two headers and four options, one row each: {rows:?}");
        assert_eq!(rows[3], "כמה פירוט להראות על כל אפשרות", "a new header is not a wrap");
        assert!(rows[1].starts_with('-') && rows[5].starts_with('-'));

        assert!(parse_sections("WORK: א\nDID: ב").is_none(), "a missing key is not a section");
        assert!(parse_sections("WORK: א\nDID:\nNEED: ג").is_none(), "an empty value likewise");
        assert!(parse_sections("סתם משפט בעברית").is_none(), "prose is prose");
    }

    /// The `WORK:` line's raw material: the user's own recent prompts, oldest
    /// first, with the runtime's `<task-notification>` wakes left out — those
    /// are a background job reporting in, not the user stating a goal, and a
    /// goal built from one describes the wrong thing entirely.
    #[test]
    fn the_input_carries_the_users_recent_prompts_not_the_runtimes() {
        let path = write_transcript("prompts", &[
            r#"{"type":"user","message":{"content":"build the thing"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}"#,
            r#"{"type":"user","message":{"content":"<task-notification><status>completed</status></task-notification>"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"noted"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"now make it faster"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"THE TURN"}]}}"#,
        ]);
        let text = read_turn(&path).unwrap().text;
        assert!(text.contains("build the thing") && text.contains("now make it faster"));
        assert!(!text.contains("task-notification"), "the runtime is not the user");
        assert!(
            text.find("build the thing") < text.find("now make it faster"),
            "oldest first, so the newest goal reads last"
        );
        assert!(text.contains("=== MY CURRENT TURN ===\nTHE TURN"));
        let _ = std::fs::remove_file(&path);
    }

    /// Both caps keep the **tail** and cut on a char boundary — the text is
    /// Hebrew, so every char is multibyte and a naive slice panics.
    #[test]
    fn the_prompt_window_is_tail_capped_on_a_char_boundary() {
        let long = "שאלה ישנה מאוד. ".repeat(500);
        assert!(long.len() > MAX_PROMPTS_BYTES);
        let line = format!(
            r#"{{"type":"user","message":{{"content":"{long}"}}}}
{{"type":"user","message":{{"content":"THE LATEST ASK"}}}}
{{"type":"assistant","message":{{"content":[{{"type":"text","text":"t"}}]}}}}"#
        );
        let path = write_transcript("promptcap", &[&line]);
        let text = read_turn(&path).unwrap().text;
        assert!(text.contains("THE LATEST ASK"), "the newest prompt survives");
        assert!(text.contains("[…truncated…]"), "and the cut says so");
        let _ = std::fs::remove_file(&path);
    }

    /// The failure text is only useful if it says why: `claude -p` reports its
    /// own errors on stdout, so a bare "exit 1" is a reason thrown away.
    #[test]
    fn a_failure_reason_is_a_capped_first_nonblank_line() {
        assert_eq!(first_line("\n\n  boom  \nsecond\n").as_deref(), Some("boom"));
        assert_eq!(first_line("   \n\t\n"), None, "no reason is None, not an empty line");
        let long = first_line(&"x".repeat(400)).unwrap();
        assert_eq!(long.chars().count(), 201, "capped, with the ellipsis");
        assert!(long.ends_with('…'));
    }

    /// The regression that sent the user here: the reason a summarizer died has
    /// to reach the panel, and `claude -p` puts it on stdout. Strings are the
    /// ones measured 2026-09-03 from a real `claude -p` with a bad token.
    #[test]
    fn a_dead_summarizer_reports_the_reason_not_just_the_code() {
        let real = "Failed to authenticate. API Error: 401 OAuth access token is invalid.";
        assert_eq!(
            failure_reason(Some(1), &format!("{real}\n"), ""),
            format!("exit 1: {real}"),
            "stdout carries the reason when stderr is empty"
        );
        assert_eq!(
            failure_reason(Some(1), "some output", "boom\n"),
            "exit 1: boom",
            "stderr wins when it has something to say"
        );
        assert_eq!(failure_reason(Some(1), "  \n", " \n"), "exit 1", "nothing to add, nothing said");
        assert_eq!(failure_reason(None, "", ""), "killed");
    }

    /// The busy dot's bookkeeping: a coalesced re-submit doesn't double-count,
    /// finish drops to idle exactly once, and forget clears without letting a
    /// late finish go negative or re-fire.
    #[test]
    fn pending_counts_track_queue_running_and_forget() {
        const H: ProjectHandle = 99004;
        let count = |id: usize| inner().pending.lock().unwrap().get(&(H, id)).copied();

        submit(H, 1, "/tmp/a.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 1, "/tmp/b.jsonl".into(), PathBuf::from("/tmp"));
        assert_eq!(count(1), Some(1), "a coalesced re-submit is one job, not two");
        submit(H, 2, "/tmp/c.jsonl".into(), PathBuf::from("/tmp"));
        assert_eq!(count(2), Some(1));

        finish_pending(inner(), H, 1);
        assert_eq!(count(1), None, "finished means idle, entry gone");
        finish_pending(inner(), H, 1);
        assert_eq!(count(1), None, "a late finish after forget/idle stays silent");

        forget(H, 2);
        assert_eq!(count(2), None, "forget clears the dot with the feed");
        forget_project(H); // drain this test's queue entries
    }

    /// Latest-wins coalescing: two requests for one instance drained in the
    /// same tick (or a second one arriving while the first still queues) replace
    /// the queued-not-yet-running job in place rather than stacking Sonnet
    /// calls, and a second instance queues alongside rather than being
    /// collapsed into the first. The request body is parsed here too: line 1
    /// is the path, a `dialog` line 2 is the marker, an empty body is nothing.
    #[test]
    fn queued_requests_coalesce_per_instance() {
        const H: ProjectHandle = 99003;
        submit(H, 1, "/tmp/a.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 2, "/tmp/b.jsonl".into(), PathBuf::from("/tmp"));
        submit(H, 1, "/tmp/c.jsonl\ndialog".into(), PathBuf::from("/tmp"));
        submit(H, 3, "   \n".into(), PathBuf::from("/tmp"));
        {
            let q = inner().queue.lock().unwrap();
            let mine: Vec<_> = q.iter().filter(|j| j.handle == H).collect();
            assert_eq!(mine.len(), 2, "one job per instance, and an empty body queues nothing");
            let turn1 = mine.iter().find(|j| j.id == 1).unwrap();
            match &turn1.input {
                Input::Now { transcript, expect_dialog } => {
                    assert_eq!(transcript, &PathBuf::from("/tmp/c.jsonl"), "the newer request won");
                    assert!(expect_dialog, "and it carried its dialog marker");
                }
                Input::Retry { .. } => unreachable!(),
            }
            let turn2 = mine.iter().find(|j| j.id == 2).unwrap();
            assert!(matches!(turn2.input, Input::Now { expect_dialog: false, .. }));
        }
        forget_project(H); // also drains this test's queue entries
        assert!(inner().queue.lock().unwrap().iter().all(|j| j.handle != H));
    }

    /// The measured `AskUserQuestion` tool_input shape flattens into text the
    /// summarizer can explain: questions numbered, options with descriptions,
    /// `(Recommended)` labels kept verbatim. Junk payloads yield None (skip).
    #[test]
    fn question_payloads_flatten_to_explainable_text() {
        let json = r#"{"questions":[
            {"question":"What next with todo.html?","header":"Next step","multiSelect":false,
             "options":[
                {"label":"Add localStorage persistence (Recommended)","description":"Tasks survive refresh."},
                {"label":"Nothing for now","description":"Just testing."}]},
            {"question":"Which parts?","header":"Parts","multiSelect":true,
             "options":[{"label":"UI","description":"the page"}]}
        ]}"#;
        let text = question_text(json).unwrap();
        assert!(text.contains("Question 1: What next with todo.html?"));
        assert!(text.contains("- Add localStorage persistence (Recommended): Tasks survive refresh."));
        assert!(text.contains("Question 2: Which parts?"));
        assert!(text.contains("(multiple answers allowed)"));
        assert_eq!(question_text(r#"{"questions":[]}"#), None, "no questions → skip");
        assert_eq!(question_text("not json"), None);
    }
}
