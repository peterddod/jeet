//! Whether a node may run, and what changes it: pause, resume and stop over
//! a subtree (M15-M18), and the one gate every start of a node in the tree
//! goes through, `[managers] max_live` (M22).
//!
//! A node's run state is written on its own row only, with who set it; what
//! holds it is the most severe state over it and its ancestors
//! ([`hold`]). A held node starts nothing and is handed nothing.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};

use anyhow::{bail, Context, Result};

use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{RunState, Setter, TaskKind, TaskRecord, TaskState};
use crate::messages::{self, Agent, Inbox, Route, TmuxAgent};
use crate::permissions::Caller;
use crate::placement::{self, NodeRuntime};
use crate::reports::{self, StatusEvent};
use crate::review;
use crate::runtime::{self, Interrupted};
use crate::status::{self, Activity};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

use super::budget::{self, Threshold};
use super::label;

/// The runtime that interrupts a node: [`placement::runtime_for`], or a
/// fake in tests.
pub type RuntimeFor<'a> = &'a dyn Fn(&TaskRecord) -> Result<&'a dyn NodeRuntime>;

/// Whether this jeet runs in a node's own session, which it may not
/// interrupt or end: [`runtime::is_own_session`], or a stand-in in tests.
pub type OwnSession<'a> = &'a dyn Fn(&TaskRecord) -> bool;

/// End a node's agent for a stop: [`end_agent`], or a stand-in in tests.
pub type EndAgent<'a> = &'a dyn Fn(&App, &TaskRecord) -> Result<String>;

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
        // A review's agent runs no `jeet manager` verb; were it to, it
        // would hold nothing above a budget's pause.
        Caller::Review(_) => Setter::Budget,
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
        (Caller::Review(_), _) => false,
        (Caller::Node(_), Setter::Root) => false,
        (_, Setter::Budget) => true,
        (Caller::Node(me), Setter::Node(id)) => {
            *id == me.id || app.db.is_strict_descendant(id, &me.id)?
        }
    })
}

/// Who `setter` is, for a refusal: `the user`, `manager backend`.
pub fn setter_label(app: &App, setter: &Setter) -> Result<String> {
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
    // A start admitted before the row was written is done before the walk.
    drop(start_lock(app)?);
    for node in tasks::subtree_leaves_first(app, target)? {
        let line = interrupt_line(app, &node, rt, own);
        outcomes.push(NodeOutcome::new(&node, line));
    }
    Ok(outcomes)
}

/// How long a pause waits to interrupt again a turn about to start.
const INTERRUPT_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

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
    let dir = tasks::task_dir(app, node);
    let interrupted = loop {
        // Under its inbox lock: a delivery that found it not held yet has
        // typed its message in, and is interrupted with the rest. Not for a
        // directory gone: the lock would make it again, and a later start
        // would run the agent in an empty one.
        let inbox = dir.is_dir().then(|| Inbox::lock(&dir).ok()).flatten();
        let interrupted = rt(node).and_then(|r| r.interrupt(app, node));
        drop(inbox);
        // Just launched with its brief, or just typed a message, and no
        // hook has said its turn began: it is about to, so the interrupt
        // is tried again until one does, or it would have by now.
        let not_yet = matches!(
            interrupted,
            Ok(Interrupted::NotWorking(
                Activity::Starting | Activity::Idle { .. }
            )) | Ok(Interrupted::AtStartup)
        );
        if !(not_yet && status::input_unreported(&dir, now_secs())) {
            break interrupted;
        }
        std::thread::sleep(INTERRUPT_RETRY);
    };
    match interrupted {
        Ok(Interrupted::Sent) => {
            charge_interrupted(app, node);
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

/// Charge `node` the turn jeet just interrupted: it ends with no Stop hook
/// to charge it, or to report a charge that uses its budget up, so that is
/// done and told here. Its status is the caller's to close. Only for an
/// interrupt `Sent`, which needs the agent's own pane alive.
pub fn charge_interrupted(app: &App, node: &TaskRecord) {
    match status::fold_busy(app, node, true, now_secs()) {
        Ok(Threshold::Hit100) => budget::report_blocked(app, node),
        Ok(_) => {}
        Err(e) => eprintln!("jeet: could not charge task {}'s budget: {e:#}", node.slug),
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
/// unless its row changed since it was read; hand each agent below it that
/// runs again, and would take a message typed in now, what queued for it
/// while held; then start what waits on `max_live` or waited on the
/// resume: a node a message held would have started, or might have once
/// its agent ended.
pub fn resume(app: &App, by: &Caller, target: &TaskRecord) -> Result<Vec<NodeOutcome>> {
    check_resume(app, by, target)?;
    let mut outcomes = Vec::new();
    if target.run_state == RunState::Run {
        outcomes.push(NodeOutcome::new(target, "was not paused or stopped"));
    } else {
        // What was checked is what is undone: a pause or stop written
        // since, by someone who may outrank `by`, stays.
        let checked = (target.run_state, target.run_state_by.as_ref());
        if !app.db.resume_run_state(&target.id, checked)? {
            bail!(
                "{} was paused or stopped again meanwhile, or used its budget; look with \
                 `jeet manager show {}`",
                label(target),
                target.slug
            );
        }
        outcomes.push(NodeOutcome::new(target, "resumed"));
    }
    let live = live_set(app)?;
    for node in tasks::subtree_leaves_first(app, target)?.into_iter().rev() {
        if node.kind == TaskKind::Assistant || node.state != TaskState::Active {
            continue;
        }
        if hold(app, &node)?.is_some() {
            continue;
        }
        // Not running: what was held from starting it had its start queued
        // ([`messages::Agent::queue_held_start`]), for the drain below; a
        // report, which never starts it, waits for its next start.
        if !live.contains(&node.id) {
            continue;
        }
        let agent = TmuxAgent::new(app, &node);
        // Under its inbox lock: a pause since, whose walk takes that lock,
        // holds it; and a Stop hook that read it held before the resume
        // has written its status by then, so its turn's end is seen here.
        let handed = Inbox::lock(&tasks::task_dir(app, &node)).and_then(|inbox| {
            if hold(app, &node)?.is_some() {
                return Ok(false);
            }
            // As a delivery would type it in: to an idle agent, or to a
            // running one with no hooks to queue for.
            let activity = runtime::activity(app, &node, true);
            if messages::route(&activity, agent.has_hooks()) != Route::Type {
                return Ok(false);
            }
            messages::hand_over_queue(&inbox, &agent)
        });
        match handed {
            Ok(true) => outcomes.push(NodeOutcome::new(&node, "handed what queued while held")),
            Ok(false) => {}
            Err(e) => outcomes.push(NodeOutcome::new(
                &node,
                format!("could not hand over what queued: {e:#}"),
            )),
        }
    }
    match drain(app) {
        Ok(warnings) => outcomes.extend(warnings.into_iter().map(|w| NodeOutcome::new(target, w))),
        Err(e) => outcomes.push(NodeOutcome::new(
            target,
            format!("could not start what waits on max_live: {e:#}"),
        )),
    }
    Ok(outcomes)
}

/// Stop `target` for `by`, or with `None` every active child of the
/// assistant: [`stop_with`] the agents' own ends.
pub fn stop(app: &App, by: &Caller, target: Option<&TaskRecord>) -> Result<Vec<NodeOutcome>> {
    stop_with(app, by, target, &end_agent, &runtime::is_own_session)
}

/// Stop `target` for `by`, or with `None` (`--all`, the user's alone) every
/// active child of the assistant as the user (M18): write each row, then end
/// the agent of every node below, leaves first, through `end`, and forget
/// its pane. The assistant and the node whose session this jeet runs in are
/// skipped. `--all` also ends the agents and jobs of the reviews the
/// assistant asked for, which are in no tree. A stopped node is never started again until resumed. Then
/// start what waits on `max_live`.
pub fn stop_with(
    app: &App,
    by: &Caller,
    target: Option<&TaskRecord>,
    end: EndAgent,
    own: OwnSession,
) -> Result<Vec<NodeOutcome>> {
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
    // A start admitted before the rows were written is done, and its
    // session seen, before the walk; one after sees the stop.
    drop(start_lock(app)?);
    let live = live_set(app)?;
    let mut outcomes = Vec::new();
    for target in &targets {
        for node in tasks::subtree_leaves_first(app, target)? {
            let line = end_line(app, &node, live.contains(&node.id), end, own);
            outcomes.push(NodeOutcome::new(&node, line));
        }
    }
    // `--all` is every review the assistant asked for too: they are not in
    // its tree, but they are its doing, and a kill switch that left them
    // running would not be one.
    if target.is_none() {
        outcomes.extend(stop_assistant_reviews(app)?);
    }
    let lines = match drain(app) {
        Ok(warnings) => warnings,
        Err(e) => vec![format!("could not start what waits on max_live: {e:#}")],
    };
    for line in lines {
        match targets.first() {
            Some(first) => outcomes.push(NodeOutcome::new(first, line)),
            None => eprintln!("jeet: {line}"),
        }
    }
    Ok(outcomes)
}

/// End the agent and the legacy jobs of every active review the assistant
/// asked for, and drop them from the `[review] max_live` queue first, so
/// the drain that follows does not start them.
fn stop_assistant_reviews(app: &App) -> Result<Vec<NodeOutcome>> {
    let root = super::root(app)?;
    let mut outcomes = Vec::new();
    for review in app.db.list_reviews(Some(TaskState::Active))? {
        if review.requested_by.as_deref() != Some(root.id.as_str()) {
            continue;
        }
        app.db.unqueue_review(&review.id)?;
        let line = match review::stop(app, &review) {
            Ok(line) => line,
            Err(e) => format!("could not end its agent: {e:#}"),
        };
        outcomes.push(NodeOutcome {
            slug: review.slug,
            line,
        });
    }
    Ok(outcomes)
}

/// End `node`'s agent for a stop, whether or not it was seen `live`,
/// through `end`, saying what came of it.
fn end_line(app: &App, node: &TaskRecord, live: bool, end: EndAgent, own: OwnSession) -> String {
    if node.kind == TaskKind::Assistant {
        return "the assistant; not stopped".to_string();
    }
    if own(node) {
        return "this jeet runs in its session; stop it from outside".to_string();
    }
    // Before its session ends, whether or not its `Ended` hook gets to; a
    // stale status of an agent already gone is no busy time. A charge that
    // uses its budget up is not reported: a stopped node's end is not.
    if let Err(e) = status::fold_busy(app, node, live, now_secs()) {
        eprintln!("jeet: could not charge task {}'s budget: {e:#}", node.slug);
    }
    let ended = match end(app, node) {
        Ok(ended) => ended,
        Err(e) => return format!("could not end its agent: {e:#}"),
    };
    if let Err(e) = app.db.delete_task_anchors(&node.id, AnchorKind::Pane) {
        return format!("ended its agent, but could not forget its pane: {e:#}");
    }
    match (live, ended.is_empty()) {
        (true, true) => "stopped its agent".to_string(),
        (true, false) => format!("stopped its agent; {ended}"),
        (false, true) => "not running".to_string(),
        (false, false) => ended,
    }
}

/// End `node`'s agent for a stop: its tmux session. Says what else it
/// ended, if anything.
pub fn end_agent(_app: &App, node: &TaskRecord) -> Result<String> {
    runtime::stop(node)?;
    Ok(String::new())
}

/// The active tasks in the tree, below the assistant, whose agent's pane
/// is live, managers included, the assistant and top-level tasks not: what
/// `[managers] max_live` counts. A session left open after its agent
/// exited, by `remain-on-exit` or a window the user opened there, does not
/// count.
pub fn live_set(app: &App) -> Result<HashSet<String>> {
    Ok(live_tasks(app)?
        .into_iter()
        .filter(|t| t.parent_id.is_some())
        .map(|t| t.id)
        .collect())
}

/// The active tasks, top-level ones included, whose agent's pane is live:
/// one `tmux list-sessions`, then a look at the agent's pane of each
/// task's session in it.
fn live_tasks(app: &App) -> Result<Vec<TaskRecord>> {
    let sessions: HashSet<String> = tmux::list_sessions().into_iter().collect();
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    Ok(app
        .db
        .list_tasks(Some(TaskState::Active))?
        .into_iter()
        .filter(|t| {
            let name = runtime::session_name(t);
            sessions.contains(&name) && tmux::agent_alive(&name)
        })
        .collect())
}

/// The `max_live` a start of a task counts toward, and what frees a slot
/// under it.
pub fn max_live_of() -> (&'static str, &'static str) {
    ("[managers] max_live", "a task in the tree ends")
}

/// The `max_live` a start of a review counts toward, and what frees a slot
/// under it: review agents are counted apart from the tree.
pub const REVIEW_MAX_LIVE: (&str, &str) = ("[review] max_live", "a review ends");

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
pub fn start_lock(app: &App) -> Result<StartGuard> {
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
    /// ends; `fresh` when it was not queued already, and its parent is
    /// still to be told.
    Queue { live: u32, max: u32, fresh: bool },
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
            let fresh = app.db.queue_start(&task.id, now_secs())?;
            Ok(StartGate::Queue { live, max, fresh })
        }
        None => Ok(StartGate::Go(guard)),
    }
}

/// [`admit_start`] for a review's agent: not while it is paused (it has
/// used its `[review]` time or turns), nor while `[review] max_live` review
/// agents run, when it is queued, keeping its place if queued already. One
/// running already goes. Whoever is delivering to it holds its inbox lock
/// first. A pause is not returned as held: the delivery asks
/// [`messages::Agent::held`] first.
pub fn admit_review(app: &App, review: &ReviewRecord) -> Result<StartGate> {
    let guard = start_lock(app)?;
    match full_reviews(app, review)? {
        Some((live, max)) => {
            let fresh = app.db.queue_review(&review.id, now_secs())?;
            Ok(StartGate::Queue { live, max, fresh })
        }
        None => Ok(StartGate::Go(guard)),
    }
}

/// [`admit_review`] for the user opening the review at a terminal, who asked
/// for it: one over `[review] max_live` goes, with how many run and the
/// limit.
pub fn admit_review_over(
    app: &App,
    review: &ReviewRecord,
) -> Result<(StartGuard, Option<(u32, u32)>)> {
    let guard = start_lock(app)?;
    Ok((guard, full_reviews(app, review)?))
}

/// The review agents whose pane is live: one `tmux list-sessions`, then a
/// look at the agent's pane of each active review's session in it.
pub fn live_reviews(app: &App) -> Result<Vec<ReviewRecord>> {
    let sessions: HashSet<String> = tmux::list_sessions().into_iter().collect();
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    Ok(app
        .db
        .list_reviews(Some(TaskState::Active))?
        .into_iter()
        .filter(|r| {
            let name = review::session_name(r);
            sessions.contains(&name) && tmux::agent_alive(&name)
        })
        .collect())
}

/// How many review agents run, and `[review] max_live`, when starting
/// `review` would take one more than that: never for one running already.
fn full_reviews(app: &App, review: &ReviewRecord) -> Result<Option<(u32, u32)>> {
    let live = live_reviews(app)?;
    if live.iter().any(|r| r.id == review.id) {
        return Ok(None);
    }
    let max = app.config.review.max_live()?;
    let n = live.len() as u32;
    Ok((n >= max).then_some((n, max)))
}

/// Tell whoever asked for `review` that its start waits on `[review]
/// max_live`, with `live` of `max` running, unless they were told at or
/// after `since`; see [`report_queued`]. Returns whether they were told
/// now.
pub fn report_review_queued(
    app: &App,
    review: &ReviewRecord,
    live: u32,
    max: u32,
    since: i64,
) -> bool {
    let (cap, frees) = REVIEW_MAX_LIVE;
    let body = format!("queued on {cap}, {live}/{max} live: it starts when {frees}");
    match reports::auto_status_review(app, review, StatusEvent::Queued, &body, since, 0) {
        Ok(told) => told.is_some(),
        Err(e) => {
            eprintln!(
                "jeet: could not report review {}'s queued start: {e:#}",
                review.slug
            );
            false
        }
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

/// How many run, and `[managers] max_live`, when starting `task` would
/// take one more than that: never for a task running already, the
/// assistant, or a top-level task.
fn full(app: &App, task: &TaskRecord) -> Result<Option<(u32, u32)>> {
    if task.kind == TaskKind::Assistant || task.parent_id.is_none() {
        return Ok(None);
    }
    let live = live_tasks(app)?;
    if live.iter().any(|t| t.id == task.id) {
        return Ok(None);
    }
    let max = app.config.manager_policy()?.max_live;
    let n = counted(&live);
    Ok((n >= max).then_some((n, max)))
}

/// Of the `live` tasks, how many count toward `[managers] max_live`: those
/// in the tree, below the assistant.
fn counted(live: &[TaskRecord]) -> u32 {
    live.iter().filter(|t| t.parent_id.is_some()).count() as u32
}

/// Tell `task`'s parent that its start waits on `max_live`, with `live` of
/// `max` running, unless it was told at or after `since`: what goes wrong
/// is warned of, not failed on. A start queued just now passes `i64::MAX`,
/// so no earlier report holds it back; a drain passes when the start was
/// queued, so one a hold queued, and nobody told, is told once. Returns
/// whether it was told now.
pub fn report_queued(app: &App, task: &TaskRecord, live: u32, max: u32, since: i64) -> bool {
    let (cap, frees) = max_live_of();
    let body = format!("queued on {cap}, {live}/{max} live: it starts when {frees}");
    match reports::auto_status(app, task, StatusEvent::Queued, &body, since, 0) {
        Ok(told) => told.is_some(),
        Err(e) => {
            eprintln!(
                "jeet: could not report task {}'s queued start: {e:#}",
                task.slug
            );
            false
        }
    }
}

/// Start what waits on `max_live`, oldest first, while slots are free:
/// each under its inbox lock, then the start lock. A task held or gone
/// active stays queued or leaves the queue; one that fails to start stays
/// queued. Nothing from a background job with no tmux server to start on.
/// Once the tree is full nothing more starts, but a task found running and
/// not held still leaves the queue, wherever it sits, and one that waits
/// on `max_live` with its parent not told so (its start was queued by a
/// hold, which said nothing of `max_live`) is told now, once (M22).
/// Returns what to warn of, for the caller to show where it can (the TUI's
/// status line, not stderr).
pub fn drain(app: &App) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if !tmux::may_start_session() {
        return Ok(warnings);
    }
    // `live` of `max` once the tree is found full.
    let mut full: Option<(u32, u32)> = None;
    for task in app.db.queued_starts()? {
        if task.state != TaskState::Active {
            app.db.unqueue(&task.id)?;
            continue;
        }
        // Its directory gone: taking its inbox lock would make it again,
        // and the agent would start in an empty one. Its start is dropped,
        // its inbox (if the directory comes back) left for its next start.
        let dir = tasks::task_dir(app, &task);
        if !dir.is_dir() {
            app.db.unqueue(&task.id)?;
            warnings.push(format!(
                "did not start queued task {}: its directory {} is missing",
                task.slug,
                dir.display()
            ));
            continue;
        }
        // Running, and resumed: the start a held message queued in case
        // its agent ended first is not needed. Left behind a row a full
        // tree keeps, it would start the agent again once that one ended.
        if tmux::pane_alive(&runtime::session_name(&task)) && hold(app, &task)?.is_none() {
            app.db.unqueue(&task.id)?;
            continue;
        }
        if let Some((live, max)) = full {
            if hold(app, &task)?.is_none() {
                tell_queued(app, &task, live, max, &mut warnings)?;
            }
            continue;
        }
        let inbox = Inbox::lock(&dir)?;
        let guard = match admit_start(app, &task)? {
            StartGate::Go(guard) => guard,
            StartGate::Held(_) => continue,
            StartGate::Queue { live, max, .. } => {
                full = Some((live, max));
                tell_queued(app, &task, live, max, &mut warnings)?;
                continue;
            }
        };
        // Started meanwhile some other way: what queued waits for its Stop.
        if tmux::pane_alive(&runtime::session_name(&task)) {
            app.db.unqueue(&task.id)?;
            continue;
        }
        match runtime::start_with_inbox(app, &task, None, &inbox, &guard) {
            Ok(running) => {
                app.db.unqueue(&task.id)?;
                warnings.extend(running.warnings);
            }
            Err(e) => warnings.push(format!("could not start queued task {}: {e:#}", task.slug)),
        }
    }
    drain_reviews(app, &mut warnings)?;
    Ok(warnings)
}

/// [`drain`] for the reviews waiting on `[review] max_live`, which fill up
/// apart from the tree: oldest first, each under its inbox lock, then the
/// start lock. One archived leaves the queue; one paused, or that fails to
/// start, stays.
fn drain_reviews(app: &App, warnings: &mut Vec<String>) -> Result<()> {
    let mut full: Option<(u32, u32)> = None;
    for review in app.db.queued_reviews()? {
        if review.state != TaskState::Active {
            app.db.unqueue_review(&review.id)?;
            continue;
        }
        let dir = review::dir(app, &review);
        if !dir.is_dir() {
            app.db.unqueue_review(&review.id)?;
            warnings.push(format!(
                "did not start queued review {}: its directory {} is missing",
                review.slug,
                dir.display()
            ));
            continue;
        }
        let session = review::session_name(&review);
        if tmux::pane_alive(&session) && !review.paused {
            app.db.unqueue_review(&review.id)?;
            continue;
        }
        if let Some((live, max)) = full {
            if !review.paused {
                tell_review_queued(app, &review, live, max, warnings);
            }
            continue;
        }
        if review.paused {
            continue;
        }
        let inbox = Inbox::lock(&dir)?;
        let guard = match admit_review(app, &review)? {
            StartGate::Go(guard) => guard,
            StartGate::Held(_) => continue,
            StartGate::Queue { live, max, .. } => {
                full = Some((live, max));
                tell_review_queued(app, &review, live, max, warnings);
                continue;
            }
        };
        if tmux::pane_alive(&session) {
            app.db.unqueue_review(&review.id)?;
            continue;
        }
        match review::start_with_inbox(app, &review, None, &inbox, &guard) {
            Ok(running) => {
                app.db.unqueue_review(&review.id)?;
                warnings.extend(running.warnings);
            }
            Err(e) => warnings.push(format!(
                "could not start queued review {}: {e:#}",
                review.slug
            )),
        }
    }
    Ok(())
}

/// [`tell_queued`] for a review.
fn tell_review_queued(
    app: &App,
    review: &ReviewRecord,
    live: u32,
    max: u32,
    warnings: &mut Vec<String>,
) {
    let Some(since) = review.queued_at else {
        return;
    };
    if report_review_queued(app, review, live, max, since) {
        let (cap, frees) = REVIEW_MAX_LIVE;
        warnings.push(format!(
            "review {} is queued on {cap} ({live}/{max} live): it starts when {frees}",
            review.slug
        ));
    }
}

/// Tell `task`'s parent, for [`drain`], that its queued start waits on
/// `max_live`, unless it was told since the start was queued; and say so
/// among `warnings`, for whoever ran the drain (a resume, say), when told.
fn tell_queued(
    app: &App,
    task: &TaskRecord,
    live: u32,
    max: u32,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(since) = app.db.queued_at(&task.id)? else {
        return Ok(());
    };
    if report_queued(app, task, live, max, since) {
        let (cap, frees) = max_live_of();
        warnings.push(format!(
            "task {} is queued on {cap} ({live}/{max} live): it starts when {frees}",
            task.slug
        ));
    }
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

    /// The tree's `max_live` counts the tasks in the tree, below the
    /// assistant: not a top-level task.
    #[test]
    fn the_tree_counts_its_tasks_below_the_assistant() {
        let live = [
            task("w", TaskKind::Work, "a"),
            task("m", TaskKind::Manager, "a"),
            TaskRecord {
                parent_id: None,
                ..task("top", TaskKind::Work, "a")
            },
        ];
        assert_eq!(counted(&live), 2);
        assert_eq!(max_live_of().0, "[managers] max_live");
        assert_eq!(REVIEW_MAX_LIVE.0, "[review] max_live");
    }

    /// A review asked for by `by` (a task id, or none), in the index.
    fn review_by(app: &App, id: &str, number: u64, by: Option<&str>) {
        let mut review = ReviewRecord::new(
            id.into(),
            format!("review-api-{number}"),
            "github.com/acme/api",
            number,
            1,
        );
        review.requested_by = by.map(str::to_string);
        app.db.insert_review(&review).unwrap();
    }

    fn end_nothing(_: &App, _: &TaskRecord) -> Result<String> {
        Ok(String::new())
    }

    /// `jeet manager stop --all` ends the job of a review the assistant
    /// asked for, and not that of one the user did.
    #[cfg(unix)]
    #[test]
    fn a_stop_all_ends_the_job_of_a_review_the_assistant_asked_for() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let (_dir, app) = lab();
        let root = super::super::root(&app).unwrap();
        review_by(&app, "mine", 7, Some(&root.id));
        review_by(&app, "yours", 8, None);
        let spawn = |id: &str| {
            let job = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            crate::identity::record(&app.db, id, AnchorKind::Job, None, job.id()).unwrap();
            job
        };
        let (mut mine, mut yours) = (spawn("mine"), spawn("yours"));
        let outcomes = stop_with(&app, &Caller::User, None, &end_nothing, &nobody).unwrap();
        let line = outcomes
            .iter()
            .find(|o| o.slug == "review-api-7")
            .map(|o| o.line.as_str());
        assert_eq!(line, Some("stopped its review job"), "{outcomes:?}");
        assert!(outcomes.iter().all(|o| o.slug != "review-api-8"));
        // Ended, not left to run on; the user's runs.
        assert!(mine.try_wait().unwrap().is_some());
        assert!(yours.try_wait().unwrap().is_none());
        yours.kill().unwrap();
        let _ = yours.wait();
        // Still a review, not archived or deleted.
        assert!(app.db.get_review("mine").unwrap().is_some());
    }

    /// Stopped, an assistant's review no longer waits on `[review]
    /// max_live` for a slot; the user's still does.
    #[test]
    fn a_stop_all_drops_the_assistants_reviews_from_the_queue() {
        let (_dir, app) = lab();
        let root = super::super::root(&app).unwrap();
        review_by(&app, "mine", 7, Some(&root.id));
        review_by(&app, "yours", 8, None);
        app.db.queue_review("mine", 5).unwrap();
        app.db.queue_review("yours", 6).unwrap();
        // Short of the drain, which would start what is left.
        stop_assistant_reviews(&app).unwrap();
        let queued: Vec<String> = app
            .db
            .queued_reviews()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(queued, ["yours"]);
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

    /// A runtime whose node is starting for its first `starting` asks,
    /// then working.
    struct Starting {
        starting: usize,
        calls: std::cell::Cell<usize>,
    }

    impl NodeRuntime for Starting {
        fn start(
            &self,
            _: &App,
            _: &TaskRecord,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Running> {
            bail!("the fake starts nothing")
        }

        fn interrupt(&self, _: &App, _: &TaskRecord) -> Result<Interrupted> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            Ok(match n < self.starting {
                true => Interrupted::NotWorking(Activity::Starting),
                false => Interrupted::Sent,
            })
        }
    }

    #[test]
    fn a_pause_waits_for_the_turn_a_just_launched_agent_is_about_to_start() {
        let (_dir, app) = lab();
        let w1 = get(&app, "w1");
        // Launched with its brief on its command line, no hook yet.
        status::mark_launched(&tasks::task_dir(&app, &w1), true, true).unwrap();
        let rt = Starting {
            starting: 2,
            calls: std::cell::Cell::new(0),
        };
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        let outcomes = pause_with(&app, &Caller::User, &w1, &lookup, &nobody).unwrap();
        assert_eq!(outcomes, [NodeOutcome::new(&w1, "interrupted")]);
        assert_eq!(rt.calls.get(), 3);

        // Not just launched: starting, and left so at once.
        let w2 = get(&app, "w2");
        let rt = Starting {
            starting: 2,
            calls: std::cell::Cell::new(0),
        };
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        let outcomes = pause_with(&app, &Caller::User, &w2, &lookup, &nobody).unwrap();
        assert_eq!(
            outcomes,
            [NodeOutcome::new(
                &w2,
                "not working (starting); nothing to interrupt"
            )]
        );
        assert_eq!(rt.calls.get(), 1);
    }

    #[test]
    fn a_pause_waits_for_the_turn_a_wake_is_about_to_start() {
        let (_dir, app) = lab();
        let w2 = get(&app, "w2");
        // The wake channel just posted to it, idle: no hook yet.
        status::wake_running(&tasks::task_dir(&app, &w2), "s", now_secs()).unwrap();
        let rt = Starting {
            starting: 2,
            calls: std::cell::Cell::new(0),
        };
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        let outcomes = pause_with(&app, &Caller::User, &w2, &lookup, &nobody).unwrap();
        assert_eq!(outcomes, [NodeOutcome::new(&w2, "interrupted")]);
        assert_eq!(rt.calls.get(), 3);
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
    fn a_pause_leaves_a_missing_task_directory_missing() {
        let (_dir, app) = lab();
        let w1 = get(&app, "w1");
        let dir = tasks::task_dir(&app, &w1);
        assert!(!dir.exists());
        let rt = fake(&app, |_| Ok(Interrupted::NotWorking(Activity::NotRunning)));
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        pause_with(&app, &Caller::User, &w1, &lookup, &nobody).unwrap();
        // Still interrupted, but its inbox lock did not make it again.
        assert_eq!(rt.calls.borrow().len(), 1);
        assert!(!dir.exists());
    }

    #[test]
    fn a_pause_whose_charge_uses_a_budget_up_reports_it_blocked() {
        let (_dir, app) = lab();
        app.db.set_budget_max("w2", Some(60), None).unwrap();
        let dir = tasks::task_dir(&app, &get(&app, "w2"));
        // Mid-turn for two minutes, with a minute's budget.
        let running = status::SessionStatus {
            session_id: "s".into(),
            state: status::State::Running,
            detail: None,
            event: "UserPromptSubmit".into(),
            at: now_secs() - 120,
            busy_since: Some(now_secs() - 120),
        };
        let path = status::status_dir(&dir).join("s.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&running).unwrap()).unwrap();
        let rt = fake(&app, |_| Ok(Interrupted::Sent));
        let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
        pause_with(&app, &Caller::Root, &get(&app, "m2"), &lookup, &nobody).unwrap();
        // No Stop hook follows the interrupt to say so: the pause does.
        let report = app.db.last_status_report("w2").unwrap().unwrap();
        assert!(report.body.contains("blocked: budget"), "{}", report.body);
        assert_eq!(report.to_id, "m2");
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
        // A node above undoes what one below it set, however far below;
        // one beside it does not.
        assert!(may(Setter::Node("m2".into()), &m).is_ok());
        assert!(may(Setter::Node("w2".into()), &m).is_ok());
        assert!(may(Setter::Node("w2".into()), &m2).is_ok());
        let w1 = Caller::Node(get(&app, "w1"));
        let err = may(Setter::Node("m2".into()), &w1).unwrap_err();
        assert!(err.contains("by manager m2; only manager m2"), "{err}");
        assert!(may(Setter::Node("w2".into()), &w1).is_err());
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

    /// `--all` is the assistant's active children and what is below them:
    /// not a top-level task, nor an archived child.
    #[test]
    fn a_stop_of_everything_spares_top_level_tasks_and_archived_children() {
        let (_dir, app) = lab();
        let root = super::super::root(&app).unwrap();
        let mut top = task("t", TaskKind::Work, "");
        top.parent_id = None;
        app.db.insert_task(&top).unwrap();
        let mut old = task("old", TaskKind::Work, &root.id);
        old.state = TaskState::Archived;
        app.db.insert_task(&old).unwrap();
        let ended: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let end = |_: &App, t: &TaskRecord| -> Result<String> {
            ended.borrow_mut().push(t.slug.clone());
            Ok(String::new())
        };
        let outcomes = stop_with(&app, &Caller::User, None, &end, &nobody).unwrap();
        let slugs: Vec<&str> = outcomes.iter().map(|o| o.slug.as_str()).collect();
        assert_eq!(slugs, ["w2", "m2", "w1", "m"]);
        assert!(
            ended.borrow().iter().all(|s| s != "t" && s != "old"),
            "{:?}",
            ended.borrow()
        );
        let m = get(&app, "m");
        assert_eq!(
            (m.run_state, m.run_state_by),
            (RunState::Stopped, Some(Setter::User))
        );
        assert_eq!(get(&app, "t"), top);
        assert_eq!(get(&app, "old"), old);
    }

    #[test]
    fn a_stop_ends_leaves_first_and_skips_the_assistant_and_its_own_session() {
        let (_dir, app) = lab();
        // Whom it ends, with the run state each had then.
        let ended: RefCell<Vec<(String, RunState)>> = RefCell::new(Vec::new());
        let end = |app: &App, t: &TaskRecord| -> Result<String> {
            let (state, _) = app.db.effective_run_state(&t.id).unwrap();
            ended.borrow_mut().push((t.slug.clone(), state));
            match t.slug.as_str() {
                "m2" => bail!("no session here"),
                _ => Ok(String::new()),
            }
        };
        let own = |t: &TaskRecord| t.slug == "w1";
        let m = get(&app, "m");
        let outcomes = stop_with(&app, &Caller::Root, Some(&m), &end, &own).unwrap();
        // Each was held already when it was ended; w1 runs this jeet.
        let stopped = |s: &str| (s.to_string(), RunState::Stopped);
        assert_eq!(
            *ended.borrow(),
            [stopped("w2"), stopped("m2"), stopped("m")]
        );
        let lines: Vec<(&str, &str)> = outcomes
            .iter()
            .map(|o| (o.slug.as_str(), o.line.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("w2", "not running"),
                ("m2", "could not end its agent: no session here"),
                ("w1", "this jeet runs in its session; stop it from outside"),
                ("m", "not running"),
            ]
        );
        let m = get(&app, "m");
        assert_eq!(
            (m.run_state, m.run_state_by),
            (RunState::Stopped, Some(Setter::Root))
        );

        // A walk that reaches the assistant leaves it alone.
        let root = super::super::root(&app).unwrap();
        let outcomes = stop_with(&app, &Caller::User, Some(&root), &end, &nobody).unwrap();
        assert_eq!(outcomes.last().unwrap().line, "the assistant; not stopped");
        assert!(ended.borrow().iter().all(|(slug, _)| *slug != root.slug));
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
            let err = messages::deliver(
                &task_dir,
                &TmuxAgent::new(app, &w1),
                "hi",
                messages::Sender::Agent,
            )
            .unwrap_err();
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
        let w2_dir = tasks::task_dir(&app, &get(&app, "w2"));
        std::fs::create_dir_all(&w2_dir).unwrap();
        Inbox::lock(&w1_dir)
            .unwrap()
            .push_to("from m", None, messages::Sender::Agent)
            .unwrap();
        // One with messages to start with, one without; neither starts with
        // no tmux, so both stay queued, and w1's message waits still.
        finishes(dir.path(), |app| {
            drain(app).unwrap();
        });
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

        // One whose directory is gone is not started in one made again: it
        // leaves the queue, with a warning.
        std::fs::remove_dir_all(&w2_dir).unwrap();
        let warnings = drain(&app).unwrap();
        assert!(!w2_dir.exists());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("did not start queued task w2") && w.contains("missing")),
            "{warnings:?}"
        );
        let queued: Vec<String> = app
            .db
            .queued_starts()
            .unwrap()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(queued, ["w1"]);
    }

    /// Run `walk` on the index at `home` in a thread of its own while a
    /// start of `w2` admitted before it holds the start lock: it waits for
    /// that start, its row written already, then goes on once it is done.
    fn waits_for_a_start_admitted_before(dir: &tempfile::TempDir, app: &App, walk: fn(&App)) {
        // A start of w2 admitted, its session still to be made.
        let StartGate::Go(guard) = admit_start(app, &get(app, "w2")).unwrap() else {
            panic!("w2 was not admitted");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let home = dir.path().canonicalize().unwrap();
        std::thread::spawn(move || {
            let app = App {
                config: config::load_or_create(&home).unwrap(),
                db: Database::open(&config::index_path(&home)).unwrap(),
                home,
            };
            walk(&app);
            tx.send(()).unwrap();
        });
        // The row is written first, so a start admitted after it is held.
        let wait = std::time::Duration::from_millis(500);
        assert!(rx.recv_timeout(wait).is_err(), "the walk did not wait");
        assert_ne!(get(app, "m2").run_state, RunState::Run);
        drop(guard);
        let done = rx.recv_timeout(std::time::Duration::from_secs(20));
        assert!(done.is_ok(), "the walk never went on");
        assert!(matches!(
            admit_start(app, &get(app, "w2")).unwrap(),
            StartGate::Held(_)
        ));
    }

    #[test]
    fn a_stop_waits_for_a_start_admitted_before_it() {
        let (dir, app) = lab();
        waits_for_a_start_admitted_before(&dir, &app, |app| {
            stop(app, &Caller::User, Some(&get(app, "m2"))).unwrap();
        });
        assert_eq!(get(&app, "m2").run_state, RunState::Stopped);
    }

    #[test]
    fn a_pause_waits_for_a_start_admitted_before_it() {
        let (dir, app) = lab();
        waits_for_a_start_admitted_before(&dir, &app, |app| {
            let rt = fake(app, |_| Ok(Interrupted::Sent));
            let lookup = |_: &TaskRecord| Ok(&rt as &dyn NodeRuntime);
            pause_with(app, &Caller::User, &get(app, "m2"), &lookup, &nobody).unwrap();
        });
        assert_eq!(get(&app, "m2").run_state, RunState::Paused);
    }

    #[test]
    fn a_resume_starts_only_what_a_held_message_queued_the_start_of() {
        let (_dir, app) = lab();
        app.db
            .set_run_state("m", RunState::Paused, Some(&Setter::User))
            .unwrap();
        // Reports for m, not running: they never start it, held or not.
        let m_dir = tasks::task_dir(&app, &get(&app, "m"));
        Inbox::lock(&m_dir)
            .unwrap()
            .push_to("idle 12m; w1 is waiting", None, messages::Sender::Agent)
            .unwrap();
        // A message held for w1, which queued its start.
        let w1_dir = tasks::task_dir(&app, &get(&app, "w1"));
        let w2_dir = tasks::task_dir(&app, &get(&app, "w2"));
        std::fs::create_dir_all(&w2_dir).unwrap();
        Inbox::lock(&w1_dir)
            .unwrap()
            .push_to("your report", None, messages::Sender::Agent)
            .unwrap();
        app.db.queue_start("w1", 1).unwrap();
        let outcomes = resume(&app, &Caller::User, &get(&app, "m")).unwrap();
        assert_eq!(get(&app, "m").run_state, RunState::Run);
        // The drain tries w1 alone; with no tmux here it stays queued, its
        // message still waiting.
        let queued: Vec<String> = app
            .db
            .queued_starts()
            .unwrap()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(queued, ["w1"]);
        assert!(
            outcomes
                .iter()
                .any(|o| o.line.contains("could not start queued task w1")),
            "{outcomes:?}"
        );
        assert!(
            !outcomes.iter().any(|o| o.line.contains("task m:")),
            "{outcomes:?}"
        );
        assert_eq!(
            Inbox::lock(&w1_dir).unwrap().read_for(None).unwrap().len(),
            1
        );
        assert_eq!(
            Inbox::lock(&m_dir).unwrap().read_for(None).unwrap().len(),
            1
        );
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
