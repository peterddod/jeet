use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct GitWorktreeEntry {
    pub path: PathBuf,
    pub _head: String,
    pub branch: Option<String>,
}

/// When set, git subprocesses capture their output instead of writing to the
/// terminal. The explorer turns this on so git never draws over the TUI.
static CAPTURE_OUTPUT: AtomicBool = AtomicBool::new(false);

pub fn set_capture_output(on: bool) {
    CAPTURE_OUTPUT.store(on, Ordering::Relaxed);
}

/// Run `git` with the given arguments, failing with git's own message.
fn run_git<I, S>(args: I, what: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.args(args);
    if CAPTURE_OUTPUT.load(Ordering::Relaxed) {
        let output = cmd.output().with_context(|| format!("spawn {what}"))?;
        if !output.status.success() {
            bail!("{what} failed: {}", first_error_line(&output.stderr));
        }
    } else {
        // git progress goes to stderr; keeping its stdout out of ours means
        // `jeet worktree` can still print a path callers can consume.
        let status = cmd
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("spawn {what}"))?;
        if !status.success() {
            bail!("{what} failed with status {status}");
        }
    }
    Ok(())
}

fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string()
}

pub fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("destination already exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create clone parent dirs")?;
    }
    run_git(["clone", url, &dest.to_string_lossy()], "git clone")
}

pub fn origin_url(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "remote",
            "get-url",
            "origin",
        ])
        .output()
        .context("spawn git remote get-url")?;
    if !output.status.success() {
        bail!("git remote get-url origin failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn default_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .context("spawn git symbolic-ref")?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout);
        if let Some(branch) = s.trim().strip_prefix("origin/") {
            return Ok(branch.to_string());
        }
    }
    Ok("main".to_string())
}

pub fn fetch(repo_path: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(
        ["-C", &repo_path.to_string_lossy(), "fetch", remote, branch],
        "git fetch",
    )
}

pub fn ref_exists(repo_path: &Path, ref_name: &str) -> Result<bool> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "show-ref",
            "--verify",
            ref_name,
        ])
        .output()
        .context("spawn git show-ref")?;
    Ok(output.status.success())
}

pub fn branch_exists(repo_path: &Path, branch: &str) -> Result<bool> {
    ref_exists(repo_path, &format!("refs/heads/{branch}"))
}

pub fn worktree_add_new_branch(
    repo_path: &Path,
    dest: &Path,
    branch: &str,
    start_point: &str,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    run_git(
        [
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            "-b",
            branch,
            &dest.to_string_lossy(),
            start_point,
        ],
        "git worktree add",
    )
}

pub fn worktree_add_existing_branch(repo_path: &Path, dest: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    run_git(
        [
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            &dest.to_string_lossy(),
            branch,
        ],
        "git worktree add",
    )
}

pub fn worktree_add_detached(repo_path: &Path, dest: &Path, start_point: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    run_git(
        [
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &dest.to_string_lossy(),
            start_point,
        ],
        "git worktree add --detach",
    )
}

pub fn status_porcelain(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "status", "--porcelain"])
        .output()
        .context("spawn git status --porcelain")?;
    if !output.status.success() {
        bail!("git status --porcelain failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn delete_local_branch(repo_path: &Path, branch: &str) -> Result<()> {
    run_git(
        ["-C", &repo_path.to_string_lossy(), "branch", "-D", branch],
        "git branch -D",
    )
}

pub fn worktree_remove(repo_path: &Path, wt_path: &Path, force: bool) -> Result<()> {
    let mut args = vec![
        "-C".to_string(),
        repo_path.to_string_lossy().to_string(),
        "worktree".to_string(),
        "remove".to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    args.push(wt_path.to_string_lossy().to_string());
    run_git(&args, "git worktree remove")
}

pub fn worktree_list(repo_path: &Path) -> Result<Vec<GitWorktreeEntry>> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()
        .context("spawn git worktree list")?;
    if !output.status.success() {
        bail!("git worktree list failed");
    }
    parse_worktree_porcelain(&String::from_utf8_lossy(&output.stdout))
}

fn parse_worktree_porcelain(text: &str) -> Result<Vec<GitWorktreeEntry>> {
    let mut entries = Vec::new();
    let mut path = None;
    let mut head = None;
    let mut branch = None;

    for line in text.lines() {
        if line.is_empty() {
            if let (Some(p), Some(h)) = (path.take(), head.take()) {
                entries.push(GitWorktreeEntry {
                    path: PathBuf::from(p),
                    _head: h,
                    branch: branch.take(),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        }
    }
    if let (Some(p), Some(h)) = (path, head) {
        entries.push(GitWorktreeEntry {
            path: PathBuf::from(p),
            _head: h,
            branch,
        });
    }
    Ok(entries)
}

pub fn git_toplevel(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .context("spawn git rev-parse")?;
    if !output.status.success() {
        bail!("not a git repository: {}", path.display());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

pub fn is_git_repo(path: &Path) -> bool {
    git_toplevel(path).is_ok()
}

/// Path to the shared git directory (for a linked worktree this is the trunk's `.git`).
pub fn git_common_dir(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .context("spawn git rev-parse --git-common-dir")?;
    if !output.status.success() {
        bail!("not a git repository: {}", path.display());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// The checked-out branch, or `None` when HEAD is detached.
pub fn head_branch(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "symbolic-ref",
            "--quiet",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

pub fn head_short_sha(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Number of entries reported by `git status --porcelain` (uncommitted work).
pub fn dirty_count(path: &Path) -> usize {
    match status_porcelain(path) {
        Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

/// `(ahead, behind)` commit counts of HEAD relative to `base`.
pub fn ahead_behind(path: &Path, base: &str) -> Option<(usize, usize)> {
    let range = format!("{base}...HEAD");
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-list",
            "--left-right",
            "--count",
            &range,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_left_right(&String::from_utf8_lossy(&output.stdout))
}

fn parse_left_right(text: &str) -> Option<(usize, usize)> {
    let mut parts = text.split_whitespace();
    let behind: usize = parts.next()?.parse().ok()?;
    let ahead: usize = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// `(files, insertions, deletions)` between the merge base with `base` and the
/// working tree (so uncommitted edits are included in the counter).
pub fn diff_stat(path: &Path, base: &str) -> Option<(usize, usize, usize)> {
    let range = format!("{base}...");
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "diff", "--numstat", &range])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_numstat(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_numstat(text: &str) -> (usize, usize, usize) {
    let mut files = 0;
    let mut insertions = 0;
    let mut deletions = 0;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(added), Some(removed)) = (parts.next(), parts.next()) else {
            continue;
        };
        files += 1;
        insertions += added.parse::<usize>().unwrap_or(0);
        deletions += removed.parse::<usize>().unwrap_or(0);
    }
    (files, insertions, deletions)
}

pub fn remote_exists(repo_path: &Path, remote: &str) -> bool {
    Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "remote",
            "get-url",
            remote,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Publish `branch` to `remote`, setting upstream tracking.
pub fn push_set_upstream(repo_path: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(
        [
            "-C",
            &repo_path.to_string_lossy(),
            "push",
            "--set-upstream",
            remote,
            branch,
        ],
        "git push",
    )
}

pub fn worktree_prune(repo_path: &Path) -> Result<()> {
    run_git(
        ["-C", &repo_path.to_string_lossy(), "worktree", "prune"],
        "git worktree prune",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_left_right_counts() {
        assert_eq!(parse_left_right("3\t5\n"), Some((5, 3)));
        assert_eq!(parse_left_right(""), None);
    }

    #[test]
    fn parses_numstat_totals() {
        let text = "10\t2\tsrc/a.rs\n0\t7\tsrc/b.rs\n-\t-\tbin\n";
        assert_eq!(parse_numstat(text), (3, 10, 9));
    }

    #[test]
    fn parses_porcelain() {
        let text = "worktree /path/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /path/feature\nHEAD def456\nbranch refs/heads/feature\n";
        let entries = parse_worktree_porcelain(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/path/feature"));
    }
}
