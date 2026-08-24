use std::io::{IsTerminal, Write};

use anyhow::{bail, Result};

use crate::cd;
use crate::context::App;
use crate::db::RepoRecord;
use crate::resolve;
use crate::worktrees::{self, CleanOptions, WorktreeKind};

/// `jeet worktree [name]` — create a worktree from anywhere inside a repo.
///
/// With a name, a branch of that name is created and published to `origin`.
/// Without one, a detached checkout of the default branch is created under the
/// ephemeral root, the same shape `jeet exec --ephemeral` uses.
pub fn create(app: &App, name: Option<&str>, repo_filter: Option<&str>, push: bool) -> Result<()> {
    let repo = repo_from_filter_or_cwd(app, repo_filter)?;

    let dest = match name {
        Some(branch) => {
            let branch = branch.trim();
            if branch.is_empty() {
                bail!("branch name cannot be empty");
            }
            let created = worktrees::create_named(app, &repo, branch, push)?;
            for warning in &created.warnings {
                eprintln!("jeet: {warning}");
            }
            eprintln!("jeet: worktree {branch} -> {}", created.path.display());
            created.path
        }
        None => {
            let dest = worktrees::create_detached(app, &repo)?;
            eprintln!(
                "jeet: detached worktree on {} -> {}",
                repo.default_branch,
                dest.display()
            );
            eprintln!("jeet: remove it later with `jeet worktree clean`");
            dest
        }
    };

    println!("{}", dest.display());
    cd::request(&dest)?;
    Ok(())
}

pub fn add(app: &App, filter: &str, branch: &str, push: bool) -> Result<()> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let created = worktrees::create_named(app, &repo, branch, push)?;
    for warning in &created.warnings {
        eprintln!("jeet: {warning}");
    }
    println!("worktree {} -> {}", branch, created.path.display());
    Ok(())
}

pub fn ls_cmd(app: &App, filter: Option<&str>) -> Result<()> {
    let repos = match filter {
        Some(f) => vec![resolve::resolve_repo_filter(&app.db, f)?],
        None => app.db.list_repos(None)?,
    };

    for repo in repos {
        println!("{}:", repo.id);
        let entries = match worktrees::list(app, &repo) {
            Ok(entries) => entries,
            Err(e) => {
                println!("  (unavailable: {e})");
                continue;
            }
        };
        for entry in entries {
            let status = worktrees::status_for(&repo, &entry);
            let mut notes = vec![format!("[{}]", entry.kind.label())];
            if entry.missing {
                notes.push("MISSING".to_string());
            }
            if status.dirty > 0 {
                notes.push(format!("{} uncommitted", status.dirty));
            }
            if status.ahead > 0 || status.behind > 0 {
                notes.push(format!("+{}/-{} commits", status.ahead, status.behind));
            }
            notes.push(status.diff_summary());
            println!(
                "  {:<28} {} {}",
                entry.display_name(),
                entry.path.display(),
                notes.join(" ")
            );
        }
    }
    Ok(())
}

pub fn remove(app: &App, filter: &str, branch: &str, force: bool) -> Result<()> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let entries = worktrees::list(app, &repo)?;
    let entry = entries
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch) && e.kind != WorktreeKind::Trunk)
        .ok_or_else(|| anyhow::anyhow!("no worktree for branch {branch} on {}", repo.id))?;

    worktrees::remove(app, &repo, entry, force)?;
    println!("removed worktree {branch} from {}", repo.id);
    Ok(())
}

/// `jeet worktree clean` — drop worktrees that hold no work, reporting the rest.
pub fn clean(
    app: &App,
    filter: Option<&str>,
    all: bool,
    force: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    let repos = match filter {
        Some(f) => vec![resolve::resolve_repo_filter(&app.db, f)?],
        None => vec![repo_from_filter_or_cwd(app, None)?],
    };
    let opts = CleanOptions { all, force };

    for repo in repos {
        let candidates = worktrees::clean_candidates(app, &repo, opts)?;
        if candidates.is_empty() {
            println!("{}: no worktrees besides the trunk", repo.id);
            continue;
        }

        println!("{}:", repo.id);
        for candidate in &candidates {
            let marker = if candidate.removable {
                "[remove]"
            } else {
                "[keep]  "
            };
            println!(
                "  {marker} {:<24} {:<11} {:<16} {}",
                candidate.entry.display_name(),
                candidate.entry.kind.label(),
                candidate.status.diff_summary(),
                candidate.reason
            );
        }

        let removable: Vec<_> = candidates.iter().filter(|c| c.removable).collect();
        if removable.is_empty() {
            println!("  nothing to clean");
            continue;
        }
        if dry_run {
            println!(
                "  dry run: {} worktree(s) would be removed",
                removable.len()
            );
            continue;
        }
        if !assume_yes && !confirm(&format!("  remove {} worktree(s)?", removable.len()))? {
            println!("  skipped");
            continue;
        }

        for candidate in removable {
            match worktrees::remove(app, &repo, &candidate.entry, true) {
                Ok(()) => println!("  removed {}", candidate.entry.path.display()),
                Err(e) => eprintln!("  failed to remove {}: {e}", candidate.entry.path.display()),
            }
        }
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to remove worktrees without a terminal; pass --yes or --dry-run");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Resolve the repository from an explicit filter, else from the current directory.
pub fn repo_from_filter_or_cwd(app: &App, filter: Option<&str>) -> Result<RepoRecord> {
    if let Some(filter) = filter {
        return resolve::resolve_repo_filter(&app.db, filter);
    }
    let cwd = std::env::current_dir()?;
    match resolve::resolve_context(app, &cwd) {
        Ok(ctx) => Ok(ctx.repo),
        Err(e) => bail!("{e}\nrun from inside a repository or pass --repo <filter>"),
    }
}
