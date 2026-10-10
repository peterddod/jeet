//! `jeet task …`: the scriptable side of tasks.
//!
//! Commands that print a path print exactly that one line on stdout;
//! everything else jeet has to say goes to stderr.

use std::io::{IsTerminal, Write};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::agent::{self, AgentSpec};
use crate::cd;
use crate::context::App;
use crate::db::tasks::{TaskRecord, TaskState};
use crate::tasks::{self, NewTask};
use crate::worktrees::now_secs;

pub fn new(
    app: &App,
    name: &str,
    project: Option<&str>,
    repos: &[String],
    branch: Option<&str>,
) -> Result<()> {
    let created = tasks::create(
        app,
        &NewTask {
            name,
            project,
            repos,
            branch,
        },
    )?;
    warn_all(&created.warnings);
    let dir = tasks::task_dir(app, &created.task);
    eprintln!("jeet: task {} -> {}", created.task.name, dir.display());
    println!("{}", dir.display());
    cd::request(&dir)
}

pub fn ls(app: &App, archived: bool) -> Result<()> {
    if let Err(e) = tasks::reconcile(app) {
        eprintln!("jeet: warning: could not reconcile worktrees: {e:#}");
    }
    let state = if archived {
        TaskState::Archived
    } else {
        TaskState::Active
    };
    let list = app.db.list_tasks(Some(state))?;
    if list.is_empty() {
        eprintln!(
            "jeet: no {} tasks; `jeet task new <name>` starts one",
            state.as_str()
        );
        return Ok(());
    }
    println!("{:<28} {:<28} {:<10} NAME", "TASK", "WORKTREES", "ACTIVE");
    for task in list {
        let rows = app.db.list_task_worktrees(&task.id)?;
        println!(
            "{:<28} {:<28} {:<10} {}",
            task.slug,
            tasks::worktree_summary(&rows),
            agent::age_label(task.last_activity),
            task.name
        );
    }
    Ok(())
}

/// Start the configured agent in the task directory, in the foreground.
/// Without a terminal there is nobody to talk to it, so the directory is
/// printed instead.
pub fn open(app: &App, name: &str) -> Result<()> {
    let task = tasks::find(app, name)?;
    if task.state == TaskState::Archived {
        bail!(
            "task {} is archived; `jeet task unarchive {}` first",
            task.name,
            task.slug
        );
    }
    let dir = tasks::task_dir(app, &task);
    let _ = app.db.touch_task(&task.id, now_secs());
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        println!("{}", dir.display());
        return Ok(());
    }

    let spec = AgentSpec::from_config(&app.config)?;
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty agent command"))?;
    eprintln!("jeet: starting {} in {}", spec.display(), dir.display());
    let status = Command::new(program)
        .args(args)
        .current_dir(&dir)
        .env(tasks::TASK_ID_ENV, &task.id)
        .status()
        .with_context(|| format!("failed to launch `{program}`"))?;
    let _ = app.db.touch_task(&task.id, now_secs());
    cd::request(&dir)?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub fn archive(app: &App, name: &str) -> Result<()> {
    let task = tasks::find(app, name)?;
    let warnings = tasks::archive(app, &task)?;
    warn_all(&warnings);
    eprintln!("jeet: archived task {}", task.name);
    Ok(())
}

pub fn unarchive(app: &App, name: &str) -> Result<()> {
    let task = tasks::find(app, name)?;
    if task.state == TaskState::Active {
        eprintln!("jeet: task {} is not archived", task.name);
        return Ok(());
    }
    let warnings = tasks::unarchive(app, &task)?;
    warn_all(&warnings);
    eprintln!(
        "jeet: task {} is back at {}",
        task.name,
        tasks::task_dir(app, &task).display()
    );
    Ok(())
}

/// Delete a task once the user has seen what would be lost and agreed — or
/// passed --force. Without a terminal there is no agreeing, only --force.
pub fn delete(app: &App, name: &str, force: bool) -> Result<()> {
    let task = tasks::find(app, name)?;
    let dir = tasks::task_dir(app, &task);
    if standing_in(&dir) {
        bail!(
            "you are inside {}; cd out of it before deleting the task",
            dir.display()
        );
    }
    if !force {
        confirm_delete(app, &task)?;
    }
    let warnings = tasks::delete(app, &task)?;
    warn_all(&warnings);
    eprintln!("jeet: deleted task {}", task.name);
    Ok(())
}

fn confirm_delete(app: &App, task: &TaskRecord) -> Result<()> {
    let losses = tasks::losses(app, task)?;
    let mut report = format!(
        "deleting task {} removes {} and its worktrees",
        task.name,
        tasks::task_dir(app, task).display()
    );
    if !losses.is_empty() {
        report.push_str(", losing:");
    }
    for loss in &losses {
        report.push_str(&format!("\n  {loss}"));
    }
    if !std::io::stdin().is_terminal() {
        bail!("{report}\npass --force to delete it without a terminal");
    }
    eprintln!("jeet: {report}");
    eprint!("delete it? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        bail!("kept task {}", task.name);
    }
    Ok(())
}

pub fn path(app: &App, name: &str) -> Result<()> {
    let task = tasks::find(app, name)?;
    println!("{}", tasks::task_dir(app, &task).display());
    Ok(())
}

/// Whether the shell is in `dir` or below it.
pub fn standing_in(dir: &std::path::Path) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    cwd.starts_with(dir)
}

fn warn_all(warnings: &[String]) {
    for warning in warnings {
        eprintln!("jeet: {warning}");
    }
}
