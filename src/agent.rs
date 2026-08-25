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

    pub fn supports_sessions(&self) -> bool {
        self.kind == AgentKind::ClaudeCode
    }
}

/// A previously recorded agent session for a worktree.
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub id: String,
    /// Unix seconds of the last write.
    pub modified: i64,
    pub summary: String,
    pub entries: usize,
}

impl AgentSession {
    pub fn age(&self) -> String {
        let now = crate::worktrees::now_secs();
        let delta = (now - self.modified).max(0);
        match delta {
            d if d < 60 => "just now".to_string(),
            d if d < 3600 => format!("{}m ago", d / 60),
            d if d < 86_400 => format!("{}h ago", d / 3600),
            d => format!("{}d ago", d / 86_400),
        }
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

/// Sessions recorded for `root`, most recently used first.
pub fn sessions_for(spec: &AgentSpec, root: &Path) -> Result<Vec<AgentSession>> {
    match spec.kind {
        AgentKind::ClaudeCode => claude_sessions(root),
        AgentKind::Unknown(ref name) => {
            bail!("jeet does not know where `{name}` stores its sessions")
        }
    }
}

fn claude_sessions(root: &Path) -> Result<Vec<AgentSession>> {
    let Some(projects) = claude_projects_dir() else {
        bail!("could not locate the Claude Code config directory");
    };
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let dir = projects.join(claude_project_slug(&canonical));
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
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (summary, entries) = summarise_session(&path);
        sessions.push(AgentSession {
            id,
            modified,
            summary,
            entries,
        });
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
}

/// Best-effort first prompt (or recorded summary) plus the transcript length.
fn summarise_session(path: &Path) -> (String, usize) {
    // Transcripts run to many megabytes and the overlay summarises every
    // session in the project, so read them a line at a time rather than
    // holding the sum of all of them in memory at once.
    let Ok(file) = std::fs::File::open(path) else {
        return ("(unreadable transcript)".to_string(), 0);
    };
    let mut summary = None;
    let mut count = 0;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if summary.is_some() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            summary = extract_summary(&value);
        }
    }
    (
        summary.unwrap_or_else(|| "(no prompt recorded)".to_string()),
        count,
    )
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
    fn detects_claude_by_path_not_just_name() {
        let spec = AgentSpec::from_command("/opt/bin/claude").unwrap();
        assert_eq!(spec.kind, AgentKind::ClaudeCode);
    }

    #[test]
    fn unknown_agent_has_no_sessions() {
        let spec = AgentSpec::from_command("aider").unwrap();
        assert!(!spec.supports_sessions());
        assert!(spec.resume_args("x").is_none());
        assert!(sessions_for(&spec, Path::new("/tmp")).is_err());
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
