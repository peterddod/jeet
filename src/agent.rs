//! Coding-agent and editor integration.
//!
//! jeet shells out to whatever editor and coding agent you configure. The
//! agent's [`Harness`] says what else jeet can do with it: give it a session
//! id and a first prompt, install its hooks, resume its sessions.

use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};

use crate::config::{split_command, Config};
use crate::harness::{self, Harness};

#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Program plus any configured arguments.
    pub argv: Vec<String>,
    pub harness: &'static dyn Harness,
}

impl AgentSpec {
    /// The agent config.toml (or `$JEET_AGENT`) sets up. A `harness` names
    /// it; otherwise it is told by its program, e.g. `claude`.
    pub fn from_config(config: &Config) -> Result<Self> {
        let named = match config.harness_name() {
            Some(id) => Some(harness::by_id(id).ok_or_else(|| {
                anyhow::anyhow!(
                    "config.toml sets harness = {id:?}; jeet knows {}",
                    harness::ids()
                )
            })?),
            None => None,
        };
        let default = named
            .unwrap_or_else(harness::default_harness)
            .default_command();
        let Some(command) = config.agent_command(default) else {
            bail!(
                "no coding agent configured (set `harness` or `agent` in config.toml, or \
                 $JEET_AGENT)"
            );
        };
        let spec = Self::from_command(&command)?;
        Ok(match named {
            Some(harness) => Self { harness, ..spec },
            None => spec,
        })
    }

    /// `command`, with the harness its program runs.
    pub fn from_command(command: &str) -> Result<Self> {
        let argv = split_command(command);
        if argv.is_empty() {
            bail!("no coding agent configured (set `agent` in config.toml or $JEET_AGENT)");
        }
        let harness = harness::detect(&argv[0]);
        Ok(Self { argv, harness })
    }

    pub fn display(&self) -> String {
        self.argv.join(" ")
    }

    /// Whether jeet can record the agent's sessions and resume them.
    pub fn supports_sessions(&self) -> bool {
        self.harness.new_session_args("", "").is_some() && self.harness.resume_args("").is_some()
    }

    /// Whether the agent reports what it is doing through jeet's hooks.
    pub fn has_hooks(&self) -> bool {
        self.harness
            .hook_args(&harness::HookSetup::default())
            .is_some()
    }

    /// A warning when the agent is older than its harness was tested with;
    /// `--version` runs at most once per run.
    pub fn version_warning(&self) -> Option<String> {
        harness::version_warning(self.harness, &self.argv[0])
    }
}

/// A session an agent recorded for a directory.
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

/// A file's modification time in unix seconds.
pub fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_claude_agent() {
        let spec = AgentSpec::from_command("claude --verbose").unwrap();
        assert_eq!(spec.harness.id(), "claude-code");
        assert_eq!(spec.argv, vec!["claude", "--verbose"]);
        assert!(spec.supports_sessions());
        assert!(spec.has_hooks());
    }

    #[test]
    fn detects_claude_by_path_not_just_name() {
        let spec = AgentSpec::from_command("/opt/bin/claude").unwrap();
        assert_eq!(spec.harness.id(), "claude-code");
    }

    #[test]
    fn unknown_agent_has_no_sessions() {
        let spec = AgentSpec::from_command("aider").unwrap();
        assert_eq!(spec.harness.id(), "generic");
        assert!(!spec.supports_sessions());
        assert!(!spec.has_hooks());
    }
}
