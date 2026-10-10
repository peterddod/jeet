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
    /// The GraphQL id, which adding a thread to the review needs.
    pub node_id: String,
    pub comments: usize,
}

/// Run `gh` in `dir`, never letting it prompt or print over the caller.
///
/// A prompt would block on a stdin the explorer is reading, invisibly; and
/// `gh` writing to our stdout would break the shell wrapper's hand-off.
fn gh(dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(gh_program()?)
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

/// The `gh` to run: the real one, except in unit tests.
#[cfg(not(test))]
fn gh_program() -> Result<std::ffi::OsString> {
    Ok("gh".into())
}

#[cfg(test)]
thread_local! {
    /// A stand-in `gh` for the unit tests on this thread.
    pub static GH_PROGRAM: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Unit tests run only the stand-in they set up: the real `gh` would reach
/// GitHub, and a test that gets here by accident fails instead.
#[cfg(test)]
fn gh_program() -> Result<std::ffi::OsString> {
    match GH_PROGRAM.with(|p| p.borrow().clone()) {
        Some(stub) => Ok(stub.into()),
        None => bail!("unit tests do not run the real gh"),
    }
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
    #[serde(default)]
    node_id: String,
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
        node_id: review.node_id,
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

/// The PR's unified diff, as `gh pr diff` prints it.
pub fn pr_diff(repo_dir: &Path, number: u64) -> Result<String> {
    gh_ok(
        repo_dir,
        &["pr", "diff", &number.to_string(), "--color", "never"],
    )
}

/// Which version of a file a review comment is on: the base (`LEFT`, a
/// removed or unchanged line) or the head (`RIGHT`, an added or unchanged one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiffSide {
    Left,
    Right,
}

impl DiffSide {
    /// The name GitHub's API uses.
    pub fn as_str(self) -> &'static str {
        match self {
            DiffSide::Left => "LEFT",
            DiffSide::Right => "RIGHT",
        }
    }
}

/// A conversation anchored to a line of the diff.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    /// GraphQL id of the thread.
    pub id: String,
    pub path: String,
    /// The line in the current diff, or none once the thread is outdated
    /// and its line has gone.
    #[serde(default)]
    pub line: Option<u32>,
    /// The line in the diff the thread was started on.
    #[serde(default)]
    pub original_line: Option<u32>,
    pub diff_side: DiffSide,
    #[serde(default)]
    pub is_resolved: bool,
    #[serde(default)]
    pub is_outdated: bool,
    #[serde(default, deserialize_with = "nodes")]
    pub comments: Vec<ThreadComment>,
}

/// One comment in a [`ReviewThread`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComment {
    /// GraphQL id.
    pub id: String,
    /// REST id, which replying, editing, deleting and reacting all take.
    pub database_id: u64,
    /// None for a deleted account ("ghost").
    #[serde(default)]
    pub author: Option<Login>,
    #[serde(default)]
    pub body: String,
    /// RFC 3339, e.g. `2026-10-08T12:34:56Z`.
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub viewer_can_update: bool,
    #[serde(default)]
    pub viewer_can_delete: bool,
    #[serde(default)]
    pub reaction_groups: Vec<ReactionGroup>,
}

impl ThreadComment {
    /// The author's login, with GitHub's own name for an account that is gone.
    pub fn author_login(&self) -> &str {
        match &self.author {
            Some(author) if !author.login.is_empty() => &author.login,
            _ => "ghost",
        }
    }

    /// When the comment was made, as a unix timestamp.
    pub fn created_secs(&self) -> Option<i64> {
        parse_timestamp(&self.created_at)
    }
}

/// How many people reacted to a comment with one emoji.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReactionGroup {
    /// GraphQL's name for it: `THUMBS_UP`, `HEART`…
    pub content: String,
    #[serde(default)]
    pub viewer_has_reacted: bool,
    #[serde(default)]
    pub users: TotalCount,
}

impl ReactionGroup {
    pub fn count(&self) -> u64 {
        self.users.total_count
    }

    pub fn reaction(&self) -> Option<Reaction> {
        Reaction::from_graphql(&self.content)
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TotalCount {
    #[serde(default)]
    pub total_count: u64,
}

/// The reactions GitHub offers on a review comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}

impl Reaction {
    pub const ALL: [Reaction; 8] = [
        Reaction::ThumbsUp,
        Reaction::ThumbsDown,
        Reaction::Laugh,
        Reaction::Hooray,
        Reaction::Confused,
        Reaction::Heart,
        Reaction::Rocket,
        Reaction::Eyes,
    ];

    pub fn emoji(self) -> &'static str {
        match self {
            Reaction::ThumbsUp => "👍",
            Reaction::ThumbsDown => "👎",
            Reaction::Laugh => "😄",
            Reaction::Hooray => "🎉",
            Reaction::Confused => "😕",
            Reaction::Heart => "❤️",
            Reaction::Rocket => "🚀",
            Reaction::Eyes => "👀",
        }
    }

    fn graphql(self) -> &'static str {
        match self {
            Reaction::ThumbsUp => "THUMBS_UP",
            Reaction::ThumbsDown => "THUMBS_DOWN",
            Reaction::Laugh => "LAUGH",
            Reaction::Hooray => "HOORAY",
            Reaction::Confused => "CONFUSED",
            Reaction::Heart => "HEART",
            Reaction::Rocket => "ROCKET",
            Reaction::Eyes => "EYES",
        }
    }

    /// The name the REST reactions endpoint takes.
    fn rest(self) -> &'static str {
        match self {
            Reaction::ThumbsUp => "+1",
            Reaction::ThumbsDown => "-1",
            Reaction::Laugh => "laugh",
            Reaction::Hooray => "hooray",
            Reaction::Confused => "confused",
            Reaction::Heart => "heart",
            Reaction::Rocket => "rocket",
            Reaction::Eyes => "eyes",
        }
    }

    pub fn from_graphql(name: &str) -> Option<Reaction> {
        Reaction::ALL.into_iter().find(|r| r.graphql() == name)
    }
}

/// GraphQL wraps every list in `{ "nodes": [...] }`; this unwraps one.
fn nodes<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    struct Nodes<T> {
        #[serde(default = "Vec::new")]
        nodes: Vec<T>,
    }
    Ok(Option::<Nodes<T>>::deserialize(deserializer)?
        .map(|n| n.nodes)
        .unwrap_or_default())
}

/// The review threads query. `$endCursor` and `pageInfo` are what
/// `gh api --paginate` needs to walk past the first hundred threads; a thread
/// with more than a hundred comments is shown with its first hundred.
const THREADS_QUERY: &str =
    "query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $endCursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id path line originalLine diffSide isResolved isOutdated
          comments(first: 100) {
            nodes {
              id databaseId author { login } body createdAt
              viewerCanUpdate viewerCanDelete
              reactionGroups { content viewerHasReacted users { totalCount } }
            }
          }
        }
      }
    }
  }
}";

/// Every review thread on `number`, in the order GitHub gives them.
pub fn review_threads(repo_dir: &Path, number: u64) -> Result<Vec<ReviewThread>> {
    let text = gh_ok(
        repo_dir,
        &[
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-F",
            &format!("number={number}"),
            "-f",
            &format!("query={THREADS_QUERY}"),
        ],
    )?;
    parse_threads(&text)
}

/// Threads out of `gh api graphql --paginate --slurp`: an array of pages.
fn parse_threads(text: &str) -> Result<Vec<ReviewThread>> {
    #[derive(Deserialize)]
    struct Page {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<Repository>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        pull_request: Option<PullRequestThreads>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PullRequestThreads {
        #[serde(deserialize_with = "nodes")]
        review_threads: Vec<ReviewThread>,
    }
    let pages: Vec<Page> = serde_json::from_str(text).context("parse review threads")?;
    let mut threads = Vec::new();
    for page in pages {
        let Some(pr) = page.data.repository.and_then(|r| r.pull_request) else {
            bail!("GitHub did not return the pull request's review threads");
        };
        threads.extend(pr.review_threads);
    }
    Ok(threads)
}

/// What changes when anything on a PR does: new commits, a new comment, an
/// edit. Compared, not shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub head_ref_oid: String,
    pub updated_at: String,
    /// Conversation comments plus review-thread comments.
    pub comments: u64,
}

const FINGERPRINT_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      headRefOid updatedAt
      comments { totalCount }
      reviewThreads(first: 100) { nodes { comments { totalCount } } }
    }
  }
}";

/// The PR's [`Fingerprint`], for noticing that the viewer is out of date.
pub fn pr_fingerprint(repo_dir: &Path, number: u64) -> Result<Fingerprint> {
    let text = gh_ok(
        repo_dir,
        &[
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-F",
            &format!("number={number}"),
            "-f",
            &format!("query={FINGERPRINT_QUERY}"),
        ],
    )?;
    parse_fingerprint(&text)
}

fn parse_fingerprint(text: &str) -> Result<Fingerprint> {
    #[derive(Deserialize)]
    struct Page {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<Repository>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        pull_request: Option<Pr>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        head_ref_oid: String,
        updated_at: String,
        #[serde(default)]
        comments: TotalCount,
        #[serde(default, deserialize_with = "nodes")]
        review_threads: Vec<Thread>,
    }
    #[derive(Deserialize)]
    struct Thread {
        #[serde(default)]
        comments: TotalCount,
    }
    let page: Page = serde_json::from_str(text).context("parse pull request fingerprint")?;
    let Some(pr) = page.data.repository.and_then(|r| r.pull_request) else {
        bail!("GitHub did not return the pull request");
    };
    let threaded: u64 = pr
        .review_threads
        .iter()
        .map(|t| t.comments.total_count)
        .sum();
    Ok(Fingerprint {
        head_ref_oid: pr.head_ref_oid,
        updated_at: pr.updated_at,
        comments: pr.comments.total_count + threaded,
    })
}

/// Reply to the thread that review comment `comment_id` belongs to.
pub fn reply(repo_dir: &Path, number: u64, comment_id: u64, body: &str) -> Result<()> {
    gh_ok(
        repo_dir,
        &[
            "api",
            "--method",
            "POST",
            &format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments/{comment_id}/replies"),
            "-f",
            &format!("body={body}"),
        ],
    )
    .map(|_| ())
}

/// Replace a review comment's text. Only its author can.
pub fn edit_comment(repo_dir: &Path, comment_id: u64, body: &str) -> Result<()> {
    gh_ok(
        repo_dir,
        &[
            "api",
            "--method",
            "PATCH",
            &format!("repos/{{owner}}/{{repo}}/pulls/comments/{comment_id}"),
            "-f",
            &format!("body={body}"),
        ],
    )
    .map(|_| ())
}

pub fn delete_comment(repo_dir: &Path, comment_id: u64) -> Result<()> {
    gh_ok(
        repo_dir,
        &[
            "api",
            "--method",
            "DELETE",
            &format!("repos/{{owner}}/{{repo}}/pulls/comments/{comment_id}"),
        ],
    )
    .map(|_| ())
}

pub fn react(repo_dir: &Path, comment_id: u64, reaction: Reaction) -> Result<()> {
    gh_ok(
        repo_dir,
        &[
            "api",
            "--method",
            "POST",
            &format!("repos/{{owner}}/{{repo}}/pulls/comments/{comment_id}/reactions"),
            "-f",
            &format!("content={}", reaction.rest()),
        ],
    )
    .map(|_| ())
}

const ADD_THREAD_MUTATION: &str = "mutation($review: ID!, $path: String!, $line: Int!, $side: DiffSide!, $body: String!) {
  addPullRequestReviewThread(input: {pullRequestReviewId: $review, path: $path, line: $line, side: $side, body: $body}) {
    thread { id }
  }
}";

/// Comment on one line of the diff, as part of the user's pending review.
///
/// GitHub allows one pending review per user per PR, so the comment joins
/// the one there is — an agent's, say — and only starts a new one when there
/// is none. Either way nothing is published until a verdict submits it.
pub fn add_inline_comment(
    repo_dir: &Path,
    number: u64,
    path: &str,
    line: u32,
    side: DiffSide,
    body: &str,
) -> Result<()> {
    match pending_review(repo_dir, number)? {
        Some(review) => gh_ok(
            repo_dir,
            &[
                "api",
                "graphql",
                "-f",
                &format!("query={ADD_THREAD_MUTATION}"),
                "-f",
                &format!("review={}", review.node_id),
                "-f",
                &format!("path={path}"),
                "-F",
                &format!("line={line}"),
                "-f",
                &format!("side={}", side.as_str()),
                "-f",
                &format!("body={body}"),
            ],
        )
        .map(|_| ()),
        // A review created without an event stays pending.
        None => gh_ok(
            repo_dir,
            &[
                "api",
                "--method",
                "POST",
                &format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews"),
                "-f",
                &format!("comments[][path]={path}"),
                "-F",
                &format!("comments[][line]={line}"),
                "-f",
                &format!("comments[][side]={}", side.as_str()),
                "-f",
                &format!("comments[][body]={body}"),
            ],
        )
        .map(|_| ()),
    }
}

/// Seconds since the epoch for an RFC 3339 UTC timestamp, as GitHub writes
/// them (`2026-10-08T12:34:56Z`, fractional seconds allowed). None for
/// anything else rather than a wrong answer.
pub fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut date_parts = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (
        date_parts.next()??,
        date_parts.next()??,
        date_parts.next()??,
    );
    let time = time.split('.').next()?;
    let mut time_parts = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (
        time_parts.next()??,
        time_parts.next()??,
        time_parts.next()??,
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from the civil date, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
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

    #[test]
    fn parses_paginated_review_threads() {
        let threads = parse_threads(include_str!("../tests/fixtures/review_threads.json")).unwrap();
        assert_eq!(threads.len(), 3, "both pages are read");

        let first = &threads[0];
        assert_eq!(first.path, "src/lib.rs");
        assert_eq!(first.line, Some(12));
        assert_eq!(first.original_line, Some(11));
        assert_eq!(first.diff_side, DiffSide::Right);
        assert!(!first.is_resolved && !first.is_outdated);
        assert_eq!(first.comments.len(), 2);
        let opener = &first.comments[0];
        assert_eq!(opener.database_id, 1001);
        assert_eq!(opener.author_login(), "ada");
        assert!(opener.body.contains('\n'));
        assert_eq!(
            opener.reaction_groups[0].reaction(),
            Some(Reaction::ThumbsUp)
        );
        assert_eq!(opener.reaction_groups[0].count(), 2);
        assert!(opener.reaction_groups[0].viewer_has_reacted);
        let reply = &first.comments[1];
        assert!(reply.viewer_can_update && reply.viewer_can_delete);
        assert_eq!(
            reply.created_secs(),
            opener.created_secs().map(|t| t + 90 * 60),
            "fractional seconds are ignored"
        );

        let removed = &threads[1];
        assert_eq!(removed.diff_side, DiffSide::Left);
        assert!(removed.is_resolved);
        assert_eq!(removed.comments[0].author_login(), "ghost");

        let outdated = &threads[2];
        assert!(outdated.is_outdated);
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.original_line, Some(40));
    }

    #[test]
    fn a_missing_pull_request_is_an_error_not_an_empty_list() {
        let json = r#"[{"data":{"repository":{"pullRequest":null}}}]"#;
        assert!(parse_threads(json).is_err());
        assert!(parse_threads("[]").unwrap().is_empty());
    }

    #[test]
    fn parses_the_fingerprint() {
        let print =
            parse_fingerprint(include_str!("../tests/fixtures/pr_fingerprint.json")).unwrap();
        assert_eq!(print.head_ref_oid, "5f13c0ffee");
        assert_eq!(print.updated_at, "2026-10-09T08:00:00Z");
        assert_eq!(print.comments, 6, "conversation plus thread comments");
    }

    #[test]
    fn reactions_round_trip_through_their_api_names() {
        for reaction in Reaction::ALL {
            assert_eq!(Reaction::from_graphql(reaction.graphql()), Some(reaction));
            assert!(!reaction.rest().is_empty());
            assert!(!reaction.emoji().is_empty());
        }
        assert_eq!(Reaction::from_graphql("NOPE"), None);
        assert_eq!(Reaction::ThumbsUp.rest(), "+1");
    }

    #[test]
    fn parses_github_timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_timestamp("2026-10-08T12:34:56Z"), Some(1_791_462_896));
        assert_eq!(
            parse_timestamp("2026-10-08T12:34:56.789Z"),
            Some(1_791_462_896)
        );
        assert_eq!(parse_timestamp("2026-10-08"), None);
        assert_eq!(parse_timestamp("2026-13-08T00:00:00Z"), None);
        assert_eq!(parse_timestamp("2026-10-08T12:34:56+02:00"), None);
    }
}
