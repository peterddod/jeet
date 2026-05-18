use clap::{Parser, Subcommand};

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
    Adopt { path: String },
    /// Scan configured roots and index git repositories
    Scan,
    /// List indexed repositories
    Ls { filter: Option<String> },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommands,
    },
    /// Print the filesystem path to a repo trunk or worktree
    Path {
        filter: String,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Start a subshell in a repo trunk or worktree
    Exec {
        filter: String,
        #[arg(long)]
        branch: Option<String>,
        /// Create a throwaway worktree that is removed when the shell exits
        #[arg(long)]
        ephemeral: bool,
    },
    /// Print shell integration snippet for native cd
    InitShell,
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// Create a worktree at the global mirror path
    Add { filter: String, branch: String },
    /// List worktrees for one or all repos
    Ls { filter: Option<String> },
    /// Remove a worktree
    Remove {
        filter: String,
        branch: String,
        #[arg(long, short)]
        force: bool,
    },
}
