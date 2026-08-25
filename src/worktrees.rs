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
///
/// Every field is only meaningful when the corresponding probe succeeded — see
/// [`WorktreeStatus::unknown`]. A failed probe must never read as "clean",
/// because that answer is what authorises deleting the worktree.
#[derive(Debug, Clone, Default)]
pub struct WorktreeStatus {
    pub dirty: usize,
    /// Files git is ignoring. git deletes these without complaint, and they are
    /// often the one thing in the tree that cannot be regenerated.
    pub ignored: usize,
    pub ahead: usize,
    pub behind: usize,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Why jeet could not assess this worktree, if it could not.
    pub unknown: Option<String>,
}

impl WorktreeStatus {
    pub fn has_work(&self) -> bool {
        self.dirty > 0 || self.ahead > 0
    }

    /// Anything at all that removing the worktree would destroy.
    pub fn has_anything_to_lose(&self) -> bool {
        self.has_work() || self.ignored > 0 || self.unknown.is_some()
    }

    /// Compact `+12/-3 in 4 files` style diff counter against the default branch.
    pub fn diff_summary(&self) -> String {
        if self.unknown.is_some() {
            return "unknown".to_string();
        }
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
///
/// Resolved once per repository rather than per worktree: it costs a git
/// subprocess and the answer is the same for every row.
pub fn comparison_base(repo: &RepoRecord) -> String {
    let trunk = Path::new(&repo.trunk_path);
    let default_branch = &repo.default_branch;
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
    status_against(entry, &comparison_base(repo))
}

/// Assess one worktree against an already-resolved comparison base.
///
/// Any probe that fails leaves `unknown` set rather than reporting zero, so a
/// git error can never masquerade as "nothing to lose".
pub fn status_against(entry: &WorktreeEntry, base: &str) -> WorktreeStatus {
    if entry.missing {
        return WorktreeStatus {
            unknown: Some("directory is missing".to_string()),
            ..WorktreeStatus::default()
        };
    }

    let (dirty, ignored) = match git::status_counts(&entry.path) {
        Ok(counts) => counts,
        Err(e) => {
            return WorktreeStatus {
                unknown: Some(format!("could not read status ({e})")),
                ..WorktreeStatus::default()
            }
        }
    };
    let Some((ahead, behind)) = git::ahead_behind(&entry.path, base) else {
        return WorktreeStatus {
            dirty,
            ignored,
            unknown: Some(format!("could not compare against {base}")),
            ..WorktreeStatus::default()
        };
    };
    let (files_changed, insertions, deletions) =
        git::diff_stat(&entry.path, base).unwrap_or((0, 0, 0));

    WorktreeStatus {
        dirty,
        ignored,
        ahead,
        behind,
        files_changed,
        insertions,
        deletions,
        unknown: None,
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
        // Branch slugs are lossy (`feat/x` and `feat-x` share a directory) and
        // a failed `worktree add` can leave a stray directory behind, so prove
        // this really is our worktree on our branch before handing it back.
        verify_worktree(&trunk, &dest, branch).with_context(|| {
            format!(
                "{} already exists but is not the worktree for {branch}",
                dest.display()
            )
        })?;
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

    let remote_ref = format!("refs/remotes/origin/{branch}");
    if git::branch_exists(&trunk, branch)? {
        git::worktree_add_existing_branch(&trunk, &dest, branch)
            .context("add worktree for existing branch")?;
    } else if git::ref_exists(&trunk, &remote_ref)? {
        // The branch already exists on the remote: check that out rather than
        // starting a fresh one from the default branch, which would otherwise
        // be pushed straight over somebody else's work.
        git::worktree_add_new_branch(&trunk, &dest, branch, &format!("origin/{branch}"))
            .context("add worktree tracking the existing remote branch")?;
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

/// Confirm `dest` is a worktree of `trunk` with `branch` checked out.
fn verify_worktree(trunk: &Path, dest: &Path, branch: &str) -> Result<()> {
    let dest_common = git::git_common_dir(dest).context("not a git worktree")?;
    let trunk_common = git::git_common_dir(trunk).context("trunk is not a git repository")?;
    if !resolve::same_path(&dest_common, &trunk_common) {
        bail!("it belongs to a different repository");
    }
    match git::head_branch(dest) {
        Some(head) if head == branch => Ok(()),
        Some(head) => bail!("it has {head} checked out"),
        None => bail!("its HEAD is detached"),
    }
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
    if push && git::ref_exists(&trunk, &format!("refs/remotes/origin/{new_name}"))? {
        bail!(
            "origin/{new_name} already exists; renaming onto it would publish this worktree over that branch. Pick another name, or pass --no-push to rename locally"
        );
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
    // Only meaningful when the branch tracked its *own* published copy — a
    // jeet-created branch that was never pushed tracks origin/<default>, and
    // telling the user to delete that would be catastrophic advice.
    if let (Some(old), Some(upstream)) = (&old_branch, &old_upstream) {
        if upstream == &format!("origin/{old}") {
            warnings.push(format!(
                "{upstream} still exists; delete it with `git push origin --delete {old}`"
            ));
        }
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
        let (dirty, _) = git::status_counts(&entry.path)?;
        if dirty > 0 {
            bail!(
                "{} has {dirty} uncommitted change{}",
                entry.display_name(),
                if dirty == 1 { "" } else { "s" }
            );
        }
    }

    if entry.missing {
        // `git worktree prune` is repo-wide: it unregisters every currently
        // missing worktree, not just this one. Only reach for it when the
        // caller has actually asked to discard, and say what it does.
        if !force {
            bail!(
                "{} is registered with git but missing on disk (pruning it unregisters every missing worktree in the repo)",
                entry.path.display()
            );
        }
        git::worktree_prune(&trunk).context("prune stale worktrees")?;
    } else {
        // Pass the caller's choice through: without --force git independently
        // re-checks for modified, untracked and submodule content at removal
        // time, which catches anything that appeared since we assessed it.
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
    /// Also consider worktrees jeet did not create — these live wherever the
    /// user put them, so removing one deletes a directory they chose.
    pub all: bool,
    /// Discard worktrees that still hold work.
    pub force: bool,
    /// Nobody is watching the report, so anything destructive must fail closed.
    pub unattended: bool,
}

#[derive(Debug, Clone)]
pub struct CleanCandidate {
    pub entry: WorktreeEntry,
    pub status: WorktreeStatus,
    pub removable: bool,
    pub reason: String,
}

/// Classify every non-trunk worktree of `repo` for cleaning.
///
/// The scope gate (which worktrees are eligible at all) is applied *before* the
/// content checks, so `--force` can never widen scope — it only decides whether
/// work already in scope may be discarded.
pub fn clean_candidates(
    app: &App,
    repo: &RepoRecord,
    opts: CleanOptions,
) -> Result<Vec<CleanCandidate>> {
    let base = comparison_base(repo);
    let mut out = Vec::new();

    for entry in list(app, repo)? {
        if entry.kind == WorktreeKind::Trunk {
            continue;
        }

        // 1. Scope: is this jeet's to remove?
        if entry.kind == WorktreeKind::External && !opts.all {
            out.push(CleanCandidate {
                status: status_against(&entry, &base),
                entry,
                removable: false,
                reason: "jeet did not create this worktree (--all to include it)".to_string(),
            });
            continue;
        }

        let status = status_against(&entry, &base);

        // 2. Content that must not be discarded without an explicit --force.
        let blocker = if entry.missing {
            Some("directory is missing; pruning it unregisters every missing worktree".to_string())
        } else if let Some(why) = status.unknown.clone() {
            Some(why)
        } else if status.dirty > 0 {
            Some(format!(
                "{} uncommitted change{}",
                status.dirty,
                if status.dirty == 1 { "" } else { "s" }
            ))
        } else if status.ahead > 0 {
            Some(format!(
                "{} commit{} not on {}",
                status.ahead,
                if status.ahead == 1 { "" } else { "s" },
                repo.default_branch
            ))
        } else {
            None
        };

        // 3. Ignored files are git's to delete, but they are also where the
        //    unrecoverable things live (.env, local databases). They do not
        //    block an interactive run — the reason line names them and the user
        //    confirms — but an unattended run must not destroy them silently.
        let ignored_note = (status.ignored > 0).then(|| {
            format!(
                "{} ignored file{} will be deleted",
                status.ignored,
                if status.ignored == 1 { "" } else { "s" }
            )
        });

        let (removable, reason) = match (&blocker, opts.force) {
            (Some(why), false) => (false, why.clone()),
            (Some(why), true) => (true, format!("forced — {why}")),
            (None, _) => match (&ignored_note, opts.force, opts.unattended) {
                (Some(note), false, true) => (false, format!("{note} (--force to discard them)")),
                (Some(note), _, _) => (true, format!("clean, but {note}")),
                (None, _, _) => (true, "nothing to lose".to_string()),
            },
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
