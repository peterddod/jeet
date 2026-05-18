use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct GitWorktreeEntry {
    pub path: PathBuf,
    pub _head: String,
    pub branch: Option<String>,
}

pub fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("destination already exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create clone parent dirs")?;
    }
    let status = Command::new("git")
        .args(["clone", url, &dest.to_string_lossy()])
        .status()
        .context("spawn git clone")?;
    if !status.success() {
        bail!("git clone failed with status {status}");
    }
    Ok(())
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
    let status = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "fetch", remote, branch])
        .status()
        .context("spawn git fetch")?;
    if !status.success() {
        bail!("git fetch failed");
    }
    Ok(())
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
    let status = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            "-b",
            branch,
            &dest.to_string_lossy(),
            start_point,
        ])
        .status()
        .context("spawn git worktree add -b")?;
    if !status.success() {
        bail!("git worktree add failed (branch may already be checked out elsewhere)");
    }
    Ok(())
}

pub fn worktree_add_existing_branch(repo_path: &Path, dest: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    let status = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            &dest.to_string_lossy(),
            branch,
        ])
        .status()
        .context("spawn git worktree add")?;
    if !status.success() {
        bail!("git worktree add failed (branch may already be checked out elsewhere)");
    }
    Ok(())
}

pub fn worktree_add_detached(repo_path: &Path, dest: &Path, start_point: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    let status = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &dest.to_string_lossy(),
            start_point,
        ])
        .status()
        .context("spawn git worktree add --detach")?;
    if !status.success() {
        bail!("git worktree add --detach failed");
    }
    Ok(())
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
    let status = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "branch", "-D", branch])
        .status()
        .context("spawn git branch -D")?;
    if !status.success() {
        bail!("git branch -D {branch} failed");
    }
    Ok(())
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
    let status = Command::new("git")
        .args(&args)
        .status()
        .context("spawn git worktree remove")?;
    if !status.success() {
        bail!("git worktree remove failed");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let text = "worktree /path/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /path/feature\nHEAD def456\nbranch refs/heads/feature\n";
        let entries = parse_worktree_porcelain(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/path/feature"));
    }
}
