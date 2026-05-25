use clap::{Parser, Subcommand};
use clap_complete::engine::ArgValueCandidates;

use crate::completion_candidates::{all_branch_candidates, repo_filter_candidates};

#[derive(Parser)]
#[command(
    name = "jeet",
    version,
    about = "Global git repo index and worktree manager"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
      /// Clone a repository into the canonical store
    Clone { url: String },
      /// Register an existing local repository in the index
    Adopt {
         #[arg(value_hint = clap::ValueHint::DirPath)]
        path: String,
      },
      /// Scan configured roots and index git repositories
    Scan,
      /// List indexed repositories
    Ls {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: Option<String>,
      },
      /// Worktree operations
    Worktree {
         #[command(subcommand)]
        command: WorktreeCommands,
      },
      /// Print the filesystem path to a repo trunk or worktree
    Path {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
         #[arg(long, add = ArgValueCandidates::new(all_branch_candidates))]
        branch: Option<String>,
      },
      /// Start a subshell in a repo trunk or worktree
    Exec {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
         #[arg(long, add = ArgValueCandidates::new(all_branch_candidates))]
        branch: Option<String>,
          /// Create a throwaway worktree that is removed when the shell exits
        #[arg(long)]
        ephemeral: bool,
      },
      /// Print shell integration snippet for native cd
    InitShell,
      /// Generate shell completion scripts (bash, zsh, fish, ...)
    Completions {
         #[arg(value_enum)]
        shell: crate::commands::completions::CompletionShell,
      },
      /// Print completion candidates for shell scripts (repos, branches)
    Complete {
         /// Candidate set: repos or branches
        what: String,
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: Option<String>,
      },
      /// Session management - persistent docker-backed workspace sessions
    Session {
         #[command(subcommand)]
        command: SessionCommands,
      },
}

/// Session subcommands
#[derive(Subcommand)]
pub enum SessionCommands {
      /// Create a new session for a repository
    Create {
        name: String,
          #[arg(short, long, help = "Repository to base session on")]
        repo: Option<String>,
          #[arg(short, long, help = "Branch to checkout (default: main)")]
        branch: Option<String>,
      },
      /// List all sessions
    Ls {
          #[arg(short, long, help = "Filter by name or repo")]
        filter: Option<String>,
           #[arg(long, default_value_t = false, help = "Enter interactive mode")]
        interactive: bool,
      },
      /// Enter a session
    Enter {
           #[arg(help = "Session name (required unless --interactive with single session)")]
        name: Option<String>,
           #[arg(long, default_value_t = false, help = "Show list if no name provided")]
        interactive: bool,
      },
      /// Rename a session
    Rename {
        old_name: String,
        new_name: String,
      },
      /// Delete a session
    Delete {
        name: String,
          #[arg(short, long, help = "Force delete without confirmation")]
        force: bool,
      },
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
      /// Create a worktree at the global mirror path
    Add {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
        branch: String,
      },
      /// List worktrees for one or all repos
    Ls {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: Option<String>,
      },
      /// Remove a worktree
    Remove {
         #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
          #[arg(add = ArgValueCandidates::new(all_branch_candidates))]
        branch: String,
          #[arg(long, short)]
        force: bool,
      },
}
