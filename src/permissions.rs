//! What the assistant may do, enforced by jeet rather than by its prompt.
//!
//! The assistant's session runs with `JEET_ASSISTANT=1`. Every jeet command
//! it runs is classified here before it does anything: free (it only reads,
//! or is jeet's own plumbing), interactive (it needs a person at a
//! terminal), or one that needs a capability. A capability is used only when
//! `[assistant] allow` in config.toml lists it; otherwise the command fails
//! with a message the assistant can pass on, so it asks the user instead.
//!
//! Each mutating action it is allowed is appended to
//! `$JEET_HOME/assistant/actions.log` (one JSON object per line) and, unless
//! `[assistant] notify = false`, announced with a desktop notification.
//! Refusals are logged too.
//!
//! Every command must be classified: [`gate`] matches them exhaustively, so a
//! new one does not compile until it says what it needs.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{
    Commands, PrCommands, ProjectCommands, TaskCommands, WatchCommands, WorktreeArgs,
    WorktreeCommands,
};
use crate::config;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::db::Database;
use crate::notify::{self, Notification};
use crate::tasks::TASK_ID_ENV;
use crate::worktrees::now_secs;

/// Set in the assistant's session; anything but empty or `0` turns the
/// checks on.
pub const ASSISTANT_ENV: &str = "JEET_ASSISTANT";

/// One thing the assistant can be allowed to do.
pub struct Capability {
    pub name: &'static str,
    /// What it lets the assistant do, to finish "the assistant may not …".
    pub about: &'static str,
    /// The command lines that use it, pre-approved in the assistant's
    /// harness when it is allowed.
    pub commands: &'static [&'static str],
}

pub const CAPABILITIES: &[Capability] = &[
    cap("task.create", "create tasks", &["jeet task new"]),
    cap("task.open", "start task agents", &["jeet task open"]),
    cap("task.send", "message task agents", &["jeet task send"]),
    cap("task.archive", "archive tasks", &["jeet task archive"]),
    cap(
        "task.unarchive",
        "unarchive tasks",
        &["jeet task unarchive"],
    ),
    cap("task.delete", "delete tasks", &["jeet task delete"]),
    cap(
        "task.resync",
        "copy project files into tasks again",
        &["jeet task resync"],
    ),
    cap(
        "worktree.add",
        "add worktrees to tasks",
        &["jeet worktree add"],
    ),
    cap(
        "worktree.rm",
        "remove worktrees from tasks",
        &["jeet worktree rm"],
    ),
    cap(
        "review.start",
        "check pull requests out for review",
        &["jeet review"],
    ),
    cap("pr.comment", "comment on pull requests", &[]),
    cap("pr.review", "submit pull request reviews", &[]),
    cap("pr.merge", "merge pull requests", &["jeet pr merge"]),
    cap("project.read", "list projects", &["jeet projects ls"]),
    cap("project.create", "make projects", &["jeet projects new"]),
    cap("project.delete", "delete projects", &["jeet projects rm"]),
    cap(
        "repo.clone",
        "clone repositories into the store",
        &["jeet clone"],
    ),
    cap(
        "repo.import",
        "import repositories and work into jeet",
        &["jeet import"],
    ),
    cap("repo.sync", "fetch the store", &["jeet sync"]),
    cap(
        "shell.install",
        "change your shell's startup files",
        &["jeet install-shell"],
    ),
    cap(
        "watch.install",
        "install or remove the background GitHub watcher",
        &["jeet watch install", "jeet watch uninstall"],
    ),
];

const fn cap(
    name: &'static str,
    about: &'static str,
    commands: &'static [&'static str],
) -> Capability {
    Capability {
        name,
        about,
        commands,
    }
}

/// Commands that only read, pre-approved whatever is allowed.
const READ_ONLY: &[&str] = &[
    "jeet ls",
    "jeet prs",
    "jeet path",
    "jeet task ls",
    "jeet task path",
    "jeet task prs",
    "jeet task last",
    "jeet worktree ls",
    "jeet worktree path",
    "jeet notify",
];

/// The assistant's own files, which it keeps through jeet: pre-approved
/// too, and needing no capability.
const OWN: &[&str] = &["jeet todo"];

pub fn capability(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|c| c.name == name)
}

/// Whether this jeet runs for the assistant.
pub fn assistant_mode() -> bool {
    std::env::var(ASSISTANT_ENV).is_ok_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0"
    })
}

/// Who runs this jeet, as far as what it may do goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A person, or an agent of a top-level task: not gated.
    User,
    /// The assistant, the root of the tree: gated by `[assistant] allow`.
    Root,
    /// The agent of a task with a parent: gated by [`effective`], and only
    /// over itself.
    Node(TaskRecord),
}

/// Who `row`, the task `$JEET_TASK_ID` names, makes the caller. A task with
/// a parent is a Node even under `JEET_ASSISTANT=1`: the narrower reading
/// wins.
fn classify(row: Option<TaskRecord>, assistant: bool) -> Caller {
    match row {
        Some(task) if task.parent_id.is_some() => Caller::Node(task),
        _ if assistant => Caller::Root,
        _ => Caller::User,
    }
}

/// Who runs this jeet: by `$JEET_TASK_ID` and `JEET_ASSISTANT` only, never
/// the working directory. An id jeet does not know names no Node. A
/// database error is an error, so what is gated fails closed.
pub fn caller(db: &Database) -> Result<Caller> {
    let row = match std::env::var(TASK_ID_ENV) {
        Ok(id) if !id.trim().is_empty() => db.get_task(id.trim())?,
        _ => None,
    };
    Ok(classify(row, assistant_mode()))
}

/// What a task with a parent may be allowed, before its parent's list cuts
/// it down. Phase 2 keeps a grant per task instead (`task_grants`).
pub const NODE_GRANT: &[&str] = &["worktree.add"];

/// The capabilities of `grant` that `allow` lists.
fn intersect(grant: &[&'static str], allow: &[String]) -> Vec<&'static str> {
    grant
        .iter()
        .copied()
        .filter(|g| allow.iter().any(|a| a.trim() == *g))
        .collect()
}

/// What `node` may do: [`NODE_GRANT`] ∩ `[assistant] allow`, so a child
/// never holds what its parent lacks. Nothing when its parent is not the
/// assistant, the only parent there is for now.
pub fn effective(app: &App, node: &TaskRecord) -> Result<Vec<&'static str>> {
    let parent = match &node.parent_id {
        Some(id) => app.db.get_task(id)?,
        None => None,
    };
    Ok(match parent {
        Some(parent) if parent.kind == TaskKind::Assistant => {
            intersect(NODE_GRANT, &app.config.assistant.allow)
        }
        _ => Vec::new(),
    })
}

/// What running a command takes, for the assistant.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Reads, or is jeet's own plumbing.
    Free,
    /// A screen or a shell for a person to use.
    Interactive,
    /// Needs `capability`. `action` says what was done, for the log, in the
    /// past tense; `None` for a read, which is not logged.
    Needs {
        capability: &'static str,
        action: Option<String>,
    },
}

fn needs(capability: &'static str, action: String) -> Gate {
    Gate::Needs {
        capability,
        action: Some(action),
    }
}

/// Classify `command` (`None`: bare `jeet`). `terminal` says whether one is
/// attached, which decides what `jeet projects` does.
pub fn gate(command: Option<&Commands>, terminal: bool) -> Gate {
    let Some(command) = command else {
        return Gate::Interactive;
    };
    match command {
        Commands::Completions { .. }
        | Commands::Complete { .. }
        | Commands::InitShell
        | Commands::Ls { .. }
        | Commands::Path { .. }
        | Commands::Prs { .. }
        | Commands::Notify { .. }
        // Only ever to the caller's own parent; the command checks who that is.
        | Commands::Report { .. }
        // Its own todo list.
        | Commands::Todo { .. }
        // Started by jeet itself, under the command that was checked.
        | Commands::ReviewJob { .. }
        | Commands::StoreSync
        | Commands::TaskHook { .. }
        | Commands::TaskStartup { .. } => Gate::Free,
        Commands::Tasks | Commands::Explore | Commands::Exec { .. } | Commands::Chat { .. } => {
            Gate::Interactive
        }
        Commands::InstallShell => needs("shell.install", "installed shell integration".into()),
        // A poll hands events on as the user's rules say; installing one
        // changes what runs on the user's machine.
        Commands::Watch { command, .. } => match command {
            None | Some(WatchCommands::Status) => Gate::Free,
            Some(WatchCommands::Install) => {
                needs("watch.install", "installed the GitHub watcher".into())
            }
            Some(WatchCommands::Uninstall) => {
                needs("watch.install", "removed the GitHub watcher".into())
            }
        },
        Commands::Clone { url } => needs("repo.clone", format!("cloned {url}")),
        Commands::Import { paths, .. } => needs(
            "repo.import",
            match paths.is_empty() {
                true => "imported from the scan roots".into(),
                false => format!("imported from {}", paths.join(", ")),
            },
        ),
        Commands::Sync { repo } => needs(
            "repo.sync",
            match repo {
                Some(repo) => format!("fetched {repo}"),
                None => "fetched the store".into(),
            },
        ),
        Commands::Review { pr, repo, .. } => needs(
            "review.start",
            format!("checked out {} for review", pr_label(pr, repo.as_deref())),
        ),
        Commands::Pr { command } => match command {
            PrCommands::Merge { pr, repo, .. } => {
                needs("pr.merge", format!("merged {}", pr_label(pr, repo.as_deref())))
            }
        },
        Commands::Task { command } => task_gate(command),
        Commands::Projects { command } => match command {
            None if terminal => Gate::Interactive,
            None | Some(ProjectCommands::Ls) => Gate::Needs {
                capability: "project.read",
                action: None,
            },
            Some(ProjectCommands::New { name, .. }) => {
                needs("project.create", format!("made project {name}"))
            }
            // Opens an editor, for a person.
            Some(ProjectCommands::Edit { .. }) => Gate::Interactive,
            Some(ProjectCommands::Rm { name, .. }) => {
                needs("project.delete", format!("deleted project {name}"))
            }
        },
        Commands::Worktree(WorktreeArgs { command, task }) => {
            let of = task
                .as_ref()
                .map(|t| format!(" of task {t}"))
                .unwrap_or_default();
            match command {
                WorktreeCommands::Ls | WorktreeCommands::Path { .. } => Gate::Free,
                WorktreeCommands::Add { repo, branch, .. } => {
                    let on = branch
                        .as_ref()
                        .map(|b| format!(" on {b}"))
                        .unwrap_or_default();
                    needs("worktree.add", format!("added a worktree of {repo}{on}{of}"))
                }
                WorktreeCommands::Rm { name, .. } => {
                    needs("worktree.rm", format!("removed worktree {name}{of}"))
                }
            }
        }
    }
}

fn task_gate(command: &TaskCommands) -> Gate {
    match command {
        TaskCommands::Ls { .. }
        | TaskCommands::Path { .. }
        | TaskCommands::Prs { .. }
        | TaskCommands::Last { .. } => Gate::Free,
        TaskCommands::New {
            name,
            project,
            repos,
            prompt,
            ..
        } => {
            let mut from: Vec<String> = repos.clone();
            if let Some(project) = project {
                from.insert(0, format!("project {project}"));
            }
            let mut action = format!("created task {name}");
            if !from.is_empty() {
                action.push_str(&format!(" ({})", from.join(", ")));
            }
            if prompt.is_some() {
                action.push_str(" and started its agent");
            }
            needs("task.create", action)
        }
        TaskCommands::Open { name, .. } => needs("task.open", format!("opened task {name}")),
        TaskCommands::Send { name, message } => needs(
            "task.send",
            format!("messaged task {name}: {}", first_line(message, 80)),
        ),
        TaskCommands::Archive { name, .. } => {
            needs("task.archive", format!("archived task {name}"))
        }
        TaskCommands::Unarchive { name } => {
            needs("task.unarchive", format!("unarchived task {name}"))
        }
        TaskCommands::Delete { name, .. } => needs("task.delete", format!("deleted task {name}")),
        TaskCommands::Resync { name, .. } => needs(
            "task.resync",
            format!("copied project files into task {name} again"),
        ),
    }
}

/// `#42 of acme/api`: a PR as the user typed it, for the log.
fn pr_label(pr: &str, repo: Option<&str>) -> String {
    let pr = match pr.trim_start_matches('#').parse::<u64>() {
        Ok(number) => format!("#{number}"),
        Err(_) => pr.to_string(),
    };
    let repo = repo.map(|r| format!(" of {r}")).unwrap_or_default();
    format!("{pr}{repo}")
}

/// The first non-blank line of `text`, cut to `max` characters.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    let line = line.unwrap_or_default();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Why `allow` does not let the assistant use `capability`, or nothing when
/// it does.
pub fn refusal(capability: &str, allow: &[String], config_path: &Path) -> Option<String> {
    if allow.iter().any(|a| a.trim() == capability) {
        return None;
    }
    let about = self::capability(capability).map_or("do that", |c| c.about);
    let mut message = format!(
        "the assistant may not {about}: `{capability}` is not in `[assistant] allow` in {}; \
         ask the user to do it, or to allow it there",
        config_path.display()
    );
    let unknown: Vec<&str> = allow
        .iter()
        .map(|a| a.trim())
        .filter(|a| self::capability(a).is_none())
        .collect();
    if !unknown.is_empty() {
        message.push_str(&format!(
            " (it also lists {}, which jeet does not know)",
            unknown.join(", ")
        ));
    }
    Some(message)
}

/// The command prefixes the assistant's harness pre-approves: the read-only
/// jeet commands, its own, and the commands of each allowed capability.
pub fn allowed_commands(allow: &[String]) -> Vec<String> {
    let allowed = CAPABILITIES
        .iter()
        .filter(|c| allow.iter().any(|a| a.trim() == c.name))
        .flat_map(|c| c.commands.iter());
    READ_ONLY
        .iter()
        .chain(OWN)
        .chain(allowed)
        .map(|command| command.to_string())
        .collect()
}

/// An action the assistant was allowed, to record once it has run.
pub struct Authorized {
    capability: &'static str,
    action: String,
    log: PathBuf,
    notify: bool,
}

/// Check that the assistant may run `command`, before it does anything.
/// Outside the assistant everything is allowed and nothing is recorded.
/// Returns the action to [`Authorized::finish`] when it is one the log
/// keeps.
///
/// The agent of a task with a parent is checked as a Node instead, whether
/// or not it runs as the assistant: see [`node`].
pub fn authorize(command: Option<&Commands>) -> Result<Option<Authorized>> {
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let gate = gate(command, terminal);
    // Only what a Node may not do opens the index: plumbing like hooks and
    // `jeet report` never does.
    let in_task = std::env::var(TASK_ID_ENV).is_ok_and(|v| !v.trim().is_empty());
    if in_task && (gate != Gate::Free || node_only(command).is_some()) {
        let app = App::open().context("check what this task may do")?;
        if let Caller::Node(task) = caller(&app.db)? {
            return node(&app, &task, command, gate, terminal).map(|()| None);
        }
    }
    if !assistant_mode() {
        return Ok(None);
    }
    if let Some(Commands::ReviewJob { state }) = command {
        check_review_job(state)?;
    }
    let (capability, action) = match gate {
        Gate::Free => return Ok(None),
        Gate::Interactive if terminal => return Ok(None),
        Gate::Interactive => bail!(
            "that opens a screen or shell for a person at a terminal, which the assistant \
             cannot use; ask the user to run it"
        ),
        Gate::Needs { capability, action } => (capability, action),
    };
    let home = config::jeet_home()?;
    let settings = config::load_or_create(&home)?.assistant;
    let log = actions_log(&home);
    let config_path = config::config_path(&home);
    if let Some(message) = refusal(capability, &settings.allow, &config_path) {
        let about = self::capability(capability).map_or("", |c| c.about);
        let tried = action.unwrap_or_else(|| about.to_string());
        if let Err(e) = append(&log, capability, &tried, Outcome::Denied, None) {
            eprintln!("jeet: warning: could not write {}: {e:#}", log.display());
        }
        bail!(message);
    }
    Ok(action.map(|action| Authorized {
        capability,
        action,
        log,
        notify: settings.notify(),
    }))
}

/// What `command` does that only the root may, for "task … may not …":
/// reaching the user, and the assistant's own list.
fn node_only(command: Option<&Commands>) -> Option<&'static str> {
    match command? {
        Commands::Notify { .. } => Some("notify the user"),
        Commands::Todo { .. } => Some("keep the assistant's todo list"),
        // Only `jeet review` starts one, and a Node may not review.
        Commands::ReviewJob { .. } => Some("run review jobs"),
        _ => None,
    }
}

/// Check that `node`, a task with a parent, may run `command`: only what
/// its parent may too ([`effective`]), and only over itself. Nothing is
/// logged or announced; a refusal says to ask the parent.
fn node(
    app: &App,
    node: &TaskRecord,
    command: Option<&Commands>,
    gate: Gate,
    terminal: bool,
) -> Result<()> {
    let slug = &node.slug;
    if let Some(about) = node_only(command) {
        bail!(
            "task {slug} may not {about}: a task with a parent tells its parent with \
             `jeet report progress|done|blocked|question \"…\"`"
        );
    }
    let capability = match gate {
        Gate::Free => return Ok(()),
        Gate::Interactive if terminal => return Ok(()),
        Gate::Interactive => bail!(
            "that opens a screen or shell for a person at a terminal, which task {slug}'s \
             agent cannot use; ask with `jeet report blocked \"…\"`"
        ),
        Gate::Needs { capability, .. } => capability,
    };
    let about = self::capability(capability).map_or("do that", |c| c.about);
    if !NODE_GRANT.contains(&capability) {
        let mut message = node_refusal(slug, about, None);
        if let Some(Commands::Task {
            command: TaskCommands::Send { .. },
        }) = command
        {
            message.push_str(" (tasks never message each other)");
        }
        bail!(message);
    }
    if !effective(app, node)?.contains(&capability) {
        let path = config::config_path(&app.home);
        bail!(node_refusal(slug, about, Some((capability, &path))));
    }
    node_scope(app, node, command)
}

/// What a Node may change: only itself, and only by adding worktrees.
/// Anything else that gets this far is refused, so a new command fails
/// closed until it says.
fn node_scope(app: &App, node: &TaskRecord, command: Option<&Commands>) -> Result<()> {
    match command {
        Some(Commands::Worktree(WorktreeArgs {
            task,
            command: WorktreeCommands::Add { .. },
        })) => {
            // As the command resolves it, so neither a prefix nor the
            // working directory can name another task.
            let target = crate::tasks::current(app, task.as_deref())?;
            if target.id != node.id {
                bail!(
                    "task {} may add worktrees only to itself, not to task {}; no task may \
                     change another's work",
                    node.slug,
                    target.slug
                );
            }
            Ok(())
        }
        _ => bail!("jeet has no scope rule for that command for a task with a parent"),
    }
}

/// Why `slug` may not do `about`: outside what any task with a parent may
/// (`parent_lacks` None), or because its parent lacks the capability, set
/// in the config file at the path.
fn node_refusal(slug: &str, about: &str, parent_lacks: Option<(&str, &Path)>) -> String {
    match parent_lacks {
        None => format!(
            "task {slug} may not {about}: a task with a parent may only add worktrees to \
             itself (when its parent may) and report; ask with \
             `jeet report blocked|question \"…\"`"
        ),
        Some((capability, path)) => format!(
            "task {slug} may not {about}: its parent, the assistant, does not hold \
             `{capability}` ([assistant] allow in {}); ask with `jeet report blocked \"…\"`",
            path.display()
        ),
    }
}

impl Authorized {
    /// Record how the action went, and announce it when it was done.
    pub fn finish(self, result: &Result<()>) {
        let (outcome, error) = match result {
            Ok(()) => (Outcome::Done, None),
            Err(e) => (Outcome::Failed, Some(format!("{e:#}"))),
        };
        if let Err(e) = append(
            &self.log,
            self.capability,
            &self.action,
            outcome,
            error.as_deref(),
        ) {
            eprintln!(
                "jeet: warning: could not write {}: {e:#}",
                self.log.display()
            );
        }
        if outcome == Outcome::Done && self.notify {
            notify::notify(&Notification {
                title: "Assistant",
                subtitle: None,
                body: &self.action,
                url: None,
            });
        }
    }
}

/// A review job runs the command its state file names, so the assistant
/// may only run the ones `jeet review` wrote, under `$JEET_HOME/reviews` —
/// not one it wrote itself.
fn check_review_job(state: &Path) -> Result<()> {
    let home = config::jeet_home()?;
    let reviews = home.join("reviews");
    let reviews = reviews.canonicalize().unwrap_or(reviews);
    let state = state
        .canonicalize()
        .with_context(|| format!("find {}", state.display()))?;
    if !state.starts_with(&reviews) {
        bail!(
            "the assistant may only run review jobs jeet started, under {}",
            reviews.display()
        );
    }
    Ok(())
}

pub fn actions_log(home: &Path) -> PathBuf {
    config::assistant_dir(home).join("actions.log")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Denied,
}

/// One line of `actions.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    pub at: i64,
    pub capability: String,
    pub action: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Append one record to the log, as a single line in a single write so two
/// jeets writing at once do not interleave.
fn append(
    log: &Path,
    capability: &str,
    action: &str,
    outcome: Outcome,
    error: Option<&str>,
) -> Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let record = ActionRecord {
        at: now_secs(),
        capability: capability.to_string(),
        action: action.to_string(),
        outcome,
        error: error.map(str::to_string),
    };
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("open {}", log.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("write {}", log.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn gate_of(args: &[&str]) -> Gate {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied()))
                .unwrap();
        gate(cli.command.as_ref(), false)
    }

    fn needs_of(args: &[&str]) -> (&'static str, Option<String>) {
        match gate_of(args) {
            Gate::Needs { capability, action } => (capability, action),
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn mutating_commands_need_their_capability() {
        let cases: &[(&[&str], &str, &str)] = &[
            (
                &["task", "new", "fix-login", "--repo", "api"],
                "task.create",
                "created task fix-login (api)",
            ),
            (
                &["task", "new", "x", "--project", "web", "--prompt", "go"],
                "task.create",
                "created task x (project web) and started its agent",
            ),
            (
                &["task", "send", "fix-x", "also add tests\nand docs"],
                "task.send",
                "messaged task fix-x: also add tests",
            ),
            (&["task", "archive", "x"], "task.archive", "archived task x"),
            (
                &["task", "unarchive", "x"],
                "task.unarchive",
                "unarchived task x",
            ),
            (
                &["task", "delete", "x", "-f"],
                "task.delete",
                "deleted task x",
            ),
            (&["task", "open", "x"], "task.open", "opened task x"),
            (
                &["worktree", "add", "api", "-b", "feat", "--task", "x"],
                "worktree.add",
                "added a worktree of api on feat of task x",
            ),
            (
                &["worktree", "rm", "api"],
                "worktree.rm",
                "removed worktree api",
            ),
            (
                &["review", "#42", "--repo", "acme/api"],
                "review.start",
                "checked out #42 of acme/api for review",
            ),
            (
                &[
                    "pr", "merge", "42", "--repo", "acme/api", "--method", "squash",
                ],
                "pr.merge",
                "merged #42 of acme/api",
            ),
            (
                &["pr", "merge", "https://github.com/acme/api/pull/7"],
                "pr.merge",
                "merged https://github.com/acme/api/pull/7",
            ),
            (
                &["clone", "https://x/y.git"],
                "repo.clone",
                "cloned https://x/y.git",
            ),
            (&["sync"], "repo.sync", "fetched the store"),
            (
                &["projects", "new", "web"],
                "project.create",
                "made project web",
            ),
            (
                &["projects", "rm", "web"],
                "project.delete",
                "deleted project web",
            ),
            (
                &["install-shell"],
                "shell.install",
                "installed shell integration",
            ),
            (
                &["watch", "install"],
                "watch.install",
                "installed the GitHub watcher",
            ),
            (
                &["watch", "uninstall"],
                "watch.install",
                "removed the GitHub watcher",
            ),
        ];
        for (args, capability, action) in cases {
            assert_eq!(
                needs_of(args),
                (*capability, Some(action.to_string())),
                "{args:?}"
            );
            assert!(self::capability(capability).is_some(), "{capability}");
        }
        // Listing projects needs leave, but is not an action to log.
        assert_eq!(needs_of(&["projects", "ls"]), ("project.read", None));
        assert_eq!(needs_of(&["projects"]), ("project.read", None));
    }

    #[test]
    fn reads_and_plumbing_are_free_and_screens_are_for_people() {
        for args in [
            &["ls"][..],
            &["task", "ls"],
            &["task", "path", "x"],
            &["task", "prs"],
            &["task", "last", "x"],
            &["task", "last", "x", "-n", "3", "--json"],
            &["worktree", "ls"],
            &["prs"],
            &["notify", "hi"],
            &["task-hook", "Stop"],
            &["store-sync"],
            &["todo"],
            &["todo", "add", "x"],
            &["todo", "done", "1"],
            &["watch", "--once"],
            &["watch", "status"],
            &["watch"],
        ] {
            assert_eq!(gate_of(args), Gate::Free, "{args:?}");
        }
        for args in [
            &["tasks"][..],
            &["explore"],
            &["exec", "api"],
            &["projects", "edit", "w"],
            &["chat"],
            &["chat", "--new", "plan"],
        ] {
            assert_eq!(gate_of(args), Gate::Interactive, "{args:?}");
        }
        assert_eq!(gate(None, false), Gate::Interactive);
        let cli = crate::cli::Cli::try_parse_from(["jeet", "projects"]).unwrap();
        assert_eq!(gate(cli.command.as_ref(), true), Gate::Interactive);
    }

    #[test]
    fn reporting_is_free() {
        for kind in ["progress", "done", "blocked", "question"] {
            assert_eq!(gate_of(&["report", kind, "x"]), Gate::Free, "{kind}");
        }
        // jeet's own kind is not the task's to send.
        assert!(crate::cli::Cli::try_parse_from(["jeet", "report", "status", "x"]).is_err());
    }

    fn task(parent_id: Option<&str>) -> TaskRecord {
        TaskRecord {
            id: "t".into(),
            slug: "t".into(),
            name: "t".into(),
            project: None,
            kind: TaskKind::Work,
            state: crate::db::tasks::TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: parent_id.map(Into::into),
        }
    }

    #[test]
    fn a_task_with_a_parent_is_a_node_whatever_else_it_says() {
        let child = task(Some("a"));
        assert_eq!(
            classify(Some(child.clone()), true),
            Caller::Node(child.clone())
        );
        assert_eq!(classify(Some(child.clone()), false), Caller::Node(child));
        assert_eq!(classify(Some(task(None)), true), Caller::Root);
        assert_eq!(classify(Some(task(None)), false), Caller::User);
        assert_eq!(classify(None, true), Caller::Root);
        assert_eq!(classify(None, false), Caller::User);
    }

    #[test]
    fn a_node_holds_only_what_its_grant_and_its_parent_both_do() {
        assert!(intersect(&["worktree.add"], &[]).is_empty());
        assert_eq!(
            intersect(
                &["worktree.add"],
                &[" worktree.add ".into(), "task.send".into()]
            ),
            ["worktree.add"]
        );
        // Never anything outside the grant, whatever the parent holds.
        let every: Vec<String> = CAPABILITIES.iter().map(|c| c.name.to_string()).collect();
        assert_eq!(intersect(NODE_GRANT, &every), NODE_GRANT);
    }

    /// An index in a tempdir with the assistant, `w` and `sib` under it, and
    /// `top` with no parent.
    fn tree() -> (tempfile::TempDir, App, TaskRecord) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: config::load_or_create(&home).unwrap(),
            db: Database::open(&config::index_path(&home)).unwrap(),
            home,
        };
        let assistant = crate::assistant::ensure(&app).unwrap();
        for (slug, parent) in [("w", Some(&assistant.id)), ("sib", Some(&assistant.id))]
            .into_iter()
            .chain([("top", None)])
        {
            let task = TaskRecord {
                id: format!("{slug}-id"),
                slug: slug.into(),
                name: slug.into(),
                ..task(parent.map(String::as_str))
            };
            app.db.insert_task(&task).unwrap();
        }
        let w = app.db.get_task("w-id").unwrap().unwrap();
        (dir, app, w)
    }

    fn node_of(app: &App, w: &TaskRecord, args: &[&str], terminal: bool) -> Result<()> {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied()))
                .unwrap();
        let command = cli.command.as_ref();
        node(app, w, command, gate(command, terminal), terminal)
    }

    #[test]
    fn a_node_adds_worktrees_to_itself_only_when_its_parent_may() {
        let (_dir, mut app, w) = tree();
        let add = ["worktree", "add", "r", "--task", "w"];
        let refused = node_of(&app, &w, &add, false).unwrap_err().to_string();
        assert!(refused.contains("`worktree.add`"), "{refused}");
        assert!(refused.contains("[assistant] allow"), "{refused}");

        app.config.assistant.allow = vec!["worktree.add".into()];
        node_of(&app, &w, &add, false).unwrap();
        for other in ["sib", "top"] {
            let refused = node_of(&app, &w, &["worktree", "add", "r", "--task", other], false)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("only to itself"), "{other}: {refused}");
        }
        // Scope is checked directly too, and anything without a rule fails.
        let cli = crate::cli::Cli::try_parse_from(["jeet", "task", "archive", "sib"]).unwrap();
        assert!(node_scope(&app, &w, cli.command.as_ref()).is_err());

        // A parent that is not the assistant passes on nothing.
        let under_top = TaskRecord {
            id: "u-id".into(),
            slug: "u".into(),
            name: "u".into(),
            ..task(Some("top-id"))
        };
        app.db.insert_task(&under_top).unwrap();
        let add = ["worktree", "add", "r", "--task", "u"];
        assert!(node_of(&app, &under_top, &add, false).is_err());
    }

    #[test]
    fn a_node_may_not_change_anything_else_whatever_its_parent_holds() {
        let (_dir, mut app, w) = tree();
        app.config.assistant.allow = CAPABILITIES.iter().map(|c| c.name.into()).collect();
        let refused: &[&[&str]] = &[
            &["worktree", "rm", "r", "--task", "w"],
            &["task", "send", "sib", "x"],
            &["task", "send", "assistant", "x"],
            &["task", "archive", "sib"],
            &["task", "unarchive", "sib"],
            &["task", "delete", "sib"],
            &["task", "open", "sib"],
            &["task", "resync", "sib"],
            &["task", "new", "x"],
            &["clone", "https://example.com/a/b"],
            &["sync"],
            &["review", "42"],
            &["pr", "merge", "42"],
            &["projects", "new", "p"],
            &["import"],
            &["watch", "install"],
            &["notify", "t"],
            &["todo", "add", "x"],
            &["todo", "ls"],
            &["review-job", "s"],
            &["tasks"],
        ];
        for args in refused {
            let message = node_of(&app, &w, args, false).unwrap_err().to_string();
            assert!(message.contains("jeet report"), "{args:?}: {message}");
        }
        let send = node_of(&app, &w, &["task", "send", "sib", "x"], false).unwrap_err();
        assert!(
            send.to_string().contains("never message each other"),
            "{send}"
        );

        let allowed: &[&[&str]] = &[
            &["ls"],
            &["task", "ls"],
            &["task", "last", "sib"],
            &["worktree", "ls"],
            &["report", "done", "x"],
        ];
        for args in allowed {
            node_of(&app, &w, args, false).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        // A person at the worker's terminal still gets the screen.
        node_of(&app, &w, &["tasks"], true).unwrap();
    }

    #[test]
    fn only_what_is_allowed_passes() {
        let path = Path::new("/h/config.toml");
        let allow = vec!["task.create".to_string(), " task.send ".to_string()];
        assert!(refusal("task.create", &allow, path).is_none());
        assert!(refusal("task.send", &allow, path).is_none());
        let message = refusal("task.delete", &allow, path).unwrap();
        assert!(message.contains("may not delete tasks"), "{message}");
        assert!(message.contains("`task.delete`"), "{message}");
        assert!(message.contains("/h/config.toml"), "{message}");
        // A typo is pointed out rather than silently denying.
        let message = refusal("task.create", &["task.creat".into()], path).unwrap();
        assert!(
            message.contains("task.creat, which jeet does not know"),
            "{message}"
        );
    }

    #[test]
    fn the_harness_pre_approves_exactly_the_allowed_commands() {
        let rules = allowed_commands(&["task.create".into(), "task.send".into(), "bogus".into()]);
        assert!(rules.contains(&"jeet task new".to_string()));
        assert!(rules.contains(&"jeet task send".to_string()));
        assert!(rules.contains(&"jeet task ls".to_string()));
        assert!(rules.contains(&"jeet notify".to_string()));
        assert!(!rules
            .iter()
            .any(|r| r.contains("delete") || r.contains("archive")));
        assert!(!rules.contains(&"jeet worktree".to_string()));
        assert!(rules.contains(&"jeet todo".to_string()));
        assert!(!rules.iter().any(|r| r.starts_with("jeet pr ")));
        assert_eq!(allowed_commands(&[]).len(), READ_ONLY.len() + OWN.len());
        // Merging is pre-approved only once allowed.
        let rules = allowed_commands(&["pr.merge".into()]);
        assert!(rules.contains(&"jeet pr merge".to_string()));
    }

    #[test]
    fn every_capability_command_is_one_it_gates() {
        for c in CAPABILITIES {
            for command in c.commands {
                let mut args: Vec<&str> = command.split(' ').skip(1).collect();
                // Enough arguments for clap to accept it.
                match c.name {
                    "task.send" => args.extend(["x", "hi"]),
                    "worktree.add" | "worktree.rm" | "repo.clone" => args.push("x"),
                    "review.start" | "pr.merge" => args.push("1"),
                    "project.read" | "repo.sync" | "repo.import" | "shell.install"
                    | "watch.install" => {}
                    _ => args.push("x"),
                }
                assert_eq!(needs_of(&args).0, c.name, "{command}");
            }
        }
    }

    #[test]
    fn records_are_one_line_each() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("assistant/actions.log");
        append(
            &log,
            "task.send",
            "messaged task x: a\nb",
            Outcome::Done,
            None,
        )
        .unwrap();
        append(&log, "task.delete", "deleted task y", Outcome::Denied, None).unwrap();
        append(
            &log,
            "task.archive",
            "archived task z",
            Outcome::Failed,
            Some("no"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let records: Vec<ActionRecord> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].action, "messaged task x: a\nb");
        assert_eq!(records[1].outcome, Outcome::Denied);
        assert_eq!(records[2].error.as_deref(), Some("no"));
    }
}
