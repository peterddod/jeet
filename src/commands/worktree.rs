//! `jeet worktree …`: the worktrees of one task.
//!
//! The task is the one named with `--task`, else `$JEET_TASK_ID` (set in a
//! task's agent session), else the task whose directory holds the current
//! directory. `add` and `path` print the worktree's path and nothing else on
//! stdout, so an agent can `cd "$(jeet worktree add api)"`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::cd;
use crate::commands::task::standing_in;
use crate::context::App;
use crate::db::tasks::TaskState;
use crate::git;
use crate::tasks;
use crate::worktrees::{self, WorktreeEntry, WorktreeKind};

pub fn add(
    app: &App,
    task: Option<&str>,
    repo: &str,
    branch: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    let task = tasks::current(app, task)?;
    let added = tasks::add_worktree(app, &task, repo, branch, name)?;
    for warning in &added.warnings {
        eprintln!("jeet: {warning}");
    }
    let on = match (&added.record.branch, added.record.read_only) {
        (Some(branch), _) => format!("on {branch}"),
        (None, true) => "detached (read-only)".to_string(),
        (None, false) => "detached".to_string(),
    };
    eprintln!(
        "jeet: {} ({}, {on}) -> {}",
        added.record.dir_name,
        added.record.repo_id,
        added.path.display()
    );
    println!("{}", added.path.display());
    cd::request(&added.path)
}

pub fn ls(app: &App, task: Option<&str>) -> Result<()> {
    let task = tasks::current(app, task)?;
    if task.state == TaskState::Active {
        match tasks::reconcile(app) {
            Ok(warnings) => {
                for warning in warnings {
                    eprintln!("jeet: warning: {warning}");
                }
            }
            Err(e) => eprintln!("jeet: warning: could not reconcile worktrees: {e:#}"),
        }
    }
    let rows = app.db.list_task_worktrees(&task.id)?;
    if rows.is_empty() {
        eprintln!(
            "jeet: task {} has no worktrees; `jeet worktree add <repo>` adds one",
            task.name
        );
        return Ok(());
    }

    // Resolved once per repository: it costs a git subprocess and cannot
    // differ between worktrees of the same one.
    let mut bases: HashMap<String, String> = HashMap::new();
    for row in rows {
        let path = tasks::worktree_dir(app, &task, &row.dir_name);
        let short = |oid: &str| oid.get(..7).unwrap_or(oid).to_string();
        let (checkout, notes) = if path.exists() {
            let entry = WorktreeEntry {
                path: path.clone(),
                branch: git::head_branch(&path),
                kind: WorktreeKind::Task,
                last_used: 0,
                missing: false,
            };
            let base = match bases.get(&row.repo_id) {
                Some(base) => base.clone(),
                None => {
                    let base = match app.db.get_repo(&row.repo_id)? {
                        Some(repo) => worktrees::comparison_base(&repo),
                        None => "HEAD".to_string(),
                    };
                    bases.insert(row.repo_id.clone(), base.clone());
                    base
                }
            };
            let status = worktrees::status_against(&entry, &base);
            let mut notes = Vec::new();
            if status.dirty > 0 {
                notes.push(format!("{} uncommitted", status.dirty));
            }
            if status.ahead > 0 || status.behind > 0 {
                notes.push(format!("+{}/-{} commits", status.ahead, status.behind));
            }
            notes.push(status.diff_summary());
            (entry.display_name(), notes.join(" "))
        } else {
            let checkout = match (&row.branch, &row.head_oid) {
                (Some(branch), _) => branch.clone(),
                (None, Some(head)) => format!("detached @ {}", short(head)),
                (None, None) => "detached".to_string(),
            };
            let note = match (&row.head_oid, &row.stash_ref) {
                (Some(_), Some(_)) => "archived, with uncommitted changes saved",
                (Some(_), None) => "archived",
                _ => "missing",
            };
            (checkout, note.to_string())
        };
        let checkout = match row.read_only {
            true => format!("{checkout} (read-only)"),
            false => checkout,
        };
        println!(
            "{:<16} {:<32} {:<24} {}",
            row.dir_name, row.repo_id, checkout, notes
        );
    }
    Ok(())
}

pub fn rm(app: &App, task: Option<&str>, name: &str, force: bool) -> Result<()> {
    let task = tasks::current(app, task)?;
    let path = tasks::worktree_dir(app, &task, name);
    // Removing the directory a shell sits in leaves it nowhere.
    if path.exists() && standing_in(&path) {
        bail!("you are inside {}; cd out of it first", path.display());
    }
    let warnings = tasks::remove_worktree(app, &task, name, force)?;
    for warning in &warnings {
        eprintln!("jeet: {warning}");
    }
    eprintln!("jeet: removed {name} from task {}", task.name);
    Ok(())
}

pub fn path(app: &App, task: Option<&str>, name: &str) -> Result<()> {
    let task = tasks::current(app, task)?;
    if app.db.get_task_worktree(&task.id, name)?.is_none() {
        bail!(
            "task {} has no worktree named {name}; `jeet worktree ls` lists them",
            task.name
        );
    }
    let path: PathBuf = tasks::worktree_dir(app, &task, name);
    if !path.is_dir() {
        bail!(
            "{name} is not on disk{}",
            match task.state {
                TaskState::Archived =>
                    format!("; `jeet task unarchive {}` brings it back", task.slug),
                TaskState::Active => String::new(),
            }
        );
    }
    println!("{}", path.display());
    Ok(())
}
