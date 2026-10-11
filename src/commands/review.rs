//! `jeet review <pr>`: find or make the PR's review task, check the PR out
//! in it, and open the review viewer.

use std::io::{self, IsTerminal};

use anyhow::Result;

use crate::cd;
use crate::context::App;
use crate::github;
use crate::permissions::{self, Caller};
use crate::pr_history;
use crate::resolve::repo_from_filter_or_cwd;
use crate::review;
use crate::tasks;

pub fn run(
    app: &App,
    pr: &str,
    repo_filter: Option<&str>,
    start_command: bool,
    rerun: bool,
) -> Result<()> {
    let number = github::parse_pr_number(pr)?;
    let repo = repo_from_filter_or_cwd(app, repo_filter)?;
    let trunk = std::path::Path::new(&repo.trunk_path);

    eprintln!("jeet: looking up #{number}");
    let found = github::pr_history(trunk, number)?;
    let pr = found.pr.clone();
    eprintln!("jeet: #{} {} ({})", pr.number, pr.title, pr.state_label());

    // What the assistant checks out reports to it, like a task it makes.
    let caller = permissions::caller(app)?;
    let parent = match caller {
        Caller::Root => Some(super::task::assistant_parent(app)?.id),
        Caller::User | Caller::Node(_) => None,
    };
    let by = parent.as_ref().map(|_| &caller);
    let review = tasks::review_task(app, &repo, &pr, by)?;
    // Only now: a sync started before the checkout would be fetching this
    // repo just as `gh pr checkout` does. The checkout waits out one already
    // running, but there is no need to start one in its way.
    crate::store::maybe_sync_background(app);
    for warning in &review.warnings {
        eprintln!("jeet: {warning}");
    }
    // The history the tasks list shows starts with the PR under review.
    let record = pr_history::to_record(&review.task.id, &repo.id, found);
    if let Err(e) = app.db.upsert_task_pr(&record) {
        eprintln!("jeet: warning: could not record #{number} in the task: {e:#}");
    }
    let verb = if review.created { "new task" } else { "task" };
    eprintln!(
        "jeet: {verb} {} -> {}",
        review.task.name,
        review.path.display()
    );

    if start_command {
        // Under a parent, the review command is the task's agent and reports.
        let as_task = review
            .task
            .parent_id
            .as_ref()
            .map(|_| review.task.id.as_str());
        // The runner is always recorded as the task it runs for: the review
        // task under a parent, the assistant that started it for a review
        // task found at the top level (never moved under it), and otherwise
        // the top-level review task itself, which runs as the user. Recorded,
        // archiving or deleting the task finds the job and ends it;
        // unrecorded, a runner that inherits the assistant's environment is
        // refused once this jeet has exited.
        let runner_for = as_task
            .or(parent.as_deref())
            .unwrap_or(review.task.id.as_str());
        start_review(app, &repo, &pr, &review.path, rerun, as_task, runner_for)?;
    }

    if io::stdout().is_terminal() {
        // The review itself, then the shell lands in the worktree to work on it.
        crate::tui::review::run(&review.path, &pr)?;
        return crate::commands::explore::hand_off(&review.path);
    }
    // Scripted: behave like `jeet worktree`, printing the path to consume.
    println!("{}", review.path.display());
    cd::request(&review.path)
}

fn start_review(
    app: &App,
    repo: &crate::db::RepoRecord,
    pr: &github::PullRequest,
    worktree: &std::path::Path,
    rerun: bool,
    as_task: Option<&str>,
    runner_for: &str,
) -> Result<()> {
    let config = &app.config.review;
    let Some(template) = config.command() else {
        return Ok(());
    };
    let existing = review::load(app, repo, pr.number);
    if let Some(job) = &existing {
        if rerun && job.state() == review::JobState::Running {
            eprintln!(
                "jeet: a review of #{} is still running; not starting another",
                pr.number
            );
            return Ok(());
        }
    }
    if !rerun {
        if let Some(why) = review::reason_to_skip(existing.as_ref(), &pr.head_ref_oid) {
            eprintln!("jeet: {why}");
            return Ok(());
        }
    }
    let job = review::start(
        app, repo, pr, worktree, config, template, as_task, runner_for,
    )?;
    eprintln!("jeet: reviewing in the background: {}", job.command);
    eprintln!("jeet: output -> {}", job.log.display());
    if as_task.is_some() {
        // No harness launches it, so no deny rules reach it (as for a task
        // on a harness without them).
        eprintln!(
            "jeet: warning: the review command runs outside a harness launch, so it has no \
             deny rules and is not stopped from force-pushing or merging; only jeet's own \
             commands are gated"
        );
    }
    Ok(())
}
