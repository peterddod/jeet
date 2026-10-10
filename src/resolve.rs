use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::db::{Database, RepoRecord};

pub fn resolve_repo_filter(db: &Database, filter: &str) -> Result<RepoRecord> {
    if let Some(repo) = db.get_repo(filter)? {
        return Ok(repo);
    }

    let repos = db.list_repos(None)?;
    let filter_lower = filter.to_lowercase();
    let suffix_matches: Vec<_> = repos
        .iter()
        .filter(|r| {
            let parts: Vec<&str> = r.id.split('/').collect();
            if parts.len() < 2 {
                return false;
            }
            let owner = parts[parts.len() - 2];
            let repo_name = parts[parts.len() - 1];
            let short = format!("{owner}/{repo_name}");
            short.to_lowercase() == filter_lower
                || repo_name.to_lowercase() == filter_lower
                || r.id.to_lowercase().ends_with(&filter_lower)
        })
        .collect();

    match suffix_matches.len() {
        0 => bail!("no repository matching filter: {filter}"),
        1 => Ok(suffix_matches[0].clone()),
        n => {
            let ids: Vec<_> = suffix_matches.iter().map(|r| r.id.as_str()).collect();
            bail!(
                "ambiguous filter {filter:?} ({n} matches): {}",
                ids.join(", ")
            )
        }
    }
}

/// Where the user currently is: which indexed repository, and which
/// worktree (or the trunk) of that repository holds the given directory.
#[derive(Debug, Clone)]
pub struct RepoContext {
    pub repo: RepoRecord,
    /// Root of the worktree (or trunk) containing the directory.
    pub root: PathBuf,
}

/// Resolve the repository context for any directory inside a trunk or worktree.
///
/// Works from nested directories and from linked worktrees. A repository
/// that is not in the index is refused, with the way to bring it in: jeet
/// works on its store and its tasks, not on checkouts elsewhere.
pub fn resolve_context(app: &crate::context::App, dir: &Path) -> Result<RepoContext> {
    let root = match crate::git::git_toplevel(dir) {
        Ok(root) => root,
        // A marker naming a task the index no longer has is a leftover: say
        // what is true of the directory instead.
        Err(_) => match crate::tasks::task_at(app, dir).unwrap_or(None) {
            Some(task) => bail!(
                "{} is in task {}'s workspace, not in one of its worktrees; \
                 `jeet worktree ls` lists them",
                dir.display(),
                task.name
            ),
            None => bail!("not inside a git repository: {}", dir.display()),
        },
    };
    let trunk = trunk_for(&root)?;

    let Some(repo) = find_repo_by_trunk(&app.db, &trunk)? else {
        bail!(
            "{} is not in jeet's store; `jeet import {}` brings it and its work into tasks",
            trunk.display(),
            trunk.display()
        );
    };

    Ok(RepoContext { repo, root })
}

/// Resolve the repository from an explicit filter, else from the current directory.
pub fn repo_from_filter_or_cwd(
    app: &crate::context::App,
    filter: Option<&str>,
) -> Result<RepoRecord> {
    if let Some(filter) = filter {
        return resolve_repo_filter(&app.db, filter);
    }
    let cwd = std::env::current_dir()?;
    match resolve_context(app, &cwd) {
        Ok(ctx) => Ok(ctx.repo),
        Err(e) => bail!("{e}\nrun from inside a repository or pass --repo <filter>"),
    }
}

/// The main working tree for a (possibly linked) worktree root.
pub fn trunk_for(root: &Path) -> Result<PathBuf> {
    let common = crate::git::git_common_dir(root)?;
    if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        if let Some(parent) = common.parent() {
            return Ok(parent.to_path_buf());
        }
    }
    Ok(root.to_path_buf())
}

fn find_repo_by_trunk(db: &Database, trunk: &Path) -> Result<Option<RepoRecord>> {
    for repo in db.list_repos(None)? {
        if same_path(Path::new(&repo.trunk_path), trunk) {
            return Ok(Some(repo));
        }
    }
    Ok(None)
}

/// Compare two paths, resolving symlinks when the paths still exist.
pub fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
