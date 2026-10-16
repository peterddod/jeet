//! `jeet show`: the whole text of a report (`r123`) or of a message
//! (`m45`) that a quietly rendered post gave only as a line, limited to what
//! the caller reaches. It reads, and asks no model.

use anyhow::{bail, Result};

use crate::agent::age_label;
use crate::assistant;
use crate::commands::task::printable_text;
use crate::context::App;
use crate::db::tasks::TaskRecord;
use crate::messages;
use crate::permissions::{self, Caller};
use crate::reports;

pub fn run(app: &App, reference: &str) -> Result<()> {
    let caller = permissions::caller(app)?;
    print!("{}", shown(app, &caller, reference)?);
    Ok(())
}

/// `reference` as `caller` is shown it. A reference that is none, is not
/// there, or is out of the caller's reach reads the same: no one learns
/// from the answer what exists.
fn shown(app: &App, caller: &Caller, reference: &str) -> Result<String> {
    let unreached = || anyhow::anyhow!("no {} reaches you", reference.trim());
    if let Caller::Review(_) = caller {
        bail!("a review's agent only reads, with `jeet ls`, `jeet path` and `jeet prs`");
    }
    let reference = reference.trim();
    let id = reference
        .get(1..)
        .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<i64>().ok());
    match (reference.chars().next(), id) {
        (Some('r'), Some(id)) => report(app, caller, id)?.ok_or_else(unreached),
        (Some('m'), Some(id)) => message(app, caller, id)?.ok_or_else(unreached),
        _ => Err(unreached()),
    }
}

/// Whether `caller` reaches `task_id`: the user and the assistant, all of
/// them; a task with a parent, itself and the tasks below it.
fn reaches(app: &App, caller: &Caller, task_id: &str) -> Result<bool> {
    match caller {
        Caller::User | Caller::Root => Ok(true),
        Caller::Node(node) => {
            Ok(task_id == node.id || app.db.is_strict_descendant(task_id, &node.id)?)
        }
        Caller::Review(_) => Ok(false),
    }
}

/// Report `id`: its kind, who from, where, how long ago, then its text:
/// plain for the user, fenced as the sender's claim for an agent.
fn report(app: &App, caller: &Caller, id: i64) -> Result<Option<String>> {
    let Some(row) = app.db.get_report(id)? else {
        return Ok(None);
    };
    if !(reaches(app, caller, &row.task_id)? || reaches(app, caller, &row.to_id)?) {
        return Ok(None);
    }
    let (shown, label, path) = match reports::asker_name(app, &row.task_id)? {
        Some(asker) => (asker.shown, asker.label, asker.path),
        None => (
            "a deleted task".to_string(),
            "a deleted task".to_string(),
            String::new(),
        ),
    };
    let mut out = format!(
        "r{} {} {} {}\n",
        row.id,
        row.kind.as_str(),
        shown,
        age_label(row.at)
    );
    match caller {
        Caller::User => {
            for line in printable_text(&row.body).lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        _ => {
            out.push_str(&assistant::fence_report(
                row.id, &label, &path, row.kind, &row.body,
            ));
            out.push('\n');
        }
    }
    Ok(Some(out))
}

/// Message `id`, as kept in the `delivered.jsonl` of a task `caller`
/// reaches.
fn message(app: &App, caller: &Caller, id: i64) -> Result<Option<String>> {
    // The assistant is no task to list, and is where most are kept.
    let mut tasks: Vec<TaskRecord> = app.db.list_tasks(None)?;
    tasks.extend(app.db.get_assistant_task()?);
    for task in tasks {
        if !reaches(app, caller, &task.id)? {
            continue;
        }
        let dir = crate::tasks::task_dir(app, &task);
        if let Some(found) = messages::read_delivered(&dir)
            .into_iter()
            .find(|d| d.id == id)
        {
            let text = match caller {
                Caller::User => printable_text(&found.message),
                _ => found.message,
            };
            return Ok(Some(format!(
                "m{} {} from {} {}\n{}\n",
                found.id,
                found.kind,
                found.from,
                age_label(found.at),
                text
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::{ReportKind, RunState, TaskKind, TaskRecord, TaskState};
    use crate::db::Database;
    use crate::render::{Class, Meta};

    fn task(slug: &str, kind: TaskKind, parent: Option<&str>) -> TaskRecord {
        TaskRecord {
            id: slug.into(),
            slug: slug.into(),
            name: slug.into(),
            project: None,
            kind,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: parent.map(Into::into),
            placement: crate::placement::for_new(),
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        }
    }

    /// The assistant with manager `m` (and its worker `mw`) and worker
    /// `sib` under it; `mw` reported to `m` (r1), `sib` to the assistant
    /// (r2); the assistant has a message (m1) and `m` another (m2).
    fn lab() -> (tempfile::TempDir, App) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().canonicalize().unwrap();
        let app = App {
            config: config::load_or_create(&path).unwrap(),
            db: Database::open(&config::index_path(&path)).unwrap(),
            home: path,
        };
        let root = assistant::ensure(&app).unwrap();
        for t in [
            task("m", TaskKind::Manager, Some(&root.id)),
            task("mw", TaskKind::Work, Some("m")),
            task("sib", TaskKind::Work, Some(&root.id)),
        ] {
            app.db.insert_task(&t).unwrap();
        }
        app.db
            .insert_report("mw", "m", ReportKind::Done, "merged <it>", 10)
            .unwrap();
        app.db
            .insert_report("sib", &root.id, ReportKind::Question, "which?", 11)
            .unwrap();
        let note = |slug: &str, text: &str| {
            let task = app.db.get_task(slug).unwrap().unwrap();
            let meta = Meta::new(Class::Line, "message", "task x", text, None);
            messages::number(&app.home, &crate::tasks::task_dir(&app, &task), meta, text).unwrap();
        };
        note(&root.id, "to the assistant");
        note("m", "to the manager");
        (home, app)
    }

    fn as_node(app: &App, slug: &str) -> Caller {
        Caller::Node(app.db.get_task(slug).unwrap().unwrap())
    }

    #[test]
    fn the_user_and_the_assistant_see_every_report_and_message() {
        let (_home, app) = lab();
        for caller in [Caller::User, Caller::Root] {
            let r1 = shown(&app, &caller, "r1").unwrap();
            assert!(r1.starts_with("r1 done task "), "{r1}");
            assert!(r1.contains("mw"), "{r1}");
            assert!(shown(&app, &caller, "r2").unwrap().contains("which?"));
            assert!(shown(&app, &caller, "m1")
                .unwrap()
                .contains("to the assistant"));
            assert!(shown(&app, &caller, "m2")
                .unwrap()
                .contains("to the manager"));
        }
        // The user reads it plain; an agent reads the sender's claim fenced.
        let plain = shown(&app, &Caller::User, "r1").unwrap();
        assert!(plain.contains("\n  merged <it>\n"), "{plain}");
        let fenced = shown(&app, &Caller::Root, "r1").unwrap();
        assert!(fenced.contains("<jeet-report id=\"1\""), "{fenced}");
        assert!(fenced.contains("merged ‹it›"), "{fenced}");
        let m1 = shown(&app, &Caller::User, "m1").unwrap();
        assert!(m1.starts_with("m1 message from task x "), "{m1}");
    }

    #[test]
    fn a_task_with_a_parent_sees_only_its_own_subtree() {
        let (_home, app) = lab();
        let m = as_node(&app, "m");
        // r1 is from a task below it, to it; m2 is in its own delivered.
        assert!(shown(&app, &m, "r1").is_ok());
        assert!(shown(&app, &m, "m2").unwrap().contains("to the manager"));
        // A sibling's report to the assistant, and the assistant's message.
        for out in ["r2", "m1"] {
            let e = shown(&app, &m, out).unwrap_err().to_string();
            assert_eq!(e, format!("no {out} reaches you"));
        }
        // The worker below it sees its own report (r1, which it filed) and
        // nothing of its manager's messages.
        let mw = as_node(&app, "mw");
        assert!(shown(&app, &mw, "r1").is_ok());
        assert!(shown(&app, &mw, "m2").is_err());
        // The sibling sees its own report, not the manager's subtree's.
        let sib = as_node(&app, "sib");
        assert!(shown(&app, &sib, "r2").is_ok());
        assert!(shown(&app, &sib, "r1").is_err());
    }

    #[test]
    fn a_missing_a_malformed_and_an_unreached_ref_read_the_same() {
        let (_home, app) = lab();
        let m = as_node(&app, "m");
        let said = |caller: &Caller, r: &str| shown(&app, caller, r).unwrap_err().to_string();
        assert_eq!(said(&m, "r2"), "no r2 reaches you");
        assert_eq!(said(&Caller::Root, "r99"), "no r99 reaches you");
        assert_eq!(said(&Caller::Root, "m99"), "no m99 reaches you");
        for bad in ["x1", "r", "rx", "r-1", "", "12"] {
            assert_eq!(said(&Caller::User, bad), format!("no {bad} reaches you"));
        }
    }

    #[test]
    fn a_review_is_refused() {
        let (_home, app) = lab();
        let review = crate::db::reviews::ReviewRecord {
            id: "rv".into(),
            slug: "rv".into(),
            repo_id: "github.com/a/b".into(),
            pr_number: 1,
            title: "t".into(),
            pr_state: None,
            pr_url: None,
            state: TaskState::Active,
            requested_by: Some("m".into()),
            created_at: 0,
            last_activity: 0,
            dir_name: None,
            branch: None,
            created_branch: false,
            head_oid: None,
            stash_ref: None,
            worktree_created_at: None,
            session_id: None,
            asked_head: None,
            asked_at: None,
            brought_back_at: None,
            max_secs: None,
            max_turns: None,
            used_secs: 0,
            used_turns: 0,
            warned: 0,
            paused: false,
            queued_at: None,
            job: None,
        };
        let e = shown(&app, &Caller::Review(Box::new(review)), "r1").unwrap_err();
        assert!(e.to_string().contains("only reads"), "{e:#}");
    }
}
