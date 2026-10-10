//! Coding-agent and editor integration.
//!
//! jeet shells out to whatever editor and coding agent you configure. For
//! Claude Code it can additionally enumerate the sessions already recorded for
//! a worktree so you can resume one instead of starting fresh.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};

use crate::config::{split_command, Config};

/// Which session store (if any) the configured agent uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Program plus any configured arguments.
    pub argv: Vec<String>,
    pub kind: AgentKind,
}

impl AgentSpec {
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::from_command(&config.agent_command())
    }

    pub fn from_command(command: &str) -> Result<Self> {
        let argv = split_command(command);
        if argv.is_empty() {
            bail!("no coding agent configured (set `agent` in config.toml or $JEET_AGENT)");
        }
        let program = Path::new(&argv[0])
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| argv[0].clone());
        let kind = if program == "claude" || program.starts_with("claude-") {
            AgentKind::ClaudeCode
        } else {
            AgentKind::Unknown(program)
        };
        Ok(Self { argv, kind })
    }

    pub fn display(&self) -> String {
        self.argv.join(" ")
    }

    /// Arguments that resume a recorded session, when the agent supports it.
    pub fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        match self.kind {
            AgentKind::ClaudeCode => Some(vec!["--resume".to_string(), session_id.to_string()]),
            AgentKind::Unknown(_) => None,
        }
    }

    /// Arguments that start a new session with a known id and display name,
    /// so jeet can record the session before the agent is even running.
    pub fn new_session_args(&self, session_id: &str, name: &str) -> Option<Vec<String>> {
        match self.kind {
            AgentKind::ClaudeCode => Some(vec![
                "--session-id".to_string(),
                session_id.to_string(),
                "-n".to_string(),
                name.to_string(),
            ]),
            AgentKind::Unknown(_) => None,
        }
    }

    /// Arguments that layer a settings JSON (hooks, permissions) over the
    /// user's own, without writing anything to disk.
    pub fn settings_args(&self, json: &str) -> Option<Vec<String>> {
        match self.kind {
            AgentKind::ClaudeCode => Some(vec!["--settings".to_string(), json.to_string()]),
            AgentKind::Unknown(_) => None,
        }
    }

    /// Arguments that give an interactive session its first prompt, after
    /// every other argument. `--` first, so a prompt that starts with a dash
    /// is a prompt, not a flag.
    pub fn prompt_args(&self, prompt: &str) -> Option<Vec<String>> {
        match self.kind {
            AgentKind::ClaudeCode => Some(vec!["--".to_string(), prompt.to_string()]),
            AgentKind::Unknown(_) => None,
        }
    }

    pub fn supports_sessions(&self) -> bool {
        self.kind == AgentKind::ClaudeCode
    }
}

/// A Claude Code session recorded for a directory, found by its transcript.
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub id: String,
    /// Unix seconds of the last write.
    pub modified: i64,
    pub summary: String,
}

/// `just now`, `5m ago`, `3h ago`… for a unix timestamp.
pub fn age_label(at: i64) -> String {
    let delta = crate::worktrees::now_secs().saturating_sub(at).max(0);
    match delta {
        d if d < 60 => "just now".to_string(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d => format!("{}d ago", d / 86_400),
    }
}

/// Editor argv, e.g. `["vim"]` or `["code", "--wait"]`.
pub fn editor_argv(config: &Config) -> Result<Vec<String>> {
    let argv = split_command(&config.editor_command());
    if argv.is_empty() {
        bail!("no editor configured (set `editor` in config.toml, $JEET_EDITOR or $EDITOR)");
    }
    Ok(argv)
}

/// Run a command in `cwd`, inheriting the terminal, returning its exit code.
pub fn run_in(argv: &[String], cwd: &Path, extra: &[String]) -> Result<i32> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;
    let status = Command::new(program)
        .args(args)
        .args(extra)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to launch `{program}`"))?;
    Ok(status.code().unwrap_or(1))
}

/// Root of Claude Code's per-project session store.
fn claude_projects_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("projects"));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Claude Code encodes a project path by replacing every non-alphanumeric
/// character with `-`, e.g. `/home/me/.jeet/store` -> `-home-me--jeet-store`.
pub fn claude_project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where Claude Code keeps the transcripts of sessions started in `root`.
fn claude_session_dir(root: &Path) -> Option<PathBuf> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    Some(claude_projects_dir()?.join(claude_project_slug(&canonical)))
}

/// Copy the transcripts of the Claude Code sessions started in `from` to
/// where Claude looks for those started in `to`, so they can be resumed
/// there. Returns the sessions copied; one already at `to` is left alone and
/// not counted. `to` must exist: Claude names the store by its real path.
pub fn copy_claude_sessions(from: &Path, to: &Path) -> Result<Vec<AgentSession>> {
    let sessions = claude_sessions(from)?;
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let (Some(source), Some(dest)) = (claude_session_dir(from), claude_session_dir(to)) else {
        bail!("could not locate the Claude Code config directory");
    };
    std::fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
    let mut copied = Vec::new();
    for session in sessions {
        let name = format!("{}.jsonl", session.id);
        let target = dest.join(&name);
        if target.exists() {
            continue;
        }
        std::fs::copy(source.join(&name), &target)
            .with_context(|| format!("copy session {}", session.id))?;
        copied.push(session);
    }
    Ok(copied)
}

/// Whether Claude Code has a transcript of session `id` started in `root`,
/// i.e. whether `--resume id` there would find anything.
pub fn claude_has_transcript(root: &Path, id: &str) -> bool {
    claude_session_dir(root).is_some_and(|dir| dir.join(format!("{id}.jsonl")).is_file())
}

/// When any Claude Code session in `root` last wrote to its transcript.
/// Only directory metadata is read, so it is cheap however long they are.
pub fn claude_latest_activity(root: &Path) -> Option<i64> {
    let dir = claude_session_dir(root)?;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| mtime_secs(&e.metadata().ok()?))
        .max()
}

/// A file's modification time in unix seconds.
pub fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn claude_sessions(root: &Path) -> Result<Vec<AgentSession>> {
    let Some(dir) = claude_session_dir(root) else {
        bail!("could not locate the Claude Code config directory");
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }
        let id = match path.file_stem() {
            Some(stem) => stem.to_string_lossy().to_string(),
            None => continue,
        };
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| mtime_secs(&m))
            .unwrap_or(0);
        sessions.push(AgentSession {
            id,
            modified,
            summary: summarise_session(&path),
        });
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
}

/// Best-effort first prompt (or recorded summary) plus the transcript length.
fn summarise_session(path: &Path) -> String {
    // Transcripts run to many megabytes, so read them a line at a time, and
    // only as far as the first prompt.
    let Ok(file) = std::fs::File::open(path) else {
        return "(unreadable transcript)".to_string();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| extract_summary(&serde_json::from_str(&line).ok()?))
        .unwrap_or_else(|| "(no prompt recorded)".to_string())
}

fn extract_summary(value: &serde_json::Value) -> Option<String> {
    let record_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if record_type == "summary" {
        if let Some(text) = value.get("summary").and_then(|s| s.as_str()) {
            return Some(one_line(text));
        }
    }
    if record_type == "user" {
        if let Some(content) = value.pointer("/message/content") {
            if let Some(text) = content.as_str() {
                return Some(one_line(text));
            }
            if let Some(items) = content.as_array() {
                for item in items {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        return Some(one_line(text));
                    }
                }
            }
        }
    }
    if let Some(text) = value.get("content").and_then(|c| c.as_str()) {
        return Some(one_line(text));
    }
    None
}

fn one_line(text: &str) -> String {
    let flat: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let flat = flat.trim();
    if flat.chars().count() > 120 {
        let truncated: String = flat.chars().take(117).collect();
        format!("{truncated}...")
    } else {
        flat.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_claude_layout() {
        assert_eq!(
            claude_project_slug(Path::new("/home/user/jeet")),
            "-home-user-jeet"
        );
        assert_eq!(
            claude_project_slug(Path::new("/home/me/.jeet/worktrees/feat-x")),
            "-home-me--jeet-worktrees-feat-x"
        );
    }

    #[test]
    fn detects_claude_agent() {
        let spec = AgentSpec::from_command("claude --verbose").unwrap();
        assert_eq!(spec.kind, AgentKind::ClaudeCode);
        assert_eq!(spec.argv, vec!["claude", "--verbose"]);
        assert!(spec.supports_sessions());
        assert_eq!(
            spec.resume_args("abc").unwrap(),
            vec!["--resume".to_string(), "abc".to_string()]
        );
    }

    #[test]
    fn claude_sessions_get_an_id_a_name_and_settings() {
        let spec = AgentSpec::from_command("claude").unwrap();
        assert_eq!(
            spec.new_session_args("abc", "Fix the login").unwrap(),
            ["--session-id", "abc", "-n", "Fix the login"]
        );
        assert_eq!(
            spec.settings_args(r#"{"hooks":{}}"#).unwrap(),
            ["--settings", r#"{"hooks":{}}"#]
        );
    }

    #[test]
    fn detects_claude_by_path_not_just_name() {
        let spec = AgentSpec::from_command("/opt/bin/claude").unwrap();
        assert_eq!(spec.kind, AgentKind::ClaudeCode);
    }

    #[test]
    fn unknown_agent_has_no_sessions() {
        let spec = AgentSpec::from_command("aider").unwrap();
        assert!(!spec.supports_sessions());
        assert!(spec.resume_args("x").is_none());
        assert!(spec.new_session_args("x", "task").is_none());
        assert!(spec.settings_args("{}").is_none());
        assert!(spec.prompt_args("go").is_none());
    }

    #[test]
    fn a_first_prompt_comes_after_the_options() {
        let spec = AgentSpec::from_command("claude").unwrap();
        assert_eq!(spec.prompt_args("--help me").unwrap(), ["--", "--help me"]);
    }

    #[test]
    fn extracts_queued_prompt_summary() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"type":"queue-operation","content":"do a thing\nplease"}"#)
                .unwrap();
        assert_eq!(extract_summary(&value).unwrap(), "do a thing please");
    }

    #[test]
    fn extracts_user_message_summary() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"type":"user","message":{"content":[{"text":"hello"}]}}"#)
                .unwrap();
        assert_eq!(extract_summary(&value).unwrap(), "hello");
    }
}
