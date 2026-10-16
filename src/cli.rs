use clap::{Args, Parser, Subcommand};
use clap_complete::engine::ArgValueCandidates;

use crate::completion_candidates::{
    all_branch_candidates, capability_candidates, manager_candidates, parent_candidates,
    project_candidates, repo_filter_candidates, task_candidates, task_worktree_candidates,
};
use crate::manager::budget::{BudgetPart, Change};
use crate::permissions::parse_capability;

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

    /// Check a pull request out into its review, have the review's agent
    /// review it, and open the review viewer (a review is one per pull
    /// request, with a worktree and a session of its own; open, archive and
    /// delete it with `jeet task open|archive|delete <review>`)
    Review {
        /// PR number, with or without a leading #, or the PR's URL
        pr: String,
        /// Repository the PR belongs to; defaults to the one containing the current directory
        #[arg(long, add = ArgValueCandidates::new(repo_filter_candidates))]
        repo: Option<String>,
        /// Only check the PR out: do not ask the review's agent to review it
        #[arg(long, visible_alias = "no-agent")]
        no_command: bool,
        /// Ask for the review again even if this commit was reviewed already
        #[arg(long, conflicts_with = "no_command")]
        rerun: bool,
    },

    /// Pull requests: merge one
    Pr {
        #[command(subcommand)]
        command: PrCommands,
    },

    /// Run the background job recorded on a review (started by `jeet review`)
    #[command(hide = true)]
    ReviewJob { review: String },

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

    /// Check that jeet's own machinery works
    Doctor {
        #[command(subcommand)]
        command: DoctorCommands,
    },

    /// Tasks: a workspace directory with any number of worktrees in it
    Task {
        #[command(subcommand)]
        command: TaskCommands,
    },

    /// Managers: tasks under the assistant that delegate to tasks under them
    Manager {
        #[command(subcommand)]
        command: ManagerCommands,
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

    /// Hand a task's agent its messages as they come (run by the agent)
    #[command(hide = true)]
    Inbox {
        #[command(subcommand)]
        command: InboxCommands,
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

    /// Notify the user, through the backends `[notify] via` names (by
    /// default, Notification Center on macOS); or run the sender that sends
    /// what is queued
    #[command(args_conflicts_with_subcommands = true)]
    Notify {
        #[command(subcommand)]
        command: Option<NotifyCommands>,
        /// The notification's title
        title: Option<String>,
        /// The notification's text
        #[arg(long)]
        body: Option<String>,
        /// Web page opened when the notification is clicked, where the
        /// notifier can (terminal-notifier on macOS)
        #[arg(long)]
        url: Option<String>,
    },

    /// Tell this task's parent how it is going (progress, done, blocked,
    /// question), or answer, pass on and list the questions that reach you
    Report {
        #[command(subcommand)]
        what: ReportCommands,
    },

    /// Print the whole text of a report (r123) or a message (m45) that jeet
    /// gave you as a line or in a digest
    Show {
        /// The reference: r and a report's number, or m and a message's
        #[arg(value_name = "REF")]
        reference: String,
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

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum ReportCommands {
    /// A milestone worth knowing about
    Progress {
        #[arg(allow_hyphen_values = true)]
        message: String,
    },
    /// The work is finished: say where it is
    Done {
        #[arg(allow_hyphen_values = true)]
        message: String,
    },
    /// Stuck: say on what
    Blocked {
        #[arg(allow_hyphen_values = true)]
        message: String,
    },
    /// A decision this task needs from above
    Question {
        #[arg(allow_hyphen_values = true)]
        message: String,
    },
    /// Answer an open question you hold (any in the tree, at your own
    /// terminal); the task that asked gets it, each manager in between a copy
    Answer {
        /// The question's report id, as `jeet report ls` shows it
        id: i64,
        #[arg(allow_hyphen_values = true)]
        message: String,
    },
    /// Pass an open question you hold to your parent; from the assistant,
    /// to the user
    Escalate {
        /// The question's report id
        id: i64,
    },
    /// The open questions you hold (every one in the tree, for the user)
    Ls,
}

#[derive(Subcommand)]
pub enum InboxCommands {
    /// Hand the agent of the session whose start hook runs this what is
    /// queued for it, through its wake channel, until the session ends
    Watch {
        /// The task jeet launched the agent for
        #[arg(long)]
        task: String,
        /// The harness whose hook runs it
        #[arg(long, default_value = "claude-code")]
        harness: String,
    },
}

#[derive(Subcommand)]
pub enum TaskCommands {
    /// Start a task and print its directory; with no --repo it is an empty canvas
    New {
        name: String,
        /// Project to start from: copies its AGENTS.md and skills, and adds a
        /// worktree of each of its repositories (a --repo for one
        /// of them, with no --branch, adds nothing twice); under a manager
        /// with a project, that one when this is not given
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
        #[command(flatten)]
        delegation: Delegation,
    },

    /// List tasks, most recently active first
    Ls {
        /// List archived tasks instead of active ones
        #[arg(long)]
        archived: bool,
    },

    /// Attach to the task's agent in tmux, starting it if needed (prints the
    /// task directory instead of attaching when not interactive); a review's
    /// name works too
    Open {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Resume this agent session instead of the task's latest one
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },

    /// Send the task's agent a message: typed in when it is idle, queued
    /// for when it stops or sits idle when it is busy or waiting on you, its
    /// first prompt when it is not running (`assistant` for the assistant; a
    /// review's name reaches its agent, for you and the task that asked for it)
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

    /// End the task's agent and resume its latest session in a new one, so
    /// it runs on this jeet and keeps its conversation (`assistant`: each of
    /// the assistant's windows). Whatever it left running in the background
    /// (subagents, workflows, /loop schedules) ends with it (a review's name
    /// works too)
    Restart {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Restart even while it is mid-turn, waiting on you, starting, or
        /// has text in its prompt that was not sent
        #[arg(short, long)]
        force: bool,
    },

    /// Vouch for an agent an older jeet started, which jeet has no record
    /// of, so its jeet commands work without restarting it (`assistant`:
    /// the assistant's windows). Run it yourself, from a terminal outside
    /// jeet's tmux sessions
    Adopt {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
    },

    /// Remove a task's worktrees to free the disk, keeping what restores them
    /// (a review's name works too)
    Archive {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Do not ask, even when files git ignores (which are not kept) would go
        #[arg(short, long)]
        force: bool,
        /// Archive every active task under it too, leaves first, asking once
        #[arg(short, long)]
        recursive: bool,
    },

    /// Bring an archived task's worktrees back (a review's name works too)
    Unarchive {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Bring every archived task under it back too, top-down
        #[arg(short, long)]
        recursive: bool,
    },

    /// Delete a task and everything it owns (a review's name works too)
    Delete {
        #[arg(add = ArgValueCandidates::new(task_candidates))]
        name: String,
        /// Do not ask, even when work would be lost
        #[arg(short, long)]
        force: bool,
        /// Delete every task under it too, leaves first, asking once
        #[arg(short, long)]
        recursive: bool,
        /// Move the tasks under it to its parent, then delete it alone
        #[arg(long, conflicts_with = "recursive")]
        reparent: bool,
    },

    /// Print a task's directory (a review's name works too)
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

/// Where a new task sits in the tree, and what it is asked and allowed.
#[derive(Args, Clone, Default)]
pub struct Delegation {
    /// The task it reports to: `assistant`, or a manager by its exact name.
    /// Made by the assistant, a task reports to it without this; made by a
    /// manager, to that manager; a manager the user makes, to the assistant
    #[arg(long, add = ArgValueCandidates::new(parent_candidates))]
    pub parent: Option<String>,
    /// What its parent wants of it, shown to its agent as each session
    /// starts (needs a parent)
    #[arg(long, allow_hyphen_values = true)]
    pub goal: Option<String>,
    /// When it is done, alongside --goal
    #[arg(long = "done", allow_hyphen_values = true)]
    pub done_when: Option<String>,
    /// What it may do (comma-separated capabilities), within what its
    /// parent holds; default `[managers] worker_default`, or
    /// `manager_default` for a manager
    #[arg(long, value_delimiter = ',', value_parser = parse_capability, add = ArgValueCandidates::new(capability_candidates))]
    pub allow: Option<Vec<String>>,
    /// What it may spend: hours (4h), minutes (30m) and turns (20turns),
    /// comma-separated, within what is left above it
    #[arg(long, value_delimiter = ',', value_parser = crate::manager::budget::parse_part)]
    pub budget: Vec<BudgetPart>,
}

#[derive(Subcommand)]
pub enum ManagerCommands {
    /// Make a manager and start its agent: under the assistant, or under
    /// --parent; it reads its brief and delegates to tasks below it
    New {
        name: String,
        #[command(flatten)]
        delegation: Delegation,
        /// A longer brief, read from this file: it follows --goal in the
        /// manager's brief (or, with no --goal, is its goal); the two
        /// together at most 8000 bytes
        #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
        brief_file: Option<std::path::PathBuf>,
        /// How many active tasks it may have under it at once; at most
        /// `[managers] max_children`, its default
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        max_children: Option<u32>,
        /// Project to start from: its skills and AGENTS.md (as PROJECT.md,
        /// beside the manager's own instructions) are copied in, and tasks
        /// made under the manager default to it. No worktrees are added
        #[arg(long, add = ArgValueCandidates::new(project_candidates))]
        project: Option<String>,
    },

    /// The tree under the assistant
    Ls {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// A task under the assistant: its brief, grant, budget and children
    Show {
        #[arg(add = ArgValueCandidates::new(manager_candidates))]
        name: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },

    /// Replace what a task under the assistant may do (comma-separated
    /// capabilities)
    Grant {
        #[arg(add = ArgValueCandidates::new(manager_candidates))]
        name: String,
        #[arg(value_delimiter = ',', required_unless_present = "none", value_parser = parse_capability, add = ArgValueCandidates::new(capability_candidates))]
        caps: Vec<String>,
        /// Grant nothing
        #[arg(long, conflicts_with = "caps")]
        none: bool,
    },

    /// Change what a task under the assistant may spend: `+` raises it
    /// (+1h, +20turns), `=` sets it (=2h,40turns), within what is left
    /// above it
    Budget {
        #[arg(add = ArgValueCandidates::new(manager_candidates))]
        name: String,
        #[arg(value_parser = crate::manager::budget::parse_change)]
        change: Change,
    },

    /// Pause a task under the assistant and everything below it: each
    /// agent's turn is interrupted, and nothing more is handed to them
    Pause {
        #[arg(add = ArgValueCandidates::new(manager_candidates))]
        name: String,
    },

    /// Let a paused or stopped task run again, and hand over what waited
    Resume {
        #[arg(add = ArgValueCandidates::new(manager_candidates))]
        name: String,
    },

    /// Stop a task under the assistant and everything below it: their
    /// agents end, and none starts again until it is resumed
    Stop {
        #[arg(add = ArgValueCandidates::new(manager_candidates), required_unless_present = "all", conflicts_with = "all")]
        name: Option<String>,
        /// Every task under the assistant (only the user)
        #[arg(long)]
        all: bool,
    },

    /// Start what waits on `[managers] max_live`, while there is room (run
    /// by jeet when a task's session ends)
    #[command(hide = true)]
    Drain {
        /// The task whose session is ending: wait a few seconds for it to
        /// go first, as its own slot is the one to fill
        #[arg(long, value_name = "TASK_ID")]
        after: Option<String>,
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
pub enum DoctorCommands {
    /// Show each live session's wake channel: watched, backing off after a
    /// refused post, due a repair, or off, and when it was last resumed to
    /// get a new watcher (read-only)
    Wake,
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
pub enum NotifyCommands {
    /// Send what is queued for the user as it comes, until killed: what
    /// `jeet notify install` keeps running
    Serve,

    /// Keep the sender running in the background: a launchd agent on
    /// macOS, a systemd user unit elsewhere
    Install,

    /// Stop running the sender in the background
    Uninstall,
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
