use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::commands::path;
use crate::context::App;
use crate::git;
use crate::paths;
use crate::remote;
use crate::resolve;
use crate::worktrees::resolve_start_point;

pub struct EphemeralSession {
    pub wt_path: PathBuf,
    pub trunk: PathBuf,
    pub branch_created: Option<String>,
}

pub fn run(app: &App, filter: &str, branch: Option<&str>, ephemeral: bool) -> Result<()> {
    if !io::stdout().is_terminal() && std::env::var("JEET_EXEC_INIT").is_err() {
        bail!("jeet exec requires an interactive terminal; use `jeet path` for scripting");
    }

    if ephemeral {
        let session = create_ephemeral_worktree(app, filter, branch)?;
        let exit_code = exec_shell(&session.wt_path)?;
        cleanup_ephemeral_worktree(app, &session)?;
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    let target = path::resolve_target(app, filter, branch)?;
    let exit_code = exec_shell(&target)?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

pub fn create_ephemeral_worktree(
    app: &App,
    filter: &str,
    branch: Option<&str>,
) -> Result<EphemeralSession> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path);
    }

    let identity = remote::identity_from_id(&repo.id)?;
    let session_id = Uuid::new_v4().to_string();
    let dest = paths::ephemeral_path(&app.ephemeral_root(), &identity, &session_id);
    if dest.exists() {
        bail!("ephemeral path already exists: {}", dest.display());
    }

    let start = resolve_start_point(&trunk, &repo.default_branch)?;
    if git::ref_exists(
        &trunk,
        &format!("refs/remotes/origin/{}", repo.default_branch),
    )? {
        let _ = git::fetch(&trunk, "origin", &repo.default_branch);
    }

    let branch_created = match branch {
        None => {
            git::worktree_add_detached(&trunk, &dest, &start)
                .context("create ephemeral detached worktree")?;
            None
        }
        Some(branch) => {
            if git::branch_exists(&trunk, branch)? {
                git::worktree_add_existing_branch(&trunk, &dest, branch)
                    .context("create ephemeral worktree for existing branch")?;
                None
            } else {
                git::worktree_add_new_branch(&trunk, &dest, branch, &start)
                    .context("create ephemeral worktree with new branch")?;
                Some(branch.to_string())
            }
        }
    };

    Ok(EphemeralSession {
        wt_path: dest,
        trunk,
        branch_created,
    })
}

pub fn cleanup_ephemeral_worktree(app: &App, session: &EphemeralSession) -> Result<()> {
    let dirty = git::status_porcelain(&session.wt_path)?;
    if !dirty.trim().is_empty() {
        eprintln!("jeet: ephemeral worktree had uncommitted changes; removing anyway:");
        for line in dirty.lines().take(20) {
            eprintln!("  {line}");
        }
        if dirty.lines().count() > 20 {
            eprintln!("  ...");
        }
    }

    if session.wt_path.exists() {
        git::worktree_remove(&session.trunk, &session.wt_path, true)
            .context("remove ephemeral worktree")?;
    }

    if let Some(branch) = &session.branch_created {
        if git::branch_exists(&session.trunk, branch)? {
            let _ = git::delete_local_branch(&session.trunk, branch);
        }
    }

    crate::worktrees::prune_empty_parents(&session.wt_path, &[app.ephemeral_root()]);
    Ok(())
}

fn exec_shell(target: &Path) -> Result<i32> {
    let shell = std::env::var("JEET_SHELL")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/sh".to_string());

    eprintln!("jeet: starting subshell in {} ({shell})", target.display());
    eprintln!("jeet: type 'exit' to return to your previous directory");
    eprintln!("jeet: for native cd, run: eval \"$(jeet init-shell)\"");

    let mut cmd = Command::new(&shell);
    cmd.current_dir(target);

    if let Ok(init) = std::env::var("JEET_EXEC_INIT") {
        cmd.arg("-c").arg(init);
    }

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to exec shell {shell}: {e}"))?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo(path: &Path) {
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .status()
            .unwrap();
    }

    #[test]
    fn status_porcelain_detects_dirty() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("dirty.txt"), "x").unwrap();
        let status = git::status_porcelain(dir.path()).unwrap();
        assert!(status.contains("dirty.txt"));
    }
}
