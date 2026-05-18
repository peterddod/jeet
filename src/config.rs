use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_scan_roots")]
    pub scan_roots: Vec<String>,
}

fn default_scan_roots() -> Vec<String> {
    vec!["~/Projects".into(), "~/code".into()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan_roots: default_scan_roots(),
        }
    }
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
