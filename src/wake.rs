//! Keeping the wake channel working ([`crate::waiter`]). A watcher can only
//! start in its session's own start-up hook, so a session without one that
//! works (it never had one, its watcher died, or its posts were refused
//! three times) gets one by being resumed: its agent ended and started
//! again in the same place, resuming the session with today's settings.
//!
//! [`sweep_wake`] looks at every live session, from `jeet watch`'s poll
//! with leave to repair, and from `jeet doctor wake` without. A session is
//! only resumed when it is idle, has no draft and nobody has typed in its
//! window for a while, and at most once a day.

use anyhow::Result;

use crate::agent::AgentSpec;
use crate::assistant;
use crate::commands::task::{end_and_resume, warn_all};
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord, TaskState};
use crate::manager::run;
use crate::messages::{self, Sender};
use crate::permissions;
use crate::render::{Class, Meta};
use crate::runtime::{self, Admission};
use crate::status::{self, SessionStatus, State};
use crate::tasks;
use crate::tmux;
use crate::waiter::{self, RepairWhy, Repaired, WakeState};

/// A session is resumed once per this many seconds at most, whatever the
/// reason.
pub const REPAIR_EVERY_SECS: i64 = 86_400;
/// A session is resumed only after it has sat idle, and nobody has typed in
/// its window, this long.
pub const QUIET_SECS: i64 = 120;

/// One live session's wake channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeRow {
    /// The task's slug.
    pub slug: String,
    pub session: String,
    pub state: WakeState,
    /// The last time it was resumed for a watcher.
    pub repaired: Option<Repaired>,
    /// Why it is due a resume, if it is.
    pub due: Option<RepairWhy>,
    /// Whether it may be resumed now. Asked only of one that is due.
    pub ready: bool,
}

/// A session whose agent is running.
struct Live {
    task: TaskRecord,
    session: String,
    /// The assistant's session, with the window it runs in.
    assistant: Option<assistant::Session>,
}

/// Why the session is due a resume at `now`, if it is: `auto` is
/// `[harness] wake_repair`, `hook` whether its harness has a wake hook, and
/// `repaired` the last time it was resumed.
pub fn repair_due(
    auto: bool,
    hook: bool,
    state: &WakeState,
    repaired: Option<&Repaired>,
    now: i64,
) -> Option<RepairWhy> {
    if !(auto && hook) || repaired.is_some_and(|r| now - r.at < REPAIR_EVERY_SECS) {
        return None;
    }
    match state {
        WakeState::Unwatched => Some(RepairWhy::Unwatched),
        WakeState::Off {
            repair_due: true, ..
        } => Some(RepairWhy::Refused),
        WakeState::Off { .. } | WakeState::Watched => None,
    }
}

/// Whether a session may be resumed at `now`: idle for [`QUIET_SECS`] by its
/// status, not held, with no draft in its prompt, and no client showing its
/// window has had input for [`QUIET_SECS`]. `typed` is when the latest did.
pub fn ready(
    status: Option<&SessionStatus>,
    held: bool,
    draft: bool,
    typed: Option<i64>,
    now: i64,
) -> bool {
    status.is_some_and(|s| s.state == State::Idle && now - s.at >= QUIET_SECS)
        && !held
        && !draft
        && typed.is_none_or(|at| now - at >= QUIET_SECS)
}

/// Every live session's wake channel, and, with `repair`, each one that is
/// due and ready resumed. A session that fails to resume is warned of; the
/// others go on.
pub fn sweep_wake(app: &App, now: i64, repair: bool) -> Result<Vec<WakeRow>> {
    let hook = AgentSpec::from_config(&app.config).is_ok_and(|s| s.harness.wake_hook());
    let mut rows = Vec::new();
    for live in live_sessions(app)? {
        let mut row = row_of(app, &live, hook, now);
        if let Some(why) = row.due {
            row.ready = is_ready(app, &live, now);
            if repair && row.ready {
                match resume(app, &live, why, now) {
                    Ok(()) => {
                        row.repaired = Some(Repaired { at: now, why });
                        row.due = None;
                    }
                    Err(e) => eprintln!("jeet: warning: could not resume {}: {e:#}", row.slug),
                }
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// [`sweep_wake`] for the one session a hook runs for, `session` of the task
/// in `task_dir`: whether it is due a resume. Reads files only; a hook never
/// resumes, as it runs under the process a resume would end.
pub fn sweep_own(app: &App, task_dir: &std::path::Path, session: &str, now: i64) -> bool {
    let hook = AgentSpec::from_config(&app.config).is_ok_and(|s| s.harness.wake_hook());
    let state = waiter::wake_state(task_dir, session, now);
    let repaired = waiter::read_repaired(task_dir, session);
    repair_due(
        app.config.wake_repair(),
        hook,
        &state,
        repaired.as_ref(),
        now,
    )
    .is_some()
}

fn row_of(app: &App, live: &Live, hook: bool, now: i64) -> WakeRow {
    let dir = tasks::task_dir(app, &live.task);
    let state = waiter::wake_state(&dir, &live.session, now);
    let repaired = waiter::read_repaired(&dir, &live.session);
    WakeRow {
        slug: live.task.slug.clone(),
        session: live.session.clone(),
        due: repair_due(
            app.config.wake_repair(),
            hook,
            &state,
            repaired.as_ref(),
            now,
        ),
        state,
        repaired,
        ready: false,
    }
}

/// The tasks whose agents run in their tmux sessions, and the assistant's
/// running sessions. Not archived tasks, nor reviews.
fn live_sessions(app: &App) -> Result<Vec<Live>> {
    let mut live = Vec::new();
    if !tmux::available() {
        return Ok(live);
    }
    for task in app.db.list_tasks(Some(TaskState::Active))? {
        if !tmux::agent_alive(&runtime::session_name(&task)) {
            continue;
        }
        let dir = tasks::task_dir(app, &task);
        if let Some(current) = status::current_status(&dir) {
            live.push(Live {
                task,
                session: current.session_id,
                assistant: None,
            });
        }
    }
    if let Some(task) = app.db.get_assistant_task()? {
        for session in assistant::sessions(app, &task)?
            .into_iter()
            .filter(assistant::Session::running)
        {
            live.push(Live {
                task: task.clone(),
                session: session.id.clone(),
                assistant: Some(session),
            });
        }
    }
    Ok(live)
}

/// [`ready`] for `live`, asking tmux and the index. What cannot be read
/// counts against it.
fn is_ready(app: &App, live: &Live, now: i64) -> bool {
    let Ok(spec) = AgentSpec::from_config(&app.config) else {
        return false;
    };
    let dir = tasks::task_dir(app, &live.task);
    let Ok(held) = run::hold(app, &live.task).map(|h| h.is_some()) else {
        return false;
    };
    let status = status::read_status(&dir, &live.session);
    let (pane, clients) = match &live.assistant {
        Some(session) => {
            let Some(window) = session.window.as_deref() else {
                return false;
            };
            (
                tmux::capture_window_styled(window),
                tmux::client_activity(assistant::TMUX_SESSION, Some(window)),
            )
        }
        None => {
            let name = runtime::session_name(&live.task);
            (
                tmux::capture_styled(&name),
                tmux::client_activity(&name, None),
            )
        }
    };
    let (Ok(pane), Some(clients)) = (pane, clients) else {
        return false;
    };
    ready(
        status.as_ref(),
        held,
        spec.harness.has_draft(&pane),
        clients.into_iter().max(),
        now,
    )
}

/// End the session's agent and resume the session in the same place, with
/// today's settings, and say so in the assistant's log.
fn resume(app: &App, live: &Live, why: RepairWhy, now: i64) -> Result<()> {
    let dir = tasks::task_dir(app, &live.task);
    waiter::record_repair(&dir, &live.session, why, now)?;
    match &live.assistant {
        Some(session) => {
            assistant::end(session)?;
            let opened = assistant::open(app, &live.task, assistant::Open::Session(&live.session))?;
            warn_all(&opened.warnings);
        }
        None => {
            let running = end_and_resume(app, &live.task, &live.session, Admission::Bypass)?;
            warn_all(&running.warnings);
        }
    }
    let reason = match why {
        RepairWhy::Unwatched => "unwatched",
        RepairWhy::Refused => "refused 3 times",
    };
    let action = format!(
        "resumed session {} of task {} to restart its wake channel ({reason})",
        live.session, live.task.slug
    );
    if let Err(e) = permissions::log_wake(&app.home, &action, permissions::Outcome::Done) {
        eprintln!("jeet: warning: {e:#}");
    }
    if let Err(e) = note_in_digest(app, &action) {
        eprintln!("jeet: warning: could not note the repair for the assistant: {e:#}");
    }
    Ok(())
}

/// Queue `action` as a digest item for the assistant, if it is rendered
/// quietly: it rides the next post, which this neither types nor starts.
fn note_in_digest(app: &App, action: &str) -> Result<()> {
    if !app.config.quiet_for(TaskKind::Assistant) {
        return Ok(());
    }
    let Some(task) = app.db.get_assistant_task()? else {
        return Ok(());
    };
    let meta = Meta::new(Class::Digest, "wake", "jeet", action, None);
    messages::Inbox::lock(&tasks::task_dir(app, &task))?.push_meta(
        action,
        None,
        Sender::Agent,
        Some(meta),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle(at: i64) -> SessionStatus {
        SessionStatus {
            session_id: "s".into(),
            state: State::Idle,
            detail: None,
            event: "Stop".into(),
            at,
            busy_since: None,
        }
    }

    fn off(refusals: u32, permanent: bool, repair_due: bool) -> WakeState {
        WakeState::Off {
            refusals,
            retry_at: 0,
            permanent,
            repair_due,
        }
    }

    fn done(at: i64) -> Option<Repaired> {
        Some(Repaired {
            at,
            why: RepairWhy::Unwatched,
        })
    }

    #[test]
    fn a_repair_is_due_for_an_unwatched_session_or_one_refused_three_times() {
        let due = |state, repaired: Option<Repaired>| {
            repair_due(true, true, &state, repaired.as_ref(), 100_000)
        };
        assert_eq!(due(WakeState::Unwatched, None), Some(RepairWhy::Unwatched));
        assert_eq!(due(off(3, false, true), None), Some(RepairWhy::Refused));
        assert_eq!(due(WakeState::Watched, None), None);
        // Backing off, or off for good, or already repaired: not yet.
        assert_eq!(due(off(1, false, false), None), None);
        assert_eq!(due(off(3, true, false), None), None);
        assert_eq!(due(off(3, false, false), None), None);
    }

    #[test]
    fn a_session_is_resumed_once_a_day_whatever_the_reason() {
        let due = |repaired_at| {
            repair_due(
                true,
                true,
                &WakeState::Unwatched,
                done(repaired_at).as_ref(),
                200_000,
            )
        };
        assert_eq!(due(200_000 - REPAIR_EVERY_SECS + 1), None);
        assert_eq!(due(200_000 - REPAIR_EVERY_SECS), Some(RepairWhy::Unwatched));
        assert_eq!(due(0), Some(RepairWhy::Unwatched));
        // Refused, a day after a repair for being unwatched: due again.
        let refused = repair_due(true, true, &off(3, false, true), done(0).as_ref(), 200_000);
        assert_eq!(refused, Some(RepairWhy::Refused));
        let soon = repair_due(
            true,
            true,
            &off(3, false, true),
            done(199_999).as_ref(),
            200_000,
        );
        assert_eq!(soon, None);
    }

    #[test]
    fn nothing_is_due_without_wake_repair_or_a_wake_hook() {
        let unwatched = WakeState::Unwatched;
        assert_eq!(repair_due(false, true, &unwatched, None, 5), None);
        assert_eq!(repair_due(true, false, &unwatched, None, 5), None);
        assert_eq!(
            repair_due(true, true, &unwatched, None, 5),
            Some(RepairWhy::Unwatched)
        );
    }

    #[test]
    fn a_session_is_ready_when_idle_undisturbed_and_not_held() {
        let now = 1_000;
        let ok = |status: Option<&SessionStatus>, held, draft, typed| {
            ready(status, held, draft, typed, now)
        };
        let status = idle(now - QUIET_SECS);
        assert!(ok(Some(&status), false, false, None));
        assert!(ok(Some(&status), false, false, Some(now - QUIET_SECS)));
        // Not idle long enough, or not idle, or no status.
        assert!(!ok(Some(&idle(now - QUIET_SECS + 1)), false, false, None));
        for state in [State::Running, State::Waiting, State::Ended] {
            let busy = SessionStatus {
                state,
                ..status.clone()
            };
            assert!(!ok(Some(&busy), false, false, None), "{state:?}");
        }
        assert!(!ok(None, false, false, None));
        // Held, a draft, or someone typing in the window.
        assert!(!ok(Some(&status), true, false, None));
        assert!(!ok(Some(&status), false, true, None));
        assert!(!ok(Some(&status), false, false, Some(now - QUIET_SECS + 1)));
    }

    fn assistant_app(config: &str) -> (tempfile::TempDir, App) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().canonicalize().unwrap();
        let app = App {
            config: toml::from_str(config).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&path)).unwrap(),
            home: path,
        };
        assistant::ensure(&app).unwrap();
        (home, app)
    }

    fn assistant_queue(app: &App) -> Vec<messages::Queued> {
        let task = app.db.get_assistant_task().unwrap().unwrap();
        messages::Inbox::lock(&tasks::task_dir(app, &task))
            .unwrap()
            .read()
            .unwrap()
    }

    #[test]
    fn a_repair_is_noted_in_the_assistants_next_digest_and_nothing_is_typed() {
        let (_home, app) = assistant_app("");
        let action = "resumed session s of task fix-x to restart its wake channel (unwatched)";
        note_in_digest(&app, action).unwrap();
        let [item] = &assistant_queue(&app)[..] else {
            panic!("one item is queued");
        };
        assert_eq!((item.message.as_str(), item.to.as_deref()), (action, None));
        let meta = item.meta.as_ref().unwrap();
        assert_eq!(meta.class, Class::Digest);
        assert_eq!((meta.kind.as_str(), meta.from.as_str()), ("wake", "jeet"));
        assert_eq!(meta.summary, action);
        assert_eq!(meta.r#ref, None);
        // It is held to ride the next post, not posted by itself.
        assert!(crate::render::held(&assistant_queue(&app), item.at, 900).is_some());
    }

    #[test]
    fn a_repair_is_not_noted_when_the_assistant_is_rendered_in_full() {
        let (_home, app) = assistant_app("[messages]\nrender = { assistant = \"full\" }\n");
        note_in_digest(&app, "resumed").unwrap();
        assert!(assistant_queue(&app).is_empty());
    }

    #[test]
    fn a_repair_is_not_noted_when_there_is_no_assistant() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::Config::default(),
            db: crate::db::Database::open(&crate::config::index_path(&path)).unwrap(),
            home: path,
        };
        note_in_digest(&app, "resumed").unwrap();
        assert!(app.db.get_assistant_task().unwrap().is_none());
    }
}
