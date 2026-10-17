//! `jeet://` links: a way to name a task, a review or a report from outside
//! jeet, in a notification, a browser or a chat message, and to open it.
//!
//! ```text
//! jeet://task/<slug>      a task
//! jeet://review/<slug>    a review
//! jeet://report/<id>      a report (`r123` works too)
//! ```
//!
//! A link is read strictly: it arrives from anywhere, so anything that is
//! not exactly one of these is refused rather than guessed at. [`registry`]
//! is how `jeet open` finds the jeet screen that is open; [`terminal`] how
//! it starts one; [`handler`] makes the system pass `jeet://` links to it.

pub mod handler;
pub mod registry;
pub mod terminal;

use std::fmt;

use anyhow::{bail, Result};

const SCHEME: &str = "jeet://";

/// Longest slug a link carries.
const SLUG_MAX: usize = 128;

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A task, by `TaskRecord.slug`.
    Task(String),
    /// A review, by `ReviewRecord.slug`.
    Review(String),
    /// A report, by `task_reports.id`.
    Report(i64),
}

impl Link {
    #[cfg(test)]
    pub fn task(slug: &str) -> Link {
        Link::Task(slug.to_string())
    }

    #[cfg(test)]
    pub fn review(slug: &str) -> Link {
        Link::Review(slug.to_string())
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Link::Task(slug) => write!(f, "{SCHEME}task/{slug}"),
            Link::Review(slug) => write!(f, "{SCHEME}review/{slug}"),
            Link::Report(id) => write!(f, "{SCHEME}report/{id}"),
        }
    }
}

/// The URL for `link`: what a notification's `Event.link` holds, so that
/// clicking it runs `jeet open` on it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn link_for(link: &Link) -> String {
    link.to_string()
}

/// Read a link. The scheme and the kind may be in any case; the slug is
/// kept as written. One trailing `/` is let through.
pub fn parse(url: &str) -> Result<Link> {
    let Some(head) = url.get(..SCHEME.len()) else {
        bail!("not a jeet link: {url:?}");
    };
    if !head.eq_ignore_ascii_case(SCHEME) {
        bail!("not a jeet link ({SCHEME}...): {url:?}");
    }
    let rest = &url[SCHEME.len()..];
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let mut parts = rest.split('/');
    let (Some(kind), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("a jeet link is {SCHEME}task/<slug>, review/<slug> or report/<id>: {url:?}");
    };
    match kind.to_ascii_lowercase().as_str() {
        "task" => Ok(Link::Task(slug(name)?)),
        "review" => Ok(Link::Review(slug(name)?)),
        "report" => Ok(Link::Report(report_id(name)?)),
        _ => bail!("unknown kind {kind:?} in a jeet link (task, review or report): {url:?}"),
    }
}

fn slug(name: &str) -> Result<String> {
    let fine = !name.is_empty()
        && name.len() <= SLUG_MAX
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !fine {
        bail!("{name:?} is not a task or review name");
    }
    Ok(name.to_string())
}

fn report_id(name: &str) -> Result<i64> {
    let digits = name.strip_prefix(['r', 'R']).unwrap_or(name);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{name:?} is not a report number");
    }
    match digits.parse::<i64>() {
        Ok(id) => Ok(id),
        Err(_) => bail!("{name:?} is not a report number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        for link in [
            Link::task("fix-login"),
            Link::review("jeet-55"),
            Link::task("a.b_c-9"),
            Link::Report(123),
        ] {
            assert_eq!(parse(&link_for(&link)).unwrap(), link);
        }
        assert_eq!(Link::Report(7).to_string(), "jeet://report/7");
    }

    #[test]
    fn reading_is_forgiving_only_where_promised() {
        assert_eq!(parse("JEET://Task/Fix").unwrap(), Link::task("Fix"));
        assert_eq!(parse("jeet://task/fix/").unwrap(), Link::task("fix"));
        assert_eq!(parse("jeet://report/r123").unwrap(), Link::Report(123));
        assert_eq!(parse("jeet://report/123/").unwrap(), Link::Report(123));
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "",
            "jeet:/task/x",
            "http://task/x",
            "jeet://",
            "jeet://task",
            "jeet://task/",
            "jeet://task//",
            "jeet://task/x//",
            "jeet://task/x/y",
            "jeet://task/x?a=b",
            "jeet://task/x#y",
            "jeet://task/x%20y",
            "jeet://task/x y",
            "jeet://task/ x",
            "jeet://task/.hidden",
            "jeet://task/..",
            "jeet://task/\u{e9}",
            "jeet://other/x",
            "jeet://report/",
            "jeet://report/r",
            "jeet://report/-1",
            "jeet://report/1x",
            "jeet://report/99999999999999999999",
            "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be refused");
        }
        let long = format!("jeet://task/{}", "a".repeat(SLUG_MAX + 1));
        assert!(parse(&long).is_err());
        let longest = format!("jeet://task/{}", "a".repeat(SLUG_MAX));
        assert!(parse(&longest).is_ok());
    }

    #[test]
    fn notify_takes_a_title_or_a_link_subcommand() {
        use crate::cli::{Cli, Commands, NotifyCommands};
        use clap::Parser;
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied())).unwrap()
        };
        let Some(Commands::Notify { title, command, .. }) =
            parse(&["notify", "done", "--body", "x"]).command
        else {
            panic!("expected notify");
        };
        assert_eq!(title.as_deref(), Some("done"));
        assert!(command.is_none());
        let Some(Commands::Notify { title, command, .. }) =
            parse(&["notify", "install-link"]).command
        else {
            panic!("expected notify");
        };
        assert!(title.is_none());
        assert!(matches!(command, Some(NotifyCommands::InstallLink)));
        assert!(matches!(
            parse(&["open", "jeet://task/x"]).command,
            Some(Commands::Open { .. })
        ));
    }
}
