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
use clap::Parser;

use cli::{Cli, Commands, WorktreeCommands};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Completions { shell } => commands::completions::run(shell),
        Commands::Complete { what, filter } => {
            commands::completions::run_complete(&what, filter.as_deref())
        }
        Commands::InitShell => commands::shell::run_init_shell(),
        other => {
            let app = context::App::open()?;
            match other {
                Commands::Clone { url } => commands::clone::run(&app, &url),
                Commands::Adopt { path } => commands::adopt::run(&app, &path),
                Commands::Scan => commands::scan::run(&app),
                Commands::Ls { filter } => commands::ls::run(&app, filter.as_deref()),
                Commands::Worktree { command } => match command {
                    WorktreeCommands::Add { filter, branch } => {
                        commands::worktree::add(&app, &filter, &branch)
                    }
                    WorktreeCommands::Ls { filter } => {
                        commands::worktree::ls_cmd(&app, filter.as_deref())
                    }
                    WorktreeCommands::Remove {
                        filter,
                        branch,
                        force,
                    } => commands::worktree::remove(&app, &filter, &branch, force),
                },
                Commands::Path { filter, branch } => {
                    commands::path::run(&app, &filter, branch.as_deref())
                }
                Commands::Exec {
                    filter,
                    branch,
                    ephemeral,
                } => commands::exec::run(&app, &filter, branch.as_deref(), ephemeral),
                Commands::InitShell | Commands::Completions { .. } | Commands::Complete { .. } => {
                    unreachable!()
                }
            }
        }
    }
}
