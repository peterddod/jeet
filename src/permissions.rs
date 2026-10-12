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
//! Refusals are logged too, and an action that ran but did nothing is
//! logged as skipped and not announced.
//!
//! The agent of a task with a parent is gated too: by its grant, cut by
//! every list above it ([`effective`]), and to the tasks in its reach
//! ([`node_scope`]), reads included. What it is allowed is logged to its
//! own `actions.log`.
//!
//! Every command must be classified: [`gate`] matches them exhaustively, so a
//! new one does not compile until it says what it needs; one [`node_scope`]
//! leaves out is refused to a task with a parent.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{
    Commands, ManagerCommands, PrCommands, ProjectCommands, TaskCommands, WatchCommands,
    WorktreeArgs, WorktreeCommands,
};
use crate::config::{self, ManagerPolicy};
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::identity;
use crate::notify::{self, Notification};
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
    /// Commands outside jeet that use it, as [`HookSetup::deny`] patterns:
    /// pre-approved for the assistant when it is allowed, and denied to a
    /// task with a parent that does not hold it.
    ///
    /// [`HookSetup::deny`]: crate::harness::HookSetup::deny
    pub external: &'static [&'static str],
}

pub const CAPABILITIES: &[Capability] = &[
    cap("task.create", "create tasks", &["jeet task new"]),
    cap("task.open", "start task agents", &["jeet task open"]),
    cap(
        "task.send",
        "message and interrupt task agents",
        &["jeet task send", "jeet task interrupt"],
    ),
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
    ext(
        "pr.comment",
        "comment on pull requests",
        &["gh pr comment", "gh issue comment"],
    ),
    ext(
        "pr.review",
        "submit pull request reviews",
        &["gh pr review"],
    ),
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
    cap("manager.create", "make managers", &["jeet manager new"]),
    cap(
        "manager.grant",
        "change what tasks under it may do and spend",
        &["jeet manager grant", "jeet manager budget"],
    ),
    cap(
        "manager.stop",
        "pause, resume and stop tasks under it",
        &[
            "jeet manager pause",
            "jeet manager resume",
            "jeet manager stop",
        ],
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
        external: &[],
    }
}

/// A capability used through commands outside jeet.
const fn ext(
    name: &'static str,
    about: &'static str,
    external: &'static [&'static str],
) -> Capability {
    Capability {
        name,
        about,
        commands: &[],
        external,
    }
}

/// What no agent jeet gates may run, whatever it holds: rewriting a
/// branch's history, and merging other than through `jeet pr merge` (which
/// runs `gh` from jeet itself, not from the agent's shell). Patterns as for
/// [`HookSetup::deny`], anchored at the start of the command, so
/// `git -C . push -f`, `sh -c '…'` and aliases get past them: a lint
/// against mistakes, with branch protection as the real control.
///
/// [`HookSetup::deny`]: crate::harness::HookSetup::deny
const ALWAYS_DENY: &[&str] = &[
    "git push --force*",
    "git push * --force*",
    "git push -f",
    "git push * -f",
    "git push * +*",
    "git push --mirror",
    "git push * --mirror",
    "git push --all",
    "git push * --all",
    // `--branches` is git 2.46's name for `--all`.
    "git push --branches",
    "git push * --branches",
    "gh pr merge",
    // The REST endpoints end at `merge`/`merges`, followed by a space or
    // nothing, so `repos/acme/mergesort` or `docs/merges.md` still run.
    "gh api *pulls/*/merge",
    "gh api *repos/*/merges",
    "gh api *mergePullRequest*",
    "gh api *enablePullRequestAutoMerge*",
    "gh api *mergeBranch*",
    "gh api *enqueuePullRequest*",
    "curl *pulls/*/merge",
    "curl *repos/*/merges",
    "curl *mergePullRequest*",
    "curl *enablePullRequestAutoMerge*",
    "curl *mergeBranch*",
    "curl *enqueuePullRequest*",
];

/// What a manager is denied besides: making commits and branches to push,
/// as its worktrees are read-only (`tasks::add_worktree`). Not `git branch`:
/// a prefix rule would deny `git branch -a` and `--show-current` too, which
/// a manager reads with, and a branch made in a detached worktree carries
/// nothing without `git commit`. A lint, as [`ALWAYS_DENY`] is.
const MANAGER_DENY: &[&str] = &["git commit", "git push", "git checkout -b", "git switch -c"];

/// Default branches assumed whatever the index says.
const FALLBACK_BRANCHES: [&str; 2] = ["main", "master"];

/// The most default branches denied, `main` and `master` among them: each
/// costs three patterns, six rules in Claude Code's settings, which every
/// gated agent carries. Past this, the branches fewest repositories use are
/// left out.
pub const MAX_DEFAULT_BRANCHES: usize = 8;

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
    "jeet manager ls",
    "jeet manager show",
    "jeet notify",
];

/// The assistant's own files, which it keeps through jeet: pre-approved
/// too, and needing no capability. Refused to every task, a manager
/// included (M39).
pub const OWN: &[&str] = &["jeet todo"];

pub fn capability(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|c| c.name == name)
}

/// A capability on the command line (`--allow`, `manager grant`): one jeet
/// knows, so a typo fails before anything is made.
pub fn parse_capability(s: &str) -> Result<String, String> {
    match capability(s.trim()) {
        Some(c) => Ok(c.name.to_string()),
        None => Err(format!(
            "{s:?} is not a capability; jeet knows {}",
            CAPABILITIES
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Whether this jeet says it runs for the assistant: a hint, which
/// [`identity`] checks.
///
/// [`identity`]: crate::identity
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

/// Who the task this jeet runs under makes the caller: the assistant is
/// the Root, a task with a parent a Node, and anything else, or no task,
/// the user.
fn classify(task: Option<&TaskRecord>) -> Caller {
    match task {
        Some(task) if task.kind == TaskKind::Assistant => Caller::Root,
        Some(task) if task.parent_id.is_some() => Caller::Node(task.clone()),
        _ => Caller::User,
    }
}

/// Who runs this jeet, by the task it runs under ([`identity::identify`]),
/// never the working directory. A claim jeet cannot confirm, or a database
/// error, is an error, so what is gated fails closed.
pub fn caller(app: &App) -> Result<Caller> {
    Ok(classify(identity::identify(app)?.task.as_ref()))
}

/// The capabilities `[assistant] allow` lists that jeet knows: the root's
/// list, which every list below it is cut by.
fn root_list(app: &App) -> Vec<&'static str> {
    known(app.config.assistant.allow.iter().map(String::as_str))
}

/// The capabilities among `names` that jeet knows, once each.
fn known<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'static str> {
    let mut list: Vec<&'static str> = Vec::new();
    for c in names.filter_map(|n| capability(n.trim())) {
        if !list.contains(&c.name) {
            list.push(c.name);
        }
    }
    list
}

/// What `node` holds itself, before its ancestors cut it: the root's list
/// for the assistant, its grant for a manager, and nothing for a worker,
/// which passes nothing on.
fn own_list(app: &App, node: &TaskRecord) -> Result<Vec<&'static str>> {
    Ok(match node.kind {
        TaskKind::Assistant => root_list(app),
        TaskKind::Manager => known(app.db.grants(&node.id)?.iter().map(String::as_str)),
        TaskKind::Work | TaskKind::Review => Vec::new(),
    })
}

/// What `node` may do: its grant, cut by each ancestor's own list up to
/// the assistant's `[assistant] allow`, so a child never holds what a
/// parent lacks. Read afresh at every check. Nothing for a task outside
/// the tree; a broken chain of parents is an error, so what is gated fails
/// closed.
pub fn effective(app: &App, node: &TaskRecord) -> Result<Vec<&'static str>> {
    if node.kind == TaskKind::Assistant {
        return Ok(root_list(app));
    }
    let ancestors = app.db.ancestors(&node.id)?;
    if ancestors
        .last()
        .is_none_or(|top| top.kind != TaskKind::Assistant)
    {
        return Ok(Vec::new());
    }
    let mut list = known(app.db.grants(&node.id)?.iter().map(String::as_str));
    for ancestor in &ancestors {
        let theirs = own_list(app, ancestor)?;
        list.retain(|c| theirs.contains(c));
    }
    Ok(list)
}

/// Who cut `capability` from `node`'s list when its grant names it: the
/// nearest ancestor whose own list lacks it, as "task <slug>" or
/// "`[assistant] allow` (<config path>)". Nothing when `node` holds it, or
/// its grant does not name it.
pub fn cut_by(app: &App, node: &TaskRecord, capability: &str) -> Result<Option<String>> {
    if effective(app, node)?.contains(&capability)
        || !app
            .db
            .grants(&node.id)?
            .iter()
            .any(|g| g.trim() == capability)
    {
        return Ok(None);
    }
    for ancestor in app.db.ancestors(&node.id)? {
        if own_list(app, &ancestor)?.contains(&capability) {
            continue;
        }
        return Ok(Some(match ancestor.kind {
            TaskKind::Assistant => format!(
                "`[assistant] allow` ({})",
                config::config_path(&app.home).display()
            ),
            _ => crate::manager::label(&ancestor),
        }));
    }
    // Every ancestor holds it, yet `node` does not: it is outside the tree.
    Ok(Some("having no place under the assistant".into()))
}

/// A new child's grant: what it may do, and which of that is held
/// ([`ManagerPolicy::held`]), to record once it is made ([`record_held`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub caps: Vec<&'static str>,
    pub held: Vec<&'static str>,
}

/// What a new child of `kind` gets when nobody names its grant:
/// `[managers] worker_default` or `manager_default`. Not cut here: its
/// ancestors cut it at every check ([`effective`]).
pub fn default_grant(kind: TaskKind, policy: &ManagerPolicy) -> Result<Vec<&'static str>> {
    Ok(match kind {
        TaskKind::Work | TaskKind::Review => policy.worker_default.clone(),
        TaskKind::Manager => policy.manager_default.clone(),
        TaskKind::Assistant => bail!("the assistant is no task to grant anything to"),
    })
}

/// The grant `by` may give a new child of `kind` under `parent`: what it
/// asks for (`requested`), else the kind's default. Each name it asks for
/// must be a capability `parent` holds, and, for a task with a parent
/// asking, one it holds itself. Changes nothing.
pub fn check_grant(
    app: &App,
    by: &Caller,
    parent: &TaskRecord,
    kind: TaskKind,
    requested: Option<&[String]>,
    policy: &ManagerPolicy,
) -> Result<Grant> {
    let Some(requested) = requested else {
        return Ok(Grant {
            caps: default_grant(kind, policy)?,
            held: Vec::new(),
        });
    };
    let parents = effective(app, parent)?;
    let own = match by {
        Caller::Node(node) => Some((node, effective(app, node)?)),
        Caller::User | Caller::Root => None,
    };
    let mut caps: Vec<&'static str> = Vec::new();
    for name in requested {
        let Some(c) = capability(name.trim()) else {
            bail!("{name} is not a capability jeet knows");
        };
        if !parents.contains(&c.name) {
            let whose = match parent.kind {
                TaskKind::Assistant => format!(
                    "the assistant does not hold it (`[assistant] allow` in {})",
                    config::config_path(&app.home).display()
                ),
                _ => format!(
                    "its parent, {}, does not hold it",
                    crate::manager::label(parent)
                ),
            };
            bail!("cannot grant `{}`: {whose}", c.name);
        }
        if let Some((node, held)) = &own {
            if !held.contains(&c.name) {
                bail!(
                    "cannot grant `{}`: {} does not hold it itself",
                    c.name,
                    crate::manager::label(node)
                );
            }
        }
        if !caps.contains(&c.name) {
            caps.push(c.name);
        }
    }
    let held = caps
        .iter()
        .copied()
        .filter(|c| policy.held.contains(c))
        .collect();
    Ok(Grant { caps, held })
}

/// Record that `by` granted `target` the held capabilities `held`: in the
/// granter's `actions.log` with a notification (whatever `[assistant]
/// notify` says), or, for the user, in the assistant's log marked
/// `by user`, without one.
pub fn record_held(app: &App, by: &Caller, target: &TaskRecord, held: &[&str]) -> Result<()> {
    record_held_with(app, by, target, held, &|n: &Notification| notify::notify(n))
}

fn record_held_with(
    app: &App,
    by: &Caller,
    target: &TaskRecord,
    held: &[&str],
    announce: &dyn Fn(&Notification),
) -> Result<()> {
    let (log, title, tag) = match by {
        Caller::User => (actions_log(&app.home), None, " by user"),
        Caller::Root => (actions_log(&app.home), Some("Assistant".to_string()), ""),
        Caller::Node(node) => (
            actions_log_for(app, node),
            Some(format!("Manager {}", node.slug)),
            "",
        ),
    };
    for capability in held {
        let action = format!("granted held `{capability}` to task {}{tag}", target.slug);
        append(&log, capability, &action, Outcome::Done, None)?;
        if let Some(title) = &title {
            announce(&Notification {
                title,
                subtitle: None,
                body: &action,
                url: None,
            });
        }
    }
    Ok(())
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
        Commands::Manager { command } => match command {
            ManagerCommands::New { name, .. } => needs(
                "manager.create",
                format!("created manager {name} and started its agent"),
            ),
            // Each checks who asks and shows only what it may see.
            ManagerCommands::Ls { .. } | ManagerCommands::Show { .. } => Gate::Free,
            // Starts only what is queued already, each through the same
            // start gate; the `Ended` hook runs it with no caller to check.
            ManagerCommands::Drain { .. } => Gate::Free,
            ManagerCommands::Grant { name, caps, .. } => needs(
                "manager.grant",
                match caps.is_empty() {
                    true => format!("set task {name}'s grant to nothing"),
                    false => format!("set task {name}'s grant to {}", caps.join(", ")),
                },
            ),
            ManagerCommands::Budget { name, change } => needs(
                "manager.grant",
                format!(
                    "changed task {name}'s budget by {}",
                    crate::manager::budget::change_label(*change)
                ),
            ),
            ManagerCommands::Pause { name } => needs("manager.stop", format!("paused task {name}")),
            ManagerCommands::Resume { name } => {
                needs("manager.stop", format!("resumed task {name}"))
            }
            ManagerCommands::Stop { name, .. } => needs(
                "manager.stop",
                match name {
                    Some(name) => format!("stopped task {name}"),
                    None => "stopped every task under the assistant".into(),
                },
            ),
        },
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
        TaskCommands::Interrupt { name } => needs("task.send", format!("interrupted task {name}")),
        TaskCommands::Restart { name, .. } => needs("task.open", format!("restarted task {name}")),
        // The user's word, at a terminal: never an agent's.
        TaskCommands::Adopt { .. } => Gate::Interactive,
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
/// jeet commands, its own, and the commands, jeet's and others, of each
/// allowed capability.
pub fn allowed_commands(allow: &[String]) -> Vec<String> {
    let allowed = CAPABILITIES
        .iter()
        .filter(|c| allow.iter().any(|a| a.trim() == c.name))
        .flat_map(|c| c.commands.iter().chain(c.external));
    READ_ONLY
        .iter()
        .chain(OWN)
        .chain(allowed)
        .map(|command| command.to_string())
        .collect()
}

/// The branches no gated agent may push to: `main`, `master` and the
/// default branch of each indexed repository, sorted, at most
/// [`MAX_DEFAULT_BRANCHES`] of them. Read at launch, so a repository cloned
/// later is covered from the next launch.
pub fn default_branches(app: &App) -> Result<Vec<String>> {
    let indexed = app
        .db
        .list_repos(None)?
        .into_iter()
        .map(|r| r.default_branch);
    Ok(most_used_branches(indexed))
}

/// `main`, `master` and the most common of `indexed`, up to
/// [`MAX_DEFAULT_BRANCHES`] (ties by name), sorted.
fn most_used_branches(indexed: impl Iterator<Item = String>) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for branch in indexed.filter(|b| !b.trim().is_empty()) {
        match counts.iter_mut().find(|(b, _)| *b == branch) {
            Some((_, n)) => *n += 1,
            None => counts.push((branch, 1)),
        }
    }
    counts.sort_by(|(a, m), (b, n)| n.cmp(m).then(a.cmp(b)));
    let mut branches: Vec<String> = FALLBACK_BRANCHES.map(str::to_string).to_vec();
    for (branch, _) in counts {
        if branches.len() == MAX_DEFAULT_BRANCHES {
            break;
        }
        if !branches.contains(&branch) {
            branches.push(branch);
        }
    }
    branches.sort();
    branches
}

/// What every agent jeet gates (the assistant, and tasks with a parent) is
/// denied: [`ALWAYS_DENY`], and pushing to any of `branches`.
pub fn denied_commands(branches: &[String]) -> Vec<String> {
    let mut deny: Vec<String> = ALWAYS_DENY.iter().map(|d| d.to_string()).collect();
    for branch in branches {
        for pattern in [
            format!("git push * {branch}"),
            format!("git push * *:{branch}"),
            // `refs/heads/b`, and `heads/b`, which git also resolves to
            // it, as the refspec or its destination.
            format!("git push * *heads/{branch}"),
        ] {
            if !deny.contains(&pattern) {
                deny.push(pattern);
            }
        }
    }
    deny
}

/// The commands outside jeet of every capability not in `held`.
fn withheld_externals<'a>(held: &'a [&str]) -> impl Iterator<Item = &'static str> + 'a {
    CAPABILITIES
        .iter()
        .filter(move |c| !held.contains(&c.name))
        .flat_map(|c| c.external.iter().copied())
}

/// What `task`'s agent is denied: nothing for a top-level task, which jeet
/// does not gate; for one with a parent, [`denied_commands`] and the
/// commands outside jeet of every capability it does not hold, and for a
/// manager [`MANAGER_DENY`] too.
pub fn task_deny(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    if task.parent_id.is_none() {
        return Ok(Vec::new());
    }
    let held = effective(app, task)?;
    let mut deny = node_deny(&held, &default_branches(app)?);
    match task.kind {
        TaskKind::Manager => {
            for command in MANAGER_DENY {
                if !deny.iter().any(|d| d == command) {
                    deny.push(command.to_string());
                }
            }
        }
        TaskKind::Work | TaskKind::Review | TaskKind::Assistant => {}
    }
    Ok(deny)
}

fn node_deny(held: &[&str], branches: &[String]) -> Vec<String> {
    let mut deny = denied_commands(branches);
    for command in withheld_externals(held) {
        if !deny.iter().any(|d| d == command) {
            deny.push(command.to_string());
        }
    }
    deny
}

/// Why the command the assistant was allowed did nothing, when it ran
/// without error but did not do what its action says: set by the command
/// through [`skipped`], read by [`Authorized::finish`].
static SKIPPED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Note that the command running did nothing, and why, so the assistant's
/// log says so rather than that it was done. Outside the assistant nothing
/// reads it.
pub fn skipped(why: String) {
    if let Ok(mut skipped) = SKIPPED.lock() {
        *skipped = Some(why);
    }
}

/// An action the assistant was allowed, to record once it has run.
pub struct Authorized {
    capability: &'static str,
    action: String,
    log: PathBuf,
    notify: bool,
}

/// Check that the caller may run `command`, before it does anything
/// ([`authorize_as`]). Returns the action to [`Authorized::finish`] when it
/// is one the log keeps.
///
/// Who the caller is comes from [`identity::identify`], whatever the
/// environment says; a caller it refuses is refused here, and one that
/// claims to be another is logged.
pub fn authorize(command: Option<&Commands>) -> Result<Option<Authorized>> {
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Only what is gated, scoped, or only the root may do, opens the
    // index: plumbing like hooks and `jeet report` never does.
    if gate(command, terminal) == Gate::Free
        && node_only(command).is_none()
        && matches!(node_scope(command), Some(Scope::NoTarget | Scope::Listed))
    {
        return Ok(None);
    }
    let app = App::open().context("check what this caller may do")?;
    let me = identity::identify(&app)?;
    if let Some(Commands::ReviewJob { state }) = command {
        check_review_job(&app.home, state)?;
        // The runner `jeet review` started, as the task it runs for.
        if me.self_job {
            return Ok(None);
        }
    }
    authorize_as(&app, &classify(me.task.as_ref()), command, terminal)
}

/// Check that `caller` may run `command`: the assistant only what
/// `[assistant] allow` lets it, the agent of a task with a parent only what
/// [`node`] does, and anyone else anything. The `jeet manager` verbs act
/// only on a task under the assistant, for everyone.
pub fn authorize_as(
    app: &App,
    caller: &Caller,
    command: Option<&Commands>,
    terminal: bool,
) -> Result<Option<Authorized>> {
    let gate = gate(command, terminal);
    if let Caller::Node(task) = caller {
        return node(app, task, command, gate, terminal);
    }
    if *caller == Caller::Root && stops_all(command) {
        bail!(
            "only the user stops every task under the assistant; `jeet manager stop <task>` \
             stops one"
        );
    }
    if let Some(Scope::Verb(verb, target)) = node_scope(command) {
        if verb.reach() == Reach::Tree {
            let target = resolve(app, caller, &target)
                .map_err(|e| anyhow!("`jeet {}`: {e:#}", verb.name()))?;
            in_scope(app, caller, verb, &target)?;
        }
    }
    if *caller == Caller::User {
        return Ok(None);
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
    let settings = &app.config.assistant;
    let log = actions_log(&app.home);
    let config_path = config::config_path(&app.home);
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
        _ if stops_all(command) => Some("stop every task under the assistant"),
        _ => None,
    }
}

/// Whether `command` is `jeet manager stop --all`, the user's alone (M11).
fn stops_all(command: Option<&Commands>) -> bool {
    matches!(
        command,
        Some(Commands::Manager {
            command: ManagerCommands::Stop { name: None, .. }
        })
    )
}

/// How to ask instead, ending every refusal a Node gets.
const ASK: &str = "ask with `jeet report blocked|question \"…\"`";

/// Check that `node`, a task with a parent, may run `command`: only on
/// tasks in its reach ([`node_scope`], reads included), and only what its
/// grant holds once its ancestors cut it ([`effective`]). An action it may
/// take is logged to its own `actions.log`, unannounced.
fn node(
    app: &App,
    node: &TaskRecord,
    command: Option<&Commands>,
    gate: Gate,
    terminal: bool,
) -> Result<Option<Authorized>> {
    let slug = &node.slug;
    if let Some(about) = node_only(command) {
        bail!(
            "task {slug} may not {about}: a task with a parent tells its parent with \
             `jeet report progress|done|blocked|question \"…\"`"
        );
    }
    let caller = Caller::Node(node.clone());
    match node_scope(command) {
        None => bail!(
            "task {slug} may not run that: jeet has no rule for what a task with a parent \
             may run it on; {ASK}"
        ),
        Some(Scope::NoTarget | Scope::Listed) => {}
        Some(Scope::Verb(verb, target)) => {
            let target = match resolve(app, &caller, &target) {
                Ok(target) => target,
                Err(e) => bail!(out_of_reach(slug, verb, &format!("{target} ({e:#})"))),
            };
            in_scope(app, &caller, verb, &target)?;
        }
    }
    let (capability, action) = match gate {
        Gate::Free => return Ok(None),
        Gate::Interactive if terminal => return Ok(None),
        Gate::Interactive => bail!(
            "that opens a screen or shell for a person at a terminal, which task {slug}'s \
             agent cannot use; {ASK}"
        ),
        Gate::Needs { capability, action } => (capability, action),
    };
    if !effective(app, node)?.contains(&capability) {
        let about = self::capability(capability).map_or("do that", |c| c.about);
        let cut = cut_by(app, node, capability)?;
        bail!(node_refusal(slug, about, capability, cut.as_deref()));
    }
    Ok(action.map(|action| Authorized {
        capability,
        action,
        log: actions_log_for(app, node),
        notify: false,
    }))
}

/// A task a command names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// By name, slug, id or (unless `exact`) the start of one slug.
    Key { key: String, exact: bool },
    /// The task a command run here is about ([`tasks::current`]), or the
    /// one named.
    ///
    /// [`tasks::current`]: crate::tasks::current
    Current(Option<String>),
    /// The parent of a new task: `assistant`, a task named exactly, or
    /// (`None`) the caller's default: the assistant for the assistant, and
    /// itself for a task with a parent.
    Parent(Option<String>),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Key { key, .. } | Target::Current(Some(key)) | Target::Parent(Some(key)) => {
                write!(f, "{key:?}")
            }
            Target::Current(None) => write!(f, "the task it runs in"),
            Target::Parent(None) => write!(f, "its default parent"),
        }
    }
}

/// The task `target` names for `caller`: for a task with a parent, only
/// itself or a task below it, so no name or prefix reaches outside its
/// subtree; for anyone else, as the commands find tasks.
pub fn resolve(app: &App, caller: &Caller, target: &Target) -> Result<TaskRecord> {
    let Caller::Node(node) = caller else {
        return match target {
            Target::Key { key, exact: false } => crate::tasks::find(app, key),
            Target::Key { key, exact: true } => crate::tasks::find_exact(app, key),
            Target::Current(key) => crate::tasks::current(app, key.as_deref()),
            Target::Parent(Some(key)) if key.trim() == crate::assistant::SLUG => {
                crate::manager::root(app)
            }
            Target::Parent(Some(key)) => crate::tasks::find_exact(app, key),
            Target::Parent(None) => match caller {
                Caller::Root => crate::manager::root(app),
                _ => bail!("a task you make has no parent unless you give --parent"),
            },
        };
    };
    let (key, exact) = match target {
        // It runs in itself: [`identity::identify`] said so.
        Target::Current(None) | Target::Parent(None) => return Ok(node.clone()),
        Target::Key { key, exact } => (key, *exact),
        Target::Current(Some(key)) => (key, false),
        Target::Parent(Some(key)) => (key, true),
    };
    let mut reach = vec![node.clone()];
    reach.extend(app.db.descendants(&node.id)?.into_iter().map(|(t, _)| t));
    crate::tasks::find_in(&reach, key, !exact)
        .with_context(|| format!("among task {} and the tasks below it", node.slug))
}

/// [`resolve`], for the caller running this jeet: how a command finds the
/// task it was checked against.
pub fn resolve_target(app: &App, target: &Target) -> Result<(Caller, TaskRecord)> {
    let caller = caller(app)?;
    let task = resolve(app, &caller, target)?;
    Ok((caller, task))
}

/// The task a command runs `verb` on, found and checked as [`authorize`]
/// found and checked it: by [`resolve_target`], then [`in_scope`].
pub fn scoped_target(app: &App, verb: Verb, target: Target) -> Result<TaskRecord> {
    let (caller, task) = resolve_target(app, &target)?;
    in_scope(app, &caller, verb, &task)?;
    Ok(task)
}

/// A command that names a task, by what a task with a parent may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    WorktreeAdd,
    WorktreeLs,
    WorktreePath,
    TaskLast,
    TaskPath,
    TaskPrs,
    TaskSend,
    TaskInterrupt,
    TaskOpen,
    TaskArchive,
    TaskUnarchive,
    TaskDelete,
    TaskResync,
    TaskNew,
    ManagerNew,
    ManagerShow,
    ManagerGrant,
    ManagerBudget,
    ManagerPause,
    ManagerResume,
    ManagerStop,
}

/// Which tasks a task with a parent may run a [`Verb`] on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Itself.
    Own,
    /// Itself, or a task below it.
    OwnOrBelow,
    /// A task below it.
    Below,
    /// The parent of a new task: itself or a task below it, a manager.
    Parent,
    /// A task below it; and for anyone, a task under the assistant.
    Tree,
}

impl Verb {
    /// The command, after `jeet `.
    pub fn name(self) -> &'static str {
        match self {
            Verb::WorktreeAdd => "worktree add",
            Verb::WorktreeLs => "worktree ls",
            Verb::WorktreePath => "worktree path",
            Verb::TaskLast => "task last",
            Verb::TaskPath => "task path",
            Verb::TaskPrs => "task prs",
            Verb::TaskSend => "task send",
            Verb::TaskInterrupt => "task interrupt",
            Verb::TaskOpen => "task open",
            Verb::TaskArchive => "task archive",
            Verb::TaskUnarchive => "task unarchive",
            Verb::TaskDelete => "task delete",
            Verb::TaskResync => "task resync",
            Verb::TaskNew => "task new",
            Verb::ManagerNew => "manager new",
            Verb::ManagerShow => "manager show",
            Verb::ManagerGrant => "manager grant",
            Verb::ManagerBudget => "manager budget",
            Verb::ManagerPause => "manager pause",
            Verb::ManagerResume => "manager resume",
            Verb::ManagerStop => "manager stop",
        }
    }

    pub fn reach(self) -> Reach {
        match self {
            Verb::WorktreeAdd | Verb::WorktreeLs | Verb::WorktreePath => Reach::Own,
            Verb::TaskLast | Verb::TaskPath | Verb::TaskPrs => Reach::OwnOrBelow,
            Verb::TaskSend
            | Verb::TaskInterrupt
            | Verb::TaskOpen
            | Verb::TaskArchive
            | Verb::TaskUnarchive
            | Verb::TaskDelete
            | Verb::TaskResync => Reach::Below,
            Verb::TaskNew | Verb::ManagerNew => Reach::Parent,
            Verb::ManagerShow
            | Verb::ManagerGrant
            | Verb::ManagerBudget
            | Verb::ManagerPause
            | Verb::ManagerResume
            | Verb::ManagerStop => Reach::Tree,
        }
    }
}

/// What a command may be run on by a task with a parent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// It names no task: reads of the store, plumbing, screens.
    NoTarget,
    /// A list it cuts down to the caller and the tasks below it itself.
    Listed,
    /// The task `Target` names, which must be in the verb's reach.
    Verb(Verb, Target),
}

/// The table of what a task with a parent may run each command on
/// (`None`, bare `jeet`: the tasks screen). A command it leaves out is
/// refused to every task with a parent, reads included, so a new one fails
/// closed until it says.
fn node_scope(command: Option<&Commands>) -> Option<Scope> {
    let Some(command) = command else {
        return Some(Scope::NoTarget);
    };
    let key = |key: &String| Target::Key {
        key: key.clone(),
        exact: false,
    };
    let verb = |verb: Verb, target: Target| Some(Scope::Verb(verb, target));
    match command {
        Commands::Completions { .. }
        | Commands::Complete { .. }
        | Commands::InitShell
        | Commands::Ls { .. }
        | Commands::Path { .. }
        | Commands::Prs { .. }
        | Commands::Notify { .. }
        | Commands::Report { .. }
        | Commands::Todo { .. }
        | Commands::ReviewJob { .. }
        | Commands::StoreSync
        | Commands::TaskHook { .. }
        | Commands::TaskStartup { .. }
        | Commands::Tasks
        | Commands::Explore
        | Commands::Exec { .. }
        | Commands::Chat { .. }
        // On a pull request, not a task: held, so only when granted by name.
        | Commands::Pr { .. } => Some(Scope::NoTarget),
        Commands::Watch {
            command: None | Some(WatchCommands::Status),
            ..
        } => Some(Scope::NoTarget),
        Commands::Worktree(WorktreeArgs { command, task }) => {
            let current = Target::Current(task.clone());
            match command {
                WorktreeCommands::Add { .. } => verb(Verb::WorktreeAdd, current),
                WorktreeCommands::Ls => verb(Verb::WorktreeLs, current),
                WorktreeCommands::Path { .. } => verb(Verb::WorktreePath, current),
                WorktreeCommands::Rm { .. } => None,
            }
        }
        Commands::Task { command } => match command {
            TaskCommands::Ls { .. } => Some(Scope::Listed),
            TaskCommands::Last { name, .. } => verb(Verb::TaskLast, key(name)),
            TaskCommands::Path { name } => verb(Verb::TaskPath, key(name)),
            TaskCommands::Prs { name, .. } => verb(Verb::TaskPrs, Target::Current(name.clone())),
            TaskCommands::Send { name, .. } => verb(Verb::TaskSend, key(name)),
            TaskCommands::Interrupt { name } => verb(Verb::TaskInterrupt, key(name)),
            TaskCommands::Open { name, .. } => verb(Verb::TaskOpen, key(name)),
            TaskCommands::Restart { name, .. } => verb(Verb::TaskOpen, key(name)),
            // Only the user vouches for an agent.
            TaskCommands::Adopt { .. } => None,
            TaskCommands::Archive { name, .. } => verb(Verb::TaskArchive, key(name)),
            TaskCommands::Unarchive { name } => verb(Verb::TaskUnarchive, key(name)),
            TaskCommands::Delete { name, .. } => verb(
                Verb::TaskDelete,
                Target::Key {
                    key: name.clone(),
                    exact: true,
                },
            ),
            TaskCommands::Resync { name, .. } => verb(Verb::TaskResync, key(name)),
            TaskCommands::New { delegation, .. } => verb(
                Verb::TaskNew,
                Target::Parent(delegation.parent.clone()),
            ),
        },
        Commands::Manager { command } => match command {
            ManagerCommands::New { delegation, .. } => verb(
                Verb::ManagerNew,
                Target::Parent(delegation.parent.clone()),
            ),
            ManagerCommands::Ls { .. } => Some(Scope::Listed),
            ManagerCommands::Show { name, .. } => verb(Verb::ManagerShow, key(name)),
            ManagerCommands::Grant { name, .. } => verb(Verb::ManagerGrant, key(name)),
            ManagerCommands::Budget { name, .. } => verb(Verb::ManagerBudget, key(name)),
            ManagerCommands::Pause { name } => verb(Verb::ManagerPause, key(name)),
            ManagerCommands::Resume { name } => verb(Verb::ManagerResume, key(name)),
            ManagerCommands::Stop {
                name: Some(name), ..
            } => verb(Verb::ManagerStop, key(name)),
            // `--all`: refused to all but the user before this is asked
            // ([`node_only`]).
            ManagerCommands::Stop { name: None, .. } => Some(Scope::NoTarget),
            // Starts only what is queued already.
            ManagerCommands::Drain { .. } => Some(Scope::NoTarget),
        },
        _ => None,
    }
}

/// Check that `caller` may run `verb` on `target`: a task with a parent
/// only on the tasks in the verb's [`Reach`], and anyone a `jeet manager`
/// verb only on a task under the assistant.
pub fn in_scope(app: &App, caller: &Caller, verb: Verb, target: &TaskRecord) -> Result<()> {
    if verb.reach() == Reach::Tree
        && (target.parent_id.is_none() || target.kind == TaskKind::Assistant)
    {
        bail!(
            "`jeet {}` is for a task under the assistant, and task {} has no parent",
            verb.name(),
            target.slug
        );
    }
    let Caller::Node(node) = caller else {
        return Ok(());
    };
    let own = target.id == node.id;
    let below = !own && app.db.is_strict_descendant(&target.id, &node.id)?;
    let reached = match verb.reach() {
        Reach::Own => own,
        Reach::OwnOrBelow => own || below,
        Reach::Below | Reach::Tree => below,
        Reach::Parent if own && target.kind != TaskKind::Manager => bail!(
            "task {} may not run `jeet {}`: a task with a parent cannot make tasks \
             unless it is a manager; {ASK}",
            node.slug,
            verb.name()
        ),
        Reach::Parent if below && target.kind != TaskKind::Manager => bail!(
            "task {} may not run `jeet {}` under task {}: only a manager has tasks \
             under it; {ASK}",
            node.slug,
            verb.name(),
            target.slug
        ),
        Reach::Parent => own || below,
    };
    match reached {
        true => Ok(()),
        false => bail!(out_of_reach(
            &node.slug,
            verb,
            &format!("task {}", target.slug)
        )),
    }
}

/// Why task `slug` may not run `verb` on `what`.
fn out_of_reach(slug: &str, verb: Verb, what: &str) -> String {
    let reach = match verb.reach() {
        Reach::Own => "itself",
        Reach::OwnOrBelow => "itself and the tasks below it",
        Reach::Below | Reach::Tree => "the tasks below it",
        Reach::Parent => "itself and the managers below it",
    };
    let sideways = match verb {
        Verb::TaskSend | Verb::TaskInterrupt => "; tasks never message each other otherwise",
        _ => "",
    };
    format!(
        "task {slug} may not run `jeet {}` on {what}: it may run it only on {reach}{sideways}; \
         {ASK}",
        verb.name()
    )
}

/// Whether `caller` sees `task` in a list (`task ls`, `manager ls`): a task
/// with a parent only itself and the tasks below it.
pub fn lists(app: &App, caller: &Caller, task: &TaskRecord) -> Result<bool> {
    match caller {
        Caller::Node(node) => {
            Ok(task.id == node.id || app.db.is_strict_descendant(&task.id, &node.id)?)
        }
        Caller::User | Caller::Root => Ok(true),
    }
}

/// Why task `slug` may not do `about` (`capability`): its grant does not
/// name it (`cut` None), so its parent may grant it, or an ancestor's list
/// cut it, named by `cut`.
fn node_refusal(slug: &str, about: &str, capability: &str, cut: Option<&str>) -> String {
    match cut {
        None => format!(
            "task {slug} may not {about}: `{capability}` is not in its grant, which its \
             parent sets; {ASK}"
        ),
        Some(by) => format!(
            "task {slug} may not {about}: `{capability}` is in its grant but cut by {by}; \
             {ASK}"
        ),
    }
}

impl Authorized {
    /// Record how the action went, and announce it when it was done.
    pub fn finish(self, result: &Result<()>) {
        let skipped = SKIPPED.lock().ok().and_then(|mut s| s.take());
        let (outcome, error) = match (result, skipped) {
            (Ok(()), None) => (Outcome::Done, None),
            (Ok(()), Some(why)) => (Outcome::Skipped, Some(why)),
            (Err(e), _) => (Outcome::Failed, Some(format!("{e:#}"))),
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

/// A review job runs the command its state file names, so only the ones
/// `jeet review` wrote, under `$JEET_HOME/reviews`, are run — not one an
/// agent wrote itself.
pub fn check_review_job(home: &Path, state: &Path) -> Result<()> {
    let reviews = home.join("reviews");
    let reviews = reviews.canonicalize().unwrap_or(reviews);
    let state = state
        .canonicalize()
        .with_context(|| format!("find {}", state.display()))?;
    if !state.starts_with(&reviews) {
        bail!(
            "review jobs run only from jeet's reviews dir, {}",
            reviews.display()
        );
    }
    Ok(())
}

/// Log that `who`, the agent jeet identified, claimed to be another
/// ([`identity::identify`]). Only jeet's own words: what it claimed, and
/// what it ran, are the agent's, and the assistant reads this log as its
/// own record.
pub fn log_mismatch(home: &Path, who: &str) {
    let log = actions_log(home);
    let action = format!("{who} claimed to be another task or the assistant");
    if let Err(e) = append(&log, "identity", &action, Outcome::Denied, None) {
        eprintln!("jeet: warning: could not write {}: {e:#}", log.display());
    }
}

pub fn actions_log(home: &Path) -> PathBuf {
    config::assistant_dir(home).join("actions.log")
}

/// Where `task`'s actions are logged: the assistant's [`actions_log`], or
/// `actions.log` in the task's directory.
pub fn actions_log_for(app: &App, task: &TaskRecord) -> PathBuf {
    crate::tasks::task_dir(app, task).join("actions.log")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Denied,
    /// It ran without error but did nothing: `error` says why.
    Skipped,
}

/// One line of `actions.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    pub at: i64,
    pub capability: String,
    pub action: String,
    pub outcome: Outcome,
    /// Why it failed, or why it did nothing.
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
    use crate::db::tasks::RunState;
    use crate::db::Database;
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
            (
                &["task", "interrupt", "x"],
                "task.send",
                "interrupted task x",
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
                &["task", "restart", "x", "--force"],
                "task.open",
                "restarted task x",
            ),
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
            (
                &["manager", "grant", "m", "task.send,worktree.add"],
                "manager.grant",
                "set task m's grant to task.send, worktree.add",
            ),
            (
                &["manager", "grant", "m", "--none"],
                "manager.grant",
                "set task m's grant to nothing",
            ),
            (
                &["manager", "budget", "m", "+1h,20turns"],
                "manager.grant",
                "changed task m's budget by +1h,20turns",
            ),
            (
                &["manager", "budget", "m", "=90m"],
                "manager.grant",
                "changed task m's budget by =90m",
            ),
            (&["manager", "pause", "m"], "manager.stop", "paused task m"),
            (
                &["manager", "resume", "m"],
                "manager.stop",
                "resumed task m",
            ),
            (&["manager", "stop", "m"], "manager.stop", "stopped task m"),
            (
                &["manager", "stop", "--all"],
                "manager.stop",
                "stopped every task under the assistant",
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
            &["manager", "drain"],
            &["store-sync"],
            &["todo"],
            &["todo", "add", "x"],
            &["todo", "done", "1"],
            &["watch", "--once"],
            &["watch", "status"],
            &["watch"],
            &["manager", "ls"],
            &["manager", "ls", "--json"],
            &["manager", "show", "x"],
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
            &["task", "adopt", "api"],
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
            placement: crate::placement::Placement::Local,
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        }
    }

    #[test]
    fn the_task_a_caller_runs_under_says_who_it_is() {
        let child = task(Some("a"));
        assert_eq!(classify(Some(&child)), Caller::Node(child.clone()));
        let root = TaskRecord {
            kind: TaskKind::Assistant,
            ..task(None)
        };
        assert_eq!(classify(Some(&root)), Caller::Root);
        assert_eq!(classify(Some(&task(None))), Caller::User);
        assert_eq!(classify(None), Caller::User);
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

    /// Insert `slug` of `kind` under `parent`, granted `grants`.
    fn add(
        app: &App,
        slug: &str,
        kind: TaskKind,
        parent: Option<&TaskRecord>,
        grants: &[&str],
    ) -> TaskRecord {
        let task = TaskRecord {
            id: format!("{slug}-id"),
            slug: slug.into(),
            name: slug.into(),
            kind,
            ..task(parent.map(|p| p.id.as_str()))
        };
        app.db.insert_task(&task).unwrap();
        app.db.set_grants(&task.id, grants).unwrap();
        task
    }

    fn every() -> Vec<&'static str> {
        CAPABILITIES.iter().map(|c| c.name).collect()
    }

    fn sorted(mut list: Vec<&'static str>) -> Vec<&'static str> {
        list.sort();
        list
    }

    fn run_as(app: &App, caller: &Caller, args: &[&str], terminal: bool) -> Result<()> {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied()))
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        authorize_as(app, caller, cli.command.as_ref(), terminal).map(|_| ())
    }

    fn node_of(app: &App, w: &TaskRecord, args: &[&str], terminal: bool) -> Result<()> {
        run_as(app, &Caller::Node(w.clone()), args, terminal)
    }

    #[test]
    fn effective_is_the_grant_cut_by_every_list_above_it() {
        let partial = ["task.create", "worktree.add"];
        for root in [Vec::new(), partial.to_vec(), every()] {
            let (_dir, mut app, _w) = tree();
            app.config.assistant.allow = root.iter().map(|c| c.to_string()).collect();
            let root_row = crate::assistant::ensure(&app).unwrap();
            assert_eq!(
                sorted(effective(&app, &root_row).unwrap()),
                sorted(root.clone())
            );
            let mut parent = root_row;
            for depth in 1..=3 {
                let m = add(
                    &app,
                    &format!("m{depth}"),
                    TaskKind::Manager,
                    Some(&parent),
                    &every(),
                );
                assert_eq!(
                    sorted(effective(&app, &m).unwrap()),
                    sorted(root.clone()),
                    "depth {depth}"
                );
                parent = m;
            }
        }

        let (_dir, mut app, _w) = tree();
        app.config.assistant.allow = vec!["task.create".into(), "task.send".into()];
        let root = crate::assistant::ensure(&app).unwrap();
        let m1 = add(&app, "m1", TaskKind::Manager, Some(&root), &every());
        let m2 = add(
            &app,
            "m2",
            TaskKind::Manager,
            Some(&m1),
            &["task.create", "task.delete"],
        );
        let m3 = add(&app, "m3", TaskKind::Manager, Some(&m2), &every());
        assert_eq!(effective(&app, &m3).unwrap(), ["task.create"]);
        assert_eq!(
            crate::manager::tree_path(&app, &m3).unwrap(),
            "assistant/m1/m2/m3"
        );
        // Cut in the middle, by the nearest that lacks it; above it, the
        // assistant's list; not granted, or held, not cut at all.
        assert_eq!(
            cut_by(&app, &m3, "task.send").unwrap().as_deref(),
            Some("manager m2")
        );
        let cut = cut_by(&app, &m3, "task.delete").unwrap().unwrap();
        assert!(cut.contains("[assistant] allow"), "{cut}");
        assert_eq!(cut_by(&app, &m3, "task.create").unwrap(), None);
        assert_eq!(cut_by(&app, &m2, "task.send").unwrap(), None);

        // A name jeet does not know is dropped.
        app.db
            .set_grants(&m1.id, &["bogus", "task.create"])
            .unwrap();
        assert_eq!(effective(&app, &m1).unwrap(), ["task.create"]);

        // A worker passes nothing on, whatever it was granted.
        let w = add(&app, "wk", TaskKind::Work, Some(&root), &every());
        let under = add(&app, "under", TaskKind::Work, Some(&w), &every());
        assert!(effective(&app, &under).unwrap().is_empty());
        assert_eq!(
            cut_by(&app, &under, "task.send").unwrap().as_deref(),
            Some("task wk")
        );
        // Nor is a task outside the tree anything.
        let top = app.db.get_task("top-id").unwrap().unwrap();
        let below_top = add(&app, "below-top", TaskKind::Work, Some(&top), &every());
        assert!(effective(&app, &top).unwrap().is_empty());
        assert!(effective(&app, &below_top).unwrap().is_empty());

        // A parent that is gone fails closed.
        let index = rusqlite::Connection::open(config::index_path(&app.home)).unwrap();
        index
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DELETE FROM tasks WHERE id = 'm2-id';",
            )
            .unwrap();
        assert!(effective(&app, &m3).is_err());
        assert!(task_deny(&app, &m3).is_err());
    }

    fn policy(app: &App) -> ManagerPolicy {
        app.config.manager_policy().unwrap()
    }

    #[test]
    fn a_grant_is_what_both_the_parent_and_the_granter_hold() {
        let (_dir, mut app, _w) = tree();
        app.config.assistant.allow = ["task.create", "task.send", "worktree.add", "pr.merge"]
            .map(String::from)
            .to_vec();
        let root = crate::assistant::ensure(&app).unwrap();
        let m = add(
            &app,
            "m",
            TaskKind::Manager,
            Some(&root),
            &["task.create", "pr.merge"],
        );
        let w = add(&app, "wk", TaskKind::Work, Some(&m), &[]);
        fn grant(
            app: &App,
            by: &Caller,
            parent: &TaskRecord,
            kind: TaskKind,
            asked: Option<&[&str]>,
        ) -> Result<Grant> {
            let asked: Option<Vec<String>> =
                asked.map(|list| list.iter().map(|c| c.to_string()).collect());
            check_grant(app, by, parent, kind, asked.as_deref(), &policy(app))
        }
        let refused = |by: &Caller, parent: &TaskRecord, asked: &[&str]| {
            grant(&app, by, parent, TaskKind::Work, Some(asked))
                .unwrap_err()
                .to_string()
        };

        let message = refused(&Caller::Root, &root, &["bogus"]);
        assert!(message.contains("bogus"), "{message}");
        let message = refused(&Caller::Root, &root, &["task.delete"]);
        assert!(message.contains("`task.delete`"), "{message}");
        assert!(message.contains("[assistant] allow"), "{message}");
        let message = refused(&Caller::User, &m, &["task.send"]);
        assert!(
            message.contains("`task.send`") && message.contains("its parent, manager m,"),
            "{message}"
        );
        // A task with a parent grants nothing above its own list.
        let message = refused(&Caller::Node(w.clone()), &m, &["task.create"]);
        assert!(message.contains("task wk does not hold it"), "{message}");

        // Nothing asked: the kind's default, uncut, which its ancestors cut
        // at every check.
        let default = grant(&app, &Caller::Root, &root, TaskKind::Work, None).unwrap();
        assert_eq!(default.caps, ["worktree.add"]);
        assert!(default.held.is_empty());
        let default = grant(&app, &Caller::Root, &root, TaskKind::Manager, None).unwrap();
        assert!(default.caps.is_empty());
        app.config.managers = Some(config::ManagersConfig {
            manager_default: Some(vec!["task.create".into()]),
            ..Default::default()
        });
        let default = grant(&app, &Caller::Node(m.clone()), &m, TaskKind::Manager, None).unwrap();
        assert_eq!(default.caps, ["task.create"]);
        assert!(grant(&app, &Caller::Root, &root, TaskKind::Assistant, None).is_err());

        // Held only when named, and then said to be.
        let given = grant(
            &app,
            &Caller::Root,
            &root,
            TaskKind::Work,
            Some(&["task.send"]),
        )
        .unwrap();
        assert_eq!((given.caps, given.held), (vec!["task.send"], vec![]));
        let given = grant(
            &app,
            &Caller::Node(m.clone()),
            &m,
            TaskKind::Work,
            Some(&["pr.merge", " task.create", "pr.merge"]),
        )
        .unwrap();
        assert_eq!(given.caps, ["pr.merge", "task.create"]);
        assert_eq!(given.held, ["pr.merge"]);
        // A capability `[managers] held` adds is held as the floor's are.
        app.config.managers = Some(config::ManagersConfig {
            held: Some(vec!["task.send".into()]),
            ..Default::default()
        });
        let given = grant(
            &app,
            &Caller::Root,
            &root,
            TaskKind::Work,
            Some(&["task.send"]),
        )
        .unwrap();
        assert_eq!(
            (given.caps, given.held),
            (vec!["task.send"], vec!["task.send"])
        );
    }

    #[test]
    fn held_grants_are_logged_by_the_granter_and_announced_unless_the_user_made_them() {
        let (_dir, mut app, w) = tree();
        // Held grants are announced even with `[assistant] notify = false`,
        // which only the root's own actions follow (M9).
        app.config.assistant.notify = Some(false);
        let root = crate::assistant::ensure(&app).unwrap();
        let m = add(&app, "m", TaskKind::Manager, Some(&root), &[]);
        let read = |log: &Path| -> Vec<ActionRecord> {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        let announced = std::cell::RefCell::new(Vec::new());
        let announce = |n: &Notification| announced.borrow_mut().push(n.title.to_string());

        record_held_with(&app, &Caller::Root, &w, &["pr.merge"], &announce).unwrap();
        let root_log = read(&actions_log(&app.home));
        assert_eq!(root_log.len(), 1);
        assert_eq!(root_log[0].capability, "pr.merge");
        assert!(root_log[0].action.contains("task w"), "{root_log:?}");
        assert_eq!(*announced.borrow(), ["Assistant"]);

        record_held_with(
            &app,
            &Caller::Node(m.clone()),
            &w,
            &["task.delete"],
            &announce,
        )
        .unwrap();
        let own = read(&crate::tasks::task_dir(&app, &m).join("actions.log"));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].capability, "task.delete");
        assert_eq!(*announced.borrow(), ["Assistant", "Manager m"]);
        assert_eq!(actions_log_for(&app, &root), actions_log(&app.home));

        record_held_with(&app, &Caller::User, &w, &["pr.merge"], &announce).unwrap();
        let root_log = read(&actions_log(&app.home));
        assert_eq!(root_log.len(), 2);
        assert!(root_log[1].action.ends_with("by user"), "{root_log:?}");
        assert_eq!(announced.borrow().len(), 2);
        // Nothing held, nothing written.
        record_held_with(&app, &Caller::Root, &w, &[], &announce).unwrap();
        assert_eq!(read(&actions_log(&app.home)).len(), 2);
    }

    /// The assistant holding everything, and under it:
    /// `m` (a manager) over `wk` and `m2`, `m2` over `fix-a`; `s` (a
    /// manager) over `fix-b`; and `top` outside the tree. Everyone is
    /// granted everything, so anything refused is out of reach.
    fn forest() -> (tempfile::TempDir, App) {
        let (dir, mut app, _w) = tree();
        app.config.assistant.allow = every().iter().map(|c| c.to_string()).collect();
        let root = crate::assistant::ensure(&app).unwrap();
        let m = add(&app, "m", TaskKind::Manager, Some(&root), &every());
        add(&app, "wk", TaskKind::Work, Some(&m), &every());
        let m2 = add(&app, "m2", TaskKind::Manager, Some(&m), &every());
        add(&app, "fix-a", TaskKind::Work, Some(&m2), &every());
        let s = add(&app, "s", TaskKind::Manager, Some(&root), &every());
        add(&app, "fix-b", TaskKind::Work, Some(&s), &every());
        (dir, app)
    }

    /// Every verb with a target, as a command line naming `t`.
    fn verbs(t: &str) -> Vec<(Verb, Vec<String>)> {
        let line = |words: &[&str]| words.iter().map(|w| w.replace("{}", t)).collect();
        vec![
            (
                Verb::WorktreeAdd,
                line(&["worktree", "add", "r", "--task", "{}"]),
            ),
            (Verb::WorktreeLs, line(&["worktree", "ls", "--task", "{}"])),
            (
                Verb::WorktreePath,
                line(&["worktree", "path", "r", "--task", "{}"]),
            ),
            (Verb::TaskLast, line(&["task", "last", "{}"])),
            (Verb::TaskPath, line(&["task", "path", "{}"])),
            (Verb::TaskPrs, line(&["task", "prs", "{}"])),
            (Verb::TaskSend, line(&["task", "send", "{}", "hi"])),
            (Verb::TaskInterrupt, line(&["task", "interrupt", "{}"])),
            (Verb::TaskOpen, line(&["task", "open", "{}"])),
            (Verb::TaskArchive, line(&["task", "archive", "{}"])),
            (Verb::TaskUnarchive, line(&["task", "unarchive", "{}"])),
            (Verb::TaskDelete, line(&["task", "delete", "{}"])),
            (Verb::TaskResync, line(&["task", "resync", "{}"])),
            (Verb::TaskNew, line(&["task", "new", "x", "--parent", "{}"])),
            (
                Verb::ManagerNew,
                line(&["manager", "new", "x", "--parent", "{}"]),
            ),
            (Verb::ManagerShow, line(&["manager", "show", "{}"])),
            (
                Verb::ManagerGrant,
                line(&["manager", "grant", "{}", "task.send"]),
            ),
            (
                Verb::ManagerBudget,
                line(&["manager", "budget", "{}", "+1h"]),
            ),
            (Verb::ManagerPause, line(&["manager", "pause", "{}"])),
            (Verb::ManagerResume, line(&["manager", "resume", "{}"])),
            (Verb::ManagerStop, line(&["manager", "stop", "{}"])),
        ]
    }

    /// `jeet manager stop --all` is the user's alone (M11), whatever the
    /// others hold.
    #[test]
    fn only_the_user_stops_everything() {
        let (_dir, app) = forest();
        let all = ["manager", "stop", "--all"];
        run_as(&app, &Caller::User, &all, false).unwrap();
        let e = run_as(&app, &Caller::Root, &all, false).unwrap_err();
        assert!(e.to_string().contains("only the user"), "{e}");
        let m = app.db.get_task("m-id").unwrap().unwrap();
        let e = node_of(&app, &m, &all, false).unwrap_err().to_string();
        assert!(
            e.contains("may not stop every task under the assistant"),
            "{e}"
        );
        assert!(e.contains("jeet report"), "{e}");
        // One task, the assistant may stop.
        run_as(&app, &Caller::Root, &["manager", "stop", "m"], false).unwrap();
    }

    #[test]
    fn a_task_with_a_parent_reaches_only_its_subtree_and_reads_are_scoped_too() {
        let (_dir, app) = forest();
        let row = |slug: &str| app.db.get_task(&format!("{slug}-id")).unwrap().unwrap();
        // Who is below whom, written out rather than asked of the index.
        let below = |caller: &str, target: &str| match caller {
            "m" => ["wk", "m2", "fix-a"].contains(&target),
            "s" => target == "fix-b",
            _ => false,
        };
        let manager = |slug: &str| ["m", "m2", "s"].contains(&slug);
        // `fix` is the start of `fix-a` and `fix-b`, one in each subtree.
        let targets = [
            "m",
            "wk",
            "m2",
            "fix-a",
            "s",
            "fix-b",
            "assistant",
            "top",
            "fix",
        ];
        let in_reach = |verb: Verb, caller: &str, target: &str| {
            let target = match target {
                // Delete and `--parent` take a task named exactly.
                "fix" if matches!(verb, Verb::TaskDelete | Verb::TaskNew | Verb::ManagerNew) => {
                    return false
                }
                "fix" if caller == "m" => "fix-a",
                "fix" if caller == "s" => "fix-b",
                "fix" => return false,
                t => t,
            };
            let own = caller == target;
            let below = below(caller, target);
            // Written out per verb rather than asked of `Verb::reach`.
            match verb {
                Verb::WorktreeAdd | Verb::WorktreeLs | Verb::WorktreePath => own,
                Verb::TaskLast | Verb::TaskPath | Verb::TaskPrs => own || below,
                Verb::TaskSend
                | Verb::TaskInterrupt
                | Verb::TaskOpen
                | Verb::TaskArchive
                | Verb::TaskUnarchive
                | Verb::TaskDelete
                | Verb::TaskResync
                | Verb::ManagerShow
                | Verb::ManagerGrant
                | Verb::ManagerBudget
                | Verb::ManagerPause
                | Verb::ManagerResume
                | Verb::ManagerStop => below,
                Verb::TaskNew | Verb::ManagerNew => (own || below) && manager(target),
            }
        };
        for caller in ["m", "wk", "s"] {
            let as_node = Caller::Node(row(caller));
            for target in targets {
                for (verb, args) in verbs(target) {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    let result = run_as(&app, &as_node, &args, false);
                    match in_reach(verb, caller, target) {
                        true => result.unwrap_or_else(|e| panic!("{caller} {args:?}: {e:#}")),
                        false => {
                            let e = result.expect_err(&format!("{caller} {args:?}")).to_string();
                            assert!(e.contains(&format!("jeet {}", verb.name())), "{e}");
                            assert!(e.contains("jeet report"), "{e}");
                        }
                    }
                }
            }
        }
        // A manager may not act on itself.
        for args in [
            &["task", "delete", "m"][..],
            &["task", "archive", "m"],
            &["task", "send", "m", "hi"],
            &["manager", "grant", "m", "--none"],
            &["manager", "pause", "m"],
            &["manager", "stop", "m"],
        ] {
            let e = node_of(&app, &row("m"), args, false)
                .unwrap_err()
                .to_string();
            assert!(e.contains(&format!("jeet {} {}", args[0], args[1])), "{e}");
            assert!(e.contains("jeet report"), "{e}");
        }
        // Sideways messages say why.
        let e = node_of(&app, &row("wk"), &["task", "send", "fix-b", "hi"], false).unwrap_err();
        assert!(e.to_string().contains("never message each other"), "{e}");
        let e = node_of(&app, &row("wk"), &["task", "new", "x"], false).unwrap_err();
        assert!(
            e.to_string()
                .contains("a task with a parent cannot make tasks"),
            "{e}"
        );
        node_of(&app, &row("m"), &["task", "new", "x"], false).unwrap();

        // The user and the assistant reach any task, but the `jeet manager`
        // verbs (but `new`) only a task under the assistant.
        for caller in [Caller::User, Caller::Root] {
            for target in targets {
                for (verb, args) in verbs(target) {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    let result = run_as(&app, &caller, &args, false);
                    let tree_node = !["assistant", "top", "fix"].contains(&target);
                    // `manager new` names a parent, which may be the
                    // assistant; whether it may have one is for the command.
                    let manager_verb =
                        verb.name().starts_with("manager ") && verb != Verb::ManagerNew;
                    match !manager_verb || tree_node {
                        true => result.unwrap_or_else(|e| panic!("{caller:?} {args:?}: {e:#}")),
                        false => {
                            let e = result.expect_err(&format!("{caller:?} {args:?}"));
                            assert!(e.to_string().contains(&format!("jeet {}", verb.name())));
                        }
                    }
                }
            }
        }

        // Not even on the assistant itself.
        for args in [
            &["manager", "show", "assistant"][..],
            &["manager", "grant", "assistant", "--none"],
        ] {
            let e = run_as(&app, &Caller::Root, args, false).unwrap_err();
            assert!(
                e.to_string()
                    .contains(&format!("jeet {} {}", args[0], args[1])),
                "{e}"
            );
        }

        // Lists show a task with a parent itself and what is below it.
        for (caller, sees) in [
            ("m", &["m", "wk", "m2", "fix-a"][..]),
            ("wk", &["wk"]),
            ("s", &["s", "fix-b"]),
        ] {
            let as_node = Caller::Node(row(caller));
            for args in [&["task", "ls"][..], &["manager", "ls"]] {
                run_as(&app, &as_node, args, false).unwrap();
            }
            let mut listed: Vec<String> = Vec::new();
            for task in app.db.list_tasks(None).unwrap() {
                if lists(&app, &as_node, &task).unwrap() {
                    listed.push(task.slug);
                }
            }
            listed.sort();
            let mut sees: Vec<String> = sees.iter().map(|s| s.to_string()).collect();
            sees.sort();
            assert_eq!(listed, sees, "{caller}");
        }
        let all = app.db.list_tasks(None).unwrap();
        for caller in [Caller::User, Caller::Root] {
            assert!(all.iter().all(|t| lists(&app, &caller, t).unwrap()));
        }
    }

    #[test]
    fn a_node_uses_only_what_its_grant_holds_once_cut() {
        let (_dir, mut app, _w) = tree();
        let root = crate::assistant::ensure(&app).unwrap();
        let m = add(
            &app,
            "m",
            TaskKind::Manager,
            Some(&root),
            &["task.send", "task.archive"],
        );
        let kid = add(&app, "kid", TaskKind::Work, Some(&m), &["worktree.add"]);
        let send = ["task", "send", "kid", "hi"];
        // In its grant, but the assistant lacks it.
        let e = node_of(&app, &m, &send, false).unwrap_err().to_string();
        assert!(
            e.contains("`task.send`") && e.contains("[assistant] allow"),
            "{e}"
        );
        app.config.assistant.allow = every().iter().map(|c| c.to_string()).collect();
        let done = authorize_as(
            &app,
            &Caller::Node(m.clone()),
            cli_of(&send).as_ref(),
            false,
        )
        .unwrap()
        .unwrap();
        // Logged in its own log, unannounced.
        assert_eq!(
            done.log,
            crate::tasks::task_dir(&app, &m).join("actions.log")
        );
        assert!(!done.notify);
        // Not in its grant: its parent sets that.
        let e = node_of(&app, &m, &["task", "delete", "kid"], false).unwrap_err();
        assert!(e.to_string().contains("not in its grant"), "{e}");
        // Cut by its parent, a manager.
        app.db
            .set_grants(&kid.id, &["worktree.add", "task.send"])
            .unwrap();
        node_of(
            &app,
            &kid,
            &["worktree", "add", "r", "--task", "kid"],
            false,
        )
        .unwrap_err();
        app.db.set_grants(&m.id, &["worktree.add"]).unwrap();
        node_of(
            &app,
            &kid,
            &["worktree", "add", "r", "--task", "kid"],
            false,
        )
        .unwrap();
        // Reads need nothing.
        node_of(&app, &kid, &["task", "last", "kid"], false).unwrap();
    }

    fn cli_of(args: &[&str]) -> Option<Commands> {
        crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied()))
            .unwrap()
            .command
    }

    #[test]
    fn a_node_adds_worktrees_to_itself_only_when_its_parent_may() {
        let (_dir, mut app, w) = tree();
        app.db.set_grants(&w.id, &["worktree.add"]).unwrap();
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
            assert!(refused.contains("only on itself"), "{other}: {refused}");
        }
        // Without a grant, its parent's list is not enough.
        app.db.set_grants(&w.id, &[]).unwrap();
        assert!(node_of(&app, &w, &add, false).is_err());
    }

    #[test]
    fn a_node_may_not_change_anything_else_whatever_its_parent_holds() {
        let (_dir, mut app, w) = tree();
        app.config.assistant.allow = every().iter().map(|c| c.to_string()).collect();
        app.db.set_grants(&w.id, &every()).unwrap();
        let refused: &[&[&str]] = &[
            &["worktree", "rm", "r", "--task", "w"],
            &["task", "send", "sib", "x"],
            &["task", "send", "assistant", "x"],
            &["task", "interrupt", "sib"],
            &["task", "archive", "sib"],
            &["task", "unarchive", "sib"],
            &["task", "delete", "sib"],
            &["task", "open", "sib"],
            &["task", "resync", "sib"],
            &["task", "last", "sib"],
            &["task", "path", "top"],
            &["task", "prs", "sib"],
            &["worktree", "ls", "--task", "sib"],
            &["worktree", "path", "r", "--task", "sib"],
            &["task", "new", "x"],
            &["clone", "https://example.com/a/b"],
            &["sync"],
            &["review", "42"],
            &["projects", "new", "p"],
            &["projects", "ls"],
            &["import"],
            &["install-shell"],
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
        for args in [
            &["task", "send", "sib", "x"][..],
            &["task", "interrupt", "sib"],
        ] {
            let refused = node_of(&app, &w, args, false).unwrap_err();
            assert!(
                refused.to_string().contains("never message each other"),
                "{refused}"
            );
        }

        let allowed: &[&[&str]] = &[
            &["ls"],
            &["prs"],
            &["task", "ls"],
            &["task", "last", "w"],
            &["task", "path", "w"],
            &["task", "prs"],
            &["worktree", "ls"],
            &["worktree", "path", "r"],
            &["report", "done", "x"],
            &["watch", "status"],
            &["task-hook", "Stop"],
            // Held, so only ever granted by name.
            &["pr", "merge", "42"],
        ];
        for args in allowed {
            node_of(&app, &w, args, false).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        // A person at the worker's terminal still gets the screen.
        node_of(&app, &w, &["tasks"], true).unwrap();
    }

    /// Every command path clap knows, as the words that name it.
    fn command_paths() -> Vec<Vec<String>> {
        fn walk(cmd: &clap::Command, at: Vec<String>, out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let mut path = at.clone();
                path.push(sub.get_name().to_string());
                if !sub.is_subcommand_required_set() || !sub.has_subcommands() {
                    out.push(path.clone());
                }
                walk(sub, path, out);
            }
        }
        let mut out = Vec::new();
        walk(
            &<crate::cli::Cli as clap::CommandFactory>::command(),
            Vec::new(),
            &mut out,
        );
        out
    }

    #[test]
    fn every_command_says_what_a_node_may_run_it_on_or_is_refused_to_one() {
        let (_dir, app, w) = tree();
        // One invocation of every command, and whether a task with a
        // parent may run it on something (listed in `node_scope`).
        let samples: &[(&[&str], bool)] = &[
            (&["clone", "u"], false),
            (&["import"], false),
            (&["ls"], true),
            (&["explore"], true),
            (&["prs"], true),
            (&["review", "1"], false),
            (&["pr", "merge", "1"], true),
            (&["review-job", "s"], true),
            (&["sync"], false),
            (&["store-sync"], true),
            (&["tasks"], true),
            (&["chat"], true),
            (&["todo"], true),
            (&["todo", "ls"], true),
            (&["todo", "add", "x"], true),
            (&["todo", "done", "1"], true),
            (&["todo", "rm", "1"], true),
            (&["task", "new", "x"], true),
            (&["task", "ls"], true),
            (&["task", "open", "x"], true),
            (&["task", "send", "x", "m"], true),
            (&["task", "interrupt", "x"], true),
            (&["task", "restart", "x"], true),
            (&["task", "adopt", "x"], false),
            (&["task", "archive", "x"], true),
            (&["task", "unarchive", "x"], true),
            (&["task", "delete", "x"], true),
            (&["task", "path", "x"], true),
            (&["task", "prs"], true),
            (&["task", "last", "x"], true),
            (&["task", "resync", "x"], true),
            (&["manager", "new", "x"], true),
            (&["manager", "ls"], true),
            (&["manager", "show", "x"], true),
            (&["manager", "grant", "x", "--none"], true),
            (&["manager", "budget", "x", "+1h"], true),
            (&["manager", "pause", "x"], true),
            (&["manager", "resume", "x"], true),
            (&["manager", "stop", "x"], true),
            // Refused to a task with a parent before its scope is asked.
            (&["manager", "stop", "--all"], true),
            (&["manager", "drain"], true),
            (&["projects"], false),
            (&["projects", "ls"], false),
            (&["projects", "new", "p"], false),
            (&["projects", "edit", "p"], false),
            (&["projects", "rm", "p"], false),
            (&["task-hook", "Stop"], true),
            (&["task-startup", "x"], true),
            (&["worktree", "add", "r"], true),
            (&["worktree", "ls"], true),
            (&["worktree", "rm", "r"], false),
            (&["worktree", "path", "r"], true),
            (&["path", "r"], true),
            (&["exec", "r"], true),
            (&["watch"], true),
            (&["watch", "install"], false),
            (&["watch", "uninstall"], false),
            (&["watch", "status"], true),
            (&["notify", "t"], true),
            (&["report", "done", "x"], true),
            (&["init-shell"], true),
            (&["install-shell"], false),
            (&["completions", "zsh"], true),
            (&["complete", "x"], true),
        ];
        for path in command_paths() {
            assert!(
                samples.iter().any(|(args, _)| args
                    .starts_with(&path.iter().map(String::as_str).collect::<Vec<_>>())
                    && args.len() > path.len() - 1),
                "no sample of `jeet {}`: add it, and decide its scope",
                path.join(" ")
            );
        }
        for (args, listed) in samples {
            let command = cli_of(args);
            assert_eq!(node_scope(command.as_ref()).is_some(), *listed, "{args:?}");
            if !listed {
                // Refused before anything else, reads and screens included.
                let e = node_of(&app, &w, args, true).unwrap_err().to_string();
                assert!(e.contains("no rule"), "{args:?}: {e}");
            }
        }
        assert_eq!(node_scope(None), Some(Scope::NoTarget));
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
        assert!(rules.contains(&"jeet task interrupt".to_string()));
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
                    "task.send" if *command == "jeet task send" => args.extend(["x", "hi"]),
                    "manager.grant" if *command == "jeet manager grant" => {
                        args.extend(["x", "task.send"])
                    }
                    "manager.grant" => args.extend(["x", "+1h"]),
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

    fn main_master() -> Vec<String> {
        FALLBACK_BRANCHES.map(str::to_string).to_vec()
    }

    #[test]
    fn every_gated_agent_is_denied_force_push_and_merge() {
        let every: Vec<&str> = CAPABILITIES.iter().map(|c| c.name).collect();
        for deny in [
            denied_commands(&main_master()),
            node_deny(&[], &main_master()),
            node_deny(&every, &main_master()),
        ] {
            for always in ALWAYS_DENY {
                assert!(deny.contains(&always.to_string()), "{always}: {deny:?}");
            }
            assert!(deny.contains(&"git push * main".to_string()), "{deny:?}");
            let mut unique = deny.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), deny.len(), "{deny:?}");
        }
    }

    #[test]
    fn comment_and_review_are_denied_unless_held() {
        let deny = node_deny(&[], &main_master());
        for external in ["gh pr comment", "gh issue comment", "gh pr review"] {
            assert!(deny.contains(&external.to_string()), "{external}");
        }
        let deny = node_deny(&["pr.comment"], &main_master());
        assert!(!deny.contains(&"gh pr comment".to_string()));
        assert!(!deny.contains(&"gh issue comment".to_string()));
        assert!(deny.contains(&"gh pr review".to_string()));
        // The assistant is never denied them: it asks the user at the prompt.
        assert!(!denied_commands(&main_master())
            .iter()
            .any(|d| d.contains("comment") || d.contains("review")));
        // Held, the assistant runs them without asking.
        let allowed = allowed_commands(&["pr.comment".into()]);
        assert!(allowed.contains(&"gh pr comment".to_string()));
        assert!(allowed.contains(&"gh issue comment".to_string()));
        assert!(!allowed.contains(&"gh pr review".to_string()));
    }

    #[test]
    fn externals_are_not_jeet_commands() {
        for c in CAPABILITIES {
            for external in c.external {
                assert!(!external.starts_with("jeet "), "{external}");
            }
        }
    }

    #[test]
    fn no_command_is_both_allowed_and_denied() {
        let every: Vec<String> = CAPABILITIES.iter().map(|c| c.name.to_string()).collect();
        let allowed = allowed_commands(&every);
        let mut denied = denied_commands(&main_master());
        // A manager's too: it is pre-approved what it holds (M37).
        denied.extend(MANAGER_DENY.iter().map(|d| d.to_string()));
        for a in &allowed {
            assert!(!denied.contains(a), "{a}");
            assert!(
                !denied.iter().any(|d| claude_match(d, a)),
                "{a} is pre-approved and denied"
            );
        }
    }

    #[test]
    fn default_branches_cover_every_repo_plus_main_master() {
        let (_dir, app, _w) = tree();
        for (id, branch) in [("a", "develop"), ("b", "main"), ("c", "develop")] {
            app.db
                .upsert_repo(&crate::db::RepoRecord {
                    id: id.into(),
                    trunk_path: format!("/store/{id}"),
                    remote_url: format!("https://example.com/o/{id}"),
                    default_branch: branch.into(),
                })
                .unwrap();
        }
        assert_eq!(
            default_branches(&app).unwrap(),
            ["develop", "main", "master"]
        );
    }

    #[test]
    fn default_branches_keep_main_master_and_the_most_used_up_to_the_cap() {
        let mut indexed: Vec<String> = (0..MAX_DEFAULT_BRANCHES)
            .map(|i| format!("rare-{i:02}"))
            .collect();
        indexed.extend(["develop", "develop", "trunk", "trunk", "main"].map(String::from));
        // However long: the settings go in a file, not on the command line.
        let long = "release/production-integration".to_string();
        indexed.extend(vec![long.clone(); 2]);
        let branches = most_used_branches(indexed.into_iter());
        assert_eq!(branches.len(), MAX_DEFAULT_BRANCHES);
        for kept in ["develop", "main", "master", "trunk", &long] {
            assert!(branches.contains(&kept.to_string()), "{branches:?}");
        }
        // Ties among the rare ones go by name.
        assert!(branches.contains(&"rare-00".to_string()), "{branches:?}");
        let last = format!("rare-{:02}", MAX_DEFAULT_BRANCHES - 1);
        assert!(!branches.contains(&last), "{branches:?}");
        let mut sorted = branches.clone();
        sorted.sort();
        assert_eq!(branches, sorted);
    }

    #[test]
    fn a_top_level_task_gets_no_deny_and_a_node_gets_the_list() {
        let (_dir, mut app, w) = tree();
        app.db
            .upsert_repo(&crate::db::RepoRecord {
                id: "d".into(),
                trunk_path: "/store/d".into(),
                remote_url: "https://example.com/o/d".into(),
                default_branch: "develop".into(),
            })
            .unwrap();
        // What the root holds is not handed down beyond the node's grant.
        app.config.assistant.allow = CAPABILITIES.iter().map(|c| c.name.into()).collect();
        let top = app.db.get_task("top-id").unwrap().unwrap();
        assert!(task_deny(&app, &top).unwrap().is_empty());
        let deny = task_deny(&app, &w).unwrap();
        for denied in [
            "gh pr merge",
            "gh pr comment",
            "gh pr review",
            "gh issue comment",
            "git push * master",
            "git push * develop",
        ] {
            assert!(deny.contains(&denied.to_string()), "{denied}: {deny:?}");
        }
    }

    /// A capability with commands jeet never sees run is lifted from the
    /// deny list only while the task's grant holds it cut by every list
    /// above it, not by its own grant alone.
    #[test]
    fn a_granted_comment_is_lifted_only_while_every_list_above_holds_it() {
        let (_dir, mut app, _w) = tree();
        app.config.assistant.allow = vec!["pr.comment".into()];
        let root = crate::assistant::ensure(&app).unwrap();
        let m = add(&app, "m", TaskKind::Manager, Some(&root), &["pr.comment"]);
        let w = add(&app, "w2", TaskKind::Work, Some(&m), &["pr.comment"]);
        let comment = "gh pr comment".to_string();
        assert!(!task_deny(&app, &w).unwrap().contains(&comment));
        // Its parent's grant no longer holds it.
        app.db.set_grants(&m.id, &[]).unwrap();
        assert!(task_deny(&app, &w).unwrap().contains(&comment));
        // Nor the root's list.
        app.db.set_grants(&m.id, &["pr.comment"]).unwrap();
        app.config.assistant.allow = Vec::new();
        assert!(task_deny(&app, &w).unwrap().contains(&comment));
    }

    #[test]
    fn a_manager_is_denied_commits_and_new_branches_but_may_read_branches() {
        let (_dir, app, w) = tree();
        let m = TaskRecord {
            id: "m-id".into(),
            slug: "m".into(),
            name: "m".into(),
            kind: TaskKind::Manager,
            ..w.clone()
        };
        app.db.insert_task(&m).unwrap();
        let deny = task_deny(&app, &m).unwrap();
        for denied in MANAGER_DENY {
            assert!(deny.contains(&denied.to_string()), "{denied}: {deny:?}");
        }
        assert!(
            !deny.iter().any(|d| d.starts_with("git branch")),
            "{deny:?}"
        );
        let mut unique = deny.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), deny.len(), "{deny:?}");
        let denied = |command: &str| deny.iter().any(|d| claude_match(d, command));
        for command in [
            "git commit -m x",
            "git commit",
            "git push -u origin HEAD",
            "git checkout -b x",
            "git switch -c x",
        ] {
            assert!(denied(command), "not denied: {command}");
        }
        for command in [
            "git branch -a",
            "git branch --show-current",
            "git log main",
            "git checkout main",
            "git switch --detach origin/main",
        ] {
            assert!(!denied(command), "denied: {command}");
        }
        // A worker still commits and pushes its own branch.
        let deny = task_deny(&app, &w).unwrap();
        assert!(!deny.iter().any(|d| claude_match(d, "git commit -m x")));
        assert!(!deny
            .iter()
            .any(|d| claude_match(d, "git push -u origin HEAD")));
    }

    /// A model of how Claude Code (2.1) matches the `Bash(…)` deny rules
    /// jeet writes for `pattern` (see `deny_rules` in the Claude harness):
    /// `*` matches any run of characters, and a trailing ` *` also matches
    /// nothing, but only when it is the rule's one `*`. It is not Claude
    /// itself; docs/harnesses.md lists what to check by hand.
    fn claude_match(pattern: &str, command: &str) -> bool {
        fn glob(p: &[u8], c: &[u8]) -> bool {
            match p.split_first() {
                None => c.is_empty(),
                Some((b'*', rest)) => (0..=c.len()).any(|i| glob(rest, &c[i..])),
                Some((b, rest)) => c.first() == Some(b) && glob(rest, &c[1..]),
            }
        }
        crate::harness::claude::deny_rules(pattern)
            .iter()
            .any(|rule| {
                let rule = &rule["Bash(".len()..rule.len() - 1];
                let bare = rule
                    .strip_suffix(" *")
                    .filter(|_| rule.matches('*').count() == 1);
                glob(rule.as_bytes(), command.as_bytes())
                    || bare.is_some_and(|b| glob(b.as_bytes(), command.as_bytes()))
            })
    }

    #[test]
    fn the_deny_list_stops_force_pushes_and_merges_but_not_work() {
        let deny = node_deny(&[], &main_master());
        let denied = |command: &str| deny.iter().any(|d| claude_match(d, command));
        for command in [
            "git push --force",
            "git push origin x --force-with-lease",
            "git push -f",
            "git push origin x -f",
            "git push origin +x",
            "git push --mirror",
            "git push origin main",
            "git push -u origin main",
            "git push origin HEAD:main",
            "git push origin HEAD:refs/heads/main",
            "git push origin --mirror",
            "git push origin :main",
            "git push origin HEAD:refs/heads/master",
            "git push origin refs/heads/main",
            "git push origin heads/main",
            "git push origin HEAD:heads/master",
            "git push origin --delete main",
            "git push --all",
            "git push --all origin",
            "git push origin --all",
            "git push --branches origin",
            "git push origin --branches",
            "gh pr merge",
            "gh pr merge 3 --squash",
            "gh api -X PUT repos/o/r/pulls/3/merge",
            "gh api graphql -f query='mutation{mergePullRequest(input:{})}'",
            "gh api graphql -f query='mutation{enablePullRequestAutoMerge(input:{})}'",
            "gh api -X POST graphql -f query='mutation{mergePullRequest(input:{})}'",
            "gh api -H 'GraphQL-Features: x' graphql -f query='mutation{enablePullRequestAutoMerge(input:{})}'",
            "gh api graphql -f query='mutation{mergeBranch(input:{})}'",
            "gh api graphql -f query='mutation{enqueuePullRequest(input:{})}'",
            "curl -X PUT https://api.github.com/repos/o/r/pulls/3/merge",
            "gh api -X POST repos/o/r/merges -f base=main -f head=task-x",
            "curl -X POST https://api.github.com/repos/o/r/merges -d '{}'",
            "gh api repos/o/r/pulls/3/merge",
            "gh api /repos/o/r/merges --method POST -f base=main",
            "curl -X POST https://api.github.com/graphql -d '{\"query\":\"mutation{mergePullRequest(input:{})}\"}'",
            "curl https://api.github.com/graphql -d 'mutation{enablePullRequestAutoMerge(input:{})}'",
            "curl https://api.github.com/graphql -d 'mutation{mergeBranch(input:{})}'",
            "curl https://api.github.com/graphql -d 'mutation{enqueuePullRequest(input:{})}'",
            "gh pr comment 3 -b hi",
            "gh pr review 3 --approve",
        ] {
            assert!(denied(command), "not denied: {command}");
        }
        for command in [
            "git push -u origin HEAD",
            "git push origin task-x",
            "git push origin main-fix",
            "git push origin refs/heads/main:refs/heads/task-x",
            "git push origin heads/main-fix",
            "git push --follow-tags",
            "gh pr create",
            "gh pr view 3",
            "gh pr checks 3",
            "jeet pr merge 3",
            "git log main",
            "git commit -m 'gh pr merge later'",
            "rg 'gh pr merge'",
            "git stash push",
            "gh api graphql -f query='{viewer{login}}'",
            "curl https://api.github.com/graphql -d '{\"query\":\"{viewer{login}}\"}'",
            "gh api repos/acme/mergesort/pulls",
            "gh api repos/mergestat/mergestat/pulls/3",
            "gh api repos/o/r/contents/docs/merges.md",
            "gh api repos/o/r/pulls/3/comments -f path=src/merge.rs -f body=x",
            "curl https://api.github.com/repos/acme/mergesort/pulls",
        ] {
            assert!(!denied(command), "denied: {command}");
        }
        // Gaps an anchored pattern leaves, pinned so a change is noticed.
        // Branch protection is the real control.
        const KNOWN_GAPS: &[&str] = &[
            "git -C . push -f",
            "git -c a=b push --force",
            "sh -c 'git push --force'",
            "git push -uf origin x",
            "git push origin x --mirr",
            "git push origin \"HEAD:main\"",
            "git push origin 'refs/heads/*:refs/heads/*'",
            "gh pr --repo o/r merge 3",
            "gh api -X PATCH repos/o/r/git/refs/heads/main -f sha=abc",
            // Commits through the API, to the default branch when none is
            // named: denying them would refuse reads, or commits to the
            // task's own branch.
            "gh api -X PUT repos/o/r/contents/README.md -f message=x -f content=eA==",
            "gh api graphql -f query='mutation{createCommitOnBranch(input:{})}'",
            // Withheld commenting and reviewing, through the API rather than
            // `gh pr comment`/`gh pr review`: denying the endpoints would
            // refuse reading comments and reviews too.
            "gh api -X POST repos/o/r/pulls/3/reviews -f event=APPROVE",
            "gh api repos/o/r/issues/3/comments -f body=x",
            "gh api graphql -f query='mutation{addPullRequestReview(input:{})}'",
            // The destination implied, not named: on main, these push it.
            "git push origin HEAD",
            "git push origin @",
            // The mutation in a file, not on the command line.
            "gh api graphql -F query=@merge.graphql -F id=PR_x",
            "gh api graphql --input body.json",
            // The endpoint quoted, or with a query string: matching what
            // follows `merge` would refuse `repos/acme/mergesort` too.
            "curl -X PUT \"https://api.github.com/repos/o/r/pulls/3/merge\"",
            "gh api -X POST 'repos/o/r/merges' -f base=main",
            "gh api -X PUT repos/o/r/pulls/3/merge?x=1",
        ];
        for command in KNOWN_GAPS {
            assert!(!denied(command), "now denied, update the docs: {command}");
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
