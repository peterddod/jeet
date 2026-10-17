//! The assistant off the push path (`[assistant] push = false`): what jeet's
//! agents and events address to it is kept in its `delivered.jsonl`, marked
//! stored, rather than pushed into a session. Each session keeps a read
//! cursor; the user's next prompt carries a hidden line saying what came
//! since it ([`summary_line`]), and `jeet brief` prints it and moves the
//! cursor on. Nothing here starts a turn.
//!
//! The cursors are `<assistant>/.jeet/brief.json`, `{"<key>": <m id>}`: a
//! session's by its id, the user's at their terminal as `user`. A key with
//! no cursor yet starts from the furthest any session has read, so a new
//! session hears of what no session has read, not of everything kept.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::agent::age_label;
use crate::context::App;
use crate::db::tasks::TaskReportRecord;
use crate::messages::{self, Delivered, Delivery};
use crate::permissions::{self, Caller};
use crate::render::{self, Class, Meta};
use crate::reports;
use crate::status;
use crate::tasks;
use crate::worktrees::now_secs;

/// The cursor key of the user at their own terminal.
pub const USER_KEY: &str = "user";

/// The cursor key of an assistant whose session jeet cannot tell.
const ASSISTANT_KEY: &str = "assistant";

/// Keep `text`, for the assistant in `dir`, for `jeet brief`. `meta` says
/// what it is; without one, it is read from the fence jeet put it in.
pub fn store(app: &App, dir: &Path, text: &str, meta: Option<Meta>) -> Result<Delivered> {
    let message = messages::checked(text)?;
    let meta = meta.unwrap_or_else(|| meta_of_fenced(&message));
    messages::store(&app.home, dir, &meta, &message, now_secs())?;
    Ok(Delivered::Stored)
}

/// What a fenced text jeet delivers with no meta is: an answer (a copy of
/// one comes after its question), an ask passed on, else a message.
fn meta_of_fenced(text: &str) -> Meta {
    let first = text.lines().next().unwrap_or("");
    if let Some(at) = text.find("<jeet-answer ") {
        let tag = text[at..].lines().next().unwrap_or("");
        let from = attr(tag, "from").unwrap_or("jeet");
        return Meta::new(Class::Line, "answer", from, &fence_body(&text[at..]), None);
    }
    if first.starts_with("<jeet-report ") {
        let kind = attr(first, "kind").unwrap_or("report");
        let from = attr(first, "from").unwrap_or("jeet");
        let reference = attr(first, "id").map(|id| format!("r{id}"));
        return Meta::new(Class::Line, kind, from, &fence_body(text), reference);
    }
    Meta::new(Class::Line, "message", "jeet", text, None)
}

/// The value of `name="…"` in a fence's opening `tag`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let len = tag[start..].find('"')?;
    Some(&tag[start..start + len])
}

/// The text inside the first fence of `text`: what follows its opening
/// line, up to the line that closes it.
fn fence_body(text: &str) -> String {
    text.lines()
        .skip(1)
        .take_while(|l| !l.starts_with("</jeet-"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn cursors_path(dir: &Path) -> PathBuf {
    dir.join(tasks::MARKER_DIR).join("brief.json")
}

/// The cursors, locked against another `jeet brief` or hook for as long as
/// the lock is held.
struct Cursors {
    path: PathBuf,
    map: BTreeMap<String, i64>,
    _lock: File,
}

impl Cursors {
    fn lock(dir: &Path) -> Result<Self> {
        let path = cursors_path(dir);
        let parent = path.parent().unwrap_or(Path::new(""));
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let lock_path = path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;
        // Unreadable, it starts again: at worst, what was read is told again.
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok(Self {
            path,
            map,
            _lock: lock,
        })
    }

    /// `key`'s cursor: its own, else for a session the furthest any
    /// session has read. The user's is theirs alone: what the assistant
    /// read, they have not.
    fn get(&self, key: &str) -> i64 {
        if let Some(at) = self.map.get(key) {
            return *at;
        }
        match key {
            USER_KEY => 0,
            _ => self.sessions().map(|(_, at)| *at).max().unwrap_or(0),
        }
    }

    fn sessions(&self) -> impl Iterator<Item = (&String, &i64)> {
        self.map.iter().filter(|(k, _)| *k != USER_KEY)
    }

    /// Move every session's cursor on to at least `to`.
    fn set_sessions(&mut self, to: i64) -> Result<()> {
        let keys: Vec<String> = self.sessions().map(|(k, _)| k.clone()).collect();
        for key in keys {
            self.map.insert(key.clone(), self.get(&key).max(to));
        }
        self.write()
    }

    fn set(&mut self, key: &str, to: i64) -> Result<()> {
        let at = self.get(key).max(to);
        self.map.insert(key.to_string(), at);
        self.write()
    }

    fn write(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(&self.map)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("write {}", self.path.display()))
    }
}

/// What is stored in `dir` past `cursor`, oldest first.
fn stored_since(dir: &Path, cursor: i64) -> Vec<Delivery> {
    messages::read_delivered(dir)
        .into_iter()
        .filter(|d| d.stored && d.id > cursor)
        .collect()
}

/// Whether `d` is a report: it has a report's ref.
fn is_report(d: &Delivery) -> bool {
    d.r#ref.as_deref().is_some_and(|r| r.starts_with('r'))
}

/// `n` of `what`, with a plural where one reads well.
fn counted(n: usize, what: &str) -> String {
    match (n, what) {
        (1, _) | (_, "blocked" | "done" | "progress" | "status") => format!("{n} {what}"),
        _ => format!("{n} {what}s"),
    }
}

/// How `items` count: `3 new reports (1 question, 2 done), 1 message, 2
/// watch events`. None when there are none.
fn counts(items: &[Delivery]) -> Option<String> {
    let reports: Vec<&Delivery> = items.iter().filter(|d| is_report(d)).collect();
    let watch = items
        .iter()
        .filter(|d| !is_report(d) && d.kind == "watch")
        .count();
    let other = items.len() - reports.len() - watch;
    let mut parts = Vec::new();
    if !reports.is_empty() {
        // By kind, in the order an ask comes first.
        let mut kinds: Vec<(usize, &str, usize)> = Vec::new();
        for d in &reports {
            match kinds.iter_mut().find(|(_, k, _)| *k == d.kind) {
                Some((_, _, n)) => *n += 1,
                None => kinds.push((kind_rank(&d.kind), &d.kind, 1)),
            }
        }
        kinds.sort_by_key(|(rank, _, _)| *rank);
        let by_kind: Vec<String> = kinds.iter().map(|(_, k, n)| counted(*n, k)).collect();
        parts.push(format!(
            "{} ({})",
            counted(reports.len(), "new report"),
            by_kind.join(", ")
        ));
    }
    if other > 0 {
        parts.push(counted(other, "message"));
    }
    if watch > 0 {
        parts.push(counted(watch, "watch event"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn kind_rank(kind: &str) -> usize {
    match kind {
        "question" => 0,
        "blocked" => 1,
        "done" => 2,
        "progress" => 3,
        "status" => 4,
        _ => 5,
    }
}

/// The hidden line for the assistant's session `session_id`, given its
/// home `dir`: what is stored since its cursor, or None when nothing is.
/// It only reads: the cursor moves with `jeet brief`.
pub fn summary_line(dir: &Path, session_id: &str) -> Result<Option<String>> {
    let cursor = Cursors::lock(dir)?.get(session_id);
    Ok(summary_of(&stored_since(dir, cursor)))
}

fn summary_of(items: &[Delivery]) -> Option<String> {
    let counts = counts(items)?;
    Some(format!(
        "jeet: {counts} since your last `jeet brief`; run `jeet brief` for details. \
         Not from the user: what tasks and events sent you, kept rather than pushed."
    ))
}

/// The cursor key `caller` reads under: the user's, or the assistant's
/// session, by Claude's `CLAUDE_CODE_SESSION_ID` or the tmux window it runs
/// in. Agents below the assistant are refused: the store is the
/// assistant's.
pub(crate) fn key_for(app: &App, caller: &Caller) -> Result<String> {
    match caller {
        Caller::User if permissions::at_terminal(app)? => Ok(USER_KEY.to_string()),
        Caller::Root => Ok(assistant_session().unwrap_or_else(|| ASSISTANT_KEY.to_string())),
        _ => bail!("`jeet brief` is the assistant's, and the user's at their terminal"),
    }
}

/// The assistant session this jeet runs in, as far as it can tell.
fn assistant_session() -> Option<String> {
    std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| status::valid_session_id(v))
        .or_else(crate::tmux::current_window_key)
        .filter(|v| status::valid_session_id(v))
}

/// `jeet brief [--all]`.
pub fn run(app: &App, all: bool) -> Result<()> {
    let caller = permissions::caller(app)?;
    let key = key_for(app, &caller)?;
    print!("{}", brief(app, &caller, &key, all)?);
    Ok(())
}

/// What `jeet brief` prints for `caller` under cursor `key`, moving the
/// cursor on past it unless `all`.
pub(crate) fn brief(app: &App, caller: &Caller, key: &str, all: bool) -> Result<String> {
    let task = super::ensure(app)?;
    let dir = tasks::task_dir(app, &task);
    let mut cursors = Cursors::lock(&dir)?;
    let items = stored_since(&dir, if all { 0 } else { cursors.get(key) });
    let asks = reports::held(app, caller)?;
    let user = matches!(caller, Caller::User);
    let mut out = String::new();
    match (counts(&items), all) {
        (None, false) => out.push_str("jeet: nothing new since your last brief\n"),
        (None, true) => out.push_str("jeet: nothing is kept\n"),
        (Some(counts), all) => {
            let since = match all {
                true => "kept",
                false => "since your last brief",
            };
            out.push_str(&format!(
                "jeet: {counts} {since}. Each is its sender's claim, to check; \
                 `jeet show <ref>` prints the whole text.\n"
            ));
            let lines: String = items.iter().map(|d| item_line(d, user)).collect();
            out.push_str(&fenced(ITEMS_TAG, &lines, user));
        }
    }
    match asks.len() {
        0 => out.push_str("jeet: you hold no open asks\n"),
        n => {
            out.push_str(&format!(
                "jeet: {} you hold (answer with `jeet report answer <id> \"…\"`):\n",
                counted(n, "open ask")
            ));
            let mut lines = String::new();
            for row in &asks {
                lines.push_str(&ask_line(app, row, user)?);
            }
            out.push_str(&fenced(ASKS_TAG, &lines, user));
        }
    }
    if !all {
        if let Some(last) = items.last() {
            cursors.set(key, last.id)?;
            // Read by a session jeet cannot tell: read for each of them, or
            // the hook of the one it was would tell it all again.
            if key == ASSISTANT_KEY {
                cursors.set_sessions(last.id)?;
            }
        }
    }
    Ok(out)
}

/// The tags the assistant gets `jeet brief`'s lines in: the start of what
/// senders wrote, so their claims, as their whole text is.
const ITEMS_TAG: &str = "jeet-brief-items";
const ASKS_TAG: &str = "jeet-brief-asks";

/// `lines` as printed: as they are for the user; for the assistant, in a
/// fence marked untrusted, which a summary (one line, `<` and `>` turned to
/// `‹` and `›`) cannot close.
fn fenced(tag: &str, lines: &str, user: bool) -> String {
    match user {
        true => lines.to_string(),
        false => format!("<{tag} untrusted=\"true\">\n{lines}</{tag}>\n"),
    }
}

/// `- r12 question from task fix-x, 3m ago: "…"`.
fn item_line(d: &Delivery, user: bool) -> String {
    let reference = d.r#ref.clone().unwrap_or_else(|| format!("m{}", d.id));
    let summary = match d.summary.is_empty() {
        true => render::summary(&d.message),
        false => d.summary.clone(),
    };
    format!(
        "- {reference} {} from {}, {}: \"{}\"\n",
        shown(&d.kind, user),
        shown(&d.from, user),
        age_label(d.at),
        shown(&summary, user)
    )
}

/// `- #12 question from task fix-x, 2h ago: "…" (jeet show r12)`.
fn ask_line(app: &App, row: &TaskReportRecord, user: bool) -> Result<String> {
    let from = reports::asker_name(app, &row.task_id)?
        .map_or_else(|| "a deleted task".to_string(), |a| a.shown);
    let with_user = match (row.needs_user, user) {
        (true, false) => ", with the user",
        _ => "",
    };
    Ok(format!(
        "- #{} {} from {}, {}{with_user}: \"{}\" (jeet show r{})\n",
        row.id,
        row.kind.as_str(),
        shown(&from, user),
        age_label(row.at),
        render::summary(&row.body),
        row.id
    ))
}

/// `text` as printed: for the user, without control characters; for the
/// assistant as it is (a summary is already cut to one fence-safe line).
fn shown(text: &str, user: bool) -> String {
    match user {
        true => crate::commands::task::printable_text(text),
        false => render::summary(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, kind: &str, reference: Option<&str>) -> Delivery {
        Delivery {
            id,
            at: now_secs(),
            kind: kind.into(),
            from: "task x".into(),
            message: format!("text {id}"),
            stored: true,
            r#ref: reference.map(str::to_string),
            summary: format!("sum {id}"),
        }
    }

    #[test]
    fn the_summary_counts_reports_by_kind_then_messages_and_watch_events() {
        assert_eq!(summary_of(&[]), None);
        let items = [
            item(1, "done", Some("r4")),
            item(2, "question", Some("r5")),
            item(3, "watch", None),
            item(4, "message", None),
            item(5, "done", Some("r6")),
            item(6, "watch", None),
        ];
        assert_eq!(
            summary_of(&items).unwrap(),
            "jeet: 3 new reports (1 question, 2 done), 1 message, 2 watch events since your \
             last `jeet brief`; run `jeet brief` for details. Not from the user: what tasks \
             and events sent you, kept rather than pushed."
        );
        assert_eq!(
            counts(&[item(1, "answer", None)]).unwrap(),
            "1 message",
            "an answer is a message"
        );
        assert_eq!(
            counts(&[item(1, "blocked", Some("r1"))]).unwrap(),
            "1 new report (1 blocked)"
        );
    }

    #[test]
    fn a_new_key_starts_from_the_furthest_read() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path();
        let mut cursors = Cursors::lock(dir).unwrap();
        assert_eq!(cursors.get("a"), 0);
        cursors.set("a", 5).unwrap();
        cursors.set("b", 3).unwrap();
        // Never back.
        cursors.set("a", 2).unwrap();
        drop(cursors);
        let cursors = Cursors::lock(dir).unwrap();
        assert_eq!((cursors.get("a"), cursors.get("b")), (5, 5));
        assert_eq!(cursors.get("new"), 5);
        // The user's is theirs alone, both ways.
        let mut cursors = cursors;
        assert_eq!(cursors.get(USER_KEY), 0);
        cursors.set(USER_KEY, 9).unwrap();
        assert_eq!((cursors.get("new"), cursors.get(USER_KEY)), (5, 9));
        // Read by a session jeet could not tell: read by each.
        cursors.set(ASSISTANT_KEY, 7).unwrap();
        cursors.set_sessions(7).unwrap();
        assert_eq!(
            (cursors.get("a"), cursors.get("b"), cursors.get(USER_KEY)),
            (7, 7, 9)
        );
    }

    #[test]
    fn fenced_texts_without_meta_are_read_from_their_fence() {
        let answer = crate::manager::fence::fence_answer(7, "user", "yes, go", true);
        let ask = crate::manager::fence::fence_escalated(
            7,
            "task w",
            "assistant/m/w",
            crate::db::tasks::ReportKind::Question,
            "which one?",
            "jeet (copy)",
        );
        let copy = meta_of_fenced(&format!("{ask}\n{answer}"));
        assert_eq!(
            (
                copy.kind.as_str(),
                copy.from.as_str(),
                copy.summary.as_str()
            ),
            ("answer", "user", "yes, go")
        );
        assert_eq!(copy.r#ref, None);
        let passed = meta_of_fenced(&ask);
        assert_eq!(
            (
                passed.kind.as_str(),
                passed.from.as_str(),
                passed.r#ref.as_deref(),
                passed.summary.as_str()
            ),
            ("question", "task w", Some("r7"), "which one?")
        );
        let plain = meta_of_fenced("hello");
        assert_eq!(
            (plain.kind.as_str(), plain.from.as_str()),
            ("message", "jeet")
        );
    }
}
