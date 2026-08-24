use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_scan_roots")]
    pub scan_roots: Vec<String>,

    /// Command used to open files from the explorer (default: $VISUAL/$EDITOR, else `vim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,

    /// Coding agent launched with `c` in the explorer (default: `claude`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

fn default_scan_roots() -> Vec<String> {
    vec!["~/Projects".into(), "~/code".into()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan_roots: default_scan_roots(),
            editor: None,
            agent: None,
        }
    }
}

impl Config {
    /// Editor command line, honouring `JEET_EDITOR`, then config, then `$VISUAL`/`$EDITOR`.
    pub fn editor_command(&self) -> String {
        env_non_empty("JEET_EDITOR")
            .or_else(|| self.editor.clone().filter(|s| !s.trim().is_empty()))
            .or_else(|| env_non_empty("VISUAL"))
            .or_else(|| env_non_empty("EDITOR"))
            .unwrap_or_else(|| "vim".to_string())
    }

    /// Coding agent command line, honouring `JEET_AGENT`, then config.
    pub fn agent_command(&self) -> String {
        env_non_empty("JEET_AGENT")
            .or_else(|| self.agent.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "claude".to_string())
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Split a configured command line into program + args, honouring simple quoting.
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => current.push(c),
        }
    }
    if has_token || !current.is_empty() {
        parts.push(current);
    }
    parts
}

pub fn jeet_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("JEET_HOME") {
        return Ok(PathBuf::from(home));
    }
    dirs::home_dir()
        .map(|h| h.join(".jeet"))
        .context("could not determine home directory")
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

pub fn index_path(home: &Path) -> PathBuf {
    home.join("index.db")
}

pub fn store_root(home: &Path) -> PathBuf {
    home.join("store")
}

pub fn worktrees_root(home: &Path) -> PathBuf {
    home.join("worktrees")
}

pub fn ephemeral_root(home: &Path) -> PathBuf {
    home.join("ephemeral")
}

pub fn load_or_create(home: &Path) -> Result<Config> {
    std::fs::create_dir_all(home).context("create jeet home")?;
    let path = config_path(home);
    if path.exists() {
        let text = std::fs::read_to_string(&path).context("read config")?;
        toml::from_str(&text).context("parse config")
    } else {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).context("serialize config")?;
        std::fs::write(&path, text).context("write default config")?;
        Ok(config)
    }
}

pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands_with_quotes() {
        assert_eq!(split_command("vim"), vec!["vim"]);
        assert_eq!(split_command("code --wait"), vec!["code", "--wait"]);
        assert_eq!(
            split_command("claude --append \"be brief\""),
            vec!["claude", "--append", "be brief"]
        );
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn config_values_win_over_defaults() {
        let config = Config {
            editor: Some("hx".into()),
            agent: Some("aider --model x".into()),
            ..Config::default()
        };
        assert_eq!(config.editor_command(), "hx");
        assert_eq!(config.agent_command(), "aider --model x");
    }

    #[test]
    fn config_round_trips_without_optional_fields() {
        let text = "scan_roots = [\"~/src\"]\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.scan_roots, vec!["~/src".to_string()]);
        assert!(config.editor.is_none());
        assert!(config.agent.is_none());
        // serialising again must not invent keys
        let out = toml::to_string_pretty(&config).unwrap();
        assert!(!out.contains("editor"), "{out}");
    }
}
