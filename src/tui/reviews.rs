//! The reviews view of the tasks list: every review jeet knows of — each
//! active review, or, toggled, the archived reviews — with where its PR and
//! its review stand on GitHub. Reviews are rows of their own
//! ([`ReviewRow`]), never among the tasks.
//!
//! What GitHub says is asked on a thread of its own ([`look`]) and kept by
//! the list until asked again, so no draw waits on `gh`. PR titles are
//! whoever opened the PR's: they are shown made printable, never run.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::state::rank_matches;
use super::tasks::{self, Outbox, Update};
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::TaskState;
use crate::github::{self, LatestReview, ReviewedPr};
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
}

impl ReviewRow {
    pub fn new(review: ReviewRecord, dir: PathBuf) -> Self {
        ReviewRow {
            last_activity: review.last_activity,
            review,
            dir,
            activity: None,
            size: None,
            unanchored: false,
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

/// One line of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLine {
    /// Index into the list's review rows.
    pub row: usize,
    pub pr: PrKey,
}

/// The view's lines: the active reviews, newest activity first as `rows`
/// are, or, `archived`, the archived reviews. Narrowed by `filter` on the
/// PR, the review's name and the title GitHub last gave.
pub fn lines(
    rows: &[ReviewRow],
    archived: bool,
    filter: &str,
    title: impl Fn(&PrKey) -> Option<String>,
) -> Vec<ReviewLine> {
    let all: Vec<ReviewLine> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.archived() == archived)
        .map(|(row, r)| ReviewLine { row, pr: r.pr() })
        .collect();
    if filter.is_empty() {
        return all;
    }
    let text: Vec<String> = all
        .iter()
        .map(|line| {
            let name = rows[line.row].review.slug.as_str();
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
        ReviewRow::new(review, PathBuf::from("/nowhere"))
    }

    #[test]
    fn lines_are_the_reviews_of_a_state() {
        let rows = [
            review_row("a", 1, TaskState::Active, 5),
            review_row("b", 2, TaskState::Archived, 4),
            review_row("c", 3, TaskState::Active, 3),
        ];
        let shown = |archived, filter| -> Vec<(usize, u64)> {
            lines(&rows, archived, filter, |_| None)
                .into_iter()
                .map(|line| (line.row, line.pr.number))
                .collect()
        };
        assert_eq!(shown(false, ""), [(0, 1), (2, 3)]);
        assert_eq!(shown(true, ""), [(1, 2)]);
        // The filter matches the PR, the review's name and the title.
        assert_eq!(shown(false, "api-3"), [(2, 3)]);
        let titled = lines(&rows, false, "login", |pr| {
            (pr.number == 1).then(|| "Speed up login".to_string())
        });
        assert_eq!(titled.len(), 1);
        assert_eq!(titled[0].row, 0);
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
}
