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
use crate::config::AssistantConfig;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord, TaskSessionRecord};
use crate::permissions;
use crate::projects;
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
    let argv = launch_argv(app, task, &spec, session, prompt)?;
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
/// recorded session, carries jeet's settings and, given one, its first
/// prompt.
fn launch_argv(
    app: &App,
    task: &TaskRecord,
    spec: &AgentSpec,
    session: Option<&str>,
    prompt: Option<&str>,
) -> Result<Vec<String>> {
    let mut argv = spec.argv.clone();
    if prompt.is_some() && spec.prompt_args("").is_none() {
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
    argv.extend(
        spec.settings_args(&settings_json(&exe, &ALLOWED))
            .unwrap_or_default(),
    );
    if let Some(prompt) = prompt {
        argv.extend(spec.prompt_args(prompt).unwrap_or_default());
    }
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
pub fn latest_session(dir: &Path, known: &[TaskSessionRecord]) -> Option<String> {
    known
        .iter()
        .max_by_key(|s| {
            let reported = status::read_status(dir, &s.session_id).map_or(0, |st| st.at);
            (s.last_activity.max(reported), s.created_at)
        })
        .map(|s| s.session_id.clone())
}

/// The `--settings` JSON: a hook per event running `jeet task-hook`, and
/// permission to run the jeet commands `allow` names (`Bash(…)` rules).
pub fn settings_json<S: AsRef<str>>(exe: &Path, allow: &[S]) -> String {
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
        "permissions": {
            "allow": allow.iter().map(|a| a.as_ref()).collect::<Vec<&str>>(),
        },
    })
    .to_string()
}

/// The `--settings` JSON for the assistant: jeet's hooks, and permission to
/// run the read-only jeet commands and those of each capability
/// `[assistant] allow` lists, so allowed actions do not prompt.
pub fn assistant_settings_json(exe: &Path, settings: &AssistantConfig) -> String {
    settings_json(exe, &permissions::bash_allow(&settings.allow))
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
    activity_at(&tasks::task_dir(app, task), &session_name(task), tmux_ok)
}

/// [`activity`] for the task in `dir` whose tmux session is `session`, for
/// a thread that has no index to look the task up in.
pub fn activity_at(dir: &Path, session: &str, tmux_ok: bool) -> Activity {
    let alive = tmux_ok && tmux::pane_alive(session);
    status::activity(status::current_status(dir).as_ref(), alive)
}

/// When anything last happened in the task: jeet touching it, a hook
/// reporting, or a session writing its transcript.
pub fn last_activity(app: &App, task: &TaskRecord) -> i64 {
    last_activity_at(&tasks::task_dir(app, task), task.last_activity)
}

/// [`last_activity`] for the task in `dir` that jeet last touched at
/// `touched`.
pub fn last_activity_at(dir: &Path, touched: i64) -> i64 {
    [
        Some(touched),
        status::latest_status_mtime(dir),
        agent::claude_latest_activity(dir),
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
        let json = settings_json(Path::new("/opt/it's here/jeet"), &ALLOWED);
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
    fn the_assistant_is_pre_approved_what_it_is_allowed() {
        let settings = AssistantConfig {
            allow: vec!["task.send".into()],
            notify: None,
        };
        let json = assistant_settings_json(Path::new("/bin/jeet"), &settings);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let allow: Vec<&str> = value["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(allow.contains(&"Bash(jeet task send:*)"), "{allow:?}");
        assert!(allow.contains(&"Bash(jeet task ls:*)"), "{allow:?}");
        assert!(!allow.contains(&"Bash(jeet task new:*)"), "{allow:?}");
        assert!(value["hooks"]["Stop"].is_array());
    }

    #[test]
    fn a_first_prompt_fits_in_one_tmux_command_with_the_settings() {
        // tmux refuses a command over 16KB; the prompt shares new-session's
        // with the settings, a long path to jeet, and every capability.
        let settings = AssistantConfig {
            allow: crate::permissions::CAPABILITIES
                .iter()
                .map(|c| c.name.to_string())
                .collect(),
            notify: None,
        };
        let exe = format!("/{}/jeet", "d".repeat(200));
        let json = assistant_settings_json(Path::new(&exe), &settings);
        let budget = 16 * 1024 - 2048; // the rest of the command
        assert!(
            json.len() + crate::messages::MAX_MESSAGE + 16 < budget,
            "{} bytes of settings",
            json.len()
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
