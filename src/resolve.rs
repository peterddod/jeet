use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::db::{Database, RepoRecord};

/// Resolve a repository from an arbitrary path by detecting if it's inside a git repo
/// and matching against indexed repos. Used to detect current repo from CWD.
pub fn resolve_repo_from_path(db: &Database, dir: &str) -> Result<RepoRecord> {
    let path = Path::new(dir);
    let git_root = crate::git::git_toplevel(path)
        .map_err(|e| anyhow::anyhow!("resolve repo at {}: {}", path.display(), e))?;

    let repos = db.list_repos(None)?;
    for repo in &repos {
        if Path::new(&repo.trunk_path) == git_root {
            return Ok(repo.clone());
        }
    }

    bail!(
        "Path is in a git repo but not indexed: {}. Run 'jeet adopt {}' to register it.",
        path.display(),
        git_root.display()
    )
}

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
    /// Checked-out branch, or `None` when HEAD is detached.
    pub branch: Option<String>,
    pub is_trunk: bool,
}

impl RepoContext {
    pub fn label(&self) -> String {
        match (&self.branch, self.is_trunk) {
            (Some(b), true) => format!("{b} (trunk)"),
            (Some(b), false) => b.clone(),
            (None, _) => match crate::git::head_short_sha(&self.root) {
                Some(sha) => format!("detached @ {sha}"),
                None => "detached".to_string(),
            },
        }
    }
}

/// Resolve the repository context for any directory inside a trunk or worktree.
///
/// Unlike [`resolve_repo_from_path`], this works from nested directories and
/// from linked worktrees, indexing the trunk on demand when it is not known yet.
pub fn resolve_context(app: &crate::context::App, dir: &Path) -> Result<RepoContext> {
    let root = crate::git::git_toplevel(dir)
        .map_err(|_| anyhow::anyhow!("not inside a git repository: {}", dir.display()))?;
    let trunk = trunk_for(&root)?;

    let repo = match find_repo_by_trunk(&app.db, &trunk)? {
        Some(repo) => repo,
        None => crate::commands::adopt::adopt_path(app, &trunk.to_string_lossy()).map_err(|e| {
            anyhow::anyhow!(
                "{} is not indexed and could not be adopted automatically: {e}",
                trunk.display()
            )
        })?,
    };

    let is_trunk = same_path(&root, Path::new(&repo.trunk_path));
    Ok(RepoContext {
        branch: crate::git::head_branch(&root),
        repo,
        root,
        is_trunk,
    })
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
