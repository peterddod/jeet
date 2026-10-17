//! Which queued notifications are sent: `[[notify.rules]]`, deduplication
//! by `(why, ref)`, and more than `max_per_hour` folded into one "N more".
//! What it remembers between sends is [`State`], kept beside the outbox.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{NotifyAction, NotifyConfig, NotifyRule};

use super::{Event, Why};

/// How long a `(why, ref)` already sent is remembered.
const SEEN_SECS: i64 = 7 * 24 * 3600;

const HOUR_SECS: i64 = 3600;

/// Titles listed in an "N more" notification; the rest are counted.
const FOLDED_LISTED: usize = 5;

/// How much of each of those titles is kept.
const TITLE_KEPT: usize = 120;

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// When each notification of the last hour was sent.
    #[serde(default)]
    pub sent: Vec<i64>,
    /// `why|ref` of what was sent, with when.
    #[serde(default)]
    pub seen: BTreeMap<String, i64>,
    /// The titles of the first few held back over `max_per_hour`.
    #[serde(default)]
    pub folded: Vec<String>,
    /// How many more than those were held back.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more: usize,
}

impl State {
    fn prune(&mut self, now: i64) {
        self.sent.retain(|&at| now - at < HOUR_SECS);
        self.seen.retain(|_, at| now - *at < SEEN_SECS);
    }

    fn full(&self, max: u32) -> bool {
        max > 0 && self.sent.len() >= max as usize
    }

    /// `event`, if it is to be sent now: not ignored by a rule, not sent
    /// before, and not over the hour's limit, where it is folded instead.
    pub fn admit(&mut self, config: &NotifyConfig, event: Event, now: i64) -> Option<Event> {
        self.prune(now);
        if decide(&config.rules, &event) == NotifyAction::Ignore {
            return None;
        }
        if let Some(reference) = &event.reference {
            let key = format!("{}|{reference}", event.why.name());
            if self.seen.contains_key(&key) {
                return None;
            }
            self.seen.insert(key, now);
        }
        if self.full(config.max_per_hour()) {
            match self.folded.len() < FOLDED_LISTED {
                true => self
                    .folded
                    .push(event.title.chars().take(TITLE_KEPT).collect()),
                false => self.more += 1,
            }
            return None;
        }
        self.sent.push(now);
        Some(event)
    }

    /// One notification for all that was folded, once the hour's limit
    /// allows another.
    pub fn unfold(&mut self, config: &NotifyConfig, now: i64) -> Option<Event> {
        self.prune(now);
        if self.folded.is_empty() || self.full(config.max_per_hour()) {
            return None;
        }
        let folded = std::mem::take(&mut self.folded);
        let more = std::mem::take(&mut self.more);
        let n = folded.len() + more;
        let mut body = folded.join("; ");
        if more > 0 {
            body = format!("{body}; and {more} others");
        }
        self.sent.push(now);
        Some(Event {
            why: Why::Info,
            kind: "folded".into(),
            title: format!(
                "jeet: {n} more notification{}",
                if n == 1 { "" } else { "s" }
            ),
            body,
            ..Event::default()
        })
    }
}

/// What the first rule matching `event` says; none matching, notify.
fn decide(rules: &[NotifyRule], event: &Event) -> NotifyAction {
    rules
        .iter()
        .find(|rule| matches(rule, event))
        .map_or(NotifyAction::Notify, |rule| rule.action)
}

fn matches(rule: &NotifyRule, event: &Event) -> bool {
    let kind = rule.kind.is_empty() || rule.kind.contains(&event.kind);
    let level = rule.level.is_empty() || event.level.is_some_and(|l| rule.level.contains(&l));
    let task = rule.task.is_empty() || event.task.as_ref().is_some_and(|t| rule.task.contains(t));
    kind && level && task
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyLevel;

    fn event(kind: &str, level: Option<NotifyLevel>, task: Option<&str>) -> Event {
        Event {
            kind: kind.into(),
            level,
            task: task.map(str::to_string),
            title: format!("{kind} title"),
            ..Event::default()
        }
    }

    fn config(text: &str) -> NotifyConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_first_matching_rule_decides_and_none_means_notify() {
        let config = config(
            r#"
[[rules]]
kind = ["review"]
level = ["review"]
action = "ignore"

[[rules]]
task = ["fix-login"]
action = "notify"

[[rules]]
level = ["task", "manager"]
action = "ignore"
"#,
        );
        let review = event("review", Some(NotifyLevel::Review), Some("review-7"));
        assert_eq!(decide(&config.rules, &review), NotifyAction::Ignore);
        let named = event("question", Some(NotifyLevel::Task), Some("fix-login"));
        assert_eq!(decide(&config.rules, &named), NotifyAction::Notify);
        let other = event("question", Some(NotifyLevel::Task), Some("other"));
        assert_eq!(decide(&config.rules, &other), NotifyAction::Ignore);
        // No level: a rule naming levels is not for it.
        let github = event("ci_failed", None, None);
        assert_eq!(decide(&config.rules, &github), NotifyAction::Notify);
        assert_eq!(decide(&[], &other), NotifyAction::Notify);
    }

    #[test]
    fn the_same_why_and_ref_is_sent_once() {
        let config = NotifyConfig::default();
        let mut state = State::default();
        let asked = Event {
            why: Why::NeedsUser,
            reference: Some("r41".into()),
            ..event("question", None, None)
        };
        assert!(state.admit(&config, asked.clone(), 100).is_some());
        assert!(state.admit(&config, asked.clone(), 200).is_none());
        // Another why for the same ref is another notification.
        let stuck = Event {
            why: Why::Stuck,
            ..asked.clone()
        };
        assert!(state.admit(&config, stuck, 300).is_some());
        // No ref: never deduplicated.
        let plain = event("action", None, None);
        assert!(state.admit(&config, plain.clone(), 400).is_some());
        assert!(state.admit(&config, plain, 400).is_some());
        // Forgotten after a week.
        assert!(state.admit(&config, asked, 100 + SEEN_SECS).is_some());
    }

    #[test]
    fn over_max_per_hour_folds_into_one_n_more() {
        let config = config("max_per_hour = 2");
        let mut state = State::default();
        for i in 0..5 {
            let sent = state.admit(&config, event(&format!("k{i}"), None, None), 10 + i);
            assert_eq!(sent.is_some(), i < 2, "{i}");
        }
        // Still full.
        assert!(state.unfold(&config, 100).is_none());
        // An hour after the first, one goes: all three folded, in one.
        let more = state.unfold(&config, 10 + HOUR_SECS).unwrap();
        assert_eq!(more.title, "jeet: 3 more notifications");
        assert_eq!(more.body, "k2 title; k3 title; k4 title");
        assert!(state.folded.is_empty());
        assert!(state.unfold(&config, 10 + HOUR_SECS).is_none());
    }

    #[test]
    fn zero_max_per_hour_never_folds() {
        let config = config("max_per_hour = 0");
        let mut state = State::default();
        for i in 0..20 {
            assert!(state.admit(&config, event("k", None, None), i).is_some());
        }
    }

    #[test]
    fn a_long_fold_lists_five_and_counts_the_rest() {
        let config = config("max_per_hour = 1");
        let mut state = State::default();
        for i in 0..8 {
            state.admit(&config, event(&format!("k{i}"), None, None), 0);
        }
        assert_eq!((state.folded.len(), state.more), (5, 2));
        let more = state.unfold(&config, HOUR_SECS).unwrap();
        assert_eq!(more.title, "jeet: 7 more notifications");
        assert_eq!(state.more, 0);
        assert!(
            more.body.ends_with("k5 title; and 2 others"),
            "{}",
            more.body
        );
    }
}
