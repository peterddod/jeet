//! What every node that manages tasks has, whatever runs it: its home's
//! templates by role, the fences what others wrote reaches it in, its inbox
//! hand-over, and ([`context`]) the sections of its summary on its children,
//! its todo and its inbox. The assistant is the root node; [`crate::assistant`] keeps its
//! sessions and windows.

pub mod budget;
pub mod context;
pub mod fence;
pub mod run;

use std::path::Path;

use anyhow::{Context, Result};

use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::instructions;
use crate::messages::{self, Queued};
use crate::status;

use fence::fenced;

/// Which node a home is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The assistant, at the root of the tree.
    Root,
}

const AGENTS_MD: &str = include_str!("templates/root/AGENTS.md");
const SKILL: &str = include_str!("templates/root/SKILL.md");
const WORKLOG: &str = include_str!("templates/root/worklog.md");
/// What `todo.md` starts as.
pub const TODO: &str = include_str!("templates/root/todo.md");

/// Each template for `role`, and where in the home it goes: AGENTS.md and
/// each harness's import of it first.
fn templates(role: Role) -> Vec<(&'static str, &'static str)> {
    match role {
        Role::Root => {
            let imports = instructions::imports()
                .into_iter()
                .map(|i| (i.file, i.text));
            std::iter::once((instructions::AGENTS_MD, AGENTS_MD))
                .chain(imports)
                .chain([
                    (".claude/skills/jeet-assistant/SKILL.md", SKILL),
                    ("notes/worklog.md", WORKLOG),
                    ("todo.md", TODO),
                ])
                .collect()
        }
    }
}

/// Write each of `role`'s templates that is not in `dir` yet, returning
/// those written. Nothing there is ever replaced: a CLAUDE.md of the user's
/// own, from before AGENTS.md, is moved to AGENTS.md rather than written
/// over.
pub fn ensure_files(dir: &Path, role: Role) -> Result<Vec<&'static str>> {
    instructions::migrate(dir).context("move the assistant's CLAUDE.md to AGENTS.md")?;
    let mut written = Vec::new();
    for (rel, text) in templates(role) {
        let path = dir.join(rel);
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // `create_new`: another jeet writing it meanwhile wins, and its
        // copy is not overwritten.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, text.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
                written.push(rel);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("write {}", path.display())),
        }
    }
    Ok(written)
}

/// The root node, the assistant's task, made the first time with its home
/// set up.
pub fn root(app: &App) -> Result<TaskRecord> {
    crate::assistant::ensure(app)
}

/// Where `task` sits in the tree, from the root down:
/// `assistant/backend/fix-x`. A task outside the tree is its slug alone.
pub fn tree_path(app: &App, task: &TaskRecord) -> Result<String> {
    let mut slugs: Vec<String> = app
        .db
        .ancestors(&task.id)?
        .into_iter()
        .rev()
        .map(|a| a.slug)
        .collect();
    slugs.push(task.slug.clone());
    Ok(slugs.join("/"))
}

/// Who `task` is, to the tasks around it: `the assistant`, `manager
/// backend` or `task fix-x`.
pub fn label(task: &TaskRecord) -> String {
    label_at(task, &task.slug)
}

/// [`label`], with `task` named by `path` (its [`tree_path`]) instead of
/// its slug: `manager assistant/backend`.
pub fn label_at(task: &TaskRecord, path: &str) -> String {
    match task.kind {
        TaskKind::Assistant => "the assistant".to_string(),
        TaskKind::Manager => format!("manager {path}"),
        TaskKind::Work | TaskKind::Review => format!("task {path}"),
    }
}

/// `YYYY-MM-DD`, in UTC, for unix seconds `at`.
pub fn date(at: i64) -> String {
    // Howard Hinnant's days-to-civil.
    let days = at.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Most of the inbox a session's summary takes.
const MAX_INBOX: usize = messages::MAX_MESSAGE;

/// The messages queued for session `session_id` — addressed to it, to
/// anyone, or to a session no longer `running` — that fit in the summary,
/// taken from its inbox; the rest stay queued.
pub fn take_inbox(
    dir: &Path,
    session_id: &str,
    running: impl FnOnce() -> Vec<String>,
) -> Result<Vec<Queued>> {
    let inbox = messages::Inbox::lock(dir)?;
    inbox.release_orphans(running)?;
    let me = Some(session_id);
    let queued = inbox.read_for(me)?;
    let mut len = 0;
    let fit = queued
        .iter()
        .take_while(|q| {
            len += q.message.len();
            len <= MAX_INBOX
        })
        .count();
    inbox.replace_for(me, &queued[fit..])?;
    Ok(queued[..fit].to_vec())
}

/// One active task that reports to the assistant, for the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLine {
    pub slug: String,
    /// From its brief, which the user or the assistant wrote.
    pub goal: Option<String>,
    pub done_when: Option<String>,
    /// Its last report: the task's own claim, untrusted.
    pub report: Option<ReportLine>,
}

/// A task's last report, for the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLine {
    pub id: i64,
    pub kind: &'static str,
    /// How long ago, as `5m ago`.
    pub age: String,
    pub body: String,
}

/// A task under the assistant, on one line: what it was asked, and what it
/// last said.
pub fn child_line(child: &ChildLine) -> String {
    let short = |text: &str| fenced(&status::first_line(text, 200).unwrap_or_default());
    let mut line = format!("- task {}", fenced(&child.slug));
    if let Some(goal) = &child.goal {
        line.push_str(&format!(": goal {}", short(goal)));
        if let Some(done_when) = &child.done_when {
            line.push_str(&format!(", done when {}", short(done_when)));
        }
    }
    match &child.report {
        Some(r) => line.push_str(&format!(
            "; last report #{} {} {}: {}",
            r.id,
            r.kind,
            r.age,
            short(&r.body)
        )),
        None => line.push_str("; no report yet"),
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::CLAUDE_SHIM;
    use crate::todo;

    #[test]
    fn templates_are_written_only_where_nothing_is() {
        let dir = tempfile::tempdir().unwrap();
        let written = ensure_files(dir.path(), Role::Root).unwrap();
        assert_eq!(written.len(), templates(Role::Root).len());
        for (rel, text) in templates(Role::Root) {
            assert_eq!(std::fs::read_to_string(dir.path().join(rel)).unwrap(), text);
        }
        std::fs::write(dir.path().join("todo.md"), "- [ ] mine\n").unwrap();
        std::fs::remove_file(dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(ensure_files(dir.path(), Role::Root).unwrap(), ["CLAUDE.md"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            CLAUDE_SHIM
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("todo.md")).unwrap(),
            "- [ ] mine\n"
        );
    }

    #[test]
    fn the_skill_is_a_skill_and_the_todo_template_is_an_empty_list() {
        assert!(SKILL.starts_with("---\nname: jeet-assistant\ndescription: "));
        assert!(todo::parse(TODO).items().is_empty());
        assert!(AGENTS_MD.contains("untrusted"));
        assert!(AGENTS_MD.contains("<jeet-message from="));
    }

    #[test]
    fn a_home_from_before_agents_md_keeps_the_users_claude_md_as_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "my assistant\n").unwrap();
        let written = ensure_files(dir.path(), Role::Root).unwrap();
        assert!(!written.contains(&"AGENTS.md"), "{written:?}");
        assert!(!written.contains(&"CLAUDE.md"), "{written:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "my assistant\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            CLAUDE_SHIM
        );
    }

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_791_590_400), "2026-10-10");
        assert_eq!(date(1_791_676_799), "2026-10-10");
    }

    #[test]
    fn the_inbox_is_handed_over_as_far_as_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        {
            let inbox = messages::Inbox::lock(dir.path()).unwrap();
            inbox.push("first").unwrap();
            inbox.push(&"x".repeat(MAX_INBOX)).unwrap();
        }
        let taken = take_inbox(dir.path(), "s", Vec::new).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].message, "first");
        let left = messages::Inbox::lock(dir.path()).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_session_starts_with_what_is_queued_for_it_and_nobody_else() {
        let dir = tempfile::tempdir().unwrap();
        {
            let inbox = messages::Inbox::lock(dir.path()).unwrap();
            inbox.push_to("for p", Some("p")).unwrap();
            inbox.push_to("for one that ended", Some("gone")).unwrap();
            inbox.push("for anyone").unwrap();
        }
        let running = || vec!["p".to_string(), "s".to_string()];
        let taken: Vec<String> = take_inbox(dir.path(), "s", running)
            .unwrap()
            .into_iter()
            .map(|q| q.message)
            .collect();
        assert_eq!(taken, ["for one that ended", "for anyone"]);
        let left = messages::Inbox::lock(dir.path()).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "for p");
    }
}
