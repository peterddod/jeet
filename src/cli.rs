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

    /// Worktree operations - manage workspace worktrees for branch testing / isolation
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommands,
    },

    /// Checkout a branch to navigate its workspace - cd into the workspace at that location
    Checkout {
        #[arg(add = ArgValueCandidates::new(all_branch_candidates))]
        branch_name: Option<String>, // Branch name; if omitted, error
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        repo_filter: Option<String>, // Repo filter like "." or "project/"; if omitted, auto-detect from CWD
        #[arg(short, long)]
        create_branch: bool, // Create a new branch (-c, equivalent to git checkout -b)
        #[arg(long)]
        start_point: Option<String>, // Start point for new branch
    },

    /// Print a file path to repository trunk or worktree location
    Path {
        filter: String,
        #[arg(long, add = ArgValueCandidates::new(all_branch_candidates))]
        branch: Option<String>,
    },

    /// Execute an interactive shell in a repo/trunk/worktree workspace (provides CWD)
    Exec {
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String, // Repo path filter like "."
        #[arg(long)]
        ephemeral: bool, // Auto-cleanup on exit?
    },

    /// Print shell integration snippet for native cd command
    InitShell,

    /// Configure shell integration in ~/.zshrc or ~/.bashrc
    InstallShell,

    /// Generate completion scripts for different shells  
    Completions {
        #[arg(value_enum)]
        shell: crate::commands::completions::CompletionShell,
    },

    /// Query & list completion candidates (repos, branches)
    Complete { what: String },
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// Add a new branch as worktree - similar to worktree.add()
    Add {
        filter: String, // Repo path/namne like "."
        branch: String, // Branch name "feature-X"
    },

    /// List all worktrees showing detached branches
    Ls {
        filter: Option<String>, // Repo filter to list just those worktrees
    },

    /// Remove a detached workspace for a branch (disconnect it)  
    Remove {
        filter: String, // Repo path like "."
        #[arg(add = ArgValueCandidates::new(all_branch_candidates))]
        branch: String, // Branch name "main-branch"
        #[arg(short, long)]
        force: bool, // Delete workspace contents?
    },
}
