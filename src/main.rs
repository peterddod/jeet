mod agent;
mod cd;
mod cli;

mod commands;
mod completion_candidates;
mod config;
mod context;
mod db;
mod git;
mod github;
mod paths;
mod projects;
mod remote;
mod resolve;
mod review;
mod store;
mod tasks;
mod tui;
mod worktrees;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{Cli, Commands, ProjectCommands, TaskCommands, WorktreeArgs, WorktreeCommands};

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    let Some(command) = cli.command else {
        let app = context::App::open()?;
        return commands::explore::run(&app);
    };

    match command {
        Commands::Completions { shell } => commands::completions::run(shell)?,
        Commands::Complete { what } => commands::completions::run_complete(&what, None)?,
        Commands::InitShell => commands::shell::run_init_shell()?,
        Commands::InstallShell => commands::install_shell::run()?,
        Commands::Checkout {
            repo_filter,
            branch_name,
            create_branch,
            start_point,
        } => {
            let app = context::App::open()?;
            commands::checkout::run(
                &app,
                repo_filter.as_deref(),
                branch_name.as_deref(),
                create_branch,
                start_point.as_deref(),
            )?;
        }
        Commands::Clone { url } => {
            let app = context::App::open()?;
            commands::clone::run(&app, &url)?;
        }
        Commands::Adopt { path } => {
            let app = context::App::open()?;
            commands::adopt::run(&app, &path)?;
        }
        Commands::Scan => {
            let app = context::App::open()?;
            commands::scan::run(&app)?;
        }
        Commands::Ls { filter } => {
            let app = context::App::open()?;
            commands::ls::run(&app, filter.as_deref())?;
        }
        Commands::Path { filter, branch } => {
            let app = context::App::open()?;
            commands::path::run(&app, &filter, branch.as_deref())?;
        }
        Commands::Exec {
            filter,
            branch,
            ephemeral,
        } => {
            let app = context::App::open()?;
            commands::exec::run(&app, &filter, branch.as_deref(), ephemeral)?;
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
        Commands::ReviewJob { state } => review::run_job(&state)?,
        Commands::Sync { repo } => {
            let app = context::App::open()?;
            commands::sync::run(&app, repo.as_deref())?;
        }
        Commands::StoreSync => {
            let app = context::App::open()?;
            store::run_background(&app)?;
        }
        Commands::Sessions => {
            let app = context::App::open()?;
            commands::sessions::run(&app)?;
        }
        Commands::Task { command } => {
            let app = context::App::open()?;
            match command {
                TaskCommands::New {
                    name,
                    project,
                    repos,
                    branch,
                } => {
                    commands::task::new(&app, &name, project.as_deref(), &repos, branch.as_deref())?
                }
                TaskCommands::Ls { archived } => commands::task::ls(&app, archived)?,
                TaskCommands::Open { name } => commands::task::open(&app, &name)?,
                TaskCommands::Archive { name } => commands::task::archive(&app, &name)?,
                TaskCommands::Unarchive { name } => commands::task::unarchive(&app, &name)?,
                TaskCommands::Delete { name, force } => commands::task::delete(&app, &name, force)?,
                TaskCommands::Path { name } => commands::task::path(&app, &name)?,
                TaskCommands::Resync { name, force } => commands::task::resync(&app, &name, force)?,
            }
        }
        Commands::Projects { command } => {
            let app = context::App::open()?;
            match command.unwrap_or(ProjectCommands::Ls) {
                ProjectCommands::Ls => commands::projects::ls(&app)?,
                ProjectCommands::New { name, repos } => {
                    commands::projects::new(&app, &name, &repos)?
                }
                ProjectCommands::Edit { name } => commands::projects::edit(&app, &name)?,
                ProjectCommands::Rm { name, force } => commands::projects::rm(&app, &name, force)?,
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
