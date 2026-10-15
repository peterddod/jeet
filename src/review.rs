//! Reviews: one per pull request, a row of `reviews` keyed by (repo, PR)
//! that is not a task. It has no parent, children or brief, one worktree
//! and one session (columns of its row), and its own directory under
//! `tasks/`, where its agent runs and keeps its status.
//!
//! `jeet review` finds or makes the PR's review ([`review_for`]), checks the
//! PR out in it, and asks its agent to review it ([`start_agent`]): the
//! agent runs in the review's tmux pane like a task's, with `[review]
//! prompt` as its first prompt. A review asked for again types the prompt
//! into that session, resuming it if it is not running, never a second.
//! Every review gets `[review]`'s time and turns afresh each time it is
//! asked ([`give_budget`]), and one gate: it reads, and reports to the task
//! that asked for it, if any ([`crate::permissions::Caller::Review`]).
//!
//! The rest is the legacy, deprecated `[review] command`: a background job.
//! A job is the `job_*` columns of the review's row, plus a log under
//! `$JEET_HOME/reviews`, keyed by repository and PR number. `jeet review`
//! records the job and launches the hidden `jeet review-job <review id>`
//! runner detached from the terminal; the runner runs the configured
//! command with its output going to the log, records how it ended, and
//! raises a desktop notification. Anything that wants to know how a review
//! is going — the explorer, a second `jeet review` — reads the row back.
//! Older jeets kept a job as a JSON file there instead; [`adopt_jobs`]
//! makes reviews of the files they left.

use std::cell::RefCell;
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::AgentSpec;
use crate::config::ReviewConfig;
use crate::context::App;
use crate::db::anchors::{Anchor, AnchorKind};
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{TaskSessionRecord, TaskState, TaskWorktreeRecord};
use crate::db::{Database, RepoRecord};
use crate::git;
use crate::github::PullRequest;
use crate::identity;
use crate::manager::run::{self, StartGate, StartGuard};
use crate::messages::{self, Delivered, Inbox, Sender};
use crate::remote;
use crate::runtime::{self, Admission, Launch, Running, Started};
use crate::status::{self, Activity};
use crate::store;
use crate::tasks;
use crate::tmux;
use crate::worktrees::{self, now_secs};

pub use crate::db::reviews::Job;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Finished(i32),
    /// Never recorded an ending and its process is gone: killed, or the
    /// machine went down under it.
    Lost,
}

impl Job {
    pub fn state(&self) -> JobState {
        match (self.exit_code, self.lost) {
            (Some(code), _) => JobState::Finished(code),
            (None, true) => JobState::Lost,
            (None, false) => JobState::Running,
        }
    }

    /// One line on where the job stands, for the explorer and the CLI.
    pub fn describe(&self) -> String {
        let ago = |at: i64| crate::agent::age_label(at);
        match self.state() {
            JobState::Running => format!("running (started {})", ago(self.started_at)),
            // Adopted from a job file whose process was gone: no end was
            // ever recorded.
            JobState::Finished(_) if self.finished_at.is_none() => {
                "stopped without finishing".to_string()
            }
            JobState::Finished(0) => {
                format!(
                    "finished {}",
                    ago(self.finished_at.unwrap_or(self.started_at))
                )
            }
            JobState::Finished(code) => format!(
                "failed with status {code} {}",
                ago(self.finished_at.unwrap_or(self.started_at))
            ),
            JobState::Lost => "stopped without finishing".to_string(),
        }
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    true
}

/// Where the logs of `repo`'s review jobs go.
fn log_dir(app: &App, repo: &RepoRecord) -> Result<PathBuf> {
    let id = remote::identity_from_id(&repo.id)?;
    Ok(app
        .home
        .join("reviews")
        .join(id.host)
        .join(id.owner)
        .join(id.repo))
}

/// The job recorded for `pr`, if one was ever started.
pub fn load(app: &App, repo: &RepoRecord, pr: u64) -> Option<Job> {
    job_of(app, &app.db.find_review(&repo.id, pr).ok()??)
}

/// The job recorded for `review`, as its row says now.
pub fn job_of(app: &App, review: &ReviewRecord) -> Option<Job> {
    let mut job = app.db.get_review(&review.id).ok()??.job?;
    job.lost = job.exit_code.is_none() && job.pid.is_some_and(|pid| !process_alive(pid));
    Some(job)
}

/// `activity`, the review's agent's, with a review job still running for
/// review `id` taking the place of an agent that is not: the job is
/// headless, with no pane or hooks to say it runs.
pub fn with_job(activity: Activity, db: Option<&Database>, id: &str) -> Activity {
    if !matches!(
        activity,
        Activity::NotStarted | Activity::NotRunning | Activity::Ended
    ) {
        return activity;
    }
    let running = db
        .and_then(|db| db.get_review(id).ok().flatten())
        .and_then(|review| running_in(&review));
    match running {
        Some(job) => Activity::Running {
            detail: Some(format!("review of #{}", job.pr)),
        },
        None => activity,
    }
}

/// `review`'s job, if it is still running.
fn running_in(review: &ReviewRecord) -> Option<Job> {
    let mut job = review.job.clone()?;
    job.lost = job.exit_code.is_none() && job.pid.is_some_and(|pid| !process_alive(pid));
    (job.state() == JobState::Running).then_some(job)
}

/// The review job still running for `review`, if one is.
pub fn running_job(app: &App, review: &ReviewRecord) -> Option<Job> {
    running_in(&app.db.get_review(&review.id).ok()??)
}

/// Why `review`'s agent is not opened while its review command runs: the
/// command is headless, with no pane to attach to, and an agent started
/// beside it would be a blank session.
pub fn running_refusal(review: &ReviewRecord, job: &Job) -> String {
    format!(
        "the review of #{} in {} is still running in the background ({}), headless, so \
         there is nothing to attach to yet; its output is in {}. Open the review once it has \
         finished to resume its session",
        job.pr,
        review.slug,
        job.describe(),
        job.log.display()
    )
}

/// Make the session the review job left in the review's worktree the
/// review's own, so opening the review resumes it.
///
/// The review command runs in the PR's worktree, so its harness files its
/// sessions under the worktree, while the review's agent runs in the review
/// directory and finds only the sessions filed there. Copied there, the
/// latest is recorded as the review's one session if it has none yet. Only
/// once the job has finished: until then it is still writing. Best effort:
/// what fails is a warning. Nothing for a harness that keeps no sessions
/// jeet knows of.
pub fn adopt_sessions(app: &App, review: &ReviewRecord) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(spec) = AgentSpec::from_config(&app.config) else {
        return warnings;
    };
    if !spec.supports_sessions() {
        return warnings;
    }
    let dir = dir(app, review);
    if !dir.is_dir() {
        return warnings;
    }
    // Only a finished job's: a copy is never brought up to date, so one
    // taken mid-review would miss the rest of it.
    let Some(job) = job_of(app, review).filter(|job| job.state() != JobState::Running) else {
        return warnings;
    };
    let copied = match spec.harness.copy_sessions(&job.worktree, &dir) {
        Ok(copied) => copied,
        Err(e) => {
            warnings.push(format!(
                "could not pick up the review of #{}'s {} sessions ({e:#})",
                job.pr,
                spec.harness.name()
            ));
            return warnings;
        }
    };
    let latest = copied.iter().max_by_key(|session| session.modified);
    if let (Some(session), None) = (latest, &review.session_id) {
        if let Err(e) = app.db.set_review_session(&review.id, &session.id) {
            warnings.push(format!("could not record session {} ({e:#})", session.id));
        }
    }
    warnings
}

/// The worktree of every review job ever recorded, under any repository.
pub fn recorded_worktrees(app: &App) -> Vec<PathBuf> {
    recorded_jobs(app)
        .into_iter()
        .map(|(_, job)| job.worktree)
        .collect()
}

/// Every review job ever recorded, under any repository, with its
/// review's id.
fn recorded_jobs(app: &App) -> Vec<(String, Job)> {
    app.db
        .list_reviews(None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|review| Some((review.id, review.job?)))
        .collect()
}

/// Why a new job should not start, given the one already recorded — or
/// nothing, when it should.
pub fn reason_to_skip(existing: Option<&Job>, head: &str) -> Option<String> {
    let job = existing?;
    match job.state() {
        JobState::Running => Some(format!("a review of #{} is already running", job.pr)),
        JobState::Finished(0) if job.head == head => Some(format!(
            "#{} was already reviewed at this commit ({}); pass --rerun to review it again",
            job.pr,
            job.describe()
        )),
        _ => None,
    }
}

/// The configured command with `{pr}` and `{url}` filled in.
///
/// Only those two: both are GitHub's own, digits and a URL built from the
/// repository's name. A branch or title comes from whoever opened the PR, and
/// pasted into a shell command it runs as code — so those go in the
/// environment, where the command can quote them itself.
pub fn expand(template: &str, pr: &PullRequest) -> String {
    template
        .replace("{pr}", &pr.number.to_string())
        .replace("{url}", &pr.url)
}

fn job_env(pr: &PullRequest, log: &Path) -> Vec<(String, String)> {
    vec![
        ("JEET_PR".into(), pr.number.to_string()),
        ("JEET_PR_URL".into(), pr.url.clone()),
        ("JEET_PR_TITLE".into(), pr.title.clone()),
        ("JEET_PR_BRANCH".into(), pr.head_ref_name.clone()),
        ("JEET_PR_BASE".into(), pr.base_ref_name.clone()),
        ("JEET_REVIEW_LOG".into(), log.to_string_lossy().to_string()),
    ]
}

// ---------------------------------------------------------------------------
// A review's life: made, checked out, archived, brought back, deleted
// ---------------------------------------------------------------------------

/// The review's directory under `tasks/`: where its agent runs and keeps
/// its status, and where its worktree is.
pub fn dir(app: &App, review: &ReviewRecord) -> PathBuf {
    app.tasks_root().join(&review.slug)
}

/// Where the review's worktree is, when it has one.
pub fn worktree_path(app: &App, review: &ReviewRecord) -> Option<PathBuf> {
    Some(dir(app, review).join(review.dir_name.as_deref()?))
}

/// The review's agent's tmux session.
pub fn session_name(review: &ReviewRecord) -> String {
    runtime::session_name_for(&review.slug)
}

/// What names the review's agent in messages: `review jeet#55`.
pub fn subject(review: &ReviewRecord) -> String {
    format!("review {}", review.label())
}

/// `review-<repo>-<n>`: what `jeet review` calls the review of a PR.
pub fn review_name(repo: &RepoRecord, number: u64) -> String {
    format!("review-{}-{number}", tasks::repo_short_name(repo))
}

/// The jeet skill in a review's directory: what its agent may do.
const SKILL: &str = r#"---
name: jeet
description: How to work in this jeet review directory.
---

# jeet review

You are reviewing one pull request. Its checkout is the worktree in this
directory; the review is yours to read and judge, not to change.

- `jeet ls`, `jeet path` and `jeet prs` read; nothing else of jeet's runs.
- You may not push, merge, approve, or comment on the pull request: say what
  you found in this session, and the user decides.
- When the assistant asked for this review, `jeet report done|progress|blocked|question "…"`
  tells it how the review went. When you were asked by the user, there is
  no one to report to.
"#;

/// A PR's review, ready to review in.
pub struct ReviewCheckout {
    pub review: ReviewRecord,
    /// The worktree with the PR checked out.
    pub path: PathBuf,
    /// The review was made just now.
    pub created: bool,
    pub warnings: Vec<String>,
}

/// Find or make the review of `pr` and check the PR out in it.
///
/// The review is found by the PR it is of, so a second repository with the
/// same short name cannot cross wires, and there is never a second for one
/// PR. An archived one is brought back; one whose checkout is there already
/// is brought up to date with the PR when clean. A review made here for a
/// PR that cannot be checked out is undone — it holds nothing yet.
///
/// A review made here is `requested_by` the task that asked, if any; one
/// found keeps who asked (`jeet review` says who asks now, with
/// [`start_agent`]).
pub fn review_for(
    app: &App,
    repo: &RepoRecord,
    pr: &PullRequest,
    requested_by: Option<&str>,
) -> Result<ReviewCheckout> {
    let mut warnings = Vec::new();
    let (mut review, created) = match app.db.find_review(&repo.id, pr.number)? {
        Some(review) => (review, false),
        None => make_review(app, repo, pr, requested_by)?,
    };
    if review.state == TaskState::Archived {
        warnings.extend(unarchive(app, &review)?);
        review = app.db.get_review(&review.id)?.unwrap_or(review);
    }
    match check_out(app, &mut review, repo, pr) {
        Ok((path, more)) => {
            warnings.extend(more);
            if let Err(e) = app
                .db
                .set_review_pr(&review.id, &pr.title, &pr.state, Some(&pr.url))
            {
                warnings.push(format!("could not record #{}'s title: {e:#}", pr.number));
            }
            let _ = app.db.touch_review(&review.id, now_secs());
            let review = app.db.get_review(&review.id)?.unwrap_or(review);
            Ok(ReviewCheckout {
                review,
                path,
                created,
                warnings,
            })
        }
        Err(e) if created => Err(match delete(app, &review) {
            Ok(_) => e,
            Err(undo) => e.context(format!(
                "and the half-made review could not be removed ({undo:#}); \
                 `jeet task delete {} --force` cleans it up",
                review.slug
            )),
        }),
        Err(e) => Err(e),
    }
}

/// Make the review of `pr`: its directory, marker and skill, then its row.
/// Another `jeet review` of the same PR may have made it meanwhile: that
/// one is then the review, and this one's directory goes. True when this
/// made it.
fn make_review(
    app: &App,
    repo: &RepoRecord,
    pr: &PullRequest,
    requested_by: Option<&str>,
) -> Result<(ReviewRecord, bool)> {
    let slug = tasks::unique_slug(app, &tasks::slugify(&review_name(repo, pr.number)))?;
    let mut review = ReviewRecord::new(
        Uuid::new_v4().to_string(),
        slug,
        &repo.id,
        pr.number,
        now_secs(),
    );
    review.title = pr.title.clone();
    review.pr_state = Some(pr.state.clone());
    review.pr_url = Some(pr.url.clone());
    review.requested_by = requested_by.map(str::to_string);
    let dir = dir(app, &review);
    std::fs::create_dir_all(app.tasks_root()).context("create the tasks directory")?;
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let made = tasks::write_marker(&dir, &review.id)
        .and_then(|()| install_skill(&dir))
        .and_then(|()| app.db.insert_review(&review));
    match made {
        Ok(()) => Ok((review, true)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            match app.db.find_review(&repo.id, pr.number)? {
                Some(found) => Ok((found, false)),
                None => Err(e),
            }
        }
    }
}

fn install_skill(dir: &Path) -> Result<()> {
    let skill = dir.join(".claude").join("skills").join("jeet");
    std::fs::create_dir_all(&skill).with_context(|| format!("create {}", skill.display()))?;
    std::fs::write(skill.join("SKILL.md"), SKILL).context("write the jeet skill")
}

/// Somewhere in a task to look at `pr`, for the explorer's PR list: the task
/// worktree that has the PR's branch already — the author's own, say —
/// brought up to date with the PR when clean, else the PR's review, as
/// `jeet review` makes it but without starting a review. Never a checkout
/// outside a task or review.
pub fn browse_pr(app: &App, repo: &RepoRecord, pr: &PullRequest) -> Result<BrowsedPr> {
    let trunk = PathBuf::from(&repo.trunk_path);
    let branch = pr.local_branch();
    let holder = git::worktree_list(&trunk)?
        .into_iter()
        .find(|e| e.branch.as_deref() == Some(branch.as_str()) && e.path.is_dir());
    if let Some(entry) = holder {
        if let Some(task) = tasks::task_owning(app, &entry.path)? {
            if task.state == TaskState::Active {
                // `gh pr checkout` fetches into the trunk.
                let _fetch = store::lock_repo(&app.home, &repo.id)?;
                let warnings = worktrees::refresh_pr_worktree(&entry.path, pr, Some(&branch));
                let _ = app.db.touch_task(&task.id, now_secs());
                return Ok(BrowsedPr {
                    path: entry.path,
                    warnings,
                    created: false,
                    owner: Owner::Task(task),
                });
            }
        }
    }
    let checkout = review_for(app, repo, pr, None)?;
    Ok(BrowsedPr {
        path: checkout.path,
        warnings: checkout.warnings,
        created: checkout.created,
        owner: Owner::Review(Box::new(checkout.review)),
    })
}

/// Who holds the checkout the explorer looks at.
pub enum Owner {
    Task(crate::db::tasks::TaskRecord),
    Review(Box<ReviewRecord>),
}

/// Where the explorer looks at a PR.
pub struct BrowsedPr {
    pub path: PathBuf,
    pub warnings: Vec<String>,
    /// Made just now.
    pub created: bool,
    pub owner: Owner,
}

/// The review's checkout of `pr`: the one it has, refreshed, or a new one.
/// The PR's branch is used unless another worktree has it — the author's
/// own task, say — and then the checkout is detached at the PR's head
/// instead, which reviews the same code without taking the branch.
fn check_out(
    app: &App,
    review: &mut ReviewRecord,
    repo: &RepoRecord,
    pr: &PullRequest,
) -> Result<(PathBuf, Vec<String>)> {
    let dir = dir(app, review);
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }
    // `gh pr checkout` fetches into the trunk: keep a store sync out of its way.
    let _fetch = store::lock_repo(&app.home, &repo.id)?;
    // Another `jeet review` of this PR may have made the worktree while this
    // one waited for the lock: it is the review's one, so use it.
    if let Some(fresh) = app.db.get_review(&review.id)? {
        *review = fresh;
    }

    let existing = review.worktree();
    if let Some(row) = &existing {
        let path = dir.join(&row.dir_name);
        if path.is_dir() {
            let warnings = worktrees::refresh_pr_worktree(&path, pr, row.branch.as_deref());
            return Ok((path, warnings));
        }
        // Gone from under jeet: forget it, and have git forget it too, or
        // its branch would still count as checked out there. An archive that
        // stopped part-way leaves what is needed to bring it back, and that
        // stays.
        if row.head_oid.is_none() && row.stash_ref.is_none() {
            app.db.set_review_worktree(&review.id, None)?;
            let _ = git::worktree_prune(&trunk);
        } else {
            bail!(
                "the review's checkout was archived and not brought back; \
                 `jeet task unarchive {}` first",
                review.slug
            );
        }
    }

    let mut warnings = Vec::new();
    let branch = pr.local_branch();
    let use_branch = match tasks::branch_holder(app, &trunk, &branch)? {
        Some(holder) => {
            warnings.push(format!(
                "{branch} is checked out {holder}, so the review is detached at the PR's head"
            ));
            None
        }
        None => Some(branch.as_str()),
    };
    let existed = git::branch_exists(&trunk, &branch)?;
    let dir_name = tasks::pick_dir_name(&dir, &[], &tasks::repo_short_name(repo));
    let dest = dir.join(&dir_name);
    worktrees::add_pr_worktree(&trunk, &repo.default_branch, &dest, pr, use_branch)?;

    // A branch made for a checkout that vanished is still the review's own.
    let made_before = existing
        .as_ref()
        .is_some_and(|gone| gone.created_branch && gone.branch.as_deref() == Some(branch.as_str()));
    let created_branch = use_branch.is_some() && (!existed || made_before);
    let record = TaskWorktreeRecord {
        id: 0,
        task_id: review.id.clone(),
        repo_id: repo.id.clone(),
        dir_name,
        branch: use_branch.map(str::to_string),
        created_branch,
        head_oid: None,
        stash_ref: None,
        pr_number: Some(pr.number),
        created_at: now_secs(),
        read_only: false,
    };
    let recorded = app.db.set_review_worktree(&review.id, Some(&record));
    if !matches!(recorded, Ok(true)) {
        let _ = git::worktree_remove(&trunk, &dest, true);
        if created_branch {
            let _ = git::delete_local_branch(&trunk, &branch);
        }
        if let Err(e) = recorded {
            return Err(e.context("record the worktree"));
        }
        // The row has another worktree by now: that is the review's.
        let Some(fresh) = app.db.get_review(&review.id)? else {
            bail!("{} is not in the index", subject(review));
        };
        let Some(row) = fresh.worktree() else {
            bail!(
                "could not record the worktree: {} has another",
                subject(review)
            );
        };
        let path = dir.join(&row.dir_name);
        *review = fresh;
        return Ok((path, warnings));
    }
    review.dir_name = Some(record.dir_name);
    Ok((dest, warnings))
}

/// Keep `row` as review `id`'s worktree, the one it already has.
fn save_worktree(app: &App, id: &str, row: &TaskWorktreeRecord) -> Result<()> {
    if !app.db.set_review_worktree(id, Some(row))? {
        bail!("the review has another worktree than {}", row.dir_name);
    }
    Ok(())
}

/// Charge `review` the time its agent's turn in progress has run, before
/// its session is ended for archiving or deleting: no hook charges it after,
/// and no sweep looks at it again. A charge that uses a limit up is not
/// reported, as a stop's is not.
fn charge_before_end(app: &App, review: &ReviewRecord, warnings: &mut Vec<String>) {
    let alive = tmux::available() && tmux::agent_alive(&session_name(review));
    if let Err(e) = status::fold_busy_review(app, review, alive, now_secs()) {
        warnings.push(format!("could not charge its limits: {e:#}"));
    }
}

/// End `review`'s agent and the review jobs that run in its directory, and
/// forget their processes: the agent works in the worktree about to be
/// removed, and what it wrote after the snapshot would be lost. Refused
/// from inside the agent's session or under a job, which ending would end
/// this jeet with. Returns how many jobs were still running.
fn end_agent(
    app: &App,
    review: &ReviewRecord,
    verb: &str,
    warnings: &mut Vec<String>,
) -> Result<usize> {
    let dir = dir(app, review);
    runtime::refuse_inside_agent_session(&review.id, &review.slug, &subject(review), verb)?;
    refuse_inside_job(app, &review.id, &dir, verb)?;
    charge_before_end(app, review, warnings);
    runtime::stop_session(&review.slug).context("end the agent's tmux session")?;
    let jobs = stop_jobs(&app.db, &review.id).context("end the review's job")?
        + stop_jobs_in(app, &dir).context("end the review's job")?;
    app.db.delete_task_anchors(&review.id, AnchorKind::Pane)?;
    app.db.delete_task_anchors(&review.id, AnchorKind::Job)?;
    // An adopted job, started by an older jeet, has no runner jeet knows.
    if let Some(job) = running_job(app, review) {
        warnings.push(format!(
            "its review job (pid {}) keeps running; jeet did not start it, so it is not stopped",
            job.pid.unwrap_or(0)
        ));
    }
    if let Err(e) = runtime::remove_settings(&app.home, &review.id) {
        warnings.push(format!("could not remove the agent's settings: {e:#}"));
    }
    Ok(jobs)
}

/// End `review`'s agent and jobs for `jeet manager stop --all`, and leave
/// the review as it is: not archived, and started again the next time it is
/// asked for or opened. Says what came of it.
pub fn stop(app: &App, review: &ReviewRecord) -> Result<String> {
    let mut warnings = Vec::new();
    let alive = tmux::available() && tmux::agent_alive(&session_name(review));
    let jobs = end_agent(app, review, "stop", &mut warnings)?;
    let mut ended = Vec::new();
    if alive {
        ended.push("stopped its agent".to_string());
    }
    match jobs {
        0 => {}
        1 => ended.push("stopped its review job".to_string()),
        n => ended.push(format!("stopped its {n} review jobs")),
    }
    ended.extend(warnings);
    Ok(match ended.is_empty() {
        true => "not running".to_string(),
        false => ended.join("; "),
    })
}

/// Archive `review`: end its agent, free the disk its worktree takes while
/// keeping what is needed to bring it back as it was, as `jeet task archive`
/// does a task's. The review is marked archived first, so a failure
/// part-way leaves it archived with its worktree still there, and running
/// archive again finishes the job. Files git ignores are not kept; the
/// caller has already confirmed what [`unkept`] lists.
pub fn archive(app: &App, review: &ReviewRecord) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    end_agent(app, review, "archive", &mut warnings)?;
    app.db.set_review_state(&review.id, TaskState::Archived)?;
    app.db.unqueue_review(&review.id)?;
    let root = dir(app, review);
    if let Some(mut row) = review.worktree() {
        let save = |row: &TaskWorktreeRecord| save_worktree(app, &review.id, row);
        warnings.extend(tasks::archive_worktree(
            app, &review.id, &root, &mut row, &save,
        )?);
    }
    Ok(warnings)
}

/// Bring an archived review's worktree back: on its branch, or detached at
/// its old HEAD, with its uncommitted changes reapplied. The review becomes
/// active only once it is back, and is noted as brought back, so one whose
/// PR is done is not archived again behind the user's back ([`retire`]).
pub fn unarchive(app: &App, review: &ReviewRecord) -> Result<Vec<String>> {
    let root = dir(app, review);
    if !root.is_dir() {
        bail!("the review directory {} is missing", root.display());
    }
    let mut warnings = Vec::new();
    if let Some(mut row) = review.worktree() {
        let save = |row: &TaskWorktreeRecord| save_worktree(app, &review.id, row);
        match tasks::restore_worktree(app, &review.id, &root, &mut row, &save, &mut warnings)? {
            tasks::Restored::Forgotten => {
                app.db.set_review_worktree(&review.id, None)?;
            }
            tasks::Restored::Kept => {}
        }
    }
    app.db.set_review_state(&review.id, TaskState::Active)?;
    let now = now_secs();
    app.db.set_review_brought_back(&review.id, now)?;
    app.db.touch_review(&review.id, now)?;
    Ok(warnings)
}

/// What archiving `review` would not keep, one line per worktree: the files
/// git ignores in it, which go with it. See [`tasks::unkept`].
pub fn unkept(app: &App, review: &ReviewRecord) -> Vec<String> {
    let rows: Vec<TaskWorktreeRecord> = review.worktree().into_iter().collect();
    tasks::unkept_in(&dir(app, review), &rows)
}

/// What deleting `review` would destroy, one line per problem. See
/// [`tasks::losses`].
pub fn losses(app: &App, review: &ReviewRecord) -> Result<Vec<String>> {
    let rows: Vec<TaskWorktreeRecord> = review.worktree().into_iter().collect();
    tasks::losses_of(app, &review.id, &dir(app, review), &rows)
}

/// Delete `review` and everything it owns: its agent, its worktree, the
/// branch it made, its `refs/jeet` refs, its directory and its row. The
/// caller has already confirmed what [`losses`] lists. The row goes last,
/// so a failure part-way leaves a review that can be deleted again.
pub fn delete(app: &App, review: &ReviewRecord) -> Result<Vec<String>> {
    if review.slug.is_empty() || review.slug.starts_with('.') || review.slug.contains(['/', '\\']) {
        bail!(
            "refusing to delete a review whose directory name is {:?}",
            review.slug
        );
    }
    let mut warnings = Vec::new();
    end_agent(app, review, "delete", &mut warnings)?;
    let rows: Vec<TaskWorktreeRecord> = review.worktree().into_iter().collect();
    tasks::remove_owned(app, &review.id, &dir(app, review), &rows, &mut warnings)?;
    app.db.delete_review(&review.id)?;
    Ok(warnings)
}

// ---------------------------------------------------------------------------
// The review's agent
// ---------------------------------------------------------------------------

/// What `review`'s agent is let run without a permission prompt: the
/// read-only commands, and `jeet report` when a task asked for the review.
fn allowed(review: &ReviewRecord) -> Vec<String> {
    let mut allow: Vec<String> = ["jeet ls", "jeet prs", "jeet path"]
        .map(str::to_string)
        .to_vec();
    if review.requested_by.is_some() {
        allow.push("jeet report".to_string());
    }
    allow
}

/// The first prompt of the review's agent is `[review] prompt`, typed or
/// started as any message to an agent is ([`messages::deliver`]); this is
/// the agent as that sees it.
pub struct ReviewAgent<'a> {
    app: &'a App,
    review: &'a ReviewRecord,
    /// May start it when it is not running; see [`ReviewAgent::may_start`].
    may_start: bool,
    /// What its start warns of, kept for the caller rather than printed;
    /// see [`ReviewAgent::keep_warnings`].
    warnings: Option<RefCell<Vec<String>>>,
    /// The session a start resumes; see [`ReviewAgent::resuming`].
    resume: Option<&'a str>,
}

impl<'a> ReviewAgent<'a> {
    pub fn new(app: &'a App, review: &'a ReviewRecord) -> Self {
        Self {
            app,
            review,
            may_start: true,
            warnings: None,
            resume: None,
        }
    }

    /// A start resumes `session` rather than the review's own.
    pub fn resuming(self, session: Option<&'a str>) -> Self {
        Self {
            resume: session,
            ..self
        }
    }

    /// Keep what its start warns of for [`ReviewAgent::warnings`] rather
    /// than print it: for a caller with no stderr to print to.
    pub fn keep_warnings(self) -> Self {
        Self {
            warnings: Some(RefCell::new(Vec::new())),
            ..self
        }
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings
            .as_ref()
            .map_or_else(Vec::new, |w| w.borrow().clone())
    }

    /// Without `may_start`, a message for it while it is not running waits
    /// in its inbox rather than start it.
    pub fn may_start(self, may_start: bool) -> Self {
        Self { may_start, ..self }
    }
}

impl messages::Agent for ReviewAgent<'_> {
    fn alive(&self) -> bool {
        tmux::available() && tmux::pane_alive(&session_name(self.review))
    }

    fn has_hooks(&self) -> bool {
        AgentSpec::from_config(&self.app.config).is_ok_and(|s| s.has_hooks())
    }

    fn type_in(&self, text: &str) -> Result<()> {
        // Before it is typed: a pause that finds it idle in between waits
        // for the turn this starts, which no hook has reported yet.
        if let Err(e) = status::mark_typed(&dir(self.app, self.review)) {
            eprintln!("jeet: warning: could not note the message typed in: {e:#}");
        }
        tmux::send_text(&session_name(self.review), text)
    }

    fn has_draft(&self) -> bool {
        let Ok(spec) = AgentSpec::from_config(&self.app.config) else {
            return false;
        };
        tmux::capture_styled(&session_name(self.review))
            .is_ok_and(|pane| spec.harness.has_draft(&pane))
    }

    fn can_start(&self) -> bool {
        self.may_start && tmux::may_start_session()
    }

    fn first_prompt_refusal(&self) -> Option<String> {
        runtime::first_prompt_refusal_for(self.app, &self.review.slug)
    }

    /// Paused, as its own limits say now: it is handed nothing until asked
    /// again.
    fn held(&self) -> Result<bool> {
        Ok(self
            .app
            .db
            .get_review(&self.review.id)?
            .is_some_and(|r| r.paused))
    }

    fn admit(&self) -> Result<Option<StartGate>> {
        run::admit_review(self.app, self.review).map(Some)
    }

    fn queued(&self, live: u32, max: u32) {
        run::report_review_queued(self.app, self.review, live, max, i64::MAX);
    }

    fn queue_held_start(&self) -> Result<()> {
        // Nor is a start queued on a harness that takes no first prompt: it
        // would start the agent with none, the message left untyped.
        if self.first_prompt_refusal().is_some() {
            return Ok(());
        }
        self.app.db.queue_review(&self.review.id, now_secs())?;
        Ok(())
    }

    fn start(&self, prompt: &str, guard: Option<&StartGuard>) -> Result<Started> {
        let running = match guard {
            Some(guard) => {
                ensure_running_admitted(self.app, self.review, self.resume, Some(prompt), guard)?
            }
            None => ensure_running(
                self.app,
                self.review,
                self.resume,
                Some(prompt),
                Admission::Gate,
            )?,
        };
        match &self.warnings {
            Some(kept) => kept.borrow_mut().extend(running.warnings),
            None => {
                for warning in &running.warnings {
                    eprintln!("jeet: {warning}");
                }
            }
        }
        Ok(running.started)
    }
}

/// Make sure `review`'s agent is running, starting it if not: resuming
/// `session` when given, else the review's session, else a new one. An
/// agent this starts gets `prompt` as its first prompt; one already
/// running does not. Given none, it starts with what waits in its inbox, as
/// a drain would. A start is admitted first ([`run::admit_review`]): by
/// `admission` while `[review] max_live` agents run.
pub fn ensure_running(
    app: &App,
    review: &ReviewRecord,
    session: Option<&str>,
    prompt: Option<&str>,
    admission: Admission,
) -> Result<Running> {
    // Running already: nothing starts, so there is nothing to admit.
    let name = session_name(review);
    if tmux::has_session(&name) && tmux::pane_alive(&name) {
        return Ok(already_running(&name, session));
    }
    if prompt.is_some() {
        if let Some(why) = runtime::first_prompt_refusal_for(app, &review.slug) {
            bail!(why);
        }
    }
    let dir = dir(app, review);
    let inbox = match prompt {
        None if dir.is_dir() => Some(Inbox::lock(&dir)?),
        _ => None,
    };
    let (guard, over) = match admission {
        Admission::Gate => match run::admit_review(app, review)? {
            StartGate::Go(guard) => (guard, None),
            StartGate::Queue { live, max, .. } => {
                return Ok(Running {
                    tmux_session: name,
                    started: Started::Queued {
                        live,
                        max,
                        prompt: prompt.map(str::to_string),
                    },
                    warnings: Vec::new(),
                })
            }
            StartGate::Held(_) => bail!(
                "{} is paused: it has used its [review] time or turns; ask for the review again",
                subject(review)
            ),
        },
        Admission::Bypass => run::admit_review_over(app, review)?,
    };
    let mut running = match &inbox {
        Some(inbox) => start_with_inbox(app, review, session, inbox, &guard)?,
        None => ensure_running_admitted(app, review, session, prompt, &guard)?,
    };
    if let Some((live, max)) = over {
        running.warnings.push(format!(
            "over [review] max_live ({live}/{max} live already)"
        ));
    }
    Ok(running)
}

/// [`ensure_running`] for a start admitted already, under `_guard`.
pub fn ensure_running_admitted(
    app: &App,
    review: &ReviewRecord,
    session: Option<&str>,
    prompt: Option<&str>,
    _guard: &StartGuard,
) -> Result<Running> {
    let running = start_tmux(app, review, session, prompt)?;
    // Started, however it was admitted: no longer waits on `max_live`, or a
    // drain would start it again once it ends.
    if running.started == Started::Now {
        app.db.unqueue_review(&review.id)?;
    }
    Ok(running)
}

/// Start `review`'s agent, admitted under `guard`, resuming `session` when
/// given, with what waits for it in its locked `inbox` as its first prompt,
/// as much of it as fits, or with none when nothing does, or when its
/// harness takes no first prompt from jeet.
pub fn start_with_inbox(
    app: &App,
    review: &ReviewRecord,
    session: Option<&str>,
    inbox: &Inbox,
    guard: &StartGuard,
) -> Result<Running> {
    let takes_prompt = AgentSpec::from_config(&app.config)
        .is_ok_and(|spec| spec.harness.prompt_args("").is_some());
    if !takes_prompt || inbox.read_for(None)?.is_empty() {
        return ensure_running_admitted(app, review, session, None, guard);
    }
    let agent = ReviewAgent::new(app, review)
        .keep_warnings()
        .resuming(session);
    let started = match messages::start_with_queue(inbox, &agent, Some(guard))? {
        // It came up meanwhile, and was typed the queue.
        Delivered::Typed => Started::AlreadyRunning,
        _ => Started::Now,
    };
    Ok(Running {
        tmux_session: session_name(review),
        started,
        warnings: agent.warnings(),
    })
}

/// The review's agent, running already in tmux session `name`: attached to,
/// not started, with what to warn of.
fn already_running(name: &str, session: Option<&str>) -> Running {
    let mut warnings = Vec::new();
    if let Some(id) = session {
        warnings.push(format!(
            "the agent is already running in tmux session {name}, so session {id} was not \
             resumed; end it there and open the review again to switch"
        ));
    }
    Running {
        tmux_session: name.to_string(),
        started: Started::AlreadyRunning,
        warnings,
    }
}

/// [`ensure_running`] in the review's tmux session.
fn start_tmux(
    app: &App,
    review: &ReviewRecord,
    session: Option<&str>,
    prompt: Option<&str>,
) -> Result<Running> {
    tmux::require()?;
    let name = session_name(review);
    if runtime::live_session(&name)? {
        return Ok(already_running(&name, session));
    }
    let spec = AgentSpec::from_config(&app.config)?;
    let dir = dir(app, review);
    if !dir.is_dir() {
        bail!("the review directory {} is missing", dir.display());
    }
    // A review job's session is filed under its worktree: picked up first,
    // so it is the one resumed, not a blank one started beside it.
    let warnings = adopt_sessions(app, review);
    let review = &app
        .db
        .get_review(&review.id)?
        .unwrap_or_else(|| review.clone());
    // Only a harness with sessions has hooks to install.
    let hooks = match spec.supports_sessions() {
        true => {
            let exe = std::env::current_exe().context("locate the jeet binary")?;
            Some(review_hooks(app, review, spec.harness, exe)?)
        }
        false => None,
    };
    let known_sessions = review
        .session_id
        .iter()
        .map(|id| TaskSessionRecord {
            session_id: id.clone(),
            task_id: review.id.clone(),
            parent_id: None,
            name: review.slug.clone(),
            created_at: review.created_at,
            last_activity: review.last_activity,
        })
        .collect();
    let launch = Launch {
        id: review.id.clone(),
        slug: review.slug.clone(),
        subject: subject(review),
        name: review.slug.clone(),
        dir,
        known_sessions,
        ungated_warning: true,
        spec,
        hooks,
        warnings,
    };
    let running = runtime::launch_tmux(
        app,
        &launch,
        session,
        prompt,
        |pane| {
            identity::record(
                &app.db,
                &review.id,
                AnchorKind::Pane,
                Some(&pane.id),
                pane.pid,
            )
        },
        |id| app.db.set_review_session(&review.id, id),
    )?;
    let _ = app.db.touch_review(&review.id, now_secs());
    Ok(running)
}

/// jeet's hooks for a review's agent run by `harness`: what it is let run
/// ([`allowed`]), what it is denied ([`crate::permissions::review_deny`]),
/// and no wake channel, which watches a task's inbox.
fn review_hooks(
    app: &App,
    review: &ReviewRecord,
    _harness: &dyn crate::harness::Harness,
    exe: PathBuf,
) -> Result<crate::harness::HookSetup> {
    Ok(crate::harness::HookSetup {
        exe,
        task_id: review.id.clone(),
        allow: allowed(review),
        deny: crate::permissions::review_deny(app)?,
        wake: false,
    })
}

/// What the review's agent is doing. Runs tmux and reads files, so it
/// belongs on a background thread, not in a draw.
pub fn activity(app: &App, review: &ReviewRecord, tmux_ok: bool) -> Activity {
    runtime::activity_at(&dir(app, review), &session_name(review), tmux_ok)
}

/// [`activity`] as lists show it: a headless review job still running
/// counts as its agent running. Only for display: what routes a delivery or
/// charges a limit asks of the agent alone.
pub fn shown_activity(app: &App, review: &ReviewRecord, tmux_ok: bool) -> Activity {
    with_job(activity(app, review, tmux_ok), Some(&app.db), &review.id)
}

/// When anything last happened in the review: jeet touching it, a hook
/// reporting, or a harness recording a session.
pub fn last_activity(app: &App, review: &ReviewRecord) -> i64 {
    runtime::last_activity_at(&dir(app, review), review.last_activity)
}

/// Get `text` to `review`'s agent as any message to it goes: typed in when
/// it is idle, queued while it is busy, held in its inbox when it may not be
/// started (`may_start` false).
pub fn deliver(app: &App, review: &ReviewRecord, text: &str, may_start: bool) -> Result<Delivered> {
    let agent = ReviewAgent::new(app, review).may_start(may_start);
    messages::deliver(&dir(app, review), &agent, text, Sender::Agent)
}

// ---------------------------------------------------------------------------
// Asking the agent
// ---------------------------------------------------------------------------

/// `[review] prompt` with `{pr}`, `{url}` and `{worktree}` filled in: all
/// GitHub's or jeet's own. The PR's branch and title are whoever opened
/// it's, so never pasted into the agent's prompt; it reads them with `gh`.
pub fn expand_prompt(template: &str, pr: &PullRequest, worktree: &Path) -> String {
    expand(template, pr).replace("{worktree}", &worktree.to_string_lossy())
}

/// Why the review agent is not asked again, given what it was last asked
/// and the legacy job recorded for the PR — or nothing, when it should be.
fn reason_not_to_ask(
    asked: Option<&Asked>,
    job: Option<&Job>,
    pr: &PullRequest,
    rerun: bool,
) -> Option<String> {
    // A legacy review still running headless: its session is not the
    // agent's to resume until it has finished.
    if let Some(job) = job.filter(|job| job.state() == JobState::Running) {
        return Some(format!(
            "a background review of #{} is still running ({}); the review agent starts \
             once it has finished",
            job.pr,
            job.describe()
        ));
    }
    if rerun {
        return None;
    }
    if asked.is_some_and(|a| a.pr == pr.number && a.head == pr.head_ref_oid) {
        return Some(format!(
            "the review agent was already asked to review #{} at this commit; pass --rerun \
             to ask it again",
            pr.number
        ));
    }
    match job {
        Some(job) if asked.is_none() => reason_to_skip(Some(job), &pr.head_ref_oid),
        _ => None,
    }
}

/// The PR head a review's agent was last asked to review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asked {
    pub pr: u64,
    pub head: String,
    pub at: i64,
}

/// What `review`'s agent was last asked to review, if anything.
pub fn asked(review: &ReviewRecord) -> Option<Asked> {
    Some(Asked {
        pr: review.pr_number,
        head: review.asked_head.clone()?,
        at: review.asked_at?,
    })
}

/// Have `review`'s agent review `pr`, checked out at `worktree`, as asked
/// by `requested_by` (a task's id; `None`: the user): the one place a
/// review agent is started or asked again, and so where any limit on
/// reviews belongs.
///
/// The prompt goes to the agent as any message to it does
/// ([`messages::deliver`]): typed into it when it is idle, queued while it
/// is busy, and otherwise starting it in its tmux pane, resuming its
/// session so a review keeps the one. One over `[review] max_live` starts
/// when a review ends. Each review gets `[review]`'s time and turns afresh
/// ([`give_budget`]), and its reports go to whoever asks now. Returns what
/// to tell the user.
pub fn start_agent(
    app: &App,
    review: &ReviewRecord,
    pr: &PullRequest,
    worktree: &Path,
    rerun: bool,
    requested_by: Option<&str>,
) -> Result<String> {
    // Who asked sticks: a run by the user never clears the assistant's, and
    // this holds when nothing is asked again.
    remember_requester(app, &review.id, requested_by)?;
    let review = &folded(app, review);
    let job = job_of(app, review);
    if let Some(why) = reason_not_to_ask(asked(review).as_ref(), job.as_ref(), pr, rerun) {
        return Ok(why);
    }
    let prompt = expand_prompt(app.config.review.prompt(), pr, worktree);
    let dir = dir(app, review);
    // Still waiting in its inbox: the worktree it names is at the new head
    // already, so one is enough, not a second review behind it.
    let checked = messages::checked(&prompt)?;
    if messages::peek(&dir).iter().any(|q| q.message == checked) {
        return Ok(format!(
            "the review of #{} is already queued for the review agent",
            pr.number
        ));
    }
    give_budget(app, review)?;
    let review = &app
        .db
        .get_review(&review.id)?
        .unwrap_or_else(|| review.clone());
    let agent = ReviewAgent::new(app, review);
    let delivered = messages::deliver(&dir, &agent, &prompt, Sender::Agent)?;
    if let Err(e) = app
        .db
        .set_review_asked(&review.id, &pr.head_ref_oid, now_secs())
    {
        eprintln!("jeet: warning: could not record the review asked for: {e:#}");
    }
    let session = session_name(review);
    let watch = format!("`jeet task open {}` to watch it", review.slug);
    Ok(match delivered {
        Delivered::Started => {
            format!(
                "reviewing #{} in tmux session {session}; {watch}",
                pr.number
            )
        }
        Delivered::Typed => format!(
            "asked the review agent in tmux session {session} to review #{}; {watch}",
            pr.number
        ),
        Delivered::Queued | Delivered::Wake => format!(
            "the review agent is busy, so the review of #{} is queued for it; {watch}",
            pr.number
        ),
        Delivered::Held => format!(
            "the review of #{} is queued; the agent starts with it when the review is next \
             opened",
            pr.number
        ),
        Delivered::Paused => format!(
            "review {} has used its [review] time or turns, so the review of #{} is queued; \
             it starts once asked for again",
            review.slug, pr.number
        ),
        Delivered::StartQueued { live, max } => format!(
            "the review of #{} waits on [review] max_live ({live}/{max} review agents live); \
             it starts when a review ends",
            pr.number
        ),
    })
}

/// Record `requested_by`, the task that asks for review `id` now, if there
/// is one. The user asking leaves who asked before as it was.
pub fn remember_requester(app: &App, id: &str, requested_by: Option<&str>) -> Result<()> {
    match requested_by {
        Some(by) => app.db.set_review_requested_by(id, Some(by)),
        None => Ok(()),
    }
}

/// Give `review` `[review]`'s caps afresh for the review it is about to be
/// asked: what it has used so far, and `time` and `turns` more, held to as
/// a manager's budget is (warned at 80%, paused at 100%). The pause the
/// last review's limits set is lifted for this one.
fn give_budget(app: &App, review: &ReviewRecord) -> Result<()> {
    let caps = app.config.review.policy()?.budget;
    // What it has used, as the index has it now: hooks charge it meanwhile.
    let Some(review) = app.db.get_review(&review.id)? else {
        bail!("{} is not in the index", subject(review));
    };
    let more = |used: i64, cap: Option<i64>| cap.map(|cap| used.saturating_add(cap));
    app.db.set_review_budget_max(
        &review.id,
        more(review.used_secs, caps.secs),
        more(review.used_turns, caps.turns),
    )?;
    app.db.resume_review(&review.id)?;
    Ok(())
}

/// Start the review command for `pr` in `worktree`, detached from this
/// terminal so it outlives both jeet and the shell it was started from.
///
/// `as_task` is the review's id when a task asked for it: the command then
/// runs as the review's agent, which reports to that task, not as whoever
/// started the review. `runner_for` is the review the runner is recorded as
/// the process of, so archiving or deleting the review finds it to end;
/// the command of a review nobody asked for runs as the user.
#[allow(clippy::too_many_arguments)]
pub fn start(
    app: &App,
    repo: &RepoRecord,
    pr: &PullRequest,
    worktree: &Path,
    config: &ReviewConfig,
    template: &str,
    as_task: Option<&str>,
    runner_for: &str,
) -> Result<Job> {
    let dir = log_dir(app, repo)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let log = dir.join(format!("{}.log", pr.number));

    let mut env = job_env(pr, &log);
    if let Some(id) = as_task {
        env.push((crate::tasks::TASK_ID_ENV.into(), id.into()));
    }
    let mut job = Job {
        pr: pr.number,
        title: pr.title.clone(),
        head: pr.head_ref_oid.clone(),
        worktree: worktree.to_path_buf(),
        command: expand(template, pr),
        env,
        notify: config.notify(),
        log: log.clone(),
        started_at: now_secs(),
        pid: None,
        runner_start: None,
        finished_at: None,
        exit_code: None,
        lost: false,
    };
    app.db.set_review_job(runner_for, &job)?;

    // Once the runner has the go-ahead it is the only writer of the job:
    // two writers is how a quick-failing command's exit status got
    // papered over with a stale copy. So launching records only the runner,
    // before the go-ahead — and that launching failed, which would otherwise
    // leave a job that reads as running forever, refusing every later
    // `jeet review`, `--rerun` included.
    if let Err(e) = launch(app, &log, worktree, runner_for) {
        job.finished_at = Some(now_secs());
        job.exit_code = Some(127);
        let _ = app.db.set_review_job(runner_for, &job);
        return Err(e);
    }
    Ok(job)
}

/// Launch the runner for review `runner_for`'s job, recorded as the
/// review's, so what its command runs is known for the review's, and
/// archiving or deleting the review finds it to end.
fn launch(app: &App, log: &Path, worktree: &Path, runner_for: &str) -> Result<()> {
    let out = File::create(log).with_context(|| format!("create {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    // `nohup` so closing the terminal does not take the review with it — the
    // ignored SIGHUP survives into everything the command runs — and a process
    // group of its own so a ctrl-c meant for the explorer does not either.
    let mut cmd = Command::new("nohup");
    cmd.arg(exe)
        .arg("review-job")
        .arg(runner_for)
        .current_dir(worktree)
        // For the go-ahead: see [`await_go`].
        .stdin(Stdio::piped())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Recorded for a review nobody asked for, the runner is the user's: a
    // hint it inherited (the shell of another task's agent, say) would be
    // taken for a claim to be that task, and what its command runs refused.
    let users = app
        .db
        .get_review(runner_for)?
        .is_some_and(|r| r.requested_by.is_none());
    if users {
        cmd.env_remove(crate::tasks::TASK_ID_ENV)
            .env_remove(crate::permissions::ASSISTANT_ENV);
    }
    let mut child = cmd.spawn().context("start the review command")?;
    hand_over(app, runner_for, users, &mut child)
}

/// Record `child`, the runner [`launch`] just started, as review
/// `runner_for`'s, name it in the review's job, then give it the go-ahead.
/// When it cannot be recorded it is killed, unless it is the user's
/// (`users`): the user needs no anchor to be let through, so its job runs
/// on, with a warning that archive and delete cannot find it.
fn hand_over(
    app: &App,
    runner_for: &str,
    users: bool,
    child: &mut std::process::Child,
) -> Result<()> {
    // `nohup` execs the runner, so this is its pid. Its pid and start go in
    // the job before it has the go-ahead, so a job is matched to its
    // runner from the first: archiving the task in the moments before the
    // runner gets going ends it all the same.
    let pid = child.id();
    let recorded = match identity::record(&app.db, runner_for, AnchorKind::Job, None, pid)
        .context("record the review command as its review's")
    {
        Err(e) if users => {
            eprintln!(
                "jeet: warning: {e:#}; archiving or deleting the review will not end this job"
            );
            Ok(())
        }
        recorded => recorded,
    };
    let recorded = recorded.and_then(|()| {
        match app
            .db
            .set_review_job_runner(runner_for, pid, identity::start_of(pid))?
        {
            true => Ok(()),
            false => bail!("the review's job is gone"),
        }
    });
    if let Err(e) = recorded {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(GO).context("start the review command")?;
    }
    Ok(())
}

/// The runners jeet recorded for review `review_id`'s jobs.
fn task_jobs(db: &Database, review_id: &str) -> Result<Vec<Anchor>> {
    db.task_anchors(review_id, AnchorKind::Job)
}

/// The runners of the review jobs still running in a worktree under `dir`,
/// whoever they are recorded as: one a review's job names but its own
/// anchors do not, so [`task_jobs`] does not find it. Only the runner the
/// job recorded as its own, by pid and start (a pid reused by another job's
/// runner is not it), while it is still that process.
fn jobs_in(app: &App, dir: &Path) -> Result<Vec<Anchor>> {
    let jobs: Vec<Job> = recorded_jobs(app)
        .into_iter()
        .map(|(_, job)| job)
        .filter(|job| job.exit_code.is_none() && job.worktree.starts_with(dir))
        .collect();
    let pids: Vec<u32> = jobs.iter().filter_map(|job| job.pid).collect();
    let mut runners = app.db.anchors_for(&pids)?;
    runners.retain(|anchor| {
        anchor.kind == AnchorKind::Job
            && jobs
                .iter()
                .any(|job| job.pid == Some(anchor.pid) && job.runner_start == Some(anchor.start))
            && identity::live(anchor)
    });
    Ok(runners)
}

/// Fail when this jeet runs under one of the review jobs [`stop_jobs`] or
/// [`stop_jobs_in`] would end for review `review_id`, in `dir`: ending the
/// job to `verb` the review would end this jeet with it, part-way through.
pub fn refuse_inside_job(app: &App, review_id: &str, dir: &Path, verb: &str) -> Result<()> {
    let mut runners = task_jobs(&app.db, review_id)?;
    runners.extend(jobs_in(app, dir)?);
    if runners.iter().any(identity::runs_under) {
        bail!(
            "this jeet runs under a review job in the review, which {verb} would end, and this \
             jeet with it; {verb} the review once the job is done, or from outside it"
        );
    }
    Ok(())
}

/// End the review jobs running for review `task_id`: each runner jeet
/// recorded for it, while it is still that process, with its process group
/// ([`launch`] gives it one), so the command it runs goes too. Returns
/// once they have exited, with how many were still running.
pub fn stop_jobs(db: &Database, task_id: &str) -> Result<usize> {
    let mut ended = 0;
    for anchor in task_jobs(db, task_id)? {
        ended += usize::from(end_group(&anchor)?);
    }
    Ok(ended)
}

/// End the review jobs still running in a worktree under `dir` (see
/// [`jobs_in`]) as [`stop_jobs`] does; their records go with them.
pub fn stop_jobs_in(app: &App, dir: &Path) -> Result<usize> {
    let mut ended = 0;
    for anchor in jobs_in(app, dir)? {
        ended += usize::from(end_group(&anchor)?);
        app.db.delete_anchor(anchor.pid, anchor.start)?;
    }
    Ok(ended)
}

/// End `anchor`'s process group, while it is still that process, and
/// return once the runner and what it started have exited: what they write
/// after this would miss an archive's snapshot. Whatever has not exited
/// within [`tmux::EXIT_GRACE`] is killed outright. Whether it was running.
fn end_group(anchor: &Anchor) -> Result<bool> {
    if anchor.pid <= 1 || !identity::live(anchor) {
        return Ok(false);
    }
    // Taken before the signal: once the runner is gone, its children are
    // no longer known as its.
    let tree = tmux::process_tree(&[anchor.pid]);
    let _ = Command::new("kill")
        .args(["-s", "TERM", "--", &format!("-{}", anchor.pid)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    tmux::end_processes(&tree, tmux::EXIT_GRACE).context("end the review command")?;
    Ok(true)
}

/// What [`launch`] writes to the runner once it is recorded.
const GO: &[u8] = b"g";

/// Wait for [`launch`]'s go-ahead, before the runner asks who it is: until
/// it has one, it is not yet recorded as its task's. Anything else on
/// stdin, a terminal or nothing at all, means it was not started by
/// `jeet review`.
pub fn await_go() -> Result<()> {
    let mut stdin = std::io::stdin();
    let refused = "review-job is started only by `jeet review`";
    if stdin.is_terminal() {
        bail!(refused);
    }
    let mut byte = [0u8; 1];
    match stdin.read(&mut byte) {
        Ok(1) if byte == GO => Ok(()),
        _ => bail!(refused),
    }
}

/// The runner for review `id`'s job, refused before it ran the command,
/// or never given the go-ahead (its `jeet review` died first): record that
/// it ended, or the job reads as running forever, refusing
/// every later `jeet review`. Only a job jeet recorded, not yet ended and
/// no other runner's.
pub fn refused(id: &str) {
    if let Ok(app) = App::open() {
        refuse(&app, id, std::process::id());
    }
}

/// [`refused`] for the runner with pid `me`.
fn refuse(app: &App, id: &str, me: u32) {
    if crate::permissions::check_review_job(&app.db, id, me).is_err() {
        return;
    }
    let _ = app.db.finish_review_job(id, now_secs(), REFUSED, Some(me));
}

/// The status a refused runner records: its command never ran.
const REFUSED: i32 = 126;

/// The status a job lost before it was adopted is stored with, and no end
/// time: it stopped without finishing.
const LOST: i32 = 127;

/// `jeet review-job <review id>`: run a review's job's command and record
/// how it ended.
///
/// Its stdout and stderr are already the job's log.
pub fn run_job(id: &str) -> Result<()> {
    run(&App::open()?, id, std::process::id())
}

/// [`run_job`] as the runner with pid `me`.
fn run(app: &App, id: &str, me: u32) -> Result<()> {
    crate::permissions::check_review_job(&app.db, id, me)?;
    let Some(job) = app.db.get_review(id)?.and_then(|r| r.job) else {
        bail!("review {id} has no job");
    };
    // Our own pid, which is the one `jeet review` launched: `nohup` execs us.
    if !app
        .db
        .set_review_job_runner(id, me, identity::start_of(me))?
    {
        bail!("review {id}'s job is another runner's, or has ended");
    }
    println!("$ {}", job.command);
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&job.command).current_dir(&job.worktree);
    // Run as a task, the command is that task's agent, never the assistant
    // that started it: the runner itself stays the assistant's, so the
    // check on its job still applies.
    if job.env.iter().any(|(k, _)| k == crate::tasks::TASK_ID_ENV) {
        cmd.env_remove(crate::permissions::ASSISTANT_ENV);
    }
    let code = match cmd
        .envs(job.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("jeet: could not run the review command: {e}");
            127
        }
    };

    app.db.finish_review_job(id, now_secs(), code, Some(me))?;

    // Run headless, the command gets no hooks to say it ended: as its
    // task's Node, the runner says so for it, unless it reported done.
    if let Some((_, task)) = job.env.iter().find(|(k, _)| k == crate::tasks::TASK_ID_ENV) {
        if let Err(e) = report_end(app, task, code, job.started_at) {
            eprintln!("jeet: could not report the review's end: {e:#}");
        }
    }

    if job.notify {
        let message = match code {
            0 => format!("Review of #{} is ready", job.pr),
            _ => format!("Review of #{} failed (status {code})", job.pr),
        };
        let url = job
            .env
            .iter()
            .find(|(key, _)| key == "JEET_PR_URL")
            .map(|(_, url)| url.as_str());
        crate::notify::notify(&crate::notify::Notification {
            title: "jeet",
            subtitle: Some(&job.title),
            body: &message,
            url,
        });
    }
    Ok(())
}

/// Tell whoever asked for review `id` its command exited with `code`, once
/// for the run that started at `since`, unless that run reported done: the
/// review is reused for every ask of its PR, so an earlier run's done does
/// not count.
fn report_end(app: &App, id: &str, code: i32, since: i64) -> Result<()> {
    let Some(review) = app.db.get_review(id)? else {
        return Ok(());
    };
    let body =
        format!("ended without reporting done; the review command exited with status {code}");
    let ended = crate::reports::StatusEvent::Ended;
    crate::reports::auto_status_review(app, &review, ended, &body, since, since)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Retiring reviews whose PR is done
// ---------------------------------------------------------------------------

/// Whether a PR in GitHub's `state` (as [`PullRequest::state_label`] has
/// it) has no more review to do: merged, or closed unmerged. A closed PR
/// may be reopened; `jeet review` then brings its archived review back.
pub fn pr_done(state: &str) -> bool {
    matches!(state, "merged" | "closed")
}

/// What [`retire`] did with a review whose PR is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retired {
    /// Archived, with what archiving warned of.
    Archived(Vec<String>),
    /// Left for now: its agent is mid-turn, or jeet cannot tell it is not.
    /// A later look tries again.
    Busy(String),
    /// Left until the user archives it by hand, and why.
    Kept(String),
    /// Brought back by the user after its PR was done: theirs to archive.
    BroughtBack,
}

/// Whether `activity` leaves a review free to archive: its agent idle,
/// gone or never started. One running, starting, asking the user, or with
/// no hooks to say (`Live`) is mid-turn as far as jeet can tell.
pub fn idle(activity: &Activity) -> bool {
    matches!(
        activity,
        Activity::Idle { .. } | Activity::NotRunning | Activity::NotStarted | Activity::Ended
    )
}

/// Whether an ignored `path` (as `git status --ignored` lists it, a
/// directory with a trailing `/`) is a build cache or tool clutter that a
/// build makes again: what an unattended archive may let go with its
/// worktree, as `jeet task archive --force` would. Anything else — a
/// `.env`, local config — keeps the review for the user to archive.
pub fn rebuildable(path: &str) -> bool {
    const CACHES: &[&str] = &[
        "target",
        "node_modules",
        ".venv",
        "venv",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".gradle",
        ".next",
        ".nuxt",
        ".turbo",
        ".parcel-cache",
        ".cache",
        ".build",
        "build",
        "dist",
        ".DS_Store",
    ];
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    CACHES.contains(&name) || name.ends_with(".pyc")
}

/// Archive `review`, whose PR is done, the way `jeet task archive` does a
/// task: its agent ended, its worktree removed, its uncommitted work kept in
/// the trunk to come back on unarchive. Not while its agent is mid-turn
/// ([`idle`]), and not when the worktree has ignored files that are not
/// [`rebuildable`], which archiving would delete with no one there to say
/// yes. Nor when the user brought it back after the PR was done at
/// `closed_at` (unknown: any time it was brought back counts).
pub fn retire(app: &App, review: &ReviewRecord, closed_at: Option<i64>) -> Result<Retired> {
    let review = &folded(app, review);
    if review.state != TaskState::Active {
        return Ok(Retired::Kept("not an active review".into()));
    }
    if review
        .brought_back_at
        .is_some_and(|back| closed_at.is_none_or(|closed| back >= closed))
    {
        return Ok(Retired::BroughtBack);
    }
    let activity = shown_activity(app, review, tmux::available());
    if !idle(&activity) {
        return Ok(Retired::Busy(format!(
            "its agent is {}",
            activity.label_with(false)
        )));
    }
    if let Some(path) = worktree_path(app, review).filter(|path| path.exists()) {
        let what = review.dir_name.as_deref().unwrap_or_default();
        let kept: Vec<String> = match git::ignored_paths(&path) {
            Ok(paths) => paths.into_iter().filter(|p| !rebuildable(p)).collect(),
            Err(e) => {
                return Ok(Retired::Kept(format!(
                    "{what}: could not list the files git ignores ({e:#})"
                )))
            }
        };
        if let Some(first) = kept.first() {
            return Ok(Retired::Kept(format!(
                "{what}: archiving would delete ignored files such as {first}"
            )));
        }
    }
    archive(app, review).map(Retired::Archived)
}

/// Retire every active review whose PR GitHub says is done: one line for
/// each review archived or kept back, and for each PR that could not be
/// asked about. For `jeet watch`, which runs unattended.
pub fn sweep(app: &App) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for review in app.db.list_reviews(Some(TaskState::Active))? {
        let number = review.pr_number;
        let Some(repo) = app.db.get_repo(&review.repo_id)? else {
            continue;
        };
        let (state, closed_at) = match crate::github::pr_review(Path::new(&repo.trunk_path), number)
        {
            Ok(found) => {
                if let Err(e) =
                    app.db
                        .set_review_pr(&review.id, &found.pr.title, &found.pr.state, None)
                {
                    lines.push(format!("could not record #{number}'s state: {e:#}"));
                }
                (
                    found.pr.state_label(),
                    found
                        .closed_at
                        .as_deref()
                        .and_then(crate::github::parse_timestamp),
                )
            }
            Err(e) => {
                lines.push(format!("could not ask GitHub about #{number}: {e:#}"));
                continue;
            }
        };
        if !pr_done(state) {
            continue;
        }
        match retire(app, &review, closed_at) {
            Ok(Retired::Archived(warnings)) => {
                lines.push(format!(
                    "archived review {}: #{number} is {state}",
                    review.slug
                ));
                lines.extend(warnings.iter().map(|w| format!("{}: {w}", review.slug)));
            }
            // Tried again on the next poll, once the agent is idle; one
            // the user brought back is theirs.
            Ok(Retired::Busy(_) | Retired::BroughtBack) => {}
            Ok(Retired::Kept(why)) => lines.push(format!(
                "review {} not archived though #{number} is {state}: {why}",
                review.slug
            )),
            Err(e) => lines.push(format!("could not archive review {}: {e:#}", review.slug)),
        }
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// After the move out of `tasks` (schema v8)
// ---------------------------------------------------------------------------

/// The `meta` flag the v8 migration sets while [`migrate_leftovers`] has
/// work to do.
const PENDING: &str = "reviews_v8_pending";

/// Under `$JEET_HOME/logs`: what [`migrate_leftovers`] did, one line each.
const MIGRATE_LOG: &str = "migrate-v8.log";

/// What the move of reviews out of `tasks` (schema v8) left for jeet to do
/// on disk, once, at the first command after the upgrade. Best effort, and
/// idempotent: what it cannot do now it does at the next command.
///
/// - What a review task kept in files under its directory — what its agent
///   was asked, when it was brought back — goes on the review's row.
/// - Each duplicate review task's worktree is archived the way `jeet task
///   archive` archives one (uncommitted work kept in the trunk under
///   `refs/jeet`, the ref logged), unless it has ignored files that are not
///   [`rebuildable`], when it is left where it is. Nothing is ever deleted
///   but the worktree an archive removes: not the duplicate's directory,
///   not a ref.
///
/// Under a lock, so two commands at once do not both archive. Each outcome
/// is a line of the log, and a summary goes to stderr once.
pub fn migrate_leftovers(app: &App) {
    if let Err(e) = migrate(app) {
        eprintln!("jeet: warning: could not finish moving reviews out of the tasks: {e:#}");
    }
}

fn migrate(app: &App) -> Result<()> {
    if app.db.meta(PENDING)?.is_none() {
        return Ok(());
    }
    let locks = app.home.join("locks");
    std::fs::create_dir_all(&locks).with_context(|| format!("create {}", locks.display()))?;
    let path = locks.join("reviews-migrate.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        // Another command is at it.
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
    // Done by whoever held the lock before.
    if app.db.meta(PENDING)?.is_none() {
        return Ok(());
    }
    let logs = app.home.join("logs");
    std::fs::create_dir_all(&logs).with_context(|| format!("create {}", logs.display()))?;
    let log_path = logs.join(MIGRATE_LOG);
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut say = |line: String| {
        let _ = writeln!(log, "{} {line}", now_secs());
    };

    let (mut archived, mut left, mut gone, mut waiting) = (0, 0, 0, 0);
    for (key, what) in [
        ("reviews_v8_kept", "kept review"),
        (
            "reviews_v8_superseded",
            "left the worktrees of duplicate review task",
        ),
        ("reviews_v8_dropped", "dropped review task with no PR"),
    ] {
        if let Some(lines) = app.db.meta(key)? {
            for line in lines.lines() {
                say(format!("{what} {line}"));
            }
            app.db.delete_meta(key)?;
        }
    }

    let reviews = app.db.list_reviews(None)?;
    for review in &reviews {
        fold_markers(app, review, &mut say);
    }

    let root = app.tasks_root();
    for old in app.db.pending_leftovers()? {
        let session = runtime::session_name_for(&old.slug);
        let dir = root.join(&old.slug);
        let path = dir.join(&old.dir_name);
        let what = format!("{}/{}", old.slug, old.dir_name);
        // A kept review's own, whatever else was recorded of it: never
        // archived, and its session never ended.
        if let Some(own) = reviews
            .iter()
            .find(|r| r.slug == old.slug || r.id == old.task_id)
        {
            let note = format!("left in place: belongs to review {}", own.slug);
            app.db.finish_leftover(&old.task_id, &old.dir_name, &note)?;
            say(format!("{what}: {note}"));
            left += 1;
            continue;
        }
        // Ending its session would end this jeet.
        if tmux::current_session().is_some_and(|s| s == session) {
            say(format!(
                "waiting: {what}: this runs inside its tmux session"
            ));
            waiting += 1;
            continue;
        }
        if tmux::available() {
            if let Err(e) = tmux::kill(&session) {
                say(format!(
                    "waiting: {what}: could not end its tmux session ({e:#})"
                ));
                waiting += 1;
                continue;
            }
        }
        if !path.exists() {
            app.db
                .finish_leftover(&old.task_id, &old.dir_name, "already gone")?;
            say(format!("{what}: already gone"));
            gone += 1;
            continue;
        }
        let ignored = match git::ignored_paths(&path) {
            Ok(paths) => paths.into_iter().find(|p| !rebuildable(p)),
            Err(e) => {
                match give_up_after_three(
                    app,
                    &old,
                    &format!("could not list its ignored files: {e:#}"),
                )? {
                    Some(note) => {
                        say(format!("{what}: {note}"));
                        left += 1;
                    }
                    None => {
                        say(format!(
                            "waiting: {what}: could not list its ignored files ({e:#})"
                        ));
                        waiting += 1;
                    }
                }
                continue;
            }
        };
        if let Some(first) = ignored {
            let note = format!("left in place: ignored files such as {first}");
            app.db.finish_leftover(&old.task_id, &old.dir_name, &note)?;
            say(format!("{what}: {note} (at {})", path.display()));
            left += 1;
            continue;
        }
        let mut row = TaskWorktreeRecord {
            id: 0,
            task_id: old.task_id.clone(),
            repo_id: old.repo_id.clone(),
            dir_name: old.dir_name.clone(),
            branch: old.branch.clone(),
            created_branch: old.created_branch,
            head_oid: old.head_oid.clone(),
            stash_ref: old.stash_ref.clone(),
            pr_number: old.pr_number,
            created_at: 0,
            read_only: false,
        };
        let save = |_: &TaskWorktreeRecord| Ok(());
        match tasks::archive_worktree(app, &old.task_id, &dir, &mut row, &save) {
            Ok(warnings) => {
                let stash = row
                    .stash_ref
                    .as_deref()
                    .map_or("nothing uncommitted".to_string(), |r| {
                        format!("uncommitted work at {r}")
                    });
                let note = format!(
                    "archived: HEAD at {}, {stash}",
                    tasks::jeet_ref("head", &old.task_id, &old.dir_name)
                );
                app.db.finish_leftover(&old.task_id, &old.dir_name, &note)?;
                say(format!("{what}: {note}"));
                for warning in warnings {
                    say(format!("{what}: {warning}"));
                }
                archived += 1;
            }
            Err(e) => match give_up_after_three(app, &old, &format!("{e:#}"))? {
                Some(note) => {
                    say(format!("{what}: {note}"));
                    left += 1;
                }
                None => {
                    say(format!("waiting: {what}: could not archive it ({e:#})"));
                    waiting += 1;
                }
            },
        }
    }

    if waiting == 0 {
        app.db.delete_meta(PENDING)?;
    }
    // Once per change: a leftover that waits to be tried again is not news.
    if archived + left + gone > 0 {
        eprintln!(
            "jeet: reviews moved out of the tasks: {archived} duplicate worktrees archived, \
             {left} left in place, {waiting} to retry; see {}",
            log_path.display()
        );
    }
    Ok(())
}

/// Attempts at a leftover before it is left where it is.
const ATTEMPTS: i64 = 3;

/// Count a failed attempt at `old`; at [`ATTEMPTS`] it is done, left in
/// place, and the note says why.
fn give_up_after_three(
    app: &App,
    old: &crate::db::reviews::Leftover,
    error: &str,
) -> Result<Option<String>> {
    if app.db.count_leftover_attempt(&old.task_id, &old.dir_name)? < ATTEMPTS {
        return Ok(None);
    }
    let note = format!("left in place: could not archive ({error})");
    app.db.finish_leftover(&old.task_id, &old.dir_name, &note)?;
    Ok(Some(note))
}

// ---------------------------------------------------------------------------
// Adopting the job files older jeets left (schema v9)
// ---------------------------------------------------------------------------

/// The `meta` marker that [`adopt_jobs`] has been through
/// `$JEET_HOME/reviews`: it reads the directory once.
const JOBS_ADOPTED: &str = "legacy_jobs_adopted";

/// Under `$JEET_HOME/logs`: what [`adopt_jobs`] did, one line per file.
const ADOPT_LOG: &str = "migrate-v9.log";

/// How [`adopt_file`] dealt with one job file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adoption {
    /// A review made for it, archived: it had finished or was lost.
    Archived,
    /// A review made for it, active: its process still runs.
    Running,
    /// Its job put on the review its PR already had, which had none.
    Folded,
    /// Its PR's review has a job already; the file is not needed.
    Held,
    /// Not a job jeet can read: left alone.
    Left,
}

/// Older jeets kept each `[review] command` job as a JSON file under
/// `$JEET_HOME/reviews/<host>/<owner>/<repo>/<n>.json`, and the Reviews tab
/// listed them read-only. At the first commands after the upgrade to schema
/// v9, each file whose PR has no review becomes one: archived when the job
/// finished or was lost (its process is gone), active while it still runs,
/// so the sweep of done PRs archives it once its PR is merged or closed. It
/// has a directory of its own like any review and no worktree: the job's is
/// never touched. A PR that has a review without a job gets the file's as
/// its job. A file that cannot be read is left. A job that is not running
/// is stored ended, so no runner can take it up.
///
/// Files are never deleted or moved. Each one gone through is recorded in
/// `review_job_files`, and one with a final outcome is never looked at
/// again, so a review deleted since is not made anew. A file whose review
/// could not be made is tried [`ATTEMPTS`] times, then left. [`JOBS_ADOPTED`]
/// is set once none is pending, and stops the next command looking. Under a
/// lock, so two commands at once do not both adopt. Each file's outcome is a
/// line of the log, and a pass that adopted or gave up on something says so
/// on stderr.
pub fn adopt_jobs(app: &App) {
    if let Err(e) = adopt(app) {
        eprintln!("jeet: warning: could not adopt the review jobs older jeets left: {e:#}");
    }
}

/// The start of the outcome of a job file that is to be tried again (up to
/// [`ATTEMPTS`] times).
const RETRY: &str = "pending: ";

fn adopt(app: &App) -> Result<()> {
    if app.db.meta(JOBS_ADOPTED)?.is_some() {
        return Ok(());
    }
    let locks = app.home.join("locks");
    std::fs::create_dir_all(&locks).with_context(|| format!("create {}", locks.display()))?;
    let path = locks.join("adopt-jobs.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        // Another command is at it.
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
    // Done by whoever held the lock before.
    if app.db.meta(JOBS_ADOPTED)?.is_some() {
        return Ok(());
    }
    let root = app.home.join("reviews");
    // Each file not yet through, with what was recorded of it.
    let mut todo = Vec::new();
    for file in job_files(&root) {
        let prior = app.db.job_file(&file.to_string_lossy())?;
        if prior
            .as_ref()
            .is_some_and(|(outcome, _)| !outcome.starts_with(RETRY))
        {
            continue;
        }
        todo.push((file, prior));
    }
    if todo.is_empty() {
        return app.db.set_meta(JOBS_ADOPTED, "1");
    }
    let logs = app.home.join("logs");
    std::fs::create_dir_all(&logs).with_context(|| format!("create {}", logs.display()))?;
    let log_path = logs.join(ADOPT_LOG);
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut say = |line: String| {
        let _ = writeln!(log, "{} {line}", now_secs());
    };

    let mut counts = [0usize; 5];
    let mut gave_up = 0;
    let mut retry = 0;
    for (file, prior) in &todo {
        let key = file.to_string_lossy();
        let attempts = prior.as_ref().map_or(0, |(_, n)| *n);
        match adopt_file(app, &root, file) {
            Ok(done) => {
                app.db.record_job_file(
                    &key,
                    &done.outcome,
                    done.review_id.as_deref(),
                    attempts + 1,
                    now_secs(),
                )?;
                say(done.line);
                counts[done.how as usize] += 1;
                gave_up += usize::from(done.how == Adoption::Left);
            }
            Err(e) => {
                let attempts = attempts + 1;
                let line = format!("{}: not adopted yet: {e:#}", file.display());
                let outcome = if attempts >= ATTEMPTS {
                    gave_up += 1;
                    let outcome = format!("left: could not adopt ({e:#})");
                    say(format!("{}: {outcome}", file.display()));
                    outcome
                } else {
                    retry += 1;
                    let outcome = format!("{RETRY}{e:#}");
                    // Only a change is news.
                    if prior.as_ref().map(|(o, _)| o) != Some(&outcome) {
                        say(line);
                    }
                    outcome
                };
                app.db
                    .record_job_file(&key, &outcome, None, attempts, now_secs())?;
            }
        }
    }
    if retry == 0 {
        app.db.set_meta(JOBS_ADOPTED, "1")?;
    }
    let [archived, running, folded, held, left] = counts;
    if archived + running + folded + gave_up > 0 {
        eprintln!(
            "jeet: review jobs older jeets left in {} are now reviews: {} adopted ({archived} \
             archived, {running} still running), {folded} put on a review that had none, \
             {held} already held, {left} left unread, {gave_up} given up on, {retry} to retry; \
             nothing was deleted; see {}",
            root.display(),
            archived + running,
            log_path.display()
        );
    }
    Ok(())
}

/// Every `<host>/<owner>/<repo>/<n>.json` under `root` that [`job_key`]
/// accepts, in path order: the owner may be a group with subgroups.
fn job_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .min_depth(4)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "json"))
        .map(walkdir::DirEntry::into_path)
        .filter(|path| job_key(root, path).is_some())
        .collect();
    files.sort();
    files
}

/// The repository and PR a job file at `path` under `root` is for.
fn job_key(root: &Path, path: &Path) -> Option<(String, u64)> {
    let number = path.file_stem()?.to_str()?.parse().ok()?;
    let repo_id = path
        .parent()?
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    remote::identity_from_id(&repo_id).ok()?;
    Some((repo_id, number))
}

/// What [`adopt_file`] did with a file.
struct Adopted {
    how: Adoption,
    /// For the log.
    line: String,
    /// For `review_job_files`.
    outcome: String,
    /// The review the file's job is on.
    review_id: Option<String>,
}

/// Adopt the job file at `path`. An error is a review that could not be
/// made now: the file is tried again.
fn adopt_file(app: &App, root: &Path, path: &Path) -> Result<Adopted> {
    let left = |why: String| {
        let outcome = format!("left: {why}");
        Ok(Adopted {
            how: Adoption::Left,
            line: format!("{}: {outcome}", path.display()),
            outcome,
            review_id: None,
        })
    };
    let Some((repo_id, number)) = job_key(root, path) else {
        return left("not named <repo>/<number>.json".into());
    };
    let label = format!("{repo_id}#{number}");
    let mut job: Job = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(job) => job,
            Err(e) => return left(format!("not a review job ({e})")),
        },
        Err(e) => return left(format!("unreadable ({e})")),
    };
    // The path is the key, whatever the file says.
    job.pr = number;
    // Recorded before any runner was: it never ran, as a refused one.
    let mut how = String::new();
    if job.exit_code.is_none() && job.pid.is_none() {
        job.finished_at = Some(now_secs());
        job.exit_code = Some(REFUSED);
        how = "; it never launched, so recorded as ended (status 126)".to_string();
    }
    job.lost = job.exit_code.is_none() && job.pid.is_some_and(|pid| !process_alive(pid));
    let state = job.state();
    let state_word = match state {
        JobState::Running => format!("still running as pid {}", job.pid.unwrap_or(0)),
        JobState::Finished(code) => format!("finished with status {code}"),
        JobState::Lost => "lost: its process is gone".to_string(),
    };
    // A job not running is stored ended, whatever the file says: nothing
    // may run it. A lost one has no end to record.
    if state == JobState::Lost {
        job.exit_code = Some(LOST);
        job.lost = false;
    }

    if let Some(review) = app.db.find_review(&repo_id, number)? {
        if review.job.is_some() {
            let line = format!(
                "{label}: skipped {}: review {} has a job already",
                path.display(),
                review.slug
            );
            return Ok(Adopted {
                how: Adoption::Held,
                line,
                outcome: format!("held: review {} has a job already", review.slug),
                review_id: Some(review.id),
            });
        }
        app.db.set_review_job(&review.id, &job)?;
        let line = format!(
            "{label}: put {} ({state_word}{how}) on review {}, which had no job",
            path.display(),
            review.slug
        );
        return Ok(Adopted {
            how: Adoption::Folded,
            line,
            outcome: format!("put on review {}", review.slug),
            review_id: Some(review.id),
        });
    }

    let repo = repo_id.rsplit('/').next().unwrap_or(&repo_id);
    let slug = tasks::unique_slug(app, &tasks::slugify(&format!("review-{repo}-{number}")))?;
    let mut review = ReviewRecord::new(
        Uuid::new_v4().to_string(),
        slug,
        &repo_id,
        number,
        job.started_at,
    );
    review.title = job.title.clone();
    review.last_activity = job.finished_at.unwrap_or(job.started_at);
    review.state = match state {
        JobState::Running => TaskState::Active,
        JobState::Finished(_) | JobState::Lost => TaskState::Archived,
    };
    review.job = Some(job);
    // The row first: a crash between the two leaves a review without its
    // directory, not a directory that forces the next slug to `-2`.
    app.db.insert_review(&review)?;
    let dir = dir(app, &review);
    let made = std::fs::create_dir_all(app.tasks_root())
        .context("create the tasks directory")
        .and_then(|()| {
            std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))
        })
        .and_then(|()| tasks::write_marker(&dir, &review.id))
        .and_then(|()| install_skill(&dir));
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&dir);
        app.db.delete_review(&review.id)?;
        return Err(e);
    }
    let (how_adopted, kind) = match review.state {
        TaskState::Active => ("active", Adoption::Running),
        TaskState::Archived => ("archived", Adoption::Archived),
    };
    let line = format!(
        "{label}: adopted {} as review {} ({how_adopted}: {state_word}{how})",
        path.display(),
        review.slug
    );
    Ok(Adopted {
        how: kind,
        line,
        outcome: format!("adopted as review {} ({how_adopted})", review.slug),
        review_id: Some(review.id),
    })
}

/// Put what review `review`'s task kept in files — `<dir>/.jeet/review.json`
/// (what its agent was asked) and `brought-back` — on its row, where they
/// are not yet.
///
/// True when a column was set.
fn fold_markers(app: &App, review: &ReviewRecord, say: &mut dyn FnMut(String)) -> bool {
    let mut changed = false;
    let marker = dir(app, review).join(tasks::MARKER_DIR);
    if review.asked_at.is_none() {
        let asked = std::fs::read_to_string(marker.join("review.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Asked>(&text).ok())
            .filter(|asked| asked.pr == review.pr_number);
        if let Some(asked) = asked {
            match app.db.set_review_asked(&review.id, &asked.head, asked.at) {
                Ok(()) => {
                    changed = true;
                    say(format!("{}: took what its agent was asked", review.slug));
                }
                Err(e) => say(format!("{}: could not record it ({e:#})", review.slug)),
            }
        }
    }
    if review.brought_back_at.is_none() {
        let at = std::fs::read_to_string(marker.join("brought-back"))
            .ok()
            .and_then(|text| text.trim().parse::<i64>().ok());
        if let Some(at) = at {
            match app.db.set_review_brought_back(&review.id, at) {
                Ok(()) => {
                    changed = true;
                    say(format!("{}: took when it was brought back", review.slug));
                }
                Err(e) => say(format!("{}: could not record it ({e:#})", review.slug)),
            }
        }
    }
    changed
}

/// `review` with what its task kept in files folded into its row first, for
/// whatever reads whether its agent was asked or when it was brought back:
/// the post-step of the upgrade ([`migrate_leftovers`]) may not have run, or
/// not got to it. Cheap: a file or two, and only where a column is empty.
pub fn folded(app: &App, review: &ReviewRecord) -> ReviewRecord {
    if review.asked_at.is_some() && review.brought_back_at.is_some() {
        return review.clone();
    }
    if !fold_markers(app, review, &mut |_| {}) {
        return review.clone();
    }
    app.db
        .get_review(&review.id)
        .ok()
        .flatten()
        .unwrap_or_else(|| review.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_review_is_done_with_its_pr_merged_or_closed() {
        assert!(pr_done("merged") && pr_done("closed"));
        assert!(!pr_done("open") && !pr_done("draft"));
    }

    #[test]
    fn only_an_idle_or_gone_agent_lets_its_review_be_archived() {
        for free in [
            Activity::Idle { since: 0 },
            Activity::NotRunning,
            Activity::NotStarted,
            Activity::Ended,
        ] {
            assert!(idle(&free), "{free:?}");
        }
        // Mid-turn, asking the user, or with no hooks to say: left alone.
        for busy in [
            Activity::Running { detail: None },
            Activity::Waiting { detail: None },
            Activity::Starting,
            Activity::Live,
        ] {
            assert!(!idle(&busy), "{busy:?}");
        }
    }

    #[test]
    fn build_caches_may_go_unattended_but_nothing_else() {
        for cache in [
            "target/",
            "web/node_modules/",
            ".venv/",
            "pkg/__pycache__/",
            "a/b.pyc",
            ".DS_Store",
            "dist/",
        ] {
            assert!(rebuildable(cache), "{cache}");
        }
        for kept in [
            ".env",
            "config.local.toml",
            "secrets/",
            "targets/",
            "my-target/",
        ] {
            assert!(!rebuildable(kept), "{kept}");
        }
    }

    fn pr() -> PullRequest {
        PullRequest {
            number: 7,
            title: "$(rm -rf ~)".into(),
            url: "https://github.com/acme/widget/pull/7".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            head_ref_name: "x;touch pwned".into(),
            head_ref_oid: "abc".into(),
            base_ref_name: "main".into(),
            is_cross_repository: true,
        }
    }

    fn job(exit_code: Option<i32>, head: &str) -> Job {
        Job {
            pr: 7,
            title: "t".into(),
            head: head.into(),
            worktree: PathBuf::from("/tmp"),
            command: "true".into(),
            env: Vec::new(),
            notify: false,
            log: PathBuf::from("/tmp/7.log"),
            started_at: 0,
            pid: None,
            runner_start: None,
            finished_at: exit_code.map(|_| 0),
            exit_code,
            lost: false,
        }
    }

    #[test]
    fn only_github_values_reach_the_shell() {
        let cmd = expand(
            "claude -p '/review-pr {pr}' # {url} {branch} {title}",
            &pr(),
        );
        assert_eq!(
            cmd,
            "claude -p '/review-pr 7' # https://github.com/acme/widget/pull/7 {branch} {title}"
        );
        let env = job_env(&pr(), Path::new("/l"));
        assert!(env.contains(&("JEET_PR_BRANCH".into(), "x;touch pwned".into())));
    }

    #[test]
    fn the_review_prompt_takes_only_github_and_jeet_values() {
        let prompt = expand_prompt(
            "Review #{pr} ({url}) in {worktree}: {branch} {title}",
            &pr(),
            Path::new("/t/w"),
        );
        assert_eq!(
            prompt,
            "Review #7 (https://github.com/acme/widget/pull/7) in /t/w: {branch} {title}"
        );
        let default = expand_prompt(crate::config::DEFAULT_REVIEW_PROMPT, &pr(), Path::new("/w"));
        assert!(!default.contains('{'), "{default}");
        assert!(!default.contains("rm -rf") && !default.contains("pwned"));
    }

    #[test]
    fn the_review_agent_is_asked_once_per_commit() {
        let asked = |head: &str| Asked {
            pr: 7,
            head: head.into(),
            at: 0,
        };
        assert!(reason_not_to_ask(None, None, &pr(), false).is_none());
        assert!(reason_not_to_ask(Some(&asked("abc")), None, &pr(), false).is_some());
        assert!(reason_not_to_ask(Some(&asked("abc")), None, &pr(), true).is_none());
        // A new head commit is asked about again, in the same session.
        assert!(reason_not_to_ask(Some(&asked("old")), None, &pr(), false).is_none());
        // A legacy job that reviewed this commit counts, until the agent
        // has been asked itself.
        let done = job(Some(0), "abc");
        assert!(reason_not_to_ask(None, Some(&done), &pr(), false).is_some());
        assert!(reason_not_to_ask(None, Some(&done), &pr(), true).is_none());
        assert!(reason_not_to_ask(Some(&asked("old")), Some(&done), &pr(), false).is_none());
        // One still running is never doubled, --rerun or not.
        let running = job(None, "old");
        assert!(reason_not_to_ask(None, Some(&running), &pr(), true).is_some());
    }

    #[test]
    fn a_finished_review_of_the_same_commit_is_not_repeated() {
        assert!(reason_to_skip(None, "abc").is_none());
        assert!(reason_to_skip(Some(&job(Some(0), "abc")), "abc").is_some());
        // New commits, or a run that failed, are worth another go.
        assert!(reason_to_skip(Some(&job(Some(0), "abc")), "def").is_none());
        assert!(reason_to_skip(Some(&job(Some(1), "abc")), "abc").is_none());
        // Still running (no pid recorded yet counts as running).
        assert!(reason_to_skip(Some(&job(None, "abc")), "def").is_some());
    }

    #[test]
    fn a_running_job_shows_as_its_reviews_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        let add = |id: &str, pr: u64, job: Option<Job>| {
            let mut review = ReviewRecord::new(
                id.into(),
                format!("review-api-{pr}"),
                "github.com/acme/api",
                pr,
                0,
            );
            review.job = job;
            db.insert_review(&review).unwrap();
        };
        add("done", 1, Some(job(Some(0), "abc")));
        let mut j = job(None, "abc");
        j.pid = Some(std::process::id());
        add("live", 7, Some(j.clone()));
        add("none", 9, None);
        // A runner that is gone is not running.
        j.pid = Some(u32::MAX - 1);
        add("gone", 10, Some(j));

        let reviewing = Activity::Running {
            detail: Some("review of #7".into()),
        };
        let idle = Activity::Idle { since: 5 };
        for none in [Activity::NotStarted, Activity::NotRunning, Activity::Ended] {
            for quiet in ["done", "none", "gone", "missing"] {
                assert_eq!(with_job(none.clone(), Some(&db), quiet), none, "{quiet}");
            }
            assert_eq!(with_job(none.clone(), Some(&db), "live"), reviewing);
            assert_eq!(with_job(none.clone(), None, "live"), none);
        }
        // An agent of its own says what it does itself.
        assert_eq!(with_job(idle.clone(), Some(&db), "live"), idle);
    }

    #[cfg(unix)]
    #[test]
    fn the_runner_records_how_the_command_ended() {
        let home = tempfile::tempdir().unwrap();
        let app = test_app(home.path());
        let mut j = job(None, "abc");
        j.worktree = home.path().to_path_buf();
        j.command = "test \"$JEET_PR\" = 7 && exit 3".into();
        j.env = vec![("JEET_PR".into(), "7".into())];
        app.db.set_review_job("t", &j).unwrap();
        let me = std::process::id();
        run(&app, "t", me).unwrap();
        let done = job_of(&app, &app.db.get_review("t").unwrap().unwrap()).unwrap();
        assert_eq!(done.state(), JobState::Finished(3));
        assert_eq!(done.pid, Some(me), "the runner records itself");
        assert_eq!(done.runner_start, identity::start_of(me));
        assert!(done.finished_at.is_some());
        // Run once: the job has ended.
        assert!(run(&app, "t", me).is_err());
    }

    /// A runner refused before it ran, or never given the go-ahead, ends
    /// its job as refused, once, and only as the runner the job names.
    #[cfg(unix)]
    #[test]
    fn a_refused_runner_ends_its_job_without_running_it() {
        let home = tempfile::tempdir().unwrap();
        let app = test_app(home.path());
        let get = || app.db.get_review("t").unwrap().unwrap().job.unwrap();
        let mut j = job(None, "abc");
        j.pid = Some(4242);
        app.db.set_review_job("t", &j).unwrap();
        // Another process's job is left as it is.
        refuse(&app, "t", 1);
        assert_eq!(get().exit_code, None);
        refuse(&app, "t", 4242);
        assert_eq!(get().exit_code, Some(REFUSED));
        // And an ended one keeps how it ended.
        app.db.finish_review_job("t", 9, 0, None).unwrap();
        assert_eq!(get().exit_code, Some(REFUSED));
        // No review, no job: nothing to do.
        refuse(&app, "nobody", 4242);
    }

    /// A job row whose pid is now another process's: its group is spared;
    /// the job's own is ended.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_spares_a_group_whose_pid_was_reused() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        db.insert_review(&ReviewRecord::new(
            "t".into(),
            "t".into(),
            "github.com/acme/api",
            7,
            0,
        ))
        .unwrap();
        // An unrelated process group, as a job's would be.
        let mut other = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&db, "t", AnchorKind::Job, None, other.id()).unwrap();
        let mut row = db.task_anchors("t", AnchorKind::Job).unwrap().remove(0);
        db.delete_anchor(row.pid, row.start).unwrap();
        row.start += 1;
        db.insert_anchor(&row, None).unwrap();

        stop_jobs(&db, "t").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "an unrelated group was ended"
        );

        db.delete_anchor(row.pid, row.start).unwrap();
        identity::record(&db, "t", AnchorKind::Job, None, other.id()).unwrap();
        stop_jobs(&db, "t").unwrap();
        let status = other.wait().unwrap();
        assert!(!status.success(), "the job's group was not ended");
    }

    /// Archiving or deleting one task's dir ends the job running there and
    /// only it: not another task's, and not a runner whose pid a stale job
    /// in the dir still names.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_in_ends_only_the_dirs_own_runner() {
        use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskState};
        use crate::placement::Placement;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: Database::open(&crate::config::index_path(&home)).unwrap(),
            home: home.clone(),
        };
        app.db
            .insert_task(&TaskRecord {
                id: "a".into(),
                slug: "a".into(),
                name: "a".into(),
                project: None,
                kind: TaskKind::Assistant,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
        let group = || {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let x = home.join("tasks/x");
        let y = home.join("tasks/y");
        // The assistant's runner for a job in `x`, and one for a job in `y`.
        let mut mine = group();
        let mut other = group();
        let state = |n: u64, worktree: &Path, pid: u32, start: Option<i64>| {
            let mut j = job(None, "abc");
            j.worktree = worktree.join("widget");
            j.pid = Some(pid);
            j.runner_start = start;
            let mut review = ReviewRecord::new(
                format!("j{n}"),
                format!("j{n}"),
                "github.com/acme/widget",
                n,
                0,
            );
            review.job = Some(j);
            app.db.insert_review(&review).unwrap();
        };
        let start = |child: &std::process::Child| identity::start_of(child.id()).unwrap();
        state(1, &x, mine.id(), Some(start(&mine)));
        state(2, &y, other.id(), Some(start(&other)));
        // A job in `x` its runner never finished, whose pid is now `other`'s,
        // and one from before runners recorded their start.
        state(3, &x, other.id(), Some(start(&other) + 1));
        state(4, &x, other.id(), None);
        for child in [&mine, &other] {
            identity::record(&app.db, "a", AnchorKind::Job, None, child.id()).unwrap();
        }

        stop_jobs_in(&app, &x).unwrap();
        let status = mine.wait().unwrap();
        assert!(!status.success(), "the dir's own job was not ended");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "another task's job was ended"
        );
        let left = app.db.task_anchors("a", AnchorKind::Job).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pid, other.id());
        let _ = other.kill();
        let _ = other.wait();
    }
    /// Each review asked gets `[review]`'s time and turns afresh on top of
    /// what it has used, whoever asked, and the pause its last review's
    /// limits set is lifted.
    #[cfg(unix)]
    #[test]
    fn each_review_gets_its_caps_afresh() {
        let home = tempfile::tempdir().unwrap();
        let app = test_app(home.path());
        let get = || app.db.get_review("t").unwrap().unwrap();
        give_budget(&app, &get()).unwrap();
        let r = get();
        assert_eq!((r.max_secs, r.max_turns), (Some(3600), Some(20)));

        // The first review spent: paused, until the next.
        app.db.charge_review("t", 3600, 3).unwrap();
        crate::manager::budget::enforce_review(&app, &get()).unwrap();
        assert!(get().paused);
        give_budget(&app, &get()).unwrap();
        let r = get();
        assert_eq!((r.max_secs, r.max_turns), (Some(7200), Some(23)));
        assert_eq!(r.warned, 0);
        assert!(!r.paused);
    }

    /// A test index holding the assistant and one review `t`.
    #[cfg(unix)]
    fn test_app(home: &Path) -> App {
        use crate::db::tasks::{RunState, TaskKind, TaskRecord};
        use crate::placement::Placement;
        let app = App {
            config: crate::config::load_or_create(home).unwrap(),
            db: Database::open(&crate::config::index_path(home)).unwrap(),
            home: home.to_path_buf(),
        };
        app.db
            .insert_task(&TaskRecord {
                id: "a".into(),
                slug: "a".into(),
                name: "a".into(),
                project: None,
                kind: TaskKind::Assistant,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
        app.db
            .insert_review(&ReviewRecord::new(
                "t".into(),
                "t".into(),
                "github.com/acme/api",
                7,
                0,
            ))
            .unwrap();
        app
    }

    /// A review command that takes a moment to finish once told to end,
    /// writing as it goes: stopping the job returns only once it is done, so
    /// what it wrote is there before an archive's snapshot.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_waits_for_the_command_to_exit() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let done = home.join("done");
        // Gone in 30s whatever happens, so a failing test leaves nothing.
        let mut job = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap 'sleep 1; touch {}; exit 0' TERM; \
                 i=0; while [ $i -lt 300 ]; do sleep 0.1; i=$((i + 1)); done",
                done.display()
            ))
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&app.db, "t", AnchorKind::Job, None, job.id()).unwrap();

        stop_jobs(&app.db, "t").unwrap();
        assert!(done.exists(), "returned before the command had exited");
        let _ = job.wait();
    }

    /// Archive or delete run under the task's review job would end itself
    /// part-way, so it is refused there; run elsewhere, it is not.
    #[cfg(unix)]
    #[test]
    fn refuse_inside_job_refuses_only_under_the_tasks_job() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let task_dir = home.join("tasks/t");
        let mut other = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&app.db, "t", AnchorKind::Job, None, other.id()).unwrap();
        refuse_inside_job(&app, "t", &task_dir, "archive").unwrap();

        // This process as the task's runner.
        let me = Anchor {
            pid: std::process::id(),
            start: identity::start_of(std::process::id()).unwrap(),
            task_id: "t".into(),
            kind: AnchorKind::Job,
            created_at: 0,
        };
        app.db.insert_anchor(&me, None).unwrap();
        let err = refuse_inside_job(&app, "t", &task_dir, "archive").unwrap_err();
        assert!(err.to_string().contains("runs under a review job"), "{err}");
        app.db.delete_anchor(me.pid, me.start).unwrap();

        // The assistant's runner for a job in the task's worktree.
        let mut j = job(None, "abc");
        j.worktree = task_dir.join("widget");
        j.pid = Some(me.pid);
        j.runner_start = Some(me.start);
        app.db.set_review_job("t", &j).unwrap();
        app.db
            .insert_anchor(
                &Anchor {
                    task_id: "a".into(),
                    ..me.clone()
                },
                None,
            )
            .unwrap();
        let err = refuse_inside_job(&app, "t", &task_dir, "delete").unwrap_err();
        assert!(err.to_string().contains("delete would end"), "{err}");

        let _ = other.kill();
        let _ = other.wait();
    }

    /// The runner is named in the review's job, and recorded, before it has
    /// the go-ahead: archiving or deleting the task in the moments before
    /// the runner writes itself ends the job all the same.
    #[cfg(unix)]
    #[test]
    fn the_runner_is_named_before_the_go_ahead() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let mut j = job(None, "abc");
        j.worktree = home.join("tasks/t/widget");
        app.db.set_review_job("t", &j).unwrap();
        // A runner that, given the go-ahead, says so, and keeps running as a
        // job would.
        let seen = home.join("seen");
        let mut runner = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "head -c1 >/dev/null; touch {}; exec sleep 30",
                seen.display()
            ))
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();

        hand_over(&app, "t", false, &mut runner).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !seen.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let named = app.db.get_review("t").unwrap().unwrap().job.unwrap();
        assert_eq!(named.pid, Some(runner.id()));
        assert!(named.runner_start.is_some());
        assert_eq!(named.runner_start, identity::start_of(runner.id()));
        let found = jobs_in(&app, &home.join("tasks/t")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, runner.id());
        assert_eq!(found[0].task_id, "t");

        stop_jobs_in(&app, &home.join("tasks/t")).unwrap();
        assert!(!runner.wait().unwrap().success());
    }

    /// A runner that cannot be recorded is killed, and its job left for
    /// [`start`] to end, unless it is the user's: the user needs no anchor,
    /// so that job runs on.
    #[cfg(unix)]
    #[test]
    fn only_the_users_runner_runs_on_unrecorded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        // A runner gone before it is recorded: its process cannot be read.
        let gone = || {
            let mut runner = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
            drop(runner.stdin.take());
            runner.wait().unwrap();
            runner
        };

        let pid = || app.db.get_review("t").unwrap().unwrap().job.unwrap().pid;
        app.db.set_review_job("t", &job(None, "abc")).unwrap();
        let mut runner = gone();
        let err = hand_over(&app, "t", false, &mut runner).unwrap_err();
        assert!(format!("{err:#}").contains("cannot be read"), "{err:#}");
        assert_eq!(pid(), None);

        let mut runner = gone();
        hand_over(&app, "t", true, &mut runner).unwrap();
        assert_eq!(pid(), Some(runner.id()));
        assert!(app
            .db
            .task_anchors("t", AnchorKind::Job)
            .unwrap()
            .is_empty());
    }
    /// The job files older jeets left become reviews, once, and nothing is
    /// deleted: archived when finished or lost, active while running; a PR
    /// that has a review gets no second.
    #[cfg(unix)]
    #[test]
    fn job_files_older_jeets_left_become_reviews_once() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let app = test_app(&home);
        app.db
            .upsert_repo(&RepoRecord {
                id: "github.com/acme/api".into(),
                trunk_path: "/nowhere/api".into(),
                remote_url: "https://github.com/acme/api.git".into(),
                default_branch: "main".into(),
            })
            .unwrap();
        // PR 8 has a review that has a job already; PR 7's, `t`, has none.
        let mut held = ReviewRecord::new("h".into(), "h".into(), "github.com/acme/api", 8, 0);
        held.job = Some(job(Some(0), "kept"));
        app.db.insert_review(&held).unwrap();

        // The worktree a job ran in is never touched.
        let worktree = home.join("jobs-worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("work.txt"), "mine").unwrap();
        let put = |repo: &str, n: &str, job: Option<Job>, raw: &str| {
            let dir = home.join("reviews").join(repo);
            std::fs::create_dir_all(&dir).unwrap();
            let text = match job {
                Some(job) => serde_json::to_string_pretty(&job).unwrap(),
                None => raw.to_string(),
            };
            std::fs::write(dir.join(format!("{n}.json")), text).unwrap();
        };
        let mk = |pr: u64, title: &str, pid: Option<u32>, exit: Option<i32>| {
            let mut j = job(exit, "abc");
            j.pr = pr;
            j.title = title.into();
            j.worktree = worktree.clone();
            j.started_at = 100;
            j.finished_at = exit.map(|_| 110);
            j.pid = pid;
            j
        };
        let api = "github.com/acme/api";
        put(api, "1", Some(mk(1, "finished", Some(5), Some(0))), "");
        put(api, "2", Some(mk(2, "failed", Some(5), Some(1))), "");
        put(api, "3", Some(mk(3, "lost", Some(u32::MAX - 1), None)), "");
        put(
            api,
            "4",
            Some(mk(4, "running", Some(std::process::id()), None)),
            "",
        );
        put(
            api,
            "7",
            Some(mk(7, "for the review t", Some(5), Some(0))),
            "",
        );
        put(
            api,
            "8",
            Some(mk(8, "for the held review", Some(5), Some(0))),
            "",
        );
        put(api, "9", Some(mk(9, "never launched", None, None)), "");
        put(api, "6", None, "{not json");
        put(
            "github.com/gone/old",
            "5",
            Some(mk(5, "unindexed", Some(5), Some(0))),
            "",
        );
        std::fs::write(home.join("reviews").join(api).join("1.log"), "output").unwrap();
        let files = || -> Vec<(PathBuf, String)> {
            let mut all: Vec<_> = walkdir::WalkDir::new(home.join("reviews"))
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| {
                    let text = std::fs::read_to_string(e.path()).unwrap();
                    (e.into_path(), text)
                })
                .collect();
            all.sort();
            all
        };
        let before = files();
        assert_eq!(before.len(), 10);

        adopt(&app).unwrap();

        let get = |repo: &str, n: u64| app.db.find_review(repo, n).unwrap();
        let gone = "github.com/gone/old";
        for (repo, n, title, state) in [
            (api, 1, "finished", TaskState::Archived),
            (api, 2, "failed", TaskState::Archived),
            (api, 3, "lost", TaskState::Archived),
            (api, 4, "running", TaskState::Active),
            (gone, 5, "unindexed", TaskState::Archived),
            (api, 9, "never launched", TaskState::Archived),
        ] {
            let r = get(repo, n).unwrap_or_else(|| panic!("{repo}#{n} was not adopted"));
            assert_eq!((r.title.as_str(), r.state), (title, state), "{repo}#{n}");
            let repo_name = repo.rsplit('/').next().unwrap();
            assert_eq!(r.slug, format!("review-{repo_name}-{n}"));
            assert_eq!(r.dir_name, None, "no worktree");
            // From when the job started, to when it finished if it did.
            assert_eq!(r.created_at, 100);
            match n {
                1 | 2 | 5 => assert_eq!(r.last_activity, 110),
                3 | 4 => assert_eq!(r.last_activity, 100),
                _ => assert!(r.last_activity > 110),
            }
            assert!(dir(&app, &r).join(tasks::MARKER_DIR).is_dir());
            assert_eq!(r.job.as_ref().unwrap().worktree, worktree);
        }
        let state = |n: u64| job_of(&app, &get(api, n).unwrap()).unwrap().state();
        assert_eq!(state(1), JobState::Finished(0));
        assert_eq!(state(2), JobState::Finished(1));
        // Lost is stored ended, with no end time, so no runner takes it up.
        assert_eq!(state(3), JobState::Finished(LOST));
        let lost = job_of(&app, &get(api, 3).unwrap()).unwrap();
        assert_eq!(lost.finished_at, None);
        assert_eq!(lost.describe(), "stopped without finishing");
        for n in [1, 2, 3, 9] {
            let id = get(api, n).unwrap().id;
            assert!(crate::permissions::check_review_job(&app.db, &id, 5).is_err());
            assert!(!app.db.set_review_job_runner(&id, 5, None).unwrap());
        }
        assert_eq!(state(4), JobState::Running);
        // Never launched is recorded as a refused runner is.
        assert_eq!(state(9), JobState::Finished(REFUSED));
        // Unreadable, and no second review for a PR that has one.
        assert!(get(api, 6).is_none());
        assert_eq!(app.db.list_reviews(None).unwrap().len(), 2 + 6);
        let t = app.db.get_review("t").unwrap().unwrap();
        let folded = t.job.unwrap();
        assert_eq!((folded.started_at, folded.exit_code), (100, Some(0)));
        assert_eq!(
            app.db.get_review("h").unwrap().unwrap().job.unwrap().head,
            "kept"
        );

        // Nothing was deleted, moved or changed, the job's worktree included.
        assert_eq!(files(), before);
        assert_eq!(
            std::fs::read_to_string(worktree.join("work.txt")).unwrap(),
            "mine"
        );

        // One line per file.
        let log = std::fs::read_to_string(home.join("logs").join(ADOPT_LOG)).unwrap();
        assert_eq!(log.lines().count(), 9, "{log}");
        for (n, what) in [
            ("1.json", "adopted"),
            ("3.json", "lost"),
            ("4.json", "still running"),
            ("5.json", "adopted"),
            ("6.json", "left"),
            ("7.json", "which had no job"),
            ("8.json", "has a job already"),
            ("9.json", "never launched"),
        ] {
            let line = log.lines().find(|l| l.contains(&format!("/{n}"))).unwrap();
            assert!(line.contains(what), "{n}: {line}");
        }
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_some());

        // Each file is recorded; a review deleted since is not made anew.
        let recorded = app
            .db
            .job_file(
                &home
                    .join("reviews")
                    .join(api)
                    .join("1.json")
                    .to_string_lossy(),
            )
            .unwrap()
            .unwrap();
        assert!(recorded.0.starts_with("adopted"), "{recorded:?}");
        let one = get(api, 1).unwrap();
        app.db.delete_review(&one.id).unwrap();
        app.db.delete_meta(JOBS_ADOPTED).unwrap();
        adopt(&app).unwrap();
        assert!(get(api, 1).is_none());
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_some());
        let again = std::fs::read_to_string(home.join("logs").join(ADOPT_LOG)).unwrap();
        assert_eq!(again, log);

        // Looked through once: a file that turns up later is not adopted,
        // and nothing is logged twice.
        put(api, "10", Some(mk(10, "later", Some(5), Some(0))), "");
        adopt(&app).unwrap();
        assert!(get(api, 10).is_none());
        assert_eq!(app.db.list_reviews(None).unwrap().len(), 7);
        let again = std::fs::read_to_string(home.join("logs").join(ADOPT_LOG)).unwrap();
        assert_eq!(again, log);
    }

    /// A file whose review cannot be made is tried three times, logged when
    /// its outcome changes, then left; the marker is set once none is pending.
    #[cfg(unix)]
    #[test]
    fn a_job_file_that_cannot_be_adopted_is_tried_three_times() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let app = test_app(&home);
        let dir = home.join("reviews/github.com/acme/api");
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = job(Some(0), "abc");
        j.pr = 3;
        let file = dir.join("3.json");
        std::fs::write(&file, serde_json::to_string(&j).unwrap()).unwrap();
        // A file where the tasks directory goes: no review directory can be made.
        let _ = std::fs::remove_dir_all(app.tasks_root());
        std::fs::write(app.tasks_root(), "in the way").unwrap();
        let log = || std::fs::read_to_string(home.join("logs").join(ADOPT_LOG)).unwrap_or_default();

        adopt(&app).unwrap();
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_none());
        assert_eq!(log().lines().count(), 1);
        adopt(&app).unwrap();
        // The same failure is not logged again.
        assert_eq!(log().lines().count(), 1);
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_none());
        adopt(&app).unwrap();
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_some());
        let log = log();
        assert_eq!(log.lines().count(), 2, "{log}");
        assert!(log.contains("left: could not adopt"), "{log}");
        let (outcome, attempts) = app.db.job_file(&file.to_string_lossy()).unwrap().unwrap();
        assert_eq!(attempts, ATTEMPTS);
        assert!(outcome.starts_with("left: could not adopt"), "{outcome}");
        // No row was left behind by the failures, and the file is kept.
        assert!(app
            .db
            .find_review("github.com/acme/api", 3)
            .unwrap()
            .is_none());
        assert!(file.is_file());
    }

    /// An owner may be a group with subgroups, so a job file lies deeper.
    #[test]
    fn job_files_include_nested_owners_only_when_named_for_a_repo() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for rel in [
            "gitlab.com/group/sub/repo/5.json",
            "github.com/acme/api/1.json",
            "github.com/acme/api/1.log",
            "github.com/acme/api/x.json",
            "github.com/acme/2.json",
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        let found: Vec<_> = job_files(root)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            found,
            [
                "github.com/acme/api/1.json",
                "gitlab.com/group/sub/repo/5.json"
            ]
        );
        assert_eq!(
            job_key(root, &root.join("gitlab.com/group/sub/repo/5.json")),
            Some(("gitlab.com/group/sub/repo".to_string(), 5))
        );
    }

    /// No job files, nothing to log: the marker is set all the same.
    #[cfg(unix)]
    #[test]
    fn no_job_files_leave_no_log_and_are_looked_for_once() {
        let home = tempfile::tempdir().unwrap();
        let app = test_app(home.path());
        adopt(&app).unwrap();
        assert!(app.db.meta(JOBS_ADOPTED).unwrap().is_some());
        assert!(!home.path().join("logs").join(ADOPT_LOG).exists());
    }

    // -----------------------------------------------------------------------
    // The review's life, against real git
    // -----------------------------------------------------------------------

    fn hermetic_git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A home with a repository in the store, cloned from a local origin.
    fn git_lab() -> (tempfile::TempDir, App, RepoRecord) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
        };
        let origin = root.join("origin");
        let trunk = root.join("trunk");
        std::fs::create_dir(&origin).unwrap();
        hermetic_git(&origin, &["init", "-q", "-b", "main"]);
        hermetic_git(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
        hermetic_git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                trunk.to_str().unwrap(),
            ],
        );
        let repo = RepoRecord {
            id: "example.com/o/api".into(),
            trunk_path: trunk.display().to_string(),
            remote_url: format!("file://{}", origin.display()),
            default_branch: "main".into(),
        };
        app.db.upsert_repo(&repo).unwrap();
        (dir, app, repo)
    }

    /// A review of PR 7 with its directory and a worktree on a branch it
    /// made, as `review_for` leaves one.
    fn reviewed(app: &App, repo: &RepoRecord) -> ReviewRecord {
        let mut review = ReviewRecord::new(
            "rv-test-id".into(),
            "zz-review-api-7".into(),
            &repo.id,
            7,
            1,
        );
        review.title = "Add x".into();
        let dir = dir(app, &review);
        std::fs::create_dir_all(&dir).unwrap();
        tasks::write_marker(&dir, &review.id).unwrap();
        let trunk = Path::new(&repo.trunk_path);
        hermetic_git(
            trunk,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "pr/7",
                dir.join("api").to_str().unwrap(),
            ],
        );
        review.dir_name = Some("api".into());
        review.branch = Some("pr/7".into());
        review.created_branch = true;
        app.db.insert_review(&review).unwrap();
        app.db.get_review(&review.id).unwrap().unwrap()
    }

    #[test]
    fn a_review_is_archived_brought_back_and_deleted_with_its_work_kept() {
        let (_dir, app, repo) = git_lab();
        let review = reviewed(&app, &repo);
        let trunk = Path::new(&repo.trunk_path);
        let worktree = worktree_path(&app, &review).unwrap();
        std::fs::write(worktree.join("notes.md"), "half a review\n").unwrap();

        archive(&app, &review).unwrap();
        let archived = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(archived.state, TaskState::Archived);
        assert!(!worktree.exists());
        assert!(archived.head_oid.is_some() && archived.stash_ref.is_some());
        let head_ref = tasks::jeet_ref("head", &review.id, "api");
        assert!(git::ref_exists(trunk, &head_ref).unwrap());
        // Archiving again, or running it half-way, changes nothing more.
        archive(&app, &archived).unwrap();

        let warnings = unarchive(&app, &archived).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let back = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Active);
        assert!(back.brought_back_at.is_some());
        assert_eq!((&back.head_oid, &back.stash_ref), (&None, &None));
        assert_eq!(
            std::fs::read_to_string(worktree.join("notes.md")).unwrap(),
            "half a review\n"
        );
        assert_eq!(git::head_branch(&worktree).as_deref(), Some("pr/7"));
        assert!(!git::ref_exists(trunk, &head_ref).unwrap());

        let losses = losses(&app, &back).unwrap();
        assert!(
            losses.iter().any(|l| l.contains("uncommitted")),
            "{losses:?}"
        );
        delete(&app, &back).unwrap();
        assert!(app.db.get_review(&review.id).unwrap().is_none());
        assert!(!dir(&app, &review).exists());
        // The branch it made goes with it, and so do its refs.
        assert!(!git::branch_exists(trunk, "pr/7").unwrap());
        assert!(git::refs_under(trunk, "refs/jeet/")
            .unwrap_or_default()
            .is_empty());
    }

    #[test]
    fn a_second_ask_for_one_pr_gets_the_same_review() {
        let (_dir, app, repo) = git_lab();
        let pr = PullRequest { number: 7, ..pr() };
        let assistant = crate::assistant::ensure(&app).unwrap();
        let (first, created) = make_review(&app, &repo, &pr, Some(&assistant.id)).unwrap();
        assert!(created);
        assert_eq!(first.requested_by.as_deref(), Some(assistant.id.as_str()));
        // As two `jeet review`s racing: the slower finds the faster's.
        let (second, created) = make_review(&app, &repo, &pr, None).unwrap();
        assert!(!created);
        assert_eq!(second.id, first.id);
        assert_eq!(app.db.list_reviews(None).unwrap().len(), 1);
        assert!(!app.tasks_root().join(format!("{}-2", first.slug)).exists());
        assert!(dir(&app, &first).join(".jeet/task-id").is_file());
        // Another PR, another review; the same name is never a task's.
        let other = PullRequest { number: 8, ..pr };
        let (third, _) = make_review(&app, &repo, &other, None).unwrap();
        assert_ne!(third.id, first.id);
        assert_eq!(
            app.db.find_review(&repo.id, 8).unwrap().unwrap().id,
            third.id
        );
    }

    /// The worktree of a duplicate review task, as migration to v8 leaves
    /// it to archive: `dir_name` `api`, on its own branch.
    fn duplicate(app: &App, repo: &RepoRecord, slug: &str) -> PathBuf {
        let dir = app.tasks_root().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        hermetic_git(
            Path::new(&repo.trunk_path),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                slug,
                dir.join("api").to_str().unwrap(),
            ],
        );
        dir.join("api")
    }

    fn leftover_rows(app: &App) -> Vec<(String, i64, Option<String>)> {
        let conn = rusqlite::Connection::open(crate::config::index_path(&app.home)).unwrap();
        let mut stmt = conn
            .prepare("SELECT task_id, done, note FROM review_leftovers ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// What migration to v8 leaves of a duplicate's worktree is archived the
    /// way `jeet task archive` does it, unless it has ignored files that
    /// are not caches, when it stays where it is; and a second look finds
    /// nothing to do.
    #[test]
    fn leftovers_are_archived_to_refs_or_left_in_place_and_only_once() {
        let (_dir, app, repo) = git_lab();
        let trunk = Path::new(&repo.trunk_path);
        std::fs::write(trunk.join(".git/info/exclude"), ".env\ntarget/\n").unwrap();
        let dirty = duplicate(&app, &repo, "zz-dup-dirty");
        std::fs::write(dirty.join("notes.md"), "half a review\n").unwrap();
        std::fs::create_dir_all(dirty.join("target")).unwrap();
        std::fs::write(dirty.join("target/cache"), "x").unwrap();
        let secret = duplicate(&app, &repo, "zz-dup-secret");
        std::fs::write(secret.join(".env"), "TOKEN=x\n").unwrap();
        let gone = app.tasks_root().join("zz-dup-gone");
        std::fs::create_dir_all(&gone).unwrap();

        let conn = rusqlite::Connection::open(crate::config::index_path(&app.home)).unwrap();
        for (task, slug) in [
            ("dup-dirty", "zz-dup-dirty"),
            ("dup-secret", "zz-dup-secret"),
            ("dup-gone", "zz-dup-gone"),
        ] {
            conn.execute(
                "INSERT INTO review_leftovers (task_id, slug, repo_id, pr_number, dir_name,
                     branch, created_branch, was_archived, done)
                 VALUES (?1, ?2, ?3, 7, 'api', ?2, 1, 0, 0)",
                rusqlite::params![task, slug, repo.id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('reviews_v8_pending', '1'),
                 ('reviews_v8_kept', 'zz-review-api-7 example.com/o/api#7')",
            [],
        )
        .unwrap();
        drop(conn);

        migrate(&app).unwrap();
        // The dirty one: its work kept in the trunk, the worktree gone.
        assert!(!dirty.exists());
        let head = tasks::jeet_ref("head", "dup-dirty", "api");
        let stash = tasks::jeet_ref("stash", "dup-dirty", "api");
        assert!(git::ref_exists(trunk, &head).unwrap());
        assert!(git::ref_exists(trunk, &stash).unwrap());
        // Its task directory is never removed, nor is the branch.
        assert!(app.tasks_root().join("zz-dup-dirty").is_dir());
        assert!(git::branch_exists(trunk, "zz-dup-dirty").unwrap());
        // The one with a .env stays exactly as it was.
        assert!(secret.join(".env").is_file());
        let rows = leftover_rows(&app);
        assert_eq!(rows.len(), 3);
        assert!(rows[0]
            .2
            .as_deref()
            .unwrap()
            .starts_with("archived: HEAD at "));
        assert!(rows[1].2.as_deref().unwrap().contains("left in place"));
        assert_eq!(rows[2].2.as_deref(), Some("already gone"));
        assert!(rows.iter().all(|r| r.1 == 1));
        assert_eq!(app.db.meta(PENDING).unwrap(), None);
        let log = std::fs::read_to_string(app.home.join("logs").join(MIGRATE_LOG)).unwrap();
        assert!(log.contains("kept review zz-review-api-7"), "{log}");
        assert!(log.contains("zz-dup-dirty/api: archived"), "{log}");
        assert!(log.contains(&stash), "{log}");
        assert!(
            log.contains("left in place: ignored files such as .env"),
            "{log}"
        );

        // Again: nothing is pending, so nothing changes or is logged.
        let before = std::fs::read_to_string(app.home.join("logs").join(MIGRATE_LOG)).unwrap();
        migrate(&app).unwrap();
        assert_eq!(
            std::fs::read_to_string(app.home.join("logs").join(MIGRATE_LOG)).unwrap(),
            before
        );
        assert_eq!(leftover_rows(&app), rows);
    }

    /// What a review task kept in files under its directory goes on its row.
    #[test]
    fn what_a_review_task_kept_in_files_goes_on_its_row() {
        let (_dir, app, repo) = git_lab();
        let review = reviewed(&app, &repo);
        let marker = dir(&app, &review).join(tasks::MARKER_DIR);
        std::fs::write(
            marker.join("review.json"),
            r#"{"pr": 7, "head": "abc", "at": 123}"#,
        )
        .unwrap();
        std::fs::write(marker.join("brought-back"), "456").unwrap();
        app.db.set_meta_for_test(PENDING).unwrap();
        migrate(&app).unwrap();
        let got = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(
            (got.asked_head.as_deref(), got.asked_at, got.brought_back_at),
            (Some("abc"), Some(123), Some(456))
        );
        // What was asked of another PR is not this one's.
        let marker_pr = r#"{"pr": 9, "head": "zzz", "at": 1}"#;
        std::fs::write(marker.join("review.json"), marker_pr).unwrap();
        app.db.set_review_asked(&review.id, "newer", 999).unwrap();
        app.db.set_meta_for_test(PENDING).unwrap();
        migrate(&app).unwrap();
        let got = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(got.asked_head.as_deref(), Some("newer"));
    }

    fn insert_leftover(app: &App, repo: &RepoRecord, task: &str, slug: &str) {
        let conn = rusqlite::Connection::open(crate::config::index_path(&app.home)).unwrap();
        conn.execute(
            "INSERT INTO review_leftovers (task_id, slug, repo_id, pr_number, dir_name,
                 branch, created_branch, was_archived, done)
             VALUES (?1, ?2, ?3, 7, 'api', ?2, 1, 0, 0)",
            rusqlite::params![task, slug, repo.id],
        )
        .unwrap();
        app.db.set_meta_for_test(PENDING).unwrap();
    }

    /// A leftover recorded under a review's id or slug is the review's own
    /// worktree: never archived.
    #[test]
    fn a_leftover_that_belongs_to_a_review_is_left_in_place() {
        let (_dir, app, repo) = git_lab();
        let review = reviewed(&app, &repo);
        let worktree = worktree_path(&app, &review).unwrap();
        insert_leftover(&app, &repo, &review.id, "some-old-slug");
        insert_leftover(&app, &repo, "old-task", &review.slug);
        migrate(&app).unwrap();
        assert!(worktree.is_dir());
        assert!(git::branch_exists(Path::new(&repo.trunk_path), "pr/7").unwrap());
        let rows = leftover_rows(&app);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.1, 1);
            assert_eq!(
                row.2.as_deref(),
                Some(format!("left in place: belongs to review {}", review.slug).as_str())
            );
        }
        let log = std::fs::read_to_string(app.home.join("logs").join(MIGRATE_LOG)).unwrap();
        assert!(log.contains("belongs to review"), "{log}");
    }

    /// One that cannot be archived is tried three times, then left.
    #[test]
    fn a_leftover_that_cannot_be_archived_is_given_up_after_three_tries() {
        let (_dir, app, repo) = git_lab();
        // Not a worktree at all, so listing what git ignores fails.
        std::fs::create_dir_all(app.tasks_root().join("zz-dup-broken/api")).unwrap();
        insert_leftover(&app, &repo, "dup-broken", "zz-dup-broken");
        for _ in 0..2 {
            migrate(&app).unwrap();
            assert_eq!(leftover_rows(&app)[0].1, 0);
            assert!(app.db.meta(PENDING).unwrap().is_some());
        }
        migrate(&app).unwrap();
        let rows = leftover_rows(&app);
        assert_eq!(rows[0].1, 1);
        assert!(
            rows[0]
                .2
                .as_deref()
                .unwrap()
                .starts_with("left in place: could not archive ("),
            "{rows:?}"
        );
        assert_eq!(app.db.meta(PENDING).unwrap(), None);
    }

    /// The markers are read from the review's directory when its columns are
    /// empty, whether or not the post-step has run.
    #[test]
    fn markers_are_folded_for_a_reader_before_the_post_step() {
        let (_dir, app, repo) = git_lab();
        let review = reviewed(&app, &repo);
        assert!(asked(&review).is_none());
        let marker = dir(&app, &review).join(tasks::MARKER_DIR);
        std::fs::write(
            marker.join("review.json"),
            r#"{"pr": 7, "head": "abc", "at": 123}"#,
        )
        .unwrap();
        std::fs::write(marker.join("brought-back"), "456").unwrap();
        let got = folded(&app, &review);
        assert_eq!(
            asked(&got).map(|a| (a.head, a.at)),
            Some(("abc".into(), 123))
        );
        assert_eq!(got.brought_back_at, Some(456));
        // And on the row, so it is read once.
        let row = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(row.brought_back_at, Some(456));
        // A PR that was done before it was brought back is not retired.
        assert!(matches!(
            retire(&app, &review, Some(100)).unwrap(),
            Retired::BroughtBack
        ));
    }

    /// The second of two `jeet review`s that raced for the lock finds the
    /// first's worktree recorded and uses it, making no second.
    #[test]
    fn a_second_check_out_reuses_the_first_ones_worktree() {
        let (_dir, app, repo) = git_lab();
        let recorded = reviewed(&app, &repo);
        let pr = PullRequest { number: 7, ..pr() };
        let worktree = worktree_path(&app, &recorded).unwrap();
        // Uncommitted: the refresh from the PR is not attempted.
        std::fs::write(worktree.join("notes.md"), "x\n").unwrap();
        // What the slower one read before the faster one recorded its own.
        let mut stale = recorded.clone();
        stale.dir_name = None;
        stale.branch = None;
        let (path, _) = check_out(&app, &mut stale, &repo, &pr).unwrap();
        assert_eq!(path, worktree);
        assert_eq!(stale.dir_name.as_deref(), Some("api"));
        let dirs = std::fs::read_dir(dir(&app, &recorded))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().join(".git").exists())
            .count();
        assert_eq!(dirs, 1);
    }

    #[test]
    fn who_asked_sticks_when_the_user_asks_again() {
        let (_dir, app, repo) = git_lab();
        let review = reviewed(&app, &repo);
        let assistant = crate::assistant::ensure(&app).unwrap();
        remember_requester(&app, &review.id, Some(&assistant.id)).unwrap();
        remember_requester(&app, &review.id, None).unwrap();
        let got = app.db.get_review(&review.id).unwrap().unwrap();
        assert_eq!(got.requested_by.as_deref(), Some(assistant.id.as_str()));
    }
}
