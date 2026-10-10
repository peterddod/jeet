//! What a task's agent is doing, as Claude Code's hooks report it.
//!
//! jeet launches Claude with a `--settings` JSON whose hooks run the hidden
//! `jeet task-hook <Event>`. Each run reads the hook's JSON on stdin and
//! rewrites `<task>/.jeet/status/<session>.json` with the state the event
//! leaves that session in. The tasks list reads those files back, together
//! with whether the task's tmux pane is still alive, into an [`Activity`].
//!
//! A hook runs on every tool call, so it has to be quick (well under 50ms),
//! must never print to stdout — Claude would read it as hook output — and
//! always exits 0: a broken status line must never get in the agent's way.
//!
//! The one exception: a Stop hook that finds messages queued for the agent
//! by `jeet task send` prints Claude's `block` decision with them, so the
//! agent carries on with them instead of stopping.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::context::App;
use crate::db::tasks::TaskSessionRecord;
use crate::messages;
use crate::tasks;
use crate::worktrees::now_secs;

/// Every hook event jeet installs.
pub const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "SubagentStop",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// More than any hook payload jeet cares about; a pasted novel of a prompt
/// is cut off rather than held in memory.
const MAX_INPUT: u64 = 4 * 1024 * 1024;

/// How much of a prompt or message is kept as the status detail.
const DETAIL_MAX: usize = 60;

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

/// The fields of a Claude Code hook payload that jeet reads; every event
/// sends a different subset.
#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// The status `event` leaves a session in, or `None` for an event that
/// says nothing about it.
pub fn status_for(event: &str, input: &HookInput, now: i64) -> Option<SessionStatus> {
    let session_id = input.session_id.clone()?;
    let (state, detail) = match event {
        "SessionStart" => (State::Idle, None),
        "UserPromptSubmit" => (
            State::Running,
            input
                .prompt
                .as_deref()
                .and_then(|p| first_line(p, DETAIL_MAX)),
        ),
        "PreToolUse" => (
            State::Running,
            input.tool_name.as_deref().map(|tool| match tool {
                "Workflow" => "running workflow".to_string(),
                "Agent" | "Task" => "waiting for subagents".to_string(),
                other => other.to_string(),
            }),
        ),
        "SubagentStop" => (State::Running, None),
        "Notification" if is_idle_prompt(input) => (State::Idle, None),
        "Notification" => (
            State::Waiting,
            input
                .message
                .as_deref()
                .and_then(|m| first_line(m, DETAIL_MAX)),
        ),
        "Stop" => (State::Idle, None),
        "SessionEnd" => (State::Ended, None),
        _ => return None,
    };
    Some(SessionStatus {
        session_id,
        state,
        detail,
        event: event.to_string(),
        at: now,
    })
}

/// Claude nags with a notification after a minute of waiting at an empty
/// prompt; that is idleness, not a question for the user.
fn is_idle_prompt(input: &HookInput) -> bool {
    input.notification_type.as_deref() == Some("idle_prompt")
        || input
            .message
            .as_deref()
            .is_some_and(|m| m.contains("waiting for your input"))
}

/// The first non-blank line of `text`, cut to `max` characters.
fn first_line(text: &str, max: usize) -> Option<String> {
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

/// `jeet task-hook <event>`: record what the hook on stdin says. Never
/// fails, never prints to stdout; what goes wrong is noted on stderr, which
/// Claude keeps out of the conversation.
pub fn run_hook(event: &str) {
    let mut raw = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut raw);
    }
    if let Err(e) = handle_hook(event, &raw, now_secs()) {
        eprintln!("jeet task-hook {event}: {e:#}");
    }
}

fn handle_hook(event: &str, raw: &str, now: i64) -> Result<()> {
    let input: HookInput = serde_json::from_str(raw).context("parse the hook input")?;
    let event = match event {
        "" => input.hook_event_name.as_deref().unwrap_or_default(),
        named => named,
    };
    let Some(session_id) = input.session_id.as_deref() else {
        return Ok(());
    };
    if !valid_session_id(session_id) {
        anyhow::bail!("{session_id:?} is not a session id");
    }
    let mut app = None;
    let Some((task_dir, task_id)) = locate_task(&input, &mut app)? else {
        // Not a jeet task: nothing to report to.
        return Ok(());
    };

    if event == "SessionStart" {
        let app = match app {
            Some(app) => app,
            None => App::open()?,
        };
        record_session(&app, &task_id, session_id, input.source.as_deref(), now)?;
        // A compaction can happen mid-prompt; the session carries on in
        // whatever state it was in.
        if input.source.as_deref() == Some("compact")
            && read_status(&task_dir, session_id).is_some()
        {
            return Ok(());
        }
    }
    if event == "Stop" {
        // A broken inbox must not keep the status from being written.
        let queued = messages::drain(&task_dir).unwrap_or_else(|e| {
            eprintln!("jeet task-hook {event}: {e:#}");
            Vec::new()
        });
        if !queued.is_empty() {
            println!("{}", messages::stop_block(&queued));
            // Not stopping after all: on to the messages.
            return write_status(
                &task_dir,
                &SessionStatus {
                    session_id: session_id.to_string(),
                    state: State::Running,
                    detail: Some("reading queued messages".to_string()),
                    event: event.to_string(),
                    at: now,
                },
            );
        }
    }
    match status_for(event, &input, now) {
        Some(status) => write_status(&task_dir, &status),
        None => Ok(()),
    }
}

/// The task the hook ran for: found by walking up from the session's
/// directory, which needs no index, else by `$JEET_TASK_ID`.
fn locate_task(input: &HookInput, app: &mut Option<App>) -> Result<Option<(PathBuf, String)>> {
    let cwd = match input.cwd.as_deref().filter(|c| !c.is_empty()) {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir().context("get cwd")?,
    };
    if let Some(found) = tasks::find_task_root(&cwd) {
        return Ok(Some(found));
    }
    let Some(id) = std::env::var(tasks::TASK_ID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let opened = App::open()?;
    let found = opened
        .db
        .get_task(&id)?
        .map(|task| (tasks::task_dir(&opened, &task), task.id));
    *app = Some(opened);
    Ok(found)
}

/// Note a session the task's agent started. One the task has not seen,
/// started by clearing or forking the conversation rather than from scratch,
/// is recorded as following on from the session active before it.
fn record_session(
    app: &App,
    task_id: &str,
    session_id: &str,
    source: Option<&str>,
    now: i64,
) -> Result<()> {
    let Some(task) = app.db.get_task(task_id)? else {
        return Ok(());
    };
    let known = app.db.list_task_sessions(task_id)?;
    let seen = known.iter().any(|s| s.session_id == session_id);
    let parent_id = match (seen, source) {
        (true, _) | (false, Some("startup") | None) => None,
        (false, Some(_)) => known.first().map(|s| s.session_id.clone()),
    };
    app.db.upsert_task_session(&TaskSessionRecord {
        session_id: session_id.to_string(),
        task_id: task_id.to_string(),
        parent_id,
        name: task.name.clone(),
        created_at: now,
        last_activity: now,
    })?;
    app.db.touch_task(task_id, now)
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
    /// Something is running that reports no status: an agent other than
    /// Claude Code.
    Live,
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
    let Some(status) = status else {
        return match alive {
            true => Activity::Live,
            false => Activity::NotStarted,
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

    fn fixture(event: &str) -> HookInput {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hooks")
            .join(format!("{event}.json"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn status(event: &str) -> SessionStatus {
        status_for(event, &fixture(event), 1000).unwrap()
    }

    #[test]
    fn each_event_leaves_the_session_in_its_state() {
        let cases: [(&str, State, Option<&str>); 7] = [
            ("SessionStart", State::Idle, None),
            (
                "UserPromptSubmit",
                State::Running,
                Some("Fix the login page so that the session cookie survives a pa…"),
            ),
            ("PreToolUse", State::Running, Some("Bash")),
            ("SubagentStop", State::Running, None),
            (
                "Notification",
                State::Waiting,
                Some("Claude needs your permission to use Bash"),
            ),
            ("Stop", State::Idle, None),
            ("SessionEnd", State::Ended, None),
        ];
        for (event, state, detail) in cases {
            let status = status(event);
            assert_eq!(status.state, state, "{event}");
            assert_eq!(status.detail.as_deref(), detail, "{event}");
            assert_eq!(status.event, event);
            assert_eq!(status.session_id, "3f1c2a9e-5b7d-4e61-9a0b-2c4d6e8f0a1b");
            assert_eq!(status.at, 1000);
        }
        // Every installed event is covered.
        assert_eq!(cases.map(|c| c.0), HOOK_EVENTS);
    }

    #[test]
    fn tools_that_wait_on_others_say_so() {
        let mut input = fixture("PreToolUse");
        for (tool, detail) in [
            ("Workflow", "running workflow"),
            ("Agent", "waiting for subagents"),
            ("Task", "waiting for subagents"),
        ] {
            input.tool_name = Some(tool.into());
            let status = status_for("PreToolUse", &input, 1).unwrap();
            assert_eq!(status.detail.as_deref(), Some(detail));
        }
    }

    #[test]
    fn an_idle_prompt_notification_is_idleness() {
        let status = status_for("Notification", &fixture("Notification-idle"), 1).unwrap();
        assert_eq!(status.state, State::Idle);
        assert_eq!(status.detail, None);
        let mut input = fixture("Notification-idle");
        input.notification_type = None;
        assert_eq!(
            status_for("Notification", &input, 1).unwrap().state,
            State::Idle
        );
    }

    #[test]
    fn unknown_events_and_anonymous_sessions_say_nothing() {
        assert!(status_for("PostToolUse", &fixture("PreToolUse"), 1).is_none());
        assert!(status_for("Stop", &HookInput::default(), 1).is_none());
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
}
