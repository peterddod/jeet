//! Reports: what a task with a parent tells it, with `jeet report`, and what
//! jeet tells the parent about it on its own (a `status` report) when it
//! waits on a prompt, sits idle or ends. Each is kept as a row, the record,
//! and delivered to the parent fenced as untrusted
//! ([`crate::assistant::fence_report`]).

use anyhow::{bail, Result};

use crate::context::App;
use crate::db::tasks::{ReportKind, TaskRecord};
use crate::messages::Delivered;

/// Longest report body, in bytes once cleaned. Each `<` or `>` becomes a
/// three-byte `‹` or `›` when fenced, so even the worst case fits a message.
#[allow(dead_code)]
pub const MAX_REPORT: usize = 2000;

/// What a `status` report is about.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusEvent {
    /// Waiting on a prompt only the user can answer.
    Waiting,
    /// Idle longer than `[managers] idle_report_mins`.
    Idle,
    /// The session ended without reporting done.
    Ended,
}

#[allow(dead_code)]
impl StatusEvent {
    /// The word its report body starts with.
    pub fn word(self) -> &'static str {
        match self {
            StatusEvent::Waiting => "waiting",
            StatusEvent::Idle => "idle",
            StatusEvent::Ended => "ended",
        }
    }
}

/// Record a report about `task` and deliver it to its parent. Returns the
/// report's id, and how delivery went: the row is kept either way.
#[allow(dead_code)]
pub fn send(
    _app: &App,
    _task: &TaskRecord,
    _kind: ReportKind,
    _body: &str,
    _may_start: bool,
) -> Result<(i64, Result<Delivered>)> {
    bail!("not yet")
}

/// Tell `task`'s parent about `event`, once per `since`.
#[allow(dead_code)]
pub fn auto_status(
    _app: &App,
    _task: &TaskRecord,
    _event: StatusEvent,
    _body: &str,
    _since: i64,
) -> Result<()> {
    bail!("not yet")
}

/// Tell the parents of tasks idle too long.
#[allow(dead_code)]
pub fn sweep_idle(_app: &App, _now: i64) -> Result<()> {
    bail!("not yet")
}

/// What `task`'s parent asked of it, fenced, for its agent's session context.
#[allow(dead_code)]
pub fn brief_context(_app: &App, _task: &TaskRecord) -> Result<String> {
    bail!("not yet")
}
