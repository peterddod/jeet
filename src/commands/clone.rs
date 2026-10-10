use anyhow::{Context, Result};

use crate::context::App;
use crate::db::RepoRecord;
use crate::git;
use crate::paths;
use crate::remote;

pub fn run(app: &App, url: &str) -> Result<()> {
    let repo = clone_into_store(app, url)?;
    println!("cloned {} -> {}", repo.id, repo.trunk_path);
    crate::store::maybe_sync_background(app);
    Ok(())
}

/// Clone `url` into the store and index it. git's own output goes to stderr,
/// so callers that print a path keep stdout to themselves.
pub fn clone_into_store(app: &App, url: &str) -> Result<RepoRecord> {
    let identity = remote::parse_remote_url_anyhow(url)?;
    let trunk = paths::trunk_path(&app.store_root(), &identity);

    git::clone_repo(url, &trunk).context("clone repository")?;

    let remote_url = git::origin_url(&trunk).unwrap_or_else(|_| url.to_string());
    let default_branch = git::default_branch(&trunk)?;

    let repo = RepoRecord {
        id: identity.id(),
        trunk_path: trunk.to_string_lossy().to_string(),
        remote_url,
        default_branch,
        managed: true,
    };
    app.db.upsert_repo(&repo)?;
    Ok(repo)
}
