use clap::{Args, Parser, Subcommand};
use clap_complete::engine::ArgValueCandidates;

use crate::completion_candidates::{
    all_branch_candidates, project_candidates, repo_filter_candidates, task_candidates,
    task_worktree_candidates,
};

#[derive(Parser)]
#[command(
    name = "jeet",
    version,
    about = "Global git repo index and worktree manager"
)]
pub struct Cli {
    /// Omit the subcommand to open the tasks list.
    #[command(subcommand)]
    pub command: Option<Commands>,
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

    /// Open the file explorer for the repository you are in
    Explore,

    /// List previous coding-agent sessions for the current worktree
    Sessions,

    /// List a repository's open pull requests
    Prs {
        /// Repository to list; defaults to the one containing the current directory
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
    },

    /// Check a pull request out into its own worktree and open the review viewer
    Review {
        /// PR number, with or without a leading #, or the PR's URL
        pr: String,
        /// Repository the PR belongs to; defaults to the one containing the current directory
        #[arg(long, add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
        /// Do not start the `[review] command` from config.toml
        #[arg(long)]
        no_command: bool,
        /// Start the review command even if it already reviewed this commit
        #[arg(long, conflicts_with = "no_command")]
        rerun: bool,
    },

    /// Run a background review job (started by `jeet review`)
    #[command(hide = true)]
    ReviewJob { state: std::path::PathBuf },

    /// Fetch every repository in the store, fast-forwarding clean trunks
    Sync {
        /// Sync only this repository
        #[arg(long, add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
    },

    /// Run a background store sync (started by commands that touch the network)
    #[command(hide = true)]
    StoreSync,

    /// The tasks list: open, explore, archive and delete tasks (also plain `jeet`)
    Tasks,

    /// Tasks: a workspace directory with any number of worktrees in it
    Task {
        #[command(subcommand)]
        command: TaskCommands,
    },

    /// Projects: repositories, CLAUDE.md and skills a task can start from
    /// (with no subcommand: the projects panel on a terminal, else the list)
    Projects {
        #[command(subcommand)]
        command: Option<ProjectCommands>,
    },

    /// Record a Claude Code hook event for a task's status (run by the agent)
    #[command(hide = true)]
    TaskHook {
        /// The hook event, e.g. Stop; the hook's JSON arrives on stdin
        #[arg(default_value = "")]
        event: String,
    },

    /// Worktrees of the current task (from $JEET_TASK_ID or the current directory)
    Worktree(WorktreeArgs),

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
        /// Open the worktree for this branch instead of the trunk
        #[arg(long, add = ArgValueCandidates::new(all_branch_candidates))]
        branch: Option<String>,
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
pub enum TaskCommands {
    /// Start a task and print its directory; with no --repo it is an empty canvas
    New {
        name: String,
        /// Project to start from: copies its CLAUDE.md and skills, and adds a
        /// worktree of each of its repositories
        #[arg(long, add = ArgValueCandidates::new(project_candidates))]
        project: Option<String>,
        /// Repository to add a worktree of straight away (repeatable)
        #[arg(long = "repo", value_name = "REPO", add = ArgValueCandidates::new(repo_filter_candidates))]
        repos: Vec<String>,
        /// Branch for the worktree of the single --repo
        #[arg(long, requires = "repos")]
        branch: Option<String>,
    },

    /// List tasks, most recently active first
    Ls {
        /// List archived tasks instead of active ones
        #[arg(long)]
        archived: bool,
    },

    /// Attach to the task's agent in tmux, starting it if needed (prints the
    /// task directory instead of attaching when not interactive)
    Open {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Resume this agent session instead of the task's latest one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },

    /// Remove a task's worktrees to free the disk, keeping what restores them
    Archive {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
    },

    /// Bring an archived task's worktrees back
    Unarchive {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
    },

    /// Delete a task and everything it owns
    Delete {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Do not ask, even when work would be lost
        #[arg(short, long)]
        force: bool,
    },

    /// Print a task's directory
    Path {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
    },

    /// Copy the task's project files (CLAUDE.md, skills) in again
    Resync {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Replace copies the task has changed, too
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommands {
    /// List projects
    Ls,

    /// Make a project and print its directory
    New {
        name: String,
        /// Repository every task from it starts with a worktree of (repeatable)
        #[arg(long = "repo", value_name = "REPO", add = ArgValueCandidates::new(repo_filter_candidates))]
        repos: Vec<String>,
    },

    /// Edit a project's project.toml in your editor
    Edit {
        #[arg(add = ArgValueCandidates::new(project_candidates))]
        name: String,
    },

    /// Delete a project; tasks made from it keep their copies of its files
    #[command(alias = "remove")]
    Rm {
        #[arg(add = ArgValueCandidates::new(project_candidates))]
        name: String,
        /// Do not ask
        #[arg(short, long)]
        force: bool,
    },
}

/// `jeet worktree <op>`, scoped to one task.
#[derive(Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeCommands,

    /// Task to act on; defaults to $JEET_TASK_ID, then the task you are in
    #[arg(long, global = true, add = ArgValueCandidates::new(task_candidates))]
    pub task: Option<String>,
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// Add a worktree of a repository to the task and print its path
    Add {
        /// Repository: a name like `api`, `owner/repo`, an id, or a clone URL
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: String,
        /// Check this branch out (created from the default branch if it is new)
        #[arg(short, long, add = ArgValueCandidates::new(all_branch_candidates))]
        branch: Option<String>,
        /// Directory name inside the task (default: the repository name)
        #[arg(long)]
        name: Option<String>,
    },

    /// List the task's worktrees
    Ls,

    /// Remove a worktree from the task
    #[command(alias = "remove")]
    Rm {
        #[arg(add = ArgValueCandidates::new(task_worktree_candidates))]
        name: String,
        /// Remove it even if it holds uncommitted or unpushed work
        #[arg(short, long)]
        force: bool,
    },

    /// Print a worktree's path
    Path {
        #[arg(add = ArgValueCandidates::new(task_worktree_candidates))]
        name: String,
    },
}
