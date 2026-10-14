//! The reviews view of the tasks list: every review jeet knows of — each
//! review task, active or archived, and any review job an older `jeet
//! review` recorded with no task to hold it — with where its PR and its
//! review stand on GitHub.
//!
//! What GitHub says is asked on a thread of its own ([`look`]) and kept by
//! the list until asked again, so no draw waits on `gh`. PR titles are
//! whoever opened the PR's: they are shown made printable, never run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::state::rank_matches;
use super::tasks::{Outbox, TaskRow, Update};
use crate::db::tasks::{TaskKind, TaskState};
use crate::github::{self, LatestReview, ReviewedPr};
use crate::review::{self, JobState};
use crate::status::first_line;

/// How often GitHub is asked again while the view is shown.
pub const LOOK_EVERY: Duration = Duration::from_secs(120);

/// A PR under review.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub repo_id: String,
    pub number: u64,
}

impl PrKey {
    /// `jeet#55`.
    pub fn label(&self) -> String {
        let repo = self.repo_id.rsplit('/').next().unwrap_or(&self.repo_id);
        format!("{repo}#{}", self.number)
    }
}

/// The PR a review task's checkout holds, if it records one.
pub fn task_pr(row: &TaskRow) -> Option<PrKey> {
    row.worktrees.iter().find_map(|wt| {
        Some(PrKey {
            repo_id: wt.repo_id.clone(),
            number: wt.pr_number?,
        })
    })
}

/// A review job with no task: one an older `jeet review` ran headless, or
/// whose task has since been deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy {
    pub pr: PrKey,
    /// The PR's title when the job started; untrusted.
    pub title: String,
    pub running: bool,
    pub started_at: i64,
}

/// Every review job recorded under `home`'s `reviews/<host>/<owner>/<repo>/
/// <n>.json`, as [`review`] files them.
pub fn legacy_jobs(home: &Path) -> Vec<Legacy> {
    let root = home.join("reviews");
    let mut jobs: Vec<Legacy> = walkdir::WalkDir::new(&root)
        .min_depth(4)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| legacy_at(&root, entry.path()))
        .collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.started_at));
    jobs
}

fn legacy_at(root: &Path, path: &Path) -> Option<Legacy> {
    if path.extension()? != "json" {
        return None;
    }
    let number = path.file_stem()?.to_str()?.parse().ok()?;
    let repo_id = path
        .parent()?
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    let job = review::load_at(path)?;
    Some(Legacy {
        pr: PrKey { repo_id, number },
        running: job.state() == JobState::Running,
        title: job.title,
        started_at: job.started_at,
    })
}

/// Where a line of the view comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Index into the list's rows: a review task.
    Task(usize),
    /// Index into the legacy jobs.
    Legacy(usize),
}

/// One line of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLine {
    pub source: Source,
    /// None for a review task whose checkout records no PR.
    pub pr: Option<PrKey>,
}

/// The view's lines: active review tasks, newest activity first as `rows`
/// are, then archived ones, then legacy jobs no task holds; narrowed by
/// `filter` on the PR, the task's name and the title GitHub last gave.
pub fn lines(
    rows: &[TaskRow],
    legacy: &[Legacy],
    filter: &str,
    title: impl Fn(&PrKey) -> Option<String>,
) -> Vec<ReviewLine> {
    let reviews = |state: TaskState| {
        rows.iter()
            .enumerate()
            .filter(move |(_, row)| row.task.kind == TaskKind::Review && row.task.state == state)
            .map(|(i, row)| ReviewLine {
                source: Source::Task(i),
                pr: task_pr(row),
            })
    };
    let mut all: Vec<ReviewLine> = reviews(TaskState::Active)
        .chain(reviews(TaskState::Archived))
        .collect();
    let held: HashSet<PrKey> = all.iter().filter_map(|line| line.pr.clone()).collect();
    let mut seen = HashSet::new();
    all.extend(
        legacy
            .iter()
            .enumerate()
            .filter(|(_, job)| !held.contains(&job.pr) && seen.insert(job.pr.clone()))
            .map(|(i, job)| ReviewLine {
                source: Source::Legacy(i),
                pr: Some(job.pr.clone()),
            }),
    );
    if filter.is_empty() {
        return all;
    }
    let text: Vec<String> = all
        .iter()
        .map(|line| {
            let name = match line.source {
                Source::Task(i) => rows[i].task.name.as_str(),
                Source::Legacy(i) => legacy[i].title.as_str(),
            };
            let pr = line.pr.as_ref();
            let label = pr.map(PrKey::label).unwrap_or_default();
            let title = pr.and_then(&title).unwrap_or_default();
            format!("{label} {name} {title}")
        })
        .collect();
    rank_matches(text.iter().map(String::as_str), filter)
        .into_iter()
        .map(|i| all[i].clone())
        .collect()
}

/// Where a PR's review stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    ChangesRequested,
    Commented,
    /// Branch protection wants a review and has none that settles it.
    ReviewRequired,
    /// No review yet, and nothing requires one.
    Awaiting,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approved => "approved",
            Verdict::ChangesRequested => "changes requested",
            Verdict::Commented => "commented",
            Verdict::ReviewRequired => "review required",
            Verdict::Awaiting => "awaiting review",
        }
    }
}

/// GitHub's `reviewDecision` when it has one that settles it, else what
/// the reviewers' latest reviews say: a change asked for outweighs an
/// approval, which outweighs a comment. An approval short of what branch
/// protection requires is not shown as approved.
pub fn verdict(decision: &str, latest: &[LatestReview]) -> Verdict {
    let any = |state: &str| latest.iter().any(|r| r.state == state);
    match decision {
        "APPROVED" => Verdict::Approved,
        "CHANGES_REQUESTED" => Verdict::ChangesRequested,
        _ if any("CHANGES_REQUESTED") => Verdict::ChangesRequested,
        "REVIEW_REQUIRED" if any("COMMENTED") || any("APPROVED") => Verdict::Commented,
        "REVIEW_REQUIRED" => Verdict::ReviewRequired,
        _ if any("APPROVED") => Verdict::Approved,
        _ if any("COMMENTED") => Verdict::Commented,
        _ => Verdict::Awaiting,
    }
}

/// What GitHub last said about a PR under review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrReview {
    /// Untrusted: shown made printable.
    pub title: String,
    /// `open`, `draft`, `merged` or `closed`.
    pub state: &'static str,
    pub verdict: Verdict,
}

impl From<ReviewedPr> for PrReview {
    fn from(found: ReviewedPr) -> Self {
        PrReview {
            state: found.pr.state_label(),
            verdict: verdict(&found.pr.review_decision, &found.latest_reviews),
            title: found.pr.title,
        }
    }
}

/// One PR to ask GitHub about, and where `gh` runs for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub pr: PrKey,
    pub trunk: PathBuf,
}

/// Ask GitHub about each PR of every batch that arrives, reporting each
/// as [`Update::Review`]. A newer batch replaces what is left of an older.
pub(super) fn look(asks: Receiver<Vec<Ask>>, tx: Outbox) {
    while let Ok(batch) = asks.recv() {
        let mut queue = batch.into_iter();
        loop {
            if let Some(newer) = asks.try_iter().last() {
                queue = newer.into_iter();
            }
            let Some(ask) = queue.next() else {
                break;
            };
            let found = github::pr_review(&ask.trunk, ask.pr.number)
                .map(PrReview::from)
                .map_err(|e| first_line(&format!("{e:#}"), 120).unwrap_or_default());
            if tx.send(Update::Review { pr: ask.pr, found }).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviews(states: &[&str]) -> Vec<LatestReview> {
        states
            .iter()
            .map(|s| LatestReview {
                state: s.to_string(),
            })
            .collect()
    }

    #[test]
    fn the_verdict_follows_the_decision_then_the_latest_reviews() {
        assert_eq!(verdict("APPROVED", &[]), Verdict::Approved);
        assert_eq!(
            verdict("CHANGES_REQUESTED", &reviews(&["APPROVED"])),
            Verdict::ChangesRequested
        );
        assert_eq!(verdict("REVIEW_REQUIRED", &[]), Verdict::ReviewRequired);
        // Not enough approvals for branch protection: not approved.
        assert_eq!(
            verdict("REVIEW_REQUIRED", &reviews(&["APPROVED"])),
            Verdict::Commented
        );
        // No branch protection: the reviews alone say.
        assert_eq!(verdict("", &[]), Verdict::Awaiting);
        assert_eq!(verdict("", &reviews(&["COMMENTED"])), Verdict::Commented);
        assert_eq!(
            verdict("", &reviews(&["COMMENTED", "APPROVED"])),
            Verdict::Approved
        );
        assert_eq!(
            verdict("", &reviews(&["APPROVED", "CHANGES_REQUESTED"])),
            Verdict::ChangesRequested
        );
        assert_eq!(verdict("", &reviews(&["DISMISSED"])), Verdict::Awaiting);
    }

    #[test]
    fn a_pr_key_is_labelled_by_its_repository_name() {
        let key = PrKey {
            repo_id: "github.com/peterddod/jeet".into(),
            number: 55,
        };
        assert_eq!(key.label(), "jeet#55");
    }

    #[test]
    fn legacy_jobs_are_read_from_where_review_files_them() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("reviews/github.com/acme/api");
        std::fs::create_dir_all(&dir).unwrap();
        let job = serde_json::json!({
            "pr": 7, "title": "Fix \u{1b}[31mit", "head": "abc", "worktree": "/nowhere",
            "command": "true", "env": [], "notify": false, "log": "/nowhere/log",
            "started_at": 100, "finished_at": 110, "exit_code": 0,
        });
        std::fs::write(dir.join("7.json"), job.to_string()).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a job").unwrap();
        let jobs = legacy_jobs(home.path());
        assert_eq!(
            jobs,
            vec![Legacy {
                pr: PrKey {
                    repo_id: "github.com/acme/api".into(),
                    number: 7,
                },
                title: "Fix \u{1b}[31mit".into(),
                running: false,
                started_at: 100,
            }]
        );
        assert!(legacy_jobs(&home.path().join("missing")).is_empty());
    }
}
