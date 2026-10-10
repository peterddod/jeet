//! A task's agent at runtime: started in the task's tmux session with
//! jeet's hooks, stopped with the task, and what it is doing now.
//!
//! Claude Code gets a session id chosen by jeet, so the session is recorded
//! before the agent even runs, and a `--settings` JSON that installs the
//! status hooks and lets it run `jeet worktree` and `jeet ls` without
//! asking. Nothing is written into the task or any repository for this.
//! Any other agent just runs in the session, with no status and no sessions.

use std::path::Path;

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::agent::{self, AgentSpec};
use crate::cd;
use crate::context::App;
use crate::db::tasks::{TaskRecord, TaskSessionRecord};
use crate::status::{self, Activity, HOOK_EVENTS};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

/// What the agent may run without a permission prompt: the commands the
/// built-in jeet skill tells it to use.
const ALLOWED: [&str; 2] = ["Bash(jeet worktree:*)", "Bash(jeet ls:*)"];

/// Seconds Claude gives a hook before giving up on it.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The task's tmux session.
pub fn session_name(task: &TaskRecord) -> String {
    format!("jeet-{}", task.slug)
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
/// a new one.
pub fn ensure_running(app: &App, task: &TaskRecord, session: Option<&str>) -> Result<Running> {
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

    let spec = AgentSpec::from_config(&app.config)?;
    let dir = tasks::task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let argv = launch_argv(app, task, &spec, session)?;
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
    let _ = app.db.touch_task(&task.id, now_secs());
    Ok(Running {
        tmux_session: name,
        started: true,
        warnings,
    })
}

/// The agent's command line. For Claude Code it resumes or starts a
/// recorded session and carries jeet's settings.
fn launch_argv(
    app: &App,
    task: &TaskRecord,
    spec: &AgentSpec,
    session: Option<&str>,
) -> Result<Vec<String>> {
    let mut argv = spec.argv.clone();
    if !spec.supports_sessions() {
        if let Some(id) = session {
            bail!(
                "{} keeps no sessions jeet knows of, so session {id} cannot be resumed",
                spec.display()
            );
        }
        return Ok(argv);
    }

    let dir = tasks::task_dir(app, task);
    let (id, resume) = pick_session(app, task, &dir, session)?;
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
        true => spec.resume_args(&id),
        false => spec.new_session_args(&id, &task.name),
    };
    argv.extend(args.unwrap_or_default());
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    argv.extend(spec.settings_args(&settings_json(&exe)).unwrap_or_default());
    Ok(argv)
}

/// Which session to run, and whether it is resumed. A recorded session that
/// never got a transcript (the agent died before writing one) cannot be
/// resumed, so it is started afresh under the same id.
fn pick_session(
    app: &App,
    task: &TaskRecord,
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
            if !known.iter().any(|s| s.session_id == id) && !agent::claude_has_transcript(dir, id) {
                bail!(
                    "task {} has no session {id}; `jeet task ls` shows its tasks, and Claude's \
                     /resume lists the sessions in a task",
                    task.name
                );
            }
            id.to_string()
        }
        None => match latest_session(dir, &known) {
            Some(id) => id,
            None => return Ok((Uuid::new_v4().to_string(), false)),
        },
    };
    let resume = agent::claude_has_transcript(dir, &id);
    Ok((id, resume))
}

/// The task's most recently active session, by what the index recorded and
/// what its hooks reported since.
fn latest_session(dir: &Path, known: &[TaskSessionRecord]) -> Option<String> {
    known
        .iter()
        .max_by_key(|s| {
            let reported = status::read_status(dir, &s.session_id).map_or(0, |st| st.at);
            (s.last_activity.max(reported), s.created_at)
        })
        .map(|s| s.session_id.clone())
}

/// The `--settings` JSON: a hook per event running `jeet task-hook`, and
/// permission to run the jeet commands the skill describes.
pub fn settings_json(exe: &Path) -> String {
    let program = shell_quote(&exe.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        let mut group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!("{program} task-hook {event}"),
                "timeout": HOOK_TIMEOUT_SECS,
            }]
        });
        if event == "PreToolUse" {
            group["matcher"] = serde_json::json!("*");
        }
        hooks.insert(event.to_string(), serde_json::json!([group]));
    }
    serde_json::json!({
        "hooks": hooks,
        "permissions": { "allow": ALLOWED },
    })
    .to_string()
}

/// A word the shell reads back exactly, whatever it holds.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// End the task's agent, if it is running, returning once it and everything
/// it started have exited. Without tmux there is none.
pub fn stop(task: &TaskRecord) -> Result<()> {
    if !tmux::available() {
        return Ok(());
    }
    tmux::kill(&session_name(task))
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
    let dir = tasks::task_dir(app, task);
    let alive = tmux_ok && tmux::pane_alive(&session_name(task));
    status::activity(status::current_status(&dir).as_ref(), alive)
}

/// When anything last happened in the task: jeet touching it, a hook
/// reporting, or a session writing its transcript.
pub fn last_activity(app: &App, task: &TaskRecord) -> i64 {
    let dir = tasks::task_dir(app, task);
    [
        Some(task.last_activity),
        status::latest_status_mtime(&dir),
        agent::claude_latest_activity(&dir),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_install_every_hook_and_the_jeet_permissions() {
        let json = settings_json(Path::new("/opt/it's here/jeet"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for event in HOOK_EVENTS {
            let groups = value["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event}");
            let hook = &groups[0]["hooks"][0];
            assert_eq!(hook["type"], "command");
            assert_eq!(
                hook["command"],
                format!(r"'/opt/it'\''s here/jeet' task-hook {event}")
            );
        }
        assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert!(value["hooks"]["Stop"][0].get("matcher").is_none());
        assert_eq!(
            value["permissions"]["allow"],
            serde_json::json!(["Bash(jeet worktree:*)", "Bash(jeet ls:*)"])
        );
    }

    #[test]
    fn quoting_survives_the_shell() {
        let word = r#"a 'b' "c" $(d) `e`"#;
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(word)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), word);
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
