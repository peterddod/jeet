//! `jeet review <pr>`: check a pull request out and start reviewing it.

use std::io::{self, IsTerminal};

use anyhow::Result;

use crate::cd;
use crate::commands::worktree::repo_from_filter_or_cwd;
use crate::context::App;
use crate::github;
use crate::review;
use crate::worktrees;

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
    let pr = github::pr_view(trunk, number)?;
    eprintln!("jeet: #{} {} ({})", pr.number, pr.title, pr.state_label());

    let checkout = worktrees::checkout_pr(app, &repo, &pr)?;
    for warning in &checkout.warnings {
        eprintln!("jeet: {warning}");
    }
    eprintln!("jeet: {} -> {}", pr.local_branch(), checkout.path.display());

    if start_command {
        start_review(app, &repo, &pr, &checkout.path, rerun)?;
    }

    if io::stdout().is_terminal() {
        return crate::commands::explore::run_at(app, &checkout.path);
    }
    // Scripted: behave like `jeet worktree`, printing the path to consume.
    println!("{}", checkout.path.display());
    cd::request(&checkout.path)
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
