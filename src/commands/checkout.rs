//! src/commands/checkout.rs - git-like checkout: cd into branch workspace!
//! Uses worktree add mechanism under-the-hoods to create persistent workspaces

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

use crate::context::App;
use crate::db::WorktreeRecord;
use crate::git;
use crate::paths;
use crate::remote;

/// Checkout a branch - navigate to its workspace like `git checkout`
pub fn run(
    app: &App,
    repo_filter: Option<&str>,
    branch_name: Option<&str>,
    create_branch: bool,
    start_point: Option<&str>,
) -> Result<()> {
    // Phase 1: Resolve the repository
    let repo: crate::db::RepoRecord = if let Some(filter) = repo_filter {
        // Explicit filter provided (e.g., ".", "project/", or cross-repo "user/repo")
        crate::resolve::resolve_repo_filter(&app.db, filter)
            .with_context(|| format!("invalid repo filter: {}", filter))?
    } else if branch_name.is_none() {
        // No args at all - error
        bail!("No branch specified. Use 'jeet checkout <branch>' or 'jeet checkout -b <branch>'")
    } else {
        // Auto-detect repo from current working directory
        let cwd = std::env::current_dir().context("get cwd")?;
        crate::resolve::resolve_repo_from_path(&app.db, &cwd.to_string_lossy())
            .with_context(|| "Could not auto-detect repository from current directory")?
    };

    let trunk = Path::new(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path)
    }

    // Phase 2: Resolve branch name
    let target_branch = branch_name.ok_or_else(|| {
        anyhow::anyhow!(
            "No branch specified. Use 'jeet checkout <branch>' or 'jeet checkout -b <branch>'"
        )
    })?;

    // Phase 3: Handle -b flag for creating new branch
    if create_branch {
        run_create_branch(app, &repo, trunk, target_branch, start_point)?;
    } else {
        // Phase 4: Checkout existing branch (local or remote)
        run_checkout_existing(app, &repo, trunk, target_branch)?;
    }

    Ok(())
}

/// Handle -b flag: create a new branch worktree
fn run_create_branch(
    app: &App,
    repo: &crate::db::RepoRecord,
    trunk: &Path,
    branch: &str,
    start_point: Option<&str>,
) -> Result<()> {
    // Check if local branch already exists
    if git::branch_exists(trunk, branch)? {
        bail!("Branch '{}' already exists locally; can't use -b", branch)
    }

    // Determine start point for new branch
    let effective_start = if let Some(sp) = start_point {
        sp.to_string()
    } else {
        crate::worktrees::resolve_start_point(trunk, &repo.default_branch)
            .unwrap_or_else(|_| repo.default_branch.clone())
    };

    eprintln!(
        "jeet: creating branch '{}' based on '{}'",
        branch, effective_start
    );

    // Create worktree directory and link to new branch
    let identity = remote::identity_from_id(&repo.id)?;
    let worktree_dir = paths::worktree_path(&app.worktrees_root(), &identity, branch);

    if worktree_dir.exists() {
        bail!("worktree path already exists: {}", worktree_dir.display());
    }

    eprintln!("jeet: creating workspace at {}", worktree_dir.display());
    git::worktree_add_new_branch(trunk, &worktree_dir, branch, &effective_start)?;

    // Upsert registry and change directory
    upsert_worktree_and_cd(&app.db, &repo.id, branch, &worktree_dir)?;
    Ok(())
}

/// Get list of remote branches like "origin/feature-xyz"
fn get_remote_branches(trunk: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["-C", &trunk.to_string_lossy(), "branch", "-r"])
        .output()
        .context("spawn git branch -r")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let branches: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();

    Ok(branches)
}

/// Handle checkout of existing local or remote branches
fn run_checkout_existing(
    app: &App,
    repo: &crate::db::RepoRecord,
    trunk: &Path,
    branch: &str,
) -> Result<()> {
    // Check if local branch exists
    if git::branch_exists(trunk, branch)? {
        eprintln!("jeet: checking out existing local branch '{}'", branch);
        checkout_local_branch(app, repo, trunk, branch)?;
        return Ok(());
    }

    // Check if it's a remote branch (e.g., origin/feature-xyz)
    let is_remote_branch = branch.contains('/');

    // Get list of all remote branches as fallback
    let remote_candidates = get_remote_branches(trunk)?;

    // Check if fully qualified remote branch exists
    if is_remote_branch && remote_candidates.contains(&branch.to_string()) {
        eprintln!("jeet: fetching and checking out remote branch '{}'", branch);

        // Fetch latest from upstream
        let identity = remote::identity_from_id(&repo.id)?;
        git::fetch(trunk, &identity.host, branch)?;

        let short = branch.rsplit('/').next().unwrap_or(branch);
        checkout_local_branch(app, repo, trunk, short)?;
        return Ok(());
    }

    // Check if this is a short name that matches a fully qualified remote branch
    let matching_remotes: Vec<_> = remote_candidates
        .iter()
        .filter(|r| r.ends_with(&format!("/{}", branch)))
        .collect();

    if matching_remotes.len() == 1 {
        eprintln!(
            "jeet: fetching and checking out '{}' (matched '{}')",
            branch, matching_remotes[0]
        );

        let identity = remote::identity_from_id(&repo.id)?;
        git::fetch(trunk, &identity.host, branch)?;

        checkout_local_branch(app, repo, trunk, branch)?;
        return Ok(());
    } else if matching_remotes.len() > 1 {
        eprintln!(
            "jeet: ambiguous branch '{}'. Multiple remote matches:",
            branch
        );
        for mb in matching_remotes.clone() {
            eprintln!("\t{}", mb);
        }
        bail!(
            "Ambiguous branch '{}'. Use --start-point to specify or use fully-qualified name.",
            branch
        )
    }

    // Branch doesn't exist anywhere - provide helpful error
    let help = if is_remote_branch && !remote_candidates.is_empty() {
        format!(
            "\nDid you mean one of these?\n{}",
            remote_candidates
                .iter()
                .take(10)
                .map(|r| format!("\t     {}", r))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        "\nUse 'jeet checkout -b <branch>' to create a new branch".to_string()
    };

    bail!("Branch '{}' not found locally{}", branch, help)
}

/// Checkout an existing branch and change to its worktree
fn checkout_local_branch(
    app: &App,
    repo: &crate::db::RepoRecord,
    trunk: &Path,
    branch: &str,
) -> Result<()> {
    // Check if worktree already exists in registry
    if let Some(existing) = app.db.get_worktree(&repo.id, branch)? {
        // Check if worktree directory still exists
        let wt_path = Path::new(&existing.path);
        if !wt_path.exists() {
            eprintln!("jeet: warning: worktree path missing, recreating...");
            git::worktree_add_existing_branch(trunk, wt_path, branch)?;
        }

        // Print path to stdout for shell wrapper to capture
        println!("{}", wt_path.display());
        eprintln!("jeet: using existing workspace '{}'", branch);

        // Change to existing worktree directory
        std::env::set_current_dir(wt_path).context("cd to worktree")?;
        return Ok(());
    }

    // Create new worktree for this branch
    let identity = remote::identity_from_id(&repo.id)?;
    let worktree_dir = paths::worktree_path(&app.worktrees_root(), &identity, branch);

    eprintln!("jeet: creating workspace at {}", worktree_dir.display());
    git::worktree_add_existing_branch(trunk, &worktree_dir, branch)?;

    // Upsert registry and change directory
    upsert_worktree_and_cd(&app.db, &repo.id, branch, &worktree_dir)?;

    Ok(())
}

/// Register worktree in database and change to its directory
fn upsert_worktree_and_cd(
    db: &crate::db::Database,
    repo_id: &str,
    branch: &str,
    worktree_dir: &std::path::Path,
) -> Result<()> {
    if !worktree_dir.exists() {
        bail!(
            "Worktree directory does not exist: {}",
            worktree_dir.display()
        )
    }

    // Get current timestamp as Unix epoch
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Upsert into database
    db.upsert_worktree(&WorktreeRecord {
        repo_id: repo_id.to_string(),
        branch: branch.to_string(),
        path: worktree_dir.to_string_lossy().to_string(),
        created_at: now,
    })?;

    // Print path to stdout for shell wrapper to capture (like `jeet path`)
    println!("{}", worktree_dir.display());

    // Also print status to stderr for user feedback
    eprintln!(
        "jeet: checked out '{}' at {}",
        branch,
        worktree_dir.display()
    );

    std::env::set_current_dir(worktree_dir).context("cd to worktree")?;

    Ok(())
}
