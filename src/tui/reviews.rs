//! The reviews view of the tasks list: every review jeet knows of — each
//! active review, and any review job an older `jeet review` recorded with
//! no review to hold it, or, toggled, the archived reviews — with where its
//! PR and its review stand on GitHub. Reviews are rows of their own
//! ([`ReviewRow`]), never among the tasks.
//!
//! What GitHub says is asked on a thread of its own ([`look`]) and kept by
//! the list until asked again, so no draw waits on `gh`. PR titles are
//! whoever opened the PR's: they are shown made printable, never run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::state::rank_matches;
use super::tasks::{self, Outbox, Update};
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::TaskState;
use crate::github::{self, LatestReview, ReviewedPr};
use crate::review::{self, JobState};
use crate::status::{first_line, Activity};

/// How often GitHub is asked again while the view is shown.
pub const LOOK_EVERY: Duration = Duration::from_secs(120);

/// A PR under review.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub repo_id: String,
    pub number: u64,
}

impl PrKey {
    /// `jeet`: the repository's own name.
    pub fn repo(&self) -> &str {
        self.repo_id.rsplit('/').next().unwrap_or(&self.repo_id)
    }

    /// `jeet#55`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo(), self.number)
    }
}

/// One review, with what the background threads have found out about it.
#[derive(Debug, Clone)]
pub struct ReviewRow {
    pub review: ReviewRecord,
    /// Its directory under the tasks root: the worktree is in it.
    pub dir: PathBuf,
    /// None until the poller first reports, and for archived reviews.
    pub activity: Option<Activity>,
    /// Unix seconds; the poller raises it from status files and transcripts.
    pub last_activity: i64,
    /// Bytes on disk, once the size walker has been through the directory.
    pub size: Option<u64>,
    /// Its agent runs without jeet knowing its process, as the last poll
    /// found.
    pub unanchored: bool,
    /// Its legacy job's state file: a job still running is shown as its
    /// agent running.
    pub jobs: Vec<PathBuf>,
}

impl ReviewRow {
    pub fn new(review: ReviewRecord, dir: PathBuf, jobs: Vec<PathBuf>) -> Self {
        ReviewRow {
            last_activity: review.last_activity,
            review,
            dir,
            activity: None,
            size: None,
            unanchored: false,
            jobs,
        }
    }

    pub fn id(&self) -> &str {
        &self.review.id
    }

    pub fn archived(&self) -> bool {
        self.review.state == TaskState::Archived
    }

    /// The PR it reviews.
    pub fn pr(&self) -> PrKey {
        PrKey {
            repo_id: self.review.repo_id.clone(),
            number: self.review.pr_number,
        }
    }

    /// Whether its agent is running, as far as the last poll knows.
    pub fn agent_running(&self) -> bool {
        tasks::running(self.activity.as_ref())
    }
}

/// Newest activity first, as the tasks are.
pub fn sort_rows(rows: &mut [ReviewRow]) {
    rows.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.review.slug.cmp(&b.review.slug))
            .then_with(|| a.review.id.cmp(&b.review.id))
    });
}

/// A review job with no review: one an older `jeet review` ran headless.
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
    /// Index into the list's review rows.
    Review(usize),
    /// Index into the legacy jobs.
    Legacy(usize),
}

/// One line of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLine {
    pub source: Source,
    pub pr: PrKey,
}

/// The view's lines: the active reviews, newest activity first as `rows`
/// are, then legacy jobs no review holds; or, `archived`, the archived
/// reviews alone. Narrowed by `filter` on the PR, the review's name and the
/// title GitHub last gave.
pub fn lines(
    rows: &[ReviewRow],
    legacy: &[Legacy],
    archived: bool,
    filter: &str,
    title: impl Fn(&PrKey) -> Option<String>,
) -> Vec<ReviewLine> {
    let mut all: Vec<ReviewLine> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.archived() == archived)
        .map(|(i, row)| ReviewLine {
            source: Source::Review(i),
            pr: row.pr(),
        })
        .collect();
    if !archived {
        // A job whose PR has a review, archived or not, is that review's.
        let held: HashSet<PrKey> = rows.iter().map(ReviewRow::pr).collect();
        let mut seen = HashSet::new();
        all.extend(
            legacy
                .iter()
                .enumerate()
                .filter(|(_, job)| !held.contains(&job.pr) && seen.insert(job.pr.clone()))
                .map(|(i, job)| ReviewLine {
                    source: Source::Legacy(i),
                    pr: job.pr.clone(),
                }),
        );
    }
    if filter.is_empty() {
        return all;
    }
    let text: Vec<String> = all
        .iter()
        .map(|line| {
            let name = match line.source {
                Source::Review(i) => rows[i].review.slug.as_str(),
                Source::Legacy(i) => legacy[i].title.as_str(),
            };
            let title = title(&line.pr).unwrap_or_default();
            format!("{} {name} {title}", line.pr.label())
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
    /// When it was merged or closed, in seconds since the epoch.
    pub closed_at: Option<i64>,
}

impl From<ReviewedPr> for PrReview {
    fn from(found: ReviewedPr) -> Self {
        PrReview {
            state: found.pr.state_label(),
            closed_at: found.closed_at.as_deref().and_then(github::parse_timestamp),
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

    fn review_row(id: &str, pr: u64, state: TaskState, last: i64) -> ReviewRow {
        let mut review = ReviewRecord::new(
            id.into(),
            format!("review-api-{pr}"),
            "github.com/acme/api",
            pr,
            last,
        );
        review.state = state;
        ReviewRow::new(review, PathBuf::from("/nowhere"), Vec::new())
    }

    fn key(number: u64) -> PrKey {
        PrKey {
            repo_id: "github.com/acme/api".into(),
            number,
        }
    }

    fn job(number: u64) -> Legacy {
        Legacy {
            pr: key(number),
            title: format!("job {number}"),
            running: false,
            started_at: 0,
        }
    }

    #[test]
    fn lines_are_the_reviews_of_a_state_then_the_jobs_no_review_holds() {
        let rows = [
            review_row("a", 1, TaskState::Active, 5),
            review_row("b", 2, TaskState::Archived, 4),
            review_row("c", 3, TaskState::Active, 3),
        ];
        // Job 2 belongs to the archived review of #2; job 9 is nobody's.
        let legacy = [job(2), job(9), job(9)];
        let shown = |archived, filter| -> Vec<(Source, u64)> {
            lines(&rows, &legacy, archived, filter, |_| None)
                .into_iter()
                .map(|line| (line.source, line.pr.number))
                .collect()
        };
        assert_eq!(
            shown(false, ""),
            [
                (Source::Review(0), 1),
                (Source::Review(2), 3),
                (Source::Legacy(1), 9)
            ]
        );
        // Archived: the archived reviews alone, never the jobs.
        assert_eq!(shown(true, ""), [(Source::Review(1), 2)]);
        // The filter matches the PR, the review's name and the title.
        assert_eq!(shown(false, "api-3"), [(Source::Review(2), 3)]);
        assert_eq!(shown(false, "job 9"), [(Source::Legacy(1), 9)]);
        let titled = lines(&rows, &[], false, "login", |pr| {
            (pr.number == 1).then(|| "Speed up login".to_string())
        });
        assert_eq!(titled.len(), 1);
        assert_eq!(titled[0].source, Source::Review(0));
    }

    #[test]
    fn rows_sort_newest_activity_first() {
        let mut rows = vec![
            review_row("a", 1, TaskState::Active, 1),
            review_row("b", 2, TaskState::Active, 9),
        ];
        sort_rows(&mut rows);
        assert_eq!(rows[0].id(), "b");
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
