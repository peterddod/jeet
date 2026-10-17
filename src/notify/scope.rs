//! Which of jeet's own events in the tree tell the user (`[notify] events`):
//! the questions, blocks and dones of tasks the user would see from the
//! assistant's level, and anything escalated or stuck at any depth.
//!
//! - A task or manager directly under the assistant tells of its asks and
//!   its done.
//! - A task the user started (`started_by = user`: made at the user's own
//!   terminal, or by the assistant with `--for-user`) tells of them at any
//!   depth.
//! - A review goes by who asked for it: the assistant, as a task directly
//!   under it; the user, as a task the user started; any other task, as a
//!   task deeper down.
//! - An escalated ask, and anything stuck, tells at any depth.
//! - A quiet task holds back what [`quiet_holds`] says.
//!
//! `[[notify.rules]]` then filter what is raised, as any notification.

use anyhow::Result;

use crate::config::NotifyLevel;
use crate::context::App;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::link::{self, Link};

use super::{Event, Why};

/// What happened, as the scope sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    /// A question or a blocked report.
    Asked,
    /// A done report.
    Done,
    /// An ask passed up by the one holding it.
    Escalated,
    /// Blocked too long, CI failing again, a budget used up, an end
    /// without done.
    Stuck,
}

/// What a quiet task's flag holds back. The user's decision: its done
/// and its asks; what is escalated or stuck still tells.
fn quiet_holds(raised: Raised) -> bool {
    match raised {
        Raised::Asked | Raised::Done => true,
        Raised::Escalated | Raised::Stuck => false,
    }
}

/// Where an event comes from, as far as the scope goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// `None` for what is about no task, e.g. a PR no task has.
    pub level: Option<NotifyLevel>,
    /// The task's or review's slug.
    pub slug: Option<String>,
    /// Directly under the assistant (a review: asked for by it).
    pub top_level: bool,
    /// Started by the user (a review: asked for by the user).
    pub user_started: bool,
    pub quiet: bool,
}

impl Origin {
    pub fn task(app: &App, task: &TaskRecord) -> Result<Origin> {
        let flags = app.db.task_notify_flags(&task.id)?;
        Ok(Origin {
            level: Some(super::level(task.kind)),
            slug: Some(task.slug.clone()),
            top_level: under_assistant(app, task.parent_id.as_deref())?,
            user_started: flags.user_started(),
            quiet: flags.quiet,
        })
    }

    pub fn review(app: &App, review: &ReviewRecord) -> Result<Origin> {
        Ok(Origin {
            level: Some(NotifyLevel::Review),
            slug: Some(review.slug.clone()),
            top_level: under_assistant(app, review.requested_by.as_deref())?,
            user_started: review.requested_by.is_none(),
            quiet: false,
        })
    }

    /// About no task: only what tells at any depth tells.
    pub fn nobody() -> Origin {
        Origin {
            level: None,
            slug: None,
            top_level: false,
            user_started: false,
            quiet: false,
        }
    }
}

/// Whether `parent` is the assistant.
fn under_assistant(app: &App, parent: Option<&str>) -> Result<bool> {
    let Some(parent) = parent else {
        return Ok(false);
    };
    Ok(app
        .db
        .get_task(parent)?
        .is_some_and(|p| p.kind == TaskKind::Assistant))
}

/// Whether `raised`, from `origin`, tells the user.
pub fn wanted(origin: &Origin, raised: Raised) -> bool {
    if origin.quiet && quiet_holds(raised) {
        return false;
    }
    match raised {
        Raised::Escalated | Raised::Stuck => true,
        Raised::Asked | Raised::Done => origin.top_level || origin.user_started,
    }
}

/// What [`raise`] is told of.
pub struct Raise {
    pub raised: Raised,
    /// `[[notify.rules]] kind`: `question`, `blocked`, `done`, `stuck`.
    pub kind: &'static str,
    /// What it is about: one is sent per `(why, ref)`.
    pub reference: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
    pub link: Option<Link>,
}

/// The `Why` of `raised`.
fn why(raised: Raised) -> Why {
    match raised {
        Raised::Asked | Raised::Escalated => Why::NeedsUser,
        Raised::Done => Why::Finished,
        Raised::Stuck => Why::Stuck,
    }
}

/// The notification `raise` from `origin` makes, if `[notify] events` is
/// on and the scope wants it.
pub fn event_for(app: &App, origin: &Origin, raise: Raise) -> Option<Event> {
    if !app.config.notify.events() || !wanted(origin, raise.raised) {
        return None;
    }
    Some(Event {
        why: why(raise.raised),
        kind: raise.kind.to_string(),
        level: origin.level,
        task: origin.slug.clone(),
        reference: raise.reference,
        title: raise.title,
        subtitle: raise.subtitle,
        body: raise.body,
        link: raise.link.as_ref().map(link::link_for),
        only: Vec::new(),
    })
}

/// Tell the user of `raise` from `origin`, as [`event_for`] decides.
pub fn raise(app: &App, origin: &Origin, raise: Raise) {
    if let Some(event) = event_for(app, origin, raise) {
        super::event(app, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(top_level: bool, user_started: bool, quiet: bool) -> Origin {
        Origin {
            level: Some(NotifyLevel::Task),
            slug: Some("t".into()),
            top_level,
            user_started,
            quiet,
        }
    }

    const ALL: [Raised; 4] = [
        Raised::Asked,
        Raised::Done,
        Raised::Escalated,
        Raised::Stuck,
    ];

    #[test]
    fn top_level_and_user_started_tell_everything() {
        for o in [origin(true, false, false), origin(false, true, false)] {
            for r in ALL {
                assert!(wanted(&o, r), "{o:?} {r:?}");
            }
        }
    }

    #[test]
    fn deeper_tells_only_escalated_and_stuck() {
        let o = origin(false, false, false);
        assert!(!wanted(&o, Raised::Asked));
        assert!(!wanted(&o, Raised::Done));
        assert!(wanted(&o, Raised::Escalated));
        assert!(wanted(&o, Raised::Stuck));
    }

    #[test]
    fn quiet_holds_back_done_and_asks_only() {
        for o in [
            origin(true, false, true),
            origin(false, true, true),
            origin(false, false, true),
        ] {
            assert!(!wanted(&o, Raised::Asked));
            assert!(!wanted(&o, Raised::Done));
            assert!(wanted(&o, Raised::Escalated));
            assert!(wanted(&o, Raised::Stuck));
        }
    }
}
