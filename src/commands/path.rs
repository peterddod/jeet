use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::context::App;
use crate::resolve;

pub fn resolve_target(app: &App, filter: &str, branch: Option<&str>) -> Result<PathBuf> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    if let Some(branch) = branch {
        if let Some(wt) = app.db.get_worktree(&repo.id, branch)? {
            return Ok(PathBuf::from(wt.path));
        }
        bail!(
            "no worktree for branch {branch} on {}; create one with: jeet worktree add {filter} {branch}",
            repo.id
        );
    }
    Ok(PathBuf::from(&repo.trunk_path))
}

pub fn run(app: &App, filter: &str, branch: Option<&str>) -> Result<()> {
    let path = resolve_target(app, filter, branch)?;
    println!("{}", path.display());
    Ok(())
}
