//! The `slack` backend: a direct message from the user's own Slack app, in
//! Block Kit. `conversations.open` with `[notify.slack] user`, then
//! `chat.postMessage`, with the bot token only the sender reads.
//!
//! What it sends, each sent message's channel and `ts` (for replies, later)
//! and whether Slack takes `jeet://` links are kept beside the outbox.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config;
use crate::link;
use crate::watch;
use crate::worktrees::now_secs;

use super::web::{self, bad_token, Http, Ureq};
use super::{dir, open_owned, openable_url, Down, Event, Notifier, Said, Tokens};

/// Its name in `[notify] via`, and the keyring account's prefix.
pub const NAME: &str = "slack";

#[cfg_attr(test, allow(dead_code))]
pub const API: &str = "https://slack.com/api/";

/// Slack's limits: a header's text, a section's, a field's.
const HEADER_MAX: usize = 150;
const SECTION_MAX: usize = 3000;
const FIELD_MAX: usize = 2000;
/// A button's url.
const URL_MAX: usize = 3000;

/// Slack's errors that mean the token is no good.
const BAD_TOKEN: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "token_expired",
    "account_inactive",
    "not_allowed_token_type",
];

/// Slack itself, at [`API`].
#[cfg_attr(test, allow(dead_code))]
pub fn http() -> Ureq {
    Ureq {
        base: API.to_string(),
        service: "Slack",
    }
}

pub struct Slack {
    /// `[notify.slack] user`: the member id messages go to.
    pub user: Option<String>,
    pub home: PathBuf,
    pub http: Box<dyn Http>,
}

/// How a call to Slack failed.
enum Failed {
    /// Slack answered `ok: false` with this error.
    Slack(String),
    /// It did not answer, or not in JSON.
    Other(anyhow::Error),
}

impl Slack {
    fn call(&self, method: &str, token: &str, body: &Value) -> Result<Value, Failed> {
        let answer = self
            .http
            .call(method, Some(token), body)
            .map_err(Failed::Other)?;
        match answer["ok"].as_bool() {
            Some(true) => Ok(answer),
            _ => Err(Failed::Slack(
                answer["error"]
                    .as_str()
                    .unwrap_or("unknown_error")
                    .to_string(),
            )),
        }
    }

    fn failed(&self, method: &str, failed: Failed) -> anyhow::Error {
        match failed {
            Failed::Slack(e) if BAD_TOKEN.contains(&e.as_str()) => bad_token(
                &self.home,
                NAME,
                &format!(
                    "Slack refused the bot token ({e}): store a new one with `jeet notify setup slack`"
                ),
            ),
            Failed::Slack(e) => anyhow!("Slack's {method}: {e}"),
            // Not reached, or not answered: nor will the next one be.
            Failed::Other(e) => anyhow!(Down(e)),
        }
    }
}

impl Notifier for Slack {
    fn name(&self) -> &str {
        NAME
    }

    fn needs_token(&self) -> bool {
        true
    }

    fn send(&self, event: &Event, tokens: &dyn Tokens) -> Result<()> {
        let Some(user) = &self.user else {
            return Err(anyhow!(Down(anyhow!(
                "[notify.slack] user is not set: give it your Slack member id"
            ))));
        };
        let Some(token) = tokens.token(NAME)? else {
            return Err(bad_token(
                &self.home,
                NAME,
                "no Slack token is stored: run `jeet notify setup slack` at your terminal",
            ));
        };
        let opened = self
            .call("conversations.open", &token, &json!({ "users": user }))
            // The user, or the app, is wrong: so for every message.
            .map_err(|f| match self.failed("conversations.open", f) {
                e if e.is::<Said>() || e.is::<Down>() => e,
                e => anyhow!(Down(e)),
            })?;
        let Some(channel) = opened["channel"]["id"].as_str() else {
            bail!("Slack's conversations.open gave no channel");
        };
        let ci = ci_status(&self.home, event.pr.as_deref());
        let now = now_secs();
        let mut links = web::load(&self.home, NAME).jeet_links.unwrap_or(true);
        let post = |links: bool| {
            let mut message = message(event, ci.as_deref(), now, links);
            message["channel"] = json!(channel);
            self.call("chat.postMessage", &token, &message)
        };
        let posted = match post(links) {
            // Slack may not take a `jeet://` link: once more without, and
            // if that goes, never with one again.
            Err(Failed::Slack(e)) if links && has_jeet_link(event) && refused_blocks(&e) => {
                links = false;
                let posted = post(false);
                if posted.is_ok() {
                    eprintln!(
                        "jeet: Slack refused a jeet:// link ({e}); messages go without one from now on"
                    );
                    web::refuse_jeet_links(&self.home, NAME);
                }
                posted
            }
            posted => posted,
        }
        .map_err(|f| self.failed("chat.postMessage", f))?;
        web::token_works(&self.home, NAME);
        let sent = Sent {
            at: now,
            why: event.why.name().to_string(),
            kind: event.kind.clone(),
            task: event.task.clone(),
            reference: event.reference.clone(),
            channel: posted["channel"].as_str().unwrap_or(channel).to_string(),
            ts: posted["ts"].as_str().unwrap_or_default().to_string(),
            jeet_link: links && has_jeet_link(event),
        };
        if let Err(e) = record(&self.home, &sent) {
            eprintln!("jeet: could not keep where Slack put a notification: {e:#}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The message
// ---------------------------------------------------------------------------

/// `event` as `chat.postMessage`'s `text` and `blocks`; `jeet://` links as
/// links only if `links`.
pub fn message(event: &Event, ci: Option<&str>, now: i64, links: bool) -> Value {
    let mut blocks = vec![json!({
        "type": "header",
        "text": { "type": "plain_text", "text": cut(&header(event), HEADER_MAX) },
    })];
    let summary = match event.body.trim() {
        "" => event.title.trim(),
        body => body,
    };
    if !summary.is_empty() {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": cut(&escape(summary), SECTION_MAX) },
        }));
    }
    let mut fields = Vec::new();
    let mut field = |name: &str, value: &str| {
        fields.push(json!({
            "type": "mrkdwn",
            "text": cut(&format!("*{name}*\n{}", escape(value)), FIELD_MAX),
        }))
    };
    if let Some(task) = &event.task {
        field("Task", task);
    }
    if let Some(at) = event.subtitle.as_deref().filter(|s| !s.trim().is_empty()) {
        field("Where", at);
    }
    if !event.kind.is_empty() {
        field("Kind", &event.kind);
    }
    if let Some(ci) = ci {
        field("CI", ci);
    }
    if let Some(at) = event.at {
        field("Age", &age(now - at));
    }
    if !fields.is_empty() {
        blocks.push(json!({ "type": "section", "fields": fields }));
    }
    let jeet = jeet_link(event);
    let mut buttons = Vec::new();
    let mut button = |id: &str, text: &str, url: &str| {
        if url.len() > URL_MAX {
            return;
        }
        buttons.push(json!({
            "type": "button",
            "action_id": id,
            "text": { "type": "plain_text", "text": text },
            "url": url,
        }))
    };
    let pr = event.pr.as_deref().filter(|u| openable_url(u));
    if let Some(pr) = pr {
        button("pr", "Pull request", pr);
        if let Some(checks) = checks_url(pr) {
            button("checks", "Checks", &checks);
        }
    }
    // A web page the event links to, unless it is the PR.
    if let Some(page) = event
        .link
        .as_deref()
        .filter(|u| openable_url(u) && Some(*u) != pr)
    {
        button("open", "Open", page);
    }
    if let (Some(jeet), true) = (&jeet, links) {
        button("jeet", "Open in jeet", jeet);
    }
    if !buttons.is_empty() {
        blocks.push(json!({ "type": "actions", "elements": buttons }));
    }
    let mut context = Vec::new();
    if let Some(jeet) = &jeet {
        context.push(match links {
            true => format!("<{jeet}|Open in jeet> · `jeet open {jeet}`"),
            false => format!("`jeet open {jeet}`"),
        });
    }
    if let Some(act) = act(event) {
        context.push(format!("`{act}`"));
    }
    if !context.is_empty() {
        blocks.push(json!({
            "type": "context",
            "elements": context
                .iter()
                .map(|text| json!({ "type": "mrkdwn", "text": text }))
                .collect::<Vec<_>>(),
        }));
    }
    json!({ "text": cut(&escape(event.title.trim()), SECTION_MAX), "blocks": blocks })
}

/// Whether Slack's `error` says it would not take the blocks as they were.
fn refused_blocks(error: &str) -> bool {
    error.starts_with("invalid_blocks")
}

/// The report id of `event`, if it is about one (`ref` = `r41`).
fn report_id(event: &Event) -> Option<i64> {
    let id = event.reference.as_deref()?.strip_prefix('r')?;
    // `r41/user`: the same report, escalated to the user.
    id.split('/').next()?.parse().ok()
}

/// "Question #41 from task x", or the title.
pub(super) fn header(event: &Event) -> String {
    let from = event.task.as_deref().map(|slug| {
        let what = match event.level {
            Some(config::NotifyLevel::Manager) => "manager",
            Some(config::NotifyLevel::Review) => "review",
            Some(config::NotifyLevel::Assistant) => "the assistant",
            _ => "task",
        };
        match event.level {
            Some(config::NotifyLevel::Assistant) => what.to_string(),
            _ => format!("{what} {slug}"),
        }
    });
    let title = event.title.trim();
    let title = title.strip_prefix("jeet: ").unwrap_or(title);
    match (event.kind.as_str(), report_id(event), from) {
        ("question", Some(id), Some(from)) => format!("Question #{id} from {from}"),
        ("blocked", Some(id), Some(from)) => format!("Blocked #{id}: {from}"),
        ("done", Some(id), Some(from)) => format!("Done #{id}: {from}"),
        _ if !title.is_empty() => title.to_string(),
        _ => "jeet".to_string(),
    }
}

/// The command that acts on `event`, where there is one.
pub(super) fn act(event: &Event) -> Option<String> {
    let id = report_id(event)?;
    Some(match event.kind.as_str() {
        "question" | "blocked" => format!("jeet report answer {id} \"…\""),
        _ => format!("jeet show r{id}"),
    })
}

/// The event's `jeet://` link, as jeet writes it, if it has a good one.
pub(super) fn jeet_link(event: &Event) -> Option<String> {
    let parsed = link::parse(event.link.as_deref()?).ok()?;
    let url = link::link_for(&parsed);
    // Kept out of mrkdwn's `<url|text>` and a code span whatever a slug holds.
    let plain = !url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "|<>`&".contains(c));
    plain.then_some(url)
}

pub(super) fn has_jeet_link(event: &Event) -> bool {
    jeet_link(event).is_some()
}

/// A GitHub PR's checks page.
fn checks_url(pr: &str) -> Option<String> {
    let rest = pr.strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [_, _, "pull", n] if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
            Some(format!("{pr}/checks"))
        }
        _ => None,
    }
}

/// `secs` as "3 min", "2 h"...
pub(super) fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86400),
    }
}

/// What `jeet watch` last saw of `pr`'s checks, if it watches it.
pub(super) fn ci_status(home: &Path, pr: Option<&str>) -> Option<String> {
    let pr = pr?;
    let path = config::watch_state_path(home);
    if !path.exists() {
        return None;
    }
    let state = watch::load_state(&path);
    let check = state.checks?.into_values().find(|c| c.url == pr)?;
    Some(match check.state.as_str() {
        "SUCCESS" => "passing".to_string(),
        "FAILURE" | "ERROR" if check.failures > 1 => {
            format!("failing ({} in a row)", check.failures)
        }
        "FAILURE" | "ERROR" => "failing".to_string(),
        "PENDING" | "EXPECTED" => "running".to_string(),
        "" => return None,
        other => other.to_ascii_lowercase(),
    })
}

/// Mrkdwn's three escapes.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `text` cut to `max` characters.
pub(super) fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => {
            let kept: String = text[..at].chars().take(max - 1).collect();
            format!("{kept}…")
        }
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// What is kept
// ---------------------------------------------------------------------------

/// One notification Slack took: where it is, to match replies to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub at: i64,
    pub why: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub channel: String,
    pub ts: String,
    /// Whether it went with its `jeet://` link.
    #[serde(default)]
    pub jeet_link: bool,
}

/// `$JEET_HOME/run/notify/slack-sent.jsonl`: one [`Sent`] a line.
pub fn sent_path(home: &Path) -> PathBuf {
    dir(home).join("slack-sent.jsonl")
}

fn record(home: &Path, sent: &Sent) -> Result<()> {
    let mut line = serde_json::to_string(sent)?;
    line.push('\n');
    let mut file = open_owned(
        &sent_path(home),
        OpenOptions::new().create(true).append(true),
    )?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// What is kept of each notification sent, oldest first.
#[cfg(test)]
pub fn sent(home: &Path) -> Vec<Sent> {
    std::fs::read_to_string(sent_path(home))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// Setting it up
// ---------------------------------------------------------------------------

/// The Slack app's manifest: a bot that may write to the user's DM and
/// nothing else; no events, no Socket Mode.
pub fn manifest() -> String {
    let manifest = json!({
        "display_information": {
            "name": "jeet",
            "description": "Notifications from jeet",
        },
        "features": {
            "bot_user": { "display_name": "jeet", "always_online": false },
            "app_home": {
                "home_tab_enabled": false,
                "messages_tab_enabled": true,
                "messages_tab_read_only_enabled": true,
            },
        },
        "oauth_config": { "scopes": { "bot": ["chat:write", "im:write"] } },
        "settings": {
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_default()
}

/// Whether `user` looks like a Slack member id (`U…` or `W…`).
pub fn member_id(user: &str) -> bool {
    let mut chars = user.chars();
    matches!(chars.next(), Some('U' | 'W'))
        && user.len() >= 5
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Whether `token` looks like a bot token.
pub fn bot_token(token: &str) -> bool {
    token.starts_with("xoxb-") && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyLevel;
    use crate::notify::secrets::{account, Keyring, KeyringTokens, MemKeyring};
    use crate::notify::{make_dir, Why};
    use std::cell::RefCell;
    use std::rc::Rc;

    type Calls = Rc<RefCell<Vec<(String, String, Value)>>>;
    type Answer = Box<dyn Fn(&str, &Value) -> Result<Value>>;

    /// Slack, as a script: each call is noted, and answered by `answer`.
    struct Fake {
        calls: Calls,
        answer: Answer,
    }

    impl Http for Fake {
        fn call(&self, method: &str, token: Option<&str>, body: &Value) -> Result<Value> {
            self.calls.borrow_mut().push((
                method.into(),
                token.unwrap_or_default().into(),
                body.clone(),
            ));
            (self.answer)(method, body)
        }
    }

    fn slack(
        home: &Path,
        answer: impl Fn(&str, &Value) -> Result<Value> + 'static,
    ) -> (Slack, Calls) {
        make_dir(home).unwrap();
        let calls = Calls::default();
        let slack = Slack {
            user: Some("U012AB3CD".into()),
            home: home.to_path_buf(),
            http: Box::new(Fake {
                calls: calls.clone(),
                answer: Box::new(answer),
            }),
        };
        (slack, calls)
    }

    fn ok(method: &str, _: &Value) -> Result<Value> {
        Ok(match method {
            "conversations.open" => json!({ "ok": true, "channel": { "id": "D1" } }),
            _ => json!({ "ok": true, "channel": "D1", "ts": "1700000000.000100" }),
        })
    }

    fn tokens(home: &Path) -> KeyringTokens {
        let keyring = MemKeyring::default();
        keyring.set(&account(NAME, home), "xoxb-1-abc").unwrap();
        KeyringTokens::new(Box::new(keyring), home)
    }

    fn question() -> Event {
        Event {
            why: Why::NeedsUser,
            kind: "question".into(),
            level: Some(NotifyLevel::Task),
            task: Some("x".into()),
            reference: Some("r41".into()),
            title: "jeet: task x has a question (#41)".into(),
            subtitle: Some("assistant/x".into()),
            body: "Ship it <now> & then?".into(),
            link: Some("jeet://report/41".into()),
            pr: Some("https://github.com/acme/api/pull/7".into()),
            at: Some(1000),
            ..Event::default()
        }
    }

    fn texts(message: &Value) -> String {
        message.to_string()
    }

    #[test]
    fn a_question_is_a_dm_with_its_header_fields_buttons_and_command() {
        let home = tempfile::tempdir().unwrap();
        let (slack, calls) = slack(home.path(), ok);
        slack.send(&question(), &tokens(home.path())).unwrap();
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "conversations.open");
        assert_eq!(calls[0].1, "xoxb-1-abc");
        assert_eq!(calls[0].2, json!({ "users": "U012AB3CD" }));
        let (method, _, posted) = &calls[1];
        assert_eq!(method, "chat.postMessage");
        assert_eq!(posted["channel"], "D1");
        assert_eq!(posted["text"], "jeet: task x has a question (#41)");
        let blocks = posted["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["text"]["text"], "Question #41 from task x");
        assert_eq!(blocks[1]["text"]["text"], "Ship it &lt;now&gt; &amp; then?");
        let fields = texts(&blocks[2]);
        for want in [
            "*Task*\\nx",
            "*Kind*\\nquestion",
            "*Age*\\n",
            "*Where*\\nassistant/x",
        ] {
            assert!(fields.contains(want), "{want} in {fields}");
        }
        let buttons = &blocks[3]["elements"];
        assert_eq!(buttons[0]["url"], "https://github.com/acme/api/pull/7");
        assert_eq!(
            buttons[1]["url"],
            "https://github.com/acme/api/pull/7/checks"
        );
        assert_eq!(buttons[2]["url"], "jeet://report/41");
        let context = texts(&blocks[4]);
        assert!(
            context.contains("<jeet://report/41|Open in jeet>"),
            "{context}"
        );
        assert!(
            context.contains("`jeet open jeet://report/41`"),
            "{context}"
        );
        assert!(
            context.contains("`jeet report answer 41 \\\"…\\\"`"),
            "{context}"
        );

        let sent = sent(home.path());
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel, "D1");
        assert_eq!(sent[0].ts, "1700000000.000100");
        assert_eq!(sent[0].reference.as_deref(), Some("r41"));
        assert!(sent[0].jeet_link);
    }

    #[test]
    fn a_refused_jeet_link_is_resent_without_and_never_tried_again() {
        let home = tempfile::tempdir().unwrap();
        let (slack, calls) = slack(home.path(), |method, body| {
            if method == "chat.postMessage" && body.to_string().contains("<jeet://") {
                return Ok(json!({ "ok": false, "error": "invalid_blocks" }));
            }
            ok(method, body)
        });
        let tokens = tokens(home.path());
        slack.send(&question(), &tokens).unwrap();
        {
            let calls = calls.borrow();
            let posts: Vec<_> = calls.iter().filter(|c| c.0 == "chat.postMessage").collect();
            assert_eq!(posts.len(), 2);
            let resent = texts(&posts[1].2);
            assert!(!resent.contains("<jeet://"), "{resent}");
            assert!(!resent.contains("\"url\":\"jeet://"), "{resent}");
            assert!(resent.contains("`jeet open jeet://report/41`"), "{resent}");
        }
        assert_eq!(web::load(home.path(), NAME).jeet_links, Some(false));
        assert!(!sent(home.path())[0].jeet_link);

        calls.borrow_mut().clear();
        slack.send(&question(), &tokens).unwrap();
        let calls = calls.borrow();
        assert_eq!(
            calls.iter().filter(|c| c.0 == "chat.postMessage").count(),
            1
        );
    }

    #[test]
    fn slack_errors_and_a_missing_token_fail_the_send() {
        let home = tempfile::tempdir().unwrap();
        let (slack, _) = slack(home.path(), |method, _| match method {
            "conversations.open" => Ok(json!({ "ok": false, "error": "user_not_found" })),
            _ => unreachable!(),
        });
        let e = slack.send(&question(), &tokens(home.path())).unwrap_err();
        assert!(format!("{e:#}").contains("user_not_found"), "{e:#}");
        // The user is wrong for every message: the backend is down.
        assert!(e.is::<Down>());

        let (slack, calls) = slack_with_http_error(home.path());
        let e = slack.send(&question(), &tokens(home.path())).unwrap_err();
        assert!(format!("{e:#}").contains("HTTP 503"), "{e:#}");
        assert!(e.is::<Down>());
        assert_eq!(calls.borrow().len(), 1);

        // What Slack says of one message is that message's alone.
        let (slack, _) = super::tests::slack(home.path(), |method, body| match method {
            "chat.postMessage" => Ok(json!({ "ok": false, "error": "msg_too_long" })),
            _ => ok(method, body),
        });
        let e = slack.send(&question(), &tokens(home.path())).unwrap_err();
        assert!(format!("{e:#}").contains("msg_too_long"), "{e:#}");
        assert!(!e.is::<Down>());

        // Without a token, nothing is called.
        let (slack, calls) = super::tests::slack(home.path(), ok);
        let none = KeyringTokens::new(Box::new(MemKeyring::default()), home.path());
        assert!(slack.send(&question(), &none).is_err());
        assert!(calls.borrow().is_empty());
        assert!(sent(home.path()).is_empty());
    }

    fn slack_with_http_error(home: &Path) -> (Slack, Calls) {
        slack(home, |_, _| bail!("Slack's conversations.open: HTTP 503"))
    }

    #[test]
    fn a_bad_token_is_said_once_until_a_send_goes_through() {
        let home = tempfile::tempdir().unwrap();
        let (slack, _) = slack(home.path(), |method, body| match method {
            "conversations.open" => Ok(json!({ "ok": false, "error": "invalid_auth" })),
            _ => ok(method, body),
        });
        let tokens = tokens(home.path());
        let first = slack.send(&question(), &tokens).unwrap_err();
        assert!(first.downcast_ref::<Said>().is_none());
        assert!(
            format!("{first:#}").contains("jeet notify setup slack"),
            "{first:#}"
        );
        let again = slack.send(&question(), &tokens).unwrap_err();
        assert!(again.downcast_ref::<Said>().is_some());
        let (good, _) = super::tests::slack(home.path(), ok);
        good.send(&question(), &tokens).unwrap();
        let after = slack.send(&question(), &tokens).unwrap_err();
        assert!(after.downcast_ref::<Said>().is_none());
    }

    #[test]
    fn a_plain_event_has_its_title_and_no_command() {
        let event = Event {
            kind: "message".into(),
            title: "Deployed".into(),
            body: "all good".into(),
            link: Some("https://example.com/x".into()),
            ..Event::default()
        };
        let m = message(&event, None, 0, true);
        let blocks = m["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["text"]["text"], "Deployed");
        assert_eq!(blocks[1]["text"]["text"], "all good");
        let all = texts(&m);
        assert!(!all.contains("jeet open"), "{all}");
        assert!(all.contains("\"url\":\"https://example.com/x\""), "{all}");
        assert!(!all.contains("*Age*"), "{all}");
    }

    #[test]
    fn helpers() {
        assert_eq!(
            checks_url("https://github.com/a/b/pull/12").as_deref(),
            Some("https://github.com/a/b/pull/12/checks")
        );
        assert_eq!(checks_url("https://github.com/a/b/issues/12"), None);
        assert_eq!(checks_url("https://gitlab.com/a/b/pull/12"), None);
        assert_eq!(age(30), "just now");
        assert_eq!(age(150), "2 min");
        assert_eq!(age(7300), "2 h");
        assert_eq!(cut("abcdef", 4), "abc…");
        assert!(member_id("U012AB3CD"));
        assert!(!member_id("u012"));
        assert!(!member_id("U01 2AB"));
        assert!(bot_token("xoxb-123-456-abc"));
        assert!(!bot_token("xapp-1"));
        assert!(!bot_token("xoxb-1 2"));
        let m: Value = serde_json::from_str(&manifest()).unwrap();
        assert_eq!(
            m["oauth_config"]["scopes"]["bot"],
            json!(["chat:write", "im:write"])
        );
        assert_eq!(m["settings"]["socket_mode_enabled"], false);
        assert!(m["settings"].get("event_subscriptions").is_none());
        // A link jeet would not write is not used.
        let mut e = question();
        e.link = Some("jeet://report/41|x".into());
        assert_eq!(jeet_link(&e), None);
    }

    #[test]
    fn ci_comes_from_what_watch_last_saw() {
        let home = tempfile::tempdir().unwrap();
        let pr = "https://github.com/acme/api/pull/7";
        assert_eq!(ci_status(home.path(), Some(pr)), None);
        let state = json!({ "checks": { "acme/api#7": {
            "repo": "acme/api", "number": 7, "title": "t", "url": pr,
            "sha": "abc", "state": "FAILURE", "failures": 2
        } } });
        std::fs::write(config::watch_state_path(home.path()), state.to_string()).unwrap();
        assert_eq!(
            ci_status(home.path(), Some(pr)).as_deref(),
            Some("failing (2 in a row)")
        );
        assert_eq!(
            ci_status(home.path(), Some("https://github.com/x/y/pull/1")),
            None
        );
    }
}
