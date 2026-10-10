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
            sync_interval_secs: None,
            assistant: AssistantConfig::default(),
            watch: None,
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
