use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct GitWorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    /// The repository's own directory, for a bare one: no working tree.
    pub bare: bool,
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
        // The explorer runs git behind a progress indicator with its output
        // captured, so a credential prompt would be invisible and would block
        // on a stdin the TUI is already reading. Fail fast with a message the
        // user can actually see instead.
        run_captured(&mut cmd, what)?;
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

/// Run a git command with no way to prompt and its output captured, failing
/// with git's own diagnostic.
fn run_captured(cmd: &mut Command, what: &str) -> Result<()> {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .stdin(Stdio::null());
    let output = cmd.output().with_context(|| format!("spawn {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", first_error_line(&output.stderr));
    }
    Ok(())
}

/// git prints progress before it fails, so prefer the diagnostic over line one.
fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or_else(|| lines.last())
        .map(|l| l.to_string())
        .unwrap_or_else(|| "no output".to_string())
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

/// origin's URL as configured. Read from the config rather than with
/// `git remote get-url`, which applies `url.<base>.insteadOf` rewrites: the
/// URL as written is what names the repository.
pub fn origin_url(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "config",
            "--get",
            "remote.origin.url",
        ])
        .output()
        .context("spawn git config")?;
    if !output.status.success() {
        bail!("no origin remote");
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
    // No `origin/HEAD` — the normal state for a repo built with `git remote add`
    // rather than cloned. The branch actually checked out is a far better guess
    // than assuming "main": guessing wrong leaves the repo unable to create any
    // worktree at all, because no start point resolves.
    if let Some(branch) = head_branch(repo_path) {
        return Ok(branch);
    }
    Ok("main".to_string())
}

/// `git fetch origin --prune`, unattended: it may run in the background, or
/// on several repos at once, where a prompt could neither be seen nor answered.
///
/// ssh asks for passphrases and host keys on the terminal itself, past
/// `GIT_TERMINAL_PROMPT`, so it is put in batch mode too — unless the user
/// already chose how git runs ssh, which is theirs to keep.
pub fn fetch_prune(repo_path: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_path);
    let ssh_chosen = std::env::var_os("GIT_SSH_COMMAND").is_some()
        || std::env::var_os("GIT_SSH").is_some()
        || config_value(repo_path, "core.sshCommand").is_some();
    if !ssh_chosen {
        cmd.args(["-c", "core.sshCommand=ssh -o BatchMode=yes"]);
    }
    cmd.args(["fetch", "--quiet", "--prune", "origin"]);
    run_captured(&mut cmd, "git fetch")
}

/// Fast-forward the checked-out branch to `rev`, refusing anything else.
pub fn merge_ff_only(repo_path: &Path, rev: &str) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo_path)
        .args(["merge", "--ff-only", "--quiet", rev]);
    run_captured(&mut cmd, "git merge --ff-only")
}

/// The commit `rev` names, in full.
pub fn rev_parse(repo_path: &Path, rev: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .output()
        .ok()?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !sha.is_empty()).then_some(sha)
}

/// The tree `rev` names, in full.
pub fn tree_of(repo_path: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{tree}}");
    git_output(
        repo_path,
        &["rev-parse", "--verify", "--quiet", &spec],
        &[],
        None,
    )
    .ok()
    .filter(|oid| !oid.is_empty())
}

/// Whether `ancestor` is reachable from `descendant`.
pub fn is_ancestor(repo_path: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn config_value(repo_path: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !value.is_empty()).then_some(value)
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

/// Relocate a linked worktree, keeping git's bookkeeping in step.
///
/// `dest` must not exist: git moves a worktree *into* an existing directory.
pub fn worktree_move(repo_path: &Path, from: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("destination already exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create worktree parent dirs")?;
    }
    run_git(
        [
            "-C",
            &repo_path.to_string_lossy(),
            "worktree",
            "move",
            &from.to_string_lossy(),
            &dest.to_string_lossy(),
        ],
        "git worktree move",
    )
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
    let mut bare = false;

    for line in text.lines() {
        if line.is_empty() {
            if let Some(p) = path.take() {
                entries.push(GitWorktreeEntry {
                    path: PathBuf::from(p),
                    head: head.take().unwrap_or_default(),
                    branch: branch.take(),
                    bare,
                });
            }
            bare = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "bare" {
            bare = true;
        }
    }
    if let Some(p) = path {
        entries.push(GitWorktreeEntry {
            path: PathBuf::from(p),
            head: head.unwrap_or_default(),
            branch,
            bare,
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

/// The checkout's own git directory: for a linked worktree, the one under
/// the repository's `worktrees/` that is its alone.
pub fn git_dir(path: &Path) -> Result<PathBuf> {
    git_output(path, &["rev-parse", "--absolute-git-dir"], &[], None).map(PathBuf::from)
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

/// `(uncommitted, ignored)` entry counts for a worktree.
///
/// `--untracked-files=normal` overrides a repo or user `status.showUntrackedFiles=no`,
/// which would otherwise hide untracked work and make a worktree look disposable.
/// Ignored files are counted separately: git will happily delete them, but they
/// are frequently the `.env` a user cannot regenerate.
pub fn status_counts(path: &Path) -> Result<(usize, usize)> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignored=matching",
        ])
        .output()
        .context("spawn git status --porcelain")?;
    if !output.status.success() {
        bail!("git status failed: {}", first_error_line(&output.stderr));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut dirty = 0;
    let mut ignored = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("!!") {
            ignored += 1;
        } else {
            dirty += 1;
        }
    }
    Ok((dirty, ignored))
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
/// working tree, so uncommitted edits count towards the diff counter.
///
/// Note this diffs against the merge base directly rather than using `base...`,
/// which is `merge-base..HEAD` and would leave uncommitted work out.
pub fn diff_stat(path: &Path, base: &str) -> Option<(usize, usize, usize)> {
    let merge_base = merge_base(path, base)?;
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "diff",
            "--numstat",
            &merge_base,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_numstat(&String::from_utf8_lossy(&output.stdout)))
}

pub fn merge_base(path: &Path, base: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "merge-base", base, "HEAD"])
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

/// One file's line counts in a diff; `None` for a binary file, which git
/// reports as `-` rather than a number.
pub type FileDiff = (PathBuf, Option<(usize, usize)>);

/// Per-file `(path, counts)` between `merge_base` and the working tree, paths
/// relative to the worktree root.
///
/// `-z` so a path git would otherwise quote (spaces, non-ASCII) comes back
/// exactly as it is on disk, and `--no-renames` so every entry is one path: a
/// rename shows up as the old file deleted and the new one added, which is
/// what the listing can put a number beside anyway.
pub fn numstat_by_file(path: &Path, merge_base: &str) -> Option<Vec<FileDiff>> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            merge_base,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_numstat_z(&output.stdout))
}

/// Parsed as bytes, not text: a filename that is not valid UTF-8 would come
/// back from a lossy decode full of U+FFFD, match nothing on disk, and its
/// changes would silently vanish from the listing.
fn parse_numstat_z(bytes: &[u8]) -> Vec<FileDiff> {
    bytes
        .split(|&b| b == 0)
        .filter_map(|record| {
            let mut parts = record.splitn(3, |&b| b == b'\t');
            let (added, deleted, file) = (parts.next()?, parts.next()?, parts.next()?);
            let number = |field: &[u8]| std::str::from_utf8(field).ok()?.parse().ok();
            let counts = number(added).zip(number(deleted));
            Some((path_from_bytes(file), counts))
        })
        .collect()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The diff tool `git difftool` would use, if the user has configured one.
pub fn configured_diff_tool(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "config", "diff.tool"])
        .output()
        .ok()?;
    let tool = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !tool.is_empty()).then_some(tool)
}

pub fn worktree_prune(repo_path: &Path) -> Result<()> {
    run_git(
        ["-C", &repo_path.to_string_lossy(), "worktree", "prune"],
        "git worktree prune",
    )
}

/// Run git in `dir` and return its stdout, failing with git's own message.
/// `env` is added to git's environment and `input`, if any, is its stdin.
fn git_output(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
    input: Option<&[u8]>,
) -> Result<String> {
    use std::io::Write as _;
    let what = format!("git {}", args.first().copied().unwrap_or(""));
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {what}"))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(bytes)
            .with_context(|| format!("write to {what}"))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", first_error_line(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// How many commits reachable from `rev` are reachable from none of
/// `exclude` (refs, or pseudo-refs such as `--remotes` and `--branches`).
/// `None` when git could not say, which callers treat as "maybe some".
pub fn count_commits_not_in(path: &Path, rev: &str, exclude: &[&str]) -> Option<usize> {
    let mut args = vec!["rev-list", "--count", rev, "--not"];
    args.extend_from_slice(exclude);
    // Revisions end here, so a ref that shares a file's name is not misread.
    args.push("--");
    let out = git_output(path, &args, &[], None).ok()?;
    out.parse().ok()
}

/// Fetch `refspecs` into the repository at `repo_path` from the one at
/// `source`, a local path: how `jeet import` copies branches and commits into
/// the store without going through a remote. Only fast-forwards, unless a
/// refspec says otherwise, and git refuses to move a branch that is checked
/// out anywhere.
pub fn fetch_local(repo_path: &Path, source: &Path, refspecs: &[&str]) -> Result<()> {
    let source = source.to_string_lossy();
    let mut args = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
    args.push(&source);
    args.extend_from_slice(refspecs);
    git_output(repo_path, &args, &[], None).map(|_| ())
}

/// Detach the worktree at `wt_path` at `rev`. A clean worktree is assumed:
/// git refuses rather than carry changes over.
pub fn checkout_detached(wt_path: &Path, rev: &str) -> Result<()> {
    git_output(
        wt_path,
        &["checkout", "--quiet", "--detach", rev, "--"],
        &[],
        None,
    )
    .map(|_| ())
}

/// Point `name` at `oid`, creating the ref if need be.
pub fn update_ref(repo_path: &Path, name: &str, oid: &str) -> Result<()> {
    git_output(repo_path, &["update-ref", name, oid], &[], None).map(|_| ())
}

pub fn delete_ref(repo_path: &Path, name: &str) -> Result<()> {
    git_output(repo_path, &["update-ref", "-d", name], &[], None).map(|_| ())
}

/// Every ref whose name starts with `prefix` (a directory such as
/// `refs/jeet/stash/<id>/`).
pub fn refs_under(repo_path: &Path, prefix: &str) -> Result<Vec<String>> {
    let out = git_output(
        repo_path,
        &["for-each-ref", "--format=%(refname)", prefix],
        &[],
        None,
    )?;
    Ok(out.lines().map(str::to_string).collect())
}

/// Apply a commit made by [`snapshot_changes`] to the worktree at `wt_path`.
/// With `index`, staged changes come back staged.
pub fn stash_apply(wt_path: &Path, rev: &str, index: bool) -> Result<()> {
    // `stash apply` reports with a full `git status` on stdout; captured, so
    // it never lands in the one line a path command prints.
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(wt_path).args(["stash", "apply"]);
    if index {
        cmd.arg("--index");
    }
    cmd.arg(rev);
    run_captured(&mut cmd, "git stash apply")
}

/// Save every uncommitted change in the worktree at `wt_path` — staged,
/// unstaged and untracked (but not ignored) files — as one commit, returning
/// it, or `None` if there is nothing to save.
///
/// The commit has the shape `git stash push --include-untracked` gives a
/// stash (HEAD, index and untracked-files parents), so `git stash apply`
/// brings it all back. Unlike `stash push` it touches nothing: not the
/// working tree, not the index, and not `refs/stash`, which every worktree of
/// the repository shares with the user.
pub fn snapshot_changes(wt_path: &Path) -> Result<Option<String>> {
    let Some(head) = rev_parse(wt_path, "HEAD") else {
        bail!("{} has no commits", wt_path.display());
    };
    let listing = git_output_raw(
        wt_path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let untracked: Vec<&[u8]> = listing
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    if let Some(nested) = untracked.iter().find(|p| p.ends_with(b"/")) {
        // git lists a repository inside the worktree as one directory and
        // cannot save its contents; it would be deleted with the worktree.
        bail!(
            "{} holds another git repository at {}; move it out first",
            wt_path.display(),
            String::from_utf8_lossy(nested)
        );
    }
    // Tracked changes, as a stash commit with HEAD and index parents; empty
    // when there are none, untracked files notwithstanding.
    let tracked = git_output(wt_path, &["stash", "create"], &[], None)?;
    let tracked = (!tracked.is_empty()).then_some(tracked);
    if untracked.is_empty() {
        return Ok(tracked);
    }

    let (tree, index_commit) = match &tracked {
        Some(stash) => {
            let index = rev_parse(wt_path, &format!("{stash}^2"))
                .with_context(|| format!("read the index of {stash}"))?;
            (format!("{stash}^{{tree}}"), index)
        }
        None => {
            let index = git_output(
                wt_path,
                &[
                    "commit-tree",
                    "--no-gpg-sign",
                    "HEAD^{tree}",
                    "-p",
                    &head,
                    "-m",
                    "jeet: index",
                ],
                &[],
                None,
            )?;
            ("HEAD^{tree}".to_string(), index)
        }
    };

    // The untracked files go in a tree of their own, built in a scratch index
    // so the worktree's real one is left alone.
    let scratch = PathBuf::from(git_output(
        wt_path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "jeet-snapshot-index",
        ],
        &[],
        None,
    )?);
    let _ = std::fs::remove_file(&scratch);
    let mut list = Vec::new();
    for path in &untracked {
        list.extend_from_slice(path);
        list.push(0);
    }
    let env = [("GIT_INDEX_FILE", scratch.as_path())];
    let built = git_output(
        wt_path,
        &["update-index", "--add", "-z", "--stdin"],
        &env,
        Some(&list),
    )
    .and_then(|_| git_output(wt_path, &["write-tree"], &env, None));
    let _ = std::fs::remove_file(&scratch);
    let untracked_tree = built?;
    let untracked_commit = git_output(
        wt_path,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &untracked_tree,
            "-m",
            "jeet: untracked files",
        ],
        &[],
        None,
    )?;
    let snapshot = git_output(
        wt_path,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            &head,
            "-p",
            &index_commit,
            "-p",
            &untracked_commit,
            "-m",
            "jeet: uncommitted changes",
        ],
        &[],
        None,
    )?;
    Ok(Some(snapshot))
}

/// [`git_output`] without decoding, for `-z` listings whose paths need not
/// be UTF-8.
fn git_output_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let what = format!("git {}", args.first().copied().unwrap_or(""));
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", first_error_line(&output.stderr));
    }
    Ok(output.stdout)
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
    fn parses_numstat_per_file() {
        let text = b"10\t2\tsrc/a.rs\x000\t7\tdir with space/b.rs\x00-\t-\tlogo.png\x00";
        assert_eq!(
            parse_numstat_z(text),
            vec![
                (PathBuf::from("src/a.rs"), Some((10, 2))),
                (PathBuf::from("dir with space/b.rs"), Some((0, 7))),
                (PathBuf::from("logo.png"), None),
            ]
        );
        assert!(parse_numstat_z(b"").is_empty());
    }

    /// A name that is not UTF-8 comes back byte for byte, so it still matches
    /// the file the explorer lists.
    #[cfg(unix)]
    #[test]
    fn numstat_paths_keep_their_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let parsed = parse_numstat_z(b"1\t0\tsrc/caf\xe9.rs\x00");
        assert_eq!(parsed[0].0.as_os_str().as_bytes(), b"src/caf\xe9.rs");
    }

    #[test]
    fn parses_porcelain() {
        let text = "worktree /path/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /path/feature\nHEAD def456\nbranch refs/heads/feature\n";
        let entries = parse_worktree_porcelain(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/path/feature"));
        assert_eq!(entries[1].head, "def456");
        assert!(!entries[0].bare);
    }

    #[test]
    fn parses_porcelain_with_bare_and_detached_entries() {
        let text = "worktree /path/repo.git\nbare\n\nworktree /path/wt\nHEAD abc123\ndetached\n\n";
        let entries = parse_worktree_porcelain(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].bare);
        assert!(entries[0].head.is_empty());
        assert!(!entries[1].bare);
        assert_eq!(entries[1].branch, None);
        assert_eq!(entries[1].head, "abc123");
    }
}
