//! The projects view of the tasks list: the active tasks and managers
//! grouped by the repository their worktrees are of — a project is a
//! repository, as `--repo` has it — or, toggled, the repositories that have
//! only archived tasks. Pure, so the grouping is unit tested.
//!
//! A task with worktrees of several repositories is in each; one with none
//! is under [`Key::NoRepo`]. A manager is also in the repositories of the
//! tasks below it, and is under `NoRepo` only if nothing below it has a
//! repository either.

use std::collections::{HashMap, HashSet};

use super::state::rank_matches;
use super::tasks::TaskRow;
use crate::db::tasks::{TaskKind, TaskState};

/// What a project is: a repository, or the tasks with none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    NoRepo,
    /// A worktree's `repo_id`.
    Repo(String),
}

impl Key {
    /// `jeet`: the repository's own name, as worktree rows show it.
    pub fn name(&self) -> &str {
        match self {
            Key::NoRepo => "(no repo)",
            Key::Repo(id) => id.rsplit('/').next().unwrap_or(id),
        }
    }

    /// `github.com/acme/jeet`; empty for `NoRepo`.
    pub fn full(&self) -> &str {
        match self {
            Key::NoRepo => "",
            Key::Repo(id) => id,
        }
    }
}

/// One project: who is in it, as indices into the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLine {
    pub key: Key,
    pub managers: usize,
    pub tasks: usize,
    /// The newest activity of any member.
    pub last_activity: i64,
    pub members: Vec<usize>,
}

/// Both lists of projects, grouped once per read of the rows rather than
/// per draw or key.
#[derive(Debug, Default, Clone)]
pub struct Groups {
    active: Vec<RepoLine>,
    /// Without any repository an active task is in.
    archived: Vec<RepoLine>,
}

impl Groups {
    pub fn new(rows: &[TaskRow]) -> Groups {
        let active = groups(rows, TaskState::Active);
        let held: HashSet<&Key> = active.iter().map(|line| &line.key).collect();
        let mut archived = groups(rows, TaskState::Archived);
        archived.retain(|line| !held.contains(&line.key));
        Groups { active, archived }
    }

    fn all(&self, archived: bool) -> &[RepoLine] {
        match archived {
            true => &self.archived,
            false => &self.active,
        }
    }

    /// How many projects there are, active or `archived`.
    pub fn count(&self, archived: bool) -> usize {
        self.all(archived).len()
    }

    /// Who is in project `key`, as indices into the rows.
    pub fn members(&self, archived: bool, key: &Key) -> &[usize] {
        self.all(archived)
            .iter()
            .find(|line| &line.key == key)
            .map_or(&[], |line| &line.members)
    }

    /// The projects, newest activity first with `NoRepo` last, narrowed by
    /// `filter` on the repository, best match first.
    pub fn lines(&self, archived: bool, filter: &str) -> Vec<RepoLine> {
        let all = self.all(archived);
        if filter.is_empty() {
            return all.to_vec();
        }
        let text: Vec<String> = all
            .iter()
            .map(|line| format!("{} {}", line.key.name(), line.key.full()))
            .collect();
        rank_matches(text.iter().map(String::as_str), filter)
            .into_iter()
            .map(|i| all[i].clone())
            .collect()
    }
}

/// The projects of the tasks and managers in `state`, sorted.
fn groups(rows: &[TaskRow], state: TaskState) -> Vec<RepoLine> {
    let in_scope = |row: &TaskRow| {
        row.task.state == state && matches!(row.task.kind, TaskKind::Work | TaskKind::Manager)
    };
    let own = |row: &TaskRow| -> Vec<Key> {
        let mut keys: Vec<Key> = Vec::new();
        for wt in &row.worktrees {
            let key = Key::Repo(wt.repo_id.clone());
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    };
    let at: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.task.id.as_str(), i))
        .collect();
    // What each task is in by itself; a manager's own comes below.
    let mut keys: Vec<Vec<Key>> = rows
        .iter()
        .map(|row| match in_scope(row) {
            true => own(row),
            false => Vec::new(),
        })
        .collect();
    let mut added: Vec<Vec<Key>> = vec![Vec::new(); rows.len()];
    for (i, row) in rows.iter().enumerate().filter(|(_, row)| in_scope(row)) {
        // Only real repositories go up: a manager is under `NoRepo` just
        // when it ends up in none.
        let theirs = keys[i].clone();
        // Up the parents that are in scope; a cycle stops where it comes round.
        let mut seen = HashSet::from([row.task.id.as_str()]);
        let mut up_from = row.task.parent_id.as_deref();
        while let Some(parent) = up_from.and_then(|id| at.get(id).copied()) {
            let up = &rows[parent];
            if !in_scope(up) || !seen.insert(up.task.id.as_str()) {
                break;
            }
            if up.task.kind == TaskKind::Manager {
                added[parent].extend(theirs.iter().cloned());
            }
            up_from = up.task.parent_id.as_deref();
        }
    }
    for (i, _) in rows.iter().enumerate().filter(|(_, row)| in_scope(row)) {
        for key in std::mem::take(&mut added[i]) {
            if !keys[i].contains(&key) {
                keys[i].push(key);
            }
        }
        if keys[i].is_empty() {
            keys[i].push(Key::NoRepo);
        }
    }
    let mut out: Vec<RepoLine> = Vec::new();
    for (i, row) in rows.iter().enumerate().filter(|(_, row)| in_scope(row)) {
        for key in &keys[i] {
            let slot = match out.iter().position(|line| &line.key == key) {
                Some(slot) => slot,
                None => {
                    out.push(RepoLine {
                        key: key.clone(),
                        managers: 0,
                        tasks: 0,
                        last_activity: 0,
                        members: Vec::new(),
                    });
                    out.len() - 1
                }
            };
            let line = &mut out[slot];
            match row.task.kind {
                TaskKind::Manager => line.managers += 1,
                _ => line.tasks += 1,
            }
            line.last_activity = line.last_activity.max(row.last_activity);
            line.members.push(i);
        }
    }
    out.sort_by(|a, b| {
        (a.key == Key::NoRepo)
            .cmp(&(b.key == Key::NoRepo))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.key.name().cmp(b.key.name()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{RunState, TaskRecord, TaskWorktreeRecord};
    use std::path::PathBuf;

    fn row(
        id: &str,
        kind: TaskKind,
        state: TaskState,
        parent: Option<&str>,
        repos: &[&str],
        last: i64,
    ) -> TaskRow {
        TaskRow {
            task: TaskRecord {
                id: id.into(),
                slug: id.into(),
                name: id.into(),
                project: None,
                kind,
                state,
                created_at: 0,
                last_activity: last,
                parent_id: parent.map(str::to_string),
                placement: crate::placement::Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            },
            dir: PathBuf::from(format!("/tasks/{id}")),
            worktrees: repos
                .iter()
                .map(|repo| TaskWorktreeRecord {
                    id: 0,
                    task_id: id.into(),
                    repo_id: format!("github.com/acme/{repo}"),
                    dir_name: repo.to_string(),
                    branch: None,
                    created_branch: false,
                    head_oid: None,
                    stash_ref: None,
                    pr_number: None,
                    created_at: 0,
                    read_only: false,
                })
                .collect(),
            activity: None,
            last_activity: last,
            size: None,
            report: None,
            review_jobs: Vec::new(),
            unanchored: false,
            queued: false,
        }
    }

    fn work(id: &str, repos: &[&str], last: i64) -> TaskRow {
        row(id, TaskKind::Work, TaskState::Active, None, repos, last)
    }

    fn lines(rows: &[TaskRow], archived: bool, filter: &str) -> Vec<RepoLine> {
        Groups::new(rows).lines(archived, filter)
    }

    fn summary(lines: &[RepoLine], rows: &[TaskRow]) -> Vec<(String, Vec<String>)> {
        lines
            .iter()
            .map(|line| {
                let ids = line.members.iter().map(|&i| rows[i].task.id.clone());
                (line.key.name().to_string(), ids.collect())
            })
            .collect()
    }

    #[test]
    fn a_task_is_under_each_repo_it_has_a_worktree_of_and_none_under_no_repo() {
        let rows = vec![
            work("both", &["api", "web"], 30),
            work("api-only", &["api"], 20),
            work("bare", &[], 40),
        ];
        let got = summary(&lines(&rows, false, ""), &rows);
        assert_eq!(
            got,
            [
                ("api".into(), vec!["both".into(), "api-only".into()]),
                ("web".into(), vec!["both".into()]),
                ("(no repo)".into(), vec!["bare".into()]),
            ]
        );
        let api = &lines(&rows, false, "")[0];
        assert_eq!((api.managers, api.tasks, api.last_activity), (0, 2, 30));
    }

    #[test]
    fn projects_are_newest_activity_first_with_no_repo_last() {
        let rows = vec![
            work("old", &["api"], 10),
            work("new", &["web"], 20),
            work("bare", &[], 99),
        ];
        let names: Vec<_> = lines(&rows, false, "")
            .iter()
            .map(|l| l.key.name().to_string())
            .collect();
        assert_eq!(names, ["web", "api", "(no repo)"]);
    }

    #[test]
    fn a_manager_is_under_the_repos_of_the_tasks_below_it() {
        let rows = vec![
            row("ops", TaskKind::Manager, TaskState::Active, None, &[], 5),
            row(
                "deploy",
                TaskKind::Manager,
                TaskState::Active,
                Some("ops"),
                &["api"],
                6,
            ),
            row(
                "ship",
                TaskKind::Work,
                TaskState::Active,
                Some("deploy"),
                &["web"],
                7,
            ),
            // Under a task, not a manager: the managers above still count it.
            row(
                "sub",
                TaskKind::Work,
                TaskState::Active,
                Some("ship"),
                &["docs"],
                8,
            ),
        ];
        let got = summary(&lines(&rows, false, ""), &rows);
        assert_eq!(
            got,
            [
                (
                    "docs".into(),
                    vec!["ops".into(), "deploy".into(), "sub".into()]
                ),
                (
                    "web".into(),
                    vec!["ops".into(), "deploy".into(), "ship".into()]
                ),
                ("api".into(), vec!["ops".into(), "deploy".into()]),
            ]
        );
        let web = &lines(&rows, false, "")[1];
        assert_eq!((web.managers, web.tasks), (2, 1));
    }

    #[test]
    fn a_manager_with_nothing_below_it_is_under_no_repo() {
        let rows = vec![
            row("ops", TaskKind::Manager, TaskState::Active, None, &[], 5),
            row(
                "lone",
                TaskKind::Work,
                TaskState::Active,
                Some("ops"),
                &[],
                6,
            ),
        ];
        let got = summary(&lines(&rows, false, ""), &rows);
        assert_eq!(
            got,
            [("(no repo)".into(), vec!["ops".into(), "lone".into()])]
        );
    }

    #[test]
    fn a_manager_with_repos_below_it_is_not_also_under_no_repo() {
        let rows = vec![
            row("ops", TaskKind::Manager, TaskState::Active, None, &[], 5),
            row(
                "has",
                TaskKind::Work,
                TaskState::Active,
                Some("ops"),
                &["api"],
                6,
            ),
            row(
                "bare",
                TaskKind::Work,
                TaskState::Active,
                Some("ops"),
                &[],
                7,
            ),
        ];
        let got = summary(&lines(&rows, false, ""), &rows);
        assert_eq!(
            got,
            [
                ("api".into(), vec!["ops".into(), "has".into()]),
                ("(no repo)".into(), vec!["bare".into()]),
            ]
        );
    }

    #[test]
    fn archived_projects_are_those_with_only_archived_tasks() {
        let rows = vec![
            work("live", &["api"], 10),
            row(
                "gone-api",
                TaskKind::Work,
                TaskState::Archived,
                None,
                &["api"],
                50,
            ),
            row(
                "gone-web",
                TaskKind::Work,
                TaskState::Archived,
                None,
                &["web"],
                40,
            ),
            row(
                "gone-mgr",
                TaskKind::Manager,
                TaskState::Archived,
                None,
                &[],
                30,
            ),
            row(
                "review",
                TaskKind::Review,
                TaskState::Archived,
                None,
                &["rev"],
                60,
            ),
        ];
        let got = summary(&lines(&rows, true, ""), &rows);
        assert_eq!(
            got,
            [
                ("web".into(), vec!["gone-web".into()]),
                ("(no repo)".into(), vec!["gone-mgr".into()]),
            ]
        );
        // Reviews are the reviews view's, in neither.
        assert_eq!(lines(&rows, false, "").len(), 1);
    }

    #[test]
    fn a_cycle_of_parents_ends() {
        let rows = vec![
            row(
                "a",
                TaskKind::Manager,
                TaskState::Active,
                Some("b"),
                &["x"],
                1,
            ),
            row(
                "b",
                TaskKind::Manager,
                TaskState::Active,
                Some("a"),
                &["y"],
                2,
            ),
        ];
        assert_eq!(lines(&rows, false, "").len(), 2);
    }

    #[test]
    fn the_filter_matches_the_repo() {
        let rows = vec![work("a", &["api"], 10), work("b", &["web"], 20)];
        let got = lines(&rows, false, "we");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key.name(), "web");
        let api = Key::Repo("github.com/acme/api".into());
        assert_eq!(Groups::new(&rows).members(false, &api), [0]);
        assert_eq!(Groups::new(&rows).count(false), 2);
    }
}
