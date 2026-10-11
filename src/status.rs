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

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord, TaskSessionRecord};
use crate::harness::{self, EventKind, Harness, HookEvent, StartSource};
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
    // What the session was doing before this event, for what to tell a
    // parent about it.
    let prev = match hook.kind {
        Some(EventKind::NeedsInput(_) | EventKind::Ended { .. }) => {
            read_status(&task_dir, session_id)
        }
        _ => None,
    };

    if let Some(EventKind::SessionStarted(source)) = hook.kind {
        let app = match app.take() {
            Some(app) => app,
            None => App::open()?,
        };
        if let Some(task) = app.db.get_task(&task_id)? {
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
            record_session(&app, &task, session_id, source, before.as_deref(), now)?;
            // The harness adds this to the conversation; a summary that
            // cannot be made costs the session its context, not its start.
            if assistant && harness.session_context_output("").is_some() {
                match assistant::session_context(&app, &task, session_id, now) {
                    Ok(context) => {
                        if let Some(output) = harness.session_context_output(&context) {
                            println!("{output}");
                        }
                    }
                    Err(e) => eprintln!("jeet task-hook {event}: {e:#}"),
                }
            } else if task.parent_id.is_some()
                && attributed(launched_for, &task.id)
                && harness.session_context_output("").is_some()
            {
                // What its parent asked of it, each time a session starts,
                // /clear included.
                match reports::brief_context(&app, &task) {
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
        if source == StartSource::Compact && read_status(&task_dir, session_id).is_some() {
            return Ok(());
        }
    }
    // The assistant's turn ending is when tasks idle too long are reported
    // to it, whatever its harness: before its inbox is handed over, so what
    // this queues comes with it. Its app is open already (by `--task`, or
    // by `$JEET_TASK_ID` from an older hook).
    if let (Some(EventKind::Stopped), Some(app)) = (&hook.kind, &app) {
        if let Err(e) = sweep_if_assistant(app, &task_id, now) {
            eprintln!("jeet task-hook {event}: {e:#}");
        }
    }
    let result = match hook.kind {
        // A harness that cannot carry on past a turn's end is idle at it.
        Some(EventKind::Stopped) if harness.continue_output("").is_some() => {
            stop(harness, &task_dir, &hook, session_id, now)
        }
        Some(EventKind::Stopped | EventKind::Idle) => {
            idle_prompt(&task_dir, &task_id, &hook, app, now)
        }
        _ => match status_for(&hook, now) {
            Some(status) => write_status(&task_dir, &status),
            None => Ok(()),
        },
    };
    // Only now, with the task's inbox no longer locked.
    if let Err(e) = report_step(&hook, &task_id, launched_for, prev, now) {
        eprintln!("jeet task-hook {event}: {e:#}");
    }
    result
}

fn sweep_if_assistant(app: &App, task_id: &str, now: i64) -> Result<()> {
    match app.db.get_task(task_id)? {
        Some(task) if task.kind == TaskKind::Assistant => reports::sweep_idle(app, now),
        _ => Ok(()),
    }
}

/// Whether a hook's task, `located`, is the one this agent was launched
/// for, as its `--task` says: a hook without one, from before `--task`,
/// never reports up.
fn attributed(task: Option<&str>, located: &str) -> bool {
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
/// Nothing for a task jeet stopped itself (archived or deleted) and, as
/// ever, nothing on stdout.
fn report_step(
    hook: &HookEvent,
    task_id: &str,
    task: Option<&str>,
    prev: Option<SessionStatus>,
    now: i64,
) -> Result<()> {
    if !attributed(task, task_id) {
        return Ok(());
    }
    let Some((event, body)) = hook.kind.as_ref().and_then(|k| auto_body(k, prev.as_ref())) else {
        return Ok(());
    };
    let app = App::open()?;
    let Some(task) = app.db.get_task(task_id)? else {
        return Ok(());
    };
    let since = match event {
        StatusEvent::Ended => prev.map_or(now, |p| p.at),
        StatusEvent::Waiting | StatusEvent::Idle => now,
    };
    reports::auto_status(&app, &task, event, body, since, 0)?;
    Ok(())
}

/// Stop: hand over what `jeet task send` queued, else go idle. The inbox
/// stays locked until the status is written, so a message is either taken
/// here or sent after the session is idle, and typed in.
fn stop(
    harness: &dyn Harness,
    task_dir: &Path,
    hook: &HookEvent,
    session_id: &str,
    now: i64,
) -> Result<()> {
    let event = hook.name.as_str();
    // A broken inbox must not keep the status from being written.
    let inbox = messages::Inbox::lock(task_dir)
        .inspect_err(|e| eprintln!("jeet task-hook {event}: {e:#}"))
        .ok();
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
        if let Some(output) = harness.continue_output(&messages::queued_reason(&queued)) {
            println!("{output}");
        }
        // Not stopping after all: on to the messages.
        return write_status(
            task_dir,
            &SessionStatus {
                session_id: session_id.to_string(),
                state: State::Running,
                detail: Some("reading queued messages".to_string()),
                event: event.to_string(),
                at: now,
            },
        );
    }
    match status_for(hook, now) {
        Some(status) => write_status(task_dir, &status),
        None => Ok(()),
    }
}

/// The agent has sat idle at its prompt. A turn the user interrupted, or a
/// permission they refused, ends with no Stop, so anything queued while it
/// was busy is typed in now, or it would wait for good.
fn idle_prompt(
    task_dir: &Path,
    task_id: &str,
    hook: &HookEvent,
    app: Option<App>,
    now: i64,
) -> Result<()> {
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
    if inbox.read_for(Some(session_id))?.is_empty() {
        return Ok(());
    }
    let app = match app {
        Some(app) => app,
        None => App::open()?,
    };
    let Some(task) = app.db.get_task(task_id)? else {
        return Ok(());
    };
    match task.kind {
        // Typed into the window of the session that sat idle.
        TaskKind::Assistant => messages::hand_over_queue(
            &inbox,
            &assistant::SessionAgent::for_session(&app, &task, session_id),
        ),
        TaskKind::Work | TaskKind::Review => {
            messages::hand_over_queue(&inbox, &messages::TmuxAgent::new(&app, &task))
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
        assert!(!attributed(None, "t"));
        assert!(!attributed(Some("u"), "t"));
        assert!(attributed(Some(" t\n"), "t"));
    }

    #[test]
    fn a_parent_hears_of_a_new_wait_and_of_an_end_nothing_replaces() {
        let prev = |state: State| SessionStatus {
            session_id: "s".into(),
            state,
            detail: None,
            event: "x".into(),
            at: 1,
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
        let hook = |name: &str, kind: EventKind| HookEvent {
            name: name.into(),
            session_id: Some("s".into()),
            cwd: None,
            kind: Some(kind),
        };
        let stop = hook("Stop", EventKind::Stopped);
        write_status(dir.path(), &status_for(&stop, 100).unwrap()).unwrap();
        let idle = hook("Notification", EventKind::Idle);
        idle_prompt(dir.path(), "t", &idle, None, 160).unwrap();
        let status = read_status(dir.path(), "s").unwrap();
        assert_eq!((status.state, status.at), (State::Idle, 100));
        // A turn since starts the clock again.
        let run = hook("UserPromptSubmit", EventKind::Working(None));
        write_status(dir.path(), &status_for(&run, 200).unwrap()).unwrap();
        idle_prompt(dir.path(), "t", &idle, None, 300).unwrap();
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
