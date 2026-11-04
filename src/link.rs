//! `plete://` links: a way to name a task, a review or a report from outside
//! plete, in a notification, a browser or a chat message, and to open it.
//!
//! ```text
//! plete://task/<slug>      a task
//! plete://review/<slug>    a review
//! plete://review/<slug>/diff     a review's PR diff, in plete
//! plete://review/<slug>/explore  a review's worktree, in the explorer
//! plete://report/<id>      a report (`r123` works too)
//! ```
//!
//! A link is read strictly: it arrives from anywhere, so anything that is
//! not exactly one of these is refused rather than guessed at. [`registry`]
//! is how `plete open` finds the plete screen that is open; [`terminal`] how
//! it starts one; [`handler`] makes the system pass `plete://` links to it.

pub mod handler;
pub mod registry;
pub mod terminal;

use std::fmt;

use anyhow::{bail, Result};

/// The schemes a link is read from: the new name and the old.
const SCHEMES: [&str; 2] = ["plete://", "jeet://"];
/// The scheme links are written with.
const SCHEME: &str = SCHEMES[0];

/// Longest slug a link carries.
const SLUG_MAX: usize = 128;

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A task, by `TaskRecord.slug`.
    Task(String),
    /// A review, by `ReviewRecord.slug`.
    Review(String),
    /// A review's PR diff in the review viewer, by `ReviewRecord.slug`.
    ReviewDiff(String),
    /// A review's worktree in the explorer, by `ReviewRecord.slug`.
    ReviewExplore(String),
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
            Link::ReviewDiff(slug) => write!(f, "{SCHEME}review/{slug}/diff"),
            Link::ReviewExplore(slug) => write!(f, "{SCHEME}review/{slug}/explore"),
            Link::Report(id) => write!(f, "{SCHEME}report/{id}"),
        }
    }
}

/// The URL for `link`: what a notification's `Event.link` holds, so that
/// clicking it runs `plete open` on it.
pub fn link_for(link: &Link) -> String {
    link.to_string()
}

/// Read a link. The scheme and the kind may be in any case; the slug is
/// kept as written. One trailing `/` is let through.
pub fn parse(url: &str) -> Result<Link> {
    let scheme = SCHEMES.into_iter().find(|s| {
        url.get(..s.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(s))
    });
    let Some(scheme) = scheme else {
        bail!("not a plete link ({SCHEME}...): {url:?}");
    };
    let rest = &url[scheme.len()..];
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').collect();
    let (kind, name, view) = match parts[..] {
        [kind, name] => (kind, name, None),
        [kind, name, view] if kind.eq_ignore_ascii_case("review") => (kind, name, Some(view)),
        _ => bail!(
            "a plete link is {SCHEME}task/<slug>, review/<slug>, review/<slug>/diff, \
             review/<slug>/explore or report/<id>: {url:?}"
        ),
    };
    match kind.to_ascii_lowercase().as_str() {
        "task" => Ok(Link::Task(slug(name)?)),
        "review" => match view.map(str::to_ascii_lowercase).as_deref() {
            None => Ok(Link::Review(slug(name)?)),
            Some("diff") => Ok(Link::ReviewDiff(slug(name)?)),
            Some("explore") => Ok(Link::ReviewExplore(slug(name)?)),
            Some(_) => bail!("a review link goes on to diff or explore: {url:?}"),
        },
        "report" => Ok(Link::Report(report_id(name)?)),
        _ => bail!("unknown kind {kind:?} in a plete link (task, review or report): {url:?}"),
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
    fn both_schemes_are_read_and_the_old_one_written() {
        assert_eq!(parse("plete://task/fix").unwrap(), Link::task("fix"));
        assert_eq!(parse("PLETE://Report/r7").unwrap(), Link::Report(7));
        assert_eq!(parse("plete://task/fix").unwrap(), Link::task("fix"));
        assert!(parse("plete://task/a b").is_err());
        assert!(parse("plet://task/fix").is_err());
        assert_eq!(Link::task("fix").to_string(), format!("{SCHEME}task/fix"));
    }

    #[test]
    fn links_round_trip() {
        for link in [
            Link::task("fix-login"),
            Link::review("plete-55"),
            Link::task("a.b_c-9"),
            Link::ReviewDiff("plete-55".into()),
            Link::ReviewExplore("plete-55".into()),
            Link::Report(123),
        ] {
            assert_eq!(parse(&link_for(&link)).unwrap(), link);
        }
        assert_eq!(Link::Report(7).to_string(), "plete://report/7");
        assert_eq!(
            Link::ReviewDiff("plete-55".into()).to_string(),
            "plete://review/plete-55/diff"
        );
    }

    #[test]
    fn reading_is_forgiving_only_where_promised() {
        assert_eq!(parse("PLETE://Task/Fix").unwrap(), Link::task("Fix"));
        assert_eq!(parse("plete://task/fix/").unwrap(), Link::task("fix"));
        assert_eq!(parse("plete://report/r123").unwrap(), Link::Report(123));
        assert_eq!(parse("plete://report/123/").unwrap(), Link::Report(123));
        assert_eq!(
            parse("PLETE://Review/x/Diff/").unwrap(),
            Link::ReviewDiff("x".into())
        );
        assert_eq!(
            parse("plete://review/x/explore").unwrap(),
            Link::ReviewExplore("x".into())
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "",
            "plete:/task/x",
            "http://task/x",
            "plete://",
            "plete://task",
            "plete://task/",
            "plete://task//",
            "plete://task/x//",
            "plete://task/x/y",
            "plete://task/x/diff",
            "plete://review/x/y",
            "plete://review/x/diff/y",
            "plete://review//diff",
            "plete://review/.hidden/diff",
            "plete://report/1/diff",
            "plete://task/x?a=b",
            "plete://task/x#y",
            "plete://task/x%20y",
            "plete://task/x y",
            "plete://task/ x",
            "plete://task/.hidden",
            "plete://task/..",
            "plete://task/\u{e9}",
            "plete://other/x",
            "plete://report/",
            "plete://report/r",
            "plete://report/-1",
            "plete://report/1x",
            "plete://report/99999999999999999999",
            "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be refused");
        }
        let long = format!("plete://task/{}", "a".repeat(SLUG_MAX + 1));
        assert!(parse(&long).is_err());
        let longest = format!("plete://task/{}", "a".repeat(SLUG_MAX));
        assert!(parse(&longest).is_ok());
    }

    #[test]
    fn notify_takes_a_title_or_a_link_subcommand() {
        use crate::cli::{Cli, Commands, NotifyCommands};
        use clap::Parser;
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("plete").chain(args.iter().copied())).unwrap()
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
            parse(&["open", "plete://task/x"]).command,
            Some(Commands::Open { .. })
        ));
    }
}
