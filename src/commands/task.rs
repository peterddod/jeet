//! `jeet task …`: the scriptable side of tasks.
//!
//! Commands that print a path print exactly that one line on stdout;
//! everything else jeet has to say goes to stderr.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Result};

use crate::agent::{self, AgentSpec};
use crate::cd;
use crate::context::App;
use crate::db::tasks::{TaskRecord, TaskState};
use crate::runtime;
use crate::tasks::{self, NewTask};
use crate::tmux;
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
    let mut list: Vec<(i64, TaskRecord)> = app
        .db
        .list_tasks(Some(state))?
        .into_iter()
        .map(|t| (runtime::last_activity(app, &t), t))
        .collect();
    list.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    if list.is_empty() {
        eprintln!(
            "jeet: no {} tasks; `jeet task new <name>` starts one",
            state.as_str()
        );
        return Ok(());
    }
    let tmux_ok = tmux::available();
    println!(
        "{:<28} {:<28} {:<10} {:<24} NAME",
        "TASK", "WORKTREES", "ACTIVE", "STATUS"
    );
    for (at, task) in list {
        let rows = app.db.list_task_worktrees(&task.id)?;
        let status = match task.state {
            TaskState::Active => runtime::activity(app, &task, tmux_ok).label(),
            TaskState::Archived => "archived".to_string(),
        };
        println!(
            "{:<28} {:<28} {:<10} {:<24} {}",
            task.slug,
            tasks::worktree_summary(&rows),
            agent::age_label(at),
            status,
            task.name
        );
    }
    Ok(())
}

/// Attach to the task's agent in its tmux session, starting the agent first
/// if it is not running. Without a terminal there is nobody to attach, so
/// the agent is left running detached and the directory is printed instead.
pub fn open(app: &App, name: &str, session: Option<&str>) -> Result<()> {
    let task = tasks::find(app, name)?;
    if task.state == TaskState::Archived {
        bail!(
            "task {} is archived; `jeet task unarchive {}` first",
            task.name,
            task.slug
        );
    }
    let dir = tasks::task_dir(app, &task);
    let running = runtime::ensure_running(app, &task, session)?;
    warn_all(&running.warnings);
    let tmux_session = &running.tmux_session;
    if running.started {
        eprintln!(
            "jeet: started {} in tmux session {tmux_session}",
            AgentSpec::from_config(&app.config)?.display()
        );
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("jeet: `tmux attach -t {tmux_session}` to talk to the agent");
        println!("{}", dir.display());
        return Ok(());
    }

    let code = tmux::attach(tmux_session)?;
    let _ = app.db.touch_task(&task.id, now_secs());
    cd::request(&dir)?;
    if code != 0 {
        std::process::exit(code);
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
    // By exact name only: with --force nothing asks, so a stale name must not
    // land on whichever task it happens to be a prefix of.
    let task = tasks::find_exact(app, name)?;
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
