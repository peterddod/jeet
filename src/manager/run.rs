//! Whether a node may run, and what changes it: pause, resume and stop over
//! a subtree (M15-M18), and the one gate every start of a node in the tree
//! goes through, `[managers] max_live` (M22).
//!
//! A node's run state is written on its own row only, with who set it; what
//! holds it is the most severe state over it and its ancestors
//! ([`hold`]). A held node starts nothing and is handed nothing.

// `jeet manager pause|resume|stop|drain` are the callers still to come.
#![allow(dead_code)]

use std::collections::HashSet;
use std::fs::{File, OpenOptions};

use anyhow::{bail, Context, Result};

use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::tasks::{RunState, Setter, TaskKind, TaskRecord, TaskState};
use crate::messages::{self, Inbox, TmuxAgent};
use crate::permissions::Caller;
use crate::placement::{self, NodeRuntime};
use crate::reports::{self, StatusEvent};
use crate::runtime::{self, Interrupted};
use crate::status::{self, Activity};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

use super::label;

/// The runtime that interrupts a node: [`placement::runtime_for`], or a
/// fake in tests.
pub type RuntimeFor<'a> = &'a dyn Fn(&TaskRecord) -> Result<&'a dyn NodeRuntime>;

/// Whether this jeet runs in a node's own session, which it may not
/// interrupt or end: [`runtime::is_own_session`], or a stand-in in tests.
pub type OwnSession<'a> = &'a dyn Fn(&TaskRecord) -> bool;

/// What a walk did to one node, or why it left it alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeOutcome {
    pub slug: String,
    pub line: String,
}

impl NodeOutcome {
    fn new(task: &TaskRecord, line: impl Into<String>) -> Self {
        Self {
            slug: task.slug.clone(),
            line: line.into(),
        }
    }
}

/// What holds `task`, if anything: the nearest task, itself or above, with
/// the most severe run state over them, and that state.
pub fn hold(app: &App, task: &TaskRecord) -> Result<Option<(TaskRecord, RunState)>> {
    let (state, holder) = app.db.effective_run_state(&task.id)?;
    Ok(holder.map(|h| (h, state)))
}

/// The setter `by` writes a run state as.
pub fn setter_of(by: &Caller) -> Setter {
    match by {
        Caller::User => Setter::User,
        Caller::Root => Setter::Root,
        Caller::Node(node) => Setter::Node(node.id.clone()),
    }
}

/// Whether `by` ranks at least as high as `setter`, and so may undo what it
/// set (M15): the user, then the assistant, then a node above the caller,
/// then the caller itself, then a node below it. `budget` ranks lowest. A
/// node setter that is neither the caller nor below it outranks it.
pub fn outranks(app: &App, by: &Caller, setter: &Setter) -> Result<bool> {
    Ok(match (by, setter) {
        (Caller::User, _) => true,
        (_, Setter::User) => false,
        (Caller::Root, _) => true,
        (Caller::Node(_), Setter::Root) => false,
        (_, Setter::Budget) => true,
        (Caller::Node(me), Setter::Node(id)) => {
            *id == me.id || app.db.is_strict_descendant(id, &me.id)?
        }
    })
}

/// Who `setter` is, for a refusal: `the user`, `manager backend`.
fn setter_label(app: &App, setter: &Setter) -> Result<String> {
    Ok(match setter {
        Setter::User => "the user".to_string(),
        Setter::Root => "the assistant".to_string(),
        Setter::Budget => "its budget".to_string(),
        Setter::Node(id) => match app.db.get_task(id)? {
            Some(task) => label(&task),
            None => format!("task {id}"),
        },
    })
}

/// `target` and everything below it, leaves first, `target` last.
fn subtree_leaves_first(app: &App, target: &TaskRecord) -> Result<Vec<TaskRecord>> {
    let mut nodes: Vec<TaskRecord> = app
        .db
        .descendants(&target.id)?
        .into_iter()
        .map(|(task, _)| task)
        .collect();
    nodes.push(target.clone());
    Ok(nodes)
}

/// Pause `target` for `by`: [`pause_with`] the runtime of each node's
/// placement.
pub fn pause(app: &App, by: &Caller, target: &TaskRecord) -> Result<Vec<NodeOutcome>> {
    let rt = |t: &TaskRecord| placement::runtime_for(t).map(|r| r as &dyn NodeRuntime);
    pause_with(app, by, target, &rt, &runtime::is_own_session)
}

/// Pause `target` (M17): write its row, with `by` as the setter, then
/// interrupt the turn of each node in its subtree, leaves first, through
/// `rt`. What a node refuses, or fails at, is a line of the outcome and the
/// walk goes on. The assistant and the node whose session this jeet runs in
/// are skipped. An interrupted node is marked idle, paused.
pub fn pause_with(
    app: &App,
    by: &Caller,
    target: &TaskRecord,
    rt: RuntimeFor,
    own: OwnSession,
) -> Result<Vec<NodeOutcome>> {
    app.db
        .set_run_state(&target.id, RunState::Paused, Some(&setter_of(by)))?;
    let mut outcomes = Vec::new();
    for node in subtree_leaves_first(app, target)? {
        let line = interrupt_line(app, &node, rt, own);
        outcomes.push(NodeOutcome::new(&node, line));
    }
    Ok(outcomes)
}

/// Interrupt `node`'s turn for a pause, saying what came of it.
fn interrupt_line(app: &App, node: &TaskRecord, rt: RuntimeFor, own: OwnSession) -> String {
    if node.kind == TaskKind::Assistant {
        return "the assistant; not interrupted".to_string();
    }
    if node.state == TaskState::Archived {
        return "archived; nothing to interrupt".to_string();
    }
    if own(node) {
        return "this jeet runs in its session; not interrupted".to_string();
    }
    let interrupted = rt(node).and_then(|r| r.interrupt(app, node));
    match interrupted {
        Ok(Interrupted::Sent) => {
            // The turn ends with no Stop hook to charge it.
            if let Err(e) = status::fold_busy(app, node, now_secs()) {
                eprintln!("jeet: could not charge task {}'s budget: {e:#}", node.slug);
            }
            match status::mark_paused(&tasks::task_dir(app, node), now_secs()) {
                Ok(()) => "interrupted".to_string(),
                Err(e) => format!("interrupted, but not marked paused: {e:#}"),
            }
        }
        Ok(Interrupted::NotWorking(activity)) => {
            format!("not working ({}); nothing to interrupt", activity.label())
        }
        Ok(Interrupted::UserTyping) => {
            "not interrupted: its input box holds what the user typed".to_string()
        }
        Ok(Interrupted::Watched) => {
            "not interrupted: it is asking, and someone is attached to answer".to_string()
        }
        Ok(Interrupted::AtStartup) => "not interrupted: it is at a startup prompt".to_string(),
        Err(e) => format!("not interrupted: {e:#}"),
    }
}

/// Why `by` may not resume `target`, if it may not (M15): an ancestor holds
/// it, its subtree has used its budget, or it was held by someone who
/// outranks `by`.
pub fn check_resume(app: &App, by: &Caller, target: &TaskRecord) -> Result<()> {
    let held_above = app
        .db
        .ancestors(&target.id)?
        .into_iter()
        .find(|a| a.run_state != RunState::Run);
    if let Some(holder) = held_above {
        bail!(
            "{} is under {}, which is {}; resume that first",
            label(target),
            label(&holder),
            holder.run_state.as_str()
        );
    }
    if let Some(row) = app.db.budget(&target.id)? {
        let (secs, turns) = app.db.subtree_used(&target.id)?;
        let spent = row.max_secs.is_some_and(|max| secs >= max)
            || row.max_turns.is_some_and(|max| turns >= max);
        if spent {
            bail!(
                "{} has used its budget; raise it with `jeet manager budget` first",
                label(target)
            );
        }
    }
    if let Some(setter) = &target.run_state_by {
        if !outranks(app, by, setter)? {
            let who = setter_label(app, setter)?;
            bail!(
                "{} was {} by {who}; only {who} can resume it",
                label(target),
                target.run_state.as_str()
            );
        }
    }
    Ok(())
}

/// Resume `target` for `by`, if [`check_resume`] lets it: set it running,
/// hand each idle agent below it that runs again what queued for it while
/// held, then start what waits on `max_live`.
pub fn resume(app: &App, by: &Caller, target: &TaskRecord) -> Result<Vec<NodeOutcome>> {
    check_resume(app, by, target)?;
    let mut outcomes = Vec::new();
    if target.run_state == RunState::Run {
        outcomes.push(NodeOutcome::new(target, "was not paused or stopped"));
    } else {
        app.db.set_run_state(&target.id, RunState::Run, None)?;
        outcomes.push(NodeOutcome::new(target, "resumed"));
    }
    let live = live_set(app)?;
    for node in subtree_leaves_first(app, target)?.into_iter().rev() {
        if !live.contains(&node.id) || hold(app, &node)?.is_some() {
            continue;
        }
        if !matches!(runtime::activity(app, &node, true), Activity::Idle { .. }) {
            continue;
        }
        let handed = Inbox::lock(&tasks::task_dir(app, &node))
            .and_then(|inbox| messages::hand_over_queue(&inbox, &TmuxAgent::new(app, &node)));
        match handed {
            Ok(true) => outcomes.push(NodeOutcome::new(&node, "handed what queued while held")),
            Ok(false) => {}
            Err(e) => outcomes.push(NodeOutcome::new(
                &node,
                format!("could not hand over what queued: {e:#}"),
            )),
        }
    }
    if let Err(e) = drain(app) {
        outcomes.push(NodeOutcome::new(
            target,
            format!("could not start what waits on max_live: {e:#}"),
        ));
    }
    Ok(outcomes)
}

/// Stop `target` for `by`, or with `None` (`--all`, the user's alone) every
/// active child of the assistant as the user (M18): write each row, then end
/// the agent of every node below, leaves first, and forget its pane. A
/// stopped node is never started again until resumed. Then start what
/// waits on `max_live`.
pub fn stop(app: &App, by: &Caller, target: Option<&TaskRecord>) -> Result<Vec<NodeOutcome>> {
    let (targets, setter) = match target {
        Some(target) => (vec![target.clone()], setter_of(by)),
        None => {
            if !matches!(by, Caller::User) {
                bail!("only the user stops every task under the assistant");
            }
            let root = super::root(app)?;
            let children = app
                .db
                .children(&root.id)?
                .into_iter()
                .filter(|c| c.state == TaskState::Active)
                .collect();
            (children, Setter::User)
        }
    };
    for target in &targets {
        app.db
            .set_run_state(&target.id, RunState::Stopped, Some(&setter))?;
    }
    let live = live_set(app)?;
    let mut outcomes = Vec::new();
    for target in &targets {
        for node in subtree_leaves_first(app, target)? {
            let line = end_line(app, &node, live.contains(&node.id));
            outcomes.push(NodeOutcome::new(&node, line));
        }
    }
    if let Err(e) = drain(app) {
        let line = format!("could not start what waits on max_live: {e:#}");
        match targets.first() {
            Some(first) => outcomes.push(NodeOutcome::new(first, line)),
            None => eprintln!("jeet: {line}"),
        }
    }
    Ok(outcomes)
}

/// End `node`'s agent for a stop, whether or not it was seen `live`,
/// saying what came of it.
fn end_line(app: &App, node: &TaskRecord, live: bool) -> String {
    if node.kind == TaskKind::Assistant {
        return "the assistant; not stopped".to_string();
    }
    if runtime::is_own_session(node) {
        return "this jeet runs in its session; stop it from outside".to_string();
    }
    // Before its session ends, whether or not its `Ended` hook gets to.
    if let Err(e) = status::fold_busy(app, node, now_secs()) {
        eprintln!("jeet: could not charge task {}'s budget: {e:#}", node.slug);
    }
    if let Err(e) = runtime::stop(node) {
        return format!("could not end its agent: {e:#}");
    }
    if let Err(e) = app.db.delete_task_anchors(&node.id, AnchorKind::Pane) {
        return format!("ended its agent, but could not forget its pane: {e:#}");
    }
    match live {
        true => "stopped its agent".to_string(),
        false => "not running".to_string(),
    }
}

/// The active tasks in the tree, below the assistant, whose tmux session
/// is live: what `max_live` counts, managers included, the assistant and
/// top-level tasks not. One `tmux list-sessions`; a session left open
/// after its agent exited (`remain-on-exit`) counts until it is replaced.
pub fn live_set(app: &App) -> Result<HashSet<String>> {
    let sessions: HashSet<String> = tmux::list_sessions().into_iter().collect();
    if sessions.is_empty() {
        return Ok(HashSet::new());
    }
    Ok(app
        .db
        .list_tasks(Some(TaskState::Active))?
        .into_iter()
        .filter(|t| t.parent_id.is_some() && sessions.contains(&runtime::session_name(t)))
        .map(|t| t.id)
        .collect())
}

/// Where the start lock is kept, beside the watch's.
const START_LOCK: &str = ".start.lock";

/// The start lock, held from [`admit_start`] until the start it admitted
/// is done, so two starts never both take the last `max_live` slot.
pub struct StartGuard {
    _file: File,
}

/// Take the start lock, waiting for whoever holds it: a start must not be
/// skipped, only made to wait. Not re-entrant: what holds a guard passes
/// it down rather than locking again.
fn start_lock(app: &App) -> Result<StartGuard> {
    std::fs::create_dir_all(&app.home).with_context(|| format!("create {}", app.home.display()))?;
    let path = app.home.join(START_LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(StartGuard { _file: file })
}

/// Whether a node may start now.
pub enum StartGate {
    /// Start it, holding this until it is started.
    Go(StartGuard),
    /// `live` of `max` nodes run already: it is queued to start when one
    /// ends.
    Queue { live: u32, max: u32 },
    /// This task, itself or above it, holds it: nothing starts.
    Held(TaskRecord),
}

/// Whether `task`'s agent may start now (M22), under the start lock: not
/// while it is held, nor, for a task in the tree, while `max_live` others
/// run, when it is queued (keeping its place if queued already). A task
/// running already, a top-level task and the assistant go. Whoever is
/// delivering to it holds its inbox lock first.
pub fn admit_start(app: &App, task: &TaskRecord) -> Result<StartGate> {
    let guard = start_lock(app)?;
    if let Some((holder, _)) = hold(app, task)? {
        return Ok(StartGate::Held(holder));
    }
    match full(app, task)? {
        Some((live, max)) => {
            app.db.queue_start(&task.id, now_secs())?;
            Ok(StartGate::Queue { live, max })
        }
        None => Ok(StartGate::Go(guard)),
    }
}

/// [`admit_start`] for the user opening `task` at a terminal, who asked
/// for it (M22): a held task is refused still, but one over `max_live`
/// goes, with how many run and the limit.
pub fn admit_over(app: &App, task: &TaskRecord) -> Result<(StartGuard, Option<(u32, u32)>)> {
    let guard = start_lock(app)?;
    if let Some((holder, _)) = hold(app, task)? {
        bail!(held_refusal(task, &holder));
    }
    Ok((guard, full(app, task)?))
}

/// Why `task`, held by `holder` (itself or above it), does not start.
pub fn held_refusal(task: &TaskRecord, holder: &TaskRecord) -> String {
    let under = match holder.id == task.id {
        true => String::new(),
        false => format!(" under {}", label(holder)),
    };
    format!(
        "{} is {}{under}; `jeet manager resume {}` first",
        label(task),
        holder.run_state.as_str(),
        holder.slug
    )
}

/// How many tasks in the tree run, and `max_live`, when starting `task`
/// would take one more than that: never for a task running already, a
/// top-level task or the assistant.
fn full(app: &App, task: &TaskRecord) -> Result<Option<(u32, u32)>> {
    if task.parent_id.is_none() || task.kind == TaskKind::Assistant {
        return Ok(None);
    }
    let live = live_set(app)?;
    if live.contains(&task.id) {
        return Ok(None);
    }
    let max = app.config.manager_policy()?.max_live;
    let n = live.len() as u32;
    Ok((n >= max).then_some((n, max)))
}

/// Tell `task`'s parent that its start waits on `max_live`, with `live` of
/// `max` running: what goes wrong is warned of, not failed on. Each queued
/// start is told, so no earlier report holds this one back.
pub fn report_queued(app: &App, task: &TaskRecord, live: u32, max: u32) {
    let body = format!("queued, {live}/{max} live: it starts when a task in the tree ends");
    if let Err(e) = reports::auto_status(app, task, StatusEvent::Queued, &body, i64::MAX, 0) {
        eprintln!(
            "jeet: could not report task {}'s queued start: {e:#}",
            task.slug
        );
    }
}

/// Start what waits on `max_live`, oldest first, while slots are free:
/// each under its inbox lock, then the start lock. A task held or gone
/// active stays queued or leaves the queue; one that fails to start is
/// warned of and stays queued. Nothing from a background job with no tmux
/// server to start on.
pub fn drain(app: &App) -> Result<()> {
    if !tmux::may_start_session() {
        return Ok(());
    }
    for task in app.db.queued_starts()? {
        if task.state != TaskState::Active {
            app.db.unqueue(&task.id)?;
            continue;
        }
        let inbox = Inbox::lock(&tasks::task_dir(app, &task))?;
        let guard = match admit_start(app, &task)? {
            StartGate::Go(guard) => guard,
            StartGate::Held(_) => continue,
            StartGate::Queue { .. } => break,
        };
        // Started meanwhile some other way: what queued waits for its Stop.
        if tmux::has_session(&runtime::session_name(&task)) {
            app.db.unqueue(&task.id)?;
            continue;
        }
        match start_queued(app, &task, &inbox, &guard) {
            Ok(()) => app.db.unqueue(&task.id)?,
            Err(e) => eprintln!("jeet: could not start queued task {}: {e:#}", task.slug),
        }
    }
    Ok(())
}

/// Start `task`, admitted under `guard`, with what queued in its inbox as
/// its first prompt.
fn start_queued(app: &App, task: &TaskRecord, inbox: &Inbox, guard: &StartGuard) -> Result<()> {
    if inbox.read_for(None)?.is_empty() {
        let running = runtime::ensure_running_admitted(app, task, None, None, guard)?;
        for warning in &running.warnings {
            eprintln!("jeet: {warning}");
        }
        return Ok(());
    }
    messages::start_with_queue(inbox, &TmuxAgent::new(app, task), Some(guard))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::config;
    use crate::db::Database;
    use crate::runtime::Running;

    /// An index in a tempdir with the assistant and, under it, manager `m`
    /// with worker `w1` and manager `m2`, which has worker `w2`.
    fn lab() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: config::load_or_create(&home).unwrap(),
            db: Database::open(&config::index_path(&home)).unwrap(),
            home,
        };
        let root = super::super::root(&app).unwrap();
        for (slug, kind, parent) in [
            ("m", TaskKind::Manager, root.id.as_str()),
            ("w1", TaskKind::Work, "m"),
            ("m2", TaskKind::Manager, "m"),
            ("w2", TaskKind::Work, "m2"),
        ] {
            app.db.insert_task(&task(slug, kind, parent)).unwrap();
        }
        (dir, app)
    }

    fn task(slug: &str, kind: TaskKind, parent: &str) -> TaskRecord {
        TaskRecord {
            id: slug.into(),
            slug: slug.into(),
            name: slug.into(),
            project: None,
            kind,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: Some(parent.into()),
            placement: placement::for_new(),
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        }
    }

    fn get(app: &App, id: &str) -> TaskRecord {
        app.db.get_task(id).unwrap().unwrap()
    }

    /// A runtime that records whom it interrupts, with the run state each
    /// had then, and answers as the test says.
    struct FakeRuntime {
        app_home: std::path::PathBuf,
        calls: RefCell<Vec<(String, RunState)>>,
        answer: fn(&str) -> Result<Interrupted>,
    }

    impl NodeRuntime for FakeRuntime {
        fn start(
            &self,
            _: &App,
            _: &TaskRecord,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Running> {
            bail!("the fake starts nothing")
        }

        fn interrupt(&self, app: &App, task: &TaskRecord) -> Result<Interrupted> {
            assert_eq!(app.home, self.app_home);
            let (state, _) = app.db.effective_run_state(&task.id).unwrap();
            self.calls.borrow_mut().push((task.slug.clone(), state));
            (self.answer)(&task.slug)
        }
    }

    fn fake(app: &App, answer: fn(&str) -> Result<Interrupted>) -> FakeRuntime {
        FakeRuntime {
            app_home: app.home.clone(),
            calls: RefCell::new(Vec::new()),
            answer,
        }
    }

    fn nobody(_: &TaskRecord) -> bool {
        false
    }

    #[test]
    fn a_pause_holds_the_row_first_then_interrupts_leaves_first() {
        let (_dir, app) = lab();
        let rt = fake(&app, |_| Ok(Interrupted::Sent));
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        let m = get(&app, "m");
        let outcomes = pause_with(&app, &Caller::Root, &m, &lookup, &nobody).unwrap();
        // Each node was held already when it was interrupted.
        let calls = rt.calls.borrow().clone();
        let paused = |s: &str| (s.to_string(), RunState::Paused);
        assert_eq!(
            calls,
            [paused("w2"), paused("m2"), paused("w1"), paused("m")]
        );
        let slugs: Vec<&str> = outcomes.iter().map(|o| o.slug.as_str()).collect();
        assert_eq!(slugs, ["w2", "m2", "w1", "m"]);
        assert!(
            outcomes.iter().all(|o| o.line == "interrupted"),
            "{outcomes:?}"
        );
        let m = get(&app, "m");
        assert_eq!(
            (m.run_state, m.run_state_by),
            (RunState::Paused, Some(Setter::Root))
        );
        // Only the target's row is written; the rest are held by it.
        assert_eq!(get(&app, "w2").run_state, RunState::Run);
        let (holder, state) = hold(&app, &get(&app, "w2")).unwrap().unwrap();
        assert_eq!((holder.slug.as_str(), state), ("m", RunState::Paused));
    }

    #[test]
    fn a_pause_goes_on_past_what_a_node_refuses_or_fails_at() {
        let (_dir, app) = lab();
        let rt = fake(&app, |slug| match slug {
            "w2" => Ok(Interrupted::NotWorking(Activity::NotRunning)),
            "m2" => Ok(Interrupted::Watched),
            "w1" => bail!("no interrupt keys"),
            _ => Ok(Interrupted::Sent),
        });
        let lookup = |t: &TaskRecord| match t.slug.as_str() {
            "m" => bail!("no runtime here"),
            _ => Ok(&rt as &dyn NodeRuntime),
        };
        let outcomes = pause_with(&app, &Caller::User, &get(&app, "m"), &lookup, &nobody).unwrap();
        let lines: Vec<(&str, &str)> = outcomes
            .iter()
            .map(|o| (o.slug.as_str(), o.line.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("w2", "not working (not running); nothing to interrupt"),
                (
                    "m2",
                    "not interrupted: it is asking, and someone is attached to answer"
                ),
                ("w1", "not interrupted: no interrupt keys"),
                ("m", "not interrupted: no runtime here"),
            ]
        );
        assert_eq!(rt.calls.borrow().len(), 3);
    }

    #[test]
    fn a_pause_skips_the_assistant_and_this_jeets_own_session() {
        let (_dir, app) = lab();
        let rt = fake(&app, |_| Ok(Interrupted::Sent));
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        let own = |t: &TaskRecord| t.slug == "w1";
        let outcomes = pause_with(&app, &Caller::User, &get(&app, "m"), &lookup, &own).unwrap();
        let interrupted: Vec<String> = rt.calls.borrow().iter().map(|c| c.0.clone()).collect();
        assert_eq!(interrupted, ["w2", "m2", "m"]);
        let w1 = outcomes.iter().find(|o| o.slug == "w1").unwrap();
        assert!(w1.line.contains("runs in its session"), "{w1:?}");

        // A walk that reaches the assistant leaves it alone.
        let root = super::super::root(&app).unwrap();
        let lines = pause_with(&app, &Caller::User, &root, &lookup, &nobody).unwrap();
        assert_eq!(lines.last().unwrap().line, "the assistant; not interrupted");
        assert_eq!(
            rt.calls
                .borrow()
                .iter()
                .filter(|c| c.0 == root.slug)
                .count(),
            0
        );
    }

    #[test]
    fn an_interrupted_node_is_marked_idle_and_paused() {
        let (_dir, app) = lab();
        let w2 = get(&app, "w2");
        let dir = tasks::task_dir(&app, &w2);
        // w2 was mid-turn, w1 has no session yet.
        let running = status::SessionStatus {
            session_id: "s".into(),
            state: status::State::Running,
            detail: None,
            event: "UserPromptSubmit".into(),
            at: 1,
            busy_since: None,
        };
        let path = status::status_dir(&dir).join("s.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&running).unwrap()).unwrap();
        let rt = fake(&app, |_| Ok(Interrupted::Sent));
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        pause_with(&app, &Caller::User, &get(&app, "m2"), &lookup, &nobody).unwrap();
        let now = status::current_status(&dir).unwrap();
        assert_eq!(now.session_id, "s");
        assert_eq!(now.state, status::State::Idle);
        assert_eq!(now.event, status::PAUSED_EVENT);
        assert_eq!(now.detail.as_deref(), Some("paused"));
        assert!(status::current_status(&tasks::task_dir(&app, &get(&app, "m2"))).is_none());
    }

    #[test]
    fn a_child_born_under_a_paused_manager_is_held() {
        let (_dir, app) = lab();
        app.db
            .set_run_state("m2", RunState::Paused, Some(&Setter::Root))
            .unwrap();
        app.db
            .insert_task(&task("w3", TaskKind::Work, "m2"))
            .unwrap();
        let (holder, state) = hold(&app, &get(&app, "w3")).unwrap().unwrap();
        assert_eq!((holder.slug.as_str(), state), ("m2", RunState::Paused));
        // A start is held too, before any count of what runs.
        match admit_start(&app, &get(&app, "w3")).unwrap() {
            StartGate::Held(holder) => assert_eq!(holder.slug, "m2"),
            _ => panic!("a held child started"),
        }
        assert!(hold(&app, &get(&app, "w1")).unwrap().is_none());
    }

    #[test]
    fn resume_is_refused_under_a_held_ancestor_and_with_the_budget_spent() {
        let (_dir, app) = lab();
        let by = Setter::Node("m".into());
        app.db
            .set_run_state("m2", RunState::Paused, Some(&by))
            .unwrap();
        app.db
            .set_run_state("m", RunState::Paused, Some(&Setter::Root))
            .unwrap();
        let err = resume(&app, &Caller::Root, &get(&app, "m2")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "manager m2 is under manager m, which is paused; resume that first"
        );
        assert_eq!(get(&app, "m2").run_state, RunState::Paused);

        app.db.set_run_state("m", RunState::Run, None).unwrap();
        app.db.set_budget_max("m2", None, Some(10)).unwrap();
        app.db.charge("w2", 0, 10).unwrap();
        let err = resume(&app, &Caller::Root, &get(&app, "m2")).unwrap_err();
        assert!(err.to_string().contains("used its budget"), "{err}");
        app.db.set_budget_max("m2", None, Some(20)).unwrap();
        check_resume(&app, &Caller::Root, &get(&app, "m2")).unwrap();
    }

    #[test]
    fn resume_needs_a_caller_who_ranks_with_the_setter() {
        let (_dir, app) = lab();
        let m = Caller::Node(get(&app, "m"));
        let m2 = Caller::Node(get(&app, "m2"));
        let may = |setter: Setter, by: &Caller| {
            app.db.set_run_state("m2", RunState::Run, None).unwrap();
            app.db
                .set_run_state("m2", RunState::Paused, Some(&setter))
                .unwrap();
            check_resume(&app, by, &get(&app, "m2")).map_err(|e| e.to_string())
        };
        let err = may(Setter::User, &Caller::Root).unwrap_err();
        assert_eq!(
            err,
            "manager m2 was paused by the user; only the user can resume it"
        );
        assert!(may(Setter::User, &m).is_err());
        assert!(may(Setter::User, &Caller::User).is_ok());
        let err = may(Setter::Root, &m).unwrap_err();
        assert!(
            err.contains("by the assistant; only the assistant"),
            "{err}"
        );
        assert!(may(Setter::Root, &Caller::Root).is_ok());
        assert!(may(Setter::Node("m".into()), &m).is_ok());
        assert!(may(Setter::Node("m".into()), &Caller::Root).is_ok());
        // A node below may not undo what the node above it set.
        let err = may(Setter::Node("m".into()), &m2).unwrap_err();
        assert!(err.contains("by manager m; only manager m"), "{err}");
        // The budget ranks lowest: anyone in scope resumes once it is raised.
        app.db.set_budget_max("m2", None, Some(1)).unwrap();
        app.db.charge("w2", 0, 1).unwrap();
        assert!(may(Setter::Budget, &m).unwrap_err().contains("budget"));
        app.db.set_budget_max("m2", None, Some(5)).unwrap();
        assert!(may(Setter::Budget, &m).is_ok());
        assert!(may(Setter::Budget, &m2).is_ok());
    }

    #[test]
    fn a_stop_of_everything_is_the_users_alone() {
        let (_dir, app) = lab();
        for by in [Caller::Root, Caller::Node(get(&app, "m"))] {
            let err = stop(&app, &by, None).unwrap_err();
            assert!(err.to_string().contains("only the user"), "{err}");
        }
        assert_eq!(get(&app, "m").run_state, RunState::Run);
    }

    /// Run `f` on the index at `home` in a thread of its own, failing
    /// rather than hanging when it does not finish: a lock taken twice in
    /// one process waits for good.
    fn finishes(home: &std::path::Path, f: fn(&App)) {
        let home = home.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let app = App {
                config: config::load_or_create(&home).unwrap(),
                db: Database::open(&config::index_path(&home)).unwrap(),
                home,
            };
            f(&app);
            tx.send(()).unwrap();
        });
        let done = rx.recv_timeout(std::time::Duration::from_secs(20));
        assert!(done.is_ok(), "did not finish: a lock was taken twice");
    }

    #[test]
    fn a_delivery_that_starts_an_agent_takes_the_start_lock_once() {
        let (dir, _app) = lab();
        finishes(dir.path(), |app| {
            let w1 = get(app, "w1");
            let task_dir = tasks::task_dir(app, &w1);
            // Under its inbox lock, through `TmuxAgent::start`, the start
            // admits it and starts it holding the start lock: so far as
            // tmux, which unit tests never reach.
            let err = messages::deliver(&task_dir, &TmuxAgent::new(app, &w1), "hi").unwrap_err();
            assert!(err.to_string().contains(tmux::INSTALL_HINT), "{err:#}");
            // Neither lock is left held.
            assert!(matches!(admit_start(app, &w1).unwrap(), StartGate::Go(_)));
            Inbox::lock(&task_dir).unwrap();
        });
    }

    #[test]
    fn a_drain_starts_queued_tasks_under_locks_it_takes_once() {
        let (dir, app) = lab();
        app.db.queue_start("w1", 1).unwrap();
        app.db.queue_start("w2", 2).unwrap();
        let w1_dir = tasks::task_dir(&app, &get(&app, "w1"));
        Inbox::lock(&w1_dir)
            .unwrap()
            .push_to("from m", None)
            .unwrap();
        // One with messages to start with, one without; neither starts with
        // no tmux, so both stay queued, and w1's message waits still.
        finishes(dir.path(), |app| drain(app).unwrap());
        let queued: Vec<String> = app
            .db
            .queued_starts()
            .unwrap()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(queued, ["w1", "w2"]);
        let left = Inbox::lock(&w1_dir).unwrap().read_for(None).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "from m");
    }

    #[test]
    fn a_held_task_does_not_start_even_for_the_user() {
        let (_dir, app) = lab();
        app.db
            .set_run_state("m", RunState::Paused, Some(&Setter::User))
            .unwrap();
        let w2 = get(&app, "w2");
        for admission in [runtime::Admission::Gate, runtime::Admission::Bypass] {
            let started = runtime::ensure_running(&app, &w2, None, Some("go"), admission);
            assert_eq!(
                started.err().unwrap().to_string(),
                "task w2 is paused under manager m; `jeet manager resume m` first"
            );
        }
        // Held is not queued.
        assert!(app.db.queued_starts().unwrap().is_empty());
    }
}
