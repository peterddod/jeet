//! What a node may spend: busy time and turns. A budget is written as parts,
//! `4h`, `30m` or `20turns`, joined by commas (`1h,20turns`); a change to one
//! is `+` or `=` before them (`+1h`, `=2h,40turns`). Cost budgets wait on
//! #26.

use anyhow::{bail, Result};

use crate::context::App;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{BudgetRow, RunState, TaskRecord};
use crate::messages::{Inbox, Sender};
use crate::permissions::Caller;
use crate::reports::{self, StatusEvent};
use crate::review;
use crate::tasks;

/// A node's caps; `None` is no cap on that unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub secs: Option<i64>,
    pub turns: Option<i64>,
}

/// One part of a budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetPart {
    Secs(i64),
    Turns(i64),
}

/// A change to a budget: raise each unit given by so much, or set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add(Limits),
    Set(Limits),
}

const FORMS: &str = "hours (4h), minutes (30m) or turns (20turns)";

/// One part: `<n>h`, `<n>m` or `<n>turns`, with `n` at least 1. A `$`
/// amount is refused: cost budgets wait on #26.
pub fn parse_part(s: &str) -> Result<BudgetPart, String> {
    let s = s.trim();
    if s.contains('$') {
        return Err(format!("{s}: cost budgets wait on #26; give {FORMS}"));
    }
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let per = match unit {
        "h" => 3600,
        "m" => 60,
        "turns" => 1,
        _ => return Err(format!("{s}: not a budget; give {FORMS}")),
    };
    let n: i64 = match digits.parse() {
        Ok(n) => n,
        Err(_) => return Err(format!("{s}: not a budget; give {FORMS}")),
    };
    if n == 0 {
        return Err(format!("{s}: a budget of 0 allows nothing"));
    }
    let Some(n) = n.checked_mul(per) else {
        return Err(format!("{s}: too large"));
    };
    match unit {
        "turns" => Ok(BudgetPart::Turns(n)),
        _ => Ok(BudgetPart::Secs(n)),
    }
}

/// The caps `parts` give, refusing a unit given twice (`1h,30m`).
pub fn limits(parts: &[BudgetPart]) -> Result<Limits> {
    let mut out = Limits::default();
    for part in parts {
        let (slot, n, unit) = match *part {
            BudgetPart::Secs(n) => (&mut out.secs, n, "time"),
            BudgetPart::Turns(n) => (&mut out.turns, n, "turns"),
        };
        if slot.is_some() {
            bail!("the budget gives {unit} twice");
        }
        *slot = Some(n);
    }
    Ok(out)
}

/// Comma-separated parts, as in config and on the command line.
pub fn parse_limits(s: &str) -> Result<Limits, String> {
    let parts = s
        .split(',')
        .map(parse_part)
        .collect::<Result<Vec<_>, _>>()?;
    limits(&parts).map_err(|e| format!("{s}: {e}"))
}

/// A change to a budget: `+` raises each unit given by so much (`+1h`,
/// `+50turns`), `=` sets it (`=2h`).
pub fn parse_change(s: &str) -> Result<Change, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        return parse_limits(rest).map(Change::Add);
    }
    if let Some(rest) = s.strip_prefix('=') {
        return parse_limits(rest).map(Change::Set);
    }
    Err(format!(
        "{s}: start a change with + to raise the budget (+1h) or = to set it (=2h)"
    ))
}

/// `limits` as `--budget` takes them: `2h,40turns`; `none` for no cap.
pub fn label(limits: Limits) -> String {
    let mut parts = Vec::new();
    if let Some(secs) = limits.secs {
        parts.push(match secs {
            s if s % 3600 == 0 => format!("{}h", s / 3600),
            s if s % 60 == 0 => format!("{}m", s / 60),
            s => format!("{s}s"),
        });
    }
    if let Some(turns) = limits.turns {
        parts.push(format!("{turns}turns"));
    }
    match parts.is_empty() {
        true => "none".into(),
        false => parts.join(","),
    }
}

/// `change` as `jeet manager budget` takes it: `+1h`, `=2h,40turns`.
pub fn change_label(change: Change) -> String {
    match change {
        Change::Add(limits) => format!("+{}", label(limits)),
        Change::Set(limits) => format!("={}", label(limits)),
    }
}

/// `had` once `change` is made: `+` raises each limit it gives, which
/// must be there to raise, `=` sets each; a unit it does not give is kept.
pub fn changed(had: Limits, change: Change) -> Result<Limits> {
    match change {
        Change::Set(set) => Ok(Limits {
            secs: set.secs.or(had.secs),
            turns: set.turns.or(had.turns),
        }),
        Change::Add(add) => {
            let raise = |had: Option<i64>, add: Option<i64>, unit: &str| match (had, add) {
                (_, None) => Ok(had),
                (Some(had), Some(add)) => Ok(Some(had.saturating_add(add))),
                (None, Some(_)) => bail!("there is no {unit} limit to raise; set one with `=`"),
            };
            Ok(Limits {
                secs: raise(had.secs, add.secs, "time")?,
                turns: raise(had.turns, add.turns, "turn")?,
            })
        }
    }
}

/// What is left to spend under `under` (M19): for each unit, the least,
/// over it and each ancestor with a limit, of that limit less what its
/// subtree has used, never below 0. `None` where nothing above limits that
/// unit; the assistant has no budget of its own.
pub fn remaining(app: &App, under: &TaskRecord) -> Result<Limits> {
    let mut left = Limits::default();
    let mut chain = vec![under.clone()];
    chain.extend(app.db.ancestors(&under.id)?);
    for node in chain {
        let Some(row) = app.db.budget(&node.id)? else {
            continue;
        };
        if row.max_secs.is_none() && row.max_turns.is_none() {
            continue;
        }
        let (secs, turns) = app.db.subtree_used(&node.id)?;
        left.secs = least(left.secs, row.max_secs.map(|max| (max - secs).max(0)));
        left.turns = least(left.turns, row.max_turns.map(|max| (max - turns).max(0)));
    }
    Ok(left)
}

/// `want` as `target`'s new limits in place of `had`, set by `by`, or why
/// it may not have them (M19): in neither unit may it be left more to
/// spend than is left above it, nor, for a node setting it, than the node
/// has left itself. No limit in a unit is always allowed: the ones above
/// still apply. Nor is a unit kept as it was, or lowered, checked: what
/// siblings spent may leave less above than a limit given before, and
/// that must not hold back a change to the other unit, or a cut.
pub fn cap(
    app: &App,
    target: &TaskRecord,
    by: &Caller,
    had: Limits,
    want: Limits,
) -> Result<Limits> {
    let parent = match &target.parent_id {
        Some(id) => app.db.get_task(id)?,
        None => None,
    };
    let mut left = match &parent {
        Some(parent) => remaining(app, parent)?,
        None => Limits::default(),
    };
    if let Caller::Node(me) = by {
        let mine = remaining(app, me)?;
        left.secs = least(left.secs, mine.secs);
        left.turns = least(left.turns, mine.turns);
    }
    // What it has used, and what is left above it.
    let (secs, turns) = app.db.subtree_used(&target.id)?;
    let room = Limits {
        secs: left.secs.map(|left| left.saturating_add(secs)),
        turns: left.turns.map(|left| left.saturating_add(turns)),
    };
    let fits = |had: Option<i64>, want: Option<i64>, room: Option<i64>| match (had, want, room) {
        (Some(had), Some(want), _) if want <= had => true,
        (_, Some(want), Some(room)) => want <= room,
        _ => true,
    };
    if !fits(had.secs, want.secs, room.secs) || !fits(had.turns, want.turns, room.turns) {
        bail!(
            "{} is more than {} may have: {}, what it has used and what is left above it",
            label(want),
            super::label(target),
            room_label(room)
        );
    }
    Ok(want)
}

/// `room` as limits `--budget` takes back: time down to whole minutes,
/// as it is charged by the second; a unit with none of that left is said
/// so, as `0m` is no limit to give.
fn room_label(room: Limits) -> String {
    let secs = room.secs.map(|secs| secs / 60 * 60);
    let some = Limits {
        secs: secs.filter(|&secs| secs > 0),
        turns: room.turns.filter(|&turns| turns > 0),
    };
    let mut parts = Vec::new();
    if some.secs.is_some() || some.turns.is_some() {
        parts.push(format!("at most {}", label(some)));
    }
    if secs == Some(0) {
        parts.push("no time".to_string());
    }
    if room.turns == Some(0) {
        parts.push("no turns".to_string());
    }
    parts.join(" and ")
}

/// The budget threshold a check newly crossed (M21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Threshold {
    None,
    /// 80% of a limit used, by the node or above it.
    Warn80,
    /// A limit used up: the task it is on is paused.
    Hit100,
}

/// Check the budgets `node`'s use counts toward, its own and each
/// ancestor's, once it has been charged (M21). At 80% of a limit, the task
/// it is on is told, through its inbox, and its parent gets a `status`
/// report. At 100% the task is paused, with `budget` as the setter, which
/// leaves a pause or stop by anyone else as it is; above `node`, its parent
/// gets a `status` report, while `node`'s own Stop hook reports it. Each
/// threshold is told once until the limit changes. Interrupts nothing:
/// what runs finishes its turn and then gets nothing new. Returns
/// `Hit100` when this check newly holds `node` by a budget, for its
/// parent to be told it is blocked ([`report_blocked`]): its own used up,
/// or one above it while nothing held it already (a node its own budget
/// paused, and reported, is not reported again when one above follows).
/// Otherwise `Warn80` when an 80% was newly crossed, else `None`.
pub fn enforce(app: &App, node: &TaskRecord) -> Result<Threshold> {
    let held_before = super::run::hold(app, node)?.is_some();
    let mut crossed = Threshold::None;
    for task in &counted_toward(app, node)? {
        // Read, paused and claimed in one go: a limit changed meanwhile is
        // checked as it now is, and only one of two checks at once tells.
        let check = app
            .db
            .check_budget(&task.id, |row, used| match percent(row, used) {
                Some(pct) if pct >= 100 => 100,
                Some(pct) if pct >= 80 => 80,
                _ => 0,
            })?;
        let Some(check) = check.filter(|c| c.newly) else {
            continue;
        };
        let (row, used) = (&check.row, check.used);
        if check.level >= 100 {
            if task.id == node.id || !held_before {
                crossed = crossed.max(Threshold::Hit100);
            }
            // Not of one being stopped: a stop's own charge would report it
            // paused, and a stopped node's end is not reported (M18).
            if task.id != node.id && !stopped(app, task)? {
                tell_parent(
                    app,
                    task,
                    &format!("budget used up ({}); paused", usage(row, used)),
                );
            }
        } else {
            crossed = crossed.max(Threshold::Warn80);
            let pct = percent(row, used).unwrap_or(80);
            let usage = usage(row, used);
            tell_parent(app, task, &format!("budget {pct}% used ({usage})"));
            let note = format!(
                "jeet: your budget is {pct}% used ({usage}). At 100% you are paused; wrap up, \
                 or report to ask for more."
            );
            let told = Inbox::lock(&tasks::task_dir(app, task))
                .and_then(|inbox| inbox.push_to(&note, None, Sender::Agent));
            if let Err(e) = told {
                eprintln!("jeet: could not tell task {} its budget: {e:#}", task.slug);
            }
        }
    }
    Ok(crossed)
}

/// The nodes whose budgets what `node` uses counts toward: it and its
/// ancestors.
fn counted_toward(app: &App, node: &TaskRecord) -> Result<Vec<TaskRecord>> {
    let mut chain = vec![node.clone()];
    chain.extend(app.db.ancestors(&node.id)?);
    Ok(chain)
}

/// Check review `review`'s `[review]` limits once it has been charged, as
/// [`enforce`] checks a node's budget: at 80% of a limit it and whoever
/// asked for it are told, and at 100% it is paused until it is asked
/// again. Each threshold is told once until the limits change. Interrupts
/// nothing: what runs finishes its turn and then gets nothing new. Returns
/// `Hit100` when this check newly paused it, for whoever asked to be told
/// it is blocked ([`report_blocked_review`]), `Warn80` when an 80% was
/// newly crossed, else `None`.
pub fn enforce_review(app: &App, review: &ReviewRecord) -> Result<Threshold> {
    let check = app
        .db
        .check_review_budget(&review.id, |row, used| match percent(row, used) {
            Some(pct) if pct >= 100 => 100,
            Some(pct) if pct >= 80 => 80,
            _ => 0,
        })?;
    let Some(check) = check.filter(|c| c.newly) else {
        return Ok(Threshold::None);
    };
    if check.level >= 100 {
        return Ok(Threshold::Hit100);
    }
    let pct = percent(&check.row, check.used).unwrap_or(80);
    let usage = usage(&check.row, check.used);
    tell_asker(app, review, &format!("budget {pct}% used ({usage})"));
    let note = format!(
        "jeet: this review is {pct}% of its [review] limits ({usage}). At 100% you are paused; \
         wrap up."
    );
    let told = Inbox::lock(&review::dir(app, review))
        .and_then(|inbox| inbox.push_to(&note, None, Sender::Agent));
    if let Err(e) = told {
        eprintln!(
            "jeet: could not tell review {} its limits: {e:#}",
            review.slug
        );
    }
    Ok(Threshold::Warn80)
}

/// Tell whoever asked for `review` that its `[review]` limits are used up
/// and it is paused (see [`report_blocked`]).
pub fn report_blocked_review(app: &App, review: &ReviewRecord) {
    let Ok(Some(fresh)) = app.db.get_review(&review.id) else {
        return;
    };
    let used = (fresh.used_secs, fresh.used_turns);
    let body = format!(
        "budget used up ({}); blocked: budget. Paused until it is asked for again \
         (`jeet review --rerun`)",
        usage(&fresh.budget(), used)
    );
    tell_asker(app, &fresh, &body);
}

/// A `status` report about `review` to whoever asked for it; what goes
/// wrong is warned of, not failed on.
fn tell_asker(app: &App, review: &ReviewRecord, body: &str) {
    if let Err(e) = reports::auto_status_review(app, review, StatusEvent::Budget, body, i64::MAX, 0)
    {
        eprintln!(
            "jeet: could not report review {}'s limits: {e:#}",
            review.slug
        );
    }
}

/// Whether `task` is stopped, itself or above.
fn stopped(app: &App, task: &TaskRecord) -> Result<bool> {
    Ok(app.db.effective_run_state(&task.id)?.0 == RunState::Stopped)
}

/// Tell `node`'s parent that a budget it counts toward is used up and it
/// is paused (M21): what [`enforce`] leaves to the node's own Stop hook,
/// or to the sweep that charged it. `blocked: budget`, naming whose budget
/// it was, its own or an ancestor's.
pub fn report_blocked(app: &App, node: &TaskRecord) {
    match blocked_body(app, node) {
        Ok(body) => tell_parent(app, node, &body),
        Err(e) => eprintln!("jeet: could not report task {}'s budget: {e:#}", node.slug),
    }
}

/// `budget used up (…); blocked: budget, …`.
fn blocked_body(app: &App, node: &TaskRecord) -> Result<String> {
    let mut whose = String::new();
    for task in &counted_toward(app, node)? {
        let Some(row) = app.db.budget(&task.id)? else {
            continue;
        };
        let used = app.db.subtree_used(&task.id)?;
        if percent(&row, used).is_some_and(|pct| pct >= 100) {
            whose = match task.id == node.id {
                true => format!(" ({})", usage(&row, used)),
                false => format!(" under {} ({})", super::label(task), usage(&row, used)),
            };
            break;
        }
    }
    Ok(format!(
        "budget used up{whose}; blocked: budget. Paused until the budget is raised \
         (`jeet manager budget`) and it is resumed"
    ))
}

/// The most of any limit in `row` that `used` (seconds, turns) is, in per
/// cent; `None` with no limit.
pub fn percent(row: &BudgetRow, (secs, turns): (i64, i64)) -> Option<i64> {
    let of = |used: i64, max: Option<i64>| max.map(|max| used.saturating_mul(100) / max.max(1));
    of(secs, row.max_secs).max(of(turns, row.max_turns))
}

/// `48m of 1h, 16 of 20turns`.
pub fn usage(row: &BudgetRow, (secs, turns): (i64, i64)) -> String {
    let mut parts = Vec::new();
    if let Some(max) = row.max_secs {
        let max = label(Limits {
            secs: Some(max),
            turns: None,
        });
        parts.push(format!("{}m of {max}", secs / 60));
    }
    if let Some(max) = row.max_turns {
        parts.push(format!("{turns} of {max}turns"));
    }
    parts.join(", ")
}

/// A `status` report about `task` to its parent; what goes wrong is warned
/// of, not failed on. `task_budgets.warned` says what was told already, so
/// no earlier report holds this one back: none was sent at or after
/// `i64::MAX`.
fn tell_parent(app: &App, task: &TaskRecord, body: &str) {
    if let Err(e) = reports::auto_status(app, task, StatusEvent::Budget, body, i64::MAX, 0) {
        eprintln!("jeet: could not report task {}'s budget: {e:#}", task.slug);
    }
}

fn least(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::{RunState, Setter, TaskKind, TaskState};
    use crate::db::Database;
    use crate::messages;

    /// An index in a tempdir with the assistant, manager `m` under it and
    /// worker `w` under `m`.
    fn lab() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: config::load_or_create(&home).unwrap(),
            db: Database::open(&config::index_path(&home)).unwrap(),
            home,
        };
        let root = super::super::root(&app).unwrap();
        for (slug, kind, parent) in [
            ("m", TaskKind::Manager, root.id.as_str()),
            ("w", TaskKind::Work, "m"),
        ] {
            app.db
                .insert_task(&TaskRecord {
                    id: slug.into(),
                    slug: slug.into(),
                    name: slug.into(),
                    project: None,
                    kind,
                    state: TaskState::Active,
                    created_at: 0,
                    last_activity: 0,
                    parent_id: Some(parent.into()),
                    placement: crate::placement::for_new(),
                    run_state: RunState::Run,
                    run_state_by: None,
                    max_children: None,
                })
                .unwrap();
        }
        (dir, app)
    }

    fn get(app: &App, id: &str) -> TaskRecord {
        app.db.get_task(id).unwrap().unwrap()
    }

    /// The bodies of jeet's budget reports about `id`.
    fn budget_reports(app: &App, id: &str) -> Vec<String> {
        let conn = rusqlite::Connection::open(config::index_path(&app.home)).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT body FROM task_reports WHERE task_id = ?1 AND kind = 'status' ORDER BY id",
            )
            .unwrap();
        stmt.query_map([id], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|b| b.starts_with("budget"))
            .collect()
    }

    fn inbox(app: &App, id: &str) -> Vec<String> {
        let dir = tasks::task_dir(app, &get(app, id));
        let queued = messages::Inbox::lock(&dir).unwrap().read().unwrap();
        queued.into_iter().map(|q| q.message).collect()
    }

    #[test]
    fn eighty_per_cent_is_told_once_and_a_hundred_pauses() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", None, Some(100)).unwrap();
        let w = get(&app, "w");
        app.db.charge("w", 0, 79).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::None);
        assert!(budget_reports(&app, "m").is_empty());

        app.db.charge("w", 0, 1).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::Warn80);
        assert_eq!(
            budget_reports(&app, "m"),
            ["budget 80% used (80 of 100turns)"]
        );
        let note = inbox(&app, "m");
        assert_eq!(note.len(), 1);
        assert!(
            note[0].starts_with("jeet: your budget is 80% used"),
            "{note:?}"
        );
        assert_eq!(app.db.budget("m").unwrap().unwrap().warned, 80);

        // Not again, an idle report about it in between or not.
        crate::reports::auto_status(&app, &get(&app, "m"), StatusEvent::Idle, "idle 9m", 1, 0)
            .unwrap();
        app.db.charge("w", 0, 5).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::None);
        assert_eq!(budget_reports(&app, "m").len(), 1);
        assert_eq!(inbox(&app, "m").len(), 1);
        assert_eq!(get(&app, "m").run_state, RunState::Run);

        // At 100% the task above is paused, inline, by its budget, and its
        // parent told.
        app.db.charge("w", 0, 15).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::Hit100);
        let m = get(&app, "m");
        assert_eq!(
            (m.run_state, m.run_state_by),
            (RunState::Paused, Some(Setter::Budget))
        );
        assert_eq!(
            budget_reports(&app, "m")[1],
            "budget used up (100 of 100turns); paused"
        );
        assert_eq!(get(&app, "w").run_state, RunState::Run);
        // Told once.
        app.db.charge("w", 0, 1).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::None);
        assert_eq!(budget_reports(&app, "m").len(), 2);
    }

    #[test]
    fn a_node_at_its_own_limit_is_paused_and_left_to_its_stop_hook_to_report() {
        let (_dir, app) = lab();
        app.db.set_budget_max("w", Some(600), None).unwrap();
        app.db.set_budget_max("m", Some(3600), None).unwrap();
        app.db.charge("w", 600, 1).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Hit100);
        let w = get(&app, "w");
        assert_eq!(
            (w.run_state, w.run_state_by),
            (RunState::Paused, Some(Setter::Budget))
        );
        assert!(budget_reports(&app, "w").is_empty());
        // m, at 16%, is left alone.
        assert_eq!(get(&app, "m").run_state, RunState::Run);
        assert!(budget_reports(&app, "m").is_empty());
    }

    #[test]
    fn one_charge_spending_a_node_and_its_parent_pauses_both() {
        let (_dir, app) = lab();
        app.db.set_budget_max("w", None, Some(10)).unwrap();
        app.db.set_budget_max("m", None, Some(10)).unwrap();
        app.db.charge("w", 0, 10).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Hit100);
        for id in ["w", "m"] {
            let t = get(&app, id);
            assert_eq!(
                (t.run_state, t.run_state_by),
                (RunState::Paused, Some(Setter::Budget)),
                "{id}"
            );
        }
        // w's own Stop hook reports it; m is reported here.
        assert!(budget_reports(&app, "w").is_empty());
        assert_eq!(
            budget_reports(&app, "m"),
            ["budget used up (10 of 10turns); paused"]
        );
    }

    #[test]
    fn a_node_its_own_budget_held_is_not_blocked_again_by_one_above() {
        let (_dir, app) = lab();
        app.db.set_budget_max("w", None, Some(10)).unwrap();
        app.db.set_budget_max("m", None, Some(15)).unwrap();
        app.db.charge("w", 0, 10).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Hit100);
        // The rest of its turn uses m's up: m's parent is told of m, but w,
        // reported blocked already, is not reported again.
        app.db.charge("w", 0, 5).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::None);
        assert_eq!(
            (get(&app, "m").run_state, get(&app, "m").run_state_by),
            (RunState::Paused, Some(Setter::Budget))
        );
        assert_eq!(
            budget_reports(&app, "m"),
            ["budget used up (15 of 15turns); paused"]
        );
    }

    #[test]
    fn a_spent_budget_leaves_a_users_stop_as_it_is() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", None, Some(10)).unwrap();
        app.db
            .set_run_state("m", RunState::Stopped, Some(&Setter::User))
            .unwrap();
        app.db.charge("w", 0, 10).unwrap();
        // w, held by m's stop already, is not newly blocked by the budget.
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::None);
        let m = get(&app, "m");
        assert_eq!(
            (m.run_state, m.run_state_by),
            (RunState::Stopped, Some(Setter::User))
        );
        // Nor is m, being stopped, reported paused by its budget.
        assert!(budget_reports(&app, "m").is_empty());
        // Nor a user's pause.
        app.db.set_run_state("m", RunState::Run, None).unwrap();
        app.db
            .set_run_state("m", RunState::Paused, Some(&Setter::User))
            .unwrap();
        app.db.set_warned("m", 0).unwrap();
        enforce(&app, &get(&app, "w")).unwrap();
        assert_eq!(get(&app, "m").run_state_by, Some(Setter::User));
    }

    #[test]
    fn a_new_limit_tells_the_thresholds_again() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", Some(3600), None).unwrap();
        app.db.charge("w", 2880, 0).unwrap();
        let w = get(&app, "w");
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::Warn80);
        assert_eq!(budget_reports(&app, "m"), ["budget 80% used (48m of 1h)"]);
        // `+1h`: 48m of 2h is under 80%, and told again once it is not.
        app.db.set_budget_max("m", Some(7200), None).unwrap();
        assert_eq!(app.db.budget("m").unwrap().unwrap().warned, 0);
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::None);
        app.db.charge("w", 3000, 0).unwrap();
        assert_eq!(enforce(&app, &w).unwrap(), Threshold::Warn80);
        assert_eq!(budget_reports(&app, "m").len(), 2);
    }

    /// A review the assistant asked for, with `[review]`'s turns capped.
    fn add_review(app: &App, turns: i64) -> ReviewRecord {
        let root = super::super::root(app).unwrap();
        let mut r = ReviewRecord::new("r".into(), "r".into(), "github.com/acme/api", 7, 0);
        r.requested_by = Some(root.id);
        app.db.insert_review(&r).unwrap();
        std::fs::create_dir_all(review::dir(app, &r)).unwrap();
        app.db
            .set_review_budget_max("r", None, Some(turns))
            .unwrap();
        r
    }

    /// A review spends apart from the budgets of the tree, and they apart
    /// from it: only its own limits hold it, it tells whoever asked for it
    /// at 80%, and at 100% it is paused until it is asked again.
    #[test]
    fn a_review_is_held_to_its_own_limits_and_tells_the_one_who_asked() {
        let (_dir, app) = lab();
        let r = add_review(&app, 5);
        app.db.set_budget_max("m", None, Some(10)).unwrap();
        app.db.charge_review("r", 600, 4).unwrap();
        assert_eq!(app.db.subtree_used("m").unwrap(), (0, 0));
        assert_eq!(enforce_review(&app, &r).unwrap(), Threshold::Warn80);
        assert_eq!(app.db.budget("m").unwrap().unwrap().warned, 0);
        let root = super::super::root(&app).unwrap();
        let told = app.db.reports_for("r", 10).unwrap();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].to_id, root.id);
        assert!(told[0].body.starts_with("budget 80% used"), "{told:?}");
        // Not again.
        assert_eq!(enforce_review(&app, &r).unwrap(), Threshold::None);

        // m spent by w: r runs on, and is held only by its own limits.
        app.db.charge("w", 0, 10).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Hit100);
        app.db.set_run_state("m", RunState::Run, None).unwrap();
        assert!(!app.db.get_review("r").unwrap().unwrap().paused);
        app.db.charge_review("r", 0, 1).unwrap();
        assert_eq!(enforce_review(&app, &r).unwrap(), Threshold::Hit100);
        assert!(app.db.get_review("r").unwrap().unwrap().paused);
        report_blocked_review(&app, &r);
        let told = app.db.reports_for("r", 10).unwrap();
        assert!(
            told[0].body.starts_with("budget used up (5 of 5turns)"),
            "{told:?}"
        );
    }

    #[test]
    fn a_limit_is_capped_by_what_is_left_above_and_the_setters_own() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", Some(3600), Some(20)).unwrap();
        app.db.charge("w", 1200, 5).unwrap();
        app.db.charge("m", 600, 0).unwrap();
        let w = get(&app, "w");
        let want = |b: &str| parse_limits(b).unwrap();
        let none = Limits::default();
        // m has 30m left; w has used 20m of it, so may have 50m.
        assert_eq!(
            cap(&app, &w, &Caller::Root, none, want("50m")).unwrap(),
            want("50m")
        );
        let err = cap(&app, &w, &Caller::User, none, want("51m")).unwrap_err();
        assert!(err.to_string().contains("at most 50m,20turns"), "{err}");
        assert!(cap(&app, &w, &Caller::Root, none, want("21turns")).is_err());
        // No limit in a unit is no more than what is above allows.
        assert_eq!(
            cap(&app, &w, &Caller::Root, none, Limits::default()).unwrap(),
            Limits::default()
        );
        // Under nothing with a limit, anything goes.
        let m = get(&app, "m");
        assert_eq!(
            cap(&app, &m, &Caller::Root, none, want("9h")).unwrap(),
            want("9h")
        );
        // A node setting it is held to what it has left itself.
        let err = cap(&app, &m, &Caller::Node(m.clone()), none, want("2h")).unwrap_err();
        assert!(err.to_string().contains("at most 1h,20turns"), "{err}");
    }

    #[test]
    fn a_refusal_gives_a_limit_the_budget_takes_back() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", Some(3600), None).unwrap();
        // m's subtree has used 20m34s of its 1h, w 20m of that: w may
        // have 59m26s, which charges by the second leave.
        app.db.charge("w", 1200, 0).unwrap();
        app.db.charge("m", 34, 0).unwrap();
        let w = get(&app, "w");
        let want = |b: &str| parse_limits(b).unwrap();
        let none = Limits::default();
        let err = cap(&app, &w, &Caller::User, none, want("2h")).unwrap_err();
        assert!(err.to_string().contains(": at most 59m, what"), "{err}");
        // Down to the minute, what is shown is allowed.
        assert!(cap(&app, &w, &Caller::User, none, want("59m")).is_ok());
        // Under a minute, or no turns, left is said so.
        let (_dir, app) = lab();
        app.db.set_budget_max("m", Some(3600), Some(5)).unwrap();
        app.db.charge("m", 3570, 5).unwrap();
        let w = get(&app, "w");
        let err = cap(&app, &w, &Caller::User, none, want("1m,1turns")).unwrap_err();
        assert!(
            err.to_string().contains(": no time and no turns, what"),
            "{err}"
        );
    }

    #[test]
    fn a_kept_or_lowered_unit_is_not_held_to_what_siblings_left() {
        let (_dir, app) = lab();
        // m's 100 turns, 80 of them given to w, while m (as a sibling of w
        // would) has spent 50: 40 are left above w, which used 10.
        app.db.set_budget_max("m", None, Some(100)).unwrap();
        app.db.set_budget_max("w", None, Some(80)).unwrap();
        app.db.charge("m", 0, 50).unwrap();
        app.db.charge("w", 0, 10).unwrap();
        let w = get(&app, "w");
        let want = |b: &str| parse_limits(b).unwrap();
        let had = want("80turns");
        // A time limit, keeping its turns.
        assert_eq!(
            cap(&app, &w, &Caller::User, had, want("2h,80turns")).unwrap(),
            want("2h,80turns")
        );
        // A cut, still above the room.
        assert_eq!(
            cap(&app, &w, &Caller::User, had, want("60turns")).unwrap(),
            want("60turns")
        );
        // A raise is still held to it.
        let err = cap(&app, &w, &Caller::User, had, want("81turns")).unwrap_err();
        assert!(err.to_string().contains("at most 50turns"), "{err}");
        // As is a limit it had none of.
        assert!(cap(&app, &w, &Caller::User, Limits::default(), want("51turns")).is_err());
    }

    #[test]
    fn a_threshold_is_claimed_once_and_checked_against_the_limit_as_it_is() {
        let (_dir, app) = lab();
        app.db.set_budget_max("m", None, Some(10)).unwrap();
        app.db.charge("w", 0, 10).unwrap();
        let level = |row: &BudgetRow, used| match percent(row, used) {
            Some(pct) if pct >= 100 => 100,
            Some(pct) if pct >= 80 => 80,
            _ => 0,
        };
        let first = app.db.check_budget("m", level).unwrap().unwrap();
        assert_eq!((first.level, first.newly), (100, true));
        // A second check at once tells nothing, though it still pauses.
        app.db.set_run_state("m", RunState::Run, None).unwrap();
        let second = app.db.check_budget("m", level).unwrap().unwrap();
        assert_eq!((second.level, second.newly), (100, false));
        assert_eq!(get(&app, "m").run_state, RunState::Paused);
        // A limit raised before a check is checked as raised: no pause, and
        // its thresholds are left to be told.
        app.db.set_run_state("m", RunState::Run, None).unwrap();
        app.db.set_budget_max("m", None, Some(20)).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::None);
        assert_eq!(get(&app, "m").run_state, RunState::Run);
        assert_eq!(app.db.budget("m").unwrap().unwrap().warned, 0);
        app.db.charge("w", 0, 6).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Warn80);
        app.db.charge("w", 0, 4).unwrap();
        assert_eq!(enforce(&app, &get(&app, "w")).unwrap(), Threshold::Hit100);
        assert!(app.db.check_budget("nothing", level).unwrap().is_none());
    }

    #[test]
    fn parses_each_part() {
        assert_eq!(parse_part("4h"), Ok(BudgetPart::Secs(4 * 3600)));
        assert_eq!(parse_part(" 30m "), Ok(BudgetPart::Secs(1800)));
        assert_eq!(parse_part("20turns"), Ok(BudgetPart::Turns(20)));
    }

    #[test]
    fn refuses_cost_zero_and_anything_else() {
        let err = parse_part("$5").unwrap_err();
        assert!(err.contains("#26"), "{err}");
        assert!(parse_part("5$").unwrap_err().contains("#26"));
        assert!(parse_part("0h").unwrap_err().contains("0"));
        assert!(parse_part("0turns").is_err());
        for bad in [
            "",
            "h",
            "4",
            "4d",
            "4 h",
            "-1h",
            "+1h",
            "1.5h",
            "4hours",
            "99999999999999999999h",
        ] {
            assert!(parse_part(bad).is_err(), "{bad}");
        }
        assert!(parse_part("9223372036854775807h")
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn limits_refuse_a_unit_given_twice() {
        assert_eq!(limits(&[]).unwrap(), Limits::default());
        assert_eq!(
            limits(&[BudgetPart::Secs(3600), BudgetPart::Turns(20)]).unwrap(),
            Limits {
                secs: Some(3600),
                turns: Some(20)
            }
        );
        let err = limits(&[BudgetPart::Secs(3600), BudgetPart::Secs(60)]).unwrap_err();
        assert!(err.to_string().contains("time twice"), "{err}");
        let err = limits(&[BudgetPart::Turns(1), BudgetPart::Turns(2)]).unwrap_err();
        assert!(err.to_string().contains("turns twice"), "{err}");
        assert!(parse_limits("1h,30m").is_err());
        assert_eq!(
            parse_limits("1h,20turns"),
            Ok(Limits {
                secs: Some(3600),
                turns: Some(20)
            })
        );
    }

    #[test]
    fn a_change_raises_or_sets_only_the_units_it_gives() {
        let had = Limits {
            secs: Some(3600),
            turns: None,
        };
        let change = |s: &str| changed(had, parse_change(s).unwrap());
        assert_eq!(
            change("+30m").unwrap(),
            Limits {
                secs: Some(5400),
                turns: None
            }
        );
        assert_eq!(
            change("=2h,40turns").unwrap(),
            Limits {
                secs: Some(7200),
                turns: Some(40)
            }
        );
        assert_eq!(
            change("=10turns").unwrap(),
            Limits {
                secs: Some(3600),
                turns: Some(10)
            }
        );
        // No limit is no cap: there is nothing to raise.
        let e = change("+5turns").unwrap_err().to_string();
        assert!(e.contains("no turn limit to raise"), "{e}");
    }

    #[test]
    fn parses_changes() {
        assert_eq!(
            parse_change("+1h"),
            Ok(Change::Add(Limits {
                secs: Some(3600),
                turns: None
            }))
        );
        assert_eq!(
            parse_change("+50turns"),
            Ok(Change::Add(Limits {
                secs: None,
                turns: Some(50)
            }))
        );
        assert_eq!(
            parse_change("=2h,10turns"),
            Ok(Change::Set(Limits {
                secs: Some(7200),
                turns: Some(10)
            }))
        );
        for bad in ["1h", "+", "=", "+$5", "+0h", "+1h,+1h", "=1h,2h", "-1h"] {
            assert!(parse_change(bad).is_err(), "{bad}");
        }
        assert!(parse_change("1h").unwrap_err().contains("start a change"));
    }
}
