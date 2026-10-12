//! A task's agent at runtime: started in the task's tmux session with
//! jeet's hooks, stopped with the task, and what it is doing now.
//!
//! An agent whose harness supports it gets a session id chosen by jeet, so
//! the session is recorded before the agent even runs, and hooks that report
//! its status and let it run `jeet worktree` and `jeet ls` without asking
//! (for Claude Code, a `--settings` file under `$JEET_HOME/run`, see
//! [`hook_args`]). Nothing is written into the task or any repository for
//! this. An agent whose harness supports none of it
//! just runs in the session, with no status and no sessions.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::agent::AgentSpec;
use crate::cd;
use crate::config::AssistantConfig;
use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::tasks::{TaskKind, TaskRecord, TaskSessionRecord};
use crate::harness::{self, HookSetup};
use crate::identity;
use crate::manager::run::{self, StartGate, StartGuard};
use crate::messages::{self, Delivered, Inbox, TmuxAgent};
use crate::permissions;
use crate::placement::{self, NodeRuntime};
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
    pub started: Started,
    pub warnings: Vec<String>,
}

/// Whether [`ensure_running`] started the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// It was not running, and was started now.
    Now,
    /// It was running already, and was given no prompt.
    AlreadyRunning,
    /// `live` of `max` tasks in the tree run already (`[managers]
    /// max_live`): it is queued to start when one ends, and `prompt` is
    /// handed back for its inbox.
    Queued {
        live: u32,
        max: u32,
        prompt: Option<String>,
    },
}

/// How a start gets past `[managers] max_live` (M22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Queued while it is full.
    Gate,
    /// Started anyway, with a warning: the user opening it at a terminal
    /// asked for it. A held task is refused either way.
    Bypass,
}

/// Make sure the task's agent is running where its placement says, starting
/// it if not: resuming `session` when given, else the task's latest
/// session, else a new one. An agent this starts gets `prompt` as its first
/// prompt; one already running does not. Given none, it starts with what
/// waits in its inbox, as a drain would (a brief queued on `max_live`, a
/// message held while it could not start), in `session` when one is named. A start is admitted first
/// ([`run::admit_start`]): refused while the task is held, and by
/// `admission` while `max_live` tasks run.
pub fn ensure_running(
    app: &App,
    task: &TaskRecord,
    session: Option<&str>,
    prompt: Option<&str>,
    admission: Admission,
) -> Result<Running> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant's sessions are opened with `jeet chat`");
    }
    // Running already: nothing starts, so there is nothing to admit, and a
    // held task's live pane is still there to attach to (M16). Attached to
    // only: one gone meanwhile is not started here, unadmitted.
    let name = session_name(task);
    if tmux::has_session(&name) && tmux::pane_alive(&name) {
        placement::runtime_for(task)?;
        return already_running(app, task, &name, session);
    }
    // Its inbox, then the start lock (M22), as a drain takes them. Only for
    // a task directory that is there: a missing one is the start's to refuse.
    // Refused now, not once a queued or held start comes to be made.
    if prompt.is_some() {
        if let Some(why) = first_prompt_refusal(app, task) {
            bail!(why);
        }
    }
    let dir = tasks::task_dir(app, task);
    let inbox = match prompt {
        None if dir.is_dir() => Some(Inbox::lock(&dir)?),
        _ => None,
    };
    let (guard, over) = match admission {
        Admission::Gate => match run::admit_start(app, task)? {
            StartGate::Go(guard) => (guard, None),
            StartGate::Queue { live, max, .. } => {
                return Ok(Running {
                    tmux_session: name,
                    started: Started::Queued {
                        live,
                        max,
                        prompt: prompt.map(str::to_string),
                    },
                    warnings: Vec::new(),
                })
            }
            StartGate::Held(holder) => bail!(run::held_refusal(task, &holder)),
        },
        Admission::Bypass => run::admit_over(app, task)?,
    };
    let mut running = match &inbox {
        Some(inbox) => start_with_inbox(app, task, session, inbox, &guard)?,
        None => ensure_running_admitted(app, task, session, prompt, &guard)?,
    };
    if let Some((live, max)) = over {
        running.warnings.push(format!(
            "over max_live ({live}/{max} tasks in the tree run already)"
        ));
    }
    Ok(running)
}

/// [`ensure_running`] for a start admitted already, under `_guard`: what
/// holds it passes it down, as the start lock is not taken twice.
pub fn ensure_running_admitted(
    app: &App,
    task: &TaskRecord,
    session: Option<&str>,
    prompt: Option<&str>,
    _guard: &StartGuard,
) -> Result<Running> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant's sessions are opened with `jeet chat`");
    }
    let running = placement::runtime_for(task)?.start(app, task, session, prompt)?;
    // Started, however it was admitted: no longer waits on `max_live` or a
    // resume, or a drain would start it again once it ends.
    if running.started == Started::Now {
        app.db.unqueue(&task.id)?;
    }
    Ok(running)
}

/// Start `task`, admitted under `guard`, resuming `session` when given,
/// with what waits for it in its locked `inbox` as its first prompt, as much of it as fits, or with none
/// when nothing does, or when its harness takes no first prompt from jeet
/// (what waits is left for a delivery to type in once it runs).
pub fn start_with_inbox(
    app: &App,
    task: &TaskRecord,
    session: Option<&str>,
    inbox: &Inbox,
    guard: &StartGuard,
) -> Result<Running> {
    let takes_prompt = AgentSpec::from_config(&app.config)
        .is_ok_and(|spec| spec.harness.prompt_args("").is_some());
    if !takes_prompt || inbox.read_for(None)?.is_empty() {
        return ensure_running_admitted(app, task, session, None, guard);
    }
    let agent = TmuxAgent::new(app, task).keep_warnings().resuming(session);
    let started = match messages::start_with_queue(inbox, &agent, Some(guard))? {
        // It came up meanwhile, and was typed the queue.
        Delivered::Typed => Started::AlreadyRunning,
        _ => Started::Now,
    };
    Ok(Running {
        tmux_session: session_name(task),
        started,
        warnings: agent.warnings(),
    })
}

/// A task's agent in a tmux session on this machine: the runtime of
/// [`placement::Placement::Local`].
pub struct LocalTmux;

impl NodeRuntime for LocalTmux {
    fn start(
        &self,
        app: &App,
        task: &TaskRecord,
        session: Option<&str>,
        prompt: Option<&str>,
    ) -> Result<Running> {
        start_tmux(app, task, session, prompt)
    }

    fn interrupt(&self, app: &App, task: &TaskRecord) -> Result<Interrupted> {
        interrupt_tmux(app, task)
    }
}

/// [`ensure_running`] in the task's tmux session.
fn start_tmux(
    app: &App,
    task: &TaskRecord,
    session: Option<&str>,
    prompt: Option<&str>,
) -> Result<Running> {
    tmux::require()?;
    let name = session_name(task);
    let mut warnings = Vec::new();
    // With `remain-on-exit` set, a session outlives its agent; one with
    // nothing left running in it is replaced rather than attached to.
    if tmux::has_session(&name) && !tmux::pane_alive(&name) {
        tmux::kill(&name)?;
    }
    if tmux::has_session(&name) {
        return already_running(app, task, &name, session);
    }

    let mut spec = AgentSpec::from_config(&app.config)?;
    // Made under a harness that had them, it is not started unguarded on
    // one configured since that lacks them.
    if task.kind == TaskKind::Manager {
        tasks::manager_harness(spec.harness)?;
    }
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
    // A review's session is filed under its worktree: picked up first, so
    // it is the one resumed, not a blank one started beside it.
    warnings.extend(crate::review::adopt_sessions(app, task));
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
    if let Err(e) = status::mark_launched(&dir, hooks, prompt.is_some()) {
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
    let pane = tmux::new_session(&name, &dir, &env, &argv)?;
    if let Err(e) = identity::record(
        &app.db,
        &task.id,
        AnchorKind::Pane,
        Some(&pane.id),
        pane.pid,
    ) {
        warnings.push(identity::unidentified(&task.slug, &e));
    }
    // Something to see it through a startup prompt, which no hook reports.
    if hooks {
        if let Err(e) = startup::launch(app, task, session_id.as_deref()) {
            warnings.push(format!("could not watch the agent start: {e:#}"));
        }
    }
    let _ = app.db.touch_task(&task.id, now_secs());
    Ok(Running {
        tmux_session: name,
        started: Started::Now,
        warnings,
    })
}

/// The task's agent, running already in tmux session `name`: attached to,
/// not started, with what to warn of.
fn already_running(
    app: &App,
    task: &TaskRecord,
    name: &str,
    session: Option<&str>,
) -> Result<Running> {
    let mut warnings = Vec::new();
    // Only a pane jeet started is the task's agent: one made some other
    // way, or by a jeet from before anchors, is never adopted as it.
    let anchors = app.db.task_anchors(&task.id, AnchorKind::Pane)?;
    if !anchors.iter().any(identity::live) {
        warnings.push(format!(
            "tmux session {name} was started by an older jeet or outside jeet, so jeet \
             cannot confirm its agent is task {}'s and refuses its jeet commands; `jeet task \
             restart {}` resumes it in a new one, or from a terminal outside jeet's tmux \
             sessions, `jeet task adopt {}` vouches for it",
            task.slug, task.slug, task.slug
        ));
    }
    if let Some(id) = session {
        warnings.push(format!(
            "the agent is already running in tmux session {name}, so session {id} was not \
             resumed; end it there and open the task again to switch"
        ));
    }
    Ok(Running {
        tmux_session: name.to_string(),
        started: Started::AlreadyRunning,
        warnings,
    })
}

/// Why `task`'s agent cannot be started with a first prompt on `spec`.
fn no_first_prompt(spec: &AgentSpec, task: &TaskRecord) -> String {
    format!(
        "{} takes no first prompt from jeet; start it with `jeet task open {}` and send \
         the message once it is running",
        spec.display(),
        task.slug
    )
}

/// Why `task`'s agent cannot be started with a first prompt, when its
/// harness takes none from jeet: asked before a start with one is queued
/// or held, as well as made, so a brief or message is never left waiting
/// for a start that would not give it.
pub fn first_prompt_refusal(app: &App, task: &TaskRecord) -> Option<String> {
    let spec = AgentSpec::from_config(&app.config).ok()?;
    match spec.harness.prompt_args("") {
        Some(_) => None,
        None => Some(no_first_prompt(&spec, task)),
    }
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
        bail!(no_first_prompt(spec, task));
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
    let hooks = task_hooks(app, task, exe)?;
    // The task's agent is not running, so no other session's file is in use.
    argv.extend(hook_args(&app.home, spec.harness, &hooks, &id, &[])?);
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
                    task.slug,
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
/// top-level task). A manager, started with nobody at its pane, also has
/// the commands of what it holds pre-approved, read at each start, as the
/// assistant has those of `[assistant] allow`.
fn task_hooks(app: &App, task: &TaskRecord, exe: std::path::PathBuf) -> Result<HookSetup> {
    let mut allow = ALLOWED.map(str::to_string).to_vec();
    match task.kind {
        TaskKind::Manager => {
            let held: Vec<String> = permissions::effective(app, task)?
                .into_iter()
                .map(str::to_string)
                .collect();
            for command in permissions::allowed_commands(&held) {
                // The assistant's todo list, which a manager keeps by hand.
                let own = permissions::OWN.contains(&command.as_str());
                if !own && !allow.contains(&command) {
                    allow.push(command);
                }
            }
        }
        TaskKind::Work | TaskKind::Review | TaskKind::Assistant => {}
    }
    Ok(HookSetup {
        exe,
        task_id: task.id.clone(),
        allow,
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
    task_id: &str,
) -> HookSetup {
    HookSetup {
        exe,
        task_id: task_id.to_string(),
        allow: permissions::allowed_commands(&settings.allow),
        deny: permissions::denied_commands(branches),
    }
}

/// Where the settings files of task `task_id`'s launches go: under
/// `$JEET_HOME/run`, outside the task and every worktree, so nothing is
/// written where the agent works.
pub fn settings_dir(home: &Path, task_id: &str) -> PathBuf {
    home.join("run").join(task_id)
}

/// jeet's hooks as the arguments of a launch of `session`. A harness that
/// reads them from a file ([`Harness::hook_file`]) gets one, readable by the
/// user alone, at `<session>.json` in [`settings_dir`], and the arguments
/// name it: on the command line they would share tmux's 16KB limit with
/// the first prompt, and each hook or rule added would push out another.
/// The file is replaced whole at each launch, never truncated where a
/// running agent may read it, and goes with the task (archive, delete).
/// The launch also removes the files of the task's other sessions, but for
/// those in `running` and any written in the last [`SETTINGS_GRACE`] (a
/// launch racing this one), so a task whose sessions come and go, like the
/// assistant's chats, keeps no more than it runs.
pub fn hook_args(
    home: &Path,
    harness: &dyn harness::Harness,
    hooks: &HookSetup,
    session: &str,
    running: &[String],
) -> Result<Vec<String>> {
    if !status::valid_session_id(session) {
        bail!("{session:?} is not a session id");
    }
    if !status::valid_session_id(&hooks.task_id) {
        bail!("{:?} is not a task id", hooks.task_id);
    }
    let path = settings_dir(home, &hooks.task_id).join(format!("{session}.json"));
    match harness.hook_file(hooks, &path) {
        Some((contents, args)) => {
            write_private(&path, &contents)
                .with_context(|| format!("write the agent's settings to {}", path.display()))?;
            prune_settings(&path, running);
            Ok(args)
        }
        None => Ok(harness.hook_args(hooks).unwrap_or_default()),
    }
}

/// How recently written a settings file is left alone when another launch
/// prunes: long enough for its agent to have read it.
const SETTINGS_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Remove the `<session>.json` files beside `kept` but for `kept` itself,
/// those of the `running` sessions and those written within
/// [`SETTINGS_GRACE`]. Best effort: a file left behind goes with the task.
fn prune_settings(kept: &Path, running: &[String]) {
    let Some(dir) = kept.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == kept || path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(session) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if running.iter().any(|r| r == session) {
            continue;
        }
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map_or(true, |age| age < SETTINGS_GRACE))
            .unwrap_or(true);
        if !recent {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write `contents` to `path` with mode 0600, in a directory only the user
/// can enter, through a new file renamed into place.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let Some(dir) = path.parent() else {
        bail!("{} has no directory", path.display());
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = dir.join(format!(".{}.tmp", Uuid::new_v4()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// Remove the settings files of task `task_id`'s launches, once its agent
/// is stopped.
pub fn remove_settings(home: &Path, task_id: &str) -> Result<()> {
    if !status::valid_session_id(task_id) {
        bail!("{task_id:?} is not a task id");
    }
    let dir = settings_dir(home, task_id);
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", dir.display()))
        }
        _ => Ok(()),
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
    /// It waits on a prompt, such as a permission dialog, or shows a dialog
    /// of its own ([`Harness::in_dialog`]), while someone is attached to
    /// its session: they may be typing an answer into the
    /// dialog, which [`Harness::has_draft`] does not see and Esc would
    /// discard, so nothing was pressed. A dialog of its own may already be
    /// answered: for Claude Code no hook reports the answer, so it counts
    /// until the agent's next tool call or the end of its turn.
    ///
    /// [`Harness::has_draft`]: crate::harness::Harness::has_draft
    /// [`Harness::in_dialog`]: crate::harness::Harness::in_dialog
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
    if is_own_session(task) {
        bail!(
            "this jeet runs inside tmux session {}, task {}'s agent session; interrupt it \
             from outside that session",
            session_name(task),
            task.name
        );
    }
    placement::runtime_for(task)?.interrupt(app, task)
}

/// Whether this jeet runs inside the task's agent session: as the agent's
/// own command (`$JEET_TASK_ID`), or in a window the user opened there.
pub fn is_own_session(task: &TaskRecord) -> bool {
    let in_task = std::env::var(tasks::TASK_ID_ENV).is_ok_and(|id| id.trim() == task.id);
    in_task || tmux::current_session().is_some_and(|s| s == session_name(task))
}

/// [`interrupt`] in the task's tmux session.
fn interrupt_tmux(app: &App, task: &TaskRecord) -> Result<Interrupted> {
    let name = session_name(task);
    let spec = AgentSpec::from_config(&app.config)?;
    let Some(keys) = spec.harness.interrupt_keys() else {
        bail!("{} cannot interrupt a turn", spec.harness.name());
    };
    let activity = activity(app, task, tmux::available());
    if !interruptible(&activity) {
        return Ok(Interrupted::NotWorking(activity));
    }
    // A session kept up by a window the user opened, its agent's pane dead
    // (`remain-on-exit`): keys sent there would "succeed", and the crashed
    // agent's stale status be charged as a turn.
    if !tmux::agent_alive(&name) {
        return Ok(Interrupted::NotWorking(Activity::NotRunning));
    }
    let dir = tasks::task_dir(app, task);
    let current = status::current_status(&dir);
    if at_startup(current.as_ref(), status::launched_at(&dir)) {
        return Ok(Interrupted::AtStartup);
    }
    if spec.harness.has_draft(&tmux::capture_styled(&name)?) {
        return Ok(Interrupted::UserTyping);
    }
    let asking = matches!(activity, Activity::Waiting { .. })
        || current.as_ref().is_some_and(|s| spec.harness.in_dialog(s));
    if asking && tmux::attached(&name) {
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
    if is_own_session(task) {
        bail!(
            "this jeet runs inside tmux session {}, the agent's session for task {}, \
             which ending the agent would end too; {verb} the task from outside that session",
            session_name(task),
            task.name
        );
    }
    Ok(())
}

/// Whether the task's agent runs, in its tmux session, without jeet knowing
/// its process: an older jeet started it, or something other than jeet.
/// Its jeet commands that need to know who calls are refused, until it is
/// restarted (`jeet task restart`) or adopted (`jeet task adopt`). Runs
/// tmux, so it belongs where [`activity`] does.
pub fn unanchored(app: &App, task: &TaskRecord) -> bool {
    unanchored_in(&app.db, &task.id, &session_name(task))
}

/// [`unanchored`] for task `task_id`, whose tmux session is `session`. A
/// lookup that fails says nothing.
pub fn unanchored_in(db: &crate::db::Database, task_id: &str, session: &str) -> bool {
    let Some(live) = tmux::live_agent_pane(session) else {
        return false;
    };
    match db.task_anchors(task_id, AnchorKind::Pane) {
        Ok(anchors) => !anchors
            .iter()
            .any(|a| a.pid == live.pane.pid && identity::live(a)),
        Err(_) => false,
    }
}

/// Why an agent doing `activity`, with unsent text in its prompt when
/// `draft`, should not be ended now, if it should not: for a restart,
/// which would cut off the turn, the question or what was typed.
pub fn busy_reason(activity: &Activity, draft: bool) -> Option<String> {
    match activity {
        Activity::Running { .. } => Some(format!("is mid-turn ({})", activity.label())),
        Activity::Waiting { .. } => Some(format!("waits on you ({})", activity.label())),
        Activity::Starting => Some("is starting".to_string()),
        _ if draft => Some("has text in its prompt that was not sent".to_string()),
        _ => None,
    }
}

/// What the task's agent is doing. Runs tmux and reads files, so it belongs
/// on a background thread, not in a draw.
pub fn activity(app: &App, task: &TaskRecord, tmux_ok: bool) -> Activity {
    activity_at(&tasks::task_dir(app, task), &session_name(task), tmux_ok)
}

/// [`activity`] as lists show it: a review task's headless review job,
/// still running, counts as its agent running. Only for display: what
/// routes a delivery or charges a budget asks of the agent alone.
pub fn shown_activity(app: &App, task: &TaskRecord, tmux_ok: bool) -> Activity {
    crate::review::with_job(
        activity(app, task, tmux_ok),
        &crate::review::state_files_of_task(app, task),
    )
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

    #[test]
    fn a_restart_waits_for_a_turn_a_question_a_start_or_a_draft() {
        let running = Activity::Running { detail: None };
        let waiting = Activity::Waiting { detail: None };
        for busy in [&running, &waiting, &Activity::Starting] {
            assert!(busy_reason(busy, false).is_some(), "{busy:?}");
        }
        let idle = Activity::Idle { since: 0 };
        assert_eq!(busy_reason(&idle, false), None);
        assert_eq!(busy_reason(&Activity::Live, false), None);
        assert!(busy_reason(&idle, true).is_some());
    }

    #[test]
    fn the_assistant_is_pre_approved_what_it_is_allowed() {
        let settings = AssistantConfig {
            allow: vec!["task.send".into()],
            notify: None,
            rules: Vec::new(),
        };
        let hooks = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()], "a");
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
    fn a_manager_has_the_commands_of_what_it_holds_pre_approved() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let mut app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
        };
        app.config.assistant.allow = vec![
            "task.create".into(),
            "task.send".into(),
            "manager.create".into(),
        ];
        let root = crate::assistant::ensure(&app).unwrap();
        let row = |slug: &str, kind| TaskRecord {
            id: format!("{slug}-id"),
            slug: slug.into(),
            name: slug.into(),
            kind,
            parent_id: Some(root.id.clone()),
            ..root.clone()
        };
        let (api, worker) = (row("api", TaskKind::Manager), row("wk", TaskKind::Work));
        for (task, grant) in [
            (&api, ["task.create", "task.delete", "manager.create"]),
            (&worker, ["task.create", "task.send", "manager.create"]),
        ] {
            app.db.insert_task(task).unwrap();
            app.db.set_grants(&task.id, &grant).unwrap();
        }
        let hooks = task_hooks(&app, &api, "/bin/jeet".into()).unwrap();
        for allowed in [
            "jeet task new",
            "jeet manager new",
            "jeet report",
            "jeet manager show",
        ] {
            assert!(
                hooks.allow.contains(&allowed.to_string()),
                "{:?}",
                hooks.allow
            );
        }
        // Granted but cut by the assistant, or never granted.
        for withheld in ["jeet task delete", "jeet task send"] {
            assert!(
                !hooks.allow.contains(&withheld.to_string()),
                "{:?}",
                hooks.allow
            );
        }
        assert!(!hooks.allow.iter().any(|a| a == "jeet" || a.ends_with('*')));
        assert!(!hooks.allow.contains(&"jeet todo".to_string()));
        // A worker asks at the prompt, as before.
        let hooks = task_hooks(&app, &worker, "/bin/jeet".into()).unwrap();
        assert_eq!(hooks.allow, ALLOWED.map(str::to_string));
    }

    #[test]
    fn the_assistant_is_denied_merging_and_force_pushes_but_may_comment() {
        let settings = AssistantConfig {
            allow: Vec::new(),
            notify: None,
            rules: Vec::new(),
        };
        let hooks = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()], "a");
        for denied in ["gh pr merge", "git push --force*", "git push * main"] {
            assert!(hooks.deny.contains(&denied.into()), "{:?}", hooks.deny);
        }
        // Commenting and reviewing are the user's to approve at the prompt.
        assert!(!hooks.deny.iter().any(|d| d.starts_with("gh pr comment")
            || d.starts_with("gh issue comment")
            || d.starts_with("gh pr review")));
    }

    #[test]
    fn the_settings_go_in_a_private_file_so_a_first_prompt_fits_one_tmux_command() {
        // tmux refuses a command over 16KB, and the prompt shares
        // new-session's with the hooks: however many rules they carry, and
        // however long the default branches, only the file's path is on it.
        let home = tempfile::tempdir().unwrap();
        let every = crate::permissions::CAPABILITIES
            .iter()
            .map(|c| c.name.to_string())
            .collect();
        let long = format!("release/{}", "p".repeat(200));
        let branches: Vec<String> = (0..crate::permissions::MAX_DEFAULT_BRANCHES)
            .map(|i| format!("{long}-{i}"))
            .collect();
        let task = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        let settings = AssistantConfig {
            allow: every,
            notify: None,
            rules: Vec::new(),
        };
        let exe = format!("/{}/jeet", "d".repeat(200));
        let hooks = assistant_hooks(exe.into(), &settings, &branches, &task);
        let claude = &crate::harness::claude::ClaudeCode;
        let args = hook_args(home.path(), claude, &hooks, &session, &[]).unwrap();
        let path = settings_dir(home.path(), &task).join(format!("{session}.json"));
        assert_eq!(args, ["--settings".to_string(), path.display().to_string()]);
        let len: usize = args.iter().map(|a| a.len() + 1).sum();
        let budget = 16 * 1024 - 2048; // the rest of the command
        assert!(len + crate::messages::MAX_MESSAGE + 16 < budget, "{len}");
        let inline: usize = claude
            .hook_args(&hooks)
            .unwrap()
            .iter()
            .map(String::len)
            .sum();
        assert!(inline + crate::messages::MAX_MESSAGE > budget, "{inline}");

        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
        };
        let deny = read()["permissions"]["deny"].clone();
        for branch in &branches {
            let rule = serde_json::json!(format!("Bash(git push * {branch})"));
            assert!(deny.as_array().unwrap().contains(&rule), "{branch}");
        }
        // A relaunch replaces it whole, and leaves nothing else behind.
        let fewer = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()], &task);
        hook_args(home.path(), claude, &fewer, &session, &[]).unwrap();
        assert!(!read().to_string().contains(&long));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        // Ids name files, so they must not name anything else.
        assert!(hook_args(home.path(), claude, &fewer, "../x", &[]).is_err());
        assert!(remove_settings(home.path(), "..").is_err());
        remove_settings(home.path(), &task).unwrap();
        assert!(!settings_dir(home.path(), &task).exists());
        remove_settings(home.path(), &task).unwrap();
        // A harness with no settings file keeps its hooks on the command line.
        let generic = &crate::harness::Generic;
        assert!(hook_args(home.path(), generic, &fewer, &session, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_launch_removes_the_settings_of_sessions_no_longer_running() {
        // The assistant starts a session per chat and is never archived:
        // without this, every chat would leave its file behind for good.
        let home = tempfile::tempdir().unwrap();
        let task = uuid::Uuid::new_v4().to_string();
        let settings = AssistantConfig {
            allow: Vec::new(),
            notify: None,
            rules: Vec::new(),
        };
        let hooks = assistant_hooks("/bin/jeet".into(), &settings, &["main".into()], &task);
        let claude = &crate::harness::claude::ClaudeCode;
        let dir = settings_dir(home.path(), &task);
        let launch = |session: &str, running: &[String]| {
            hook_args(home.path(), claude, &hooks, session, running).unwrap();
        };
        let age = |session: &str| {
            let old = std::time::SystemTime::now() - 2 * SETTINGS_GRACE;
            std::fs::File::options()
                .write(true)
                .open(dir.join(format!("{session}.json")))
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        let [ended, live, fresh, new] = std::array::from_fn(|_| uuid::Uuid::new_v4().to_string());
        for session in [&ended, &live] {
            launch(session, &[]);
        }
        age(&ended);
        age(&live);
        let running = std::slice::from_ref(&live);
        launch(&fresh, running);
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        launch(&new, running);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // `ended` is gone; `live`'s window still runs; `fresh` is too new.
        let mut want = vec![
            format!("{live}.json"),
            format!("{fresh}.json"),
            format!("{new}.json"),
            "notes.txt".to_string(),
        ];
        want.sort();
        assert_eq!(left, want);
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
            busy_since: None,
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
