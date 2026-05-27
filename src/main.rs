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

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{Cli, Commands, WorktreeCommands};

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    match cli.command {
        Commands::Completions { shell } => commands::completions::run(shell),
        Commands::Complete { what, filter } => {
            commands::completions::run_complete(&what, filter.as_deref())
        }
        Commands::InitShell => commands::shell::run_init_shell(),
        Commands::InstallShell => commands::install_shell::run(),
        Commands::Clone { url } => {
            let app = context::App::open()?;
            commands::clone::run(&app, &url)
        }
        Commands::Adopt { path } => {
            let app = context::App::open()?;
            commands::adopt::run(&app, &path)
        }
        Commands::Scan => {
            let app = context::App::open()?;
            commands::scan::run(&app)
        }
        Commands::Ls { filter } => {
            let app = context::App::open()?;
            commands::ls::run(&app, filter.as_deref())
        }
        Commands::Path { filter, branch } => {
            let app = context::App::open()?;
            commands::path::run(&app, &filter, branch.as_deref())
        }
        Commands::Exec {
            filter,
            branch,
            ephemeral,
        } => {
            let app = context::App::open()?;
            commands::exec::run(&app, &filter, branch.as_deref(), ephemeral)
        }
        Commands::Worktree { command } => match command {
            WorktreeCommands::Add { filter, branch } => {
                let app = context::App::open()?;
                commands::worktree::add(&app, &filter, &branch)
            }
            WorktreeCommands::Ls { filter } => {
                let app = context::App::open()?;
                commands::worktree::ls_cmd(&app, filter.as_deref())
            }
            WorktreeCommands::Remove {
                filter,
                branch,
                force,
            } => {
                let app = context::App::open()?;
                commands::worktree::remove(&app, &filter, &branch, force)
            }
        },
    }
}
