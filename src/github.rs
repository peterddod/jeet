//! Pull requests, through the GitHub CLI.
//!
//! jeet never talks to the GitHub API itself: `gh` already knows which host a
//! remote points at, holds the user's credentials, and understands forks. So
//! everything here is a `gh` subprocess run from inside the repository, and a
//! machine without `gh` simply has no pull requests to show.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The fields jeet asks `gh pr view` / `gh pr list` for.
const PR_FIELDS: &str = "number,title,url,state,isDraft,reviewDecision,headRefName,headRefOid,baseRefName,isCrossRepository";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    #[serde(default)]
    pub review_decision: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    pub base_ref_name: String,
    #[serde(default)]
    pub is_cross_repository: bool,
}

impl PullRequest {
    /// The local branch a review of this PR is checked out on.
    ///
    /// A PR from a fork gets `pr/<number>` rather than its head branch's name:
    /// a fork's branch is very often called `main`, and a local branch of that
    /// name is the one thing it must not be allowed to collide with.
    pub fn local_branch(&self) -> String {
        if self.is_cross_repository {
            format!("pr/{}", self.number)
        } else {
            self.head_ref_name.clone()
        }
    }

    /// `open`, `draft`, `merged` or `closed`, for display.
    pub fn state_label(&self) -> &'static str {
        match self.state.as_str() {
            "OPEN" if self.is_draft => "draft",
            "OPEN" => "open",
            "MERGED" => "merged",
            _ => "closed",
        }
    }

    /// Where review stands, in words, or nothing when GitHub has no opinion.
    pub fn decision_label(&self) -> Option<&'static str> {
        match self.review_decision.as_str() {
            "APPROVED" => Some("approved"),
            "CHANGES_REQUESTED" => Some("changes requested"),
            "REVIEW_REQUIRED" => Some("review required"),
            _ => None,
        }
    }
}

/// What a review says overall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Comment,
    RequestChanges,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::Comment => "comment",
            Verdict::RequestChanges => "request changes",
        }
    }

    /// The review event name GitHub's API uses.
    fn event(self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::Comment => "COMMENT",
            Verdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    fn gh_flag(self) -> &'static str {
        match self {
            Verdict::Approve => "--approve",
            Verdict::Comment => "--comment",
            Verdict::RequestChanges => "--request-changes",
        }
    }
}

/// A review the user has started but not submitted — typically one an agent
/// filled with inline comments. GitHub allows one per user per PR, and only
/// its author can see it, so any pending review we can see is the user's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReview {
    pub id: u64,
    pub comments: usize,
}

/// Run `gh` in `dir`, never letting it prompt or print over the caller.
///
/// A prompt would block on a stdin the explorer is reading, invisibly; and
/// `gh` writing to our stdout would break the shell wrapper's hand-off.
fn gh(dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow::anyhow!("the GitHub CLI (`gh`) is not installed")
            }
            _ => anyhow::anyhow!("could not run gh: {e}"),
        })?;
    Ok(output)
}

/// `gh` that must succeed, returning its stdout.
fn gh_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let output = gh(dir, args)?;
    if !output.status.success() {
        bail!("{}", gh_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn gh_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("gh failed with no output")
        .to_string()
}

/// A PR by number.
pub fn pr_view(repo_dir: &Path, number: u64) -> Result<PullRequest> {
    let text = gh_ok(
        repo_dir,
        &["pr", "view", &number.to_string(), "--json", PR_FIELDS],
    )?;
    serde_json::from_str(&text).context("parse gh pr view output")
}

/// The PR for a local branch, if there is one.
///
/// `pr/<n>` is how [`PullRequest::local_branch`] names a fork's PR, so that
/// goes straight to the number; anything else is looked up by head branch,
/// which `gh` resolves to the most recent PR from it, open or not.
pub fn pr_for_branch(repo_dir: &Path, branch: &str) -> Result<Option<PullRequest>> {
    if let Some(number) = fork_pr_number(branch) {
        return pr_view(repo_dir, number).map(Some);
    }
    let output = gh(repo_dir, &["pr", "view", branch, "--json", PR_FIELDS])?;
    if !output.status.success() {
        let why = gh_error(&output);
        // "no pull requests found for branch" is an answer, not a failure.
        if why.contains("no pull requests found") {
            return Ok(None);
        }
        bail!("{why}");
    }
    let pr = serde_json::from_slice(&output.stdout).context("parse gh pr view output")?;
    Ok(Some(pr))
}

fn fork_pr_number(branch: &str) -> Option<u64> {
    branch.strip_prefix("pr/")?.parse().ok()
}

/// Open PRs by the local branch they would be reviewed on, for labelling a
/// whole list of worktrees with one request.
pub fn open_prs_by_branch(repo_dir: &Path) -> Result<Vec<(String, u64)>> {
    let text = gh_ok(
        repo_dir,
        &[
            "pr", "list", "--state", "open", "--limit", "200", "--json", PR_FIELDS,
        ],
    )?;
    let prs: Vec<PullRequest> = serde_json::from_str(&text).context("parse gh pr list output")?;
    Ok(prs
        .into_iter()
        .map(|pr| (pr.local_branch(), pr.number))
        .collect())
}

/// A PR as `jeet prs` lists it: enough to decide which one to review.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedPr {
    pub number: u64,
    pub title: String,
    pub author: Login,
    #[serde(default)]
    pub additions: usize,
    #[serde(default)]
    pub deletions: usize,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub review_decision: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub is_cross_repository: bool,
    #[serde(default)]
    pub review_requests: Vec<Login>,
}

/// A GitHub account, or a team — `gh` gives both a `login`, and a team's
/// is never the user's, so they need no telling apart here.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Login {
    #[serde(default)]
    pub login: String,
}

impl ListedPr {
    /// Same rule as [`PullRequest::local_branch`], so a listed PR can be
    /// matched to the worktree `jeet review` would have put it in.
    pub fn local_branch(&self) -> String {
        if self.is_cross_repository {
            format!("pr/{}", self.number)
        } else {
            self.head_ref_name.clone()
        }
    }

    /// Where review stands: draft, or GitHub's decision, or nothing yet.
    pub fn status(&self) -> &'static str {
        if self.is_draft {
            return "draft";
        }
        match self.review_decision.as_str() {
            "APPROVED" => "approved",
            "CHANGES_REQUESTED" => "changes requested",
            _ => "needs review",
        }
    }

    pub fn requests(&self, login: &str) -> bool {
        !login.is_empty() && self.review_requests.iter().any(|r| r.login == login)
    }
}

/// Open PRs, newest first.
pub fn open_prs(repo_dir: &Path, limit: usize) -> Result<Vec<ListedPr>> {
    let text = gh_ok(
        repo_dir,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit.to_string(),
            "--json",
            "number,title,author,additions,deletions,isDraft,reviewDecision,headRefName,isCrossRepository,reviewRequests",
        ],
    )?;
    serde_json::from_str(&text).context("parse gh pr list output")
}

/// The signed-in user's login.
pub fn current_user(repo_dir: &Path) -> Result<String> {
    Ok(gh_ok(repo_dir, &["api", "user", "--jq", ".login"])?
        .trim()
        .to_string())
}

/// Check `number` out on `branch` in the worktree at `dir`.
///
/// `gh pr checkout` rather than a hand-rolled fetch: it knows where a fork's
/// branch lives and sets up tracking so a later run pulls new commits.
pub fn checkout(dir: &Path, number: u64, branch: &str) -> Result<()> {
    gh_ok(
        dir,
        &["pr", "checkout", &number.to_string(), "--branch", branch],
    )
    .map(|_| ())
}

/// Open the PR in the browser, through `gh` so its browser setting applies.
pub fn open_in_browser(repo_dir: &Path, number: u64) -> Result<()> {
    gh_ok(repo_dir, &["pr", "view", &number.to_string(), "--web"]).map(|_| ())
}

#[derive(Deserialize)]
struct ReviewRecord {
    id: u64,
    state: String,
}

/// The user's pending review on `number`, with how many inline comments it
/// holds, if they have one.
pub fn pending_review(repo_dir: &Path, number: u64) -> Result<Option<PendingReview>> {
    let text = gh_ok(
        repo_dir,
        &[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews?per_page=100"),
        ],
    )?;
    let pages: Vec<Vec<ReviewRecord>> = serde_json::from_str(&text).context("parse review list")?;
    let Some(review) = pages.into_iter().flatten().find(|r| r.state == "PENDING") else {
        return Ok(None);
    };
    let text = gh_ok(
        repo_dir,
        &[
            "api",
            "--paginate",
            "--slurp",
            &format!(
                "repos/{{owner}}/{{repo}}/pulls/{number}/reviews/{}/comments?per_page=100",
                review.id
            ),
        ],
    )?;
    let pages: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(&text).context("parse review comments")?;
    Ok(Some(PendingReview {
        id: review.id,
        comments: pages.iter().map(Vec::len).sum(),
    }))
}

/// Submit a review of `number`.
///
/// With a pending review, that review is what gets submitted — the inline
/// comments already in it go out with the verdict and body. Creating a fresh
/// review instead would fail outright: GitHub allows only one pending review
/// per user, and would leave the agent's comments stranded in it besides.
pub fn submit_review(
    repo_dir: &Path,
    number: u64,
    verdict: Verdict,
    body: &str,
    pending: Option<&PendingReview>,
) -> Result<()> {
    match pending {
        Some(review) => gh_ok(
            repo_dir,
            &[
                "api",
                "--method",
                "POST",
                &format!(
                    "repos/{{owner}}/{{repo}}/pulls/{number}/reviews/{}/events",
                    review.id
                ),
                "-f",
                &format!("event={}", verdict.event()),
                "-f",
                &format!("body={body}"),
            ],
        )
        .map(|_| ()),
        None => gh_ok(
            repo_dir,
            &[
                "pr",
                "review",
                &number.to_string(),
                verdict.gh_flag(),
                "--body",
                body,
            ],
        )
        .map(|_| ()),
    }
}

/// A PR number from what the user typed: `123`, `#123`, or a PR's URL.
pub fn parse_pr_number(text: &str) -> Result<u64> {
    let text = text.trim();
    let candidate = match text.split_once("/pull/") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None => text.strip_prefix('#').unwrap_or(text),
    };
    match candidate.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => bail!("`{text}` is not a pull request number (try 123, #123 or the PR's URL)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(cross: bool) -> PullRequest {
        PullRequest {
            number: 42,
            title: "Fix it".into(),
            url: "https://github.com/acme/widget/pull/42".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            head_ref_name: "main".into(),
            head_ref_oid: "abc".into(),
            base_ref_name: "main".into(),
            is_cross_repository: cross,
        }
    }

    #[test]
    fn parses_pr_numbers_in_every_spelling() {
        assert_eq!(parse_pr_number("123").unwrap(), 123);
        assert_eq!(parse_pr_number("#123").unwrap(), 123);
        assert_eq!(parse_pr_number(" #7 ").unwrap(), 7);
        assert_eq!(
            parse_pr_number("https://github.com/acme/widget/pull/88/files").unwrap(),
            88
        );
        assert_eq!(
            parse_pr_number("https://github.com/acme/widget/pull/88#discussion").unwrap(),
            88
        );
        assert!(parse_pr_number("#").is_err());
        assert!(parse_pr_number("0").is_err());
        assert!(parse_pr_number("feature-x").is_err());
    }

    #[test]
    fn a_fork_never_lands_on_its_own_branch_name() {
        assert_eq!(pr(true).local_branch(), "pr/42");
        assert_eq!(pr(false).local_branch(), "main");
        assert_eq!(fork_pr_number("pr/42"), Some(42));
        assert_eq!(fork_pr_number("pr/fix"), None);
        assert_eq!(fork_pr_number("feature"), None);
    }

    #[test]
    fn parses_gh_pr_list_output() {
        let json = r#"[{"additions":9,"author":{"id":"x","is_bot":false,"login":"ada","name":"Ada"},"deletions":2,"headRefName":"main","isCrossRepository":true,"isDraft":false,"number":14,"reviewDecision":"CHANGES_REQUESTED","reviewRequests":[{"__typename":"User","login":"me"},{"__typename":"Team","name":"core"}],"title":"T"}]"#;
        let prs: Vec<ListedPr> = serde_json::from_str(json).unwrap();
        let pr = &prs[0];
        assert_eq!(pr.author.login, "ada");
        assert_eq!(pr.status(), "changes requested");
        assert_eq!(pr.local_branch(), "pr/14");
        assert!(pr.requests("me"));
        assert!(!pr.requests("ada"));
        assert!(!pr.requests(""), "a team request is nobody's login");
    }

    #[test]
    fn parses_gh_pr_view_output() {
        let json = r#"{"baseRefName":"main","headRefName":"feat","headRefOid":"5f13","headRepositoryOwner":{"login":"x"},"isCrossRepository":false,"isDraft":true,"number":10,"reviewDecision":"","state":"OPEN","title":"T","url":"u"}"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 10);
        assert_eq!(pr.state_label(), "draft");
        assert_eq!(pr.decision_label(), None);
    }
}
