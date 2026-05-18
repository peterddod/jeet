use std::path::Path;

use anyhow::{Context, Result};

use crate::context::App;
use crate::db::RepoRecord;
use crate::git;
use crate::remote;

pub fn run(app: &App, path: &str) -> Result<()> {
    let path = crate::config::expand_path(path);
    let toplevel = git::git_toplevel(&path).context("adopt path")?;
    let remote_url = git::origin_url(&toplevel).context("read origin remote")?;
    let identity = remote::parse_remote_url_anyhow(&remote_url)?;
    let default_branch = git::default_branch(&toplevel)?;

    let repo = RepoRecord {
        id: identity.id(),
        trunk_path: toplevel.to_string_lossy().to_string(),
        remote_url,
        default_branch,
        managed: false,
    };
    app.db.upsert_repo(&repo)?;

    sync_worktrees(app, &repo)?;

    println!("adopted {} at {}", repo.id, repo.trunk_path);
    Ok(())
}

pub fn sync_worktrees(app: &App, repo: &RepoRecord) -> Result<()> {
    let trunk = Path::new(&repo.trunk_path);
    let entries = git::worktree_list(trunk).context("list worktrees")?;
    app.db.delete_worktrees_for_repo(&repo.id)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    for entry in entries {
        if entry.path == trunk {
            continue;
        }
        if let Some(branch) = entry.branch {
            app.db.upsert_worktree(&crate::db::WorktreeRecord {
                repo_id: repo.id.clone(),
                branch,
                path: entry.path.to_string_lossy().to_string(),
                created_at: now,
            })?;
        }
    }
    Ok(())
}
