use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::context::App;
use crate::resolve;

/// The trunk of the repository `filter` names. Worktrees belong to tasks:
/// `jeet worktree path` prints those.
pub fn resolve_target(app: &App, filter: &str) -> Result<PathBuf> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }
    Ok(trunk)
}

pub fn run(app: &App, filter: &str) -> Result<()> {
    let path = resolve_target(app, filter)?;
    println!("{}", path.display());
    Ok(())
}
