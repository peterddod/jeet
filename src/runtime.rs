//! A task's agent at runtime: started in the task's tmux session with
//! jeet's hooks, stopped with the task, and what it is doing now.
//!
//! An agent whose harness supports it gets a session id chosen by jeet, so
//! the session is recorded before the agent even runs, and hooks that report
//! its status and let it run `jeet worktree` and `jeet ls` without asking
//! (for Claude Code, a `--settings` JSON). Nothing is written into the task
//! or any repository for this. An agent whose harness supports none of it
//! just runs in the session, with no status and no sessions.

use std::path::Path;

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::agent::AgentSpec;
use crate::cd;
use crate::config::AssistantConfig;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord, TaskSessionRecord};
use crate::harness::{self, HookSetup};
use crate::permissions;
use crate::projects;
use crate::startup;
use crate::status::{self, Activity};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

/// What the agent may run without a permission prompt: the commands the
/// built-in jeet skill tells it to use.
const ALLOWED: [&str; 3] = ["jeet worktree", "jeet ls", "jeet report"];

/// The task's tmux session.
pub fn session_name(task: &TaskRecord) -> String {
    format!("{}{}", crate::tmux::SESSION_PREFIX, task.slug)
}

/// The outcome of [`ensure_running`].
pub struct Running {
    pub tmux_session: String,
    /// False when the agent was already running.
    pub started: bool,
    pub warnings: Vec<String>,
}

/// Make sure the task's agent is running in its tmux session, starting it if
/// not: resuming `session` when given, else the task's latest session, else
/// a new one. An agent this starts gets `prompt` as its first prompt; one
/// already running does not.
pub fn ensure_running(
    app: &App,
    task: &TaskRecord,
    session: Option<&str>,
    prompt: Option<&str>,
) -> Result<Running> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant's sessions are opened with `jeet chat`");
    }
    tmux::require()?;
    let name = session_name(task);
    let mut warnings = Vec::new();
    // With `remain-on-exit` set, a session outlives its agent; one with
    // nothing left running in it is replaced rather than attached to.
    if tmux::has_session(&name) && !tmux::pane_alive(&name) {
        tmux::kill(&name)?;
    }
    if tmux::has_session(&name) {
        if let Some(id) = session {
            warnings.push(format!(
                "the agent is already running in tmux session {name}, so session {id} was not \
                 resumed; end it there and open the task again to switch"
            ));
        }
        return Ok(Running {
            tmux_session: name,
            started: false,
            warnings,
        });
    }

    let mut spec = AgentSpec::from_config(&app.config)?;
    // The project's own arguments; a project since deleted or broken costs
    // only those, not the task.
    match projects::agent_args(app, task) {
        Ok(args) => spec.argv.extend(args),
        Err(e) => warnings.push(format!("no agent arguments from the task's project: {e:#}")),
    }
    let dir = tasks::task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let (argv, session_id) = launch_argv(app, task, &spec, session, prompt)?;
    warnings.extend(spec.version_warning());
    if task.parent_id.is_some() && spec.harness.enforcement() == harness::Enforcement::None {
        warnings.push(format!(
            "{} has no deny rules, so task {}'s agent is not stopped from force-pushing or \
             merging; only jeet's own commands are gated",
            spec.harness.name(),
            task.slug
        ));
    }
    let hooks = spec.has_hooks();
    if let Err(e) = status::mark_launched(&dir, hooks) {
        warnings.push(format!("could not note the agent's launch: {e:#}"));
    }
    let home = app.home.to_string_lossy();
    let env = [
        (tasks::TASK_ID_ENV, task.id.as_str()),
        // The agent's own `jeet` calls and hooks must find this index, not
        // whatever the tmux server's environment says.
        ("JEET_HOME", home.as_ref()),
        // Blank, in case a server started elsewhere has one: an agent's
        // `jeet worktree add` must not move the user's shell.
        (cd::CD_FILE_ENV, ""),
    ];
    tmux::new_session(&name, &dir, &env, &argv)?;
    // Something to see it through a startup prompt, which no hook reports.
    if hooks {
        if let Err(e) = startup::launch(app, task, session_id.as_deref()) {
            warnings.push(format!("could not watch the agent start: {e:#}"));
        }
    }
    let _ = app.db.touch_task(&task.id, now_secs());
    Ok(Running {
        tmux_session: name,
        started: true,
        warnings,
    })
}

/// The agent's command line. When its harness supports it, it resumes or
/// starts a recorded session, carries jeet's hooks and, given one, its
/// first prompt.
fn launch_argv(
    app: &App,
    task: &TaskRecord,
    spec: &AgentSpec,
    session: Option<&str>,
    prompt: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    let mut argv = spec.argv.clone();
    if prompt.is_some() && spec.harness.prompt_args("").is_none() {
        bail!(
            "{} takes no first prompt from jeet; start it with `jeet task open {}` and send              the message once it is running",
            spec.display(),
            task.slug
        );
    }
    if !spec.supports_sessions() {
        if let Some(id) = session {
            bail!(
                "{} keeps no sessions jeet knows of, so session {id} cannot be resumed",
                spec.display()
            );
        }
        return Ok((argv, None));
    }

    let dir = tasks::task_dir(app, task);
    let (id, resume) = pick_session(app, task, spec, &dir, session)?;
    let now = now_secs();
    // Recorded before launch, so the session is known even if the agent
    // never gets as far as its first hook.
    app.db.upsert_task_session(&TaskSessionRecord {
        session_id: id.clone(),
        task_id: task.id.clone(),
        parent_id: None,
        name: task.name.clone(),
        created_at: now,
        last_activity: now,
    })?;
    let args = match resume {
        true => spec.harness.resume_args(&id),
        false => spec.harness.new_session_args(&id, &task.name),
    };
    argv.extend(args.unwrap_or_default());
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    argv.extend(
        spec.harness
            .hook_args(&task_hooks(app, task, exe)?)
            .unwrap_or_default(),
    );
    if let Some(prompt) = prompt {
        argv.extend(spec.harness.prompt_args(prompt).unwrap_or_default());
    }
    Ok((argv, Some(id)))
}

/// Which session to run, and whether it is resumed. A recorded session that
/// never got a transcript (the agent died before writing one) cannot be
/// resumed, so it is started afresh under the same id.
fn pick_session(
    app: &App,
    task: &TaskRecord,
    spec: &AgentSpec,
    dir: &Path,
    session: Option<&str>,
) -> Result<(String, bool)> {
    let known = app.db.list_task_sessions(&task.id)?;
    let id = match session {
        Some(id) => {
            let id = id.trim();
            if !status::valid_session_id(id) {
                bail!("{id:?} is not a session id");
            }
            if !known.iter().any(|s| s.session_id == id) && !spec.harness.has_session(dir, id) {
                bail!(
                    "task {} has no session {id} that jeet or {} knows of; `jeet task ls` \
                     shows its tasks, and ctrl-s in a task's explorer its sessions",
                    task.name,
                    spec.harness.name()
                );
            }
            id.to_string()
        }
        None => match latest_session(dir, &known) {
            Some(id) => id,
            None => return Ok((Uuid::new_v4().to_string(), false)),
        },
    };
    let resume = spec.harness.has_session(dir, &id);
    Ok((id, resume))
}

/// The task's most recently active session, by what the index recorded and
/// what its hooks reported since.
pub fn latest_session(dir: &Path, known: &[TaskSessionRecord]) -> Option<String> {
    known
        .iter()
        .max_by_key(|s| {
            let reported = status::read_status(dir, &s.session_id).map_or(0, |st| st.at);
            (s.last_activity.max(reported), s.created_at)
        })
        .map(|s| s.session_id.clone())
}

/// jeet's hooks for a task's agent, with what it is denied (nothing for a
/// top-level task).
fn task_hooks(app: &App, task: &TaskRecord, exe: std::path::PathBuf) -> Result<HookSetup> {
    Ok(HookSetup {
        exe,
        allow: ALLOWED.map(str::to_string).to_vec(),
        deny: permissions::task_deny(app, task)?,
    })
}

/// jeet's hooks for the assistant, pre-approving the read-only jeet
/// commands and those of each capability `[assistant] allow` lists, so
/// allowed actions do not prompt, and denying force-pushes, merges outside
/// `jeet pr merge` and pushes to any of `branches`.
pub fn assistant_hooks(
    exe: std::path::PathBuf,
    settings: &AssistantConfig,
    branches: &[String],
) -> HookSetup {
    HookSetup {
        exe,
        allow: permissions::allowed_commands(&settings.allow),
        deny: permissions::denied_commands(branches),
    }
}

/// End the task's agent, if it is running, returning once it and everything
/// it started have exited. Without tmux there is none.
pub fn stop(task: &TaskRecord) -> Result<()> {
    if !tmux::available() {
        return Ok(());
    }
    tmux::kill(&session_name(task))
}

/// What [`interrupt`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interrupted {
    /// The keys were pressed: the agent's turn ends, its session goes on.
    Sent,
    /// It was doing this, not working on a turn, so nothing was pressed.
    NotWorking(Activity),
    /// Its input box holds what the user typed and has not sent, which a
    /// key could clear, so nothing was pressed.
    UserTyping,
    /// It waits on a prompt, such as a permission dialog, while someone is
    /// attached to its session: they may be typing an answer into the
    /// dialog, which [`Harness::has_draft`] does not see and Esc would
    /// discard, so nothing was pressed.
    ///
    /// [`Harness::has_draft`]: crate::harness::Harness::has_draft
    Watched,
    /// It is held at a startup prompt, such as the trust dialog, where the
    /// keys would answer the prompt (Esc exits), so nothing was pressed.
    AtStartup,
}

/// End the turn the task's agent is running, keeping its session: its
/// harness's interrupt keys, pressed once, only while it is working or
/// waiting on the user (and then only with no one attached to answer).
/// One with nothing to interrupt is not an error, so a caller can go over
/// many tasks.
pub fn interrupt(app: &App, task: &TaskRecord) -> Result<Interrupted> {
    if task.kind == TaskKind::Assistant {
        bail!("only the user interrupts the assistant, in its own window");
    }
    let name = session_name(task);
    let in_task = std::env::var(tasks::TASK_ID_ENV).is_ok_and(|id| id.trim() == task.id);
    if in_task || tmux::current_session().is_some_and(|s| s == name) {
        bail!(
            "this jeet runs inside tmux session {name}, task {}'s agent session; interrupt it \
             from outside that session",
            task.name
        );
    }
    let spec = AgentSpec::from_config(&app.config)?;
    let Some(keys) = spec.harness.interrupt_keys() else {
        bail!("{} cannot interrupt a turn", spec.harness.name());
    };
    let activity = activity(app, task, tmux::available());
    if !interruptible(&activity) {
        return Ok(Interrupted::NotWorking(activity));
    }
    let dir = tasks::task_dir(app, task);
    if at_startup(
        status::current_status(&dir).as_ref(),
        status::launched_at(&dir),
    ) {
        return Ok(Interrupted::AtStartup);
    }
    if spec.harness.has_draft(&tmux::capture_styled(&name)?) {
        return Ok(Interrupted::UserTyping);
    }
    if matches!(activity, Activity::Waiting { .. }) && tmux::attached(&name) {
        return Ok(Interrupted::Watched);
    }
    tmux::press(&name, keys)?;
    Ok(Interrupted::Sent)
}

/// Whether an agent doing `activity` may have a turn to interrupt; one
/// Waiting is still [`at_startup`] when no hook has said so.
fn interruptible(activity: &Activity) -> bool {
    matches!(
        activity,
        Activity::Running { .. } | Activity::Waiting { .. }
    )
}

/// Whether the agent whose latest status is `current`, last launched at
/// `launched`, may be at a startup prompt rather than in a turn: no hook
/// has reported yet (a live agent with no status shows as Waiting once it
/// has been starting a while), none has since this launch (the status is
/// left from an agent that died without its SessionEnd hook, and still
/// reads as it did then), or the startup watcher marked it at one.
fn at_startup(current: Option<&status::SessionStatus>, launched: Option<i64>) -> bool {
    current.is_none_or(|s| {
        s.event == status::STARTUP_PROMPT_EVENT || launched.is_some_and(|l| s.at < l)
    })
}

/// Fail when this jeet runs inside the task's agent session, whether as the
/// agent's own command or in a window the user opened there: ending the
/// session to `verb` the task would end this jeet with it, before it had
/// done anything.
pub fn refuse_inside_session(task: &TaskRecord, verb: &str) -> Result<()> {
    let name = session_name(task);
    let in_task = std::env::var(tasks::TASK_ID_ENV).is_ok_and(|id| id.trim() == task.id);
    if in_task || tmux::current_session().is_some_and(|s| s == name) {
        bail!(
            "this jeet runs inside tmux session {name}, the agent's session for task {}, \
             which ending the agent would end too; {verb} the task from outside that session",
            task.name
        );
    }
    Ok(())
}

/// What the task's agent is doing. Runs tmux and reads files, so it belongs
/// on a background thread, not in a draw.
pub fn activity(app: &App, task: &TaskRecord, tmux_ok: bool) -> Activity {
    activity_at(&tasks::task_dir(app, task), &session_name(task), tmux_ok)
}

/// [`activity`] for the task in `dir` whose tmux session is `session`, for
/// a thread that has no index to look the task up in.
pub fn activity_at(dir: &Path, session: &str, tmux_ok: bool) -> Activity {
    let alive = tmux_ok && tmux::pane_alive(session);
    status::activity_since(
        status::current_status(dir).as_ref(),
        alive,
        status::launched_at(dir),
        now_secs(),
    )
}

/// When anything last happened in the task: jeet touching it, a hook
/// reporting, or a harness recording a session.
pub fn last_activity(app: &App, task: &TaskRecord) -> i64 {
    last_activity_at(&tasks::task_dir(app, task), task.last_activity)
}

/// [`last_activity`] for the task in `dir` that jeet last touched at
/// `touched`.
pub fn last_activity_at(dir: &Path, touched: i64) -> i64 {
    [
        Some(touched),
        status::latest_status_mtime(dir),
        harness::latest_activity(dir),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;

    fn branches(n: usize) -> Vec<String> {
        let len = crate::permissions::MAX_BRANCH_NAME;
        (0..n).map(|i| format!("{i:0>len$}")).collect()
    }

    #[test]
    fn the_assistant_is_pre_approved_what_it_is_allowed() {
        let settings = AssistantConfig {
            allow: vec!["task.send".into()],
            notify: None,
            rules: Vec::new(),
        };
        let hooks = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()]);
        assert!(
            hooks.allow.contains(&"jeet task send".into()),
            "{:?}",
            hooks.allow
        );
        assert!(
            hooks.allow.contains(&"jeet task ls".into()),
            "{:?}",
            hooks.allow
        );
        assert!(
            !hooks.allow.contains(&"jeet task new".into()),
            "{:?}",
            hooks.allow
        );
        // A task with a parent reports to it without a prompt.
        assert!(ALLOWED.contains(&"jeet report"));
    }

    #[test]
    fn the_assistant_is_denied_merging_and_force_pushes_but_may_comment() {
        let settings = AssistantConfig {
            allow: Vec::new(),
            notify: None,
            rules: Vec::new(),
        };
        let hooks = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()]);
        for denied in ["gh pr merge", "git push --force*", "git push * main"] {
            assert!(hooks.deny.contains(&denied.into()), "{:?}", hooks.deny);
        }
        // Commenting and reviewing are the user's to approve at the prompt.
        assert!(!hooks.deny.iter().any(|d| d.starts_with("gh pr comment")
            || d.starts_with("gh issue comment")
            || d.starts_with("gh pr review")));
    }

    #[test]
    fn a_first_prompt_fits_in_one_tmux_command_with_the_settings() {
        // tmux refuses a command over 16KB; the prompt shares new-session's
        // with the settings, a long path to jeet, every capability or none,
        // and the pushes denied to as many default branches as jeet denies,
        // each as long as it denies.
        let every = crate::permissions::CAPABILITIES
            .iter()
            .map(|c| c.name.to_string())
            .collect();
        for allow in [every, Vec::new()] {
            let settings = AssistantConfig {
                allow,
                notify: None,
                rules: Vec::new(),
            };
            let exe = format!("/{}/jeet", "d".repeat(200));
            let hooks = assistant_hooks(
                exe.into(),
                &settings,
                &branches(crate::permissions::MAX_DEFAULT_BRANCHES),
            );
            let args = crate::harness::claude::ClaudeCode
                .hook_args(&hooks)
                .unwrap();
            let len: usize = args.iter().map(|a| a.len() + 1).sum();
            let budget = 16 * 1024 - 2048; // the rest of the command
            assert!(
                len + crate::messages::MAX_MESSAGE + 16 < budget,
                "{len} bytes of settings"
            );
        }
    }

    #[test]
    fn interruptible_only_while_working() {
        let working = [
            Activity::Running { detail: None },
            Activity::Waiting {
                detail: Some("Bash".into()),
            },
        ];
        let not = [
            Activity::NotStarted,
            Activity::NotRunning,
            Activity::Live,
            Activity::Starting,
            Activity::Idle { since: 0 },
            Activity::Ended,
        ];
        assert!(working.iter().all(interruptible));
        assert!(!not.iter().any(interruptible));
    }

    #[test]
    fn a_startup_prompt_is_never_interrupted() {
        // No hook yet: a live agent past its start reads as Waiting.
        assert!(matches!(
            status::activity_since(None, true, Some(0), 60),
            Activity::Waiting { .. }
        ));
        assert!(at_startup(None, Some(0)));
        // The startup watcher marked it at the trust dialog: Waiting too.
        let dir = tempfile::tempdir().unwrap();
        status::mark_startup_prompt(dir.path(), "s", "trust folder", 60).unwrap();
        let marked = status::current_status(dir.path());
        assert!(matches!(
            status::activity_since(marked.as_ref(), true, Some(0), 60),
            Activity::Waiting { .. }
        ));
        assert!(at_startup(marked.as_ref(), Some(0)));
        // A hook reported: past the startup.
        let hooked = status::SessionStatus {
            session_id: "s".into(),
            state: status::State::Waiting,
            detail: Some("Bash".into()),
            event: "Notification".into(),
            at: 61,
        };
        assert!(!at_startup(Some(&hooked), Some(0)));
        assert!(!at_startup(Some(&hooked), Some(61)));
        assert!(!at_startup(Some(&hooked), None));
        // A status left from an earlier launch, whose agent died mid-turn
        // without its SessionEnd hook: the relaunched one may be at a
        // startup prompt, though the status still reads Running.
        let stale = status::SessionStatus {
            state: status::State::Running,
            event: "UserPromptSubmit".into(),
            at: 50,
            ..hooked
        };
        assert!(matches!(
            status::activity_since(Some(&stale), true, Some(60), 70),
            Activity::Running { .. }
        ));
        assert!(at_startup(Some(&stale), Some(60)));
    }

    #[test]
    fn the_latest_session_counts_what_hooks_reported() {
        let dir = tempfile::tempdir().unwrap();
        let session = |id: &str, last_activity: i64| TaskSessionRecord {
            session_id: id.into(),
            task_id: "t".into(),
            parent_id: None,
            name: "t".into(),
            created_at: 1,
            last_activity,
        };
        let known = [session("a", 50), session("b", 10)];
        assert_eq!(latest_session(dir.path(), &known).as_deref(), Some("a"));

        let status_dir = status::status_dir(dir.path());
        std::fs::create_dir_all(&status_dir).unwrap();
        std::fs::write(
            status_dir.join("b.json"),
            r#"{"session_id":"b","state":"idle","event":"Stop","at":99}"#,
        )
        .unwrap();
        assert_eq!(latest_session(dir.path(), &known).as_deref(), Some("b"));
        assert!(latest_session(dir.path(), &[]).is_none());
    }
}
