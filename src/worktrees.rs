//! Shared worktree services used by both the CLI and the file explorer.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::context::App;
use crate::db::RepoRecord;
use crate::git;
use crate::paths;
use crate::remote;
use crate::resolve;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// The canonical checkout in the store.
    Trunk,
    /// One of a task's worktrees, under `~/.jeet/tasks`.
    Task,
    /// A worktree git knows about outside any task: one from before tasks,
    /// or made by hand. `jeet import` brings these into tasks.
    External,
}

impl WorktreeKind {
    pub fn label(&self) -> &'static str {
        match self {
            WorktreeKind::Trunk => "trunk",
            WorktreeKind::Task => "task",
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
    // `git worktree list` reports symlink-resolved paths.
    let tasks_root = app.tasks_root();
    let tasks_root = tasks_root.canonicalize().unwrap_or(tasks_root);

    let mut entries = Vec::new();
    for entry in git::worktree_list(&trunk)? {
        let is_trunk = resolve::same_path(&entry.path, &trunk);
        let kind = if is_trunk {
            WorktreeKind::Trunk
        } else if entry.path.starts_with(&tasks_root) {
            WorktreeKind::Task
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

/// Check a pull request out into a worktree of its own, reusing the one that
/// already has its branch.
///
/// A reused worktree is brought up to date with the PR only when it is clean
/// — it may be the user's own branch with work in it, and a review is not a
/// reason to touch that.
///
/// `gh pr checkout` fetches into the trunk's repository, so it holds the
/// repo's fetch lock: a store sync fetching the same repo at the same moment
/// would otherwise fight it for ref locks, and the checkout would fail.
pub fn checkout_pr(
    app: &App,
    repo: &RepoRecord,
    pr: &crate::github::PullRequest,
) -> Result<Outcome> {
    let trunk = PathBuf::from(&repo.trunk_path);
    let branch = pr.local_branch();
    let _fetch = crate::store::lock_repo(&app.home, &repo.id)?;

    if let Some(existing) = list(app, repo)?
        .into_iter()
        .find(|e| e.branch.as_deref() == Some(branch.as_str()) && !e.missing)
    {
        let mut warnings = Vec::new();
        match git::status_counts(&existing.path) {
            Ok((0, _)) => {
                if let Err(e) = crate::github::checkout(&existing.path, pr.number, &branch) {
                    warnings.push(format!("could not update {branch} from the PR: {e}"));
                }
            }
            _ => warnings.push(format!(
                "{branch} has uncommitted changes, so it was not updated from the PR"
            )),
        }
        return Ok(Outcome {
            path: existing.path,
            warnings,
        });
    }

    let identity = remote::identity_from_id(&repo.id)?;
    let dest = paths::worktree_path(&app.worktrees_root(), &identity, &branch);
    if dest.exists() {
        bail!(
            "{} already exists but is not a worktree for {branch}",
            dest.display()
        );
    }
    // Detached first, then `gh pr checkout` inside it: gh knows where a fork's
    // branch lives and how to track it, and git will not put one branch in two
    // worktrees, so the branch cannot be created before the worktree exists.
    let start = resolve_start_point(&trunk, &repo.default_branch)?;
    git::worktree_add_detached(&trunk, &dest, &start).context("create worktree for the PR")?;
    if let Err(e) = crate::github::checkout(&dest, pr.number, &branch) {
        let _ = git::worktree_remove(&trunk, &dest, true);
        prune_empty_parents(&dest, &[app.worktrees_root()]);
        return Err(e.context(format!("check out #{} on {branch}", pr.number)));
    }
    Ok(Outcome {
        path: dest,
        warnings: Vec::new(),
    })
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
