//! What a task's agent is doing, as its harness's hooks report it.
//!
//! jeet launches the agent with hooks (see [`Harness::hook_args`]) that run
//! the hidden `jeet task-hook <Event>`. Each run hands the hook's stdin to
//! the harness, which says what it means as a [`HookEvent`], and rewrites
//! `<task>/.jeet/status/<session>.json` with the state the event leaves that
//! session in. The tasks list reads those files back, together with whether
//! the task's tmux pane is still alive, into an [`Activity`].
//!
//! A hook runs on every tool call, so it has to be quick (well under 50ms),
//! must never print to stdout — the agent would read it as hook output —
//! and always exits 0: a broken status line must never get in its way.
//!
//! The exceptions: a turn ending ([`EventKind::Stopped`]) that finds
//! messages queued for the agent by `jeet task send` prints the harness's
//! [`Harness::continue_output`] with them, so the agent carries on with them
//! instead of stopping. A turn the user interrupts ends with no Stop, so the
//! agent reporting itself idle ([`EventKind::Idle`]) types anything still
//! queued into its pane instead, as does a turn ending for a harness that
//! cannot carry on. And the assistant's session start prints the summary of
//! the user's work it starts every session with.
//!
//! A task in the tree is charged for what its agent spends (M19): a turn
//! for each turn its own agent ends, and the time it was busy (running, or
//! waiting on a prompt) from `busy_since`. A task paused or stopped, itself
//! or above it, is handed nothing when its turn ends or it sits idle.

use std::fs::OpenOptions;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant;
use crate::context::App;
use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskSessionRecord};
use crate::harness::{self, EventKind, Harness, HookEvent, StartSource};
use crate::identity;
use crate::manager::budget::{self, Threshold};
use crate::manager::run;
use crate::messages;
use crate::reports::{self, StatusEvent};
use crate::tasks;
use crate::worktrees::now_secs;

/// More than any hook payload jeet cares about; a pasted novel of a prompt
/// is cut off rather than held in memory.
const MAX_INPUT: u64 = 4 * 1024 * 1024;

/// How much of a prompt or message is kept as the status detail.
pub const DETAIL_MAX: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Waiting for the next prompt.
    Idle,
    /// Working on a prompt.
    Running,
    /// Stopped mid-prompt to ask the user something: a permission, a
    /// question.
    Waiting,
    /// The session is over.
    Ended,
}

/// One session's status file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The hook event that wrote it.
    pub event: String,
    /// Unix seconds of that event: for `idle`, idle since when.
    pub at: i64,
    /// For `running` and `waiting`: when the busy spell it is part of
    /// began, which the task's budget is charged from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_since: Option<i64>,
}

/// The status `event` leaves a session in, or `None` for an event that
/// says nothing about it.
pub fn status_for(event: &HookEvent, now: i64) -> Option<SessionStatus> {
    let session_id = event.session_id.clone()?;
    let (state, detail) = match event.kind.clone()? {
        EventKind::SessionStarted(_) => (State::Idle, None),
        EventKind::Working(detail) => (State::Running, detail),
        EventKind::Idle => (State::Idle, None),
        EventKind::NeedsInput(detail) => (State::Waiting, detail),
        EventKind::Stopped => (State::Idle, None),
        EventKind::Ended { .. } => (State::Ended, None),
    };
    Some(SessionStatus {
        session_id,
        state,
        detail,
        event: event.name.clone(),
        at: now,
        busy_since: None,
    })
}

/// The first non-blank line of `text`, cut to `max` characters.
pub fn first_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= max {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(max - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// Session ids name files, so only the characters a uuid has are allowed.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn status_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(tasks::MARKER_DIR).join("status")
}

/// Seconds an agent with hooks is "starting" before its first hook, after
/// which it is taken to be stuck at a startup prompt.
pub const STARTING_SECS: i64 = 15;

/// Touched when an agent whose harness has hooks is launched, so that one
/// with no status yet reads as starting rather than running.
fn launched_path(task_dir: &Path) -> PathBuf {
    task_dir.join(tasks::MARKER_DIR).join("launched")
}

/// Note that the task's agent was just launched, `hooks` saying whether its
/// harness reports its status.
pub fn mark_launched(task_dir: &Path, hooks: bool) -> Result<()> {
    let path = launched_path(task_dir);
    if !hooks {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(task_dir.join(tasks::MARKER_DIR))?;
    std::fs::write(&path, b"").with_context(|| format!("write {}", path.display()))
}

/// When an agent with hooks was last launched in the task.
pub fn launched_at(task_dir: &Path) -> Option<i64> {
    crate::agent::mtime_secs(&std::fs::metadata(launched_path(task_dir)).ok()?)
}

/// Touched when jeet types a message into the task's agent, so a pause can
/// tell a turn about to start from an idle agent.
fn typed_path(task_dir: &Path) -> PathBuf {
    task_dir.join(tasks::MARKER_DIR).join("typed")
}

/// Note that a message was just typed into the task's agent.
pub fn mark_typed(task_dir: &Path) -> Result<()> {
    let path = typed_path(task_dir);
    std::fs::create_dir_all(task_dir.join(tasks::MARKER_DIR))?;
    std::fs::write(&path, b"").with_context(|| format!("write {}", path.display()))
}

/// Whether the task's agent was handed input in the last [`STARTING_SECS`]
/// (launched with hooks, its first prompt on its command line, or typed a
/// message) that no hook has reported on since: a turn that may be about
/// to start, though its status does not say so yet. Not one the startup
/// watcher found held at a startup prompt.
pub fn input_unreported(task_dir: &Path, now: i64) -> bool {
    let typed = std::fs::metadata(typed_path(task_dir))
        .ok()
        .and_then(|m| crate::agent::mtime_secs(&m));
    let Some(given) = launched_at(task_dir).max(typed) else {
        return false;
    };
    if now - given >= STARTING_SECS {
        return false;
    }
    // In whole seconds: a status from the second it was given may be the
    // one before it (SessionStart's), so only a later one has seen it.
    current_status(task_dir).is_none_or(|s| s.event != STARTUP_PROMPT_EVENT && s.at <= given)
}

/// The event of a status [`mark_startup_prompt`] wrote.
pub const STARTUP_PROMPT_EVENT: &str = "StartupPrompt";

/// Mark session `session_id` as waiting for the user at its startup:
/// `detail` says on what. Its first hook replaces this.
pub fn mark_startup_prompt(
    task_dir: &Path,
    session_id: &str,
    detail: &str,
    now: i64,
) -> Result<()> {
    write_status(
        task_dir,
        &SessionStatus {
            session_id: session_id.to_string(),
            state: State::Waiting,
            detail: Some(detail.to_string()),
            event: STARTUP_PROMPT_EVENT.to_string(),
            at: now,
            busy_since: None,
        },
    )
}

/// The event of the status a pause leaves an interrupted session in.
pub const PAUSED_EVENT: &str = "Paused";

/// Mark the task's latest session idle, its turn ended by a pause: an
/// interrupted turn ends with no Stop hook to say so. Nothing for a task
/// with no status.
pub fn mark_paused(task_dir: &Path, now: i64) -> Result<()> {
    let Some(current) = current_status(task_dir) else {
        return Ok(());
    };
    write_status(
        task_dir,
        &SessionStatus {
            session_id: current.session_id,
            state: State::Idle,
            detail: Some("paused".to_string()),
            event: PAUSED_EVENT.to_string(),
            at: now,
            busy_since: None,
        },
    )
}

/// `jeet task-hook [--task <id>] <event>`: record what the hook on stdin
/// says, as the harness called `harness` reports it, for `task`, the one
/// jeet launched the agent for (none from hooks older than `--task`).
/// Never fails, never prints to stdout; what goes wrong is noted on stderr,
/// which the agent keeps out of the conversation.
pub fn run_hook(harness: &str, event: &str, task: Option<&str>) {
    let Some(found) = harness::by_id(harness) else {
        eprintln!("jeet task-hook {event}: no harness {harness:?}");
        return;
    };
    let mut raw = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut raw);
    }
    if let Err(e) = handle_hook(found, event, task, &raw, now_secs()) {
        eprintln!("jeet task-hook {event}: {e:#}");
    }
}

fn handle_hook(
    harness: &dyn Harness,
    event: &str,
    task: Option<&str>,
    raw: &str,
    now: i64,
) -> Result<()> {
    let hook = harness.parse_hook(event, raw)?;
    let event = hook.name.as_str();
    let Some(session_id) = hook.session_id.as_deref() else {
        return Ok(());
    };
    if !valid_session_id(session_id) {
        anyhow::bail!("{session_id:?} is not a session id");
    }
    let launched_for = task.map(str::trim).filter(|t| !t.is_empty());
    let mut app = None;
    let Some((task_dir, task_id)) = locate_task(launched_for, &hook, &mut app)? else {
        // Not a jeet task: nothing to report to.
        return Ok(());
    };
    let ctx = hook_ctx(app, &task_dir, &task_id, launched_for, &hook, now)?;

    if let (Some(EventKind::SessionStarted(source)), app) = (&hook.kind, &ctx.app) {
        if let Some(task) = &ctx.task {
            let assistant = task.kind == TaskKind::Assistant;
            // The window this session runs in names the one it follows on
            // from, and from now on is found by this one.
            let before = match assistant {
                true => assistant::adopt_window(session_id),
                false => None,
            };
            if let Some(before) = &before {
                // What was queued for the session before goes on to it.
                if let Err(e) = messages::Inbox::lock(&task_dir)
                    .and_then(|inbox| inbox.readdress(before, session_id))
                {
                    eprintln!("jeet task-hook {event}: {e:#}");
                }
            }
            record_session(app, task, session_id, *source, before.as_deref(), now)?;
            // The harness adds this to the conversation; a summary that
            // cannot be made costs the session its context, not its start.
            if assistant && harness.session_context_output("").is_some() {
                match assistant::session_context(app, task, session_id, now) {
                    Ok(context) => {
                        if let Some(output) = harness.session_context_output(&context) {
                            println!("{output}");
                        }
                    }
                    Err(e) => eprintln!("jeet task-hook {event}: {e:#}"),
                }
            } else if task.parent_id.is_some()
                && harness.session_context_output("").is_some()
                && attributed(launched_for, &task.id, app)
            {
                // What its parent asked of it, each time a session starts,
                // /clear included.
                match reports::brief_context(app, task) {
                    Ok(brief) => {
                        if let Some(output) = harness.session_context_output(&brief) {
                            println!("{output}");
                        }
                    }
                    Err(e) => eprintln!("jeet task-hook {event}: {e:#}"),
                }
            }
        }
        // A compaction can happen mid-prompt; the session carries on in
        // whatever state it was in.
        if *source == StartSource::Compact && read_status(&task_dir, session_id).is_some() {
            return Ok(());
        }
    }
    // The assistant's turn ending is when tasks idle too long are reported
    // to it, whatever its harness: before its inbox is handed over, so what
    // this queues comes with it.
    if let Some(EventKind::Stopped) = &hook.kind {
        if let Err(e) = sweep_if_assistant(&ctx.app, &task_id, now) {
            eprintln!("jeet task-hook {event}: {e:#}");
        }
    }
    let result = match hook.kind {
        // A harness that cannot carry on past a turn's end is idle at it.
        Some(EventKind::Stopped) if harness.continue_output("").is_some() => {
            stop(harness, &task_dir, &hook, session_id, &ctx, now).map(|output| {
                if let Some(output) = output {
                    println!("{output}");
                }
            })
        }
        Some(EventKind::Stopped | EventKind::Idle) => idle_prompt(&task_dir, &hook, &ctx, now),
        _ => write_event_status(&task_dir, &hook, &ctx, now),
    };
    // Only now, with the task's inbox no longer locked.
    if let (Threshold::Hit100, Some(task)) = (ctx.threshold, &ctx.task) {
        budget::report_blocked(&ctx.app, task);
    }
    if let Err(e) = report_step(&hook, &ctx, launched_for, now) {
        eprintln!("jeet task-hook {event}: {e:#}");
    }
    // A slot under `max_live` may be free now.
    if let Some(EventKind::Ended { cleared: false }) = hook.kind {
        if let Err(e) = spawn_drain(&ctx.app, &task_id) {
            eprintln!("jeet task-hook {event}: {e:#}");
        }
    }
    result
}

/// What [`handle_hook`] knows of the hook's task before it acts on the
/// event, worked out in one step before any of it ([`hook_ctx`]).
struct HookCtx {
    app: App,
    /// The task, unless its row is gone.
    task: Option<TaskRecord>,
    /// Whether the hook is the task's own agent's ([`attributed`]): asked
    /// only of a task in the tree, for an event that charges it or that its
    /// parent hears of.
    attributed: bool,
    /// The session's status before this event.
    prev: Option<SessionStatus>,
    /// The `busy_since` of a running or waiting status this event writes:
    /// the spell it carries on, or one that starts now.
    busy_since: i64,
    /// What holds the task, itself or above, once this event has charged
    /// it: asked at a turn's end, sitting idle and the session's end.
    hold: Option<RunState>,
    /// The budget threshold this event's charge newly crossed.
    threshold: Threshold,
}

/// The [`HookCtx`] for the hook of task `task_id`: opening the app when the
/// task was found without it (by the session's directory), and, for an
/// attributed turn's end, charging the task a turn and the time it was
/// busy, or at the session's end, or sitting idle after a spell no turn's
/// end closed, the busy time left, and checking its budget ([`budget::enforce`]) before what holds it is read, so a pause
/// the charge makes holds this same turn's end.
fn hook_ctx(
    app: Option<App>,
    task_dir: &Path,
    task_id: &str,
    launched_for: Option<&str>,
    hook: &HookEvent,
    now: i64,
) -> Result<HookCtx> {
    let ours = |app: &App| attributed(launched_for, task_id, app);
    hook_ctx_with(app, &App::open, task_dir, task_id, hook, now, &ours)
}

/// [`hook_ctx`], opening the app with `open` and asking `ours` whether the
/// hook is the task's own agent's.
fn hook_ctx_with(
    app: Option<App>,
    open: &dyn Fn() -> Result<App>,
    task_dir: &Path,
    task_id: &str,
    hook: &HookEvent,
    now: i64,
    ours: &dyn Fn(&App) -> bool,
) -> Result<HookCtx> {
    let app = match app {
        Some(app) => app,
        None => open()?,
    };
    let task = app.db.get_task(task_id)?;
    let session_id = hook.session_id.as_deref().unwrap_or_default();
    let prev = read_status(task_dir, session_id);
    let busy = prev
        .as_ref()
        .filter(|p| matches!(p.state, State::Running | State::Waiting))
        .and_then(|p| p.busy_since);
    let kind = hook.kind.as_ref();
    // A turn's end, the session's, or sitting idle after a busy spell no
    // Stop closed (the user pressed Escape, or refused a permission).
    let charges = match kind {
        Some(EventKind::Stopped | EventKind::Ended { .. }) => true,
        Some(EventKind::Idle) => busy.is_some(),
        _ => false,
    };
    let in_tree = task.as_ref().is_some_and(|t| t.parent_id.is_some());
    // Asked only when needed: it walks the process tree.
    let attributed =
        in_tree && (charges || matches!(kind, Some(EventKind::NeedsInput(_)))) && ours(&app);
    let mut threshold = Threshold::None;
    if let (true, Some(task)) = (charges && attributed, &task) {
        let turns = i64::from(matches!(kind, Some(EventKind::Stopped)));
        let charged = charge_busy(&app, task, task_dir, session_id, busy, turns, now)
            .and_then(|()| budget::enforce(&app, task));
        match charged {
            Ok(crossed) => threshold = crossed,
            Err(e) => eprintln!(
                "jeet task-hook {}: could not charge its budget: {e:#}",
                hook.name
            ),
        }
    }
    let asks = matches!(
        kind,
        Some(EventKind::Stopped | EventKind::Idle | EventKind::Ended { .. })
    );
    let hold = match &task {
        Some(task) if in_tree && asks => match run::hold(&app, task) {
            Ok(hold) => hold.map(|(_, state)| state),
            Err(e) => {
                eprintln!("jeet task-hook {}: {e:#}", hook.name);
                None
            }
        },
        _ => None,
    };
    // A turn's end closes the spell it charged: what carries on is new.
    let busy_since = match (charges, busy) {
        (false, Some(since)) => since,
        _ => now,
    };
    Ok(HookCtx {
        app,
        task,
        attributed,
        prev,
        busy_since,
        hold,
        threshold,
    })
}

/// Write the status an event that neither ends a turn nor sits idle
/// leaves, with the busy spell it carries on or starts.
fn write_event_status(task_dir: &Path, hook: &HookEvent, ctx: &HookCtx, now: i64) -> Result<()> {
    match status_for(hook, now) {
        Some(status) => write_status(task_dir, &with_busy(status, ctx)),
        None => Ok(()),
    }
}

/// `status` with its `busy_since`, for one running or waiting.
fn with_busy(status: SessionStatus, ctx: &HookCtx) -> SessionStatus {
    let busy = matches!(status.state, State::Running | State::Waiting);
    SessionStatus {
        busy_since: busy.then_some(ctx.busy_since),
        ..status
    }
}

/// Charge `task` `turns`, and the time session `session_id` has been busy
/// since `busy_since` (none: it is not busy) that is not charged yet. How far
/// a session is charged is kept beside its status, under a lock, so a
/// turn's end and a sweep never charge the same time twice.
fn charge_busy(
    app: &App,
    task: &TaskRecord,
    task_dir: &Path,
    session_id: &str,
    busy_since: Option<i64>,
    turns: i64,
    now: i64,
) -> Result<()> {
    let dir = status_dir(task_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!(".{session_id}.charged"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("read {}", path.display()))?;
    let charged = text.trim().parse::<i64>().unwrap_or(0);
    let secs = busy_since.map_or(0, |since| (now - since.max(charged)).max(0));
    if secs == 0 && turns == 0 {
        return Ok(());
    }
    app.db.charge(&task.id, secs, turns)?;
    if secs > 0 {
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(now.to_string().as_bytes()))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Charge `task`, in the tree, the time its agent's busy spell has run so
/// far, and check its budget: for a sweep, so a long turn is held to its
/// budget before it ends, and for a pause or a stop, which end a spell with
/// no hook to say so. Returns the threshold newly crossed. Nothing for an
/// agent that is not `alive`: a status left running by one that died with
/// no `Ended` hook (a crash, a reboot, a killed server) is no busy time,
/// nor is it once a new agent is alive but has not reported yet: a status
/// older than the latest launch is that dead agent's.
pub fn fold_busy(app: &App, task: &TaskRecord, alive: bool, now: i64) -> Result<Threshold> {
    if !alive {
        return Ok(Threshold::None);
    }
    let task_dir = tasks::task_dir(app, task);
    let Some(status) = current_status(&task_dir) else {
        return Ok(Threshold::None);
    };
    if launched_at(&task_dir).is_some_and(|l| status.at < l) {
        return Ok(Threshold::None);
    }
    let busy = matches!(status.state, State::Running | State::Waiting);
    let Some(since) = status.busy_since.filter(|_| busy) else {
        return Ok(Threshold::None);
    };
    if !valid_session_id(&status.session_id) {
        return Ok(Threshold::None);
    }
    charge_busy(
        app,
        task,
        &task_dir,
        &status.session_id,
        Some(since),
        0,
        now,
    )?;
    budget::enforce(app, task)
}

/// Where a detached drain's output goes, in jeet's home.
const DRAIN_LOG: &str = "drain.log";

/// Start what waits on `max_live` now that task `task_id`'s session is
/// ending: a detached `jeet manager drain --after <task_id>`, as
/// [`crate::startup::launch`] starts its job, since a hook has seconds to
/// run and the pane it runs in may be going; the drain waits for it to go.
/// Nothing when nothing waits.
fn spawn_drain(app: &App, task_id: &str) -> Result<()> {
    // The test binary is no jeet.
    if cfg!(test) || app.db.queued_starts()?.is_empty() {
        return Ok(());
    }
    let log = app.home.join(DRAIN_LOG);
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let mut cmd = Command::new("nohup");
    cmd.arg(exe)
        .args(["manager", "drain", "--after", task_id])
        .env("JEET_HOME", &app.home)
        .env_remove(crate::permissions::ASSISTANT_ENV)
        .env_remove(tasks::TASK_ID_ENV)
        .current_dir(&app.home)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("start jeet manager drain")?;
    Ok(())
}

fn sweep_if_assistant(app: &App, task_id: &str, now: i64) -> Result<()> {
    match app.db.get_task(task_id)? {
        Some(task) if task.kind == TaskKind::Assistant => reports::sweep_idle(app, now),
        _ => Ok(()),
    }
}

/// Whether a hook's task, `located`, is the one this agent was launched
/// for, as its `--task` says, and the one it runs under
/// ([`identity::identify`]): a hook without `--task`, from before it, or
/// one naming another task than its own, never reports up.
fn attributed(task: Option<&str>, located: &str, app: &App) -> bool {
    names_task(task, located)
        && identity::identify(app).is_ok_and(|me| me.task.is_some_and(|t| t.id == located))
}

/// Whether a hook's `--task` names `located`.
fn names_task(task: Option<&str>, located: &str) -> bool {
    task.map(str::trim) == Some(located)
}

/// What to tell a task's parent about `kind`, given the session's status
/// before it: waiting on a prompt, once until it is answered, or an end
/// that no new session replaces in the same window.
fn auto_body(
    kind: &EventKind,
    prev: Option<&SessionStatus>,
) -> Option<(StatusEvent, &'static str)> {
    match kind {
        EventKind::NeedsInput(_) if prev.is_none_or(|p| p.state != State::Waiting) => Some((
            StatusEvent::Waiting,
            "waiting on a prompt only the user can answer",
        )),
        EventKind::Ended { cleared: false } => {
            Some((StatusEvent::Ended, "ended without reporting done"))
        }
        _ => None,
    }
}

/// Tell the task's parent what this hook means for it, if anything.
/// Nothing for a task jeet stopped itself (archived or deleted, or stopped
/// with `jeet manager stop`, whose caller says so) and, as ever, nothing on
/// stdout.
fn report_step(
    hook: &HookEvent,
    ctx: &HookCtx,
    launched_for: Option<&str>,
    now: i64,
) -> Result<()> {
    // A hook from before `--task` is never attributed: no need to look.
    if launched_for.is_none() {
        return Ok(());
    }
    let prev = ctx.prev.as_ref();
    let Some((event, body)) = hook.kind.as_ref().and_then(|k| auto_body(k, prev)) else {
        return Ok(());
    };
    let Some(task) = ctx.task.as_ref().filter(|_| ctx.attributed) else {
        return Ok(());
    };
    if ctx.hold == Some(RunState::Stopped) {
        return Ok(());
    }
    let since = match event {
        StatusEvent::Ended => prev.map_or(now, |p| p.at),
        StatusEvent::Waiting | StatusEvent::Idle | StatusEvent::Budget | StatusEvent::Queued => now,
    };
    reports::auto_status(&ctx.app, task, event, body, since, 0)?;
    Ok(())
}

/// Whether the hook's task is held: as [`HookCtx::hold`] read it, or as its
/// row says now. Asked again under the task's inbox lock, before handing
/// anything over: a pause written since `ctx` was made interrupts the task
/// under that same lock, so it either holds this event or comes after it.
fn held_now(ctx: &HookCtx) -> bool {
    if ctx.hold.is_some() {
        return true;
    }
    match &ctx.task {
        Some(task) if task.parent_id.is_some() => match run::hold(&ctx.app, task) {
            Ok(hold) => hold.is_some(),
            Err(e) => {
                eprintln!("jeet task-hook: {e:#}");
                false
            }
        },
        _ => false,
    }
}

/// Stop: hand over what `jeet task send` queued, else go idle. The inbox
/// stays locked until the status is written, so a message is either taken
/// here or sent after the session is idle, and typed in. Returns the
/// harness's [`Harness::continue_output`] to print, if it carries on. A
/// held task goes idle with its queue left where it is (M16), and so does
/// one whose turn this charge used its budget up: its pause holds it.
fn stop(
    harness: &dyn Harness,
    task_dir: &Path,
    hook: &HookEvent,
    session_id: &str,
    ctx: &HookCtx,
    now: i64,
) -> Result<Option<String>> {
    let event = hook.name.as_str();
    // A broken inbox must not keep the status from being written.
    let inbox = messages::Inbox::lock(task_dir)
        .inspect_err(|e| eprintln!("jeet task-hook {event}: {e:#}"))
        .ok();
    if held_now(ctx) {
        if let Some(status) = status_for(hook, now) {
            write_status(task_dir, &status)?;
        }
        return Ok(None);
    }
    // Only what is for this session: the assistant's sessions share an
    // inbox, and what was sent to one of them is that one's to deal with.
    // What was sent to one that has ended is let go by the next send or
    // session start, not here: a hook that could not reach tmux would take
    // the messages of sessions busy elsewhere.
    let queued = match &inbox {
        Some(inbox) => inbox.take_for(Some(session_id)).unwrap_or_else(|e| {
            eprintln!("jeet task-hook {event}: {e:#}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    if !queued.is_empty() {
        let output = harness.continue_output(&messages::queued_reason(&queued));
        // Not stopping after all: on to the messages.
        write_status(
            task_dir,
            &with_busy(
                SessionStatus {
                    session_id: session_id.to_string(),
                    state: State::Running,
                    detail: Some("reading queued messages".to_string()),
                    event: event.to_string(),
                    at: now,
                    busy_since: None,
                },
                ctx,
            ),
        )?;
        return Ok(output);
    }
    if let Some(status) = status_for(hook, now) {
        write_status(task_dir, &status)?;
    }
    Ok(None)
}

/// The agent has sat idle at its prompt. A turn the user interrupted, or a
/// permission they refused, ends with no Stop, so anything queued while it
/// was busy is typed in now, or it would wait for good; unless it is held
/// (M16), when it waits for the resume.
fn idle_prompt(task_dir: &Path, hook: &HookEvent, ctx: &HookCtx, now: i64) -> Result<()> {
    let event = hook.name.as_str();
    let inbox = messages::Inbox::lock(task_dir)
        .inspect_err(|e| eprintln!("jeet task-hook {event}: {e:#}"))
        .ok();
    // Idle before anything is typed in, so the prompt it submits is what
    // marks it running again. Idle since the turn ended, not since it was
    // last said so, so how long it has been idle holds still.
    if let Some(mut status) = status_for(hook, now) {
        let before = read_status(task_dir, &status.session_id);
        if let Some(before) = before.filter(|b| b.state == State::Idle) {
            status.at = before.at;
        }
        write_status(task_dir, &status)?;
    }
    let Some(inbox) = inbox else {
        return Ok(());
    };
    let Some(session_id) = hook.session_id.as_deref() else {
        return Ok(());
    };
    if held_now(ctx) || inbox.read_for(Some(session_id))?.is_empty() {
        return Ok(());
    }
    let Some(task) = &ctx.task else {
        return Ok(());
    };
    let app = &ctx.app;
    match task.kind {
        // Typed into the window of the session that sat idle.
        TaskKind::Assistant => messages::hand_over_queue(
            &inbox,
            &assistant::SessionAgent::for_session(app, task, session_id),
        ),
        TaskKind::Work | TaskKind::Review | TaskKind::Manager => {
            messages::hand_over_queue(&inbox, &messages::TmuxAgent::new(app, task))
        }
    }
    .map(|_| ())
}

/// The task the hook ran for: the one its `--task` names, wherever the
/// session has `cd`ed. A hook from before `--task` finds it by walking up
/// from the session's directory, which needs no index, else by
/// `$JEET_TASK_ID`. The assistant goes by `$JEET_TASK_ID` first: it may
/// well `cd` into a task's directory, and its sessions must not report as
/// that task's. Nor does another task's agent that `cd`s into the
/// assistant's home go as the assistant: its inbox holds the reports of
/// every task the assistant made.
fn locate_task(
    task: Option<&str>,
    hook: &HookEvent,
    app: &mut Option<App>,
) -> Result<Option<(PathBuf, String)>> {
    if let Some(id) = task {
        let opened = App::open()?;
        let found = opened
            .db
            .get_task(id)?
            .map(|task| (tasks::task_dir(&opened, &task), task.id));
        *app = Some(opened);
        return Ok(found);
    }
    let env_id = std::env::var(tasks::TASK_ID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let cwd = match hook.cwd.as_deref().filter(|c| !c.is_empty()) {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir().context("get cwd")?,
    };
    let by_env_first = env_id.is_some() && crate::permissions::assistant_mode();
    if !by_env_first {
        if let Some(found) = tasks::find_task_root(&cwd) {
            if env_id.as_deref().is_none_or(|id| id == found.1) {
                return Ok(Some(found));
            }
            let opened = App::open()?;
            let assistant = opened
                .db
                .get_task(&found.1)?
                .is_some_and(|task| task.kind == TaskKind::Assistant);
            *app = Some(opened);
            if !assistant {
                return Ok(Some(found));
            }
        }
    }
    let Some(id) = env_id else {
        return Ok(None);
    };
    let opened = match app.take() {
        Some(opened) => opened,
        None => App::open()?,
    };
    let found = opened
        .db
        .get_task(&id)?
        .map(|task| (tasks::task_dir(&opened, &task), task.id));
    *app = Some(opened);
    Ok(found)
}

/// Note a session the task's agent started. One the task has not seen,
/// started by clearing or forking the conversation rather than from scratch,
/// is recorded as following on from the one before it: `before`, the
/// session the assistant's window ran until now; for a task's agent, the
/// task's most recently active session.
fn record_session(
    app: &App,
    task: &TaskRecord,
    session_id: &str,
    source: StartSource,
    before: Option<&str>,
    now: i64,
) -> Result<()> {
    let task_id = task.id.as_str();
    let known = app.db.list_task_sessions(task_id)?;
    let guess = task.kind != TaskKind::Assistant;
    let parent = parent_of(&known, session_id, source, before, guess);
    let seen = known.iter().find(|s| s.session_id == session_id);
    // A session keeps its name, and one cleared or forked from another
    // takes that one's: the assistant's sessions are named, not after it.
    let name = seen
        .or(parent)
        .map_or_else(|| task.name.clone(), |s| s.name.clone());
    app.db.upsert_task_session(&TaskSessionRecord {
        session_id: session_id.to_string(),
        task_id: task_id.to_string(),
        parent_id: parent.map(|s| s.session_id.clone()),
        name,
        created_at: now,
        last_activity: now,
    })?;
    app.db.touch_task(task_id, now)
}

/// The session `session_id`, started by `source`, follows on from, among
/// `known`, most recently active first: none for one seen before or started
/// from scratch. `before` is the session its window ran before. Without one,
/// `guess` takes the most recent session, which is right for a task's agent,
/// with its one session at a time, and not for the assistant, with several.
fn parent_of<'a>(
    known: &'a [TaskSessionRecord],
    session_id: &str,
    source: StartSource,
    before: Option<&str>,
    guess: bool,
) -> Option<&'a TaskSessionRecord> {
    if known.iter().any(|s| s.session_id == session_id) {
        return None;
    }
    if let Some(parent) = before.and_then(|b| known.iter().find(|s| s.session_id == b)) {
        return Some(parent);
    }
    match source {
        StartSource::Startup => None,
        _ if guess => known.first(),
        _ => None,
    }
}

/// Replace a session's status file whole, so a reader never sees half of
/// one.
fn write_status(task_dir: &Path, status: &SessionStatus) -> Result<()> {
    let dir = status_dir(task_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.json", status.session_id));
    let tmp = dir.join(format!(".{}.{}.tmp", status.session_id, std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(status)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}

pub fn read_status(task_dir: &Path, session_id: &str) -> Option<SessionStatus> {
    let text = std::fs::read(status_dir(task_dir).join(format!("{session_id}.json"))).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Every session status in a task, most recent first. Files that cannot be
/// read are skipped: a hook may be halfway through replacing one.
pub fn read_statuses(task_dir: &Path) -> Vec<SessionStatus> {
    let Ok(entries) = std::fs::read_dir(status_dir(task_dir)) else {
        return Vec::new();
    };
    let mut statuses: Vec<SessionStatus> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name.ends_with(".json")
        })
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    statuses.sort_by_key(|s| {
        (
            std::cmp::Reverse(s.at),
            s.state == State::Ended,
            s.session_id.clone(),
        )
    });
    statuses
}

/// The status that speaks for the task: the most recent one. Clearing a
/// conversation ends one session and starts another in the same second, so
/// on a tie the session still going wins.
pub fn current_status(task_dir: &Path) -> Option<SessionStatus> {
    read_statuses(task_dir).into_iter().next()
}

/// When any session in the task last reported anything.
pub fn latest_status_mtime(task_dir: &Path) -> Option<i64> {
    std::fs::read_dir(status_dir(task_dir))
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|e| crate::agent::mtime_secs(&e.metadata().ok()?))
        .max()
}

/// What the tasks list shows for a task's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// No agent has run, or none that reports its status.
    NotStarted,
    /// The agent's tmux session is gone, though it never said it finished.
    NotRunning,
    /// Something is running that reports no status: an agent whose harness
    /// has no hooks.
    Live,
    /// An agent with hooks, launched moments ago, that none has reported
    /// on yet.
    Starting,
    Idle {
        since: i64,
    },
    Running {
        detail: Option<String>,
    },
    /// Needs the user: a permission prompt or a question.
    Waiting {
        detail: Option<String>,
    },
    Ended,
}

/// Combine the latest status with whether the task's tmux pane is alive.
pub fn activity(status: Option<&SessionStatus>, alive: bool) -> Activity {
    activity_since(status, alive, None, 0)
}

/// [`activity`] for an agent `launched` at that time, when its harness has
/// hooks. Alive with no status, it is starting for [`STARTING_SECS`], then
/// most likely held up at a startup prompt: one with hooks says something
/// as soon as it is going.
pub fn activity_since(
    status: Option<&SessionStatus>,
    alive: bool,
    launched: Option<i64>,
    now: i64,
) -> Activity {
    let Some(status) = status else {
        return match (alive, launched) {
            (false, _) => Activity::NotStarted,
            (true, None) => Activity::Live,
            (true, Some(at)) if now - at < STARTING_SECS => Activity::Starting,
            (true, Some(_)) => Activity::Waiting {
                detail: Some("startup prompt?".to_string()),
            },
        };
    };
    match (status.state, alive) {
        (State::Ended, _) => Activity::Ended,
        (_, false) => Activity::NotRunning,
        (State::Idle, true) => Activity::Idle { since: status.at },
        (State::Running, true) => Activity::Running {
            detail: status.detail.clone(),
        },
        (State::Waiting, true) => Activity::Waiting {
            detail: status.detail.clone(),
        },
    }
}

impl Activity {
    pub fn label(&self) -> String {
        let with = |what: &str, detail: &Option<String>| match detail {
            Some(detail) => format!("{what} – {detail}"),
            None => what.to_string(),
        };
        match self {
            Activity::NotStarted => "no agent".to_string(),
            Activity::NotRunning => "not running".to_string(),
            Activity::Live => "running".to_string(),
            Activity::Starting => "starting".to_string(),
            Activity::Idle { since } => {
                match crate::agent::age_label(*since).strip_suffix(" ago") {
                    Some(age) => format!("idle {age}"),
                    None => "idle".to_string(),
                }
            }
            Activity::Running { detail } => with("running", detail),
            Activity::Waiting { detail } => with("needs input", detail),
            Activity::Ended => "ended".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::TaskState;
    use crate::db::Database;

    /// The app of the index in `home`.
    fn app_at(home: &Path) -> App {
        App {
            config: config::load_or_create(home).unwrap(),
            db: Database::open(&config::index_path(home)).unwrap(),
            home: home.to_path_buf(),
        }
    }

    /// An index in a tempdir with the assistant and worker `w` under it.
    fn lab() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(&dir.path().canonicalize().unwrap());
        let root = crate::manager::root(&app).unwrap();
        app.db
            .insert_task(&TaskRecord {
                id: "w".into(),
                slug: "w".into(),
                name: "w".into(),
                project: None,
                kind: TaskKind::Work,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: Some(root.id),
                placement: crate::placement::for_new(),
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
        (dir, app)
    }

    /// A context that knows of no task and charges nothing.
    fn quiet_ctx(app: App) -> HookCtx {
        HookCtx {
            app,
            task: None,
            attributed: false,
            prev: None,
            busy_since: 0,
            hold: None,
            threshold: Threshold::None,
        }
    }

    fn hook(name: &str, kind: EventKind) -> HookEvent {
        HookEvent {
            name: name.into(),
            session_id: Some("s".into()),
            cwd: None,
            kind: Some(kind),
        }
    }

    /// `w`'s directory, with session `s` busy since 100.
    fn busy_w(app: &App) -> PathBuf {
        let dir = tasks::task_dir(app, &app.db.get_task("w").unwrap().unwrap());
        let running = SessionStatus {
            session_id: "s".into(),
            state: State::Running,
            detail: None,
            event: "UserPromptSubmit".into(),
            at: 100,
            busy_since: Some(100),
        };
        write_status(&dir, &running).unwrap();
        dir
    }

    fn used(app: &App, id: &str) -> (i64, i64) {
        let row = app.db.budget(id).unwrap().unwrap();
        (row.used_secs, row.used_turns)
    }

    #[test]
    fn an_unattributed_stop_charges_nothing() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", None, Some(10)).unwrap();
        let dir = busy_w(&app);
        let stop = hook("Stop", EventKind::Stopped);
        let open = || -> Result<App> { panic!("the app is open already") };
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &stop, 400, &|_| false).unwrap();
        assert!(!ctx.attributed);
        assert_eq!(used(&ctx.app, "w"), (0, 0));
        assert_eq!(ctx.threshold, Threshold::None);
    }

    #[test]
    fn a_hook_found_without_the_app_opens_it_and_charges() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", None, Some(10)).unwrap();
        let dir = busy_w(&app);
        let home = app.home.clone();
        // A sweep charged the spell so far: the turn's end charges the rest.
        let w = app.db.get_task("w").unwrap().unwrap();
        fold_busy(&app, &w, true, 250).unwrap();
        assert_eq!(used(&app, "w"), (150, 0));

        let stop = hook("Stop", EventKind::Stopped);
        let open = || Ok(app_at(&home));
        let ctx = hook_ctx_with(None, &open, &dir, "w", &stop, 400, &|_| true).unwrap();
        assert!(ctx.attributed);
        assert_eq!(used(&ctx.app, "w"), (300, 1));
        // What carries on after the turn's end is a new spell.
        assert_eq!(ctx.busy_since, 400);
        assert_eq!(ctx.hold, None);
    }

    #[test]
    fn a_stop_that_uses_the_budget_up_hands_nothing_over() {
        let harness = harness::by_id("claude-code").unwrap();
        let stop_hook = hook("Stop", EventKind::Stopped);
        let open = || -> Result<App> { panic!("the app is open already") };

        // Within its budget, what queued is handed over and it carries on.
        let (_home, app) = lab();
        app.db.set_budget_max("w", None, Some(2)).unwrap();
        let dir = busy_w(&app);
        messages::Inbox::lock(&dir).unwrap().push("more").unwrap();
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &stop_hook, 400, &|_| true).unwrap();
        assert_eq!(ctx.threshold, Threshold::None);
        assert!(stop(harness, &dir, &stop_hook, "s", &ctx, 400)
            .unwrap()
            .is_some());

        // Its last turn: paused by its budget, so it stops, with what
        // queued left for the resume.
        let (_home, app) = lab();
        app.db.set_budget_max("w", None, Some(1)).unwrap();
        let dir = busy_w(&app);
        messages::Inbox::lock(&dir).unwrap().push("more").unwrap();
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &stop_hook, 400, &|_| true).unwrap();
        assert_eq!(ctx.threshold, Threshold::Hit100);
        assert_eq!(ctx.hold, Some(RunState::Paused));
        let w = ctx.app.db.get_task("w").unwrap().unwrap();
        assert_eq!(w.run_state_by, Some(crate::db::tasks::Setter::Budget));
        assert_eq!(
            stop(harness, &dir, &stop_hook, "s", &ctx, 400).unwrap(),
            None
        );
        assert_eq!(read_status(&dir, "s").unwrap().state, State::Idle);
        let left = messages::Inbox::lock(&dir).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_turns_busy_spell_runs_from_its_prompt_through_tools_and_waits() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", Some(1000), Some(10)).unwrap();
        let dir = tasks::task_dir(&app, &app.db.get_task("w").unwrap().unwrap());
        let open = || -> Result<App> { panic!("the app is open already") };
        let mut app = Some(app);
        for (name, kind, at) in [
            ("UserPromptSubmit", EventKind::Working(None), 100),
            ("PreToolUse", EventKind::Working(Some("Bash".into())), 200),
            ("Notification", EventKind::NeedsInput(None), 250),
            ("PostToolUse", EventKind::Working(None), 300),
        ] {
            let event = hook(name, kind);
            let ctx = hook_ctx_with(app.take(), &open, &dir, "w", &event, at, &|_| true).unwrap();
            write_event_status(&dir, &event, &ctx, at).unwrap();
            let status = read_status(&dir, "s").unwrap();
            assert_eq!(status.busy_since, Some(100), "{name}");
            assert_eq!(status.at, at, "{name}");
            assert_eq!(used(&ctx.app, "w"), (0, 0), "{name}");
            app = Some(ctx.app);
        }
        // Its end charges the whole spell and the one turn.
        let stop = hook("Stop", EventKind::Stopped);
        let ctx = hook_ctx_with(app.take(), &open, &dir, "w", &stop, 400, &|_| true).unwrap();
        assert_eq!(used(&ctx.app, "w"), (300, 1));
    }

    #[test]
    fn a_sessions_end_charges_the_time_left_and_no_turn() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", Some(1000), Some(10)).unwrap();
        let dir = busy_w(&app);
        let ended = hook("SessionEnd", EventKind::Ended { cleared: false });
        let open = || -> Result<App> { panic!("the app is open already") };
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &ended, 400, &|_| true).unwrap();
        assert!(ctx.attributed);
        assert_eq!(used(&ctx.app, "w"), (300, 0));
    }

    #[test]
    fn a_pause_since_the_hook_began_is_asked_again_under_the_inbox_lock() {
        let harness = harness::by_id("claude-code").unwrap();
        let stop_hook = hook("Stop", EventKind::Stopped);
        let idle = hook("Notification", EventKind::Idle);
        let open = || -> Result<App> { panic!("the app is open already") };
        let (_home, app) = lab();
        let dir = busy_w(&app);
        messages::Inbox::lock(&dir).unwrap().push("more").unwrap();
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &stop_hook, 400, &|_| true).unwrap();
        assert_eq!(ctx.hold, None);
        // Paused after the hook read its hold, before it took the inbox.
        ctx.app
            .db
            .set_run_state("w", RunState::Paused, Some(&crate::db::tasks::Setter::User))
            .unwrap();
        assert_eq!(
            stop(harness, &dir, &stop_hook, "s", &ctx, 400).unwrap(),
            None
        );
        assert_eq!(read_status(&dir, "s").unwrap().state, State::Idle);
        // Handed over, it would be typed into a tmux unit tests never have.
        idle_prompt(&dir, &idle, &ctx, 500).unwrap();
        let left = messages::Inbox::lock(&dir).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_stopped_tasks_end_is_not_reported() {
        let (_home, app) = lab();
        let dir = busy_w(&app);
        let prev = read_status(&dir, "s");
        let ended = hook("SessionEnd", EventKind::Ended { cleared: false });
        let w = app.db.get_task("w").unwrap().unwrap();
        let mut ctx = HookCtx {
            task: Some(w),
            attributed: true,
            prev,
            hold: Some(RunState::Stopped),
            ..quiet_ctx(app)
        };
        // `jeet manager stop`, which ended it, said so to whoever asked.
        report_step(&ended, &ctx, Some("w"), 500).unwrap();
        assert!(ctx.app.db.last_status_report("w").unwrap().is_none());
        // Paused, or running, its end is news to its parent.
        ctx.hold = Some(RunState::Paused);
        report_step(&ended, &ctx, Some("w"), 500).unwrap();
        let report = ctx.app.db.last_status_report("w").unwrap().unwrap();
        assert!(report.body.contains("ended"), "{}", report.body);
    }

    #[test]
    fn sitting_idle_after_a_spell_no_stop_closed_charges_it() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", Some(1000), None).unwrap();
        let dir = busy_w(&app);
        let idle = hook("Notification", EventKind::Idle);
        let open = || -> Result<App> { panic!("the app is open already") };
        // The user pressed Escape: no Stop, then idle.
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &idle, 400, &|_| true).unwrap();
        assert!(ctx.attributed);
        assert_eq!(used(&ctx.app, "w"), (300, 0));
        idle_prompt(&dir, &idle, &ctx, 400).unwrap();
        // Idle again: nothing more to charge, and nobody is asked whose it is.
        let ctx = hook_ctx_with(Some(ctx.app), &open, &dir, "w", &idle, 500, &|_| {
            panic!("asked whose an idle with no spell is")
        })
        .unwrap();
        assert_eq!(used(&ctx.app, "w"), (300, 0));
    }

    #[test]
    fn a_held_agent_sitting_idle_is_handed_nothing() {
        let (_home, app) = lab();
        app.db
            .set_run_state("w", RunState::Paused, Some(&crate::db::tasks::Setter::User))
            .unwrap();
        let dir = busy_w(&app);
        messages::Inbox::lock(&dir).unwrap().push("more").unwrap();
        let idle = hook("Notification", EventKind::Idle);
        let open = || -> Result<App> { panic!("the app is open already") };
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &idle, 400, &|_| true).unwrap();
        assert_eq!(ctx.hold, Some(RunState::Paused));
        // Handed over, it would be typed into a tmux unit tests never have.
        idle_prompt(&dir, &idle, &ctx, 400).unwrap();
        assert_eq!(read_status(&dir, "s").unwrap().state, State::Idle);
        let left = messages::Inbox::lock(&dir).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_stopped_tasks_end_asks_what_holds_it() {
        let (_home, app) = lab();
        app.db
            .set_run_state(
                "w",
                RunState::Stopped,
                Some(&crate::db::tasks::Setter::User),
            )
            .unwrap();
        let dir = busy_w(&app);
        let ended = hook("SessionEnd", EventKind::Ended { cleared: false });
        let open = || -> Result<App> { panic!("the app is open already") };
        let ctx = hook_ctx_with(Some(app), &open, &dir, "w", &ended, 400, &|_| true).unwrap();
        assert_eq!(ctx.hold, Some(RunState::Stopped));
        report_step(&ended, &ctx, Some("w"), 400).unwrap();
        assert!(ctx.app.db.last_status_report("w").unwrap().is_none());
    }

    #[test]
    fn a_dead_agents_stale_status_is_not_charged() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", Some(10), None).unwrap();
        busy_w(&app);
        let w = app.db.get_task("w").unwrap().unwrap();
        assert_eq!(
            fold_busy(&app, &w, false, 100_000).unwrap(),
            Threshold::None
        );
        assert_eq!(used(&app, "w"), (0, 0));
        assert_eq!(
            app.db.get_task("w").unwrap().unwrap().run_state,
            RunState::Run
        );
    }

    #[test]
    fn a_dead_agents_stale_status_is_not_charged_once_a_new_one_is_alive() {
        let (_home, app) = lab();
        app.db.set_budget_max("w", Some(10), None).unwrap();
        // Left running at 100 by an agent that died; one launched since
        // is alive and has not reported yet.
        let dir = busy_w(&app);
        mark_launched(&dir, true).unwrap();
        let w = app.db.get_task("w").unwrap().unwrap();
        assert_eq!(
            fold_busy(&app, &w, true, now_secs()).unwrap(),
            Threshold::None
        );
        assert_eq!(used(&app, "w"), (0, 0));
        assert_eq!(
            app.db.get_task("w").unwrap().unwrap().run_state,
            RunState::Run
        );
    }

    #[test]
    fn input_no_hook_has_reported_on_is_a_turn_about_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let now = now_secs();
        // Nothing handed it: nothing to wait for.
        assert!(!input_unreported(dir, now));
        // Launched with its first prompt, no hook yet.
        mark_launched(dir, true).unwrap();
        assert!(input_unreported(dir, now));
        // Long since: whatever it is doing, it would have said.
        assert!(!input_unreported(dir, now + STARTING_SECS));
        let status = |state, event: &str, at| SessionStatus {
            session_id: "s".into(),
            state,
            detail: None,
            event: event.into(),
            at,
            busy_since: None,
        };
        // Its SessionStart, in the second it was launched, is from before
        // its prompt was submitted.
        write_status(dir, &status(State::Idle, "SessionStart", now)).unwrap();
        assert!(input_unreported(dir, now));
        // Its prompt submitted, seconds on.
        write_status(dir, &status(State::Running, "UserPromptSubmit", now + 3)).unwrap();
        assert!(!input_unreported(dir, now + 3));
        // Typed a message since its last hook: not reported on until the
        // next.
        std::fs::remove_file(launched_path(dir)).unwrap();
        write_status(dir, &status(State::Idle, "Stop", now - 1)).unwrap();
        assert!(!input_unreported(dir, now));
        mark_typed(dir).unwrap();
        assert!(input_unreported(dir, now));
        // Held at a startup prompt the watcher found: no turn is coming.
        mark_startup_prompt(dir, "s", "trust", now - 1).unwrap();
        assert!(!input_unreported(dir, now));
    }

    #[test]
    fn a_new_session_follows_on_from_the_one_its_window_ran() {
        let record = |id: &str| TaskSessionRecord {
            session_id: id.into(),
            task_id: "t".into(),
            parent_id: None,
            name: id.into(),
            created_at: 1,
            last_activity: 1,
        };
        // Most recently active first: "a" was busy a moment ago, "b" was
        // cleared after sitting idle.
        let known = [record("a"), record("b")];
        let parent = |id, source, before, guess| {
            parent_of(&known, id, source, before, guess).map(|s| s.session_id.as_str())
        };
        use StartSource::*;
        assert_eq!(parent("c", Clear, Some("b"), false), Some("b"));
        assert_eq!(parent("c", Clear, Some("b"), true), Some("b"));
        // No window to go by: a task's agent has only the one session, the
        // assistant may have several, so it does not guess.
        assert_eq!(parent("c", Clear, None, true), Some("a"));
        assert_eq!(parent("c", Other, None, true), Some("a"));
        assert_eq!(parent("c", Clear, None, false), None);
        assert_eq!(parent("c", Clear, Some("gone"), false), None);
        // Started from scratch, or seen before: nothing to follow on from.
        assert_eq!(parent("c", Startup, None, true), None);
        assert_eq!(parent("a", Resume, Some("b"), true), None);
    }

    #[test]
    fn first_lines_are_cut_to_length() {
        assert_eq!(first_line("\n  hello \nworld", 60).unwrap(), "hello");
        assert_eq!(first_line("abcdef", 4).unwrap(), "abc…");
        assert_eq!(first_line("ab  cdef", 4).unwrap(), "ab…");
        assert!(first_line(" \n ", 4).is_none());
    }

    #[test]
    fn session_ids_cannot_climb_out_of_the_status_directory() {
        assert!(valid_session_id("3f1c2a9e-5b7d-4e61-9a0b-2c4d6e8f0a1b"));
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn status_files_are_written_whole_and_read_back_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path();
        let at = |id: &str, state: State, at: i64| SessionStatus {
            session_id: id.into(),
            state,
            detail: None,
            event: "Stop".into(),
            at,
            busy_since: None,
        };
        write_status(task, &at("old", State::Idle, 10)).unwrap();
        write_status(task, &at("cleared", State::Ended, 20)).unwrap();
        write_status(task, &at("new", State::Idle, 20)).unwrap();
        // Rewritten in place.
        write_status(task, &at("old", State::Running, 15)).unwrap();
        // Junk and half-written files are skipped.
        std::fs::write(status_dir(task).join("junk.json"), "{").unwrap();
        std::fs::write(status_dir(task).join(".x.1.tmp"), "{}").unwrap();

        let ids: Vec<String> = read_statuses(task)
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, ["new", "cleared", "old"]);
        assert_eq!(current_status(task).unwrap().session_id, "new");
        assert_eq!(read_status(task, "old").unwrap().state, State::Running);
        assert!(latest_status_mtime(task).is_some());
        assert!(current_status(&task.join("nowhere")).is_none());
    }

    #[test]
    fn only_the_task_an_agent_was_launched_for_reports() {
        // A hook from before `--task` never reports up.
        assert!(!names_task(None, "t"));
        assert!(!names_task(Some("u"), "t"));
        assert!(names_task(Some(" t\n"), "t"));
    }

    #[test]
    fn a_parent_hears_of_a_new_wait_and_of_an_end_nothing_replaces() {
        let prev = |state: State| SessionStatus {
            session_id: "s".into(),
            state,
            detail: None,
            event: "x".into(),
            at: 1,
            busy_since: None,
        };
        let waiting = EventKind::NeedsInput(None);
        let word = |kind: &EventKind, before: Option<&SessionStatus>| {
            auto_body(kind, before).map(|(event, _)| event.word())
        };
        assert_eq!(word(&waiting, None), Some("waiting"));
        assert_eq!(word(&waiting, Some(&prev(State::Running))), Some("waiting"));
        assert_eq!(word(&waiting, Some(&prev(State::Waiting))), None);
        let ended = EventKind::Ended { cleared: false };
        assert_eq!(word(&ended, Some(&prev(State::Idle))), Some("ended"));
        assert_eq!(word(&EventKind::Ended { cleared: true }, None), None);
        assert_eq!(word(&EventKind::Stopped, None), None);
        assert_eq!(word(&EventKind::Idle, None), None);
        // Each body starts with its word, which debouncing goes by.
        for kind in [ended, waiting] {
            let (event, body) = auto_body(&kind, None).unwrap();
            assert!(body.starts_with(event.word()), "{body}");
        }
    }

    #[test]
    fn idle_is_since_the_turn_ended_however_often_it_is_said() {
        let dir = tempfile::tempdir().unwrap();
        let (_home, app) = lab();
        let ctx = quiet_ctx(app);
        let stop = hook("Stop", EventKind::Stopped);
        write_status(dir.path(), &status_for(&stop, 100).unwrap()).unwrap();
        let idle = hook("Notification", EventKind::Idle);
        idle_prompt(dir.path(), &idle, &ctx, 160).unwrap();
        let status = read_status(dir.path(), "s").unwrap();
        assert_eq!((status.state, status.at), (State::Idle, 100));
        // A turn since starts the clock again.
        let run = hook("UserPromptSubmit", EventKind::Working(None));
        write_status(dir.path(), &status_for(&run, 200).unwrap()).unwrap();
        idle_prompt(dir.path(), &idle, &ctx, 300).unwrap();
        assert_eq!(read_status(dir.path(), "s").unwrap().at, 300);
    }

    #[test]
    fn activity_trusts_the_pane_over_a_stale_status() {
        let s = |state: State, detail: Option<&str>| SessionStatus {
            session_id: "s".into(),
            state,
            detail: detail.map(Into::into),
            event: "x".into(),
            at: crate::worktrees::now_secs() - 120,
            busy_since: None,
        };
        let label = |status: Option<&SessionStatus>, alive| activity(status, alive).label();

        assert_eq!(label(None, false), "no agent");
        assert_eq!(label(None, true), "running");
        assert_eq!(
            label(Some(&s(State::Running, Some("Bash"))), false),
            "not running"
        );
        assert_eq!(label(Some(&s(State::Idle, None)), true), "idle 2m");
        assert_eq!(
            label(Some(&s(State::Running, Some("Bash"))), true),
            "running – Bash"
        );
        assert_eq!(label(Some(&s(State::Running, None)), true), "running");
        assert_eq!(
            label(Some(&s(State::Waiting, Some("may I?"))), true),
            "needs input – may I?"
        );
        assert_eq!(label(Some(&s(State::Ended, None)), false), "ended");
        let just_now = SessionStatus {
            at: crate::worktrees::now_secs(),
            ..s(State::Idle, None)
        };
        assert_eq!(label(Some(&just_now), true), "idle");
    }

    #[test]
    fn an_agent_with_hooks_but_no_status_is_starting_then_stuck() {
        let label = |alive, launched, now| activity_since(None, alive, launched, now).label();
        // No hooks: nothing better to say than that it runs.
        assert_eq!(label(true, None, 100), "running");
        assert_eq!(label(true, Some(100), 100), "starting");
        assert_eq!(label(true, Some(100), 100 + STARTING_SECS - 1), "starting");
        assert_eq!(
            label(true, Some(100), 100 + STARTING_SECS),
            "needs input – startup prompt?"
        );
        assert_eq!(label(false, Some(100), 500), "no agent");
        // What a hook said wins.
        let idle = SessionStatus {
            session_id: "s".into(),
            state: State::Idle,
            detail: None,
            event: "Stop".into(),
            at: 400,
            busy_since: None,
        };
        assert!(matches!(
            activity_since(Some(&idle), true, Some(100), 500),
            Activity::Idle { .. }
        ));
    }

    #[test]
    fn the_launch_marker_is_kept_only_for_agents_with_hooks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(launched_at(dir.path()).is_none());
        mark_launched(dir.path(), true).unwrap();
        assert!(launched_at(dir.path()).is_some());
        mark_launched(dir.path(), false).unwrap();
        assert!(launched_at(dir.path()).is_none());
        mark_launched(dir.path(), false).unwrap();
        mark_startup_prompt(dir.path(), "s", "trust folder", 7).unwrap();
        let status = current_status(dir.path()).unwrap();
        assert_eq!(
            activity(Some(&status), true).label(),
            "needs input – trust folder"
        );
    }
}
