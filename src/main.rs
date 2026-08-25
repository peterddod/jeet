mod agent;
mod cd;
mod cli;

mod commands;
mod completion_candidates;
mod config;
mod context;
mod db;
mod git;
mod paths;
mod remote;
mod resolve;
mod tui;
mod worktrees;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{Cli, Commands, WorktreeArgs, WorktreeCommands};

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
        Commands::Sessions => {
            let app = context::App::open()?;
            commands::sessions::run(&app)?;
        }
        Commands::Worktree(WorktreeArgs {
            command,
            name,
            repo,
            no_push,
        }) => {
            let app = context::App::open()?;
            match command {
                None => {
                    commands::worktree::create(&app, name.as_deref(), repo.as_deref(), !no_push)?
                }
                Some(WorktreeCommands::Add {
                    filter,
                    branch,
                    push,
                }) => commands::worktree::add(&app, &filter, &branch, push)?,
                Some(WorktreeCommands::Rename {
                    name,
                    new_name,
                    repo,
                    no_push,
                }) => commands::worktree::rename(
                    &app,
                    &name,
                    new_name.as_deref(),
                    repo.as_deref(),
                    !no_push,
                )?,
                Some(WorktreeCommands::Clean {
                    filter,
                    all,
                    force,
                    dry_run,
                    assume_yes,
                }) => commands::worktree::clean(
                    &app,
                    filter.as_deref(),
                    all,
                    force,
                    dry_run,
                    assume_yes,
                )?,
                Some(WorktreeCommands::Ls { filter }) => {
                    commands::worktree::ls_cmd(&app, filter.as_deref())?
                }
                Some(WorktreeCommands::Remove {
                    filter,
                    branch,
                    force,
                }) => commands::worktree::remove(&app, &filter, &branch, force)?,
            }
        }
    }
    Ok(())
}
