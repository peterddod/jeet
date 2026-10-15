mod agent;
mod assistant;
mod cd;
mod cli;

mod commands;
mod completion_candidates;
mod config;
mod context;
mod db;
mod git;
mod github;
mod harness;
mod identity;
mod instructions;
mod manager;
mod messages;
mod notify;
mod paths;
mod permissions;
mod placement;
mod pr_history;
mod projects;
mod remote;
mod reports;
mod resolve;
mod review;
mod runtime;
mod startup;
mod status;
mod store;
mod tasks;
mod tmux;
mod todo;
mod tui;
mod waiter;
mod watch;
mod worktrees;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{
    Cli, Commands, InboxCommands, ManagerCommands, PrCommands, ProjectCommands, TaskCommands,
    TodoCommands, WatchCommands, WorktreeArgs, WorktreeCommands,
};

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    // The wake channel's token leaves the environment before anything runs
    // that could pass it on.
    let wake = match &cli.command {
        Some(Commands::Inbox {
            command: InboxCommands::Watch { harness, .. },
        }) => harness::by_id(harness).and_then(|h| h.wake_env()),
        _ => None,
    };

    // Only once `jeet review` has recorded the runner as its task's does the
    // runner go on to find out who it is. One that never gets it, its
    // launcher gone first, ends its job too.
    if let Some(Commands::ReviewJob { state }) = &cli.command {
        if let Err(e) = review::await_go() {
            review::refused(state);
            return Err(e);
        }
    }
    // Who the caller is and what it may do, before the command does
    // anything.
    let authorized = match permissions::authorize(cli.command.as_ref()) {
        Ok(authorized) => authorized,
        Err(e) => {
            if let Some(Commands::ReviewJob { state }) = &cli.command {
                review::refused(state);
            }
            return Err(e);
        }
    };
    let result = run(cli.command, wake);
    if let Some(action) = authorized {
        action.finish(&result);
    }
    result
}

/// Whether `command` is one a person runs, as against jeet's own plumbing
/// (hooks, background jobs, completions): what finishes moving reviews out
/// of the tasks after an upgrade ([`review::migrate_leftovers`]) waits for
/// one. Nor from inside an agent's session.
fn moves_reviews(command: Option<&Commands>) -> bool {
    if std::env::var(tasks::TASK_ID_ENV).is_ok_and(|v| !v.trim().is_empty()) {
        return false;
    }
    !matches!(
        command,
        Some(
            Commands::Completions { .. }
                | Commands::Complete { .. }
                | Commands::InitShell
                | Commands::ReviewJob { .. }
                | Commands::StoreSync
                | Commands::TaskHook { .. }
                | Commands::TaskStartup { .. }
                | Commands::Inbox { .. }
                | Commands::Manager {
                    command: ManagerCommands::Drain { .. }
                }
        )
    )
}

fn run(command: Option<Commands>, wake: Option<waiter::WakeEnv>) -> Result<()> {
    if moves_reviews(command.as_ref()) {
        if let Ok(app) = context::App::open() {
            review::migrate_leftovers(&app);
        }
    }
    let Some(command) = command else {
        let app = context::App::open()?;
        return commands::tasks::run(&app);
    };

    match command {
        Commands::Completions { shell } => commands::completions::run(shell)?,
        Commands::Complete { what } => commands::completions::run_complete(&what, None)?,
        Commands::InitShell => commands::shell::run_init_shell()?,
        Commands::Notify { title, body, url } => {
            commands::notify::run(&title, body.as_deref(), url.as_deref())?
        }
        Commands::Report { what } => {
            let app = context::App::open()?;
            commands::report::run(&app, what)?
        }
        Commands::InstallShell => commands::install_shell::run()?,
        Commands::Watch { once, command } => {
            let app = context::App::open()?;
            match (once, command) {
                (true, _) => watch::run_once(&app)?,
                (false, Some(WatchCommands::Install)) => watch::install(&app)?,
                (false, Some(WatchCommands::Uninstall)) => watch::uninstall()?,
                (false, None | Some(WatchCommands::Status)) => watch::status(&app)?,
            }
        }
        Commands::Clone { url } => {
            let app = context::App::open()?;
            commands::clone::run(&app, &url)?;
        }
        Commands::Import {
            paths,
            yes,
            remove_originals,
        } => {
            let app = context::App::open()?;
            commands::import::run(&app, &paths, yes, remove_originals)?;
        }
        Commands::Ls { filter } => {
            let app = context::App::open()?;
            commands::ls::run(&app, filter.as_deref())?;
        }
        Commands::Path { filter } => {
            let app = context::App::open()?;
            commands::path::run(&app, &filter)?;
        }
        Commands::Exec { filter } => {
            let app = context::App::open()?;
            commands::exec::run(&app, &filter)?;
        }
        Commands::Tasks => {
            let app = context::App::open()?;
            commands::tasks::run(&app)?;
        }
        Commands::Chat { new, pick } => {
            let app = context::App::open()?;
            commands::chat::run(&app, new, pick)?;
        }
        Commands::Todo { command } => {
            let app = context::App::open()?;
            match command.unwrap_or(TodoCommands::Ls { json: false }) {
                TodoCommands::Ls { json } => commands::todo::ls(&app, json)?,
                TodoCommands::Add {
                    text,
                    priority,
                    tasks,
                    json,
                } => commands::todo::add(&app, &text, priority, &tasks, json)?,
                TodoCommands::Done { items, undo, json } => {
                    commands::todo::done(&app, &items, undo, json)?
                }
                TodoCommands::Rm { items, json } => commands::todo::rm(&app, &items, json)?,
            }
        }
        Commands::Explore => {
            let app = context::App::open()?;
            commands::explore::run(&app)?;
        }
        Commands::Prs { repo } => {
            let app = context::App::open()?;
            commands::prs::run(&app, repo.as_deref())?;
        }
        Commands::Review {
            pr,
            repo,
            no_command,
            rerun,
        } => {
            let app = context::App::open()?;
            commands::review::run(&app, &pr, repo.as_deref(), !no_command, rerun)?;
        }
        Commands::Pr { command } => {
            let app = context::App::open()?;
            match command {
                PrCommands::Merge {
                    pr,
                    repo,
                    method,
                    keep_branch,
                } => commands::pr::merge(&app, &pr, repo.as_deref(), method, keep_branch)?,
            }
        }
        Commands::ReviewJob { state } => review::run_job(&state)?,
        Commands::Sync { repo } => {
            let app = context::App::open()?;
            commands::sync::run(&app, repo.as_deref())?;
        }
        Commands::StoreSync => {
            let app = context::App::open()?;
            store::run_background(&app)?;
        }
        // Run by the agent on every tool call: quick, silent on stdout, and
        // never a failure, whatever goes wrong.
        Commands::TaskHook {
            event,
            harness,
            task,
        } => {
            let _ =
                std::panic::catch_unwind(|| status::run_hook(&harness, &event, task.as_deref()));
        }
        // Started in the background by the agent's session-start hook, whose
        // stdout is the agent's context: says nothing, and never fails.
        Commands::Inbox {
            command: InboxCommands::Watch { task, harness },
        } => {
            let (Some(found), Some(env)) = (harness::by_id(&harness), wake) else {
                return Ok(());
            };
            let watch = std::panic::AssertUnwindSafe(|| waiter::run(found, &task, env));
            if let Ok(Err(e)) = std::panic::catch_unwind(watch) {
                eprintln!("jeet inbox watch: {e:#}");
            }
        }
        // Launched in the background with a task's agent; says nothing.
        Commands::TaskStartup { task, session } => {
            let app = context::App::open()?;
            startup::run(&app, &task, session.as_deref())?;
        }
        Commands::Manager { command } => {
            let app = context::App::open()?;
            match command {
                ManagerCommands::New {
                    name,
                    delegation,
                    brief_file,
                    max_children,
                    project,
                } => commands::manager::new(
                    &app,
                    &commands::manager::NewManager {
                        name: &name,
                        delegation: &delegation,
                        brief_file: brief_file.as_deref(),
                        max_children,
                        project: project.as_deref(),
                    },
                )?,
                ManagerCommands::Ls { json } => commands::manager::ls(&app, json)?,
                ManagerCommands::Show { name, json } => commands::manager::show(&app, &name, json)?,
                ManagerCommands::Grant { name, caps, none } => {
                    commands::manager::grant(&app, &name, &caps, none)?
                }
                ManagerCommands::Budget { name, change } => {
                    commands::manager::set_budget(&app, &name, change)?
                }
                ManagerCommands::Pause { name } => commands::manager::pause(&app, &name)?,
                ManagerCommands::Resume { name } => commands::manager::resume(&app, &name)?,
                ManagerCommands::Stop { name, all } => {
                    commands::manager::stop(&app, name.as_deref(), all)?
                }
                ManagerCommands::Drain { after } => {
                    commands::manager::drain(&app, after.as_deref())?
                }
            }
        }
        Commands::Task { command } => {
            let app = context::App::open()?;
            match command {
                TaskCommands::New {
                    name,
                    project,
                    repos,
                    branch,
                    prompt,
                    delegation,
                } => commands::task::new(
                    &app,
                    &commands::task::NewTaskArgs {
                        name: &name,
                        project: project.as_deref(),
                        repos: &repos,
                        branch: branch.as_deref(),
                        prompt: prompt.as_deref(),
                        delegation: &delegation,
                    },
                )?,
                TaskCommands::Send { name, message } => {
                    commands::task::send(&app, &name, &message)?
                }
                TaskCommands::Interrupt { name } => commands::task::interrupt(&app, &name)?,
                TaskCommands::Restart { name, force } => {
                    commands::task::restart(&app, &name, force)?
                }
                TaskCommands::Adopt { name } => commands::task::adopt(&app, &name)?,
                TaskCommands::Ls { archived } => commands::task::ls(&app, archived)?,
                TaskCommands::Open { name, session } => {
                    commands::task::open(&app, &name, session.as_deref())?
                }
                TaskCommands::Archive {
                    name,
                    force,
                    recursive,
                } => commands::task::archive(&app, &name, force, recursive)?,
                TaskCommands::Unarchive { name, recursive } => {
                    commands::task::unarchive(&app, &name, recursive)?
                }
                TaskCommands::Delete {
                    name,
                    force,
                    recursive,
                    reparent,
                } => match reparent {
                    true => commands::task::delete_reparenting(&app, &name, force)?,
                    false => commands::task::delete(&app, &name, force, recursive)?,
                },
                TaskCommands::Path { name } => commands::task::path(&app, &name)?,
                TaskCommands::Prs { name, offline } => {
                    commands::task::prs(&app, name.as_deref(), offline)?
                }
                TaskCommands::Last { name, count, json } => {
                    commands::task::last(&app, &name, count.into(), json)?
                }
                TaskCommands::Resync { name, force } => commands::task::resync(&app, &name, force)?,
            }
        }
        Commands::Projects { command } => {
            let app = context::App::open()?;
            match command {
                None if std::io::IsTerminal::is_terminal(&std::io::stdout()) => {
                    commands::tasks::run_projects(&app)?
                }
                None | Some(ProjectCommands::Ls) => commands::projects::ls(&app)?,
                Some(ProjectCommands::New { name, repos }) => {
                    commands::projects::new(&app, &name, &repos)?
                }
                Some(ProjectCommands::Edit { name }) => commands::projects::edit(&app, &name)?,
                Some(ProjectCommands::Rm { name, force }) => {
                    commands::projects::rm(&app, &name, force)?
                }
            }
        }
        Commands::Worktree(WorktreeArgs { command, task }) => {
            let app = context::App::open()?;
            let task = task.as_deref();
            match command {
                WorktreeCommands::Add { repo, branch, name } => {
                    commands::worktree::add(&app, task, &repo, branch.as_deref(), name.as_deref())?
                }
                WorktreeCommands::Ls => commands::worktree::ls(&app, task)?,
                WorktreeCommands::Rm { name, force } => {
                    commands::worktree::rm(&app, task, &name, force)?
                }
                WorktreeCommands::Path { name } => commands::worktree::path(&app, task, &name)?,
            }
        }
    }
    Ok(())
}
