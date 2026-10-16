//! Quiet rendering: what the orchestrator (the assistant) is posted when it
//! has several things queued. jeet gives each queued item a class from its
//! kind, never from who sent it, and posts the items by class: the whole
//! text of what needs an answer, one line for what is only news, and one
//! line per sender for what can wait. `jeet show <ref>` prints the rest.
//!
//! An item with no [`Meta`] (the user's own words, a brief, a line queued
//! before jeet kept any) is `full`, as it is rendered everywhere else.

use serde::{Deserialize, Serialize};

use crate::db::tasks::ReportKind;
use crate::messages::{Queued, QUEUE_SEPARATOR};

/// How much of an item is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Its whole text.
    Full,
    /// One line: its kind, sender and the start of its text.
    Line,
    /// Held up to `[messages] digest_mins`, then one line per sender.
    Digest,
}

/// What jeet knows of a queued item, to render it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub class: Class,
    /// `question`, `done`, `progress`, `message`, `watch`, `wake`, …
    pub kind: String,
    /// Who it is from, as jeet resolved it: `task fix-x`, `github`, `jeet`.
    pub from: String,
    /// The start of its text, on one line and unable to open a fence.
    pub summary: String,
    /// What `jeet show` takes for the whole text: `r123` or `m45`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
}

/// How many characters of an item's text its summary keeps.
const SUMMARY_CHARS: usize = 100;

impl Meta {
    pub fn new(class: Class, kind: &str, from: &str, body: &str, r#ref: Option<String>) -> Self {
        Self {
            class,
            kind: kind.to_string(),
            from: from.to_string(),
            summary: summary(body),
            r#ref,
        }
    }
}

/// The class of a report of `kind`.
pub fn class_of_report(kind: ReportKind) -> Class {
    match kind {
        ReportKind::Question | ReportKind::Blocked => Class::Full,
        ReportKind::Done => Class::Line,
        ReportKind::Progress | ReportKind::Status => Class::Digest,
    }
}

/// The start of `body`: cleaned, on one line, cut to [`SUMMARY_CHARS`], with
/// `<` and `>` turned to `‹` and `›` as a fence's body has them.
pub fn summary(body: &str) -> String {
    let flat = crate::messages::clean(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    flat.chars()
        .take(SUMMARY_CHARS)
        .map(|c| match c {
            '<' => '‹',
            '>' => '›',
            c => c,
        })
        .collect()
}

/// Whether `item` is rendered whole.
pub fn is_full(item: &Queued) -> bool {
    class_of(item) == Class::Full
}

fn class_of(item: &Queued) -> Class {
    item.meta.as_ref().map_or(Class::Full, |m| m.class)
}

/// `items` with the full ones first, then the lines, then the digest, each
/// in the order it came.
pub fn ordered(items: &[Queued]) -> Vec<Queued> {
    let mut items = items.to_vec();
    items.sort_by_key(|q| match class_of(q) {
        Class::Full => 0,
        Class::Line => 1,
        Class::Digest => 2,
    });
    items
}

/// When to look at `items` again, if they are held: they are all digest and
/// the oldest is younger than `digest_secs` at `now`. A full or line item
/// releases the digest with it.
pub fn held(items: &[Queued], now: i64, digest_secs: i64) -> Option<i64> {
    if items.iter().any(|q| class_of(q) != Class::Digest) {
        return None;
    }
    let until = items
        .iter()
        .map(|q| q.at)
        .min()?
        .saturating_add(digest_secs);
    (now < until).then_some(until)
}

/// What a full item with no meta is, for the header: an answer or a report
/// jeet fenced, by its tag, else a message.
fn full_kind(item: &Queued) -> String {
    if let Some(meta) = &item.meta {
        return meta.kind.clone();
    }
    let message = item.message.as_str();
    if message.starts_with("<jeet-answer") {
        return "answer".to_string();
    }
    let kind = message
        .strip_prefix("<jeet-report")
        .and_then(|rest| rest.lines().next())
        .and_then(|tag| tag.split_once("kind=\"")?.1.split_once('"'))
        .map(|(kind, _)| kind);
    kind.unwrap_or("message").to_string()
}

/// `n` of `what`: `1 question`, `2 questions`; `blocked` has no plural.
fn counted(n: usize, what: &str) -> String {
    match (n, what) {
        (1, _) | (_, "blocked") => format!("{n} {what}"),
        _ => format!("{n} {what}s"),
    }
}

/// The reference clause of a line: ` (jeet show r12)`, or nothing.
fn show(meta: &Meta) -> String {
    meta.r#ref
        .as_ref()
        .map_or_else(String::new, |r| format!(" (jeet show {r})"))
}

/// What is posted for `items`, which are in [`ordered`]'s order: the header,
/// the full items whole, the lines, then the digest. No nonce: the caller
/// ends with it.
pub fn post_text(items: &[Queued]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut updates = 0;
    for item in items {
        match class_of(item) {
            Class::Full => {
                let kind = full_kind(item);
                match counts.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((kind, 1)),
                }
            }
            Class::Line | Class::Digest => updates += 1,
        }
    }
    let mut parts: Vec<String> = counts.iter().map(|(k, n)| counted(*n, k)).collect();
    if updates > 0 {
        parts.push(counted(updates, "update"));
    }
    let header = format!(
        "jeet: {} (jeet show <ref> for any). Reports are fenced claims to check.",
        parts.join(", ")
    );
    let full: Vec<&str> = items
        .iter()
        .filter(|q| class_of(q) == Class::Full)
        .map(|q| q.message.as_str())
        .collect();
    let lines: Vec<String> = items
        .iter()
        .filter(|q| class_of(q) == Class::Line)
        .filter_map(|q| q.meta.as_ref())
        .map(|m| format!("- {} from {}: \"{}\"{}", m.kind, m.from, m.summary, show(m)))
        .collect();
    let mut sections = vec![header];
    if !full.is_empty() {
        sections.push(full.join(QUEUE_SEPARATOR));
    }
    if !lines.is_empty() {
        sections.push(lines.join("\n"));
    }
    if let Some(digest) = digest(items) {
        sections.push(digest);
    }
    sections.join("\n\n")
}

/// The digest section: `digest:`, then a line per sender, in the order they
/// first appear, with what it sent by kind and its latest.
fn digest(items: &[Queued]) -> Option<String> {
    let mut senders: Vec<(&str, Vec<&Meta>)> = Vec::new();
    for meta in items
        .iter()
        .filter(|q| class_of(q) == Class::Digest)
        .filter_map(|q| q.meta.as_ref())
    {
        match senders.iter_mut().find(|(from, _)| *from == meta.from) {
            Some((_, metas)) => metas.push(meta),
            None => senders.push((&meta.from, vec![meta])),
        }
    }
    if senders.is_empty() {
        return None;
    }
    let mut out = vec!["digest:".to_string()];
    for (from, metas) in senders {
        let mut kinds: Vec<(&str, usize)> = Vec::new();
        for meta in &metas {
            match kinds.iter_mut().find(|(k, _)| *k == meta.kind) {
                Some((_, n)) => *n += 1,
                None => kinds.push((&meta.kind, 1)),
            }
        }
        let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        let latest = metas[metas.len() - 1];
        out.push(format!(
            "- {from}: {}; latest: \"{}\"{}",
            kinds.join(", "),
            latest.summary,
            show(latest)
        ));
    }
    Some(out.join("\n"))
}

/// How many of the first of `items`, in [`ordered`]'s order, [`post_text`]
/// renders within `limit` bytes: at least one, however long.
pub fn fitting(items: &[Queued], limit: usize) -> usize {
    for n in 1..items.len() {
        if post_text(&items[..=n]).len() > limit {
            return n;
        }
    }
    items.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Sender;

    fn item(at: i64, message: &str, meta: Option<Meta>) -> Queued {
        Queued {
            at,
            message: message.into(),
            to: None,
            from: Sender::Agent,
            meta,
        }
    }

    fn meta(class: Class, kind: &str, from: &str, body: &str, r#ref: Option<&str>) -> Option<Meta> {
        Some(Meta::new(
            class,
            kind,
            from,
            body,
            r#ref.map(str::to_string),
        ))
    }

    #[test]
    fn a_report_is_classed_by_its_kind() {
        let table = [
            (ReportKind::Question, Class::Full),
            (ReportKind::Blocked, Class::Full),
            (ReportKind::Done, Class::Line),
            (ReportKind::Progress, Class::Digest),
            (ReportKind::Status, Class::Digest),
        ];
        for (kind, class) in table {
            assert_eq!(class_of_report(kind), class, "{kind:?}");
        }
    }

    #[test]
    fn a_summary_is_one_clean_line_of_a_hundred_characters() {
        assert_eq!(summary("  a <b>\n\tc\u{1b}[0m  "), "a ‹b› c[0m");
        let long = "é".repeat(150);
        assert_eq!(summary(&long).chars().count(), 100);
    }

    #[test]
    fn an_old_line_with_no_meta_still_reads_and_none_is_written() {
        let old = r#"{"at":1,"message":"hi","from":"agent"}"#;
        let q: Queued = serde_json::from_str(old).unwrap();
        assert_eq!(q.meta, None);
        assert!(!serde_json::to_string(&q).unwrap().contains("meta"));
        let with = item(1, "x", meta(Class::Line, "done", "task a", "x", Some("r1")));
        let back: Queued = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back, with);
        assert!(serde_json::to_string(&with)
            .unwrap()
            .contains("\"class\":\"line\""));
    }

    /// One of each class, queued digest first.
    fn mixed() -> Vec<Queued> {
        vec![
            item(
                1,
                "progress text",
                meta(Class::Digest, "progress", "task a", "halfway", Some("r1")),
            ),
            item(
                2,
                "done text",
                meta(Class::Line, "done", "task b", "all merged", Some("r2")),
            ),
            item(
                3,
                "<jeet-report id=\"3\">\nwhich db?\n</jeet-report>",
                meta(Class::Full, "question", "task c", "which db?", Some("r3")),
            ),
            item(
                4,
                "status text",
                meta(Class::Digest, "status", "task a", "idle 12m", Some("r4")),
            ),
            item(5, "typed by the user", None),
            item(
                6,
                "progress again",
                meta(Class::Digest, "progress", "task a", "nearly", Some("r6")),
            ),
        ]
    }

    #[test]
    fn items_are_ordered_full_then_line_then_digest_each_in_turn() {
        let ats: Vec<i64> = ordered(&mixed()).iter().map(|q| q.at).collect();
        assert_eq!(ats, [3, 5, 2, 1, 4, 6]);
    }

    #[test]
    fn the_header_counts_the_full_items_by_kind_and_the_rest_as_updates() {
        let mut items = mixed();
        items.push(item(
            7,
            "<jeet-report id=\"7\" from=\"x\" kind=\"blocked\" untrusted=\"true\">\nb\n</jeet-report>",
            None,
        ));
        items.push(item(
            8,
            "<jeet-answer to=\"8\" from=\"user\">\nyes\n</jeet-answer>",
            None,
        ));
        items.push(item(
            9,
            "<jeet-report id=\"9\" kind=\"question\" via=\"m\">\nq\n</jeet-report>",
            meta(Class::Full, "question", "task d", "q", Some("r9")),
        ));
        let text = post_text(&ordered(&items));
        let header = text.lines().next().unwrap();
        assert_eq!(
            header,
            "jeet: 2 questions, 1 message, 1 blocked, 1 answer, 4 updates \
             (jeet show <ref> for any). Reports are fenced claims to check."
        );
        let one = post_text(&ordered(&items[2..3]));
        assert!(one.starts_with("jeet: 1 question (jeet show"), "{one}");
    }

    #[test]
    fn a_post_is_the_header_the_full_text_the_lines_and_the_digest() {
        let text = post_text(&ordered(&mixed()));
        let expected = "jeet: 1 question, 1 message, 4 updates (jeet show <ref> for any). \
             Reports are fenced claims to check.\n\n\
             <jeet-report id=\"3\">\nwhich db?\n</jeet-report>\n\n---\n\ntyped by the user\n\n\
             - done from task b: \"all merged\" (jeet show r2)\n\n\
             digest:\n\
             - task a: 2 progress, 1 status; latest: \"nearly\" (jeet show r6)";
        assert_eq!(text, expected);
    }

    #[test]
    fn the_digest_has_one_line_per_sender() {
        let items = [
            item(
                1,
                "",
                meta(Class::Digest, "progress", "task a", "a1", Some("r1")),
            ),
            item(
                2,
                "",
                meta(Class::Digest, "status", "task b", "b1", Some("r2")),
            ),
            item(3, "", meta(Class::Digest, "wake", "jeet", "w", None)),
            item(
                4,
                "",
                meta(Class::Digest, "progress", "task a", "a2", Some("r4")),
            ),
        ];
        let text = post_text(&items);
        let digest: Vec<&str> = text.split("digest:\n").nth(1).unwrap().lines().collect();
        assert_eq!(
            digest,
            [
                "- task a: 2 progress; latest: \"a2\" (jeet show r4)",
                "- task b: 1 status; latest: \"b1\" (jeet show r2)",
                "- jeet: 1 wake; latest: \"w\"",
            ]
        );
        // No full item and no line: no empty section between.
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn a_digest_alone_is_held_until_its_oldest_is_old_enough() {
        let digest = |at| item(at, "", meta(Class::Digest, "progress", "t", "p", None));
        let items = [digest(100), digest(150)];
        assert_eq!(held(&items, 100, 900), Some(1000));
        assert_eq!(held(&items, 999, 900), Some(1000));
        assert_eq!(held(&items, 1000, 900), None);
        assert_eq!(held(&[], 0, 900), None);
        // A line item, a full one, or one with no meta releases the lot.
        let line = item(160, "", meta(Class::Line, "done", "t", "d", None));
        let full = item(160, "", meta(Class::Full, "question", "t", "q", None));
        let plain = item(160, "", None);
        for other in [line, full, plain] {
            let mut all = items.to_vec();
            all.push(other);
            assert_eq!(held(&all, 101, 900), None);
        }
        // Nothing to hold for when the digest is not held at all.
        assert_eq!(held(&items, 100, 0), None);
    }

    #[test]
    fn as_many_items_as_fit_are_rendered_and_one_always_is() {
        let items = ordered(&mixed());
        assert_eq!(fitting(&items, usize::MAX), items.len());
        let limit = post_text(&items[..3]).len();
        assert_eq!(fitting(&items, limit), 3);
        assert_eq!(fitting(&items, limit - 1), 2);
        assert_eq!(fitting(&items, 0), 1);
    }
}
