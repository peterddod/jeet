//! The `ntfy` backend: a message published to `[notify.ntfy] topic` on
//! `[notify.ntfy] server`, through ntfy's JSON publish API (a POST to the
//! server's root), with the access token, if one is stored, that only the
//! sender reads.
//!
//! Whether the server takes `jeet://` links is kept beside the outbox.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::worktrees::now_secs;

use super::slack::{act, age, ci_status, cut, has_jeet_link, header, jeet_link};
use super::web::{self, bad_token, Http, Status, Ureq};
use super::{openable_url, Down, Event, Notifier, Tokens, Why};

/// Its name in `[notify] via`, and the keyring account's prefix.
pub const NAME: &str = "ntfy";

/// The longest title sent.
const TITLE_MAX: usize = 150;

/// ntfy's message limit, in bytes: a longer one arrives as an attachment.
const MESSAGE_MAX: usize = 4096;

/// The longest field value (`Where`, `Task`…), in characters.
const FIELD_MAX: usize = 200;

/// ntfy's priorities.
const HIGH: u8 = 4;
const DEFAULT: u8 = 3;
const LOW: u8 = 2;

/// `server` itself, through `ureq`.
#[cfg_attr(test, allow(dead_code))]
pub fn http(server: &str) -> Ureq {
    Ureq {
        base: format!("{server}/"),
        service: "ntfy",
    }
}

pub struct Ntfy {
    /// `[notify.ntfy] topic`.
    pub topic: Option<String>,
    pub home: PathBuf,
    /// The server, at `[notify.ntfy] server`.
    pub http: Box<dyn Http>,
}

impl Notifier for Ntfy {
    fn name(&self) -> &str {
        NAME
    }

    /// The token is optional, but only the sender may look for one: a
    /// process that cannot tell whether one is stored leaves ntfy to it.
    fn needs_token(&self) -> bool {
        true
    }

    fn send(&self, event: &Event, tokens: &dyn Tokens) -> Result<()> {
        let Some(topic) = &self.topic else {
            return Err(anyhow!(Down(anyhow!(
                "[notify.ntfy] topic is not set: run `jeet notify setup ntfy`"
            ))));
        };
        let token = tokens.token(NAME)?;
        let ci = ci_status(&self.home, event.pr.as_deref());
        let now = now_secs();
        let links = web::load(&self.home, NAME).jeet_links.unwrap_or(true);
        let publish = |links: bool| {
            self.http.call(
                "",
                token.as_deref(),
                &message(topic, event, ci.as_deref(), now, links),
            )
        };
        let published = match publish(links) {
            // The server may not take a `jeet://` link: once more without,
            // and if that goes, never with one again.
            Err(e) if links && has_jeet_link(event) && status(&e) == Some(400) => {
                let published = publish(false);
                if published.is_ok() {
                    eprintln!(
                        "jeet: ntfy refused a jeet:// link ({e:#}); messages go without one from now on"
                    );
                    web::refuse_jeet_links(&self.home, NAME);
                }
                published
            }
            published => published,
        };
        if let Err(e) = published {
            return Err(match status(&e) {
                Some(401 | 403) => bad_token(
                    &self.home,
                    NAME,
                    &match token {
                        Some(_) => format!(
                            "ntfy refused the access token ({e:#}): store a new one with `jeet notify setup ntfy`"
                        ),
                        None => format!(
                            "ntfy refused to publish to {topic} without a token ({e:#}): store one with `jeet notify setup ntfy`"
                        ),
                    },
                ),
                // Not there, not in time, rate limited or failing, or not
                // reached at all: so for the next one.
                None | Some(404 | 405 | 408 | 429) => anyhow!(Down(e)),
                Some(code) if code >= 500 => anyhow!(Down(e)),
                // Of this message alone.
                Some(_) => e,
            });
        }
        web::token_works(&self.home, NAME);
        Ok(())
    }
}

/// The HTTP status a failed call was answered with, if it was.
fn status(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<Status>().map(|s| s.code)
}

/// `event` as ntfy's JSON publish body, for `topic`; `jeet://` links as
/// links only if `links`.
pub fn message(topic: &str, event: &Event, ci: Option<&str>, now: i64, links: bool) -> Value {
    let summary = match event.body.trim() {
        "" => event.title.trim(),
        body => body,
    };
    let mut fields = Vec::new();
    if let Some(task) = &event.task {
        fields.push(format!("Task: {}", cut(task, FIELD_MAX)));
    }
    if let Some(at) = event.subtitle.as_deref().filter(|s| !s.trim().is_empty()) {
        fields.push(format!("Where: {}", cut(at.trim(), FIELD_MAX)));
    }
    if !event.kind.is_empty() {
        fields.push(format!("Kind: {}", cut(&event.kind, FIELD_MAX)));
    }
    if let Some(ci) = ci {
        fields.push(format!("CI: {ci}"));
    }
    if let Some(at) = event.at {
        fields.push(format!("Age: {}", age(now - at)));
    }
    let jeet = jeet_link(event);
    let mut tail = Vec::new();
    if !fields.is_empty() {
        tail.push(fields.join(" · "));
    }
    if let Some(act) = act(event) {
        tail.push(act);
    }
    if let Some(jeet) = &jeet {
        tail.push(format!("jeet open {jeet}"));
    }
    let tail = tail.join("\n");
    // The summary gives way, so the command and the link are kept.
    let room = MESSAGE_MAX.saturating_sub(tail.len() + 2);
    let mut text = cut_bytes(summary, room);
    if !tail.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&tail);
    }
    if text.is_empty() {
        text = "jeet".to_string();
    }

    let pr = event.pr.as_deref().filter(|u| openable_url(u));
    // A web page the event links to, unless it is the PR.
    let page = event
        .link
        .as_deref()
        .filter(|u| openable_url(u) && Some(*u) != pr);
    let jeet = jeet.filter(|_| links);
    let mut actions = Vec::new();
    let mut view = |label: &str, url: &str| {
        actions.push(json!({ "action": "view", "label": label, "url": url }))
    };
    if let Some(pr) = pr {
        view("Pull request", pr);
    }
    if let Some(page) = page {
        view("Open", page);
    }
    if let Some(jeet) = &jeet {
        view("Open in jeet", jeet);
    }

    let mut message = json!({
        "topic": topic,
        "title": cut(&header(event), TITLE_MAX),
        "message": cut_bytes(&text, MESSAGE_MAX),
        "priority": priority(event.why),
        "tags": tags(event),
    });
    if let Some(click) = pr.or(jeet.as_deref()).or(page) {
        message["click"] = json!(click);
    }
    if !actions.is_empty() {
        message["actions"] = json!(actions);
    }
    message
}

/// needs-user and stuck high, finished as ntfy's default, the rest low.
fn priority(why: Why) -> u8 {
    match why {
        Why::NeedsUser | Why::Stuck => HIGH,
        Why::Finished => DEFAULT,
        Why::Info => LOW,
    }
}

/// An emoji for why (ntfy shows a tag that names one as it), then the kind.
fn tags(event: &Event) -> Vec<String> {
    let emoji = match event.why {
        Why::NeedsUser => "question",
        Why::Stuck => "warning",
        Why::Finished => "white_check_mark",
        Why::Info => "information_source",
    };
    let mut tags = vec![emoji.to_string()];
    let kind = event.kind.trim();
    if !kind.is_empty() && kind != emoji {
        tags.push(kind.to_string());
    }
    tags
}

/// `text` cut to at most `max` bytes, on a character's edge.
fn cut_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let ellipsis = "…";
    if max < ellipsis.len() {
        return String::new();
    }
    let mut end = max - ellipsis.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ellipsis}", &text[..end])
}

/// Whether `topic` is one ntfy takes: 1 to 64 of letters, digits, `-`, `_`.
pub fn valid_topic(topic: &str) -> bool {
    (1..=64).contains(&topic.len())
        && topic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A topic hard to guess, for a public server.
pub fn random_topic() -> String {
    format!("jeet-{}", uuid::Uuid::new_v4().simple())
}

/// Whether `token` could be an access token: printable, no spaces.
pub fn plausible_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyLevel;
    use crate::notify::secrets::{account, Keyring, KeyringTokens, MemKeyring};
    use crate::notify::web::tests::serve_once;
    use crate::notify::{make_dir, Said};
    use std::cell::RefCell;
    use std::rc::Rc;

    type Calls = Rc<RefCell<Vec<(Option<String>, Value)>>>;
    type Answer = Box<dyn Fn(&Value) -> Result<Value>>;

    /// A server, as a script: each publish is noted, and answered by
    /// `answer`.
    struct Fake {
        calls: Calls,
        answer: Answer,
    }

    impl Http for Fake {
        fn call(&self, path: &str, token: Option<&str>, body: &Value) -> Result<Value> {
            assert_eq!(path, "");
            self.calls
                .borrow_mut()
                .push((token.map(str::to_string), body.clone()));
            (self.answer)(body)
        }
    }

    fn ntfy(
        home: &std::path::Path,
        answer: impl Fn(&Value) -> Result<Value> + 'static,
    ) -> (Ntfy, Calls) {
        make_dir(home).unwrap();
        let calls = Calls::default();
        let ntfy = Ntfy {
            topic: Some("jeet-abc".into()),
            home: home.to_path_buf(),
            http: Box::new(Fake {
                calls: calls.clone(),
                answer: Box::new(answer),
            }),
        };
        (ntfy, calls)
    }

    fn ok(_: &Value) -> Result<Value> {
        Ok(json!({ "id": "x", "event": "message" }))
    }

    fn refused(code: u16) -> anyhow::Error {
        anyhow!(Status {
            code,
            error: Some("no".into()),
            retry_after: None,
        })
        .context("ntfy")
    }

    fn tokens(home: &std::path::Path, token: Option<&str>) -> KeyringTokens {
        let keyring = MemKeyring::default();
        if let Some(token) = token {
            keyring.set(&account(NAME, home), token).unwrap();
        }
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

    #[test]
    fn a_question_is_high_with_its_pr_to_click_and_buttons() {
        let home = tempfile::tempdir().unwrap();
        let (ntfy, calls) = ntfy(home.path(), ok);
        ntfy.send(&question(), &tokens(home.path(), Some("tk_abc")))
            .unwrap();
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        let (token, m) = &calls[0];
        assert_eq!(token.as_deref(), Some("tk_abc"));
        assert_eq!(m["topic"], "jeet-abc");
        assert_eq!(m["title"], "Question #41 from task x");
        assert_eq!(m["priority"], 4);
        assert_eq!(m["tags"], json!(["question"]));
        assert_eq!(m["click"], "https://github.com/acme/api/pull/7");
        assert_eq!(
            m["actions"],
            json!([
                { "action": "view", "label": "Pull request", "url": "https://github.com/acme/api/pull/7" },
                { "action": "view", "label": "Open in jeet", "url": "jeet://report/41" },
            ])
        );
        let text = m["message"].as_str().unwrap();
        assert!(text.starts_with("Ship it <now> & then?\n\n"), "{text}");
        assert!(
            text.contains("Task: x · Where: assistant/x · Kind: question · Age: "),
            "{text}"
        );
        assert!(text.contains("\njeet report answer 41 \"…\"\n"), "{text}");
        assert!(text.ends_with("\njeet open jeet://report/41"), "{text}");
    }

    #[test]
    fn without_a_token_it_publishes_with_none() {
        let home = tempfile::tempdir().unwrap();
        let (ntfy, calls) = ntfy(home.path(), ok);
        ntfy.send(&question(), &tokens(home.path(), None)).unwrap();
        assert_eq!(calls.borrow()[0].0, None);
    }

    #[test]
    fn priorities_and_the_click_fall_back_to_the_jeet_link() {
        let mut event = question();
        event.pr = None;
        event.why = Why::Finished;
        event.kind = "done".into();
        let m = message("t", &event, None, 1000, true);
        assert_eq!(m["priority"], 3);
        assert_eq!(m["tags"], json!(["white_check_mark", "done"]));
        assert_eq!(m["click"], "jeet://report/41");
        assert_eq!(m["actions"].as_array().unwrap().len(), 1);
        // Without jeet links: the command stays, no click, no button.
        let m = message("t", &event, None, 1000, false);
        assert!(m.get("click").is_none(), "{m}");
        assert!(m.get("actions").is_none(), "{m}");
        assert!(m["message"]
            .as_str()
            .unwrap()
            .contains("jeet open jeet://report/41"));

        event.why = Why::Info;
        event.link = Some("https://example.com/x".into());
        let m = message("t", &event, None, 1000, true);
        assert_eq!(m["priority"], 2);
        assert_eq!(m["click"], "https://example.com/x");
        assert_eq!(m["actions"][0]["url"], "https://example.com/x");
        event.why = Why::Stuck;
        assert_eq!(message("t", &event, None, 0, true)["priority"], 4);
    }

    #[test]
    fn a_long_message_stays_under_ntfys_limit() {
        let mut event = question();
        event.body = "é".repeat(5000);
        event.subtitle = Some("ü".repeat(5000));
        let m = message("t", &event, None, 0, true);
        let text = m["message"].as_str().unwrap();
        assert!(text.len() <= MESSAGE_MAX, "{}", text.len());
        // The summary gives way; the command and the link stay.
        assert!(text.starts_with("éé"), "{text}");
        assert!(text.contains("é…\n\nTask: x · Where: üü"), "{text}");
        assert!(text.contains("\njeet report answer 41 \"…\"\n"), "{text}");
        assert!(text.ends_with("\njeet open jeet://report/41"), "{text}");
        assert_eq!(cut_bytes("abc", 3), "abc");
        assert_eq!(cut_bytes("ab€d", 5), "ab…");
        assert_eq!(cut_bytes("ab€d", 4), "a…");
        assert_eq!(cut_bytes("ab€d", 2), "");
    }

    #[test]
    fn a_refused_jeet_link_is_resent_without_and_never_tried_again() {
        let home = tempfile::tempdir().unwrap();
        let mut event = question();
        event.pr = None;
        let (ntfy, calls) = ntfy(home.path(), |m| match m.get("actions") {
            Some(_) => Err(refused(400)),
            None => ok(m),
        });
        let tokens = tokens(home.path(), None);
        ntfy.send(&event, &tokens).unwrap();
        assert_eq!(calls.borrow().len(), 2);
        assert!(calls.borrow()[1].1.get("click").is_none());
        assert_eq!(web::load(home.path(), NAME).jeet_links, Some(false));
        calls.borrow_mut().clear();
        ntfy.send(&event, &tokens).unwrap();
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn a_refusal_is_the_messages_or_the_backends() {
        let home = tempfile::tempdir().unwrap();
        let tokens = tokens(home.path(), Some("tk_abc"));
        // A bad message: its own.
        let (bad, _) = ntfy(home.path(), |_| Err(refused(413)));
        let e = bad.send(&question(), &tokens).unwrap_err();
        assert!(!e.is::<Down>(), "{e:#}");
        assert!(format!("{e:#}").contains("HTTP 413"), "{e:#}");
        // Rate limited, failing, or not reached: down for the round.
        for code in [404, 405, 408, 429, 502] {
            let (down, _) = ntfy(home.path(), move |_| Err(refused(code)));
            assert!(down.send(&question(), &tokens).unwrap_err().is::<Down>());
        }
        let (gone, _) = ntfy(home.path(), |_| Err(anyhow!("connection refused")));
        assert!(gone.send(&question(), &tokens).unwrap_err().is::<Down>());
        // No topic: down, and nothing is called.
        let (mut none, calls) = ntfy(home.path(), ok);
        none.topic = None;
        assert!(none.send(&question(), &tokens).unwrap_err().is::<Down>());
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_refused_token_is_said_once_until_a_send_goes_through() {
        let home = tempfile::tempdir().unwrap();
        let tokens = tokens(home.path(), Some("tk_abc"));
        let (ntfy_401, _) = ntfy(home.path(), |_| Err(refused(401)));
        let first = ntfy_401.send(&question(), &tokens).unwrap_err();
        assert!(first.is::<Down>());
        assert!(
            format!("{first:#}").contains("jeet notify setup ntfy"),
            "{first:#}"
        );
        let again = ntfy_401.send(&question(), &tokens).unwrap_err();
        assert!(again.is::<Said>());
        let (good, _) = ntfy(home.path(), ok);
        good.send(&question(), &tokens).unwrap();
        assert!(!ntfy_401
            .send(&question(), &tokens)
            .unwrap_err()
            .is::<Said>());

        // Without a token, a protected topic says to store one.
        let home = tempfile::tempdir().unwrap();
        let (ntfy_403, _) = ntfy(home.path(), |_| Err(refused(403)));
        let e = ntfy_403
            .send(&question(), &super::tests::tokens(home.path(), None))
            .unwrap_err();
        assert!(format!("{e:#}").contains("without a token"), "{e:#}");
    }

    #[test]
    fn it_posts_to_the_servers_root() {
        let (base, asked) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        // `base` is `http://127.0.0.1:<port>/api/`: a server under a path.
        let server = base.trim_end_matches('/');
        let home = tempfile::tempdir().unwrap();
        make_dir(home.path()).unwrap();
        let ntfy = Ntfy {
            topic: Some("jeet-abc".into()),
            home: home.path().to_path_buf(),
            http: Box::new(http(server)),
        };
        ntfy.send(&question(), &tokens(home.path(), Some("tk_9")))
            .unwrap();
        let asked = asked.join().unwrap();
        assert!(asked.starts_with("POST /api/ "), "{asked}");
        assert!(
            asked
                .to_ascii_lowercase()
                .contains("authorization: bearer tk_9\r\n"),
            "{asked}"
        );
        let body: Value =
            serde_json::from_str(&asked[asked.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body["topic"], "jeet-abc");
    }

    #[test]
    fn helpers() {
        assert!(valid_topic("jeet-abc_1"));
        assert!(!valid_topic(""));
        assert!(!valid_topic("a/b"));
        assert!(!valid_topic(&"a".repeat(65)));
        let topic = random_topic();
        assert!(valid_topic(&topic), "{topic}");
        assert_ne!(topic, random_topic());
        assert!(plausible_token("tk_AgQdq7mVBoFD37zQVN29RhuMzNIz2"));
        assert!(!plausible_token("tk 1"));
        assert!(!plausible_token(""));
    }
}
