use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::commands::adopt;
use crate::context::App;
use crate::db::WorktreeRecord;
use crate::git;
use crate::paths;
use crate::remote;
use crate::resolve;

pub fn add(app: &App, filter: &str, branch: &str) -> Result<()> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let trunk = Path::new(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path);
    }

    let identity = remote::identity_from_id(&repo.id)?;
    let dest = paths::worktree_path(&app.worktrees_root(), &identity, branch);
    if dest.exists() {
        bail!("worktree path already exists: {}", dest.display());
    }

    let start = resolve_start_point(trunk, &repo.default_branch)?;
    if git::ref_exists(
        trunk,
        &format!("refs/remotes/origin/{}", repo.default_branch),
    )? {
        let _ = git::fetch(trunk, "origin", &repo.default_branch);
    }

    if git::branch_exists(trunk, branch)? {
        git::worktree_add_existing_branch(trunk, &dest, branch)
            .context("add worktree for existing branch")?;
    } else {
        git::worktree_add_new_branch(trunk, &dest, branch, &start)
            .context("add worktree with new branch")?;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    app.db.upsert_worktree(&WorktreeRecord {
        repo_id: repo.id.clone(),
        branch: branch.to_string(),
        path: dest.to_string_lossy().to_string(),
        created_at: now,
    })?;

    println!("worktree {} -> {}", branch, dest.display());
    Ok(())
}

pub fn ls_cmd(app: &App, filter: Option<&str>) -> Result<()> {
    let repos = if let Some(f) = filter {
        vec![resolve::resolve_repo_filter(&app.db, f)?]
    } else {
        app.db.list_repos(None)?
    };

    for repo in repos {
        adopt::sync_worktrees(app, &repo)?;
        let wts = app.db.list_worktrees(&repo.id)?;
        println!("{}:", repo.id);
        if wts.is_empty() {
            println!("  (no extra worktrees; trunk at {})", repo.trunk_path);
            continue;
        }
        for wt in wts {
            println!("  {} -> {}", wt.branch, wt.path);
        }
    }
    Ok(())
}

pub fn remove(app: &App, filter: &str, branch: &str, force: bool) -> Result<()> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let trunk = Path::new(&repo.trunk_path);
    let wt = app
        .db
        .get_worktree(&repo.id, branch)?
        .ok_or_else(|| anyhow::anyhow!("no worktree indexed for branch {branch}"))?;
    let wt_path = Path::new(&wt.path);

    git::worktree_remove(trunk, wt_path, force).context("remove worktree")?;
    app.db.remove_worktree(&repo.id, branch)?;
    println!("removed worktree {branch} from {}", repo.id);
    Ok(())
}

pub fn resolve_start_point(trunk: &Path, default_branch: &str) -> Result<String> {
    let origin_ref = format!("refs/remotes/origin/{default_branch}");
    if git::ref_exists(trunk, &origin_ref)? {
        return Ok(format!("origin/{default_branch}"));
    }
    if git::branch_exists(trunk, default_branch)? {
        return Ok(default_branch.to_string());
    }
    bail!("could not find start point for new branch (tried origin/{default_branch} and local {default_branch})")
}
