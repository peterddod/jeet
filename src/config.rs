use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::github::MergeMethod;
use crate::manager::budget::{self, Limits};
use crate::permissions;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_scan_roots")]
    pub scan_roots: Vec<String>,

    /// Command used to open files from the explorer (default: $VISUAL/$EDITOR, else `vim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,

    /// The coding agent's harness: `harness = "claude-code"`, or a
    /// `[harness]` table with a `name` and a `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessConfig>,

    /// Coding agent command line (default: the harness's, `claude`). Kept
    /// for configs from before `harness`; `[harness] command` says the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    /// What `jeet review` does besides checking the PR out.
    #[serde(default, skip_serializing_if = "ReviewConfig::is_empty")]
    pub review: ReviewConfig,

    /// How `jeet pr merge` merges (`[merge]`).
    #[serde(default, skip_serializing_if = "MergeConfig::is_empty")]
    pub merge: MergeConfig,

    /// Minimum seconds between background fetches of the store (default:
    /// 300). `0` turns background syncing off; `jeet sync` still works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_interval_secs: Option<u64>,

    /// What the assistant may do (`[assistant]`).
    #[serde(default, skip_serializing_if = "AssistantConfig::is_empty")]
    pub assistant: AssistantConfig,

    /// What `jeet watch` looks at on GitHub (`[watch]`). Present, it also
    /// has the tasks list poll while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchConfig>,

    /// How tasks with a parent are watched over (`[managers]`).
    #[serde(default, skip_serializing_if = "managers_is_empty")]
    pub managers: Option<ManagersConfig>,
}

/// `harness` in config.toml: a name, or a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum HarnessConfig {
    /// `harness = "claude-code"`.
    Name(String),
    /// `[harness]` with `name = "claude-code"` and, to run something other
    /// than its default binary, `command = "/opt/bin/claude --verbose"`.
    Table {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// Accept the agent's "trust this folder?" prompt in task
        /// directories under `$JEET_HOME/tasks` (default: off).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_trust: Option<bool>,
        /// With `auto_trust`, accept it for review tasks too, whose
        /// checkouts are other people's PR branches (default: off).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_trust_reviews: Option<bool>,
    },
}

pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 300;

/// `[review]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// Shell command run in the background from the new worktree, e.g.
    /// `claude -p "/review-pr {pr}"`. `{pr}` and `{url}` are substituted; the
    /// PR's branch, base and title arrive as environment variables instead,
    /// because a branch name from a fork is not safe to paste into a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Desktop notification when the command finishes (default: on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
}

impl ReviewConfig {
    fn is_empty(&self) -> bool {
        self.command.is_none() && self.notify.is_none()
    }

    /// The configured command, if it says anything.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref().filter(|c| !c.trim().is_empty())
    }

    pub fn notify(&self) -> bool {
        self.notify.unwrap_or(true)
    }
}

/// `[merge]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeConfig {
    /// Merge method used when `--method` is not given and the repository
    /// allows it; otherwise the repository's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<MergeMethod>,
}

impl MergeConfig {
    fn is_empty(&self) -> bool {
        self.method.is_none()
    }
}

/// `[assistant]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantConfig {
    /// Capabilities the assistant may use, e.g. `task.create`. Nothing is
    /// allowed unless listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,

    /// Desktop notification for each action it takes (default: on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,

    /// What `jeet watch` does with each GitHub event (`[[assistant.rules]]`):
    /// the first rule that matches decides; none matching, it is notified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

impl AssistantConfig {
    fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.notify.is_none() && self.rules.is_empty()
    }

    pub fn notify(&self) -> bool {
        self.notify.unwrap_or(true)
    }
}

/// One `[[assistant.rules]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The kinds of event it is for; empty for any.
    #[serde(default)]
    pub on: Vec<EventKind>,
    /// Repositories it is for, as `owner/repo` with `*` for either part;
    /// empty for any.
    #[serde(default)]
    pub repos: Vec<String>,
    pub action: RuleAction,
}

/// What a GitHub event is about, as `jeet watch` tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    ReviewRequested,
    Mention,
    Comment,
    CiFailed,
    CiPassed,
    PrMerged,
    PrClosed,
    Assigned,
    ProjectItemChanged,
}

/// What a rule does with the events it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Hand it to the assistant, which decides what to do within what it is
    /// allowed.
    Assistant,
    /// Send it to the task whose pull request it is about; to the assistant
    /// when no task has it.
    Task,
    /// A desktop notification, nothing more.
    Notify,
}

/// `[watch]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchConfig {
    /// Only events from these repositories (`owner/repo`, `*` for either
    /// part); empty for every one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,

    /// GitHub Projects whose items are watched for changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectRef>,

    /// Seconds between polls (default: 120); `0` stops the tasks list
    /// polling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
}

pub const DEFAULT_WATCH_INTERVAL_SECS: u64 = 120;

impl WatchConfig {
    pub fn interval_secs(&self) -> u64 {
        self.interval_secs.unwrap_or(DEFAULT_WATCH_INTERVAL_SECS)
    }
}

/// `[managers]` in config.toml.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ManagersConfig {
    /// Minutes a task with a parent may sit idle before its parent is told
    /// (default: 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_report_mins: Option<u64>,

    /// How deep the tree under the assistant goes, the assistant being 0: a
    /// worker sits at most this deep, a manager one less (default: 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,

    /// Active tasks a manager may have under it, unless it was given fewer
    /// (default: 6). The assistant has no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,

    /// Tasks under the assistant whose agents may run at once (default: 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live: Option<u32>,

    /// The budget a manager, or a task under one, gets unless given one,
    /// e.g. `"4h"` or `"2h,40turns"` (default: `"4h"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_budget: Option<String>,

    /// What a new worker may do unless given `--allow` (default:
    /// `["worktree.add"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_default: Option<Vec<String>>,

    /// What a new manager may do unless given `--allow` (default: nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_default: Option<Vec<String>>,

    /// Capabilities, besides [`HELD_FLOOR`], that pass down only when named
    /// and are logged and notified when granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Vec<String>>,
}

impl ManagersConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn managers_is_empty(managers: &Option<ManagersConfig>) -> bool {
    managers.as_ref().is_none_or(ManagersConfig::is_empty)
}

pub const DEFAULT_IDLE_REPORT_MINS: u64 = 10;
pub const DEFAULT_MAX_DEPTH: u32 = 3;
pub const DEFAULT_MAX_CHILDREN: u32 = 6;
pub const DEFAULT_MAX_LIVE: u32 = 8;
pub const DEFAULT_BUDGET: &str = "4h";
pub const DEFAULT_WORKER_GRANT: &[&str] = &["worktree.add"];

/// Capabilities that are always held, whatever `[managers] held` says.
pub const HELD_FLOOR: &[&str] = &["pr.merge", "pr.comment", "pr.review", "task.delete"];

/// `[managers]`, checked and with its defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerPolicy {
    pub max_depth: u32,
    pub max_children: u32,
    // Read once `max_live` gates starts.
    #[allow(dead_code)]
    pub max_live: u32,
    pub default_budget: Limits,
    pub worker_default: Vec<&'static str>,
    pub manager_default: Vec<&'static str>,
    /// [`HELD_FLOOR`] and `[managers] held`.
    pub held: Vec<&'static str>,
}

/// A GitHub Project: `{ owner = "acme", number = 3 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub owner: String,
    pub number: u64,
}

fn default_scan_roots() -> Vec<String> {
    vec!["~/Projects".into(), "~/code".into()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan_roots: default_scan_roots(),
            editor: None,
            harness: None,
            agent: None,
            review: ReviewConfig::default(),
            merge: MergeConfig::default(),
            sync_interval_secs: None,
            assistant: AssistantConfig::default(),
            watch: None,
            managers: None,
        }
    }
}

impl Config {
    /// Editor command line, honouring `JEET_EDITOR`, then config, then `$VISUAL`/`$EDITOR`.
    pub fn editor_command(&self) -> String {
        first_set(
            [
                env_non_empty("JEET_EDITOR"),
                self.editor.clone(),
                env_non_empty("VISUAL"),
                env_non_empty("EDITOR"),
            ],
            "vim",
        )
    }

    /// The harness config.toml names, if it names one.
    pub fn harness_name(&self) -> Option<&str> {
        let name = match &self.harness {
            Some(HarnessConfig::Name(name)) => Some(name),
            Some(HarnessConfig::Table { name, .. }) => name.as_ref(),
            None => None,
        };
        name.map(|n| n.trim()).filter(|n| !n.is_empty())
    }

    /// `[harness] auto_trust` and `auto_trust_reviews`, both off unless
    /// set.
    pub fn auto_trust(&self) -> (bool, bool) {
        match &self.harness {
            Some(HarnessConfig::Table {
                auto_trust,
                auto_trust_reviews,
                ..
            }) => (
                auto_trust.unwrap_or(false),
                auto_trust.unwrap_or(false) && auto_trust_reviews.unwrap_or(false),
            ),
            _ => (false, false),
        }
    }

    /// Coding agent command line, honouring `JEET_AGENT`, then `[harness]
    /// command`, then `agent`, then the harness's `default`.
    pub fn agent_command(&self, default: Option<&str>) -> Option<String> {
        let command = match &self.harness {
            Some(HarnessConfig::Table { command, .. }) => command.clone(),
            _ => None,
        };
        [env_non_empty("JEET_AGENT"), command, self.agent.clone()]
            .into_iter()
            .flatten()
            .find(|v| !v.trim().is_empty())
            .or(default.map(str::to_string))
    }

    /// Background sync throttle, honouring `JEET_SYNC_INTERVAL_SECS`, then
    /// config. An unparseable override is ignored rather than fatal.
    pub fn sync_interval_secs(&self) -> u64 {
        env_non_empty("JEET_SYNC_INTERVAL_SECS")
            .and_then(|v| v.trim().parse().ok())
            .or(self.sync_interval_secs)
            .unwrap_or(DEFAULT_SYNC_INTERVAL_SECS)
    }

    /// `[managers] idle_report_mins`, else the default.
    pub fn idle_report_mins(&self) -> u64 {
        self.managers
            .as_ref()
            .and_then(|m| m.idle_report_mins)
            .unwrap_or(DEFAULT_IDLE_REPORT_MINS)
    }

    /// `[managers]` with its defaults, checked where it is used rather
    /// than when config.toml is read, so a mistake there breaks only the
    /// tree. It refuses an unknown capability, a held one in a default, a
    /// limit of 0 and a budget that does not parse.
    pub fn manager_policy(&self) -> Result<ManagerPolicy> {
        let m = self.managers.clone().unwrap_or_default();
        let limit = |key: &str, value: Option<u32>, default: u32| match value {
            Some(0) => bail!("[managers] {key} = 0 allows nothing; give at least 1"),
            Some(n) => Ok(n),
            None => Ok(default),
        };
        let mut held = HELD_FLOOR.to_vec();
        for name in capabilities("held", m.held.as_deref().unwrap_or_default())? {
            if !held.contains(&name) {
                held.push(name);
            }
        }
        let worker_default = match &m.worker_default {
            Some(names) => capabilities("worker_default", names)?,
            None => DEFAULT_WORKER_GRANT.to_vec(),
        };
        let manager_default = capabilities(
            "manager_default",
            m.manager_default.as_deref().unwrap_or_default(),
        )?;
        for (key, names) in [
            ("worker_default", &worker_default),
            ("manager_default", &manager_default),
        ] {
            if let Some(name) = names.iter().find(|n| held.contains(n)) {
                bail!(
                    "[managers] {key} names {name}, which is held: \
                     it passes down only when a grant names it"
                );
            }
        }
        let budget = m.default_budget.as_deref().unwrap_or(DEFAULT_BUDGET);
        let default_budget =
            budget::parse_limits(budget).map_err(|e| anyhow!("[managers] default_budget: {e}"))?;
        Ok(ManagerPolicy {
            max_depth: limit("max_depth", m.max_depth, DEFAULT_MAX_DEPTH)?,
            max_children: limit("max_children", m.max_children, DEFAULT_MAX_CHILDREN)?,
            max_live: limit("max_live", m.max_live, DEFAULT_MAX_LIVE)?,
            default_budget,
            worker_default,
            manager_default,
            held,
        })
    }
}

/// `names` as capabilities, refusing one jeet does not know.
fn capabilities(key: &str, names: &[String]) -> Result<Vec<&'static str>> {
    names
        .iter()
        .map(|name| match permissions::capability(name.trim()) {
            Some(c) => Ok(c.name),
            None => bail!("[managers] {key}: {name} is not a capability"),
        })
        .collect()
}

/// First non-blank candidate, in precedence order, else `default`.
fn first_set(candidates: impl IntoIterator<Item = Option<String>>, default: &str) -> String {
    candidates
        .into_iter()
        .flatten()
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
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

/// Where task workspaces live, one directory per task.
pub fn tasks_root(home: &Path) -> PathBuf {
    home.join("tasks")
}

/// Project templates a task can be started from.
pub fn projects_root(home: &Path) -> PathBuf {
    home.join("projects")
}

/// The assistant's home: its AGENTS.md, worklog, todo, inbox and
/// `actions.log`.
pub fn assistant_dir(home: &Path) -> PathBuf {
    home.join("assistant")
}

/// What `jeet watch` remembers between polls.
pub fn watch_state_path(home: &Path) -> PathBuf {
    home.join("watch.json")
}

/// Where free-standing worktrees lived before tasks. Nothing new is made
/// here; `jeet import` moves what is left into tasks.
pub fn worktrees_root(home: &Path) -> PathBuf {
    home.join("worktrees")
}

/// Where `jeet exec --ephemeral` put its throwaway checkouts before tasks;
/// `jeet import` moves any that survived into tasks.
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
    fn precedence_prefers_the_first_set_candidate() {
        // Exercised directly so the test does not depend on the ambient
        // JEET_EDITOR/VISUAL/EDITOR of whoever runs it.
        assert_eq!(
            first_set([None, Some("hx".into()), Some("vi".into()), None], "vim"),
            "hx"
        );
        assert_eq!(
            first_set([Some("code".into()), Some("hx".into()), None, None], "vim"),
            "code"
        );
        assert_eq!(
            first_set([None, Some("  ".into()), None, None], "vim"),
            "vim"
        );
        assert_eq!(first_set([None, None, None, None], "claude"), "claude");
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
        assert!(!out.contains("review"), "{out}");
        assert!(!out.contains("sync"), "{out}");
    }

    #[test]
    fn reads_the_harness_as_a_name_or_a_table() {
        let config: Config = toml::from_str("harness = \"claude-code\"\n").unwrap();
        assert_eq!(config.harness_name(), Some("claude-code"));
        assert_eq!(
            config.harness,
            Some(HarnessConfig::Name("claude-code".into()))
        );

        let text = "[harness]\nname = \"claude-code\"\ncommand = \"/opt/claude --verbose\"\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.harness_name(), Some("claude-code"));
        assert_eq!(
            config.harness,
            Some(HarnessConfig::Table {
                name: Some("claude-code".into()),
                command: Some("/opt/claude --verbose".into()),
                auto_trust: None,
                auto_trust_reviews: None,
            })
        );
        let out = toml::to_string_pretty(&config).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&out).unwrap().harness,
            config.harness
        );

        // A typo is an error, not a silently ignored key.
        assert!(toml::from_str::<Config>("[harness]\nnmae = \"x\"\n").is_err());
        assert!(Config::default().harness_name().is_none());
    }

    #[test]
    fn auto_trust_is_off_unless_asked_and_reviews_need_their_own_yes() {
        let read = |text: &str| toml::from_str::<Config>(text).unwrap().auto_trust();
        assert_eq!(Config::default().auto_trust(), (false, false));
        assert_eq!(read("harness = \"claude-code\"\n"), (false, false));
        assert_eq!(read("[harness]\nauto_trust = true\n"), (true, false));
        assert_eq!(
            read("[harness]\nauto_trust = true\nauto_trust_reviews = true\n"),
            (true, true)
        );
        // Reviews only ever on top of tasks.
        assert_eq!(
            read("[harness]\nauto_trust_reviews = true\n"),
            (false, false)
        );
    }

    #[test]
    fn reads_the_sync_interval() {
        let config: Config = toml::from_str("sync_interval_secs = 0\n").unwrap();
        assert_eq!(config.sync_interval_secs, Some(0));
        assert!(Config::default().sync_interval_secs.is_none());
    }

    #[test]
    fn reads_the_review_section() {
        let text =
            "scan_roots = []\n[review]\ncommand = \"claude -p '/review {pr}'\"\nnotify = false\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.review.command(), Some("claude -p '/review {pr}'"));
        assert!(!config.review.notify());
        assert!(Config::default().review.command().is_none());
        assert!(Config::default().review.notify());
    }

    #[test]
    fn reads_the_assistant_section() {
        let text = "[assistant]\nallow = [\"task.create\", \"task.send\"]\nnotify = false\n";
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.assistant.allow, ["task.create", "task.send"]);
        assert!(!config.assistant.notify());
        // Nothing is allowed until the user says so.
        assert!(Config::default().assistant.allow.is_empty());
        assert!(Config::default().assistant.notify());
        let out = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!out.contains("assistant"), "{out}");
    }

    #[test]
    fn reads_the_managers_section() {
        let config: Config = toml::from_str("[managers]\nidle_report_mins = 5\n").unwrap();
        assert_eq!(config.idle_report_mins(), 5);
        assert_eq!(
            Config::default().idle_report_mins(),
            DEFAULT_IDLE_REPORT_MINS
        );
        assert_eq!(DEFAULT_IDLE_REPORT_MINS, 10);
        let out = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!out.contains("managers"), "{out}");
        // A `placement` key left from an earlier design is ignored: managers
        // run locally, and there is nothing to choose.
        let config: Config =
            toml::from_str("[managers]\nplacement = \"cloud\"\nidle_report_mins = 5\n").unwrap();
        assert_eq!(config.idle_report_mins(), 5);
        assert!(config.manager_policy().is_ok());
        // A `[managers]` present but empty writes nothing back.
        let config: Config = toml::from_str("[managers]\n").unwrap();
        assert!(config.managers.is_some());
        let out = toml::to_string_pretty(&config).unwrap();
        assert!(!out.contains("managers"), "{out}");
    }

    fn policy_of(text: &str) -> Result<ManagerPolicy> {
        toml::from_str::<Config>(&format!("[managers]\n{text}\n"))
            .unwrap()
            .manager_policy()
    }

    #[test]
    fn manager_policy_fills_in_its_defaults() {
        let policy = Config::default().manager_policy().unwrap();
        assert_eq!(
            policy,
            ManagerPolicy {
                max_depth: 3,
                max_children: 6,
                max_live: 8,
                default_budget: Limits {
                    secs: Some(4 * 3600),
                    turns: None
                },
                worker_default: vec!["worktree.add"],
                manager_default: vec![],
                held: HELD_FLOOR.to_vec(),
            }
        );

        let text = "max_depth = 2\nmax_children = 3\nmax_live = 4\n\
                    default_budget = \"2h,40turns\"\nworker_default = []\n\
                    manager_default = [\"task.create\", \"worktree.add\"]\n\
                    held = [\"task.archive\", \"pr.merge\"]";
        let policy = policy_of(text).unwrap();
        assert_eq!(
            (policy.max_depth, policy.max_children, policy.max_live),
            (2, 3, 4)
        );
        assert_eq!(
            policy.default_budget,
            Limits {
                secs: Some(7200),
                turns: Some(40)
            }
        );
        assert!(policy.worker_default.is_empty());
        assert_eq!(policy.manager_default, ["task.create", "worktree.add"]);
        // The floor stays, whatever `held` says, and nothing is listed twice.
        assert_eq!(
            policy.held,
            [
                "pr.merge",
                "pr.comment",
                "pr.review",
                "task.delete",
                "task.archive"
            ]
        );

        // Round trip: every key written back.
        let config: Config = toml::from_str(&format!("[managers]\n{text}\n")).unwrap();
        let out = toml::to_string_pretty(&config).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&out).unwrap().managers,
            config.managers
        );
    }

    #[test]
    fn manager_policy_refuses_what_it_cannot_honour() {
        let refused = |text: &str, says: &str| {
            let err = policy_of(text).unwrap_err().to_string();
            assert!(err.contains(says), "{text}: {err}");
        };
        // An unknown capability, in each list.
        refused(
            "worker_default = [\"task.fly\"]",
            "worker_default: task.fly is not",
        );
        refused(
            "manager_default = [\"nope\"]",
            "manager_default: nope is not",
        );
        refused("held = [\"nope\"]", "held: nope is not");
        // A held capability in a default: the floor's, and one added.
        refused(
            "worker_default = [\"pr.merge\"]",
            "worker_default names pr.merge",
        );
        refused(
            "manager_default = [\"task.delete\"]",
            "manager_default names task.delete",
        );
        refused(
            "held = [\"task.create\"]\nmanager_default = [\"task.create\"]",
            "names task.create, which is held",
        );
        // A limit of 0.
        for key in ["max_depth", "max_children", "max_live"] {
            refused(&format!("{key} = 0"), &format!("{key} = 0 allows nothing"));
        }
        // A budget that does not parse, costs money, is 0 or repeats a unit.
        refused("default_budget = \"4\"", "default_budget");
        refused("default_budget = \"$5\"", "#26");
        refused("default_budget = \"0h\"", "default_budget");
        refused("default_budget = \"1h,30m\"", "time twice");
        refused("default_budget = \"\"", "default_budget");
        // A mistake there does not stop config.toml from loading.
        assert!(toml::from_str::<Config>("[managers]\nmax_live = 0\n").is_ok());
    }

    #[test]
    fn reads_the_watch_section_and_rules() {
        let text = r#"
[watch]
repos = ["acme/*", "me/jeet"]
projects = [{ owner = "acme", number = 3 }]
interval_secs = 60

[[assistant.rules]]
on = ["review_requested"]
repos = ["acme/*"]
action = "assistant"

[[assistant.rules]]
on = ["ci_failed"]
action = "task"
"#;
        let config: Config = toml::from_str(text).unwrap();
        let watch = config.watch.unwrap();
        assert_eq!(watch.repos, ["acme/*", "me/jeet"]);
        assert_eq!(
            watch.projects,
            [ProjectRef {
                owner: "acme".into(),
                number: 3
            }]
        );
        assert_eq!(watch.interval_secs(), 60);
        let rules = &config.assistant.rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].on, [EventKind::ReviewRequested]);
        assert_eq!(rules[0].action, RuleAction::Assistant);
        assert!(rules[1].repos.is_empty());
        assert_eq!(rules[1].action, RuleAction::Task);

        // An empty `[watch]` turns watching on with the defaults; none at
        // all leaves it off.
        let config: Config = toml::from_str("[watch]\n").unwrap();
        assert_eq!(
            config.watch.unwrap().interval_secs(),
            DEFAULT_WATCH_INTERVAL_SECS
        );
        assert!(Config::default().watch.is_none());
        // A kind or an action jeet does not know is an error, not ignored.
        let bad = "[[assistant.rules]]\non = [\"reviewed\"]\naction = \"notify\"\n";
        assert!(toml::from_str::<Config>(bad).is_err());
        let bad = "[[assistant.rules]]\naction = \"merge\"\n";
        assert!(toml::from_str::<Config>(bad).is_err());
    }
}
