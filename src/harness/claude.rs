//! Claude Code.
//!
//! jeet picks each session's id (`--session-id`, `-n`), resumes with
//! `--resume`, and gives a first prompt after `--`. Its hooks arrive in a
//! `--settings` JSON layered over the user's own, which also pre-approves
//! the jeet commands the agent may run. Each hook runs `jeet task-hook
//! <Event>` with the event's JSON on stdin.
//!
//! Sessions are found by their transcripts, in
//! `$CLAUDE_CONFIG_DIR/projects/<slug>/<id>.jsonl` (default `~/.claude`),
//! which are also where what a session last said is read from.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    shell_quote, AgentMessage, EventKind, Harness, HookEvent, HookSetup, LastTurn, StartSource,
    StartupPrompt, TrustStep, TurnEnd,
};
use crate::agent::{mtime_secs, AgentSession};
use crate::status::{first_line, DETAIL_MAX};

/// Every hook event jeet installs.
pub const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "SubagentStop",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// Seconds Claude gives a hook before giving up on it.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The trust prompt Claude shows once for a folder not under one already
/// trusted, before any hook runs:
///
/// ```text
///  Quick safety check: Is this a project you created or one you trust? …
///  ❯ No, exit
///    Yes, I trust this folder
///  Enter to confirm · Esc to cancel
/// ```
///
/// "No" is selected first, so Enter alone exits.
const TRUST_QUESTION: &str = "Is this a project you created or one you trust?";
const TRUST_YES: &str = "Yes, I trust this folder";
const TRUST_NO: &str = "No, exit";
/// The file Claude Code reads its instructions from.
pub const INSTRUCTIONS_FILE: &str = "CLAUDE.md";

/// A `CLAUDE.md` that reads `AGENTS.md`: Claude Code reads the import as if
/// the file were inlined.
pub const IMPORT_AGENTS_MD: &str = "@AGENTS.md\n";

/// What marks the selected option.
const CURSOR: char = '❯';

#[derive(Debug)]
pub struct ClaudeCode;

impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn default_command(&self) -> Option<&'static str> {
        Some("claude")
    }

    fn runs_program(&self, program: &str) -> bool {
        program == "claude" || program.starts_with("claude-")
    }

    fn tested_version(&self) -> Option<&'static str> {
        Some("2.1.296")
    }

    fn new_session_args(&self, id: &str, name: &str) -> Option<Vec<String>> {
        Some(vec![
            "--session-id".to_string(),
            id.to_string(),
            "-n".to_string(),
            name.to_string(),
        ])
    }

    fn resume_args(&self, id: &str) -> Option<Vec<String>> {
        Some(vec!["--resume".to_string(), id.to_string()])
    }

    /// `--` first, so a prompt that starts with a dash is a prompt, not a
    /// flag.
    fn prompt_args(&self, prompt: &str) -> Option<Vec<String>> {
        Some(vec!["--".to_string(), prompt.to_string()])
    }

    fn hook_args(&self, hooks: &HookSetup) -> Option<Vec<String>> {
        Some(vec!["--settings".to_string(), settings_json(hooks)])
    }

    fn parse_hook(&self, event: &str, raw: &str) -> Result<HookEvent> {
        let input: HookInput = serde_json::from_str(raw).context("parse the hook input")?;
        let name = match event {
            "" => input.hook_event_name.clone().unwrap_or_default(),
            named => named.to_string(),
        };
        Ok(HookEvent {
            kind: event_kind(&name, &input),
            name,
            session_id: input.session_id,
            cwd: input.cwd,
        })
    }

    /// Claude reads the Stop hook's `block` decision as "do not stop yet",
    /// and the reason as what to do next.
    fn continue_output(&self, reason: &str) -> Option<String> {
        Some(serde_json::json!({ "decision": "block", "reason": reason }).to_string())
    }

    fn session_context_output(&self, context: &str) -> Option<String> {
        Some(
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": context,
                }
            })
            .to_string(),
        )
    }

    fn has_session(&self, dir: &Path, id: &str) -> bool {
        session_dir(dir).is_some_and(|d| d.join(format!("{id}.jsonl")).is_file())
    }

    fn sessions(&self, dir: &Path) -> Result<Vec<AgentSession>> {
        sessions(dir)
    }

    /// `to` must exist: Claude names the store by its real path. A session
    /// already at `to` is left alone and not counted.
    fn copy_sessions(&self, from: &Path, to: &Path) -> Result<Vec<AgentSession>> {
        let sessions = self.sessions(from)?;
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let (Some(source), Some(dest)) = (session_dir(from), session_dir(to)) else {
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

    fn last_messages(&self, dir: &Path, id: &str, n: usize) -> Option<Result<LastTurn>> {
        let Some(sessions) = session_dir(dir) else {
            return Some(Err(anyhow::anyhow!(
                "could not locate the Claude Code config directory"
            )));
        };
        Some(last_turn(&sessions.join(format!("{id}.jsonl")), n))
    }

    /// Only directory metadata is read, so it is cheap however long the
    /// transcripts are.
    fn latest_activity(&self, dir: &Path) -> Option<i64> {
        let dir = session_dir(dir)?;
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| mtime_secs(&e.metadata().ok()?))
            .max()
    }

    fn startup_prompt(&self, pane: &str) -> Option<StartupPrompt> {
        // Joined up, so a question the pane wrapped still matches.
        let flat = pane.split_whitespace().collect::<Vec<_>>().join(" ");
        (flat.contains(TRUST_QUESTION) && flat.contains(TRUST_YES)).then_some(StartupPrompt::Trust)
    }

    /// Down to "Yes", and Enter only once "Yes" is seen selected: Enter on
    /// "No" exits.
    fn accept_trust(&self, pane: &str) -> Option<TrustStep> {
        self.startup_prompt(pane)?;
        match selected_option(pane)? {
            TRUST_YES => Some(TrustStep::Confirm(vec!["Enter"])),
            TRUST_NO => Some(TrustStep::Select(vec!["Down"])),
            _ => None,
        }
    }

    fn has_draft(&self, pane: &str) -> bool {
        input_box(pane).is_some_and(|text| {
            !text
                .trim_start()
                .trim_start_matches(CURSOR)
                .trim()
                .is_empty()
        })
    }

    fn instructions_file(&self) -> Option<&'static str> {
        Some(INSTRUCTIONS_FILE)
    }

    fn instructions_import(&self) -> Option<&'static str> {
        Some(IMPORT_AGENTS_MD)
    }
}

/// What the input box shows, with the dimmed placeholder Claude puts in an
/// empty one left out: the lines between the last two rules across the
/// pane. The rule above may carry the session's name (`──── fix-x ─`).
/// `pane` is captured with styles.
fn input_box(pane: &str) -> Option<String> {
    let lines: Vec<String> = pane.lines().map(undimmed).collect();
    let is_rule = |line: &String| {
        let line = line.trim_end();
        line.starts_with("───") && line.ends_with('─')
    };
    let below = lines.iter().rposition(is_rule)?;
    let above = lines[..below].iter().rposition(is_rule)?;
    Some(lines[above + 1..below].join("\n"))
}

/// `line` without its escape sequences, and without the text they dim.
fn undimmed(line: &str) -> String {
    let mut text = String::new();
    let mut dim = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if !dim {
                text.push(c);
            }
            continue;
        }
        // CSI: `ESC [`, parameters, then a final letter; only SGR (`m`)
        // matters here.
        if chars.next() != Some('[') {
            continue;
        }
        let mut params = String::new();
        let end = chars.by_ref().find(|&c| {
            let done = c.is_ascii_alphabetic() || c == '~';
            if !done {
                params.push(c);
            }
            done
        });
        if end != Some('m') {
            continue;
        }
        for param in params.split(';') {
            match param {
                "2" => dim = true,
                "" | "0" | "22" => dim = false,
                _ => {}
            }
        }
    }
    text
}

/// The option the cursor is on, in a pane showing a list of them.
fn selected_option(pane: &str) -> Option<&str> {
    pane.lines()
        .filter_map(|line| line.trim().strip_prefix(CURSOR))
        .map(str::trim)
        .next_back()
}

/// The fields of a Claude Code hook payload that jeet reads; every event
/// sends a different subset.
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    /// SessionEnd's: why it ended (`clear`, `logout`, `prompt_input_exit`…).
    #[serde(default)]
    reason: Option<String>,
}

fn event_kind(event: &str, input: &HookInput) -> Option<EventKind> {
    Some(match event {
        "SessionStart" => EventKind::SessionStarted(match input.source.as_deref() {
            Some("startup") | None => StartSource::Startup,
            Some("resume") => StartSource::Resume,
            Some("clear") => StartSource::Clear,
            Some("compact") => StartSource::Compact,
            Some(_) => StartSource::Other,
        }),
        "UserPromptSubmit" => EventKind::Working(
            input
                .prompt
                .as_deref()
                .and_then(|p| first_line(p, DETAIL_MAX)),
        ),
        "PreToolUse" => EventKind::Working(input.tool_name.as_deref().map(|tool| match tool {
            "Workflow" => "running workflow".to_string(),
            "Agent" | "Task" => "waiting for subagents".to_string(),
            other => other.to_string(),
        })),
        "SubagentStop" => EventKind::Working(None),
        "Notification" if is_idle_prompt(input) => EventKind::Idle,
        "Notification" if asks_the_user(input) => EventKind::NeedsInput(
            input
                .message
                .as_deref()
                .and_then(|m| first_line(m, DETAIL_MAX)),
        ),
        "Stop" => EventKind::Stopped,
        "SessionEnd" => EventKind::Ended {
            cleared: input.reason.as_deref() == Some("clear"),
        },
        _ => return None,
    })
}

/// Claude nags with a notification after a minute of waiting at an empty
/// prompt; that is idleness, not a question for the user. A turn the user
/// interrupts ends with no Stop, so this is what types in anything queued.
fn is_idle_prompt(input: &HookInput) -> bool {
    input.notification_type.as_deref() == Some("idle_prompt")
        || input
            .message
            .as_deref()
            .is_some_and(|m| m.contains("waiting for your input"))
}

/// A notification that leaves Claude stuck until the user answers: a
/// permission prompt or an MCP elicitation, or an untyped one from a
/// Claude too old to say. Others (`auth_success`…) are only news.
fn asks_the_user(input: &HookInput) -> bool {
    matches!(
        input.notification_type.as_deref(),
        None | Some("permission_prompt") | Some("elicitation_dialog")
    )
}

/// The `--settings` JSON: a hook per event running `jeet task-hook`, and
/// permission to run the commands `allow` names, as `Bash(…)` rules.
fn settings_json(hooks: &HookSetup) -> String {
    let program = shell_quote(&hooks.exe.to_string_lossy());
    let mut groups = serde_json::Map::new();
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
        groups.insert(event.to_string(), serde_json::json!([group]));
    }
    let allow: Vec<String> = hooks
        .allow
        .iter()
        .map(|command| format!("Bash({command}:*)"))
        .collect();
    serde_json::json!({
        "hooks": groups,
        "permissions": { "allow": allow },
    })
    .to_string()
}

/// Root of Claude Code's per-project session store.
fn projects_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("projects"));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Claude Code encodes a project path by replacing every non-alphanumeric
/// character with `-`, e.g. `/home/me/.jeet/store` -> `-home-me--jeet-store`.
fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where Claude Code keeps the transcripts of sessions started in `root`.
fn session_dir(root: &Path) -> Option<PathBuf> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    Some(projects_dir()?.join(project_slug(&canonical)))
}

fn sessions(root: &Path) -> Result<Vec<AgentSession>> {
    let Some(dir) = session_dir(root) else {
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

/// What Claude writes in place of a reply to a turn the user interrupted,
/// with ` for tool use]` when it was at a permission prompt.
const INTERRUPTED: &str = "[Request interrupted by user";

/// User records that are a command typed at the prompt or its output,
/// which start no turn.
const LOCAL_COMMAND: [&str; 5] = [
    "<command-name>",
    "<local-command-",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];

/// The last `n` replies with text in the transcript at `path`, and how the
/// last turn ended.
///
/// Claude writes each block of a reply (thinking, text, a tool call) as its
/// own record, sharing the reply's `message.id`. A turn that finishes is
/// followed by a `turn_duration` system record; one the user interrupts by
/// a user record saying so; one that fails on the API by a synthetic reply
/// marked `isApiErrorMessage`, with the error as its text. Subagents'
/// records (`isSidechain`) are not the session's own and are skipped.
fn last_turn(path: &Path, n: usize) -> Result<LastTurn> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    // Transcripts run to many megabytes, so only the last `n` replies are
    // kept, with the id of the one being added to.
    let mut messages: VecDeque<(Option<String>, AgentMessage)> = VecDeque::new();
    let mut end = TurnEnd::Running;
    let mut at = None;
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if record["isSidechain"] == true {
            continue;
        }
        let time = record["timestamp"]
            .as_str()
            .and_then(crate::github::parse_timestamp);
        match record["type"].as_str() {
            Some("assistant") if record["isApiErrorMessage"] == true => {
                let text = record_text(&record["message"]["content"]);
                let error = text.strip_prefix("API Error: ").unwrap_or(&text);
                end = TurnEnd::ApiError(error.trim().to_string());
                at = time;
            }
            Some("assistant") => {
                end = match record["message"]["stop_reason"].as_str() {
                    Some("end_turn") => TurnEnd::Finished,
                    _ => TurnEnd::Running,
                };
                at = time;
                let text = record_text(&record["message"]["content"]);
                if text.is_empty() {
                    continue;
                }
                let id = record["message"]["id"].as_str().map(str::to_string);
                match messages.back_mut() {
                    Some((last, message)) if id.is_some() && *last == id => {
                        message.text.push_str("\n\n");
                        message.text.push_str(&text);
                    }
                    _ => {
                        messages.push_back((id, AgentMessage { text, at: time }));
                        if messages.len() > n {
                            messages.pop_front();
                        }
                    }
                }
            }
            Some("user") => {
                if record["isMeta"] == true {
                    continue;
                }
                let content = &record["message"]["content"];
                let text = match content.as_str() {
                    Some(text) => text.to_string(),
                    None => record_text(content),
                };
                if LOCAL_COMMAND.iter().any(|c| text.starts_with(c)) {
                    continue;
                }
                end = match text.starts_with(INTERRUPTED) {
                    true => TurnEnd::Interrupted,
                    false => TurnEnd::Running,
                };
                at = time;
            }
            // A turn ending, unless an error or the user already ended it.
            Some("system")
                if matches!(end, TurnEnd::Running | TurnEnd::Finished)
                    && matches!(
                        record["subtype"].as_str(),
                        Some("turn_duration" | "stop_hook_summary")
                    ) =>
            {
                end = TurnEnd::Finished;
                at = time;
            }
            _ => {}
        }
    }
    Ok(LastTurn {
        messages: messages.into_iter().map(|(_, m)| m).collect(),
        end,
        at,
    })
}

/// The text blocks of a record's content, joined.
fn record_text(content: &serde_json::Value) -> String {
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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
    use crate::status::{status_for, State};

    fn raw_fixture(event: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hooks/claude-code")
            .join(format!("{event}.json"));
        std::fs::read_to_string(path).unwrap()
    }

    fn fixture(event: &str) -> HookInput {
        serde_json::from_str(&raw_fixture(event)).unwrap()
    }

    fn parsed(event: &str, raw: &str) -> HookEvent {
        ClaudeCode.parse_hook(event, raw).unwrap()
    }

    #[test]
    fn each_event_leaves_the_session_in_its_state() {
        let cases: [(&str, State, Option<&str>); 7] = [
            ("SessionStart", State::Idle, None),
            (
                "UserPromptSubmit",
                State::Running,
                Some("Fix the login page so that the session cookie survives a pa…"),
            ),
            ("PreToolUse", State::Running, Some("Bash")),
            ("SubagentStop", State::Running, None),
            (
                "Notification",
                State::Waiting,
                Some("Claude needs your permission to use Bash"),
            ),
            ("Stop", State::Idle, None),
            ("SessionEnd", State::Ended, None),
        ];
        for (event, state, detail) in cases {
            let status = status_for(&parsed(event, &raw_fixture(event)), 1000).unwrap();
            assert_eq!(status.state, state, "{event}");
            assert_eq!(status.detail.as_deref(), detail, "{event}");
            assert_eq!(status.event, event);
            assert_eq!(status.session_id, "3f1c2a9e-5b7d-4e61-9a0b-2c4d6e8f0a1b");
            assert_eq!(status.at, 1000);
        }
        // Every installed event is covered.
        assert_eq!(cases.map(|c| c.0), HOOK_EVENTS);
        // A session /clear ends is over too, though another replaces it.
        let cleared = status_for(
            &parsed("SessionEnd", &raw_fixture("SessionEnd-clear")),
            1000,
        );
        assert_eq!(cleared.unwrap().state, State::Ended);
    }

    #[test]
    fn a_session_ended_by_clear_says_so() {
        for (fixture, cleared) in [("SessionEnd", false), ("SessionEnd-clear", true)] {
            assert_eq!(
                parsed("SessionEnd", &raw_fixture(fixture)).kind,
                Some(EventKind::Ended { cleared }),
                "{fixture}"
            );
        }
    }

    #[test]
    fn the_event_comes_from_the_payload_when_the_hook_names_none() {
        let event = parsed("", &raw_fixture("Stop"));
        assert_eq!(event.name, "Stop");
        assert_eq!(event.kind, Some(EventKind::Stopped));
        assert!(event.cwd.is_some());
    }

    #[test]
    fn sessions_start_for_a_reason() {
        let mut input = fixture("SessionStart");
        for (source, expected) in [
            (None, StartSource::Startup),
            (Some("startup"), StartSource::Startup),
            (Some("resume"), StartSource::Resume),
            (Some("clear"), StartSource::Clear),
            (Some("compact"), StartSource::Compact),
            (Some("fork"), StartSource::Other),
        ] {
            input.source = source.map(Into::into);
            assert_eq!(
                event_kind("SessionStart", &input),
                Some(EventKind::SessionStarted(expected))
            );
        }
    }

    #[test]
    fn tools_that_wait_on_others_say_so() {
        let mut input = fixture("PreToolUse");
        for (tool, detail) in [
            ("Workflow", "running workflow"),
            ("Agent", "waiting for subagents"),
            ("Task", "waiting for subagents"),
        ] {
            input.tool_name = Some(tool.into());
            assert_eq!(
                event_kind("PreToolUse", &input),
                Some(EventKind::Working(Some(detail.into())))
            );
        }
    }

    #[test]
    fn an_idle_prompt_notification_is_idleness() {
        let mut input = fixture("Notification-idle");
        assert_eq!(event_kind("Notification", &input), Some(EventKind::Idle));
        input.notification_type = None;
        assert_eq!(event_kind("Notification", &input), Some(EventKind::Idle));
    }

    #[test]
    fn only_notifications_that_ask_the_user_need_input() {
        let mut input = fixture("Notification");
        for kind in [None, Some("permission_prompt"), Some("elicitation_dialog")] {
            input.notification_type = kind.map(Into::into);
            assert!(
                matches!(
                    event_kind("Notification", &input),
                    Some(EventKind::NeedsInput(_))
                ),
                "{kind:?}"
            );
        }
        let auth = parsed("Notification", &raw_fixture("Notification-auth"));
        assert_eq!(auth.kind, None);
        assert!(status_for(&auth, 1000).is_none());
    }

    #[test]
    fn unknown_events_and_anonymous_sessions_say_nothing() {
        assert!(parsed("PostToolUse", &raw_fixture("PreToolUse"))
            .kind
            .is_none());
        assert!(status_for(&parsed("Stop", "{}"), 1).is_none());
        assert!(ClaudeCode.parse_hook("Stop", "not json").is_err());
    }

    #[test]
    fn settings_install_every_hook_and_the_jeet_permissions() {
        let hooks = HookSetup {
            exe: "/opt/it's here/jeet".into(),
            allow: vec!["jeet worktree".into(), "jeet ls".into()],
        };
        let args = ClaudeCode.hook_args(&hooks).unwrap();
        assert_eq!(args[0], "--settings");
        let value: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
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
    fn hook_output_blocks_a_stop_and_adds_context() {
        let value: serde_json::Value =
            serde_json::from_str(&ClaudeCode.continue_output("go on").unwrap()).unwrap();
        assert_eq!(value["decision"], "block");
        assert_eq!(value["reason"], "go on");
        let value: serde_json::Value =
            serde_json::from_str(&ClaudeCode.session_context_output("hello\nworld").unwrap())
                .unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            "hello\nworld"
        );
    }

    fn pane(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/panes/claude-code")
            .join(format!("{name}.txt"));
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_draft_in_the_input_box_is_seen_but_not_the_placeholder() {
        assert!(ClaudeCode.has_draft(&pane("input-draft")));
        assert!(ClaudeCode.has_draft(&pane("input-draft-lines")));
        // Named, as jeet starts it: the name is in the rule above the box,
        // and a message sent before shows above it.
        assert!(ClaudeCode.has_draft(&pane("input-draft-named")));
        // Dimmed: Claude's suggestion, not typed.
        assert!(!ClaudeCode.has_draft(&pane("input-placeholder")));
        // Empty after a turn; the message sent shows above it.
        assert!(!ClaudeCode.has_draft(&pane("input-empty")));
        // At the trust prompt there is no input box.
        assert!(!ClaudeCode.has_draft(&pane("trust-no-selected")));
        assert_eq!(undimmed("\x1b[39m❯ \x1b[2mTry this\x1b[0m!"), "❯ !");
        assert_eq!(undimmed("\x1b[1;2mdim\x1b[22mbold"), "bold");
    }

    #[test]
    fn the_trust_prompt_is_seen_and_accepted_one_step_at_a_time() {
        let no = pane("trust-no-selected");
        assert_eq!(ClaudeCode.startup_prompt(&no), Some(StartupPrompt::Trust));
        // Never Enter on "No, exit": that quits.
        assert_eq!(
            ClaudeCode.accept_trust(&no),
            Some(TrustStep::Select(vec!["Down"]))
        );
        let yes = pane("trust-yes-selected");
        assert_eq!(ClaudeCode.startup_prompt(&yes), Some(StartupPrompt::Trust));
        assert_eq!(
            ClaudeCode.accept_trust(&yes),
            Some(TrustStep::Confirm(vec!["Enter"]))
        );
    }

    #[test]
    fn no_prompt_means_no_keys() {
        let ready = pane("no-prompt");
        assert!(ClaudeCode.startup_prompt(&ready).is_none());
        assert!(ClaudeCode.accept_trust(&ready).is_none());
        assert!(ClaudeCode.accept_trust("").is_none());
    }

    #[test]
    fn a_wrapped_prompt_is_still_seen_but_an_unknown_selection_is_left_alone() {
        let narrow = pane("trust-no-selected").replace("created or one", "created or\n one");
        assert_eq!(
            ClaudeCode.startup_prompt(&narrow),
            Some(StartupPrompt::Trust)
        );
        let odd = pane("trust-no-selected").replace("❯ No, exit", "❯ Maybe");
        assert!(ClaudeCode.accept_trust(&odd).is_none());
    }

    #[test]
    fn slug_matches_claude_layout() {
        assert_eq!(
            project_slug(Path::new("/home/user/jeet")),
            "-home-user-jeet"
        );
        assert_eq!(
            project_slug(Path::new("/home/me/.jeet/worktrees/feat-x")),
            "-home-me--jeet-worktrees-feat-x"
        );
    }

    #[test]
    fn sessions_get_an_id_a_name_and_a_prompt() {
        assert_eq!(
            ClaudeCode.new_session_args("abc", "Fix the login").unwrap(),
            ["--session-id", "abc", "-n", "Fix the login"]
        );
        assert_eq!(ClaudeCode.resume_args("abc").unwrap(), ["--resume", "abc"]);
        assert_eq!(
            ClaudeCode.prompt_args("--help me").unwrap(),
            ["--", "--help me"]
        );
    }

    fn transcript(name: &str, n: usize) -> LastTurn {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transcripts/claude-code")
            .join(format!("{name}.jsonl"));
        last_turn(&path, n).unwrap()
    }

    fn at(time: &str) -> Option<i64> {
        crate::github::parse_timestamp(&format!("2026-10-10T22:{time}Z"))
    }

    fn texts(turn: &LastTurn) -> Vec<&str> {
        turn.messages.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn a_finished_turn_ends_on_its_whole_last_reply() {
        let turn = transcript("finished", 1);
        assert_eq!(turn.end, TurnEnd::Finished);
        assert_eq!(turn.at, at("01:02"));
        // The reply's text blocks are one message; a command typed at the
        // prompt afterwards starts no turn.
        assert_eq!(
            texts(&turn),
            ["The cookie had no `Max-Age`, so it died with the tab.\n\n\
              I set it to 30 days and pushed fix/login-cookie; tests pass."]
        );
        assert_eq!(turn.messages[0].at, at("01:00"));
    }

    #[test]
    fn the_last_n_replies_come_oldest_first_without_subagents() {
        let turn = transcript("finished", 5);
        assert_eq!(turn.messages.len(), 2);
        assert_eq!(texts(&turn)[0], "Looking at how the cookie is set.");
        assert!(!texts(&turn).iter().any(|t| t.contains("Subagent")));
    }

    #[test]
    fn an_api_error_is_the_ending_not_the_last_message() {
        let turn = transcript("api-error", 1);
        assert_eq!(
            turn.end,
            TurnEnd::ApiError(
                "Connection lost mid-response. The response above may be incomplete.".into()
            )
        );
        assert_eq!(turn.at, at("02:35"));
        assert_eq!(
            texts(&turn),
            ["Pushed fix/login-cookie; opening the PR next."]
        );
    }

    #[test]
    fn an_interrupted_turn_says_so_with_what_it_had_said() {
        let turn = transcript("interrupted", 1);
        assert_eq!(turn.end, TurnEnd::Interrupted);
        assert_eq!(turn.at, at("03:01"));
        assert_eq!(texts(&turn), ["Typing \"cl"]);
    }

    #[test]
    fn a_turn_mid_way_is_still_running() {
        let turn = transcript("mid-turn", 1);
        assert_eq!(turn.end, TurnEnd::Running);
        assert_eq!(turn.at, at("04:00"));
        assert_eq!(texts(&turn), ["Running the tests now."]);
    }

    #[test]
    fn a_missing_transcript_is_an_error() {
        assert!(last_turn(Path::new("/nonexistent/x.jsonl"), 1).is_err());
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
