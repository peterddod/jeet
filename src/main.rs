mod cli;
mod commands;
mod completion_candidates;
mod config;
mod context;
mod db;
mod docker;
mod git;
mod paths;
mod remote;
mod resolve;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use cli::{Cli, Commands, SessionCommands};

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    match cli.command {
        Commands::Completions { shell } => commands::completions::run(shell),
        Commands::Complete { what, filter } => {
            commands::completions::run_complete(&what, filter.as_deref())
        }
        Commands::InitShell => commands::shell::run_init_shell(),
        Commands::Session { command } => {
            let app = context::App::open()?;
            match command {
                SessionCommands::Create { name, repo, branch } => {
                    commands::sessions::create(&app, &name, repo.as_deref(), branch.as_deref())
                 }
                SessionCommands::Ls { filter, interactive } => {
                    commands::sessions::list(&app, filter.as_deref(), interactive)
                 }
                SessionCommands::Enter { name, interactive } => {
                    commands::sessions::enter(&app, name.as_deref(), interactive)
                 }
                SessionCommands::Rename { old_name, new_name } => {
                    commands::sessions::rename(&app, &old_name, &new_name)
                 }
                SessionCommands::Delete { name, force } => {
                    commands::sessions::delete(&app, &name, force)
                 }
              }
          },
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
        Commands::Worktree { command } => {
            let app = context::App::open()?;
            match command {
                cli::WorktreeCommands::Add { filter, branch } => {
                    commands::worktree::add(&app, &filter, &branch)
                 }
                cli::WorktreeCommands::Ls { filter } => {
                    commands::worktree::ls_cmd(&app, filter.as_deref())
                 }
                cli::WorktreeCommands::Remove { filter, branch, force } => {
                    commands::worktree::remove(&app, &filter, &branch, force)
                 }
              }
          }
        Commands::Path { filter, branch } => {
            let app = context::App::open()?;
            commands::path::run(&app, &filter, branch.as_deref())
          }
        Commands::Exec { filter, branch, ephemeral } => {
            let app = context::App::open()?;
            commands::exec::run(&app, &filter, branch.as_deref(), ephemeral)
          }
    }
}
