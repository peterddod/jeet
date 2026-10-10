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
    Commands, ProjectCommands, TaskCommands, WatchCommands, WorktreeArgs, WorktreeCommands,
};
use crate::config;
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
        Commands::Review { pr, repo, .. } => {
            let pr = match pr.trim_start_matches('#').parse::<u64>() {
                Ok(number) => format!("#{number}"),
                Err(_) => pr.clone(),
            };
            let repo = repo.as_ref().map(|r| format!(" of {r}")).unwrap_or_default();
            needs("review.start", format!("checked out {pr}{repo} for review"))
        }
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
        TaskCommands::Ls { .. } | TaskCommands::Path { .. } | TaskCommands::Prs { .. } => {
            Gate::Free
        }
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
pub fn authorize(command: Option<&Commands>) -> Result<Option<Authorized>> {
    if !assistant_mode() {
        return Ok(None);
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if let Some(Commands::ReviewJob { state }) = command {
        check_review_job(state)?;
    }
    let (capability, action) = match gate(command, terminal) {
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
        assert_eq!(allowed_commands(&[]).len(), READ_ONLY.len() + OWN.len());
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
                    "review.start" => args.push("1"),
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
