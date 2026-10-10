//! `jeet review <pr>`: find or make the PR's review task, check the PR out
//! in it, and open the review viewer.

use std::io::{self, IsTerminal};

use anyhow::Result;

use crate::cd;
use crate::context::App;
use crate::github;
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

    let review = tasks::review_task(app, &repo, &pr)?;
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
        start_review(app, &repo, &pr, &review.path, rerun)?;
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
    let job = review::start(app, repo, pr, worktree, config, template)?;
    eprintln!("jeet: reviewing in the background: {}", job.command);
    eprintln!("jeet: output -> {}", job.log.display());
    Ok(())
}
