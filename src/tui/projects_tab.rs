//! The projects view of the tasks list: the active tasks and managers
//! grouped by the jeet project they were started from (`jeet task new
//! --project`, `TaskRecord.project`) — the projects `P` manages — or,
//! toggled, the projects with no active work. Pure, so the grouping is unit
//! tested.
//!
//! A task is in its own project. A manager is in the project of every task
//! below it, through any intermediate tasks, and in its own if it has one.
//! Whatever has no project is in none, and is not listed.

use std::collections::{HashMap, HashSet};

use super::state::rank_matches;
use super::tasks::TaskRow;
use crate::db::tasks::{TaskKind, TaskState};

/// One project: who is in it, as indices into the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGroup {
    pub name: String,
    pub managers: usize,
    pub tasks: usize,
    /// The newest activity of any member; 0 with none.
    pub last_activity: i64,
    pub members: Vec<usize>,
}

/// Both lists of projects, grouped once per read of the rows rather than
/// per draw or key.
#[derive(Debug, Default, Clone)]
pub struct Groups {
    active: Vec<ProjectGroup>,
    /// Every other project: with only archived tasks, or with none at all.
    archived: Vec<ProjectGroup>,
}

impl Groups {
    /// `known`: the names of every project that exists, as `projects::list`
    /// has them.
    pub fn new(rows: &[TaskRow], known: &[String]) -> Groups {
        let active = groups(rows, TaskState::Active);
        let mut archived = groups(rows, TaskState::Archived);
        let held: HashSet<&str> = active.iter().map(|line| line.name.as_str()).collect();
        archived.retain(|line| !held.contains(line.name.as_str()));
        for name in known {
            let listed = |line: &ProjectGroup| &line.name == name;
            if !held.contains(name.as_str()) && !archived.iter().any(listed) {
                archived.push(ProjectGroup {
                    name: name.clone(),
                    managers: 0,
                    tasks: 0,
                    last_activity: 0,
                    members: Vec::new(),
                });
            }
        }
        sort(&mut archived);
        Groups { active, archived }
    }

    fn all(&self, archived: bool) -> &[ProjectGroup] {
        match archived {
            true => &self.archived,
            false => &self.active,
        }
    }

    /// How many projects there are, active or `archived`.
    pub fn count(&self, archived: bool) -> usize {
        self.all(archived).len()
    }

    /// Who is in project `name`, as indices into the rows; `None` if the
    /// list has no such project. An archived project may have nobody.
    pub fn members(&self, archived: bool, name: &str) -> Option<&[usize]> {
        self.all(archived)
            .iter()
            .find(|line| line.name == name)
            .map(|line| line.members.as_slice())
    }

    /// The projects, newest activity first, narrowed by `filter` on the
    /// name, best match first.
    pub fn lines(&self, archived: bool, filter: &str) -> Vec<ProjectGroup> {
        let all = self.all(archived);
        if filter.is_empty() {
            return all.to_vec();
        }
        rank_matches(all.iter().map(|line| line.name.as_str()), filter)
            .into_iter()
            .map(|i| all[i].clone())
            .collect()
    }
}

fn sort(lines: &mut [ProjectGroup]) {
    lines.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// The projects of the tasks and managers in `state`, sorted.
fn groups(rows: &[TaskRow], state: TaskState) -> Vec<ProjectGroup> {
    let in_scope = |row: &TaskRow| {
        row.task.state == state && matches!(row.task.kind, TaskKind::Work | TaskKind::Manager)
    };
    let at: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.task.id.as_str(), i))
        .collect();
    // What each is in by itself; a manager's below-it comes next.
    let mut names: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| match (in_scope(row), row.task.project.as_deref()) {
            (true, Some(project)) => vec![project],
            _ => Vec::new(),
        })
        .collect();
    let mut added: Vec<Vec<&str>> = vec![Vec::new(); rows.len()];
    for (i, row) in rows.iter().enumerate().filter(|(_, row)| in_scope(row)) {
        let theirs = names[i].clone();
        // Up the parents that are in scope; a cycle stops where it comes round.
        let mut seen = HashSet::from([row.task.id.as_str()]);
        let mut up_from = row.task.parent_id.as_deref();
        while let Some(parent) = up_from.and_then(|id| at.get(id).copied()) {
            let up = &rows[parent];
            if !in_scope(up) || !seen.insert(up.task.id.as_str()) {
                break;
            }
            if up.task.kind == TaskKind::Manager {
                added[parent].extend(theirs.iter().copied());
            }
            up_from = up.task.parent_id.as_deref();
        }
    }
    for (own, up) in names.iter_mut().zip(added) {
        for name in up {
            if !own.contains(&name) {
                own.push(name);
            }
        }
    }
    let mut out: Vec<ProjectGroup> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        for &name in &names[i] {
            let slot = match out.iter().position(|line| line.name == name) {
                Some(slot) => slot,
                None => {
                    out.push(ProjectGroup {
                        name: name.to_string(),
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
    sort(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{RunState, TaskRecord};
    use std::path::PathBuf;

    fn row(
        id: &str,
        kind: TaskKind,
        state: TaskState,
        parent: Option<&str>,
        project: Option<&str>,
        last: i64,
    ) -> TaskRow {
        TaskRow {
            task: TaskRecord {
                id: id.into(),
                slug: id.into(),
                name: id.into(),
                project: project.map(str::to_string),
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
            worktrees: Vec::new(),
            activity: None,
            last_activity: last,
            size: None,
            report: None,
            review_jobs: Vec::new(),
            unanchored: false,
            queued: false,
        }
    }

    fn work(id: &str, project: Option<&str>, last: i64) -> TaskRow {
        row(id, TaskKind::Work, TaskState::Active, None, project, last)
    }

    fn lines(rows: &[TaskRow], known: &[&str], archived: bool, filter: &str) -> Vec<ProjectGroup> {
        let known: Vec<String> = known.iter().map(|s| s.to_string()).collect();
        Groups::new(rows, &known).lines(archived, filter)
    }

    fn summary(lines: &[ProjectGroup], rows: &[TaskRow]) -> Vec<(String, Vec<String>)> {
        lines
            .iter()
            .map(|line| {
                let ids = line.members.iter().map(|&i| rows[i].task.id.clone());
                (line.name.clone(), ids.collect())
            })
            .collect()
    }

    #[test]
    fn tasks_are_grouped_by_project_newest_first_and_projectless_ones_are_left_out() {
        let rows = vec![
            work("a", Some("web"), 10),
            work("b", Some("api"), 30),
            work("c", Some("web"), 20),
            work("bare", None, 99),
        ];
        let got = lines(&rows, &[], false, "");
        assert_eq!(
            summary(&got, &rows),
            [
                ("api".into(), vec!["b".into()]),
                ("web".into(), vec!["a".into(), "c".into()]),
            ]
        );
        assert_eq!(
            (got[1].managers, got[1].tasks, got[1].last_activity),
            (0, 2, 20)
        );
    }

    #[test]
    fn a_manager_is_in_the_projects_of_the_tasks_below_it() {
        let active = TaskState::Active;
        let rows = vec![
            row("ops", TaskKind::Manager, active, None, None, 5),
            row("deploy", TaskKind::Manager, active, Some("ops"), None, 6),
            row(
                "ship",
                TaskKind::Work,
                active,
                Some("deploy"),
                Some("web"),
                7,
            ),
            // Under a task, not a manager: the managers above still count it.
            row("sub", TaskKind::Work, active, Some("ship"), Some("docs"), 8),
            row("lone", TaskKind::Work, active, Some("ops"), None, 9),
        ];
        let got = summary(&lines(&rows, &[], false, ""), &rows);
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
            ]
        );
        let web = &lines(&rows, &[], false, "")[1];
        assert_eq!((web.managers, web.tasks), (2, 1));
    }

    #[test]
    fn a_manager_with_its_own_project_is_in_it_and_in_those_below() {
        let active = TaskState::Active;
        let rows = vec![
            row("ops", TaskKind::Manager, active, None, Some("infra"), 5),
            row("ship", TaskKind::Work, active, Some("ops"), Some("web"), 6),
            // A manager with nothing below it and no project is in none.
            row("idle", TaskKind::Manager, active, None, None, 7),
        ];
        let got = summary(&lines(&rows, &[], false, ""), &rows);
        assert_eq!(
            got,
            [
                ("web".into(), vec!["ops".into(), "ship".into()]),
                ("infra".into(), vec!["ops".into()]),
            ]
        );
    }

    #[test]
    fn archived_projects_are_those_without_active_work_including_ones_with_no_tasks() {
        let gone = TaskState::Archived;
        let rows = vec![
            work("live", Some("api"), 10),
            row("gone-api", TaskKind::Work, gone, None, Some("api"), 50),
            row("gone-web", TaskKind::Work, gone, None, Some("web"), 40),
            row("gone-mgr", TaskKind::Manager, gone, None, None, 30),
            row("review", TaskKind::Review, gone, None, Some("rev"), 60),
        ];
        let got = lines(&rows, &["api", "empty", "web", "zero"], true, "");
        assert_eq!(
            summary(&got, &rows),
            [
                ("web".into(), vec!["gone-web".into()]),
                ("empty".into(), vec![]),
                ("zero".into(), vec![]),
            ]
        );
        let groups = Groups::new(&rows, &["empty".to_string()]);
        assert_eq!(groups.members(true, "empty"), Some(&[][..]));
        assert_eq!(groups.members(false, "empty"), None);
        // Reviews are the reviews view's, in neither.
        assert_eq!(lines(&rows, &[], false, "").len(), 1);
    }

    #[test]
    fn a_cycle_of_parents_ends() {
        let active = TaskState::Active;
        let rows = vec![
            row("a", TaskKind::Manager, active, Some("b"), Some("x"), 1),
            row("b", TaskKind::Manager, active, Some("a"), Some("y"), 2),
        ];
        assert_eq!(lines(&rows, &[], false, "").len(), 2);
    }

    #[test]
    fn the_filter_matches_the_name() {
        let rows = vec![work("a", Some("api"), 10), work("b", Some("web"), 20)];
        let got = lines(&rows, &[], false, "we");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "web");
        let groups = Groups::new(&rows, &[]);
        assert_eq!(groups.members(false, "api"), Some(&[0][..]));
        assert_eq!(groups.count(false), 2);
    }
}
