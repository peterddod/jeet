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
mod instructions;
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
mod watch;
mod worktrees;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{
    Cli, Commands, PrCommands, ProjectCommands, TaskCommands, TodoCommands, WatchCommands,
    WorktreeArgs, WorktreeCommands,
};

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    // The assistant's own checks, before the command does anything.
    let authorized = permissions::authorize(cli.command.as_ref())?;
    let result = run(cli.command);
    if let Some(action) = authorized {
        action.finish(&result);
    }
    result
}

fn run(command: Option<Commands>) -> Result<()> {
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
        Commands::Report { kind, message } => {
            let app = context::App::open()?;
            commands::report::run(&app, kind, &message)?
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
        Commands::TaskHook { event, harness } => {
            let _ = std::panic::catch_unwind(|| status::run_hook(&harness, &event));
        }
        // Launched in the background with a task's agent; says nothing.
        Commands::TaskStartup { task, session } => {
            let app = context::App::open()?;
            startup::run(&app, &task, session.as_deref())?;
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
                    parent,
                    goal,
                    done_when,
                } => commands::task::new(
                    &app,
                    &name,
                    project.as_deref(),
                    &repos,
                    branch.as_deref(),
                    prompt.as_deref(),
                    parent.as_deref(),
                    goal.as_deref(),
                    done_when.as_deref(),
                )?,
                TaskCommands::Send { name, message } => {
                    commands::task::send(&app, &name, &message)?
                }
                TaskCommands::Interrupt { name } => commands::task::interrupt(&app, &name)?,
                TaskCommands::Ls { archived } => commands::task::ls(&app, archived)?,
                TaskCommands::Open { name, session } => {
                    commands::task::open(&app, &name, session.as_deref())?
                }
                TaskCommands::Archive { name, force } => {
                    commands::task::archive(&app, &name, force)?
                }
                TaskCommands::Unarchive { name } => commands::task::unarchive(&app, &name)?,
                TaskCommands::Delete { name, force } => commands::task::delete(&app, &name, force)?,
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
