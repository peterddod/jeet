//! The agents view's tree: the assistant at the root and its active
//! descendants, what each needs, and what rolls up to the nodes above it
//! (design §6). Nothing here draws.
//!
//! What only the index or tmux knows — asks, budgets, run states, inboxes,
//! PRs, the start queue and the live set — is read by [`gather`], on a
//! thread of its own. What each agent is doing is the status poller's
//! ([`TaskRow::activity`]) and the assistant's thread's, so the two tabs
//! never disagree. [`retree`] and [`lines`] put them together, pure, on the
//! screen's thread.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::state::rank_matches;
use super::tasks::TaskRow;
use crate::assistant::Session;
use crate::context::App;
use crate::db::tasks::{BudgetRow, ReportKind, RunState, Setter, TaskKind, TaskRecord, TaskState};
use crate::manager::{self, budget, run};
use crate::messages;
use crate::status::Activity;
use crate::tasks;

/// How much a node needs looking at, least first. A prompt is
/// `NeedsInput` on the node itself and `NeedsYou` above it: only the user
/// can answer one (§4). A question passed to the user is `NeedsYou` on its
/// asker too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Done,
    Idle,
    Running,
    Budget,
    Blocked,
    NeedsInput,
    NeedsYou,
}

/// What the tree thread reads of one node from the index; never its
/// activity, which the status poller has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// The run state that applies to it, its own or an ancestor's.
    pub run: RunState,
    /// How many of its descendants it holds: those whose run state is its.
    pub held: u32,
    /// Who set the run state that holds it ([`run::setter_label`]).
    pub setter: Option<String>,
    /// Its own limits, and what its subtree has used (seconds, turns).
    pub budget: Option<(BudgetRow, (i64, i64))>,
    /// What is queued in its inbox; `None` while a hook holds the inbox, so
    /// the last count stands.
    pub inbox: Option<usize>,
    /// Its newest open PR, as `repo#n`.
    pub pr: Option<String>,
    /// Its start waits on `max_live`.
    pub queued: bool,
    /// What it asks of its parent and has not moved on from
    /// ([`manager::open_ask`]): the report's id and kind, and whether it
    /// was passed to the user.
    pub ask: Option<(i64, ReportKind, bool)>,
    /// Its last report of its own says it is done.
    pub done: bool,
    /// Its active children.
    pub children: usize,
}

/// One read of the tree from the index, by [`gather`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeInfo {
    /// The nodes whose agent's pane is live: what `max_live` counts.
    pub live: HashSet<String>,
    /// `[managers] max_live`; `None` when the policy has an error.
    pub max_live: Option<u32>,
    /// Starts waiting on `max_live`.
    pub queued: usize,
    /// By task id: the assistant and its active descendants.
    pub nodes: HashMap<String, NodeInfo>,
}

/// A node's own urgency (Q7), from what its agent is doing and what the
/// index says of it; `ask` with whether it is with the user, which
/// outranks a prompt (R16). `None` is an agent the poller has yet to
/// report on.
pub fn own_urgency(
    activity: Option<&Activity>,
    ask: Option<(ReportKind, bool)>,
    done: bool,
    pct: Option<i64>,
    by_budget: bool,
) -> Urgency {
    if ask.is_some_and(|(_, user)| user) {
        return Urgency::NeedsYou;
    }
    if matches!(activity, Some(Activity::Waiting { .. })) {
        return Urgency::NeedsInput;
    }
    if ask.is_some() {
        return Urgency::Blocked;
    }
    if by_budget || pct.is_some_and(|pct| pct >= 80) {
        return Urgency::Budget;
    }
    match activity {
        Some(Activity::Running { .. } | Activity::Live | Activity::Starting) => Urgency::Running,
        _ if done => Urgency::Done,
        _ => Urgency::Idle,
    }
}

/// What each of `nodes` (id, parent id, own urgency) shows: the most urgent
/// of its own and its descendants', a descendant's `NeedsInput` lifted to
/// `NeedsYou`. When that comes from below, with how many descendants are at
/// it; its own comes with 0.
pub fn rollup(nodes: &[(String, Option<String>, Urgency)]) -> HashMap<String, (Urgency, u32)> {
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, (id, _, _))| (id.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, (_, parent, _)) in nodes.iter().enumerate() {
        // A node its own parent is no child of its own.
        if let Some(&p) = parent.as_deref().and_then(|p| index.get(p)) {
            if p != i {
                children[p].push(i);
            }
        }
    }
    let lifted = |u: Urgency| match u {
        Urgency::NeedsInput => Urgency::NeedsYou,
        u => u,
    };
    // The most urgent of each node's descendants, with how many are at it.
    let mut below: Vec<Option<(Urgency, u32)>> = vec![None; nodes.len()];
    let mut done = vec![false; nodes.len()];
    for start in 0..nodes.len() {
        // Children before parents; a node on a cycle is taken once.
        let mut stack = vec![(start, false)];
        let mut on_stack = HashSet::new();
        while let Some((i, expanded)) = stack.pop() {
            if done[i] {
                continue;
            }
            if !expanded {
                if !on_stack.insert(i) {
                    continue;
                }
                stack.push((i, true));
                stack.extend(children[i].iter().map(|&c| (c, false)));
                continue;
            }
            done[i] = true;
            let mut most: Option<(Urgency, u32)> = None;
            for &c in &children[i] {
                for at in [Some((lifted(nodes[c].2), 1)), below[c]]
                    .into_iter()
                    .flatten()
                {
                    most = Some(match most {
                        Some((u, n)) if u == at.0 => (u, n + at.1),
                        Some((u, n)) if u > at.0 => (u, n),
                        _ => at,
                    });
                }
            }
            below[i] = most;
        }
    }
    nodes
        .iter()
        .enumerate()
        .map(|(i, (id, _, own))| {
            let shown = match below[i] {
                Some((u, n)) if u > *own => (u, n),
                _ => (*own, 0),
            };
            (id.clone(), shown)
        })
        .collect()
}

/// [`rollup`] over the tree under `root`, the assistant: each node's own
/// urgency from its activity and its [`NodeInfo`] (a node with none yet,
/// from its activity alone), the assistant's from its `sessions`.
pub fn retree(
    root: &TaskRow,
    rows: &[TaskRow],
    sessions: &[Session],
    info: Option<&TreeInfo>,
) -> HashMap<String, (Urgency, u32)> {
    let root_own = sessions
        .iter()
        .map(|s| own_urgency(Some(&s.activity), None, false, None, false))
        .max()
        .unwrap_or(Urgency::Idle);
    let mut nodes = vec![(root.task.id.clone(), None, root_own)];
    for (i, _) in order(root, rows) {
        let task = &rows[i].task;
        nodes.push((
            task.id.clone(),
            task.parent_id.clone(),
            row_urgency(&rows[i], info),
        ));
    }
    rollup(&nodes)
}

/// [`rollup`] over the active tasks among `rows`, whoever their parent:
/// what the active view's managers show. A parent not among them, the
/// assistant's or none, is a root.
pub fn active_rollup(rows: &[TaskRow], info: Option<&TreeInfo>) -> HashMap<String, (Urgency, u32)> {
    let nodes: Vec<_> = rows
        .iter()
        .filter(|row| row.task.state == TaskState::Active)
        .map(|row| {
            (
                row.task.id.clone(),
                row.task.parent_id.clone(),
                row_urgency(row, info),
            )
        })
        .collect();
    rollup(&nodes)
}

/// A task's own urgency from its activity and its [`NodeInfo`], if the
/// tree thread has read one.
fn row_urgency(row: &TaskRow, info: Option<&TreeInfo>) -> Urgency {
    let task = &row.task;
    let node = info.and_then(|info| info.nodes.get(&task.id));
    own_urgency(
        row.activity.as_ref(),
        node.and_then(|n| n.ask).map(|(_, kind, user)| (kind, user)),
        node.is_some_and(|n| n.done),
        node.and_then(|n| n.budget.as_ref())
            .and_then(|(row, used)| budget::percent(row, *used)),
        task.run_state_by == Some(Setter::Budget),
    )
}

/// Read the tree from the index: the tree thread's, never the draw's. tmux
/// is asked only for the live set, and only when `tmux_ok`. A home with no
/// assistant yet has no nodes; this never makes one.
pub fn gather(app: &App, tmux_ok: bool) -> Result<TreeInfo> {
    let queued_starts: HashSet<String> =
        app.db.queued_starts()?.into_iter().map(|t| t.id).collect();
    let mut info = TreeInfo {
        live: match tmux_ok {
            true => run::live_set(app)?,
            false => HashSet::new(),
        },
        max_live: app.config.manager_policy().ok().map(|p| p.max_live),
        queued: queued_starts.len(),
        nodes: HashMap::new(),
    };
    let Some(root) = app.db.get_assistant_task()? else {
        return Ok(info);
    };
    let mut tree: Vec<TaskRecord> = app
        .db
        .descendants(&root.id)?
        .into_iter()
        .map(|(task, _)| task)
        .filter(|task| task.state == TaskState::Active)
        .collect();
    tree.push(root);
    let mut holders: Vec<Option<String>> = Vec::with_capacity(tree.len());
    for task in &tree {
        let (run, holder) = app.db.effective_run_state(&task.id)?;
        let setter = match holder.as_ref().and_then(|h| h.run_state_by.as_ref()) {
            Some(by) => Some(run::setter_label(app, by)?),
            None => None,
        };
        let budget = match app.db.budget(&task.id)? {
            Some(row) => Some((row, app.db.subtree_used(&task.id)?)),
            None => None,
        };
        let pr = app
            .db
            .list_task_prs(&task.id)?
            .into_iter()
            .find(|pr| pr.state == "OPEN")
            .map(|pr| {
                let repo = pr.repo_id.rsplit('/').next().unwrap_or(&pr.repo_id);
                format!("{repo}#{}", pr.number)
            });
        let last = app.db.last_own_report(&task.id)?;
        let node = NodeInfo {
            run,
            held: 0,
            setter,
            budget,
            inbox: messages::try_peek(&tasks::task_dir(app, task)).map(|q| q.len()),
            pr,
            queued: queued_starts.contains(&task.id),
            ask: manager::open_ask(app, task)?.map(|r| (r.id, r.kind, r.needs_user)),
            done: last.is_some_and(|r| r.kind == ReportKind::Done),
            children: app.db.child_count(&task.id, true)?,
        };
        holders.push(holder.map(|h| h.id));
        info.nodes.insert(task.id.clone(), node);
    }
    // A holder counts the nodes it holds, not itself.
    for (task, holder) in tree.iter().zip(&holders) {
        let Some(holder) = holder.as_ref().filter(|h| **h != task.id) else {
            continue;
        };
        if let Some(node) = info.nodes.get_mut(holder) {
            node.held += 1;
        }
    }
    Ok(info)
}

/// The open ask `a` on node `at` answers (R15): among the asks at or below
/// it, those with the user first (C9), then its own, else the shallowest,
/// ties in tree order. `nodes` are (id, parent id) in tree order, parents
/// first; the asker's id and the report's.
pub fn nearest_ask(
    at: &str,
    nodes: &[(String, Option<String>)],
    info: &TreeInfo,
) -> Option<(String, i64)> {
    // The depth below `at` of each node under it, as the order reaches it.
    let mut depth: HashMap<&str, u32> = HashMap::new();
    let mut best: Option<((bool, u32), &str, i64)> = None;
    for (id, parent) in nodes {
        let d = match parent.as_deref().and_then(|p| depth.get(p)) {
            _ if id == at => 0,
            Some(&d) => d + 1,
            None => continue,
        };
        depth.insert(id, d);
        let Some((report, _, user)) = info.nodes.get(id).and_then(|n| n.ask) else {
            continue;
        };
        // Lower ranks first: with the user, then shallower.
        let rank = (!user, d);
        if best.is_none_or(|(b, _, _)| rank < b) {
            best = Some((rank, id, report));
        }
    }
    best.map(|(_, id, report)| (id.to_string(), report))
}

/// The tree under `root`, the assistant, as [`nearest_ask`] takes it: the
/// root, then its active descendants among `rows` in tree order.
pub fn tree_nodes(root: &TaskRow, rows: &[TaskRow]) -> Vec<(String, Option<String>)> {
    let mut nodes = vec![(root.task.id.clone(), None)];
    nodes.extend(
        order(root, rows)
            .into_iter()
            .map(|(i, _)| (rows[i].task.id.clone(), rows[i].task.parent_id.clone())),
    );
    nodes
}

/// Where a tree line's node is: the assistant, or `rows[i]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    Root,
    Row(usize),
}

/// One line of the tree as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLine {
    pub at: At,
    /// The assistant is 0, its children 1.
    pub depth: u32,
    /// For each ancestor below the root, outermost first: whether its rail
    /// goes on past this line (`│`), as it has siblings still to come.
    pub rails: Vec<bool>,
    /// The last of its siblings shown (`└─`, not `├─`).
    pub last: bool,
    /// A manager, which shows `▾` or `▸` even with no children.
    pub manager: bool,
    /// Not collapsed.
    pub open: bool,
}

/// The tree under `root`, the assistant, as shown: its active descendants
/// among `rows`, depth first with siblings by slug (as `manager ls` has
/// them), less what a collapsed manager hides. A `filter` keeps the nodes
/// whose slug, which the row shows, it matches and their ancestors, collapsed or not
/// (and every manager then shows open). Archived
/// and top-level tasks are never in it.
pub fn lines(
    root: Option<&TaskRow>,
    rows: &[TaskRow],
    collapsed: &HashSet<String>,
    filter: &str,
) -> Vec<TreeLine> {
    let Some(root) = root else {
        return Vec::new();
    };
    let mut all = vec![(At::Root, 0)];
    all.extend(order(root, rows).into_iter().map(|(i, d)| (At::Row(i), d)));
    let task = |at: At| match at {
        At::Root => &root.task,
        At::Row(i) => &rows[i].task,
    };
    let filtering = !filter.is_empty();
    let is_manager = |at: At| at != At::Root && task(at).kind == TaskKind::Manager;
    let keep: Vec<bool> = match filter.is_empty() {
        true => {
            let mut hidden_below: Option<u32> = None;
            all.iter()
                .map(|&(at, depth)| {
                    if hidden_below.is_some_and(|d| depth > d) {
                        return false;
                    }
                    hidden_below =
                        (is_manager(at) && collapsed.contains(&task(at).id)).then_some(depth);
                    true
                })
                .collect()
        }
        false => {
            let mut keep = vec![false; all.len()];
            // What each line shows: the slug, and "assistant" for the root.
            let names = all.iter().map(|&(at, _)| match at {
                At::Root => "assistant",
                At::Row(_) => task(at).slug.as_str(),
            });
            for hit in rank_matches(names, filter) {
                // It and its ancestors: each the nearest line before it a
                // level up.
                let mut i = hit;
                loop {
                    keep[i] = true;
                    let depth = all[i].1;
                    match all[..i].iter().rposition(|&(_, d)| d < depth) {
                        Some(up) => i = up,
                        None => break,
                    }
                }
            }
            keep
        }
    };
    let shown: Vec<(At, u32)> = all
        .into_iter()
        .zip(keep)
        .filter_map(|(line, keep)| keep.then_some(line))
        .collect();
    // Whether the rail at each depth goes on, as the lines are laid out.
    let mut going_on: Vec<bool> = Vec::new();
    let mut lines = Vec::with_capacity(shown.len());
    for (i, &(at, depth)) in shown.iter().enumerate() {
        let last = shown[i + 1..]
            .iter()
            .find(|&&(_, d)| d <= depth)
            .is_none_or(|&(_, d)| d < depth);
        let d = depth as usize;
        going_on.resize(d + 1, false);
        going_on[d] = !last;
        lines.push(TreeLine {
            at,
            depth,
            rails: going_on.get(1..d).unwrap_or_default().to_vec(),
            last,
            manager: is_manager(at),
            open: filtering || !collapsed.contains(&task(at).id),
        });
    }
    lines
}

/// The line of `lines[i]`'s parent, for `←`.
pub fn parent(lines: &[TreeLine], i: usize) -> Option<usize> {
    let depth = lines.get(i)?.depth;
    lines[..i].iter().rposition(|line| line.depth < depth)
}

/// The task id of `line`'s node.
pub fn id_at<'a>(
    line: &TreeLine,
    root: Option<&'a TaskRow>,
    rows: &'a [TaskRow],
) -> Option<&'a str> {
    match line.at {
        At::Root => root.map(|r| r.task.id.as_str()),
        At::Row(i) => rows.get(i).map(|r| r.task.id.as_str()),
    }
}

/// The active nodes among `rows` under `root`, in [`manager::tree_order`]:
/// indexes into `rows`, with their depth below the root.
fn order(root: &TaskRow, rows: &[TaskRow]) -> Vec<(usize, u32)> {
    let active: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].task.state == TaskState::Active)
        .collect();
    let mut nodes: Vec<TaskRecord> = active.iter().map(|&i| rows[i].task.clone()).collect();
    nodes.push(root.task.clone());
    manager::tree_order(&nodes, &root.task)
        .into_iter()
        .filter(|&(k, _)| k < active.len())
        .map(|(k, depth)| (active[k], depth))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config;
    use crate::db::Database;
    use crate::messages::Inbox;
    use crate::placement::Placement;

    fn record(id: &str, kind: TaskKind, parent: Option<&str>, state: TaskState) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            slug: id.into(),
            name: id.into(),
            project: None,
            kind,
            state,
            created_at: 0,
            last_activity: 0,
            parent_id: parent.map(str::to_string),
            placement: Placement::Local,
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        }
    }

    fn row(id: &str, kind: TaskKind, parent: Option<&str>, activity: Option<Activity>) -> TaskRow {
        TaskRow {
            task: record(id, kind, parent, TaskState::Active),
            dir: PathBuf::from(format!("/tasks/{id}")),
            worktrees: Vec::new(),
            activity,
            last_activity: 0,
            size: None,
            report: None,
            review_jobs: Vec::new(),
            unanchored: false,
            queued: false,
        }
    }

    fn waiting() -> Option<Activity> {
        Some(Activity::Waiting {
            detail: Some("Bash".into()),
        })
    }

    fn running() -> Option<Activity> {
        Some(Activity::Running { detail: None })
    }

    /// assistant → api → {w1 waiting, w2 running}, fe → w3; then an
    /// archived child of api and a top-level task, which are not in it.
    fn tree() -> (TaskRow, Vec<TaskRow>) {
        let root = row("root", TaskKind::Assistant, None, None);
        let mut gone = row("old", TaskKind::Work, Some("api"), None);
        gone.task.state = TaskState::Archived;
        let rows = vec![
            row("w2", TaskKind::Work, Some("api"), running()),
            row("fe", TaskKind::Manager, Some("root"), None),
            row("top", TaskKind::Work, None, waiting()),
            row("w1", TaskKind::Work, Some("api"), waiting()),
            gone,
            row("api", TaskKind::Manager, Some("root"), running()),
            row("w3", TaskKind::Work, Some("fe"), None),
        ];
        (root, rows)
    }

    fn info_with_ask(id: &str) -> TreeInfo {
        let mut info = TreeInfo::default();
        add_ask(&mut info, id, 7, false);
        info
    }

    /// `id`'s node in `info`, asking report `report`, with the user or not.
    fn add_ask(info: &mut TreeInfo, id: &str, report: i64, user: bool) {
        info.nodes.insert(
            id.into(),
            NodeInfo {
                run: RunState::Run,
                held: 0,
                setter: None,
                budget: None,
                inbox: Some(0),
                pr: None,
                queued: false,
                ask: Some((report, ReportKind::Blocked, user)),
                done: false,
                children: 0,
            },
        );
    }

    #[test]
    fn a_nodes_own_urgency_follows_the_rules_in_order() {
        use Urgency::*;
        let idle = Some(Activity::Idle { since: 0 });
        type Case = (
            Option<Activity>,
            Option<(ReportKind, bool)>,
            bool,
            Option<i64>,
            bool,
            Urgency,
        );
        let cases: &[Case] = &[
            (waiting(), None, false, None, false, NeedsInput),
            // A prompt outranks the ask it may be about.
            (
                waiting(),
                Some((ReportKind::Question, false)),
                true,
                Some(99),
                true,
                NeedsInput,
            ),
            // A question with the user outranks a prompt.
            (
                waiting(),
                Some((ReportKind::Question, true)),
                false,
                None,
                false,
                NeedsYou,
            ),
            (
                running(),
                Some((ReportKind::Question, true)),
                false,
                None,
                false,
                NeedsYou,
            ),
            (
                running(),
                Some((ReportKind::Question, false)),
                false,
                None,
                false,
                Blocked,
            ),
            (
                idle.clone(),
                Some((ReportKind::Blocked, false)),
                true,
                Some(90),
                false,
                Blocked,
            ),
            (running(), None, false, Some(80), false, Budget),
            (running(), None, false, Some(79), false, Running),
            (idle.clone(), None, false, None, true, Budget),
            (Some(Activity::Live), None, false, None, false, Running),
            (Some(Activity::Starting), None, true, None, false, Running),
            (idle.clone(), None, true, None, false, Done),
            (idle, None, false, None, false, Idle),
            (Some(Activity::NotRunning), None, false, None, false, Idle),
            (Some(Activity::NotStarted), None, false, None, false, Idle),
            (Some(Activity::Ended), None, false, None, false, Idle),
            (None, None, false, None, false, Idle),
        ];
        for (activity, ask, done, pct, by_budget, want) in cases {
            assert_eq!(
                own_urgency(activity.as_ref(), *ask, *done, *pct, *by_budget),
                *want,
                "{activity:?} {ask:?} done={done} {pct:?} budget={by_budget}"
            );
        }
    }

    #[test]
    fn a_prompt_rolls_up_as_needs_you_with_a_count() {
        use Urgency::*;
        let node =
            |id: &str, parent: Option<&str>, u| (id.to_string(), parent.map(str::to_string), u);
        let nodes = vec![
            node("root", None, Idle),
            node("api", Some("root"), Running),
            node("w1", Some("api"), NeedsInput),
            node("w2", Some("api"), Running),
            node("fe", Some("root"), Idle),
            node("w3", Some("fe"), Blocked),
        ];
        let shown = rollup(&nodes);
        assert_eq!(shown["api"], (NeedsYou, 1));
        // Not blocked: a prompt below outranks fe's ask.
        assert_eq!(shown["root"], (NeedsYou, 1));
        assert_eq!(shown["fe"], (Blocked, 1));
        // Its own state, so no count.
        assert_eq!(shown["w1"], (NeedsInput, 0));
        assert_eq!(shown["w2"], (Running, 0));

        let mut two = nodes.clone();
        two[3].2 = NeedsInput;
        let shown = rollup(&two);
        assert_eq!(shown["api"], (NeedsYou, 2));
        assert_eq!(shown["root"], (NeedsYou, 2));

        let calm = vec![node("m", None, Running), node("c", Some("m"), Idle)];
        assert_eq!(rollup(&calm)["m"], (Running, 0));
        // As urgent as below is still its own.
        let same = vec![node("m", None, Blocked), node("c", Some("m"), Blocked)];
        assert_eq!(rollup(&same)["m"], (Blocked, 0));
    }

    #[test]
    fn a_question_with_the_user_rolls_up_as_needs_you() {
        use Urgency::*;
        let node =
            |id: &str, parent: Option<&str>, u| (id.to_string(), parent.map(str::to_string), u);
        let own = own_urgency(
            running().as_ref(),
            Some((ReportKind::Question, true)),
            false,
            None,
            false,
        );
        let nodes = vec![
            node("root", None, Idle),
            node("api", Some("root"), Running),
            node("w", Some("api"), own),
        ];
        let shown = rollup(&nodes);
        assert_eq!(shown["w"], (NeedsYou, 0));
        assert_eq!(shown["api"], (NeedsYou, 1));
        assert_eq!(shown["root"], (NeedsYou, 1));
    }

    #[test]
    fn a_answers_the_nearest_ask_with_the_user_first() {
        let (root, rows) = tree();
        let nodes = tree_nodes(&root, &rows);
        let ids: Vec<&str> = nodes.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["root", "api", "w1", "w2", "fe", "w3"]);
        let nearest = |at: &str, info: &TreeInfo| nearest_ask(at, &nodes, info);

        // Nothing open.
        assert_eq!(nearest("root", &TreeInfo::default()), None);
        let mut info = TreeInfo::default();
        add_ask(&mut info, "w1", 1, false);
        add_ask(&mut info, "w2", 2, false);
        add_ask(&mut info, "w3", 3, false);
        // Ties in tree order.
        assert_eq!(nearest("root", &info), Some(("w1".into(), 1)));
        assert_eq!(nearest("fe", &info), Some(("w3".into(), 3)));
        assert_eq!(nearest("w2", &info), Some(("w2".into(), 2)));
        // Never above or beside it.
        assert_eq!(nearest("w2", &TreeInfo::default()), None);
        let mut only_w3 = TreeInfo::default();
        add_ask(&mut only_w3, "w3", 3, false);
        assert_eq!(nearest("api", &only_w3), None);
        // The node's own, then the shallowest.
        add_ask(&mut info, "api", 4, false);
        assert_eq!(nearest("root", &info), Some(("api".into(), 4)));
        assert_eq!(nearest("api", &info), Some(("api".into(), 4)));
        // A deeper ask with the user wins over a shallower one a manager
        // holds (C9).
        add_ask(&mut info, "w3", 3, true);
        assert_eq!(nearest("root", &info), Some(("w3".into(), 3)));
        assert_eq!(nearest("api", &info), Some(("api".into(), 4)));
    }

    #[test]
    fn a_node_its_own_parent_rolls_up_nothing_from_itself() {
        let shown = rollup(&[("x".into(), Some("x".into()), Urgency::Running)]);
        assert_eq!(shown["x"], (Urgency::Running, 0));
    }

    #[test]
    fn a_cycle_in_the_parents_is_rolled_up_once() {
        let nodes = vec![
            ("a".to_string(), Some("b".to_string()), Urgency::Idle),
            ("b".to_string(), Some("a".to_string()), Urgency::NeedsInput),
        ];
        let shown = rollup(&nodes);
        assert_eq!(shown.len(), 2);
    }

    #[test]
    fn retree_reads_activity_from_the_rows_and_the_rest_from_the_tree_info() {
        let (root, rows) = tree();
        // No tree info yet: activity alone.
        let shown = retree(&root, &rows, &[], None);
        assert_eq!(shown["root"], (Urgency::NeedsYou, 1));
        assert_eq!(shown["fe"], (Urgency::Idle, 0));
        assert_eq!(shown["w1"], (Urgency::NeedsInput, 0));
        // Not in the tree.
        assert!(!shown.contains_key("top"));
        assert!(!shown.contains_key("old"));

        let shown = retree(&root, &rows, &[], Some(&info_with_ask("w3")));
        assert_eq!(shown["fe"], (Urgency::Blocked, 1));
        assert_eq!(shown["root"], (Urgency::NeedsYou, 1));

        // A question with the user: needs you on the asker and above it.
        let mut info = TreeInfo::default();
        add_ask(&mut info, "w2", 9, true);
        let shown = retree(&root, &rows, &[], Some(&info));
        assert_eq!(shown["w2"], (Urgency::NeedsYou, 0));
        assert_eq!(shown["api"], (Urgency::NeedsYou, 2));
        assert_eq!(shown["root"], (Urgency::NeedsYou, 2));

        // A budget's pause is the node's own row.
        let mut paused = rows.clone();
        paused[6].task.run_state_by = Some(Setter::Budget);
        assert_eq!(
            retree(&root, &paused, &[], None)["w3"],
            (Urgency::Budget, 0)
        );
    }

    #[test]
    fn the_assistant_waiting_on_a_prompt_needs_input_itself() {
        let root = row("root", TaskKind::Assistant, None, None);
        let session = |activity| Session {
            id: "s".into(),
            name: "main".into(),
            primary: true,
            last: 0,
            activity,
            window: Some("@1".into()),
        };
        let calm = [session(Activity::Running { detail: None })];
        assert_eq!(
            retree(&root, &[], &calm, None)["root"],
            (Urgency::Running, 0)
        );
        let asking = [
            session(Activity::Idle { since: 0 }),
            session(waiting().unwrap()),
        ];
        assert_eq!(
            retree(&root, &[], &asking, None)["root"],
            (Urgency::NeedsInput, 0)
        );
        assert_eq!(retree(&root, &[], &[], None)["root"], (Urgency::Idle, 0));
    }

    /// Each line as `depth:id`.
    fn layout(lines: &[TreeLine], root: &TaskRow, rows: &[TaskRow]) -> Vec<String> {
        lines
            .iter()
            .map(|l| format!("{}:{}", l.depth, id_at(l, Some(root), rows).unwrap()))
            .collect()
    }

    /// Each line drawn with its rails, as the view will draw it.
    fn drawn(lines: &[TreeLine], root: &TaskRow, rows: &[TaskRow]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let id = id_at(l, Some(root), rows).unwrap();
                if l.depth == 0 {
                    return id.to_string();
                }
                let rails: String = l
                    .rails
                    .iter()
                    .map(|&on| if on { "│  " } else { "   " })
                    .collect();
                let branch = if l.last { "└─ " } else { "├─ " };
                let marker = match (l.manager, l.open) {
                    (true, true) => "▾ ",
                    (true, false) => "▸ ",
                    (false, _) => "",
                };
                format!("{rails}{branch}{marker}{id}")
            })
            .collect()
    }

    #[test]
    fn the_tree_is_laid_out_depth_first_with_siblings_by_slug() {
        let (root, rows) = tree();
        let lines = lines(Some(&root), &rows, &HashSet::new(), "");
        assert_eq!(
            layout(&lines, &root, &rows),
            ["0:root", "1:api", "2:w1", "2:w2", "1:fe", "2:w3"]
        );
        assert_eq!(
            drawn(&lines, &root, &rows),
            [
                "root",
                "├─ ▾ api",
                "│  ├─ w1",
                "│  └─ w2",
                "└─ ▾ fe",
                "   └─ w3",
            ]
        );
        assert_eq!(lines[0].at, At::Root);
        assert!(!lines[0].manager);
    }

    #[test]
    fn a_collapsed_manager_hides_its_subtree_but_keeps_its_marker() {
        let (root, rows) = tree();
        let collapsed = HashSet::from(["api".to_string()]);
        let lines = lines(Some(&root), &rows, &collapsed, "");
        assert_eq!(
            drawn(&lines, &root, &rows),
            ["root", "├─ ▸ api", "└─ ▾ fe", "   └─ w3"]
        );
        // A manager with nothing under it still shows it is one.
        let lone = vec![row("m", TaskKind::Manager, Some("root"), None)];
        let lines = super::lines(Some(&root), &lone, &HashSet::new(), "");
        assert_eq!(drawn(&lines, &root, &lone), ["root", "└─ ▾ m"]);
    }

    #[test]
    fn a_filter_keeps_the_matches_and_their_ancestors_whatever_is_collapsed() {
        let (root, rows) = tree();
        let collapsed = HashSet::from(["fe".to_string()]);
        let lines = lines(Some(&root), &rows, &collapsed, "w3");
        assert_eq!(drawn(&lines, &root, &rows), ["root", "└─ ▾ fe", "   └─ w3"]);
        assert!(super::lines(Some(&root), &rows, &collapsed, "zzz").is_empty());
    }

    #[test]
    fn a_filter_matches_the_slug_the_row_shows_not_the_name() {
        let (root, mut rows) = tree();
        let i = rows.iter().position(|r| r.task.id == "w3").unwrap();
        rows[i].task.name = "Fix the login".to_string();
        rows[i].task.slug = "fix-the-login".to_string();
        let none = HashSet::new();
        assert_eq!(lines(Some(&root), &rows, &none, "the-l").len(), 3);
        assert!(lines(Some(&root), &rows, &none, "the login").is_empty());
        // The root reads "assistant".
        assert_eq!(lines(Some(&root), &rows, &none, "assist").len(), 1);
    }

    #[test]
    fn there_is_no_tree_without_the_assistant() {
        let (_, rows) = tree();
        assert!(lines(None, &rows, &HashSet::new(), "").is_empty());
    }

    #[test]
    fn a_lines_parent_is_the_nearest_line_a_level_up() {
        let (root, rows) = tree();
        let lines = lines(Some(&root), &rows, &HashSet::new(), "");
        let parent_of = |i| parent(&lines, i).map(|p| layout(&lines, &root, &rows)[p].clone());
        assert_eq!(parent_of(0), None);
        assert_eq!(parent_of(1).as_deref(), Some("0:root"));
        assert_eq!(parent_of(3).as_deref(), Some("1:api"));
        assert_eq!(parent_of(5).as_deref(), Some("1:fe"));
        assert_eq!(parent(&lines, 99), None);
    }

    fn app_at(home: &std::path::Path) -> App {
        App {
            config: config::load_or_create(home).unwrap(),
            db: Database::open(&config::index_path(home)).unwrap(),
            home: home.to_path_buf(),
        }
    }

    #[test]
    fn gather_reads_asks_budgets_holds_and_inboxes_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(&dir.path().canonicalize().unwrap());
        // No assistant yet: no nodes, and none is made.
        let info = gather(&app, false).unwrap();
        assert!(info.nodes.is_empty());
        assert!(app.db.get_assistant_task().unwrap().is_none());

        let active = TaskState::Active;
        for task in [
            record("root", TaskKind::Assistant, None, active),
            record("api", TaskKind::Manager, Some("root"), active),
            record("w1", TaskKind::Work, Some("api"), active),
            record("w2", TaskKind::Work, Some("api"), active),
            record("old", TaskKind::Work, Some("api"), TaskState::Archived),
            record("top", TaskKind::Work, None, active),
        ] {
            app.db.insert_task(&task).unwrap();
        }
        let asked = app
            .db
            .insert_report("w1", "api", ReportKind::Question, "which db?", 5)
            .unwrap();
        // Progress since leaves the question open (H3).
        app.db
            .insert_report("w1", "api", ReportKind::Progress, "still waiting", 5)
            .unwrap();
        app.db.set_budget_max("api", Some(3600), None).unwrap();
        app.db.charge("w1", 600, 2).unwrap();
        app.db.charge("w2", 60, 1).unwrap();
        app.db
            .set_run_state("api", RunState::Paused, Some(&Setter::User))
            .unwrap();
        app.db
            .insert_report("w2", "api", ReportKind::Done, "done", 6)
            .unwrap();
        assert!(app.db.queue_start("w2", 7).unwrap());
        for (repo, number, state, at) in [
            ("github/acme/repo", 3, "OPEN", "2026-01-01T00:00:00Z"),
            ("github/acme/repo", 4, "MERGED", "2026-02-01T00:00:00Z"),
            ("github/acme/repo", 2, "OPEN", "2025-12-01T00:00:00Z"),
        ] {
            app.db
                .upsert_task_pr(&crate::db::tasks::TaskPrRecord {
                    task_id: "w1".to_string(),
                    repo_id: repo.to_string(),
                    number,
                    title: "t".to_string(),
                    url: "u".to_string(),
                    state: state.to_string(),
                    is_draft: false,
                    additions: 1,
                    deletions: 1,
                    updated_at: at.to_string(),
                })
                .unwrap();
        }
        let w2 = app.db.get_task("w2").unwrap().unwrap();
        Inbox::lock(&tasks::task_dir(&app, &w2))
            .unwrap()
            .push("hi")
            .unwrap();

        // A try can find the inbox lock held for a moment by another test's
        // forked child (`messages::tests::settled_peek`): read again until
        // both inboxes were read.
        let mut info = gather(&app, false).unwrap();
        for _ in 0..200 {
            if info.nodes.values().all(|node| node.inbox.is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            info = gather(&app, false).unwrap();
        }
        assert!(info.live.is_empty());
        assert_eq!(info.queued, 1);
        assert_eq!(
            info.max_live,
            Some(app.config.manager_policy().unwrap().max_live)
        );
        let mut ids: Vec<&str> = info.nodes.keys().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, ["api", "root", "w1", "w2"]);

        let api = &info.nodes["api"];
        assert_eq!((api.run, api.held), (RunState::Paused, 2));
        assert_eq!(api.setter.as_deref(), Some("the user"));
        let (row, used) = api.budget.as_ref().unwrap();
        assert_eq!((row.max_secs, *used), (Some(3600), (660, 3)));
        assert_eq!(api.children, 2);
        assert_eq!(api.ask, None);

        let w1 = &info.nodes["w1"];
        assert_eq!(w1.ask, Some((asked, ReportKind::Question, false)));
        assert_eq!((w1.run, w1.held), (RunState::Paused, 0));
        assert_eq!(w1.inbox, Some(0));
        assert_eq!(info.nodes["w2"].inbox, Some(1));
        // The newest PR that is still open, not the merged one or older.
        assert_eq!(w1.pr.as_deref(), Some("repo#3"));
        assert_eq!(info.nodes["w2"].pr, None);
        assert!(info.nodes["w2"].queued && info.nodes["w2"].done);
        assert!(!w1.queued && !w1.done);
        assert_eq!(info.nodes["root"].children, 1);
        assert_eq!(info.nodes["root"].run, RunState::Run);
    }
}
