use std::path::Path;

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
