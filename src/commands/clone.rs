use anyhow::{bail, Context, Result};

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
    let store = app.store_root();
    let trunk = paths::trunk_path(&store, &identity);
    // parse_remote_url refuses `..` and the like; a lexical `starts_with`
    // would pass `store/../x`, so check every component is a plain name.
    let inside = trunk.strip_prefix(store).is_ok_and(|rel| {
        rel.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    });
    if !inside {
        bail!(
            "refusing to clone {}: it would land outside the store",
            url.escape_debug()
        );
    }

    git::clone_repo(url, &trunk).context("clone repository")?;

    let remote_url = git::origin_url(&trunk).unwrap_or_else(|_| url.to_string());
    let default_branch = git::default_branch(&trunk)?;

    let repo = RepoRecord {
        id: identity.id(),
        trunk_path: trunk.to_string_lossy().to_string(),
        remote_url,
        default_branch,
    };
    app.db.upsert_repo(&repo)?;
    Ok(repo)
}
