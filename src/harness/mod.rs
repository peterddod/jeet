//! Coding-agent harnesses: what jeet knows about the agent it runs in a
//! task's tmux session.
//!
//! A [`Harness`] is everything jeet needs from one agent: how to launch it
//! (a new session under an id jeet picks, a resumed one, a first prompt),
//! how to install jeet's hooks, what its hook payloads mean as a
//! [`HookEvent`], which sessions it has recorded for a directory, and the
//! instruction file it reads. Each harness is one module here; the rest of
//! jeet sees only the trait and the neutral events.
//!
//! Whatever a harness cannot do it answers with `None` (or nothing), and
//! jeet does without: no status, no resume, no first prompt. [`Generic`] is
//! the harness that can do none of it, for any other command.
//!
//! `docs/harnesses.md` says how to add one.

pub mod claude;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::agent::AgentSession;

/// Every harness jeet knows, the default first.
pub static HARNESSES: &[&dyn Harness] = &[&claude::ClaudeCode, &Generic];

/// The harness used when nothing says otherwise.
pub fn default_harness() -> &'static dyn Harness {
    HARNESSES[0]
}

/// The harness called `id` in config.toml, e.g. `claude-code`.
pub fn by_id(id: &str) -> Option<&'static dyn Harness> {
    HARNESSES.iter().copied().find(|h| h.id() == id.trim())
}

/// The harness a command runs, going by its program's file name; anything
/// unrecognised is [`Generic`].
pub fn detect(program: &str) -> &'static dyn Harness {
    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string());
    HARNESSES
        .iter()
        .copied()
        .find(|h| h.runs_program(&name))
        .unwrap_or(&Generic)
}

/// Whether `harness` installs jeet's hooks, either way (see
/// `runtime::hook_args`).
pub fn has_hooks(harness: &dyn Harness) -> bool {
    let hooks = HookSetup::default();
    harness.hook_args(&hooks).is_some()
        || harness
            .hook_file(&hooks, Path::new("/hooks.json"))
            .is_some()
}

/// What `harness` lacks that a manager needs (design §7), by name:
/// recorded sessions (without them `runtime::launch_argv` installs no
/// hooks, deny rules or first prompt), hook events (its budget and its
/// children's reports), continue-turn output (handing it what queued
/// mid-turn), `interrupt_keys` (pausing it) and deny rules (the git denies
/// that keep it from writing code). Nothing when it has them all. A worker
/// runs without them, with a warning.
pub fn manager_missing(harness: &dyn Harness) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if harness.new_session_args("", "").is_none() || harness.resume_args("").is_none() {
        missing.push("recorded sessions");
    }
    if !has_hooks(harness) {
        missing.push("hook events");
    }
    if harness.continue_output("").is_none() {
        missing.push("continue-turn output");
    }
    if harness.interrupt_keys().is_none() {
        missing.push("interrupt_keys");
    }
    if harness.enforcement() < Enforcement::Rules {
        missing.push("deny rules");
    }
    missing
}

/// `claude-code, generic`: what `harness` in config.toml may be.
pub fn ids() -> String {
    HARNESSES
        .iter()
        .map(|h| h.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A coding agent jeet can run. Every method but the first few has a
/// default that says "not supported".
pub trait Harness: Sync {
    /// What `harness` in config.toml calls it, e.g. `claude-code`.
    fn id(&self) -> &'static str;

    /// What messages call it, e.g. `Claude Code`.
    fn name(&self) -> &'static str;

    /// The command run when config.toml names the harness but no command.
    /// `None` when there is no sensible default.
    fn default_command(&self) -> Option<&'static str>;

    /// Whether a program file name (no directory) is this harness, so that
    /// `agent = "/opt/bin/claude"` needs no `harness` beside it.
    fn runs_program(&self, _program: &str) -> bool {
        false
    }

    /// The version jeet was last tested with. Only the latest is supported:
    /// an older one is warned about, never refused.
    fn tested_version(&self) -> Option<&'static str> {
        None
    }

    /// Arguments that start a new session with id `id` and display name
    /// `name`, so jeet can record the session before the agent runs.
    fn new_session_args(&self, _id: &str, _name: &str) -> Option<Vec<String>> {
        None
    }

    /// Arguments that resume recorded session `id`.
    fn resume_args(&self, _id: &str) -> Option<Vec<String>> {
        None
    }

    /// Arguments, after every other, that give an interactive session its
    /// first prompt.
    fn prompt_args(&self, _prompt: &str) -> Option<Vec<String>> {
        None
    }

    /// Arguments that install jeet's hooks for this launch only, without
    /// writing anything to disk.
    fn hook_args(&self, _hooks: &HookSetup) -> Option<Vec<String>> {
        None
    }

    /// [`Harness::hook_args`] read from a file at `path` instead: what to
    /// write there, and the arguments naming it. jeet launches with this when
    /// the harness has it, so the hooks and rules, however many, cost the
    /// command line only the path (see `runtime::hook_args`). A harness may
    /// have this alone; either counts as having hooks. The file is
    /// `<session>.json`, whatever its contents.
    fn hook_file(&self, _hooks: &HookSetup, _path: &Path) -> Option<(String, Vec<String>)> {
        None
    }

    /// How it keeps an agent from running what [`HookSetup::deny`] names.
    /// [`Enforcement::None`]: the deny list is dropped.
    fn enforcement(&self) -> Enforcement {
        Enforcement::None
    }

    /// The tmux key names that end the turn it is running, keeping the
    /// session; jeet presses them once, only while it is working or waiting
    /// on the user. `None`: it cannot be interrupted. A harness returning
    /// keys needs [`Harness::hook_args`] or [`Harness::hook_file`] too,
    /// since jeet presses nothing
    /// until a hook has reported since the launch, and
    /// [`Harness::has_draft`], or the keys are pressed over unsent text.
    fn interrupt_keys(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// What a hook installed by [`Harness::hook_args`] reported: `event`
    /// as the hook command named it (may be empty), `raw` its stdin.
    fn parse_hook(&self, _event: &str, _raw: &str) -> Result<HookEvent> {
        Ok(HookEvent::default())
    }

    /// What a hook prints for a turn that is ending to carry on instead,
    /// with `reason` as what to do next. `None`: it cannot, and what was
    /// queued for it is typed in once it is idle.
    fn continue_output(&self, _reason: &str) -> Option<String> {
        None
    }

    /// What a session-start hook prints to add `context` to the session.
    fn session_context_output(&self, _context: &str) -> Option<String> {
        None
    }

    /// Whether it has a record of session `id` started in `dir`, i.e.
    /// whether resuming it there would find anything.
    fn has_session(&self, _dir: &Path, _id: &str) -> bool {
        false
    }

    /// The sessions it recorded for `dir`, most recent first.
    fn sessions(&self, _dir: &Path) -> Result<Vec<AgentSession>> {
        Ok(Vec::new())
    }

    /// Copy the sessions started in `from` to where it looks for those
    /// started in `to`, returning those copied.
    fn copy_sessions(&self, _from: &Path, _to: &Path) -> Result<Vec<AgentSession>> {
        Ok(Vec::new())
    }

    /// What session `id`, started in `dir`, last said: its last `n`
    /// messages with text, oldest first, and how its last turn ended.
    /// `None` when it keeps no record jeet can read.
    fn last_messages(&self, _dir: &Path, _id: &str, _n: usize) -> Option<Result<LastTurn>> {
        None
    }

    /// When any of its sessions in `dir` last recorded anything; cheap.
    fn latest_activity(&self, _dir: &Path) -> Option<i64> {
        None
    }

    /// The prompt, if any, the agent is blocked at before its session can
    /// start, from the text of its tmux pane.
    fn startup_prompt(&self, _pane: &str) -> Option<StartupPrompt> {
        None
    }

    /// The next keys towards accepting the trust prompt the pane shows.
    /// `None` when it shows none, or not in a state jeet knows: then no key
    /// is sent.
    fn accept_trust(&self, _pane: &str) -> Option<TrustStep> {
        None
    }

    /// Whether its input box holds text the user typed and has not sent,
    /// from its tmux pane captured with styles (see
    /// [`crate::tmux::capture_styled`]): a message typed in then would be
    /// joined to it.
    fn has_draft(&self, _pane: &str) -> bool {
        false
    }

    /// Whether `status`, though it reads as working, is the agent showing
    /// the user a dialog of its own, such as a question: one an attached
    /// user may be typing an answer into, which [`Harness::has_draft`]
    /// does not see and its interrupt keys would discard.
    fn in_dialog(&self, _status: &crate::status::SessionStatus) -> bool {
        false
    }

    /// The file in a session's directory it reads its instructions from:
    /// `AGENTS.md`, or a file of its own that imports it (see
    /// [`Harness::instructions_import`]).
    fn instructions_file(&self) -> Option<&'static str> {
        None
    }

    /// What its [`Harness::instructions_file`] holds, when that is not
    /// `AGENTS.md`, for it to read `AGENTS.md` instead. jeet writes it next
    /// to every `AGENTS.md` of its own; see `src/instructions.rs`.
    fn instructions_import(&self) -> Option<&'static str> {
        None
    }
}

impl std::fmt::Debug for dyn Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Any other command: it just runs, with none of jeet's integration.
#[derive(Debug)]
pub struct Generic;

impl Harness for Generic {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn name(&self) -> &'static str {
        "the agent"
    }

    fn default_command(&self) -> Option<&'static str> {
        None
    }
}

/// How a harness keeps its agent from running what jeet denies it, weakest
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Enforcement {
    /// It cannot: deny rules are dropped.
    None,
    /// It refuses commands that match a deny rule, before any allow rule
    /// and without asking. A lint against mistakes, not a sandbox.
    Rules,
}

/// What jeet's hooks need to know, for [`Harness::hook_args`].
#[derive(Debug, Clone, Default)]
pub struct HookSetup {
    /// The jeet binary the hooks run, as `jeet task-hook`.
    pub exe: PathBuf,
    /// The task the agent is launched for, which each hook names with
    /// `--task`.
    pub task_id: String,
    /// Command prefixes the agent may run without asking, e.g.
    /// `jeet worktree`.
    pub allow: Vec<String>,
    /// Commands the agent may not run, even with the user's approval: words
    /// from the start of the command, where `*` matches any run of
    /// characters, each followed by any further arguments, e.g.
    /// `git push --force*`. Kept only by a harness with
    /// [`Enforcement::Rules`].
    pub deny: Vec<String>,
}

/// One hook report, in jeet's terms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEvent {
    /// The harness's own name for the event, kept in the status file.
    pub name: String,
    pub session_id: Option<String>,
    /// The session's working directory.
    pub cwd: Option<String>,
    /// `None` for an event that says nothing jeet uses.
    pub kind: Option<EventKind>,
}

/// What a session is doing, as a hook reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// A session began, or began again.
    SessionStarted(StartSource),
    /// Busy with a prompt; the detail says on what.
    Working(Option<String>),
    /// Sitting at its prompt, waiting for the next one, without a turn
    /// having just ended: the queue is typed in.
    Idle,
    /// Stopped mid-turn for the user: a permission, a question.
    NeedsInput(Option<String>),
    /// A turn finished; the queue is handed over.
    Stopped,
    /// The session ended. `cleared`: a new one replaces it in the same
    /// window (e.g. `/clear`).
    Ended { cleared: bool },
}

/// What a session last said, from [`Harness::last_messages`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastTurn {
    /// Oldest first; empty when it has said nothing yet.
    pub messages: Vec<AgentMessage>,
    pub end: TurnEnd,
    /// Unix seconds the last turn ended, or for one still running, when it
    /// last recorded anything. `None` when nothing says.
    pub at: Option<i64>,
}

/// One reply of the agent's, its text blocks joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentMessage {
    pub text: String,
    /// Unix seconds, when recorded.
    pub at: Option<i64>,
}

/// How a session's last turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "error", rename_all = "snake_case")]
pub enum TurnEnd {
    /// It finished as the agent meant it to.
    Finished,
    /// The user interrupted it.
    Interrupted,
    /// It stopped on an error from the model's API, as the agent put it.
    ApiError(String),
    /// It has not ended, or no turn has started.
    Running,
}

impl TurnEnd {
    pub fn label(&self) -> String {
        match self {
            TurnEnd::Finished => "finished".to_string(),
            TurnEnd::Interrupted => "interrupted".to_string(),
            TurnEnd::ApiError(error) => format!("API error: {error}"),
            TurnEnd::Running => "still running".to_string(),
        }
    }
}

/// A prompt an agent shows before its session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPrompt {
    /// Whether to trust the folder it was started in.
    Trust,
}

/// One step of accepting a trust prompt, as tmux key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStep {
    /// Keys that move the selection towards accepting; the pane is looked
    /// at again before anything else is sent.
    Select(Vec<&'static str>),
    /// Accepting is selected: the keys that confirm it.
    Confirm(Vec<&'static str>),
}

/// Why a session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartSource {
    /// From scratch.
    Startup,
    Resume,
    /// The conversation was cleared: a new session following on.
    Clear,
    /// The conversation was compacted mid-turn.
    Compact,
    /// Anything else that carries on from an earlier session.
    Other,
}

/// A word the shell reads back exactly, whatever it holds.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// When any harness recorded anything in `dir`.
pub fn latest_activity(dir: &Path) -> Option<i64> {
    HARNESSES
        .iter()
        .filter_map(|h| h.latest_activity(dir))
        .max()
}

/// Set to `0` to skip [`version_warning`]'s `--version` run.
pub const VERSION_CHECK_ENV: &str = "JEET_HARNESS_VERSION_CHECK";

/// A warning when `program --version` reports an older version of
/// `harness` than jeet was tested with. Runs it at most once per run; one
/// that will not run or says nothing parseable is not warned about.
pub fn version_warning(harness: &dyn Harness, program: &str) -> Option<String> {
    static CHECKED: OnceLock<Option<String>> = OnceLock::new();
    let tested = harness.tested_version()?;
    if std::env::var(VERSION_CHECK_ENV).is_ok_and(|v| v.trim() == "0") {
        return None;
    }
    CHECKED
        .get_or_init(|| {
            let found = installed_version(program)?;
            older_than(&found, tested).then(|| {
                format!(
                    "`{program} --version` says {found}, older than {} {tested}, the version \
                     jeet is tested with; update it if anything misbehaves",
                    harness.name()
                )
            })
        })
        .clone()
}

/// The version `program --version` prints, given a couple of seconds.
fn installed_version(program: &str) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().ok()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().ok()?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// The first `1.2.3`-like word of `text`.
fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .map(|w| w.trim_start_matches('v'))
        .find(|w| version_parts(w).is_some_and(|p| p.len() >= 2))
        .map(str::to_string)
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|p| p.parse().ok()).collect()
}

/// Whether `found` is an older version than `tested`; unparseable is not.
fn older_than(found: &str, tested: &str) -> bool {
    match (version_parts(found), version_parts(tested)) {
        (Some(found), Some(tested)) => found < tested,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harnesses_are_found_by_id_and_program() {
        assert_eq!(default_harness().id(), "claude-code");
        assert_eq!(by_id("claude-code").unwrap().id(), "claude-code");
        assert_eq!(by_id(" generic ").unwrap().id(), "generic");
        assert!(by_id("claude").is_none());
        assert_eq!(detect("claude").id(), "claude-code");
        assert_eq!(detect("/opt/bin/claude").id(), "claude-code");
        assert_eq!(detect("claude-beta").id(), "claude-code");
        assert_eq!(detect("aider").id(), "generic");
        assert_eq!(ids(), "claude-code, generic");
    }

    #[test]
    fn the_generic_harness_supports_nothing() {
        let h = &Generic;
        assert!(h.new_session_args("x", "n").is_none());
        assert!(h.resume_args("x").is_none());
        assert!(h.prompt_args("go").is_none());
        let hooks = HookSetup {
            exe: "/bin/jeet".into(),
            ..Default::default()
        };
        assert!(h.hook_args(&hooks).is_none());
        assert!(h.hook_file(&hooks, Path::new("/s.json")).is_none());
        assert_eq!(h.enforcement(), Enforcement::None);
        assert!(h.interrupt_keys().is_none());
        assert!(h.parse_hook("Stop", "{}").unwrap().kind.is_none());
        assert!(h.continue_output("x").is_none());
        assert!(h.session_context_output("x").is_none());
        assert!(!h.has_session(Path::new("/"), "x"));
        assert!(h.startup_prompt("Yes, I trust this folder").is_none());
        assert!(h.accept_trust("Yes, I trust this folder").is_none());
        assert!(h.tested_version().is_none());
        assert!(h.last_messages(Path::new("/"), "x", 1).is_none());
    }

    /// A harness with everything a manager needs, less what `drop` names.
    struct Partial(&'static str);

    impl Harness for Partial {
        fn id(&self) -> &'static str {
            "partial"
        }
        fn name(&self) -> &'static str {
            "partial"
        }
        fn default_command(&self) -> Option<&'static str> {
            None
        }
        fn new_session_args(&self, _id: &str, _name: &str) -> Option<Vec<String>> {
            (self.0 != "sessions").then(Vec::new)
        }
        fn resume_args(&self, _id: &str) -> Option<Vec<String>> {
            (self.0 != "resume").then(Vec::new)
        }
        fn hook_args(&self, _hooks: &HookSetup) -> Option<Vec<String>> {
            (self.0 != "hooks").then(Vec::new)
        }
        fn enforcement(&self) -> Enforcement {
            match self.0 {
                "rules" => Enforcement::None,
                _ => Enforcement::Rules,
            }
        }
        fn interrupt_keys(&self) -> Option<&'static [&'static str]> {
            (self.0 != "interrupt").then_some(&["Escape"][..])
        }
        fn continue_output(&self, _reason: &str) -> Option<String> {
            (self.0 != "continue").then(String::new)
        }
    }

    #[test]
    fn a_manager_needs_sessions_hooks_continue_output_interrupt_and_rules() {
        assert!(manager_missing(&claude::ClaudeCode).is_empty());
        assert!(manager_missing(&Partial("")).is_empty());
        assert_eq!(
            manager_missing(&Generic),
            [
                "recorded sessions",
                "hook events",
                "continue-turn output",
                "interrupt_keys",
                "deny rules"
            ]
        );
        for (drop, named) in [
            ("sessions", "recorded sessions"),
            ("resume", "recorded sessions"),
            ("hooks", "hook events"),
            ("continue", "continue-turn output"),
            ("interrupt", "interrupt_keys"),
            ("rules", "deny rules"),
        ] {
            assert_eq!(manager_missing(&Partial(drop)), [named], "{drop}");
        }
    }

    #[test]
    fn versions_are_read_and_compared() {
        assert_eq!(
            parse_version("2.1.296 (Claude Code)\n").as_deref(),
            Some("2.1.296")
        );
        assert_eq!(parse_version("goose v1.9.0").as_deref(), Some("1.9.0"));
        assert_eq!(parse_version("opencode 0.15").as_deref(), Some("0.15"));
        assert!(parse_version("no version here 7").is_none());
        assert!(older_than("2.0.9", "2.1.0"));
        assert!(older_than("2.1", "2.1.0"));
        assert!(!older_than("2.10.0", "2.9.9"));
        assert!(!older_than("2.1.0", "2.1.0"));
        assert!(!older_than("junk", "2.1.0"));
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
}
