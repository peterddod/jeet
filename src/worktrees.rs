//! Shared worktree services used by both the CLI and the file explorer.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::commands::adopt;
use crate::context::App;
use crate::db::{RepoRecord, WorktreeRecord};
use crate::git;
use crate::paths;
use crate::remote;
use crate::resolve;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// The canonical checkout in the store.
    Trunk,
    /// Created by jeet under `~/.jeet/worktrees`.
    Managed,
    /// Throwaway detached checkout under `~/.jeet/ephemeral`.
    Ephemeral,
    /// A worktree git knows about that jeet did not create.
    External,
}

impl WorktreeKind {
    pub fn label(&self) -> &'static str {
        match self {
            WorktreeKind::Trunk => "trunk",
            WorktreeKind::Managed => "worktree",
            WorktreeKind::Ephemeral => "ephemeral",
            WorktreeKind::External => "external",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub kind: WorktreeKind,
    /// Last modification time of the worktree directory (unix seconds).
    pub last_used: i64,
    /// Registered with git but no longer present on disk.
    pub missing: bool,
}

impl WorktreeEntry {
    pub fn display_name(&self) -> String {
        match &self.branch {
            Some(branch) => branch.clone(),
            None => match git::head_short_sha(&self.path) {
                Some(sha) => format!("detached @ {sha}"),
                None => "detached".to_string(),
            },
        }
    }
}

/// Uncommitted work and divergence from the repository's default branch.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorktreeStatus {
    pub dirty: usize,
    pub ahead: usize,
    pub behind: usize,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

impl WorktreeStatus {
    pub fn has_work(&self) -> bool {
        self.dirty > 0 || self.ahead > 0
    }

    /// Compact `+12/-3 in 4 files` style diff counter against the default branch.
    pub fn diff_summary(&self) -> String {
        if self.files_changed == 0 {
            return "no diff".to_string();
        }
        format!(
            "+{}/-{} in {} file{}",
            self.insertions,
            self.deletions,
            self.files_changed,
            if self.files_changed == 1 { "" } else { "s" }
        )
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn dir_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The ref new worktrees branch from, preferring the published default branch.
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

/// Ref used as the comparison base for diff counters.
pub fn comparison_base(trunk: &Path, default_branch: &str) -> String {
    if git::ref_exists(trunk, &format!("refs/remotes/origin/{default_branch}")).unwrap_or(false) {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    }
}

/// Every worktree git knows about for `repo`, trunk first, then most recent.
pub fn list(app: &App, repo: &RepoRecord) -> Result<Vec<WorktreeEntry>> {
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path);
    }
    let _ = adopt::sync_worktrees(app, repo);

    let worktrees_root = app.worktrees_root();
    let ephemeral_root = app.ephemeral_root();

    let mut entries = Vec::new();
    for entry in git::worktree_list(&trunk)? {
        let is_trunk = resolve::same_path(&entry.path, &trunk);
        let kind = if is_trunk {
            WorktreeKind::Trunk
        } else if entry.path.starts_with(&worktrees_root) {
            WorktreeKind::Managed
        } else if entry.path.starts_with(&ephemeral_root) {
            WorktreeKind::Ephemeral
        } else {
            WorktreeKind::External
        };
        let missing = !entry.path.exists();
        entries.push(WorktreeEntry {
            last_used: dir_mtime(&entry.path),
            path: entry.path,
            branch: entry.branch,
            kind,
            missing,
        });
    }

    entries.sort_by(|a, b| match (a.kind, b.kind) {
        (WorktreeKind::Trunk, WorktreeKind::Trunk) => std::cmp::Ordering::Equal,
        (WorktreeKind::Trunk, _) => std::cmp::Ordering::Less,
        (_, WorktreeKind::Trunk) => std::cmp::Ordering::Greater,
        _ => b.last_used.cmp(&a.last_used),
    });
    Ok(entries)
}

pub fn status_for(repo: &RepoRecord, entry: &WorktreeEntry) -> WorktreeStatus {
    if entry.missing {
        return WorktreeStatus::default();
    }
    let base = comparison_base(Path::new(&repo.trunk_path), &repo.default_branch);
    let (ahead, behind) = git::ahead_behind(&entry.path, &base).unwrap_or((0, 0));
    let (files_changed, insertions, deletions) =
        git::diff_stat(&entry.path, &base).unwrap_or((0, 0, 0));
    WorktreeStatus {
        dirty: git::dirty_count(&entry.path),
        ahead,
        behind,
        files_changed,
        insertions,
        deletions,
    }
}

/// The worktree a command left you with, plus anything worth telling the user
/// that is not serious enough to fail on (a failed publish, say).
#[derive(Debug, Clone)]
pub struct Outcome {
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// Create (or reuse) a worktree for `branch`, publishing the branch when asked.
pub fn create_named(app: &App, repo: &RepoRecord, branch: &str, push: bool) -> Result<Outcome> {
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path);
    }
    let identity = remote::identity_from_id(&repo.id)?;
    let dest = paths::worktree_path(&app.worktrees_root(), &identity, branch);

    if dest.exists() {
        register(app, repo, branch, &dest)?;
        return Ok(Outcome {
            path: dest,
            warnings: vec![format!("reusing the existing worktree for {branch}")],
        });
    }

    if git::ref_exists(
        &trunk,
        &format!("refs/remotes/origin/{}", repo.default_branch),
    )? {
        let _ = git::fetch(&trunk, "origin", &repo.default_branch);
    }

    let existing_branch = git::branch_exists(&trunk, branch)?;
    if existing_branch {
        git::worktree_add_existing_branch(&trunk, &dest, branch)
            .context("add worktree for existing branch")?;
    } else {
        let start = resolve_start_point(&trunk, &repo.default_branch)?;
        git::worktree_add_new_branch(&trunk, &dest, branch, &start)
            .context("add worktree with new branch")?;
    }

    register(app, repo, branch, &dest)?;

    let mut warnings = Vec::new();
    if push {
        if !git::remote_exists(&trunk, "origin") {
            warnings.push("no origin remote; branch was not published".to_string());
        } else if let Err(e) = git::push_set_upstream(&dest, "origin", branch) {
            warnings.push(format!(
                "could not publish {branch} to origin ({e}); the worktree is ready, push it yourself"
            ));
        }
    }

    Ok(Outcome {
        path: dest,
        warnings,
    })
}

/// Create a throwaway detached worktree at the repository's default branch.
pub fn create_detached(app: &App, repo: &RepoRecord) -> Result<PathBuf> {
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.exists() {
        bail!("trunk path does not exist: {}", repo.trunk_path);
    }
    let identity = remote::identity_from_id(&repo.id)?;
    let dest = paths::ephemeral_path(
        &app.ephemeral_root(),
        &identity,
        &Uuid::new_v4().to_string(),
    );
    if dest.exists() {
        bail!("ephemeral path already exists: {}", dest.display());
    }

    if git::ref_exists(
        &trunk,
        &format!("refs/remotes/origin/{}", repo.default_branch),
    )? {
        let _ = git::fetch(&trunk, "origin", &repo.default_branch);
    }
    let start = resolve_start_point(&trunk, &repo.default_branch)?;
    git::worktree_add_detached(&trunk, &dest, &start).context("create detached worktree")?;
    Ok(dest)
}

fn register(app: &App, repo: &RepoRecord, branch: &str, dest: &Path) -> Result<()> {
    app.db.upsert_worktree(&WorktreeRecord {
        repo_id: repo.id.clone(),
        branch: branch.to_string(),
        path: dest.to_string_lossy().to_string(),
        created_at: now_secs(),
    })
}

/// Give a worktree a (new) branch name, moving it to match.
///
/// A detached scratchpad gets a branch created at its current HEAD — nothing in
/// the working tree is touched, so uncommitted work survives the promotion —
/// and moves out of the ephemeral root so `clean` stops treating it as
/// disposable. A named worktree has its branch renamed instead. Worktrees jeet
/// does not manage keep their directory where the user put it.
pub fn rename(
    app: &App,
    repo: &RepoRecord,
    entry: &WorktreeEntry,
    new_name: &str,
    push: bool,
) -> Result<Outcome> {
    let trunk = PathBuf::from(&repo.trunk_path);
    let new_name = new_name.trim();

    if entry.kind == WorktreeKind::Trunk {
        bail!("refusing to rename the trunk checkout");
    }
    if entry.missing {
        bail!(
            "{} is registered with git but missing on disk",
            entry.path.display()
        );
    }
    if new_name.is_empty() {
        bail!("branch name cannot be empty");
    }
    if entry.branch.as_deref() == Some(new_name) {
        bail!("{new_name} is already the branch name");
    }
    if git::branch_exists(&trunk, new_name)? {
        bail!("branch {new_name} already exists");
    }

    let old_branch = entry.branch.clone();
    let old_upstream = old_branch
        .as_deref()
        .and_then(|b| git::upstream_of(&entry.path, b));

    // Managed and ephemeral worktrees live at a path derived from the branch,
    // so they follow the rename; anything the user placed themselves stays put.
    let identity = remote::identity_from_id(&repo.id)?;
    let dest = match entry.kind {
        WorktreeKind::External => entry.path.clone(),
        _ => paths::worktree_path(&app.worktrees_root(), &identity, new_name),
    };
    let moving = !resolve::same_path(&dest, &entry.path);
    if moving && dest.exists() {
        bail!("worktree path already exists: {}", dest.display());
    }

    match &old_branch {
        Some(_) => git::rename_current_branch(&entry.path, new_name)
            .context("rename the worktree's branch")?,
        None => git::checkout_new_branch_here(&entry.path, new_name)
            .context("create a branch for the detached worktree")?,
    }

    let mut warnings = Vec::new();
    if moving {
        git::worktree_move(&trunk, &entry.path, &dest).with_context(|| {
            format!("branch is now {new_name}, but the worktree could not be moved")
        })?;
    } else if entry.kind == WorktreeKind::External {
        warnings.push(format!(
            "left the worktree at {} (jeet did not create it)",
            entry.path.display()
        ));
    }

    if let Some(old) = &old_branch {
        let _ = app.db.remove_worktree(&repo.id, old);
    }
    register(app, repo, new_name, &dest)?;
    prune_empty_parents(&entry.path, &[app.worktrees_root(), app.ephemeral_root()]);

    if push {
        if !git::remote_exists(&trunk, "origin") {
            warnings.push("no origin remote; branch was not published".to_string());
        } else if let Err(e) = git::push_set_upstream(&dest, "origin", new_name) {
            warnings.push(format!(
                "could not publish {new_name} to origin ({e}); push it yourself when you can"
            ));
        }
    }
    if let (Some(old), Some(upstream)) = (&old_branch, &old_upstream) {
        warnings.push(format!(
            "{upstream} still exists; delete it with `git push origin --delete {old}`"
        ));
    }

    Ok(Outcome {
        path: dest,
        warnings,
    })
}

/// Remove a worktree, refusing to discard uncommitted work unless forced.
pub fn remove(app: &App, repo: &RepoRecord, entry: &WorktreeEntry, force: bool) -> Result<()> {
    let trunk = PathBuf::from(&repo.trunk_path);
    if entry.kind == WorktreeKind::Trunk {
        bail!("refusing to remove the trunk checkout");
    }

    if !force && !entry.missing {
        let dirty = git::dirty_count(&entry.path);
        if dirty > 0 {
            bail!(
                "{} has {dirty} uncommitted change{}; re-run with --force to discard",
                entry.display_name(),
                if dirty == 1 { "" } else { "s" }
            );
        }
    }

    if entry.missing {
        git::worktree_prune(&trunk).context("prune stale worktrees")?;
    } else {
        git::worktree_remove(&trunk, &entry.path, force).context("remove worktree")?;
    }

    if let Some(branch) = &entry.branch {
        let _ = app.db.remove_worktree(&repo.id, branch);
    }
    prune_empty_parents(&entry.path, &[app.worktrees_root(), app.ephemeral_root()]);
    Ok(())
}

/// Delete now-empty directories left behind under a jeet-managed root.
pub fn prune_empty_parents(path: &Path, roots: &[PathBuf]) {
    let mut current = path;
    while let Some(parent) = current.parent() {
        if roots.iter().any(|r| resolve::same_path(parent, r)) {
            break;
        }
        if !roots.iter().any(|r| parent.starts_with(r)) {
            break;
        }
        match std::fs::remove_dir(parent) {
            Ok(()) => current = parent,
            Err(_) => break,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CleanOptions {
    /// Also consider worktrees jeet did not create.
    pub all: bool,
    /// Treat worktrees that still hold work as removable.
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct CleanCandidate {
    pub entry: WorktreeEntry,
    pub status: WorktreeStatus,
    pub removable: bool,
    pub reason: String,
}

/// Classify every non-trunk worktree of `repo` for cleaning.
pub fn clean_candidates(
    app: &App,
    repo: &RepoRecord,
    opts: CleanOptions,
) -> Result<Vec<CleanCandidate>> {
    let mut out = Vec::new();
    for entry in list(app, repo)? {
        if entry.kind == WorktreeKind::Trunk {
            continue;
        }
        let status = status_for(repo, &entry);

        let (removable, reason) = if entry.missing {
            (true, "directory missing (stale git metadata)".to_string())
        } else if status.dirty > 0 && !opts.force {
            (
                false,
                format!(
                    "{} uncommitted change{}",
                    status.dirty,
                    if status.dirty == 1 { "" } else { "s" }
                ),
            )
        } else if status.ahead > 0 && !opts.force {
            (
                false,
                format!(
                    "{} commit{} not on {}",
                    status.ahead,
                    if status.ahead == 1 { "" } else { "s" },
                    repo.default_branch
                ),
            )
        } else if status.has_work() {
            (true, "forced (work will be discarded)".to_string())
        } else if entry.kind == WorktreeKind::Ephemeral {
            (true, "ephemeral checkout".to_string())
        } else if opts.all || entry.kind == WorktreeKind::Managed {
            (true, "nothing to lose".to_string())
        } else {
            (false, "external worktree (use --all)".to_string())
        };

        out.push(CleanCandidate {
            entry,
            status,
            removable,
            reason,
        });
    }
    Ok(out)
}
