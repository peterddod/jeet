//! Tasks: the unit of work.
//!
//! A task is a directory under `$JEET_HOME/tasks/<slug>` — a blank canvas the
//! agent works in — plus any number of git worktrees inside it, added and
//! removed over the task's life from any repository in the store. The index
//! records which worktrees a task holds; [`reconcile`] keeps that record
//! honest when worktrees come and go behind jeet's back.
//!
//! Archiving frees the disk but keeps what is needed to bring every worktree
//! back as it was: its HEAD, held by `refs/jeet/head/<task-id>/<dir>` so it
//! survives gc, and its uncommitted changes, saved as a stash-shaped commit
//! under `refs/jeet/stash/<task-id>/<dir>`. Whatever unarchive cannot put
//! back is moved to `refs/jeet/kept/<task-id>/…`, which nothing reuses, and
//! stays there until the task is deleted. The task directory itself stays,
//! so the agent's sessions, which Claude keys by working directory, still
//! resolve when the task comes back.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord, TaskState, TaskWorktreeRecord};
use crate::db::RepoRecord;
use crate::git;
use crate::projects::{self, Project};
use crate::remote;
use crate::resolve;
use crate::runtime;
use crate::store;
use crate::worktrees::{self, now_secs};

/// Set in the agent's environment so `jeet worktree` knows its task without
/// having to be run from inside the task directory.
pub const TASK_ID_ENV: &str = "JEET_TASK_ID";

/// `<task>/.jeet/task-id` marks a task directory, so the task can be found
/// from anywhere beneath it.
pub const MARKER_DIR: &str = ".jeet";
const MARKER_FILE: &str = "task-id";

const SLUG_MAX: usize = 48;

/// The built-in skill every task gets, at `.claude/skills/jeet/SKILL.md`: how
/// the agent gets at code from inside a blank-canvas task.
const SKILL: &str = r#"---
name: jeet
description: How to work in this jeet task workspace - check repositories out with `jeet worktree add`, list and remove worktrees, and open pull requests. Use whenever you need code from a repository.
---

# Working in a jeet task

This directory is your task workspace. It starts as a blank canvas: keep notes
and scratch files here freely. Repositories are checked out on demand, as git
worktrees inside this directory.

- Get a repository: `cd "$(jeet worktree add <repo>)"`. It prints the new
  worktree's path and nothing else. `<repo>` is a name such as `api`, an
  `owner/repo`, or a clone URL; a repository jeet does not have yet is cloned
  first. The worktree starts detached at the freshly fetched default branch.
  - `--branch <name>` checks that branch out instead: an existing branch is
    reused, otherwise it is created from the default branch.
  - `--name <dir>` picks the directory name (default: the repository name).
  - Add as many as you need, including several of the same repository.
- List the repositories jeet knows: `jeet ls`.
- List this task's worktrees: `jeet worktree ls`.
- Remove one you are done with: `jeet worktree rm <name>`. It refuses while the
  worktree holds uncommitted or unpushed work.
- Open pull requests from inside a worktree with `gh pr create`.
- Never touch `$JEET_HOME/store` (the canonical clones) directly. Prefer
  `jeet worktree add` over running `git worktree add` yourself.
"#;

pub fn task_dir(app: &App, task: &TaskRecord) -> PathBuf {
    app.tasks_root().join(&task.slug)
}

pub fn worktree_dir(app: &App, task: &TaskRecord, dir_name: &str) -> PathBuf {
    task_dir(app, task).join(dir_name)
}

/// A directory name from a task name: lowercase ASCII letters and digits,
/// with runs of anything else collapsed to a single `-`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= SLUG_MAX {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug.to_string()
    }
}

/// `base`, or `base-2`, `base-3`… — the first that neither another task nor
/// a stray directory already has.
fn unique_slug(app: &App, base: &str) -> Result<String> {
    let root = app.tasks_root();
    let mut n = 1;
    loop {
        let candidate = numbered(base, n);
        if app.db.get_task_by_slug(&candidate)?.is_none() && !root.join(&candidate).exists() {
            return Ok(candidate);
        }
        n += 1;
    }
}

fn numbered(base: &str, n: usize) -> String {
    match n {
        1 => base.to_string(),
        n => format!("{base}-{n}"),
    }
}

/// What `jeet task new` asks for.
pub struct NewTask<'a> {
    pub name: &'a str,
    /// Project to start from: its files are copied in, and a worktree of each
    /// of its repositories is added.
    pub project: Option<&'a str>,
    /// Repositories to add a worktree of straight away, after the project's.
    /// None at all, and no project, makes an empty canvas.
    pub repos: &'a [String],
    /// Branch for the worktree of the single repository in `repos`; the
    /// project's worktrees start detached.
    pub branch: Option<&'a str>,
}

/// A task that was just made, plus anything worth telling the user.
pub struct Created {
    pub task: TaskRecord,
    pub warnings: Vec<String>,
}

/// Make a task: its directory, marker and skill, the project's files, and a
/// worktree of each preset repository — the project's first, then `repos`.
///
/// If a preset worktree cannot be added the whole task is undone — it was
/// made a moment ago and holds nothing of anyone's yet.
pub fn create(app: &App, spec: &NewTask) -> Result<Created> {
    let name = spec.name.trim();
    if name.is_empty() {
        bail!("a task needs a name");
    }
    if spec.branch.is_some() && spec.repos.len() != 1 {
        bail!("--branch needs exactly one --repo: it names that repository's branch");
    }
    // Read up front, so a missing or broken project makes nothing at all.
    let project = match spec.project {
        Some(name) => Some(projects::load(app, name)?),
        None => None,
    };

    let (task, mut warnings) = make_task(app, name, project.as_ref(), TaskKind::Work)?;

    let preset = project
        .iter()
        .flat_map(|p| &p.config.repos)
        .map(|r| (r, None));
    let given = spec.repos.iter().map(|r| (r, spec.branch));
    for (repo, branch) in preset.chain(given) {
        match add_worktree(app, &task, repo, branch, None) {
            Ok(added) => warnings.extend(added.warnings),
            Err(e) => {
                let e = e.context(format!("could not add {repo} to the new task"));
                return Err(match delete(app, &task) {
                    Ok(_) => e,
                    Err(undo) => e.context(format!(
                        "and the half-made task could not be removed ({undo:#}); \
                         `jeet task delete {} --force` cleans it up",
                        task.slug
                    )),
                });
            }
        }
    }
    Ok(Created { task, warnings })
}

/// A task with no worktrees yet: its row, its directory, marker and skill,
/// and the project's files. Returns notes on project files not copied.
fn make_task(
    app: &App,
    name: &str,
    project: Option<&Project>,
    kind: TaskKind,
) -> Result<(TaskRecord, Vec<String>)> {
    let now = now_secs();
    let task = TaskRecord {
        id: Uuid::new_v4().to_string(),
        slug: unique_slug(app, &slugify(name))?,
        name: name.to_string(),
        project: project.map(|p| p.name.clone()),
        kind,
        state: TaskState::Active,
        created_at: now,
        last_activity: now,
    };
    let dir = task_dir(app, &task);
    std::fs::create_dir_all(app.tasks_root()).context("create the tasks directory")?;
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    match scaffold(&dir, &task, project).and_then(|notes| app.db.insert_task(&task).map(|_| notes))
    {
        Ok(notes) => Ok((task, notes)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// The marker, the project's files and the jeet skill. Returns notes on
/// project files that were not copied.
fn scaffold(dir: &Path, task: &TaskRecord, project: Option<&Project>) -> Result<Vec<String>> {
    let marker = dir.join(MARKER_DIR);
    std::fs::create_dir_all(&marker).with_context(|| format!("create {}", marker.display()))?;
    std::fs::write(marker.join(MARKER_FILE), format!("{}\n", task.id))
        .context("write the task marker")?;
    let notes = match project {
        Some(project) => projects::copy_files(project, dir, false)?
            .notes()
            .into_iter()
            .map(|note| format!("project {}: {note}", project.name))
            .collect(),
        None => Vec::new(),
    };
    // Last, so nothing of the project's can shadow it.
    install_skill(dir)?;
    Ok(notes)
}

/// Write the built-in jeet skill into a task directory.
pub fn install_skill(task_dir: &Path) -> Result<()> {
    let dir = task_dir.join(".claude").join("skills").join("jeet");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("SKILL.md"), SKILL).context("write the jeet skill")
}

/// The task a user means by `key`: its slug, its id, its name (any case), or
/// the start of exactly one slug.
pub fn find(app: &App, key: &str) -> Result<TaskRecord> {
    find_by(app, key, true)
}

/// Like [`find`], but never by a slug prefix: for deleting, where a name
/// that no longer matches its task exactly must not pick another one.
pub fn find_exact(app: &App, key: &str) -> Result<TaskRecord> {
    find_by(app, key, false)
}

fn find_by(app: &App, key: &str, by_prefix: bool) -> Result<TaskRecord> {
    let key = key.trim();
    if let Some(task) = app.db.get_task_by_slug(key)? {
        return Ok(task);
    }
    if let Some(task) = app.db.get_task(key)? {
        return Ok(task);
    }
    let tasks = app.db.list_tasks(None)?;
    let lower = key.to_lowercase();
    let mut matches: Vec<&TaskRecord> = tasks
        .iter()
        .filter(|t| t.name.to_lowercase() == lower)
        .collect();
    if matches.is_empty() && !key.is_empty() {
        let prefixed: Vec<&TaskRecord> = tasks
            .iter()
            .filter(|t| t.slug.starts_with(&lower))
            .collect();
        if !by_prefix && !prefixed.is_empty() {
            let slugs: Vec<&str> = prefixed.iter().map(|t| t.slug.as_str()).collect();
            bail!(
                "no task is named exactly {key:?} (did you mean {}?)",
                slugs.join(", ")
            );
        }
        matches = prefixed;
    }
    match matches.as_slice() {
        [] => bail!("no task named {key:?}; `jeet task ls` lists them"),
        [task] => Ok((*task).clone()),
        many => {
            let slugs: Vec<&str> = many.iter().map(|t| t.slug.as_str()).collect();
            bail!("{key:?} could be any of: {}", slugs.join(", "))
        }
    }
}

/// The task a command run here is about: the one named, else the one in
/// `$JEET_TASK_ID` (set inside a task's agent session), else the task whose
/// directory holds the current directory.
pub fn current(app: &App, explicit: Option<&str>) -> Result<TaskRecord> {
    if let Some(key) = explicit {
        return find(app, key);
    }
    if let Some(id) = std::env::var(TASK_ID_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        let id = id.trim();
        return app.db.get_task(id)?.ok_or_else(|| {
            anyhow::anyhow!("${TASK_ID_ENV} is {id}, which is no task jeet knows; was it deleted?")
        });
    }
    let cwd = std::env::current_dir().context("get cwd")?;
    match task_at(app, &cwd)? {
        Some(task) => Ok(task),
        None => bail!(
            "not inside a task: run this from a task's directory, or pick one with --task <name> \
             (`jeet task ls` lists them, `jeet task new <name>` makes one)"
        ),
    }
}

/// The task whose directory holds `dir`, if any.
pub fn task_at(app: &App, dir: &Path) -> Result<Option<TaskRecord>> {
    let Some((_, id)) = find_task_root(dir) else {
        return Ok(None);
    };
    match app.db.get_task(&id)? {
        Some(task) => Ok(Some(task)),
        None => bail!(
            "{} is marked as task {id}, which is no task jeet knows",
            dir.display()
        ),
    }
}

/// Walk up from `dir` to the nearest task directory: its path and task id.
pub fn find_task_root(dir: &Path) -> Option<(PathBuf, String)> {
    for candidate in dir.ancestors() {
        let marker = candidate.join(MARKER_DIR).join(MARKER_FILE);
        if !marker.is_file() {
            continue;
        }
        let id = std::fs::read_to_string(&marker).ok()?.trim().to_string();
        if id.is_empty() {
            return None;
        }
        return Some((candidate.to_path_buf(), id));
    }
    None
}

/// `3 wt: api, web` — how many worktrees a task holds and of which repos.
pub fn worktree_summary(rows: &[TaskWorktreeRecord]) -> String {
    if rows.is_empty() {
        return "empty".to_string();
    }
    let mut repos: Vec<&str> = Vec::new();
    for row in rows {
        let name = row.repo_id.rsplit('/').next().unwrap_or(&row.repo_id);
        if !repos.contains(&name) {
            repos.push(name);
        }
    }
    format!("{} wt: {}", rows.len(), repos.join(", "))
}

/// A worktree just added to a task.
pub struct AddedWorktree {
    pub record: TaskWorktreeRecord,
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// Add a worktree of `repo_spec` to `task` — the one way worktrees are made,
/// whether the CLI, the UI or the agent asks.
///
/// The repository is found in the store (cloned into it first if need be)
/// and fetched, so the worktree starts from what origin has now. Without a
/// branch it is detached at `origin/<default>`. With one, an existing local
/// branch is checked out, one that exists only on origin is tracked, and
/// anything else is created from `origin/<default>`.
pub fn add_worktree(
    app: &App,
    task: &TaskRecord,
    repo_spec: &str,
    branch: Option<&str>,
    name: Option<&str>,
) -> Result<AddedWorktree> {
    if task.state == TaskState::Archived {
        bail!(
            "task {} is archived; `jeet task unarchive {}` first",
            task.name,
            task.slug
        );
    }
    let dir = task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let branch = match branch.map(str::trim) {
        Some("") => bail!("branch name cannot be empty"),
        other => other,
    };
    if let Some(name) = name {
        validate_dir_name(name)?;
    }

    let repo = resolve_repo(app, repo_spec)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }

    let mut warnings = Vec::new();
    // Synchronously, unlike the background sync: the point is a fresh base.
    // Offline is no reason to refuse; the store's last fetch will do.
    if let Err(e) = store::sync_repo(app, &repo) {
        warnings.push(format!(
            "could not fetch {} ({e:#}); starting from the last fetch",
            repo.id
        ));
    }

    let rows = app.db.list_task_worktrees(&task.id)?;
    let taken: Vec<String> = rows.iter().map(|r| r.dir_name.clone()).collect();
    let dir_name = match name {
        Some(name) => {
            if taken.iter().any(|t| t == name) || dir.join(name).exists() {
                bail!("{name} already exists in task {}", task.name);
            }
            name.to_string()
        }
        None => pick_dir_name(&dir, &taken, &repo_short_name(&repo)),
    };
    let dest = dir.join(&dir_name);
    let start = worktrees::resolve_start_point(&trunk, &repo.default_branch)?;

    let created_branch = match branch {
        None => {
            git::worktree_add_detached(&trunk, &dest, &start).context("add a detached worktree")?;
            false
        }
        Some(branch) => {
            let created = check_out_branch(app, &trunk, &dest, branch, &start)?;
            for other in app.db.task_worktrees_on_branch(&repo.id, branch)? {
                if other.task_id == task.id {
                    continue;
                }
                let holder = app.db.get_task(&other.task_id)?;
                if let Some(holder) = holder.filter(|t| t.state == TaskState::Archived) {
                    warnings.push(format!(
                        "archived task {} also uses {branch}; it cannot be unarchived while this \
                         worktree has it",
                        holder.name
                    ));
                }
            }
            created
        }
    };

    let mut record = TaskWorktreeRecord {
        id: 0,
        task_id: task.id.clone(),
        repo_id: repo.id.clone(),
        dir_name,
        branch: branch.map(str::to_string),
        created_branch,
        head_oid: None,
        stash_ref: None,
        pr_number: None,
        created_at: now_secs(),
    };
    match app.db.insert_task_worktree(&record) {
        Ok(id) => record.id = id,
        Err(e) => {
            let _ = git::worktree_remove(&trunk, &dest, true);
            if let (true, Some(branch)) = (created_branch, branch) {
                let _ = git::delete_local_branch(&trunk, branch);
            }
            return Err(e.context("record the worktree"));
        }
    }
    let _ = app.db.touch_task(&task.id, now_secs());
    Ok(AddedWorktree {
        record,
        path: dest,
        warnings,
    })
}

/// `review-<repo>-<n>`: what `jeet review` calls the task for a PR.
pub fn review_task_name(repo: &RepoRecord, number: u64) -> String {
    format!("review-{}-{number}", repo_short_name(repo))
}

/// A PR's review task, ready to review in.
pub struct ReviewCheckout {
    pub task: TaskRecord,
    /// The worktree with the PR checked out.
    pub path: PathBuf,
    /// The task was made just now.
    pub created: bool,
    pub warnings: Vec<String>,
}

/// Find or make the review task for `pr` and check the PR out in it.
///
/// The task is found by the PR it reviews, not by name, so renaming nothing
/// and a second repository with the same short name cannot cross wires. An
/// archived one is brought back; one whose checkout is there already is
/// brought up to date with the PR when clean. A task made here for a PR
/// that cannot be checked out is undone — it holds nothing yet.
pub fn review_task(
    app: &App,
    repo: &RepoRecord,
    pr: &crate::github::PullRequest,
) -> Result<ReviewCheckout> {
    let mut warnings = Vec::new();
    let (mut task, created) = match app.db.find_review_task(&repo.id, pr.number)? {
        Some(task) => (task, false),
        None => {
            let name = review_task_name(repo, pr.number);
            let (task, notes) = make_task(app, &name, None, TaskKind::Review)?;
            warnings.extend(notes);
            (task, true)
        }
    };
    if task.state == TaskState::Archived {
        warnings.extend(unarchive(app, &task)?);
        task.state = TaskState::Active;
    }

    match review_worktree(app, &task, repo, pr) {
        Ok((path, more)) => {
            warnings.extend(more);
            let _ = app.db.touch_task(&task.id, now_secs());
            Ok(ReviewCheckout {
                task,
                path,
                created,
                warnings,
            })
        }
        Err(e) if created => Err(match delete(app, &task) {
            Ok(_) => e,
            Err(undo) => e.context(format!(
                "and the half-made task could not be removed ({undo:#}); \
                 `jeet task delete {} --force` cleans it up",
                task.slug
            )),
        }),
        Err(e) => Err(e),
    }
}

/// The review task's checkout of `pr`: the one it has, refreshed, or a new
/// one. The PR's branch is used unless another worktree has it — the
/// author's own task, say — and then the checkout is detached at the PR's
/// head instead, which reviews the same code without taking the branch.
fn review_worktree(
    app: &App,
    task: &TaskRecord,
    repo: &RepoRecord,
    pr: &crate::github::PullRequest,
) -> Result<(PathBuf, Vec<String>)> {
    let dir = task_dir(app, task);
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }
    // `gh pr checkout` fetches into the trunk: keep a store sync out of its way.
    let _fetch = store::lock_repo(&app.home, &repo.id)?;

    let rows = app.db.list_task_worktrees(&task.id)?;
    let existing = rows
        .iter()
        .find(|r| r.repo_id == repo.id && r.pr_number == Some(pr.number));
    if let Some(row) = existing {
        let path = dir.join(&row.dir_name);
        if path.is_dir() {
            let warnings = worktrees::refresh_pr_worktree(&path, pr, row.branch.as_deref());
            return Ok((path, warnings));
        }
        // Gone from under jeet: forget it, and have git forget it too, or
        // its branch would still count as checked out there.
        app.db.delete_task_worktree(&task.id, &row.dir_name)?;
        let _ = git::worktree_prune(&trunk);
    }

    let mut warnings = Vec::new();
    let branch = pr.local_branch();
    let use_branch = match branch_holder(app, &trunk, &branch)? {
        Some(holder) => {
            warnings.push(format!(
                "{branch} is checked out {holder}, so the review is detached at the PR's head"
            ));
            None
        }
        None => Some(branch.as_str()),
    };
    let existed = git::branch_exists(&trunk, &branch)?;
    let taken: Vec<String> = rows
        .iter()
        .filter(|r| !matches!(existing, Some(gone) if gone.id == r.id))
        .map(|r| r.dir_name.clone())
        .collect();
    let dir_name = pick_dir_name(&dir, &taken, &repo_short_name(repo));
    let dest = dir.join(&dir_name);
    worktrees::add_pr_worktree(&trunk, &repo.default_branch, &dest, pr, use_branch)?;

    // A branch made for a checkout that vanished is still the task's own.
    let made_before = existing
        .is_some_and(|gone| gone.created_branch && gone.branch.as_deref() == Some(branch.as_str()));
    let created_branch = use_branch.is_some() && (!existed || made_before);
    let record = TaskWorktreeRecord {
        id: 0,
        task_id: task.id.clone(),
        repo_id: repo.id.clone(),
        dir_name,
        branch: use_branch.map(str::to_string),
        created_branch,
        head_oid: None,
        stash_ref: None,
        pr_number: Some(pr.number),
        created_at: now_secs(),
    };
    if let Err(e) = app.db.insert_task_worktree(&record) {
        let _ = git::worktree_remove(&trunk, &dest, true);
        if created_branch {
            let _ = git::delete_local_branch(&trunk, &branch);
        }
        return Err(e.context("record the worktree"));
    }
    Ok((dest, warnings))
}

/// Check `branch` out at `dest`, returning whether this made the branch.
fn check_out_branch(
    app: &App,
    trunk: &Path,
    dest: &Path,
    branch: &str,
    start: &str,
) -> Result<bool> {
    if git::branch_exists(trunk, branch)? {
        // git refuses this too, but cannot say which task holds the branch.
        if let Some(holder) = branch_holder(app, trunk, branch)? {
            bail!("branch {branch} is already checked out {holder}");
        }
        git::worktree_add_existing_branch(trunk, dest, branch)
            .with_context(|| format!("check out {branch}"))?;
        return Ok(false);
    }
    if git::ref_exists(trunk, &format!("refs/remotes/origin/{branch}"))? {
        // Track the published branch rather than starting a new one from the
        // default branch, which would be pushed straight over it.
        git::worktree_add_new_branch(trunk, dest, branch, &format!("origin/{branch}"))
            .with_context(|| format!("check out origin/{branch}"))?;
        return Ok(true);
    }
    git::worktree_add_new_branch(trunk, dest, branch, start)
        .with_context(|| format!("create {branch}"))?;
    Ok(true)
}

/// Where `branch` is checked out already, phrased for an error: in which
/// task, or at which path.
fn branch_holder(app: &App, trunk: &Path, branch: &str) -> Result<Option<String>> {
    for entry in git::worktree_list(trunk)? {
        if entry.branch.as_deref() != Some(branch) {
            continue;
        }
        return Ok(Some(match task_owning(app, &entry.path)? {
            Some(task) => format!("in task {} ({})", task.name, entry.path.display()),
            None => format!("at {}", entry.path.display()),
        }));
    }
    Ok(None)
}

/// The task whose directory a worktree path is in, judged by path alone.
fn task_owning(app: &App, path: &Path) -> Result<Option<TaskRecord>> {
    let root = canonical_tasks_root(app);
    let Ok(rel) = path.strip_prefix(&root) else {
        return Ok(None);
    };
    match rel.components().next() {
        Some(slug) => app.db.get_task_by_slug(&slug.as_os_str().to_string_lossy()),
        None => Ok(None),
    }
}

/// `git worktree list` reports symlink-resolved paths; compare against a
/// root resolved the same way.
fn canonical_tasks_root(app: &App) -> PathBuf {
    let root = app.tasks_root();
    root.canonicalize().unwrap_or(root)
}

/// The repository `spec` names: a URL (cloned into the store if it is not
/// there yet), an id, or a filter such as `api` or `acme/api`. A full id
/// that is not indexed is cloned from `https://<id>.git`.
fn resolve_repo(app: &App, spec: &str) -> Result<RepoRecord> {
    let spec = spec.trim();
    if spec.contains("://") || spec.starts_with("git@") {
        let identity = remote::parse_remote_url_anyhow(spec)?;
        if let Some(repo) = app.db.get_repo(&identity.id())? {
            return Ok(repo);
        }
        return crate::commands::clone::clone_into_store(app, spec);
    }
    match remote::identity_from_id(spec) {
        Ok(id) if id.host.contains('.') => match app.db.get_repo(spec)? {
            Some(repo) => Ok(repo),
            None => crate::commands::clone::clone_into_store(app, &format!("https://{spec}.git")),
        },
        _ => resolve::resolve_repo_filter(&app.db, spec),
    }
}

pub fn repo_short_name(repo: &RepoRecord) -> String {
    repo.id.rsplit('/').next().unwrap_or(&repo.id).to_string()
}

/// A worktree directory name given by hand: one plain path component that
/// is also safe inside a ref name.
fn validate_dir_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "{name:?} is not a usable worktree name: use letters, digits, `-`, `_` and `.`, \
             not starting with `.`"
        );
    }
    Ok(())
}

/// `base`, then `base-2`, `base-3`… — the first name neither a worktree of
/// the task nor anything else in its directory has.
pub fn pick_dir_name(task_dir: &Path, taken: &[String], base: &str) -> String {
    let base = match base.trim_start_matches('.') {
        "" => "repo",
        trimmed => trimmed,
    };
    let mut n = 1;
    loop {
        let candidate = numbered(base, n);
        if !taken.contains(&candidate) && !task_dir.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// `refs/jeet/<kind>/<task-id>/<dir>`, with `dir` escaped into one ref
/// component by [`ref_component`], so no two worktrees share a ref.
fn jeet_ref(kind: &str, task_id: &str, dir_name: &str) -> String {
    format!("refs/jeet/{kind}/{task_id}/{}", ref_component(dir_name))
}

/// `name` as a single ref component, one-to-one: ASCII letters, digits and
/// `-` stand for themselves, and every other byte becomes `_XX` (hex). So
/// `docs.site` and `docs-site` stay apart, a `/` cannot nest one worktree's
/// refs under another's, and no name can make an invalid ref.
fn ref_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => out.push(char::from(b)),
            _ => out.push_str(&format!("_{b:02X}")),
        }
    }
    out
}

/// Set `oid` aside under `refs/jeet/kept/<task-id>/<dir>/<kind>-<n>`: names
/// neither archive nor unarchive ever reuse, for work jeet could not put
/// back. It stays until the user deals with it or deletes the task, and
/// [`losses`] lists it until then.
pub fn keep(trunk: &Path, task_id: &str, dir_name: &str, kind: &str, oid: &str) -> Result<String> {
    let base = jeet_ref("kept", task_id, dir_name);
    let mut n = 1;
    loop {
        let name = format!("{base}/{kind}-{n}");
        if !git::ref_exists(trunk, &name)? {
            git::update_ref(trunk, &name, oid).with_context(|| format!("create {name}"))?;
            return Ok(name);
        }
        if git::rev_parse(trunk, &name).as_deref() == Some(oid) {
            return Ok(name);
        }
        n += 1;
    }
}

/// Every ref set aside by [`keep`] for `task`, in any repository: a worktree
/// may have been removed from the task since, taking its row with it.
fn kept_refs(app: &App, task: &TaskRecord) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    for repo in app.db.list_repos(None)? {
        let trunk = PathBuf::from(&repo.trunk_path);
        if !trunk.is_dir() {
            continue;
        }
        let Ok(names) = git::refs_under(&trunk, &format!("refs/jeet/kept/{}/", task.id)) else {
            continue;
        };
        for name in names {
            out.push((trunk.clone(), name));
        }
    }
    Ok(out)
}

/// Whether two snapshots hold the same files: tracked and untracked alike.
/// What they were taken on top of does not matter; the content does.
fn same_changes(trunk: &Path, a: &str, b: &str) -> bool {
    let trees = |c: &str| {
        (
            git::tree_of(trunk, c),
            git::tree_of(trunk, &format!("{c}^3")),
        )
    };
    let first = trees(a);
    first.0.is_some() && first == trees(b)
}

/// Before archive reuses a worktree's `refs/jeet/{head,stash}` names, set
/// aside whatever they hold that the worktree does not: left by an unarchive
/// that stopped part-way, they may be the only copy of someone's work. A
/// pinned HEAD the live one contains, or a snapshot of exactly what is in
/// the worktree now (an earlier archive attempt's), is not worth keeping.
fn set_aside_leftovers(
    trunk: &Path,
    task_id: &str,
    dir_name: &str,
    head: &str,
    snapshot: Option<&str>,
) -> Result<Vec<String>> {
    let mut kept = Vec::new();
    if let Some(old) = git::rev_parse(trunk, &jeet_ref("head", task_id, dir_name)) {
        if old != head
            && !git::is_ancestor(trunk, &old, head)
            && unreachable(trunk, &old) != Some(0)
        {
            kept.push(keep(trunk, task_id, dir_name, "head", &old)?);
        }
    }
    if let Some(old) = git::rev_parse(trunk, &jeet_ref("stash", task_id, dir_name)) {
        if !snapshot.is_some_and(|new| same_changes(trunk, &old, new)) {
            kept.push(keep(trunk, task_id, dir_name, "stash", &old)?);
        }
    }
    Ok(kept)
}

/// Remove a worktree from a task.
///
/// Refuses, unless forced, while it has uncommitted changes or commits that
/// are not pushed. A branch the task made is deleted with it only when
/// nothing would be lost: its commits are on origin or on the default branch.
pub fn remove_worktree(
    app: &App,
    task: &TaskRecord,
    name: &str,
    force: bool,
) -> Result<Vec<String>> {
    if task.state == TaskState::Archived {
        bail!(
            "task {} is archived; `jeet task unarchive {}` first",
            task.name,
            task.slug
        );
    }
    let mut warnings = Vec::new();
    if let Err(e) = reconcile(app) {
        warnings.push(format!("could not reconcile worktrees ({e:#})"));
    }
    let Some(row) = app.db.get_task_worktree(&task.id, name)? else {
        bail!(
            "task {} has no worktree named {name}; `jeet worktree ls` lists them",
            task.name
        );
    };
    let repo = repo_of(app, &row)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    let path = worktree_dir(app, task, &row.dir_name);

    if path.exists() {
        if !force {
            let blockers = removal_blockers(&repo, &path);
            if !blockers.is_empty() {
                bail!(
                    "{name} has {}; pass --force to remove it anyway",
                    blockers.join(" and ")
                );
            }
        }
        // Pass the caller's choice through: without --force git re-checks for
        // modified and untracked files itself, catching anything new since.
        git::worktree_remove(&trunk, &path, force).context("remove the worktree")?;
    }
    let _ = git::worktree_prune(&trunk);

    if let (true, Some(branch)) = (row.created_branch, &row.branch) {
        if git::branch_exists(&trunk, branch)? {
            match unpushed(&trunk, &repo, &format!("refs/heads/{branch}")) {
                Some(0) => git::delete_local_branch(&trunk, branch)
                    .with_context(|| format!("delete branch {branch}"))?,
                _ => warnings.push(format!(
                    "kept branch {branch}: it has commits that are not pushed"
                )),
            }
        }
    }
    app.db.delete_task_worktree(&task.id, &row.dir_name)?;
    worktrees::prune_empty_parents(&path, &[task_dir(app, task)]);
    Ok(warnings)
}

fn repo_of(app: &App, row: &TaskWorktreeRecord) -> Result<RepoRecord> {
    app.db
        .get_repo(&row.repo_id)?
        .ok_or_else(|| anyhow::anyhow!("{} is no longer indexed", row.repo_id))
}

/// Commits on `rev` that are neither on a remote nor on the default branch.
/// A branch made from the default branch and never committed to has none.
fn unpushed(dir: &Path, repo: &RepoRecord, rev: &str) -> Option<usize> {
    let default = format!("refs/heads/{}", repo.default_branch);
    let mut exclude = vec!["--remotes"];
    if git::ref_exists(dir, &default).unwrap_or(false) {
        exclude.push(&default);
    }
    git::count_commits_not_in(dir, rev, &exclude)
}

/// Commits on `rev` that no branch, tag or remote holds: what a detached
/// HEAD would lose.
fn unreachable(dir: &Path, rev: &str) -> Option<usize> {
    git::count_commits_not_in(dir, rev, &["--remotes", "--branches", "--tags"])
}

/// Why removing the live worktree at `path` would lose work, if it would.
/// A probe that fails counts as a reason: it is never read as "clean".
fn removal_blockers(repo: &RepoRecord, path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    match git::status_counts(path) {
        Ok((0, _)) => {}
        Ok((dirty, _)) => out.push(plural(dirty, "uncommitted change")),
        Err(e) => out.push(format!("a status jeet could not read ({e})")),
    }
    let (commits, what) = match git::head_branch(path) {
        Some(_) => (unpushed(path, repo, "HEAD"), "unpushed commit"),
        None => (unreachable(path, "HEAD"), "commit on no branch"),
    };
    match commits {
        Some(0) => {}
        Some(n) => out.push(plural(n, what)),
        None => out.push("commits jeet could not check".to_string()),
    }
    out
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Bring the index in line with what is on disk, for every active task.
///
/// A worktree git knows inside a task directory but the index does not — an
/// agent ran `git worktree add` itself — is adopted into that task. A row
/// whose directory has vanished is dropped, and git told to forget it.
/// Archived tasks are left alone: their rows describe worktrees that are
/// meant to be gone.
pub fn reconcile(app: &App) -> Result<()> {
    let root = canonical_tasks_root(app);
    let tasks = app.db.list_tasks(Some(TaskState::Active))?;
    let by_slug: HashMap<&str, &TaskRecord> = tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    let mut prune = BTreeSet::new();

    for repo in app.db.list_repos(None)? {
        let trunk = PathBuf::from(&repo.trunk_path);
        if !trunk.is_dir() {
            continue;
        }
        let Ok(entries) = git::worktree_list(&trunk) else {
            continue;
        };
        for entry in entries {
            let Some((slug, dir_name)) = split_task_path(&root, &entry.path) else {
                continue;
            };
            let Some(task) = by_slug.get(slug.as_str()) else {
                continue;
            };
            if !entry.path.exists() {
                continue;
            }
            match app.db.get_task_worktree(&task.id, &dir_name)? {
                Some(mut row) if row.repo_id == repo.id && row.branch != entry.branch => {
                    // Someone switched branches in it. The branch it is on now
                    // is not one this task made, so it is never jeet's to delete.
                    row.branch = entry.branch.clone();
                    row.created_branch = false;
                    app.db.update_task_worktree(&row)?;
                }
                Some(_) => {}
                None => {
                    app.db.insert_task_worktree(&TaskWorktreeRecord {
                        id: 0,
                        task_id: task.id.clone(),
                        repo_id: repo.id.clone(),
                        dir_name,
                        branch: entry.branch.clone(),
                        created_branch: false,
                        head_oid: None,
                        stash_ref: None,
                        pr_number: None,
                        created_at: now_secs(),
                    })?;
                }
            }
        }
    }

    for task in &tasks {
        for row in app.db.list_task_worktrees(&task.id)? {
            if root.join(&task.slug).join(&row.dir_name).exists() {
                continue;
            }
            // An archive that stopped part-way leaves rows holding what is
            // needed to restore the worktree; those are never forgotten.
            if row.head_oid.is_some() || row.stash_ref.is_some() {
                continue;
            }
            app.db.delete_task_worktree(&task.id, &row.dir_name)?;
            if let Some(repo) = app.db.get_repo(&row.repo_id)? {
                prune.insert(PathBuf::from(repo.trunk_path));
            }
        }
    }
    for trunk in prune {
        let _ = git::worktree_prune(&trunk);
    }
    Ok(())
}

/// `(slug, dir_name)` for a path inside a task directory: `dir_name` is the
/// rest of the path, `/`-separated, so a worktree nested in a subdirectory of
/// the task is still the task's.
fn split_task_path(root: &Path, path: &Path) -> Option<(String, String)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string());
    let slug = parts.next()?;
    let rest: Vec<String> = parts.collect();
    if rest.is_empty() {
        return None;
    }
    Some((slug, rest.join("/")))
}

/// Archive a task: end its agent and free the disk its worktrees take while
/// keeping what is needed to bring each back as it was.
///
/// For each worktree: uncommitted changes, untracked files included, are
/// saved under `refs/jeet/stash/…`; HEAD is pinned under `refs/jeet/head/…`;
/// both are recorded on its row, and only then is the worktree removed. The
/// task is marked archived first, so a failure part-way leaves an archived
/// task with some worktrees still present, and running archive again
/// finishes the job. It is refused from inside the agent's tmux session,
/// which ending the agent would end too.
pub fn archive(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    runtime::refuse_inside_session(task, "archive")?;
    let mut warnings = Vec::new();
    // The agent goes first, and is gone before anything is saved: it works
    // in the worktrees about to be removed, and what it wrote after the
    // snapshot would be lost.
    runtime::stop(task).context("end the agent's tmux session")?;
    if task.state == TaskState::Active {
        if let Err(e) = reconcile(app) {
            warnings.push(format!("could not reconcile worktrees ({e:#})"));
        }
    }
    app.db.set_task_state(&task.id, TaskState::Archived)?;
    let task_root = task_dir(app, task);

    for mut row in app.db.list_task_worktrees(&task.id)? {
        let path = worktree_dir(app, task, &row.dir_name);
        if !path.exists() {
            continue;
        }
        let repo = repo_of(app, &row)?;
        let trunk = PathBuf::from(&repo.trunk_path);
        let what = &row.dir_name;

        let Some(head) = git::rev_parse(&path, "HEAD") else {
            bail!("{what}: could not read its HEAD");
        };
        let ignored = git::status_counts(&path).map(|(_, i)| i).unwrap_or(0);
        let snapshot = git::snapshot_changes(&path)
            .with_context(|| format!("{what}: save its uncommitted changes"))?;

        let kept = set_aside_leftovers(&trunk, &task.id, what, &head, snapshot.as_deref())
            .with_context(|| format!("{what}: keep what an earlier unarchive left"))?;
        for name in kept {
            warnings.push(format!(
                "{what}: work an earlier unarchive could not put back is kept at {name}"
            ));
        }
        let head_ref = jeet_ref("head", &task.id, what);
        git::update_ref(&trunk, &head_ref, &head).with_context(|| format!("{what}: pin HEAD"))?;
        let stash_ref = jeet_ref("stash", &task.id, what);
        row.stash_ref = match snapshot {
            Some(oid) => {
                git::update_ref(&trunk, &stash_ref, &oid)
                    .with_context(|| format!("{what}: keep its uncommitted changes"))?;
                Some(stash_ref)
            }
            None => {
                // Left by an earlier attempt, and stale now there is nothing;
                // anything else it held was set aside above.
                let _ = git::delete_ref(&trunk, &stash_ref);
                None
            }
        };
        let branch = git::head_branch(&path);
        if branch != row.branch {
            row.created_branch = false;
            row.branch = branch;
        }
        row.head_oid = Some(head);
        // Recorded before anything is removed: from here on the row alone is
        // enough to restore the worktree.
        app.db.update_task_worktree(&row)?;

        git::worktree_remove(&trunk, &path, true)
            .with_context(|| format!("{what}: remove the worktree"))?;
        worktrees::prune_empty_parents(&path, std::slice::from_ref(&task_root));
        if ignored > 0 {
            warnings.push(format!(
                "{what}: {} not kept",
                plural(ignored, "ignored file or directory")
            ));
        }
    }
    Ok(warnings)
}

/// Bring an archived task's worktrees back: each on its branch, or detached
/// at its old HEAD, with its uncommitted changes reapplied.
///
/// Worktrees already present are left as they are, so running it again after
/// a failure picks up where it stopped. The task becomes active only once
/// every worktree is back.
pub fn unarchive(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    let dir = task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let mut warnings = Vec::new();

    for mut row in app.db.list_task_worktrees(&task.id)? {
        let path = worktree_dir(app, task, &row.dir_name);
        if path.exists() {
            continue;
        }
        let what = row.dir_name.clone();
        let Some(head) = row.head_oid.clone() else {
            warnings.push(format!(
                "{what}: nothing was recorded to restore it from; forgotten"
            ));
            app.db.delete_task_worktree(&task.id, &row.dir_name)?;
            continue;
        };
        let repo = repo_of(app, &row)?;
        let trunk = PathBuf::from(&repo.trunk_path);
        let head_ref = jeet_ref("head", &task.id, &what);
        let mut keep_head_ref = false;

        match row.branch.clone() {
            Some(branch) if git::branch_exists(&trunk, &branch)? => {
                if let Some(holder) = branch_holder(app, &trunk, &branch)? {
                    bail!("{what}: branch {branch} is checked out {holder}; free it and unarchive again");
                }
                git::worktree_add_existing_branch(&trunk, &path, &branch)
                    .with_context(|| format!("{what}: check out {branch}"))?;
                let tip = git::rev_parse(&trunk, &format!("refs/heads/{branch}"));
                if tip.as_deref() != Some(head.as_str())
                    && !tip.is_some_and(|tip| git::is_ancestor(&trunk, &head, &tip))
                {
                    // Under a name of its own: the next archive reuses
                    // head_ref, and the task is active again either way.
                    let kept = match keep(&trunk, &task.id, &what, "head", &head) {
                        Ok(kept) => kept,
                        Err(_) => {
                            keep_head_ref = true;
                            head_ref.clone()
                        }
                    };
                    warnings.push(format!(
                        "{what}: {branch} moved while the task was archived; \
                         the commit it was on is kept at {kept}"
                    ));
                }
            }
            Some(branch) => {
                git::worktree_add_new_branch(&trunk, &path, &branch, &head)
                    .with_context(|| format!("{what}: recreate {branch}"))?;
                warnings.push(format!(
                    "{what}: branch {branch} had been deleted; recreated it where it was"
                ));
            }
            None => git::worktree_add_detached(&trunk, &path, &head)
                .with_context(|| format!("{what}: check out {head}"))?,
        }

        if let Some(stash) = row.stash_ref.clone() {
            // Staged changes come back staged when they can.
            let applied = git::stash_apply(&path, &stash, true)
                .or_else(|_| git::stash_apply(&path, &stash, false));
            match applied {
                Ok(()) => {
                    let _ = git::delete_ref(&trunk, &stash);
                    row.stash_ref = None;
                }
                Err(e) => {
                    // Moved under a name of its own, as archive reuses this one.
                    let kept = git::rev_parse(&trunk, &stash)
                        .and_then(|oid| keep(&trunk, &task.id, &what, "stash", &oid).ok());
                    let at = match kept {
                        Some(kept) => {
                            let _ = git::delete_ref(&trunk, &stash);
                            row.stash_ref = None;
                            kept
                        }
                        None => stash,
                    };
                    warnings.push(format!(
                        "{what}: could not reapply its uncommitted changes ({e:#}); \
                         they are kept at {at}: `git stash apply {at}` in the worktree \
                         retries, and `git update-ref -d {at}` drops them"
                    ));
                }
            }
        }
        if !keep_head_ref {
            let _ = git::delete_ref(&trunk, &head_ref);
            row.head_oid = None;
        }
        app.db.update_task_worktree(&row)?;
    }

    app.db.set_task_state(&task.id, TaskState::Active)?;
    app.db.touch_task(&task.id, now_secs())?;
    Ok(warnings)
}

/// What deleting `task` would destroy, one line per problem: uncommitted
/// changes (live, or saved at archive time), and commits that would be left
/// on no branch, tag or remote — a detached HEAD's, those of a branch the
/// task made (delete removes it), a HEAD pinned at archive that the branch
/// has since moved away from, and anything set aside under `refs/jeet/kept`.
/// A branch the task did not make keeps its commits, so it is not listed.
pub fn losses(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    if task.state == TaskState::Active {
        let _ = reconcile(app);
    }
    let mut out = Vec::new();
    for row in app.db.list_task_worktrees(&task.id)? {
        let what = &match &row.branch {
            Some(branch) => format!("{} ({branch})", row.dir_name),
            None => row.dir_name.clone(),
        };
        let Some(repo) = app.db.get_repo(&row.repo_id)? else {
            continue;
        };
        let trunk = PathBuf::from(&repo.trunk_path);
        let path = worktree_dir(app, task, &row.dir_name);
        let live = path.exists();

        if live {
            match git::status_counts(&path) {
                Ok((0, _)) => {}
                Ok((dirty, _)) => {
                    out.push(format!("{what}: {}", plural(dirty, "uncommitted change")))
                }
                Err(e) => out.push(format!("{what}: could not read its status ({e})")),
            }
        }
        // Saved at archive; on a live worktree, ones unarchive did not get to
        // reapply.
        let stash = row
            .stash_ref
            .clone()
            .unwrap_or_else(|| jeet_ref("stash", &task.id, &row.dir_name));
        if git::ref_exists(&trunk, &stash).unwrap_or(false) {
            out.push(format!(
                "{what}: uncommitted changes saved at archive ({stash})"
            ));
        }

        // The commits at risk are a detached HEAD's, or those of a branch the
        // task made — which delete removes, so it cannot protect them itself.
        let at_risk = if live {
            match git::head_branch(&path) {
                None => Some((path.clone(), "HEAD".to_string(), None)),
                Some(branch) if Some(&branch) == row.branch.as_ref() && row.created_branch => {
                    Some((path.clone(), format!("refs/heads/{branch}"), Some(branch)))
                }
                Some(_) => None,
            }
        } else {
            match (&row.branch, row.created_branch, &row.head_oid) {
                (None, _, Some(head)) => Some((trunk.clone(), head.clone(), None)),
                (Some(branch), true, _) if git::branch_exists(&trunk, branch).unwrap_or(false) => {
                    Some((
                        trunk.clone(),
                        format!("refs/heads/{branch}"),
                        Some(branch.clone()),
                    ))
                }
                _ => None,
            }
        };
        let mut counted = Vec::new();
        if let Some((dir, rev, own_branch)) = at_risk {
            let mut exclude = vec!["--remotes".to_string(), "--tags".to_string()];
            if let Some(branch) = own_branch {
                exclude.push(format!("--exclude={branch}"));
            }
            exclude.push("--branches".to_string());
            let exclude: Vec<&str> = exclude.iter().map(String::as_str).collect();
            match git::count_commits_not_in(&dir, &rev, &exclude) {
                Some(0) => {}
                Some(n) => out.push(format!(
                    "{what}: {}",
                    plural(n, "commit on no other branch or remote")
                )),
                None => out.push(format!("{what}: could not check for unpushed commits")),
            }
            counted.extend(git::rev_parse(&dir, &rev));
        }
        if live {
            counted.extend(git::rev_parse(&path, "HEAD"));
        }

        // HEAD as pinned at archive. Normally its branch or the commits just
        // counted hold it, but not if the branch moved while the task was
        // archived, or an unarchive stopped before releasing it.
        let head_ref = jeet_ref("head", &task.id, &row.dir_name);
        if git::ref_exists(&trunk, &head_ref).unwrap_or(false) {
            let mut exclude = vec!["--remotes", "--tags", "--branches"];
            exclude.extend(counted.iter().map(String::as_str));
            match git::count_commits_not_in(&trunk, &head_ref, &exclude) {
                Some(0) => {}
                Some(n) => out.push(format!(
                    "{what}: {} kept at {head_ref}",
                    plural(n, "commit on no branch or remote")
                )),
                None => out.push(format!("{what}: could not check {head_ref}")),
            }
        }
    }

    // Work unarchive could not put back, whichever worktree it came from.
    for (trunk, name) in kept_refs(app, task)? {
        let kind = name.rsplit('/').next().unwrap_or_default();
        if kind.starts_with("stash-") {
            out.push(format!(
                "{name}: uncommitted changes unarchive could not reapply"
            ));
            continue;
        }
        match unreachable(&trunk, &name) {
            Some(0) => {}
            Some(n) => out.push(format!(
                "{name}: {}",
                plural(n, "commit on no branch or remote")
            )),
            None => out.push(format!("{name}: could not check its commits")),
        }
    }
    Ok(out)
}

/// Delete a task and everything it owns: its agent's tmux session, its
/// worktrees, the branches it made, its `refs/jeet` refs, its directory and
/// its rows. Branches it did not make
/// are left alone. The caller has already confirmed what [`losses`] lists.
///
/// The rows go last, so a failure part-way leaves a task that can be deleted
/// again.
pub fn delete(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    if task.slug.is_empty() || task.slug.starts_with('.') || task.slug.contains(['/', '\\']) {
        bail!(
            "refusing to delete a task whose directory name is {:?}",
            task.slug
        );
    }
    runtime::refuse_inside_session(task, "delete")?;
    let mut warnings = Vec::new();
    // Stopped, and gone, before anything is removed: a running agent would
    // carry on working in worktrees that are about to go, and its exit hook
    // would write into the task directory as it was being removed.
    runtime::stop(task).context("end the agent's tmux session")?;
    if task.state == TaskState::Active {
        if let Err(e) = reconcile(app) {
            warnings.push(format!("could not reconcile worktrees ({e:#})"));
        }
    }
    let dir = task_dir(app, task);
    let mut trunks: Vec<PathBuf> = Vec::new();

    for row in app.db.list_task_worktrees(&task.id)? {
        let Some(repo) = app.db.get_repo(&row.repo_id)? else {
            continue;
        };
        let trunk = PathBuf::from(&repo.trunk_path);
        let path = worktree_dir(app, task, &row.dir_name);
        if path.exists() {
            git::worktree_remove(&trunk, &path, true)
                .with_context(|| format!("{}: remove the worktree", row.dir_name))?;
        }
        if let (true, Some(branch)) = (row.created_branch, &row.branch) {
            if git::branch_exists(&trunk, branch)? {
                match branch_holder(app, &trunk, branch)? {
                    Some(holder) => {
                        warnings.push(format!("kept branch {branch}: it is checked out {holder}"))
                    }
                    None => git::delete_local_branch(&trunk, branch)
                        .with_context(|| format!("delete branch {branch}"))?,
                }
            }
        }
        if !trunks.contains(&trunk) {
            trunks.push(trunk);
        }
    }

    for trunk in &trunks {
        for kind in ["stash", "head"] {
            for name in git::refs_under(trunk, &format!("refs/jeet/{kind}/{}/", task.id))? {
                git::delete_ref(trunk, &name).with_context(|| format!("delete {name}"))?;
            }
        }
        let _ = git::worktree_prune(trunk);
    }
    for (trunk, name) in kept_refs(app, task)? {
        git::delete_ref(&trunk, &name).with_context(|| format!("delete {name}"))?;
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    app.db.delete_task(&task.id)?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_plain_lowercase_words() {
        assert_eq!(slugify("Fix the Login page"), "fix-the-login-page");
        assert_eq!(slugify("  feat/API: v2!  "), "feat-api-v2");
        assert_eq!(slugify("über café"), "ber-caf");
        assert_eq!(slugify("!!!"), "task");
        assert_eq!(slugify(""), "task");
        let long = slugify(&"word ".repeat(40));
        assert!(long.len() <= SLUG_MAX, "{long}");
        assert!(!long.ends_with('-'), "{long}");
    }

    #[test]
    fn numbered_names_start_at_two() {
        assert_eq!(numbered("api", 1), "api");
        assert_eq!(numbered("api", 2), "api-2");
    }

    #[test]
    fn dir_names_skip_taken_ones_and_strays() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(pick_dir_name(dir.path(), &[], "api"), "api");
        assert_eq!(pick_dir_name(dir.path(), &["api".into()], "api"), "api-2");
        std::fs::create_dir(dir.path().join("api-2")).unwrap();
        assert_eq!(pick_dir_name(dir.path(), &["api".into()], "api"), "api-3");
        // A repo named like a dotfile does not hide itself.
        assert_eq!(pick_dir_name(dir.path(), &[], ".github"), "github");
    }

    #[test]
    fn hand_picked_dir_names_stay_inside_the_task() {
        for ok in ["api", "api-v2", "web_2", "docs.site"] {
            assert!(validate_dir_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", ".", "..", ".jeet", "a/b", "../up", "a b", "x.lock", "a..b",
        ] {
            assert!(validate_dir_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jeet_refs_are_valid_ref_names() {
        assert_eq!(jeet_ref("head", "t1", "api"), "refs/jeet/head/t1/api");
        assert_eq!(jeet_ref("head", "t1", "api-2"), "refs/jeet/head/t1/api-2");
        assert_eq!(
            jeet_ref("stash", "t1", "sub/my dir.v2"),
            "refs/jeet/stash/t1/sub_2Fmy_20dir_2Ev2"
        );
        assert_eq!(ref_component(".x.lock"), "_2Ex_2Elock");
    }

    #[test]
    fn jeet_refs_never_share_a_name() {
        // Every pair here once mapped to one ref, or nested one under another.
        let names = [
            "docs.site",
            "docs-site",
            "docs_site",
            "docs_2Esite",
            "foo.js",
            "foo-js",
            "sub",
            "sub/web",
            "sub-web",
        ];
        let refs: BTreeSet<String> = names.iter().map(|n| ref_component(n)).collect();
        assert_eq!(refs.len(), names.len(), "{refs:?}");
        assert!(refs.iter().all(|r| !r.contains('/')), "{refs:?}");
    }

    #[test]
    fn task_paths_split_into_slug_and_worktree() {
        let root = Path::new("/home/.jeet/tasks");
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix/api")),
            Some(("fix".into(), "api".into()))
        );
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix/sub/web")),
            Some(("fix".into(), "sub/web".into()))
        );
        // The task directory itself, and anything outside, are not worktrees of it.
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix")),
            None
        );
        assert_eq!(split_task_path(root, Path::new("/elsewhere/fix/api")), None);
    }

    #[test]
    fn the_task_root_is_found_from_below() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("tasks/fix");
        std::fs::create_dir_all(task.join(".jeet")).unwrap();
        std::fs::write(task.join(".jeet/task-id"), "abc-123\n").unwrap();
        let deep = task.join("api/src/bin");
        std::fs::create_dir_all(&deep).unwrap();

        let (root, id) = find_task_root(&deep).unwrap();
        assert_eq!(root, task);
        assert_eq!(id, "abc-123");
        assert!(find_task_root(dir.path()).is_none());
    }

    #[test]
    fn summaries_count_worktrees_and_name_repos_once() {
        let row = |repo: &str, dir: &str| TaskWorktreeRecord {
            id: 0,
            task_id: "t".into(),
            repo_id: repo.into(),
            dir_name: dir.into(),
            branch: None,
            created_branch: false,
            head_oid: None,
            stash_ref: None,
            pr_number: None,
            created_at: 0,
        };
        assert_eq!(worktree_summary(&[]), "empty");
        assert_eq!(
            worktree_summary(&[
                row("github.com/acme/api", "api"),
                row("github.com/acme/web", "web"),
                row("github.com/acme/api", "api-2"),
            ]),
            "3 wt: api, web"
        );
    }

    #[test]
    fn the_skill_tells_the_agent_how_to_get_code() {
        let dir = tempfile::tempdir().unwrap();
        install_skill(dir.path()).unwrap();
        let skill =
            std::fs::read_to_string(dir.path().join(".claude/skills/jeet/SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: jeet\n"), "{skill}");
        for needle in [
            "jeet worktree add",
            "jeet worktree ls",
            "jeet worktree rm",
            "jeet ls",
            "gh pr create",
            "$JEET_HOME/store",
        ] {
            assert!(skill.contains(needle), "skill does not mention {needle}");
        }
    }
}
