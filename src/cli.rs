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

    /// Bring repositories, branches and worktrees from outside jeet into the
    /// store and into tasks (scans `scan_roots` when no path is given)
    Import {
        /// Directories to scan for repositories instead of `scan_roots`
        #[arg(value_hint = clap::ValueHint::DirPath)]
        paths: Vec<String>,
        /// Import everything found, one task per branch, without asking
        #[arg(short, long)]
        yes: bool,
        /// Remove each original worktree or branch once its task has a copy
        #[arg(long)]
        remove_originals: bool,
    },

    /// List the repositories in the store
    Ls {
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: Option<String>,
    },

    /// Open the file explorer for the repository or task you are in
    Explore,

    /// List a repository's open pull requests
    Prs {
        /// Repository to list; defaults to the one containing the current directory
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
    },

    /// Check a pull request out into its review task and open the review viewer
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

    /// Pull requests: merge one
    Pr {
        #[command(subcommand)]
        command: PrCommands,
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

    /// Talk to the assistant: resume its most recently active session in
    /// tmux, start a new one, or pick one
    Chat {
        /// Start a new session, called NAME if given
        #[arg(long, value_name = "NAME", num_args = 0..=1, conflicts_with = "pick")]
        new: Option<Option<String>>,
        /// Choose a session: resume, start, rename or end one, or mark the
        /// primary one (without a terminal, list them)
        #[arg(long)]
        pick: bool,
    },

    /// The assistant's todo list, `todo.md` in its home (with no
    /// subcommand: the list)
    Todo {
        #[command(subcommand)]
        command: Option<TodoCommands>,
    },

    /// Tasks: a workspace directory with any number of worktrees in it
    Task {
        #[command(subcommand)]
        command: TaskCommands,
    },

    /// Projects: repositories, AGENTS.md and skills a task can start from
    /// (with no subcommand: the projects panel on a terminal, else the list)
    Projects {
        #[command(subcommand)]
        command: Option<ProjectCommands>,
    },

    /// Record an agent's hook event for a task's status (run by the agent)
    #[command(hide = true)]
    TaskHook {
        /// The hook event, e.g. Stop; the hook's payload arrives on stdin
        #[arg(default_value = "")]
        event: String,
        /// The harness whose hook it is
        #[arg(long, default_value = "claude-code")]
        harness: String,
        /// The task jeet launched the agent for
        #[arg(long)]
        task: Option<String>,
    },

    /// Watch a task's agent start, for a trust prompt (run by jeet)
    #[command(hide = true)]
    TaskStartup {
        /// The task's id
        task: String,
        /// The session jeet started it with
        #[arg(long)]
        session: Option<String>,
    },

    /// Worktrees of the current task (from $JEET_TASK_ID or the current directory)
    Worktree(WorktreeArgs),

    /// Print the path to a repository's trunk in the store
    Path {
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
    },

    /// Start an interactive shell in a repository's trunk
    Exec {
        /// Repository: a name like `api`, `owner/repo`, or an id
        #[arg(add = ArgValueCandidates::new(repo_filter_candidates))]
        filter: String,
    },

    /// Poll GitHub — notifications, your pull requests' checks, the projects
    /// in `[watch]` — and send what is new where `[[assistant.rules]]` say
    /// (with neither --once nor a subcommand: its status)
    #[command(args_conflicts_with_subcommands = true)]
    Watch {
        /// Poll once and exit: what launchd and the tasks list run
        #[arg(long)]
        once: bool,
        #[command(subcommand)]
        command: Option<WatchCommands>,
    },

    /// Raise a desktop notification (Notification Center on macOS)
    Notify {
        title: String,
        /// The notification's text
        #[arg(long)]
        body: Option<String>,
        /// Web page opened when the notification is clicked, where the
        /// notifier can (terminal-notifier on macOS)
        #[arg(long)]
        url: Option<String>,
    },

    /// Tell this task's parent how it is going (progress, done, blocked, question)
    Report {
        #[arg(value_enum)]
        kind: crate::db::tasks::ReportKind,
        #[arg(allow_hyphen_values = true)]
        message: String,
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
        /// Project to start from: copies its AGENTS.md and skills, and adds a
        /// worktree of each of its repositories
        #[arg(long, add = ArgValueCandidates::new(project_candidates))]
        project: Option<String>,
        /// Repository to add a worktree of straight away (repeatable)
        #[arg(long = "repo", value_name = "REPO", add = ArgValueCandidates::new(repo_filter_candidates))]
        repos: Vec<String>,
        /// Branch for the worktree of the single --repo
        #[arg(long, requires = "repos")]
        branch: Option<String>,
        /// Start the task's agent straight away, with this as its first prompt
        #[arg(long, allow_hyphen_values = true)]
        prompt: Option<String>,
        /// The task it reports to: `assistant`, the only one for now. Made by
        /// the assistant, a task reports to it without this
        #[arg(long)]
        parent: Option<String>,
        /// What its parent wants of it, shown to its agent as each session
        /// starts (needs a parent)
        #[arg(long, allow_hyphen_values = true)]
        goal: Option<String>,
        /// When it is done, alongside --goal
        #[arg(long = "done", allow_hyphen_values = true)]
        done_when: Option<String>,
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

    /// Send the task's agent a message: typed in when it is idle, queued
    /// for when it stops or sits idle when it is busy or waiting on you, its
    /// first prompt when it is not running (`assistant` for the assistant)
    Send {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        #[arg(allow_hyphen_values = true)]
        message: String,
    },

    /// End the task agent's current turn, keeping its session: only while
    /// it is working or waiting on you, and not over text you have not sent
    Interrupt {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
    },

    /// Remove a task's worktrees to free the disk, keeping what restores them
    Archive {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Do not ask, even when files git ignores (which are not kept) would go
        #[arg(short, long)]
        force: bool,
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

    /// Look up the task's pull requests on GitHub and list its PR history
    Prs {
        /// Task to list; defaults to $JEET_TASK_ID, then the task you are in
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: Option<String>,
        /// List what is recorded, without asking GitHub
        #[arg(long)]
        offline: bool,
    },

    /// Show what the task's agent last said and how its last turn ended
    Last {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Show its last N messages, oldest first
        #[arg(short = 'n', long = "messages", value_name = "N", default_value_t = 1,
              value_parser = clap::value_parser!(u16).range(1..))]
        count: u16,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Copy the task's project files (AGENTS.md, skills) in again
    Resync {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Replace copies the task has changed, too
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum PrCommands {
    /// Merge a pull request, only when it is open, not a draft, free of
    /// conflicts, its checks have all passed and GitHub does not block it
    Merge {
        /// PR number, with or without a leading #, or the PR's URL
        pr: String,
        /// Repository the PR belongs to; defaults to the PR URL's, then the
        /// one containing the current directory
        #[arg(long, add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
        /// Merge method (default: `[merge] method` in config.toml if the
        /// repository allows it, else the repository's default)
        #[arg(long, value_enum)]
        method: Option<crate::github::MergeMethod>,
        /// Keep the PR's branch on GitHub instead of deleting it
        #[arg(long)]
        keep_branch: bool,
    },
}

#[derive(Subcommand)]
pub enum TodoCommands {
    /// List the items, numbered, most important first
    Ls {
        /// Print them as JSON
        #[arg(long)]
        json: bool,
    },

    /// Add an item, in priority order
    Add {
        /// What there is to do
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Its priority: 1 is the most important (written as `!p1`)
        #[arg(short, long, value_parser = clap::value_parser!(u8).range(1..=9))]
        priority: Option<u8>,
        /// A task it is about (written as `@slug`; repeatable)
        #[arg(long = "task", value_name = "TASK", add = ArgValueCandidates::new(task_candidates))]
        tasks: Vec<String>,
        /// Print the item as JSON
        #[arg(long)]
        json: bool,
    },

    /// Tick items off, by text or number
    Done {
        /// The item's text, or enough of it to tell it apart; or its number
        /// in `jeet todo ls`, which moves as items are added and removed
        #[arg(required = true, value_name = "ITEM")]
        items: Vec<String>,
        /// Put them back as not done
        #[arg(long)]
        undo: bool,
        /// Print the items as JSON
        #[arg(long)]
        json: bool,
    },

    /// Remove items, by text or number
    #[command(alias = "remove")]
    Rm {
        /// The item's text, or enough of it to tell it apart; or its number
        /// in `jeet todo ls`, which moves as items are added and removed
        #[arg(required = true, value_name = "ITEM")]
        items: Vec<String>,
        /// Print the removed items as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum WatchCommands {
    /// Poll every `[watch] interval_secs` in the background: a launchd job
    /// on macOS; elsewhere, print a systemd user timer to set up
    Install,

    /// Stop polling in the background
    Uninstall,

    /// Say whether the watcher is installed, when it last polled and what
    /// it follows
    Status,
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
