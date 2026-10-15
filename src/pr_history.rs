//! A task's pull requests: found by asking GitHub, through `gh`, about each
//! of the task's worktrees, and kept in `task_prs` so the history outlives
//! the branches — a merged PR whose branch is long gone still shows.
//!
//! Finding them is slow, a `gh` call per worktree, so it comes in three
//! parts: [`job`] reads from the index what there is to ask about,
//! [`discover`] asks — touching no index, so it can run on a thread of its
//! own — and [`record`] writes the answers back.

use std::path::PathBuf;

use anyhow::Result;

use crate::context::App;
use crate::db::tasks::{TaskPrRecord, TaskRecord};
use crate::git;
use crate::github::{self, HistoryPr};
use crate::tasks;

/// How one worktree's PRs are looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The worktree is a checkout of this PR: a review's.
    Number(u64),
    /// PRs whose head is this branch, open or not.
    Branch(String),
    /// PRs holding this commit: a detached HEAD's.
    Commit(String),
}

/// The lookup for a worktree checked out for PR `pr_number`, or on `branch`,
/// or detached at `head` — which `head_on_default` says the default branch
/// already has. That is where every fresh worktree starts, and the PR that
/// last landed there is no work of this task's, so it is not looked up.
pub fn choose(
    pr_number: Option<u64>,
    branch: Option<&str>,
    head: Option<&str>,
    head_on_default: bool,
) -> Option<Lookup> {
    if let Some(number) = pr_number {
        return Some(Lookup::Number(number));
    }
    if let Some(branch) = branch {
        return Some(match github::fork_pr_number(branch) {
            Some(number) => Lookup::Number(number),
            None => Lookup::Branch(branch.to_string()),
        });
    }
    match head {
        Some(sha) if !head_on_default => Some(Lookup::Commit(sha.to_string())),
        _ => None,
    }
}

/// One worktree of the task, as [`discover`] needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub repo_id: String,
    /// Where `gh` runs: the trunk knows the same remote as the worktree, and
    /// is still there when an archived worktree is not.
    pub trunk: PathBuf,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub pr_number: Option<u64>,
    /// HEAD as archive recorded it, for a worktree not on disk.
    pub head_oid: Option<String>,
    pub default_branch: String,
}

/// A PR the history has as open: asked about again by number, so a merge is
/// seen even once no worktree leads to it any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub repo_id: String,
    pub trunk: PathBuf,
    pub number: u64,
}

/// Everything to ask GitHub about for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub task_id: String,
    pub sources: Vec<Source>,
    pub open: Vec<Known>,
}

/// What GitHub said about a task's PRs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub task_id: String,
    pub prs: Vec<TaskPrRecord>,
    /// Lookups that failed, each said once.
    pub errors: Vec<String>,
}

/// What to ask about `task`: its worktrees, and its PRs still open.
pub fn job(app: &App, task: &TaskRecord) -> Result<Job> {
    let mut sources = Vec::new();
    for row in app.db.list_task_worktrees(&task.id)? {
        let Some(repo) = app.db.get_repo(&row.repo_id)? else {
            continue;
        };
        sources.push(Source {
            worktree: tasks::worktree_dir(app, task, &row.dir_name),
            trunk: PathBuf::from(&repo.trunk_path),
            repo_id: repo.id,
            branch: row.branch,
            pr_number: row.pr_number,
            head_oid: row.head_oid,
            default_branch: repo.default_branch,
        });
    }
    let mut open = Vec::new();
    for pr in app.db.list_task_prs(&task.id)? {
        if pr.state != "OPEN" {
            continue;
        }
        let Some(repo) = app.db.get_repo(&pr.repo_id)? else {
            continue;
        };
        open.push(Known {
            repo_id: repo.id,
            trunk: PathBuf::from(&repo.trunk_path),
            number: pr.number,
        });
    }
    Ok(Job {
        task_id: task.id.clone(),
        sources,
        open,
    })
}

/// Ask GitHub about every worktree in `job`, then about the open PRs no
/// worktree led to. Without `gh` there is nothing to ask, and one error
/// says so.
pub fn discover(job: &Job) -> Found {
    let mut found = Found {
        task_id: job.task_id.clone(),
        prs: Vec::new(),
        errors: Vec::new(),
    };
    for source in &job.sources {
        if !source.trunk.is_dir() {
            found.note(format!("{}: trunk is missing", source.repo_id));
            continue;
        }
        let Some(lookup) = lookup_for(source) else {
            continue;
        };
        let answer = match &lookup {
            Lookup::Number(number) => github::pr_history(&source.trunk, *number).map(|pr| vec![pr]),
            Lookup::Branch(branch) => github::prs_for_branch(&source.trunk, branch),
            Lookup::Commit(sha) => github::prs_for_commit(&source.trunk, sha),
        };
        match answer {
            Ok(prs) => found.add(&source.repo_id, prs),
            Err(e) if github::is_not_installed(&e) => {
                found.note(format!("{e:#}"));
                return found;
            }
            Err(e) => found.note(format!("{}: {e:#}", source.repo_id)),
        }
    }
    for known in &job.open {
        let seen = found
            .prs
            .iter()
            .any(|p| p.repo_id == known.repo_id && p.number == known.number);
        if seen || !known.trunk.is_dir() {
            continue;
        }
        match github::pr_history(&known.trunk, known.number) {
            Ok(pr) => found.add(&known.repo_id, vec![pr]),
            Err(e) if github::is_not_installed(&e) => {
                found.note(format!("{e:#}"));
                return found;
            }
            Err(e) => found.note(format!("{} #{}: {e:#}", known.repo_id, known.number)),
        }
    }
    found
}

impl Found {
    /// Take in PRs of `repo_id`, each once.
    fn add(&mut self, repo_id: &str, prs: Vec<HistoryPr>) {
        for pr in prs {
            let record = to_record(&self.task_id, repo_id, pr);
            let dup = self
                .prs
                .iter()
                .any(|p| p.repo_id == record.repo_id && p.number == record.number);
            if !dup {
                self.prs.push(record);
            }
        }
    }

    fn note(&mut self, error: String) {
        if !self.errors.contains(&error) {
            self.errors.push(error);
        }
    }
}

/// The lookup for a worktree on disk — or, archived, as it was left.
fn lookup_for(source: &Source) -> Option<Lookup> {
    if source.pr_number.is_some() || source.branch.is_some() {
        return choose(source.pr_number, source.branch.as_deref(), None, false);
    }
    let head = match source.worktree.is_dir() {
        true => git::rev_parse(&source.worktree, "HEAD"),
        false => source.head_oid.clone(),
    };
    let default = format!("refs/remotes/origin/{}", source.default_branch);
    let on_default = head
        .as_deref()
        .is_some_and(|head| git::is_ancestor(&source.trunk, head, &default));
    choose(None, None, head.as_deref(), on_default)
}

pub fn to_record(task_id: &str, repo_id: &str, pr: HistoryPr) -> TaskPrRecord {
    TaskPrRecord {
        task_id: task_id.to_string(),
        repo_id: repo_id.to_string(),
        number: pr.pr.number,
        title: pr.pr.title,
        url: pr.pr.url,
        state: pr.pr.state,
        is_draft: pr.pr.is_draft,
        additions: pr.additions,
        deletions: pr.deletions,
        updated_at: pr.updated_at,
    }
}

/// Write what was found into the task's history. A task deleted while its
/// PRs were being looked up takes nothing.
pub fn record(app: &App, found: &Found) -> Result<()> {
    if app.db.get_task(&found.task_id)?.is_none() {
        return Ok(());
    }
    for pr in &found.prs {
        app.db.upsert_task_pr(pr)?;
    }
    Ok(())
}

/// Look `task`'s PRs up and record them, in one go: for the CLI, which has
/// nothing else to do meanwhile.
pub fn refresh(app: &App, task: &TaskRecord) -> Result<Found> {
    let found = discover(&job(app, task)?);
    record(app, &found)?;
    Ok(found)
}

/// `open`, `draft`, `merged` or `closed`, as the history shows a PR.
pub fn state_label(pr: &TaskPrRecord) -> &'static str {
    match pr.state.as_str() {
        "OPEN" if pr.is_draft => "draft",
        "OPEN" => "open",
        "MERGED" => "merged",
        _ => "closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(number: u64, cross: bool) -> HistoryPr {
        HistoryPr {
            pr: github::PullRequest {
                number,
                title: format!("PR {number}"),
                url: format!("https://github.com/acme/api/pull/{number}"),
                state: "OPEN".into(),
                is_draft: false,
                review_decision: String::new(),
                head_ref_name: "feat".into(),
                head_ref_oid: "abc".into(),
                base_ref_name: "main".into(),
                is_cross_repository: cross,
            },
            additions: 3,
            deletions: 1,
            updated_at: "2026-10-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn each_kind_of_worktree_has_its_lookup() {
        // A review's checkout knows its PR, whatever branch it is on.
        assert_eq!(
            choose(Some(7), Some("feat"), None, false),
            Some(Lookup::Number(7))
        );
        assert_eq!(
            choose(None, Some("feat"), None, false),
            Some(Lookup::Branch("feat".into()))
        );
        // A fork's PR is checked out as pr/<n>: no branch of that name on GitHub.
        assert_eq!(
            choose(None, Some("pr/12"), None, false),
            Some(Lookup::Number(12))
        );
        assert_eq!(
            choose(None, None, Some("5f13"), false),
            Some(Lookup::Commit("5f13".into()))
        );
    }

    #[test]
    fn a_fresh_detached_checkout_has_no_prs_to_look_for() {
        // Detached at what the default branch already has: where it started.
        assert_eq!(choose(None, None, Some("5f13"), true), None);
        // No HEAD to go on at all.
        assert_eq!(choose(None, None, None, false), None);
    }

    #[test]
    fn found_prs_are_kept_once_each() {
        let mut found = Found {
            task_id: "t".into(),
            prs: Vec::new(),
            errors: Vec::new(),
        };
        found.add(
            "github.com/acme/api",
            vec![history(1, false), history(2, false)],
        );
        found.add("github.com/acme/api", vec![history(2, false)]);
        // The same number in another repository is another PR.
        found.add("github.com/acme/web", vec![history(2, false)]);
        let keys: Vec<(&str, u64)> = found
            .prs
            .iter()
            .map(|p| (p.repo_id.as_str(), p.number))
            .collect();
        assert_eq!(
            keys,
            [
                ("github.com/acme/api", 1),
                ("github.com/acme/api", 2),
                ("github.com/acme/web", 2)
            ]
        );
        found.note("boom".into());
        found.note("boom".into());
        assert_eq!(found.errors, ["boom"]);
    }

    #[test]
    fn a_record_carries_everything_the_panel_shows() {
        let record = to_record("t", "github.com/acme/api", history(9, false));
        assert_eq!(record.task_id, "t");
        assert_eq!(record.number, 9);
        assert_eq!((record.additions, record.deletions), (3, 1));
        assert_eq!(record.updated_at, "2026-10-01T00:00:00Z");
        assert_eq!(state_label(&record), "open");
    }

    #[test]
    fn states_read_as_github_shows_them() {
        let mut pr = to_record("t", "r", history(1, false));
        pr.is_draft = true;
        assert_eq!(state_label(&pr), "draft");
        pr.state = "MERGED".into();
        assert_eq!(state_label(&pr), "merged", "a merged draft is merged");
        pr.state = "CLOSED".into();
        assert_eq!(state_label(&pr), "closed");
    }

    #[test]
    fn without_gh_one_error_says_so_and_nothing_more_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        github::GH_PROGRAM.with(|p| *p.borrow_mut() = Some(dir.path().join("no-gh")));
        let source = |branch: &str| Source {
            repo_id: format!("github.com/acme/{branch}"),
            trunk: dir.path().to_path_buf(),
            worktree: dir.path().join("missing"),
            branch: Some(branch.into()),
            pr_number: None,
            head_oid: None,
            default_branch: "main".into(),
        };
        let found = discover(&Job {
            task_id: "t".into(),
            sources: vec![source("a"), source("b")],
            open: Vec::new(),
        });
        github::GH_PROGRAM.with(|p| *p.borrow_mut() = None);
        assert!(found.prs.is_empty());
        assert_eq!(found.errors.len(), 1, "{:?}", found.errors);
        assert!(
            found.errors[0].contains("not installed"),
            "{:?}",
            found.errors
        );
    }
}
