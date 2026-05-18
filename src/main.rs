mod cli;
mod commands;
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
use context::App;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let app = App::open()?;

    match cli.command {
        Commands::Clone { url } => commands::clone::run(&app, &url),
        Commands::Adopt { path } => commands::adopt::run(&app, &path),
        Commands::Scan => commands::scan::run(&app),
        Commands::Ls { filter } => commands::ls::run(&app, filter.as_deref()),
        Commands::Worktree { command } => match command {
            WorktreeCommands::Add { filter, branch } => {
                commands::worktree::add(&app, &filter, &branch)
            }
            WorktreeCommands::Ls { filter } => commands::worktree::ls_cmd(&app, filter.as_deref()),
            WorktreeCommands::Remove {
                filter,
                branch,
                force,
            } => commands::worktree::remove(&app, &filter, &branch, force),
        },
        Commands::Path { filter, branch } => commands::path::run(&app, &filter, branch.as_deref()),
        Commands::Exec {
            filter,
            branch,
            ephemeral,
        } => commands::exec::run(&app, &filter, branch.as_deref(), ephemeral),
        Commands::InitShell => commands::shell::run_init_shell(),
    }
}
