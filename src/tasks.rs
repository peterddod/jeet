//! Tasks: the unit of work.
//!
//! A task is a directory under `$JEET_HOME/tasks/<slug>` — a blank canvas the
//! agent works in — plus any number of git worktrees inside it, added and
//! removed over the task's life from any repository in the store. The index
//! records which worktrees a task holds; [`reconcile`] keeps that record
//! honest when worktrees come and go behind jeet's back.
//!
//! Archiving frees the disk but keeps what is needed to bring every worktree
//! back as it was: its HEAD, held by `refs/jeet/head/<task-id>/<dir>` so it
//! survives gc, and its uncommitted changes, saved as a stash-shaped commit
//! under `refs/jeet/stash/<task-id>/<dir>`. Whatever unarchive cannot put
//! back is moved to `refs/jeet/kept/<task-id>/…`, which nothing reuses, and
//! stays there until the task is deleted. The task directory itself stays,
//! so the agent's sessions, which Claude keys by working directory, still
//! resolve when the task comes back.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use crate::agent::AgentSpec;
use crate::cli::Delegation;
use crate::config::{PrMode, DEFAULT_PR_MODE};
use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::tasks::{
    MoveRows, RunState, Setter, TaskBriefRecord, TaskKind, TaskRecord, TaskState,
    TaskWorktreeRecord,
};
use crate::db::RepoRecord;
use crate::git;
use crate::identity;
use crate::instructions;
use crate::manager;
use crate::manager::budget::{self, Limits};
use crate::messages::{self, Delivered, Sender};
use crate::permissions::{self, Caller, Grant, Target, Verb};
use crate::placement;
use crate::projects::{self, Project};
use crate::remote;
use crate::reports::{self, StatusEvent};
use crate::resolve;
use crate::runtime;
use crate::store;
use crate::worktrees::{self, now_secs};

/// The PR policy of `task`'s agent, most specific first: its own (`jeet
/// task new --pr`), its project's `[tasks] pr`, the global `[tasks] pr`, or
/// [`DEFAULT_PR_MODE`]. Read at each start, so a change to the config
/// reaches the task's next session. A project since deleted or broken is
/// passed over, as its agent arguments are at launch.
pub fn pr_mode(app: &App, task: &TaskRecord) -> Result<PrMode> {
    if let Some(mode) = app.db.task_pr(&task.id)? {
        return Ok(mode);
    }
    let project = match &task.project {
        Some(name) => projects::load(app, name)
            .ok()
            .and_then(|p| p.config.tasks.pr),
        None => None,
    };
    Ok(project.or(app.config.tasks.pr).unwrap_or(DEFAULT_PR_MODE))
}

/// The one line a work task's session starts with about pull requests,
/// which the worker skill points to.
pub fn pr_policy_line(mode: PrMode) -> String {
    let what = match mode {
        PrMode::Auto => {
            "open a ready PR with `gh pr create` once the work is pushed and its checks pass \
             locally; don't ask"
        }
        PrMode::Draft => {
            "open PRs with `gh pr create --draft`; don't ask. Mark one ready (`gh pr ready`) \
             only when your parent or the user asks"
        }
        PrMode::Ask => {
            "open no PR yourself; ask first (`jeet report question \"open a PR for <branch>?\"`, \
             or the user at the pane in a top-level task)"
        }
    };
    format!("PR policy: {} — {what}.", mode.as_str())
}

/// Set in the agent's environment so `jeet worktree` knows its task without
/// having to be run from inside the task directory.
pub const TASK_ID_ENV: &str = "JEET_TASK_ID";

/// `<task>/.jeet/task-id` marks a task directory, so the task can be found
/// from anywhere beneath it.
pub const MARKER_DIR: &str = ".jeet";
const MARKER_FILE: &str = "task-id";

const SLUG_MAX: usize = 48;

/// The built-in skill every task gets, at `.claude/skills/jeet/SKILL.md`: how
/// the agent gets at code from inside a blank-canvas task.
const SKILL: &str = r#"---
name: jeet
description: How to work in this jeet task workspace - check repositories out with `jeet worktree add`, list and remove worktrees, open pull requests, and report to the task's parent with `jeet report`. Use whenever you need code from a repository, or to say how the task is going.
---

# Working in a jeet task

This directory is your task workspace. It starts as a blank canvas: keep notes
and scratch files here freely. Repositories are checked out on demand, as git
worktrees inside this directory.

- Get a repository: `cd "$(jeet worktree add <repo>)"`. It prints the new
  worktree's path and nothing else. `<repo>` is a name such as `api`, an
  `owner/repo`, or a clone URL; a repository jeet does not have yet is cloned
  first, if your task may clone (a task with a parent needs `repo.clone` in
  its grant; without it, ask with `jeet report blocked "…"`). The worktree
  starts detached at the freshly fetched default branch.
  - `--branch <name>` checks that branch out instead: an existing branch is
    reused, otherwise it is created from the default branch.
  - `--name <dir>` picks the directory name (default: the repository name).
  - Add as many as you need, including several of the same repository.
- List the repositories jeet knows: `jeet ls`.
- List this task's worktrees: `jeet worktree ls`.
- Remove one you are done with: `jeet worktree rm <name>`. It refuses while the
  worktree holds uncommitted or unpushed work.
- Open pull requests from inside a worktree with `gh pr create`, as the
  task's PR policy says. The context shown as each session starts has a
  `PR policy:` line: `auto` opens a ready PR once the work is pushed and
  its checks pass locally; `draft` opens it with `gh pr create --draft` and
  marks it ready only when your parent or the user asks; `ask` opens none
  and asks instead (`jeet report question "open a PR for <branch>?"`, or the
  user at the pane in a top-level task). Don't ask about what the policy
  already says.
- Never touch `$JEET_HOME/store` (the canonical clones) directly. Prefer
  `jeet worktree add` over running `git worktree add` yourself.

## Reporting to your parent (when this task has one)

A task made by the assistant reports to it; its brief, shown as each session
starts, says so. Tell it how the task is going:

- `jeet report done "…"` when the work is finished, saying where it is (a
  pull request, a file).
- `jeet report blocked "…"` or `jeet report question "…"` when you are
  stuck or need a decision, rather than waiting.
- `jeet report progress "…"` for a milestone worth knowing about.

Keep reports short; put detail in the pull request or a file and say where.
jeet tells the parent itself when you wait on a prompt, sit idle or end.

The answer to your question comes as `<jeet-answer to="<id>" from="…">`.
`from="user"` is the user; any other `from` is the manager or the
assistant above you, to act on as its instruction. A question stays open
while you report progress; asking again, or reporting done or blocked,
withdraws it.

Such a task may only add worktrees to itself (when its parent may) and
report: `jeet task send`, `jeet notify`, `jeet todo` and `jeet worktree rm`
are refused. Ask your parent instead.
"#;

/// A task's directory; the assistant's is its home, outside `tasks/`.
pub fn task_dir(app: &App, task: &TaskRecord) -> PathBuf {
    match task.kind {
        TaskKind::Assistant => crate::config::assistant_dir(&app.home),
        TaskKind::Work | TaskKind::Manager => app.tasks_root().join(&task.slug),
    }
}

/// The file marking `dir` as the directory of the task whose id it holds.
pub fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_DIR).join(MARKER_FILE)
}

pub fn worktree_dir(app: &App, task: &TaskRecord, dir_name: &str) -> PathBuf {
    task_dir(app, task).join(dir_name)
}

/// A directory name from a task name: lowercase ASCII letters and digits,
/// with runs of anything else collapsed to a single `-`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= SLUG_MAX {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug.to_string()
    }
}

/// `base`, or `base-2`, `base-3`… — the first that neither another task or
/// review nor a stray directory already has.
pub(crate) fn unique_slug(app: &App, base: &str) -> Result<String> {
    let root = app.tasks_root();
    let mut n = 1;
    loop {
        let candidate = numbered(base, n);
        // Kept for the assistant, whose tmux session is `jeet-assistant`.
        let reserved = candidate == crate::assistant::SLUG;
        // A slug that spells another task's id would capture that id in
        // slug-first lookups (`find_exact`), so ids are kept too.
        if !reserved
            && app.db.get_task(&candidate)?.is_none()
            && app.db.get_task_by_slug(&candidate)?.is_none()
            && app.db.get_review(&candidate)?.is_none()
            && app.db.find_review_by_slug(&candidate)?.is_none()
            && !root.join(&candidate).exists()
        {
            return Ok(candidate);
        }
        n += 1;
    }
}

fn numbered(base: &str, n: usize) -> String {
    match n {
        1 => base.to_string(),
        n => format!("{base}-{n}"),
    }
}

/// What `jeet task new` asks for.
pub struct NewTask<'a> {
    pub name: &'a str,
    /// Project to start from: its files are copied in, and a worktree of each
    /// of its repositories is added.
    pub project: Option<&'a str>,
    /// Repositories to add a worktree of straight away, after the project's.
    /// None at all, and no project, makes an empty canvas.
    pub repos: &'a [String],
    /// Branch for the worktree of the single repository in `repos`; the
    /// project's worktrees start detached.
    pub branch: Option<&'a str>,
    /// Its place under a parent, planned by [`plan_child`] for the caller
    /// making it; `None` for a top-level task.
    pub child: Option<(&'a Caller, &'a NewNode)>,
    /// With no `project`, start from the parent's, if it has one. The CLI
    /// does; a form that showed the choice (the TUI) does not, so its
    /// "no project" is taken at its word.
    pub inherit_project: bool,
}

/// A task that was just made, plus anything worth telling the user.
pub struct Created {
    pub task: TaskRecord,
    pub warnings: Vec<String>,
}

/// Make a task: its directory, marker and skill, the project's files, and a
/// worktree of each preset repository — the project's first, then `repos`.
///
/// If a preset worktree cannot be added the whole task is undone — it was
/// made a moment ago and holds nothing of anyone's yet.
pub fn create(app: &App, spec: &NewTask) -> Result<Created> {
    let name = spec.name.trim();
    if name.is_empty() {
        bail!("a task needs a name");
    }
    if spec.branch.is_some() && spec.repos.len() != 1 {
        bail!("--branch needs exactly one --repo: it names that repository's branch");
    }
    // A task under a node that has a project starts from it, unless it was
    // given one of its own. Read up front, so a missing or broken project
    // makes nothing at all.
    let mut warnings = Vec::new();
    let project = match (spec.project, spec.child) {
        (Some(name), _) => Some(projects::load(app, name)?),
        (None, Some((_, plan))) if spec.inherit_project => {
            let parent = match plan.record.parent_id.as_deref() {
                Some(id) => app.db.get_task(id)?,
                None => None,
            };
            // The parent's project may have been deleted or broken since:
            // that costs the child its project, not its existence.
            match parent
                .as_ref()
                .and_then(|p| p.project.as_deref().map(|n| (p, n)))
            {
                Some((parent, name)) => match projects::load(app, name) {
                    Ok(project) => Some(project),
                    Err(e) => {
                        warnings.push(format!(
                            "made without a project: {}'s project {name} could not be read ({e:#})",
                            parent.slug
                        ));
                        None
                    }
                },
                None => None,
            }
        }
        _ => None,
    };

    let (task, notes) = match spec.child {
        Some((_, plan)) => commit_child(app, plan, project.as_ref())?,
        None => make_task(app, name, project.as_ref(), TaskKind::Work)?,
    };
    warnings.extend(notes);

    let preset = project
        .iter()
        .flat_map(|p| &p.config.repos)
        .map(|r| (r, None));
    // A repository the project already gave, asked for again with no branch,
    // would be a second detached worktree of the same thing: skipped.
    let preset_ids: Vec<String> = project
        .iter()
        .flat_map(|p| &p.config.repos)
        .filter_map(|r| known_repo_id(app, r))
        .collect();
    let given = spec
        .repos
        .iter()
        .filter(|r| {
            spec.branch.is_some()
                || !known_repo_id(app, r).is_some_and(|id| preset_ids.contains(&id))
        })
        .map(|r| (r, spec.branch));
    for (repo, branch) in preset.chain(given) {
        match add_worktree(app, &task, repo, branch, None) {
            Ok(added) => warnings.extend(added.warnings),
            Err(e) => {
                let e = e.context(format!(
                    "could not add {} to the new task",
                    repo.escape_debug()
                ));
                return Err(match delete(app, &task, false) {
                    Ok(_) => e,
                    Err(undo) => e.context(format!(
                        "and the half-made task could not be removed ({undo:#}); \
                         `jeet task delete {} --force` cleans it up",
                        task.slug
                    )),
                });
            }
        }
    }
    if let Some((by, plan)) = spec.child {
        record_child_held(app, by, &task, &plan.grant.held, &mut warnings);
    }
    Ok(Created { task, warnings })
}

/// A top-level task with no worktrees yet: its row, its directory, marker
/// and skill, and the project's files. Returns notes on project files not
/// copied.
fn make_task(
    app: &App,
    name: &str,
    project: Option<&Project>,
    kind: TaskKind,
) -> Result<(TaskRecord, Vec<String>)> {
    let task = TaskRecord {
        project: project.map(|p| p.name.clone()),
        ..new_record(app, name, kind, None)?
    };
    let notes = write_task(app, &task, project, || app.db.insert_task(&task))?;
    Ok((task, notes))
}

/// The row of a new task called `name`, under `parent_id`, with a slug of
/// its own.
fn new_record(
    app: &App,
    name: &str,
    kind: TaskKind,
    parent_id: Option<&str>,
) -> Result<TaskRecord> {
    let now = now_secs();
    Ok(TaskRecord {
        id: Uuid::new_v4().to_string(),
        slug: unique_slug(app, &slugify(name))?,
        name: name.to_string(),
        project: None,
        kind,
        state: TaskState::Active,
        created_at: now,
        last_activity: now,
        parent_id: parent_id.map(str::to_string),
        placement: placement::for_new(),
        run_state: RunState::Run,
        run_state_by: None,
        max_children: None,
    })
}

/// Make `task`'s directory and fill it, then write its row with `insert`;
/// if either fails the directory goes again. Returns notes on project
/// files not copied.
fn write_task(
    app: &App,
    task: &TaskRecord,
    project: Option<&Project>,
    insert: impl FnOnce() -> Result<()>,
) -> Result<Vec<String>> {
    let dir = task_dir(app, task);
    std::fs::create_dir_all(app.tasks_root()).context("create the tasks directory")?;
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    match scaffold(&dir, task, project).and_then(|notes| insert().map(|_| notes)) {
        Ok(notes) => Ok(notes),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// A task to make under a parent: checked by [`plan_child`], made by
/// [`commit_child`].
#[derive(Debug, Clone)]
pub struct NewNode {
    /// Its row; the project is filled in as it is made.
    pub record: TaskRecord,
    pub brief: Option<TaskBriefRecord>,
    pub grant: Grant,
    pub budget: Option<Limits>,
    /// How many active tasks its parent may hold, checked again as the row
    /// is written; `None` under the assistant.
    pub fan_out: Option<u32>,
}

/// Plan task `name` of `kind`, which `caller` makes under the parent `d`
/// names (by default: the assistant, for the assistant and for a manager
/// the user makes; itself, for a manager). Checks, in this order (M2), and
/// writes
/// nothing: the parent (within the caller's subtree, for a task with a
/// parent), that the caller may make tasks under it, that it is active and
/// the assistant or a manager, the depth, its fan-out, the grant
/// ([`permissions::check_grant`]), the budget and, for a manager, the
/// harness. `max_children` is a manager's own fan-out limit.
pub fn plan_child(
    app: &App,
    caller: &Caller,
    kind: TaskKind,
    name: &str,
    d: &Delegation,
    max_children: Option<u32>,
) -> Result<NewNode> {
    // A worker has its prompt; a manager has only its brief to work
    // towards, so it is not made without one. First of all, before the
    // parent is resolved, which may make the assistant.
    if kind == TaskKind::Manager && d.goal.is_none() {
        bail!("a manager needs --goal or --brief-file: it is what it works towards");
    }
    let name = child_name(name)?;
    let goal = match d.goal.as_deref().map(messages::checked).transpose() {
        Ok(goal) => goal,
        Err(e) => bail!("--goal: {e:#}"),
    };
    let done_when = match d.done_when.as_deref().map(messages::checked).transpose() {
        Ok(done_when) => done_when,
        Err(e) => bail!("--done: {e:#}"),
    };
    if done_when.is_some() && goal.is_none() {
        bail!("--done says when --goal is met; give --goal too");
    }
    let policy = app.config.manager_policy()?;
    if let Some(n) = max_children.filter(|n| *n > policy.max_children) {
        bail!(
            "--max-children {n} is above `[managers] max_children` ({})",
            policy.max_children
        );
    }

    // Before the parent is resolved, which makes the assistant if the user's
    // manager would be its first child: a harness that refuses managers
    // leaves nothing behind.
    if kind == TaskKind::Manager {
        manager_harness(AgentSpec::from_config(&app.config)?.harness)?;
    }
    let target = match (&d.parent, caller, kind) {
        // A manager the user makes goes under the assistant.
        (None, Caller::User, TaskKind::Manager) => {
            Target::Parent(Some(crate::assistant::SLUG.to_string()))
        }
        (parent, _, _) => Target::Parent(parent.clone()),
    };
    let parent = match permissions::resolve(app, caller, &target) {
        Ok(parent) => parent,
        Err(e) => bail!("--parent: {e:#}"),
    };
    let verb = match kind {
        TaskKind::Manager => Verb::ManagerNew,
        _ => Verb::TaskNew,
    };
    permissions::in_scope(app, caller, verb, &parent)?;
    if parent.state != TaskState::Active {
        bail!("task {} is archived; it takes no new tasks", parent.slug);
    }
    if !matches!(parent.kind, TaskKind::Assistant | TaskKind::Manager) {
        bail!(
            "task {} is not a manager: only the assistant and managers have tasks under them",
            parent.slug
        );
    }
    let depth = app.db.depth(&parent.id)? + 1;
    let deepest = match kind {
        TaskKind::Manager => policy.max_depth.saturating_sub(1),
        _ => policy.max_depth,
    };
    if depth > deepest {
        bail!(
            "task {name} would sit {depth} levels below the assistant, under task {}; a {} \
             sits at most {deepest} below it (`[managers] max_depth` is {})",
            parent.slug,
            kind.as_str(),
            policy.max_depth
        );
    }
    let fan_out = fan_out_of(app, &parent)?;
    if let Some(max) = fan_out {
        let active = app.db.child_count(&parent.id, true)?;
        if active >= max as usize {
            bail!(crate::db::tasks::fan_out_full(&parent.slug, active, max));
        }
    }
    let grant = permissions::check_grant(app, caller, &parent, kind, d.allow.as_deref(), &policy)?;
    let budget = child_budget(app, &parent, kind, d, &policy)?;

    let created_by = match caller {
        Caller::User => "user".to_string(),
        Caller::Root => crate::manager::root(app)?.id,
        Caller::Node(node) => node.id.clone(),
        Caller::Review(_) => bail!("a review's agent makes no tasks"),
    };
    let record = TaskRecord {
        max_children,
        ..new_record(app, name, kind, Some(&parent.id))?
    };
    let brief = goal.map(|goal| TaskBriefRecord {
        task_id: record.id.clone(),
        goal,
        done_when,
        created_by,
    });
    Ok(NewNode {
        record,
        brief,
        grant,
        budget,
        fan_out,
    })
}

/// Refuse a manager on `harness` when it lacks what a manager needs: its
/// hooks, its deny rules, its interrupt. Checked as one is made and again
/// at each start, as the configured harness may have changed meanwhile.
pub fn manager_harness(harness: &dyn crate::harness::Harness) -> Result<()> {
    let missing = crate::harness::manager_missing(harness);
    if !missing.is_empty() {
        bail!(
            "a manager needs what {} lacks: {}; configure a harness that has them \
             (see docs/harnesses.md)",
            harness.name(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// Most characters a task made under a parent may have in its name.
pub const MAX_CHILD_NAME: usize = 100;

/// `name` as a task made under a parent may be called: a name the
/// assistant or a manager may have written, which reaches other agents
/// (their context lists it, its reports carry it), so on one line, short,
/// with no control character and no `<` or `>` that could open or close a
/// fence.
fn child_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a task needs a name");
    }
    if name.chars().count() > MAX_CHILD_NAME {
        bail!("a task's name may be at most {MAX_CHILD_NAME} characters");
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_control() || (c.is_whitespace() && *c != ' '))
    {
        bail!("a task's name may not contain {c:?}: keep it to one line of text");
    }
    if let Some(c) = name.chars().find(|c| format_char(*c)) {
        bail!("a task's name may not contain {c:?}: it would hide or reorder the text shown");
    }
    if name.contains(['<', '>']) {
        bail!("a task's name may not contain `<` or `>`: they would read as a fence tag");
    }
    Ok(name)
}

/// Bidirectional controls, zero-width and other invisible characters:
/// every format character (Unicode's Cf, as of Unicode 16), the whole tag
/// block, the combining grapheme joiner, the Hangul fillers and every
/// variation selector (a run of them encodes any bytes). Printed,
/// they reorder or hide the text around them, so a name could show as
/// another, or carry text the user never sees but an agent reads.
pub(crate) fn format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{115f}'..='\u{1160}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e007f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// A new child's budget: what `--budget` asks for, which must fit in what
/// is left above it, else `[managers] default_budget` for a manager or a
/// manager's child (M20), cut to what is left. A child of the assistant
/// gets none unless asked.
fn child_budget(
    app: &App,
    parent: &TaskRecord,
    kind: TaskKind,
    d: &Delegation,
    policy: &crate::config::ManagerPolicy,
) -> Result<Option<Limits>> {
    let left = budget::remaining(app, parent)?;
    // Spent in either unit, whatever is asked: nothing more is born there.
    if left.secs == Some(0) || left.turns == Some(0) {
        bail!("nothing is left of the budget under task {}", parent.slug);
    }
    let fits = |want: Option<i64>, left: Option<i64>| match (want, left) {
        (Some(want), Some(left)) => want <= left,
        _ => true,
    };
    if !d.budget.is_empty() {
        let want = budget::limits(&d.budget)?;
        if !fits(want.secs, left.secs) || !fits(want.turns, left.turns) {
            bail!(
                "--budget {} is more than is left under task {} ({})",
                budget::label(want),
                parent.slug,
                budget::label(left)
            );
        }
        return Ok(Some(want));
    }
    if kind != TaskKind::Manager && parent.kind != TaskKind::Manager {
        return Ok(None);
    }
    let cut = |want: Option<i64>, left: Option<i64>| match (want, left) {
        (Some(want), Some(left)) => Some(want.min(left)),
        (want, _) => want,
    };
    let given = Limits {
        secs: cut(policy.default_budget.secs, left.secs),
        turns: cut(policy.default_budget.turns, left.turns),
    };
    Ok((given != Limits::default()).then_some(given))
}

/// Make the task [`plan_child`] planned, with `project`'s files: its
/// directory, then its row, grant, budget and brief in one transaction
/// that refuses should the parent have been archived or filled up since;
/// the directory goes again if that fails. Returns notes as [`make_task`]
/// does. The held capabilities its grant names are for the caller to log,
/// once nothing can undo the task any more ([`record_child_held`]).
pub fn commit_child(
    app: &App,
    plan: &NewNode,
    project: Option<&Project>,
) -> Result<(TaskRecord, Vec<String>)> {
    let task = TaskRecord {
        project: project.map(|p| p.name.clone()),
        ..plan.record.clone()
    };
    let budget = plan.budget.map(|b| (b.secs, b.turns));
    let mut notes = write_task(app, &task, project, || {
        app.db.insert_child(
            &task,
            &plan.grant.caps,
            budget,
            plan.brief.as_ref(),
            plan.fan_out,
        )
    })?;
    // Only now, with its row in: the brief names its place in the tree.
    // Its session context carries the same, so a brief not written costs
    // the file, not the manager.
    if task.kind == TaskKind::Manager {
        if let Err(e) = manager::write_brief(app, &task) {
            notes.push(format!(
                "could not write task {}'s BRIEF.md: {e:#}",
                task.slug
            ));
        }
    }
    Ok((task, notes))
}

/// Log, and announce, the held capabilities `held` grants the task just
/// made ([`permissions::record_held`]): only once it is fully made, so no
/// entry names a task that was undone. A failure is a note, not an error.
fn record_child_held(
    app: &App,
    caller: &Caller,
    task: &TaskRecord,
    held: &[&str],
    notes: &mut Vec<String>,
) {
    if let Err(e) = permissions::record_held(app, caller, task, held) {
        notes.push(format!(
            "could not log the held capabilities granted to task {}: {e:#}",
            task.slug
        ));
    }
}

/// The marker, the project's files and the jeet skill, or for a manager
/// its role's files ([`manager::ensure_files`]) in the skill's place: the
/// worker's skill would tell it what a worker may do. Returns notes on
/// project files that were not copied.
fn scaffold(dir: &Path, task: &TaskRecord, project: Option<&Project>) -> Result<Vec<String>> {
    write_marker(dir, &task.id)?;
    let is_manager = task.kind == TaskKind::Manager;
    let notes = match project {
        Some(project) => projects::copy_files_for(project, dir, false, is_manager)?
            .notes()
            .into_iter()
            .map(|note| format!("project {}: {note}", project.name))
            .collect(),
        None => Vec::new(),
    };
    // Last, so nothing of the project's can shadow it.
    match task.kind {
        TaskKind::Manager => {
            manager::ensure_files(dir, manager::Role::Manager)?;
            if let Some(project) = project {
                link_project_instructions(dir, &project.name)?;
            }
        }
        TaskKind::Work | TaskKind::Assistant => install_skill(dir)?,
    }
    Ok(notes)
}

/// Have a manager's AGENTS.md import the project's instructions
/// ([`projects::MANAGER_INSTRUCTIONS`]), if it has them: a section at the
/// end, so the manager's own text stands first and is kept. Done once; an
/// AGENTS.md that already names the file is left alone.
pub fn link_project_instructions(dir: &Path, project: &str) -> Result<()> {
    use std::io::Write;
    let file = projects::MANAGER_INSTRUCTIONS;
    if !dir.join(file).is_file() {
        return Ok(());
    }
    let path = dir.join(instructions::AGENTS_MD);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    // The heading and the import together mark it as ours; a manager's own
    // mention of `@PROJECT.md` elsewhere does not. Known limit: should the
    // project later drop its AGENTS.md, the section stays and imports a
    // file that is gone (resync removes PROJECT.md, not the section).
    if text.contains(&format!("## Project: {project}\n")) && text.contains(&format!("\n@{file}\n"))
    {
        return Ok(());
    }
    let section = format!(
        "\n## Project: {project}\n\nYou were made from the `{project}` project. Its instructions \
         follow; they add to the above and do not replace it. Tasks you make \
         start from the project too.\n\n@{file}\n"
    );
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    out.write_all(section.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

/// Mark `dir` as task `id`'s directory.
pub fn write_marker(dir: &Path, id: &str) -> Result<()> {
    let marker = dir.join(MARKER_DIR);
    std::fs::create_dir_all(&marker).with_context(|| format!("create {}", marker.display()))?;
    std::fs::write(marker.join(MARKER_FILE), format!("{id}\n")).context("write the task marker")
}

/// Write the built-in jeet skill into a task directory.
pub fn install_skill(task_dir: &Path) -> Result<()> {
    let dir = task_dir.join(".claude").join("skills").join("jeet");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("SKILL.md"), SKILL).context("write the jeet skill")
}

/// The task a user means by `key`: its slug, its id, its name (any case), or
/// the start of exactly one slug.
pub fn find(app: &App, key: &str) -> Result<TaskRecord> {
    find_by(app, key, true)
}

/// Like [`find`], but never by a slug prefix: for deleting, where a name
/// that no longer matches its task exactly must not pick another one.
pub fn find_exact(app: &App, key: &str) -> Result<TaskRecord> {
    find_by(app, key, false)
}

fn find_by(app: &App, key: &str, by_prefix: bool) -> Result<TaskRecord> {
    let key = key.trim();
    // The assistant is no task to open, archive or delete by name.
    let listed = |task: &TaskRecord| task.kind != TaskKind::Assistant;
    if let Some(task) = app.db.get_task_by_slug(key)?.filter(listed) {
        return Ok(task);
    }
    if let Some(task) = app.db.get_task(key)?.filter(listed) {
        return Ok(task);
    }
    find_in(&app.db.list_tasks(None)?, key, by_prefix)
}

/// [`find`] (or, not `by_prefix`, [`find_exact`]) among `tasks` alone: by
/// slug, id, name (any case), then the start of exactly one slug.
pub fn find_in(tasks: &[TaskRecord], key: &str, by_prefix: bool) -> Result<TaskRecord> {
    let key = key.trim();
    // The assistant is no task to open, archive or delete by name.
    let tasks: Vec<&TaskRecord> = tasks
        .iter()
        .filter(|t| t.kind != TaskKind::Assistant)
        .collect();
    let by = |same: &dyn Fn(&TaskRecord) -> bool| tasks.iter().find(|t| same(t)).copied();
    if let Some(task) = by(&|t| t.slug == key).or_else(|| by(&|t| t.id == key)) {
        return Ok(task.clone());
    }
    let lower = key.to_lowercase();
    let mut matches: Vec<&TaskRecord> = tasks
        .iter()
        .copied()
        .filter(|t| t.name.to_lowercase() == lower)
        .collect();
    if matches.is_empty() && !key.is_empty() {
        let prefixed: Vec<&TaskRecord> = tasks
            .iter()
            .copied()
            .filter(|t| t.slug.starts_with(&lower))
            .collect();
        if !by_prefix && !prefixed.is_empty() {
            let slugs: Vec<&str> = prefixed.iter().map(|t| t.slug.as_str()).collect();
            bail!(
                "no task is named exactly {key:?} (did you mean {}?)",
                slugs.join(", ")
            );
        }
        matches = prefixed;
    }
    match matches.as_slice() {
        [] => bail!("no task named {key:?}; `jeet task ls` lists them"),
        [task] => Ok((*task).clone()),
        many => {
            let slugs: Vec<&str> = many.iter().map(|t| t.slug.as_str()).collect();
            bail!("{key:?} could be any of: {}", slugs.join(", "))
        }
    }
}

/// The task a command run here is about: the one named, else the one whose
/// agent runs this jeet ([`identity::identify`]), else the one in
/// `$JEET_TASK_ID` (set inside a task's agent session), else the task
/// whose directory holds the current directory.
pub fn current(app: &App, explicit: Option<&str>) -> Result<TaskRecord> {
    if let Some(key) = explicit {
        return find(app, key);
    }
    let own = identity::identify(app)?.task.map(|task| task.id);
    let hinted = || {
        std::env::var(TASK_ID_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let Some(id) = own.or_else(hinted) {
        let id = id.as_str();
        return match app.db.get_task(id)? {
            Some(task) if task.kind == TaskKind::Assistant => bail!(
                "the assistant's home is no task to work in; pick one with --task <name> \
                 (`jeet task ls` lists them)"
            ),
            Some(task) => Ok(task),
            None => bail!("${TASK_ID_ENV} is {id}, which is no task jeet knows; was it deleted?"),
        };
    }
    let cwd = std::env::current_dir().context("get cwd")?;
    match task_at(app, &cwd)? {
        Some(task) if task.kind == TaskKind::Assistant => bail!(
            "the assistant's home is no task to work in; pick one with --task <name> \
             (`jeet task ls` lists them)"
        ),
        Some(task) => Ok(task),
        None => bail!(
            "not inside a task: run this from a task's directory, or pick one with --task <name> \
             (`jeet task ls` lists them, `jeet task new <name>` makes one)"
        ),
    }
}

/// The task whose directory holds `dir`, if any.
pub fn task_at(app: &App, dir: &Path) -> Result<Option<TaskRecord>> {
    let Some((_, id)) = find_task_root(dir) else {
        return Ok(None);
    };
    match app.db.get_task(&id)? {
        Some(task) => Ok(Some(task)),
        // A review's directory is no task's.
        None if app.db.get_review(&id)?.is_some() => Ok(None),
        None => bail!(
            "{} is marked as task {id}, which is no task jeet knows",
            dir.display()
        ),
    }
}

/// Walk up from `dir` to the nearest task directory: its path and task id.
pub fn find_task_root(dir: &Path) -> Option<(PathBuf, String)> {
    for candidate in dir.ancestors() {
        let marker = candidate.join(MARKER_DIR).join(MARKER_FILE);
        if !marker.is_file() {
            continue;
        }
        let id = std::fs::read_to_string(&marker).ok()?.trim().to_string();
        if id.is_empty() {
            return None;
        }
        return Some((candidate.to_path_buf(), id));
    }
    None
}

/// `3 wt: api, web` — how many worktrees a task holds and of which repos.
pub fn worktree_summary(rows: &[TaskWorktreeRecord]) -> String {
    if rows.is_empty() {
        return "empty".to_string();
    }
    let mut repos: Vec<&str> = Vec::new();
    for row in rows {
        let name = row.repo_id.rsplit('/').next().unwrap_or(&row.repo_id);
        if !repos.contains(&name) {
            repos.push(name);
        }
    }
    format!("{} wt: {}", rows.len(), repos.join(", "))
}

/// Why a manager may not take a branch: its worktrees are read-only.
pub const MANAGER_READ_ONLY: &str =
    "a manager holds read-only worktrees; delegate the change to a worker (`jeet task new`)";

/// A worktree just added to a task.
pub struct AddedWorktree {
    pub record: TaskWorktreeRecord,
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// Add a worktree of `repo_spec` to `task` — the one way worktrees are made,
/// whether the CLI, the UI or the agent asks.
///
/// The repository is found in the store (cloned into it first if need be)
/// and fetched, so the worktree starts from what origin has now. Without a
/// branch it is detached at `origin/<default>`. With one, an existing local
/// branch is checked out, one that exists only on origin is tracked, and
/// anything else is created from `origin/<default>`.
///
/// A manager's worktrees are always detached and marked read-only, to read
/// and review from: a lint that keeps an honest manager from writing code,
/// not a guarantee (it runs unconfined, as the user).
pub fn add_worktree(
    app: &App,
    task: &TaskRecord,
    repo_spec: &str,
    branch: Option<&str>,
    name: Option<&str>,
) -> Result<AddedWorktree> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant keeps no worktrees; add them to a task with --task <name>");
    }
    if task.state == TaskState::Archived {
        bail!(
            "task {0} is archived; `jeet task unarchive {0}` first",
            task.slug
        );
    }
    let dir = task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let branch = match branch.map(str::trim) {
        Some("") => bail!("branch name cannot be empty"),
        other => other,
    };
    let read_only = match task.kind {
        TaskKind::Manager if branch.is_some() => bail!("{MANAGER_READ_ONLY}"),
        TaskKind::Manager => true,
        TaskKind::Work | TaskKind::Assistant => false,
    };
    if let Some(name) = name {
        validate_dir_name(name)?;
    }

    let repo = resolve_repo(app, repo_spec)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }

    let mut warnings = Vec::new();
    // Synchronously, unlike the background sync: the point is a fresh base.
    // Offline is no reason to refuse; the store's last fetch will do.
    if let Err(e) = store::sync_repo(app, &repo) {
        warnings.push(format!(
            "could not fetch {} ({e:#}); starting from the last fetch",
            repo.id
        ));
    }

    let rows = app.db.list_task_worktrees(&task.id)?;
    let taken: Vec<String> = rows.iter().map(|r| r.dir_name.clone()).collect();
    let dir_name = match name {
        Some(name) => {
            if taken.iter().any(|t| t == name) || dir.join(name).exists() {
                bail!("{name} already exists in task {}", task.slug);
            }
            name.to_string()
        }
        None => pick_dir_name(&dir, &taken, &repo_short_name(&repo)),
    };
    let dest = dir.join(&dir_name);
    let start = worktrees::resolve_start_point(&trunk, &repo.default_branch)?;

    let created_branch = match branch {
        None => {
            git::worktree_add_detached(&trunk, &dest, &start).context("add a detached worktree")?;
            false
        }
        Some(branch) => {
            let created = check_out_branch(app, &trunk, &dest, branch, &start)?;
            for other in app.db.task_worktrees_on_branch(&repo.id, branch)? {
                if other.task_id == task.id {
                    continue;
                }
                let holder = app.db.get_task(&other.task_id)?;
                if let Some(holder) = holder.filter(|t| t.state == TaskState::Archived) {
                    warnings.push(format!(
                        "archived task {} also uses {branch}; it cannot be unarchived while this \
                         worktree has it",
                        crate::commands::task::called(app, &holder)
                    ));
                }
            }
            created
        }
    };

    let mut record = TaskWorktreeRecord {
        id: 0,
        task_id: task.id.clone(),
        repo_id: repo.id.clone(),
        dir_name,
        branch: branch.map(str::to_string),
        created_branch,
        head_oid: None,
        stash_ref: None,
        pr_number: None,
        created_at: now_secs(),
        read_only,
    };
    // A `jeet task ls` running meanwhile reconciles: it can see the new
    // worktree in `git worktree list` before its row is written, and adopt
    // it as one an agent made. The row is then ours to fill in, not a reason
    // to undo the worktree.
    let inserted = app.db.insert_task_worktree(&record).or_else(|e| {
        match app.db.get_task_worktree(&task.id, &record.dir_name) {
            Ok(Some(adopted)) if adopted.repo_id == record.repo_id => {
                let row = TaskWorktreeRecord {
                    id: adopted.id,
                    ..record.clone()
                };
                app.db.update_task_worktree(&row).map(|_| adopted.id)
            }
            _ => Err(e),
        }
    });
    match inserted {
        Ok(id) => record.id = id,
        Err(e) => {
            let _ = git::worktree_remove(&trunk, &dest, true);
            if let (true, Some(branch)) = (created_branch, branch) {
                let _ = git::delete_local_branch(&trunk, branch);
            }
            return Err(e.context("record the worktree"));
        }
    }
    let _ = app.db.touch_task(&task.id, now_secs());
    Ok(AddedWorktree {
        record,
        path: dest,
        warnings,
    })
}

/// Check `branch` out at `dest`, returning whether this made the branch.
fn check_out_branch(
    app: &App,
    trunk: &Path,
    dest: &Path,
    branch: &str,
    start: &str,
) -> Result<bool> {
    if git::branch_exists(trunk, branch)? {
        // git refuses this too, but cannot say which task holds the branch.
        if let Some(holder) = branch_holder(app, trunk, branch)? {
            bail!("branch {branch} is already checked out {holder}");
        }
        git::worktree_add_existing_branch(trunk, dest, branch)
            .with_context(|| format!("check out {branch}"))?;
        return Ok(false);
    }
    if git::ref_exists(trunk, &format!("refs/remotes/origin/{branch}"))? {
        // Track the published branch rather than starting a new one from the
        // default branch, which would be pushed straight over it.
        git::worktree_add_new_branch(trunk, dest, branch, &format!("origin/{branch}"))
            .with_context(|| format!("check out origin/{branch}"))?;
        return Ok(true);
    }
    git::worktree_add_new_branch(trunk, dest, branch, start)
        .with_context(|| format!("create {branch}"))?;
    Ok(true)
}

/// Where `branch` is checked out already, phrased for an error: in which
/// task, named as the caller is shown it, or at which path.
pub(crate) fn branch_holder(app: &App, trunk: &Path, branch: &str) -> Result<Option<String>> {
    for entry in git::worktree_list(trunk)? {
        if entry.branch.as_deref() != Some(branch) {
            continue;
        }
        let in_review = match task_owning(app, &entry.path)? {
            Some(_) => None,
            None => review_owning(app, &entry.path)?,
        };
        return Ok(Some(match (task_owning(app, &entry.path)?, in_review) {
            (Some(task), _) => format!(
                "in task {} ({})",
                crate::commands::task::called(app, &task),
                entry.path.display()
            ),
            (None, Some(review)) => {
                format!("in review {} ({})", review.label(), entry.path.display())
            }
            (None, None) => format!("at {}", entry.path.display()),
        }));
    }
    Ok(None)
}

/// The slug of the task or review directory a worktree path is in, judged
/// by path alone.
fn owner_slug(app: &App, path: &Path) -> Option<String> {
    let root = canonical_tasks_root(app);
    let rel = path.strip_prefix(&root).ok()?;
    let slug = rel.components().next()?;
    Some(slug.as_os_str().to_string_lossy().to_string())
}

/// The task whose directory a worktree path is in, judged by path alone.
pub(crate) fn task_owning(app: &App, path: &Path) -> Result<Option<TaskRecord>> {
    match owner_slug(app, path) {
        Some(slug) => app.db.get_task_by_slug(&slug),
        None => Ok(None),
    }
}

/// The review whose directory a worktree path is in, judged by path alone.
pub(crate) fn review_owning(
    app: &App,
    path: &Path,
) -> Result<Option<crate::db::reviews::ReviewRecord>> {
    match owner_slug(app, path) {
        Some(slug) => app.db.find_review_by_slug(&slug),
        None => Ok(None),
    }
}

/// `git worktree list` reports symlink-resolved paths; compare against a
/// root resolved the same way.
fn canonical_tasks_root(app: &App) -> PathBuf {
    let root = app.tasks_root();
    root.canonicalize().unwrap_or(root)
}

/// The repository `spec` names: a URL (cloned into the store if it is not
/// there yet), an id, or a filter such as `api` or `acme/api`. A full id
/// that is not indexed is cloned from `https://<id>.git`.
fn resolve_repo(app: &App, spec: &str) -> Result<RepoRecord> {
    let spec = spec.trim();
    if spec.contains("://") || spec.starts_with("git@") {
        let identity = remote::parse_remote_url_anyhow(spec)?;
        if let Some(repo) = app.db.get_repo(&identity.id())? {
            return Ok(repo);
        }
        return clone_gated(app, spec);
    }
    match remote::identity_from_id(spec) {
        Ok(id) if id.host.contains('.') => match app.db.get_repo(spec)? {
            Some(repo) => Ok(repo),
            None => {
                let url = format!("https://{spec}.git");
                clone_gated(app, &url)
            }
        },
        _ => resolve::resolve_repo_filter(&app.db, spec),
    }
}

/// The id of the store repository `spec` names, if it is there: as
/// [`resolve_repo`], but never cloning, so safe to ask of what may yet be
/// refused.
fn known_repo_id(app: &App, spec: &str) -> Option<String> {
    let spec = spec.trim();
    let id = match spec.contains("://") || spec.starts_with("git@") {
        true => remote::parse_remote_url_anyhow(spec).ok()?.id(),
        false => match remote::identity_from_id(spec) {
            Ok(id) if id.host.contains('.') => spec.to_string(),
            _ => {
                return resolve::resolve_repo_filter(&app.db, spec)
                    .ok()
                    .map(|r| r.id)
            }
        },
    };
    app.db.get_repo(&id).ok().flatten().map(|r| r.id)
}

/// Clone `url` into the store for a caller [`permissions::may_clone`]
/// lets, logged as `jeet clone` would be.
fn clone_gated(app: &App, url: &str) -> Result<RepoRecord> {
    let authorized = permissions::may_clone(app, url)?;
    let cloned = crate::commands::clone::clone_into_store(app, url);
    if let Some(authorized) = authorized {
        authorized.record(cloned.as_ref().err().map(|e| format!("{e:#}")));
    }
    cloned
}

pub fn repo_short_name(repo: &RepoRecord) -> String {
    repo.id.rsplit('/').next().unwrap_or(&repo.id).to_string()
}

/// A worktree directory name given by hand: one plain path component that
/// is also safe inside a ref name.
fn validate_dir_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "{name:?} is not a usable worktree name: use letters, digits, `-`, `_` and `.`, \
             not starting with `.`"
        );
    }
    Ok(())
}

/// `base`, then `base-2`, `base-3`… — the first name neither a worktree of
/// the task nor anything else in its directory has.
pub fn pick_dir_name(task_dir: &Path, taken: &[String], base: &str) -> String {
    let base = match base.trim_start_matches('.') {
        "" => "repo",
        trimmed => trimmed,
    };
    let mut n = 1;
    loop {
        let candidate = numbered(base, n);
        if !taken.contains(&candidate) && !task_dir.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// `refs/jeet/<kind>/<task-id>/<dir>`, with `dir` escaped into one ref
/// component by [`ref_component`], so no two worktrees share a ref.
pub(crate) fn jeet_ref(kind: &str, task_id: &str, dir_name: &str) -> String {
    format!("refs/jeet/{kind}/{task_id}/{}", ref_component(dir_name))
}

/// `name` as a single ref component, one-to-one: ASCII letters, digits and
/// `-` stand for themselves, and every other byte becomes `_XX` (hex). So
/// `docs.site` and `docs-site` stay apart, a `/` cannot nest one worktree's
/// refs under another's, and no name can make an invalid ref.
fn ref_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => out.push(char::from(b)),
            _ => out.push_str(&format!("_{b:02X}")),
        }
    }
    out
}

/// Set `oid` aside under `refs/jeet/kept/<task-id>/<dir>/<kind>-<n>`: names
/// neither archive nor unarchive ever reuse, for work jeet could not put
/// back. It stays until the user deals with it or deletes the task, and
/// [`losses`] lists it until then.
pub fn keep(trunk: &Path, task_id: &str, dir_name: &str, kind: &str, oid: &str) -> Result<String> {
    let base = jeet_ref("kept", task_id, dir_name);
    let mut n = 1;
    loop {
        let name = format!("{base}/{kind}-{n}");
        if !git::ref_exists(trunk, &name)? {
            git::update_ref(trunk, &name, oid).with_context(|| format!("create {name}"))?;
            return Ok(name);
        }
        if git::rev_parse(trunk, &name).as_deref() == Some(oid) {
            return Ok(name);
        }
        n += 1;
    }
}

/// Every ref set aside by [`keep`] for the task or review `owner_id`, in
/// any repository: a worktree may have been removed since, taking its row
/// with it.
fn kept_refs(app: &App, owner_id: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    for repo in app.db.list_repos(None)? {
        let trunk = PathBuf::from(&repo.trunk_path);
        if !trunk.is_dir() {
            continue;
        }
        let Ok(names) = git::refs_under(&trunk, &format!("refs/jeet/kept/{owner_id}/")) else {
            continue;
        };
        for name in names {
            out.push((trunk.clone(), name));
        }
    }
    Ok(out)
}

/// Whether two snapshots hold the same files: tracked and untracked alike.
/// What they were taken on top of does not matter; the content does.
fn same_changes(trunk: &Path, a: &str, b: &str) -> bool {
    let trees = |c: &str| {
        (
            git::tree_of(trunk, c),
            git::tree_of(trunk, &format!("{c}^3")),
        )
    };
    let first = trees(a);
    first.0.is_some() && first == trees(b)
}

/// Before archive reuses a worktree's `refs/jeet/{head,stash}` names, set
/// aside whatever they hold that the worktree does not: left by an unarchive
/// that stopped part-way, they may be the only copy of someone's work. A
/// pinned HEAD the live one contains, or a snapshot of exactly what is in
/// the worktree now (an earlier archive attempt's), is not worth keeping.
fn set_aside_leftovers(
    trunk: &Path,
    task_id: &str,
    dir_name: &str,
    head: &str,
    snapshot: Option<&str>,
) -> Result<Vec<String>> {
    let mut kept = Vec::new();
    if let Some(old) = git::rev_parse(trunk, &jeet_ref("head", task_id, dir_name)) {
        if old != head
            && !git::is_ancestor(trunk, &old, head)
            && unreachable(trunk, &old) != Some(0)
        {
            kept.push(keep(trunk, task_id, dir_name, "head", &old)?);
        }
    }
    if let Some(old) = git::rev_parse(trunk, &jeet_ref("stash", task_id, dir_name)) {
        if !snapshot.is_some_and(|new| same_changes(trunk, &old, new)) {
            kept.push(keep(trunk, task_id, dir_name, "stash", &old)?);
        }
    }
    Ok(kept)
}

/// Remove a worktree from a task.
///
/// Refuses, unless forced, while it has uncommitted changes or commits that
/// are not pushed. A branch the task made is deleted with it only when
/// nothing would be lost: its commits are on origin or on the default branch.
pub fn remove_worktree(
    app: &App,
    task: &TaskRecord,
    name: &str,
    force: bool,
) -> Result<Vec<String>> {
    if task.state == TaskState::Archived {
        bail!(
            "task {0} is archived; `jeet task unarchive {0}` first",
            task.slug
        );
    }
    let mut warnings = Vec::new();
    if let Err(e) = reconcile(app) {
        warnings.push(format!("could not reconcile worktrees ({e:#})"));
    }
    let Some(row) = app.db.get_task_worktree(&task.id, name)? else {
        bail!(
            "task {} has no worktree named {name}; `jeet worktree ls` lists them",
            task.slug
        );
    };
    let repo = repo_of(app, &row)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    let path = worktree_dir(app, task, &row.dir_name);

    if path.exists() {
        if !force {
            let blockers = removal_blockers(&repo, &path);
            if !blockers.is_empty() {
                bail!(
                    "{name} has {}; pass --force to remove it anyway",
                    blockers.join(" and ")
                );
            }
        }
        // Pass the caller's choice through: without --force git re-checks for
        // modified and untracked files itself, catching anything new since.
        git::worktree_remove(&trunk, &path, force).context("remove the worktree")?;
    }
    let _ = git::worktree_prune(&trunk);

    if let (true, Some(branch)) = (row.created_branch, &row.branch) {
        if git::branch_exists(&trunk, branch)? {
            match unpushed(&trunk, &repo, &format!("refs/heads/{branch}")) {
                Some(0) => git::delete_local_branch(&trunk, branch)
                    .with_context(|| format!("delete branch {branch}"))?,
                _ => warnings.push(format!(
                    "kept branch {branch}: it has commits that are not pushed"
                )),
            }
        }
    }
    app.db.delete_task_worktree(&task.id, &row.dir_name)?;
    worktrees::prune_empty_parents(&path, &[task_dir(app, task)]);
    Ok(warnings)
}

fn repo_of(app: &App, row: &TaskWorktreeRecord) -> Result<RepoRecord> {
    app.db
        .get_repo(&row.repo_id)?
        .ok_or_else(|| anyhow::anyhow!("{} is no longer indexed", row.repo_id))
}

/// Commits on `rev` that are neither on a remote nor on the default branch.
/// A branch made from the default branch and never committed to has none.
fn unpushed(dir: &Path, repo: &RepoRecord, rev: &str) -> Option<usize> {
    let default = format!("refs/heads/{}", repo.default_branch);
    let mut exclude = vec!["--remotes"];
    if git::ref_exists(dir, &default).unwrap_or(false) {
        exclude.push(&default);
    }
    git::count_commits_not_in(dir, rev, &exclude)
}

/// Commits on `rev` that no branch, tag or remote holds: what a detached
/// HEAD would lose.
fn unreachable(dir: &Path, rev: &str) -> Option<usize> {
    git::count_commits_not_in(dir, rev, &["--remotes", "--branches", "--tags"])
}

/// Why removing the live worktree at `path` would lose work, if it would.
/// A probe that fails counts as a reason: it is never read as "clean".
fn removal_blockers(repo: &RepoRecord, path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    match git::status_counts(path) {
        Ok((0, _)) => {}
        Ok((dirty, _)) => out.push(plural(dirty, "uncommitted change")),
        Err(e) => out.push(format!("a status jeet could not read ({e})")),
    }
    let (commits, what) = match git::head_branch(path) {
        Some(_) => (unpushed(path, repo, "HEAD"), "unpushed commit"),
        None => (unreachable(path, "HEAD"), "commit on no branch"),
    };
    match commits {
        Some(0) => {}
        Some(n) => out.push(plural(n, what)),
        None => out.push("commits jeet could not check".to_string()),
    }
    out
}

pub(crate) fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Bring the index in line with what is on disk, for every active task.
///
/// A worktree git knows inside a task directory but the index does not — an
/// agent ran `git worktree add` itself — is adopted into that task. A row
/// whose directory has vanished is dropped, and git told to forget it.
/// Archived tasks are left alone: their rows describe worktrees that are
/// meant to be gone.
///
/// A worktree adopted into a manager is marked read-only, as one jeet made
/// for it would be. Returns a warning for each read-only worktree found on
/// a branch: a manager checked one out itself.
pub fn reconcile(app: &App) -> Result<Vec<String>> {
    let root = canonical_tasks_root(app);
    let tasks = app.db.list_tasks(Some(TaskState::Active))?;
    let by_slug: HashMap<&str, &TaskRecord> = tasks.iter().map(|t| (t.slug.as_str(), t)).collect();
    let mut prune = BTreeSet::new();
    let mut warnings = Vec::new();

    for repo in app.db.list_repos(None)? {
        let trunk = PathBuf::from(&repo.trunk_path);
        if !trunk.is_dir() {
            continue;
        }
        let Ok(entries) = git::worktree_list(&trunk) else {
            continue;
        };
        for entry in entries {
            let Some((slug, dir_name)) = split_task_path(&root, &entry.path) else {
                continue;
            };
            let Some(task) = by_slug.get(slug.as_str()) else {
                continue;
            };
            if !entry.path.exists() {
                continue;
            }
            let row = app.db.get_task_worktree(&task.id, &dir_name)?;
            let read_only = row
                .as_ref()
                .map_or(task.kind == TaskKind::Manager, |r| r.read_only);
            if let (true, Some(branch)) = (read_only, &entry.branch) {
                // The dir name is whatever the agent gave `git worktree add`.
                warnings.push(format!(
                    "{}/{} is read-only but on branch {}; {MANAGER_READ_ONLY}",
                    task.slug,
                    crate::commands::task::printable(&dir_name),
                    crate::commands::task::printable(branch)
                ));
            }
            match row {
                Some(mut row) if row.repo_id == repo.id && row.branch != entry.branch => {
                    // Someone switched branches in it. The branch it is on now
                    // is not one this task made, so it is never jeet's to delete.
                    row.branch = entry.branch.clone();
                    row.created_branch = false;
                    app.db.update_task_worktree(&row)?;
                }
                Some(_) => {}
                None => {
                    app.db.insert_task_worktree(&TaskWorktreeRecord {
                        id: 0,
                        task_id: task.id.clone(),
                        repo_id: repo.id.clone(),
                        dir_name,
                        branch: entry.branch.clone(),
                        created_branch: false,
                        head_oid: None,
                        stash_ref: None,
                        pr_number: None,
                        created_at: now_secs(),
                        read_only,
                    })?;
                }
            }
        }
    }

    for task in &tasks {
        for row in app.db.list_task_worktrees(&task.id)? {
            if root.join(&task.slug).join(&row.dir_name).exists() {
                continue;
            }
            // An archive that stopped part-way leaves rows holding what is
            // needed to restore the worktree; those are never forgotten.
            if row.head_oid.is_some() || row.stash_ref.is_some() {
                continue;
            }
            app.db.delete_task_worktree(&task.id, &row.dir_name)?;
            if let Some(repo) = app.db.get_repo(&row.repo_id)? {
                prune.insert(PathBuf::from(repo.trunk_path));
            }
        }
    }
    for trunk in prune {
        let _ = git::worktree_prune(&trunk);
    }
    Ok(warnings)
}

/// `(slug, dir_name)` for a path inside a task directory: `dir_name` is the
/// rest of the path, `/`-separated, so a worktree nested in a subdirectory of
/// the task is still the task's.
fn split_task_path(root: &Path, path: &Path) -> Option<(String, String)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string());
    let slug = parts.next()?;
    let rest: Vec<String> = parts.collect();
    if rest.is_empty() {
        return None;
    }
    Some((slug, rest.join("/")))
}

/// Charge a task in the tree the time its agent's turn in progress has run,
/// before its session is ended for archiving or deleting: no hook charges
/// it after (its pane is forgotten, or its row gone), and no sweep looks
/// at it again. A charge that uses a budget up is not reported, as a
/// stop's is not. Only while its agent's own pane runs: a window the user
/// opened in its session is no agent's turn.
fn charge_before_end(app: &App, task: &TaskRecord, warnings: &mut Vec<String>) {
    if task.parent_id.is_none() {
        return;
    }
    let alive = crate::tmux::available() && crate::tmux::agent_alive(&runtime::session_name(task));
    if let Err(e) = crate::status::fold_busy(app, task, alive, now_secs()) {
        warnings.push(format!("could not charge its budget: {e:#}"));
    }
}

/// Archive a task: end its agent and free the disk its worktrees take while
/// keeping what is needed to bring each back as it was.
///
/// For each worktree: uncommitted changes, untracked files included, are
/// saved under `refs/jeet/stash/…`; HEAD is pinned under `refs/jeet/head/…`;
/// both are recorded on its row, and only then is the worktree removed. The
/// task is marked archived first, so a failure part-way leaves an archived
/// task with some worktrees still present, and running archive again
/// finishes the job. It is refused from inside the agent's tmux session,
/// which ending the agent would end too.
///
/// Files git ignores are not kept: removing a worktree deletes them. The
/// caller has already confirmed what [`unkept`] lists.
///
/// It is refused while a task under it is active, before anything is done,
/// and checked again as it is marked archived; with `recursive`, every
/// active task under it is archived first, leaves first (M23), once all of
/// their agents have ended. The caller has then confirmed what
/// [`subtree_unkept`] lists.
pub fn archive(app: &App, task: &TaskRecord, recursive: bool) -> Result<Vec<String>> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant cannot be archived");
    }
    if !recursive {
        return archive_one(app, task);
    }
    let nodes: Vec<TaskRecord> = subtree_leaves_first(app, task)?
        .into_iter()
        .filter(|node| node.state == TaskState::Active)
        .collect();
    refuse_inside_any(&nodes, "archive")?;
    let mut warnings = end_agents(app, &nodes)?;
    // A failure part-way leaves those done archived, and their parents
    // active: running it again finishes the job.
    for node in &nodes {
        let done = archive_one(app, node).with_context(|| format!("archive task {}", node.slug))?;
        warnings.extend(done.into_iter().map(|w| format!("{}: {w}", node.slug)));
    }
    Ok(warnings)
}

/// [`archive`] one task.
fn archive_one(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    app.db.check_archive(&task.id)?;
    runtime::refuse_inside_session(task, "archive")?;
    let mut warnings = Vec::new();
    charge_before_end(app, task, &mut warnings);
    // The agent goes first, and is gone before anything is saved: it works
    // in the worktrees about to be removed, and what it wrote after the
    // snapshot would be lost.
    runtime::stop(task).context("end the agent's tmux session")?;
    // An agent that outlives its session is no longer the task's.
    app.db.delete_task_anchors(&task.id, AnchorKind::Pane)?;
    if let Err(e) = runtime::remove_settings(&app.home, &task.id) {
        warnings.push(format!("could not remove the agent's settings: {e:#}"));
    }
    if task.state == TaskState::Active {
        if let Err(e) = reconcile(app) {
            warnings.push(format!("could not reconcile worktrees ({e:#})"));
        }
    }
    app.db.archive_task(&task.id)?;
    let task_root = task_dir(app, task);

    for mut row in app.db.list_task_worktrees(&task.id)? {
        let save = |row: &TaskWorktreeRecord| app.db.update_task_worktree(row);
        warnings.extend(archive_worktree(
            app, &task.id, &task_root, &mut row, &save,
        )?);
    }
    Ok(warnings)
}

/// Archive one worktree of the task or review `owner_id`, whose directory
/// is `owner_root`: the worktree's uncommitted changes, untracked files
/// included, are saved under `refs/jeet/stash/…`, HEAD is pinned under
/// `refs/jeet/head/…`, both are recorded on `row`, which `save` writes
/// before anything is removed, and only then is the worktree removed. One
/// that is gone already is skipped. Returns what to warn of.
pub(crate) fn archive_worktree(
    app: &App,
    owner_id: &str,
    owner_root: &Path,
    row: &mut TaskWorktreeRecord,
    save: &dyn Fn(&TaskWorktreeRecord) -> Result<()>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let path = owner_root.join(&row.dir_name);
    if !path.exists() {
        return Ok(warnings);
    }
    let repo = repo_of(app, row)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    let what = row.dir_name.clone();
    let what = &what;

    let Some(head) = git::rev_parse(&path, "HEAD") else {
        bail!("{what}: could not read its HEAD");
    };
    let ignored = git::status_counts(&path).map(|(_, i)| i).unwrap_or(0);
    let snapshot = git::snapshot_changes(&path)
        .with_context(|| format!("{what}: save its uncommitted changes"))?;

    let kept = set_aside_leftovers(&trunk, owner_id, what, &head, snapshot.as_deref())
        .with_context(|| format!("{what}: keep what an earlier unarchive left"))?;
    for name in kept {
        warnings.push(format!(
            "{what}: work an earlier unarchive could not put back is kept at {name}"
        ));
    }
    let head_ref = jeet_ref("head", owner_id, what);
    git::update_ref(&trunk, &head_ref, &head).with_context(|| format!("{what}: pin HEAD"))?;
    let stash_ref = jeet_ref("stash", owner_id, what);
    row.stash_ref = match snapshot {
        Some(oid) => {
            git::update_ref(&trunk, &stash_ref, &oid)
                .with_context(|| format!("{what}: keep its uncommitted changes"))?;
            Some(stash_ref)
        }
        None => {
            // Left by an earlier attempt, and stale now there is nothing;
            // anything else it held was set aside above.
            let _ = git::delete_ref(&trunk, &stash_ref);
            None
        }
    };
    let branch = git::head_branch(&path);
    if branch != row.branch {
        row.created_branch = false;
        row.branch = branch;
    }
    row.head_oid = Some(head);
    // Recorded before anything is removed: from here on the row alone is
    // enough to restore the worktree.
    save(row)?;

    git::worktree_remove(&trunk, &path, true)
        .with_context(|| format!("{what}: remove the worktree"))?;
    worktrees::prune_empty_parents(&path, std::slice::from_ref(&owner_root.to_path_buf()));
    if ignored > 0 {
        warnings.push(format!(
            "{what}: {} not kept",
            plural(ignored, "ignored file or directory")
        ));
    }
    Ok(warnings)
}

/// The most ignored paths [`unkept`] names per worktree before summing up the rest.
const UNKEPT_SHOWN: usize = 5;

/// What archiving `task` would not keep, one line per worktree: the files
/// git ignores in it — a `.env`, local config, build output — which go with
/// the worktree, as no snapshot holds them. A worktree whose ignored files
/// cannot be listed is named too: unknown is never read as "nothing".
pub fn unkept(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    if task.state == TaskState::Active {
        let _ = reconcile(app);
    }
    let rows = app.db.list_task_worktrees(&task.id)?;
    Ok(unkept_in(&task_dir(app, task), &rows))
}

/// [`unkept`] for the worktrees `rows` in `root`.
pub(crate) fn unkept_in(root: &Path, rows: &[TaskWorktreeRecord]) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows {
        let path = root.join(&row.dir_name);
        if !path.exists() {
            continue;
        }
        match git::ignored_paths(&path) {
            Ok(paths) if paths.is_empty() => {}
            Ok(paths) => out.push(format!("{}: {}", row.dir_name, name_some(&paths))),
            Err(e) => out.push(format!(
                "{}: could not list the files git ignores ({e})",
                row.dir_name
            )),
        }
    }
    out
}

/// What archiving `task` and every active task below it would not keep:
/// [`unkept`] for each, leaves first, `task` last.
pub fn subtree_unkept(app: &App, task: &TaskRecord) -> Result<Vec<(TaskRecord, Vec<String>)>> {
    subtree_leaves_first(app, task)?
        .into_iter()
        .filter(|node| node.state == TaskState::Active)
        .map(|node| Ok((node.clone(), unkept(app, &node)?)))
        .collect()
}

/// The first few of `paths`, then how many more.
fn name_some(paths: &[String]) -> String {
    let mut out = paths
        .iter()
        .take(UNKEPT_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > UNKEPT_SHOWN {
        out.push_str(&format!(" and {} more", paths.len() - UNKEPT_SHOWN));
    }
    out
}

/// A git checkout on a task's canvas that is none of the task's worktrees:
/// a clone made there, or a worktree of a repository the store does not
/// have. Deleting the task deletes it with the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignCheckout {
    pub path: PathBuf,
    /// For a linked worktree, the repository it belongs to (its common git
    /// directory), which needs telling once it is gone. None for a clone.
    pub common_dir: Option<PathBuf>,
}

/// Every git checkout under `dir` that is none of `own`, outermost only:
/// what is inside a checkout is that checkout's. Symlinks are not followed.
fn checkouts_under(dir: &Path, own: &[PathBuf]) -> Vec<ForeignCheckout> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut walk = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walk.next() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        if own.iter().any(|o| o == path) {
            walk.skip_current_dir();
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(path.join(".git")) else {
            continue;
        };
        walk.skip_current_dir();
        // A `.git` file points into another repository's git directory.
        let common_dir = match meta.is_file() {
            true => git::git_common_dir(path).ok(),
            false => None,
        };
        out.push(ForeignCheckout {
            path: path.to_path_buf(),
            common_dir,
        });
    }
    out
}

/// One line of [`losses`] for a checkout the task does not manage.
fn describe_foreign(dir: &Path, checkout: &ForeignCheckout) -> String {
    let rel = checkout
        .path
        .strip_prefix(dir)
        .unwrap_or(&checkout.path)
        .display()
        .to_string();
    let mut line = match &checkout.common_dir {
        Some(common) => {
            let repo = match common.file_name().is_some_and(|n| n == ".git") {
                true => common.parent().unwrap_or(common),
                false => common.as_path(),
            };
            format!(
                "{rel}: a worktree of {}, not one of the task's, deleted with it",
                repo.display()
            )
        }
        None => {
            format!("{rel}: a git clone on the task's canvas, deleted with it and every commit only it has")
        }
    };
    match git::status_counts(&checkout.path) {
        Ok((0, _)) => {}
        Ok((dirty, _)) => line.push_str(&format!(" ({})", plural(dirty, "uncommitted change"))),
        Err(e) => line.push_str(&format!(" (could not read its status: {e})")),
    }
    line
}

/// Bring an archived task's worktrees back: each on its branch, or detached
/// at its old HEAD, with its uncommitted changes reapplied.
///
/// Worktrees already present are left as they are, so running it again after
/// a failure picks up where it stopped. The task becomes active only once
/// every worktree is back.
///
/// It is refused under an archived parent, and when the parent already has
/// as many active tasks under it as it may, before anything is done, and
/// checked again as it is marked active. With `recursive`, every archived
/// task under it comes back too, top-down, each under a parent already
/// back (M23). Each is held to its parent's limit, as a manager may have
/// made others since, so one that would be over it stops the walk; running
/// it again after freeing room finishes the job.
pub fn unarchive(app: &App, task: &TaskRecord, recursive: bool) -> Result<Vec<String>> {
    if !recursive {
        return unarchive_one(app, task, true);
    }
    let mut warnings = Vec::new();
    for node in subtree_leaves_first(app, task)?.into_iter().rev() {
        if node.state != TaskState::Archived {
            continue;
        }
        let done = unarchive_one(app, &node, true)
            .with_context(|| format!("unarchive task {}", node.slug))?;
        warnings.extend(done.into_iter().map(|w| format!("{}: {w}", node.slug)));
    }
    Ok(warnings)
}

/// [`unarchive`] one task, held to its parent's limit when `limit`.
fn unarchive_one(app: &App, task: &TaskRecord, limit: bool) -> Result<Vec<String>> {
    let fan_out = match limit {
        true => fan_out_of_parent(app, task)?,
        false => None,
    };
    app.db.check_unarchive(&task.id, fan_out)?;
    let dir = task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let mut warnings = Vec::new();

    for mut row in app.db.list_task_worktrees(&task.id)? {
        let save = |row: &TaskWorktreeRecord| app.db.update_task_worktree(row);
        match restore_worktree(app, &task.id, &dir, &mut row, &save, &mut warnings)? {
            Restored::Forgotten => app.db.delete_task_worktree(&task.id, &row.dir_name)?,
            Restored::Kept => {}
        }
    }

    app.db.unarchive_task(&task.id, fan_out)?;
    app.db.touch_task(&task.id, now_secs())?;
    Ok(warnings)
}

/// What [`restore_worktree`] did with a worktree.
pub(crate) enum Restored {
    /// Present, back or already there.
    Kept,
    /// Nothing was recorded to restore it from: its row is to go.
    Forgotten,
}

/// Bring back one archived worktree of the task or review `owner_id`, whose
/// directory is `owner_root`: on its branch, or detached at its old HEAD,
/// with its uncommitted changes reapplied. One already present is left as
/// it is, so running it again after a failure picks up where it stopped.
/// `row` is updated as work is put back and written by `save`; what to warn
/// of goes to `warnings`.
pub(crate) fn restore_worktree(
    app: &App,
    owner_id: &str,
    owner_root: &Path,
    row: &mut TaskWorktreeRecord,
    save: &dyn Fn(&TaskWorktreeRecord) -> Result<()>,
    warnings: &mut Vec<String>,
) -> Result<Restored> {
    let path = owner_root.join(&row.dir_name);
    if path.exists() {
        return Ok(Restored::Kept);
    }
    let what = row.dir_name.clone();
    let Some(head) = row.head_oid.clone() else {
        warnings.push(format!(
            "{what}: nothing was recorded to restore it from; forgotten"
        ));
        return Ok(Restored::Forgotten);
    };
    let repo = repo_of(app, row)?;
    let trunk = PathBuf::from(&repo.trunk_path);
    let head_ref = jeet_ref("head", owner_id, &what);
    let mut keep_head_ref = false;

    match row.branch.clone() {
        Some(branch) if git::branch_exists(&trunk, &branch)? => {
            if let Some(holder) = branch_holder(app, &trunk, &branch)? {
                bail!(
                    "{what}: branch {branch} is checked out {holder}; free it and unarchive again"
                );
            }
            git::worktree_add_existing_branch(&trunk, &path, &branch)
                .with_context(|| format!("{what}: check out {branch}"))?;
            let tip = git::rev_parse(&trunk, &format!("refs/heads/{branch}"));
            if tip.as_deref() != Some(head.as_str())
                && !tip.is_some_and(|tip| git::is_ancestor(&trunk, &head, &tip))
            {
                // Under a name of its own: the next archive reuses
                // head_ref, and the task is active again either way.
                let kept = match keep(&trunk, owner_id, &what, "head", &head) {
                    Ok(kept) => kept,
                    Err(_) => {
                        keep_head_ref = true;
                        head_ref.clone()
                    }
                };
                warnings.push(format!(
                    "{what}: {branch} moved while the task was archived; \
                     the commit it was on is kept at {kept}"
                ));
            }
        }
        Some(branch) => {
            git::worktree_add_new_branch(&trunk, &path, &branch, &head)
                .with_context(|| format!("{what}: recreate {branch}"))?;
            warnings.push(format!(
                "{what}: branch {branch} had been deleted; recreated it where it was"
            ));
        }
        None => git::worktree_add_detached(&trunk, &path, &head)
            .with_context(|| format!("{what}: check out {head}"))?,
    }

    if let Some(stash) = row.stash_ref.clone() {
        // Staged changes come back staged when they can.
        let applied = git::stash_apply(&path, &stash, true)
            .or_else(|_| git::stash_apply(&path, &stash, false));
        match applied {
            Ok(()) => {
                let _ = git::delete_ref(&trunk, &stash);
                row.stash_ref = None;
            }
            Err(e) => {
                // Moved under a name of its own, as archive reuses this one.
                let kept = git::rev_parse(&trunk, &stash)
                    .and_then(|oid| keep(&trunk, owner_id, &what, "stash", &oid).ok());
                let at = match kept {
                    Some(kept) => {
                        let _ = git::delete_ref(&trunk, &stash);
                        row.stash_ref = None;
                        kept
                    }
                    None => stash,
                };
                warnings.push(format!(
                    "{what}: could not reapply its uncommitted changes ({e:#}); \
                     they are kept at {at}: `git stash apply {at}` in the worktree \
                     retries, and `git update-ref -d {at}` drops them"
                ));
            }
        }
    }
    if !keep_head_ref {
        let _ = git::delete_ref(&trunk, &head_ref);
        row.head_oid = None;
    }
    save(row)?;
    Ok(Restored::Kept)
}

/// How many active tasks `task`'s parent may hold: its own `max_children`,
/// else, for a manager, `[managers] max_children`. The assistant has no
/// limit, nor does a task with no parent.
fn fan_out_of_parent(app: &App, task: &TaskRecord) -> Result<Option<u32>> {
    let Some(parent_id) = &task.parent_id else {
        return Ok(None);
    };
    // A parent that is gone is refused by the check that follows.
    match app.db.get_task(parent_id)? {
        Some(parent) => fan_out_of(app, &parent),
        None => Ok(None),
    }
}

/// How many active tasks `parent` may hold: its own `max_children`, else,
/// for a manager, `[managers] max_children`. The assistant has no limit.
fn fan_out_of(app: &App, parent: &TaskRecord) -> Result<Option<u32>> {
    match parent.kind {
        TaskKind::Assistant => Ok(None),
        TaskKind::Manager => match parent.max_children {
            Some(n) => Ok(Some(n)),
            None => Ok(Some(app.config.manager_policy()?.max_children)),
        },
        TaskKind::Work => Ok(parent.max_children),
    }
}

/// What deleting `task` would destroy, one line per problem: uncommitted
/// changes (live, or saved at archive time), and commits that would be left
/// on no branch, tag or remote — a detached HEAD's, those of a branch the
/// task made (delete removes it), a HEAD pinned at archive that the branch
/// has since moved away from, and anything set aside under `refs/jeet/kept`.
/// A branch the task did not make keeps its commits, so it is not listed.
/// Every git checkout on the canvas that is none of the task's worktrees —
/// a clone, or a worktree of a repository outside the store — is listed,
/// as it goes with the task directory.
pub fn losses(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    if task.state == TaskState::Active {
        let _ = reconcile(app);
    }
    let rows = app.db.list_task_worktrees(&task.id)?;
    losses_of(app, &task.id, &task_dir(app, task), &rows)
}

/// [`losses`] of the task or review `owner_id`, whose directory is
/// `owner_root` and whose worktrees are `rows`.
pub(crate) fn losses_of(
    app: &App,
    owner_id: &str,
    owner_root: &Path,
    rows: &[TaskWorktreeRecord],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for row in rows {
        let what = &match &row.branch {
            Some(branch) => format!("{} ({branch})", row.dir_name),
            None => row.dir_name.clone(),
        };
        let Some(repo) = app.db.get_repo(&row.repo_id)? else {
            continue;
        };
        let trunk = PathBuf::from(&repo.trunk_path);
        let path = owner_root.join(&row.dir_name);
        let live = path.exists();

        if live {
            match git::status_counts(&path) {
                Ok((0, _)) => {}
                Ok((dirty, _)) => {
                    out.push(format!("{what}: {}", plural(dirty, "uncommitted change")))
                }
                Err(e) => out.push(format!("{what}: could not read its status ({e})")),
            }
        }
        // Saved at archive; on a live worktree, ones unarchive did not get to
        // reapply.
        let stash = row
            .stash_ref
            .clone()
            .unwrap_or_else(|| jeet_ref("stash", owner_id, &row.dir_name));
        if git::ref_exists(&trunk, &stash).unwrap_or(false) {
            out.push(format!(
                "{what}: uncommitted changes saved at archive ({stash})"
            ));
        }

        // The commits at risk are a detached HEAD's, or those of a branch the
        // task made — which delete removes, so it cannot protect them itself.
        let at_risk = if live {
            match git::head_branch(&path) {
                None => Some((path.clone(), "HEAD".to_string(), None)),
                Some(branch) if Some(&branch) == row.branch.as_ref() && row.created_branch => {
                    Some((path.clone(), format!("refs/heads/{branch}"), Some(branch)))
                }
                Some(_) => None,
            }
        } else {
            match (&row.branch, row.created_branch, &row.head_oid) {
                (None, _, Some(head)) => Some((trunk.clone(), head.clone(), None)),
                (Some(branch), true, _) if git::branch_exists(&trunk, branch).unwrap_or(false) => {
                    Some((
                        trunk.clone(),
                        format!("refs/heads/{branch}"),
                        Some(branch.clone()),
                    ))
                }
                _ => None,
            }
        };
        let mut counted = Vec::new();
        if let Some((dir, rev, own_branch)) = at_risk {
            let mut exclude = vec!["--remotes".to_string(), "--tags".to_string()];
            if let Some(branch) = own_branch {
                exclude.push(format!("--exclude={branch}"));
            }
            exclude.push("--branches".to_string());
            let exclude: Vec<&str> = exclude.iter().map(String::as_str).collect();
            match git::count_commits_not_in(&dir, &rev, &exclude) {
                Some(0) => {}
                Some(n) => out.push(format!(
                    "{what}: {}",
                    plural(n, "commit on no other branch or remote")
                )),
                None => out.push(format!("{what}: could not check for unpushed commits")),
            }
            counted.extend(git::rev_parse(&dir, &rev));
        }
        if live {
            counted.extend(git::rev_parse(&path, "HEAD"));
        }

        // HEAD as pinned at archive. Normally its branch or the commits just
        // counted hold it, but not if the branch moved while the task was
        // archived, or an unarchive stopped before releasing it.
        let head_ref = jeet_ref("head", owner_id, &row.dir_name);
        if git::ref_exists(&trunk, &head_ref).unwrap_or(false) {
            let mut exclude = vec!["--remotes", "--tags", "--branches"];
            exclude.extend(counted.iter().map(String::as_str));
            match git::count_commits_not_in(&trunk, &head_ref, &exclude) {
                Some(0) => {}
                Some(n) => out.push(format!(
                    "{what}: {} kept at {head_ref}",
                    plural(n, "commit on no branch or remote")
                )),
                None => out.push(format!("{what}: could not check {head_ref}")),
            }
        }
    }

    // Work unarchive could not put back, whichever worktree it came from.
    for (trunk, name) in kept_refs(app, owner_id)? {
        let kind = name.rsplit('/').next().unwrap_or_default();
        if kind.starts_with("stash-") {
            out.push(format!(
                "{name}: uncommitted changes unarchive could not reapply"
            ));
            continue;
        }
        match unreachable(&trunk, &name) {
            Some(0) => {}
            Some(n) => out.push(format!(
                "{name}: {}",
                plural(n, "commit on no branch or remote")
            )),
            None => out.push(format!("{name}: could not check its commits")),
        }
    }

    // Checkouts on the canvas that are none of the task's worktrees: the
    // directory goes, and they go with it.
    let own: Vec<PathBuf> = rows
        .iter()
        .map(|row| owner_root.join(&row.dir_name))
        .collect();
    for checkout in checkouts_under(owner_root, &own) {
        out.push(describe_foreign(owner_root, &checkout));
    }
    Ok(out)
}

/// Delete a task and everything it owns: its agent's tmux session, its
/// worktrees, the branches it made, its `refs/jeet` refs, its directory —
/// with whatever else is on its canvas, other git checkouts included — and
/// its rows. Branches it did not make are left alone. The caller has already
/// confirmed what [`losses`] lists.
///
/// The rows go last, so a failure part-way leaves a task that can be deleted
/// again. It is refused, before anything is done, while any task sits under
/// it (see [`refuse_delete_with_children`]); with `recursive`, every task
/// under it goes too (M23): all their agents are ended, leaves first, then
/// each is removed from disk, leaves first, and their rows go together, in
/// one transaction. The caller has then confirmed what [`subtree_losses`]
/// lists.
pub fn delete(app: &App, task: &TaskRecord, recursive: bool) -> Result<Vec<String>> {
    // Its directory is the assistant's home: its notes, its todo list.
    if task.kind == TaskKind::Assistant {
        bail!("the assistant cannot be deleted");
    }
    let nodes = match recursive {
        true => subtree_leaves_first(app, task)?,
        false => {
            refuse_delete_with_children(app, task)?;
            vec![task.clone()]
        }
    };
    for node in &nodes {
        if node.slug.is_empty() || node.slug.starts_with('.') || node.slug.contains(['/', '\\']) {
            bail!(
                "refusing to delete a task whose directory name is {:?}",
                node.slug
            );
        }
    }
    refuse_inside_any(&nodes, "delete")?;
    if !recursive {
        let warnings = remove_from_disk(app, task)?;
        app.db.delete_task(&task.id)?;
        return Ok(warnings);
    }
    let mut warnings = end_agents(app, &nodes)?;
    // A manager may have made a task just before it was ended: its row
    // would refuse to go once the disk was gone. With every agent ended,
    // nothing below makes more, so running it again takes it too.
    let now = subtree_leaves_first(app, task)?;
    if let Some(new) = now
        .iter()
        .find(|node| !nodes.iter().any(|old| old.id == node.id))
    {
        bail!(
            "task {} was made under task {} as its tree was being ended; nothing was \
             deleted, so run it again",
            new.slug,
            task.slug
        );
    }
    for node in &nodes {
        let done =
            remove_from_disk(app, node).with_context(|| format!("delete task {}", node.slug))?;
        warnings.extend(done.into_iter().map(|w| format!("{}: {w}", node.slug)));
    }
    let ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
    app.db.delete_rows_leaves_first(&ids)?;
    Ok(warnings)
}

/// [`delete`] everything of one task but its row: its agent, its worktrees,
/// branches and refs, its directory and its settings.
fn remove_from_disk(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    // Stopped, and gone, before anything is removed: a running agent would
    // carry on working in worktrees that are about to go, and its exit hook
    // would write into the task directory as it was being removed. Charged
    // first, so what its turn ran goes up to its parent with the rest.
    charge_before_end(app, task, &mut warnings);
    runtime::stop(task).context("end the agent's tmux session")?;
    if task.state == TaskState::Active {
        if let Err(e) = reconcile(app) {
            warnings.push(format!("could not reconcile worktrees ({e:#})"));
        }
    }
    let dir = task_dir(app, task);
    let rows = app.db.list_task_worktrees(&task.id)?;
    remove_owned(app, &task.id, &dir, &rows, &mut warnings)?;
    if let Err(e) = runtime::remove_settings(&app.home, &task.id) {
        warnings.push(format!("could not remove the agent's settings: {e:#}"));
    }
    Ok(warnings)
}

/// Remove everything on disk that the task or review `owner_id` owns,
/// whose directory is `dir` and whose worktrees are `rows`: the worktrees,
/// the branches it made (kept when checked out elsewhere), its
/// `refs/jeet` refs, and the directory with whatever else is on its canvas.
pub(crate) fn remove_owned(
    app: &App,
    owner_id: &str,
    dir: &Path,
    rows: &[TaskWorktreeRecord],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut trunks: Vec<PathBuf> = Vec::new();

    for row in rows {
        let Some(repo) = app.db.get_repo(&row.repo_id)? else {
            continue;
        };
        let trunk = PathBuf::from(&repo.trunk_path);
        let path = dir.join(&row.dir_name);
        if path.exists() {
            git::worktree_remove(&trunk, &path, true)
                .with_context(|| format!("{}: remove the worktree", row.dir_name))?;
        }
        if let (true, Some(branch)) = (row.created_branch, &row.branch) {
            if git::branch_exists(&trunk, branch)? {
                match branch_holder(app, &trunk, branch)? {
                    Some(holder) => {
                        warnings.push(format!("kept branch {branch}: it is checked out {holder}"))
                    }
                    None => git::delete_local_branch(&trunk, branch)
                        .with_context(|| format!("delete branch {branch}"))?,
                }
            }
        }
        if !trunks.contains(&trunk) {
            trunks.push(trunk);
        }
    }

    for trunk in &trunks {
        for kind in ["stash", "head"] {
            for name in git::refs_under(trunk, &format!("refs/jeet/{kind}/{owner_id}/"))? {
                git::delete_ref(trunk, &name).with_context(|| format!("delete {name}"))?;
            }
        }
        let _ = git::worktree_prune(trunk);
    }
    for (trunk, name) in kept_refs(app, owner_id)? {
        git::delete_ref(&trunk, &name).with_context(|| format!("delete {name}"))?;
    }
    // Worktrees of other repositories on the canvas go with the directory;
    // their repositories are told, so they do not keep a record of them.
    let own: Vec<PathBuf> = rows.iter().map(|row| dir.join(&row.dir_name)).collect();
    let foreign = checkouts_under(dir, &own);
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    for common in foreign.iter().filter_map(|c| c.common_dir.as_ref()) {
        let _ = git::worktree_prune(common);
    }
    Ok(())
}

/// What deleting `task` and everything below it would destroy: [`losses`]
/// for each, leaves first, `task` last.
pub fn subtree_losses(app: &App, task: &TaskRecord) -> Result<Vec<(TaskRecord, Vec<String>)>> {
    subtree_leaves_first(app, task)?
        .into_iter()
        .map(|node| Ok((node.clone(), losses(app, &node)?)))
        .collect()
}

/// `task` and everything below it, leaves first, `task` last.
pub fn subtree_leaves_first(app: &App, task: &TaskRecord) -> Result<Vec<TaskRecord>> {
    let mut nodes: Vec<TaskRecord> = app
        .db
        .descendants(&task.id)?
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    nodes.push(task.clone());
    Ok(nodes)
}

/// Refuse to `verb` `nodes` when this jeet runs inside the agent's session
/// of any of them: ending it would end this jeet part-way through.
pub fn refuse_inside_any(nodes: &[TaskRecord], verb: &str) -> Result<()> {
    for node in nodes {
        runtime::refuse_inside_session(node, verb)?;
    }
    Ok(())
}

/// End the agent of each of `nodes`, in order, as a stop does
/// ([`manager::run::end_agent`]), and forget its pane, before any of them
/// is torn down: a manager left running would go on making tasks, and
/// starting them, under one being removed. What each turn ran is charged
/// first.
fn end_agents(app: &App, nodes: &[TaskRecord]) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for node in nodes {
        let mut charged = Vec::new();
        charge_before_end(app, node, &mut charged);
        warnings.extend(charged.into_iter().map(|w| format!("{}: {w}", node.slug)));
        manager::run::end_agent(app, node)
            .with_context(|| format!("end task {}'s agent", node.slug))?;
        app.db.delete_task_anchors(&node.id, AnchorKind::Pane)?;
    }
    Ok(warnings)
}

/// Refuse to delete `task` while any task sits under it, archived ones
/// included: its row cannot go before theirs, so nothing of it on disk may
/// go first either.
pub fn refuse_delete_with_children(app: &App, task: &TaskRecord) -> Result<()> {
    let n = app.db.child_count(&task.id, false)?;
    if n > 0 {
        bail!(
            "{} has {} under it, archived ones included; delete them first",
            task.slug,
            plural(n, "task")
        );
    }
    Ok(())
}

/// A move of tasks to a new parent, planned and checked, nothing written:
/// what [`Database::apply_move`] writes, and what to say of it.
///
/// [`Database::apply_move`]: crate::db::Database::apply_move
#[derive(Debug, Clone)]
pub struct MovePlan {
    /// As they were read, under their old parent.
    pub movers: Vec<TaskRecord>,
    pub to: TaskRecord,
    /// The task being deleted, whose children the movers are.
    pub deleting: Option<TaskRecord>,
    /// Each node whose grant drops a capability, by slug, with what it
    /// drops.
    pub lost: Vec<(String, Vec<String>)>,
    pub rows: MoveRows,
}

/// A move done: its plan, the reports moved with it, and what it told
/// whom, one line each.
#[derive(Debug, Clone)]
pub struct Moved {
    pub plan: MovePlan,
    pub reports: Vec<i64>,
    pub lines: Vec<String>,
}

/// Plan moving `movers`, with everything below them, under `to`, for
/// `caller` (R21-R26), writing nothing. Every node of the subtrees is to
/// keep only what it may do now and the new chain above it lets through,
/// so stored equals effective and nothing widens, now or once a list above
/// it grows (C14). A node held by a setter off the new chain is to be held
/// by `caller` instead, if `caller` ranks as high; otherwise it would
/// escape that hold, and the move is refused. Then the invariants are
/// checked as [`Database::apply_move`] checks them again.
///
/// [`Database::apply_move`]: crate::db::Database::apply_move
pub fn plan_move(
    app: &App,
    caller: &Caller,
    movers: Vec<TaskRecord>,
    to: TaskRecord,
    deleting: Option<TaskRecord>,
) -> Result<MovePlan> {
    let mut subtree = Vec::new();
    for mover in &movers {
        subtree.push(mover.clone());
        subtree.extend(app.db.descendants(&mover.id)?.into_iter().map(|(t, _)| t));
    }
    // What the chain from `to` up lets through.
    let through = permissions::effective(app, &to)?;
    let mut grants = Vec::new();
    let mut lost = Vec::new();
    for node in &subtree {
        let stored = app.db.grants(&node.id)?;
        let kept: Vec<String> = permissions::effective(app, node)?
            .into_iter()
            .filter(|c| through.contains(c))
            .map(String::from)
            .collect();
        let dropped: Vec<String> = stored
            .iter()
            .filter(|c| !kept.contains(c))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            lost.push((node.slug.clone(), dropped));
        }
        grants.push((node.id.clone(), stored, kept));
    }

    let mut chain = vec![to.id.clone()];
    chain.extend(app.db.ancestors(&to.id)?.into_iter().map(|a| a.id));
    let mut setters = Vec::new();
    for node in &subtree {
        let Some(Setter::Node(x)) = &node.run_state_by else {
            continue;
        };
        if subtree.iter().any(|t| &t.id == x) || chain.contains(x) {
            continue;
        }
        let old = Setter::Node(x.clone());
        if !manager::run::outranks(app, caller, &old)? {
            bail!(
                "task {} is {} by {}, which ranks above you; it would escape that by moving, \
                 so pause or resume first",
                node.slug,
                node.run_state.as_str(),
                manager::run::setter_label(app, &old)?
            );
        }
        setters.push((node.id.clone(), old, manager::run::setter_of(caller)));
    }

    let rows = MoveRows {
        movers: movers.iter().map(|m| m.id.clone()).collect(),
        to: to.id.clone(),
        deleting: deleting.as_ref().map(|d| d.id.clone()),
        fan_out: fan_out_of(app, &to)?,
        grants,
        setters,
    };
    app.db.check_move(&rows)?;
    Ok(MovePlan {
        movers,
        to,
        deleting,
        lost,
        rows,
    })
}

/// Plan deleting `task` with `--reparent`: the tasks under it, archived
/// ones too, move to its parent ([`plan_move`]). Refused for the
/// assistant, and for a task with nothing under it, which a plain delete
/// takes.
pub fn plan_reparent(app: &App, caller: &Caller, task: &TaskRecord) -> Result<MovePlan> {
    if task.kind == TaskKind::Assistant {
        bail!("the assistant cannot be deleted");
    }
    let children = app.db.children(&task.id)?;
    if children.is_empty() {
        bail!(
            "{} has no tasks under it to move; delete it without --reparent",
            task.slug
        );
    }
    let Some(parent_id) = &task.parent_id else {
        bail!("{} has no parent to take the tasks under it", task.slug);
    };
    let Some(parent) = app.db.get_task(parent_id)? else {
        bail!("the parent of task {} is gone", task.slug);
    };
    plan_move(app, caller, children, parent, Some(task.clone()))
}

/// Delete the task `plan` deletes, moving the tasks under it to its parent
/// ([`plan_reparent`]): its agent is ended and its files go as a lone
/// [`delete`]'s do, then the move is checked again and written with its row
/// gone, in one transaction, and the moved tasks and their new parent are
/// told ([`after_move`]). The caller has confirmed what [`losses`] lists.
/// Returns the move and the warnings of the delete.
pub fn delete_reparenting(
    app: &App,
    caller: &Caller,
    plan: MovePlan,
) -> Result<(Moved, Vec<String>)> {
    let Some(task) = plan.deleting.clone() else {
        bail!("this move deletes no task");
    };
    if task.slug.is_empty() || task.slug.starts_with('.') || task.slug.contains(['/', '\\']) {
        bail!(
            "refusing to delete a task whose directory name is {:?}",
            task.slug
        );
    }
    refuse_inside_any(std::slice::from_ref(&task), "delete")?;
    // The agent ends first, since it could change the tree as it runs, and
    // the move is checked again once it has: a plan gone stale during the
    // confirmation is refused while nothing of the task is removed.
    let mut warnings = end_agents(app, std::slice::from_ref(&task))?;
    if let Err(e) = app.db.check_move(&plan.rows) {
        bail!("{e:#}\nnothing was deleted, so run it again");
    }
    warnings.extend(remove_from_disk(app, &task)?);
    let reports = app.db.apply_move(&plan.rows).with_context(|| {
        format!(
            "task {}'s files are removed but its row is kept: run the delete again",
            task.slug
        )
    })?;
    let lines = after_move(app, caller, &plan, &reports);
    Ok((
        Moved {
            plan,
            reports,
            lines,
        },
        warnings,
    ))
}

/// Once `plan` is written: each moved task's new place, and what each
/// node's grant dropped; every active manager in the subtrees gets its
/// `BRIEF.md` again (its path and who it reports to changed); a running
/// mover is handed its new brief now, without starting one that is not;
/// and each mover tells its new parent it moved there, in jeet's words,
/// naming its open question if it has one; then each open ask among the
/// reports `kept` from a deleted task is delivered to the new parent
/// that holds it now. Slugs only: a node may run this, and a manager
/// writes task names (C19). One line per thing said.
fn after_move(app: &App, caller: &Caller, plan: &MovePlan, kept: &[i64]) -> Vec<String> {
    let mut lines = Vec::new();
    let to = manager::label(&plan.to);
    let by = match caller {
        Caller::User => "the user".to_string(),
        Caller::Root => "the assistant".to_string(),
        Caller::Node(node) => manager::label(node),
        Caller::Review(review) => format!("review {}", review.label()),
    };
    let mut nodes = Vec::new();
    for (id, _, _) in &plan.rows.grants {
        match app.db.get_task(id) {
            Ok(Some(node)) => nodes.push(node),
            Ok(None) => {}
            Err(e) => lines.push(format!("could not read task {id} again: {e:#}")),
        }
    }
    // Each mover as it is now, with who it was under.
    let mut moved = Vec::new();
    for old in &plan.movers {
        let Some(mover) = nodes.iter().find(|n| n.id == old.id) else {
            continue;
        };
        let from = match (&plan.deleting, &old.parent_id) {
            (Some(deleting), _) => manager::label(deleting),
            (None, Some(id)) => match app.db.get_task(id) {
                Ok(Some(parent)) => manager::label(&parent),
                _ => "its parent".to_string(),
            },
            (None, None) => "its parent".to_string(),
        };
        let path = manager::tree_path(app, mover).unwrap_or_else(|_| mover.slug.clone());
        lines.push(format!(
            "moved {} from {from} to {to} ({path})",
            manager::label(mover)
        ));
        moved.push((mover, from));
    }
    for (slug, dropped) in &plan.lost {
        lines.push(format!(
            "{slug}'s grant drops {}, which it could not use where it was",
            dropped.join(", ")
        ));
    }
    manager::write_briefs(app, &nodes);

    for (mover, from) in moved {
        if mover.state != TaskState::Active {
            continue;
        }
        let who = manager::label(mover);
        let agent = messages::TmuxAgent::new(app, mover).may_start(false);
        if messages::Agent::alive(&agent) {
            let told = reports::brief_context(app, mover).and_then(|brief| {
                messages::deliver(&task_dir(app, mover), &agent, &brief, Sender::Agent, None)
            });
            lines.push(format!("{who}'s new brief: {}", delivered_words(&told)));
        }
        let mut body = format!("moved here from {from} by {by}");
        match manager::open_ask(app, mover) {
            Ok(Some(ask)) => body.push_str(&format!(
                "; its open {} #{} is yours now",
                ask.kind.as_str(),
                ask.id
            )),
            Ok(None) => {}
            Err(e) => lines.push(format!("could not read {who}'s open question: {e:#}")),
        }
        match reports::auto_status(app, mover, StatusEvent::Moved, &body, i64::MAX, 0) {
            Ok(Some((id, told))) => lines.push(format!(
                "told {to} of {who} (#{id}): {}",
                delivered_words(&told)
            )),
            Ok(None) => {}
            Err(e) => lines.push(format!("could not tell {to} of {who}: {e:#}")),
        }
    }
    match reports::redeliver_moved(app, &plan.to, kept) {
        Ok(asks) => lines.extend(asks.into_iter().map(|(id, told)| {
            format!(
                "handed {to} #{id}, which it holds now: {}",
                delivered_words(&told)
            )
        })),
        Err(e) => lines.push(format!("could not hand {to} the asks it holds now: {e:#}")),
    }
    lines
}

/// How a message went, as a line says it.
fn delivered_words(delivered: &Result<Delivered>) -> String {
    match delivered {
        Ok(Delivered::Typed) => "typed into its session".to_string(),
        Ok(Delivered::Wake) => "delivered to its session".to_string(),
        Ok(Delivered::Queued) => "queued; it gets it when it finishes or sits idle".to_string(),
        Ok(Delivered::Started) => "started a session with it".to_string(),
        Ok(Delivered::Held) => "it waits for its next session".to_string(),
        Ok(Delivered::Paused) => "it waits until it is resumed".to_string(),
        Ok(Delivered::StartQueued { live, max }) => {
            format!("it waits for its start ({live}/{max} live)")
        }
        Err(e) => format!("not delivered: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_checkouts_are_the_outermost_ones_the_task_does_not_hold() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mk = |rel: &str| std::fs::create_dir_all(root.join(rel)).unwrap();
        // The task's own worktree, with a nested clone of its own inside.
        mk("api/vendor/x/.git");
        std::fs::write(
            root.join("api/.git"),
            "gitdir: /store/api/.git/worktrees/api\n",
        )
        .unwrap();
        // A clone on the canvas, holding another: only the outer one counts.
        mk("libs/tool/.git");
        mk("libs/tool/sub/.git");
        // Plain folders are not checkouts.
        mk("notes/drafts");
        mk(".jeet/status");
        // Nor is a symlink to one.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("libs/tool"), root.join("link")).unwrap();

        let found = checkouts_under(root, &[root.join("api")]);
        assert_eq!(
            found,
            [ForeignCheckout {
                path: root.join("libs/tool"),
                common_dir: None,
            }]
        );
        assert!(checkouts_under(&root.join("missing"), &[]).is_empty());
    }

    #[test]
    fn unkept_paths_are_named_up_to_a_few() {
        let paths: Vec<String> = (1..=7).map(|n| format!("f{n}")).collect();
        assert_eq!(name_some(&paths[..2]), "f1, f2");
        assert_eq!(name_some(&paths), "f1, f2, f3, f4, f5 and 2 more");
    }

    #[test]
    fn slugs_are_plain_lowercase_words() {
        assert_eq!(slugify("Fix the Login page"), "fix-the-login-page");
        assert_eq!(slugify("  feat/API: v2!  "), "feat-api-v2");
        assert_eq!(slugify("über café"), "ber-caf");
        assert_eq!(slugify("!!!"), "task");
        assert_eq!(slugify(""), "task");
        let long = slugify(&"word ".repeat(40));
        assert!(long.len() <= SLUG_MAX, "{long}");
        assert!(!long.ends_with('-'), "{long}");
    }

    #[test]
    fn numbered_names_start_at_two() {
        assert_eq!(numbered("api", 1), "api");
        assert_eq!(numbered("api", 2), "api-2");
    }

    #[test]
    fn dir_names_skip_taken_ones_and_strays() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(pick_dir_name(dir.path(), &[], "api"), "api");
        assert_eq!(pick_dir_name(dir.path(), &["api".into()], "api"), "api-2");
        std::fs::create_dir(dir.path().join("api-2")).unwrap();
        assert_eq!(pick_dir_name(dir.path(), &["api".into()], "api"), "api-3");
        // A repo named like a dotfile does not hide itself.
        assert_eq!(pick_dir_name(dir.path(), &[], ".github"), "github");
    }

    #[test]
    fn hand_picked_dir_names_stay_inside_the_task() {
        for ok in ["api", "api-v2", "web_2", "docs.site"] {
            assert!(validate_dir_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", ".", "..", ".jeet", "a/b", "../up", "a b", "x.lock", "a..b",
        ] {
            assert!(validate_dir_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn jeet_refs_are_valid_ref_names() {
        assert_eq!(jeet_ref("head", "t1", "api"), "refs/jeet/head/t1/api");
        assert_eq!(jeet_ref("head", "t1", "api-2"), "refs/jeet/head/t1/api-2");
        assert_eq!(
            jeet_ref("stash", "t1", "sub/my dir.v2"),
            "refs/jeet/stash/t1/sub_2Fmy_20dir_2Ev2"
        );
        assert_eq!(ref_component(".x.lock"), "_2Ex_2Elock");
    }

    #[test]
    fn jeet_refs_never_share_a_name() {
        // Every pair here once mapped to one ref, or nested one under another.
        let names = [
            "docs.site",
            "docs-site",
            "docs_site",
            "docs_2Esite",
            "foo.js",
            "foo-js",
            "sub",
            "sub/web",
            "sub-web",
        ];
        let refs: BTreeSet<String> = names.iter().map(|n| ref_component(n)).collect();
        assert_eq!(refs.len(), names.len(), "{refs:?}");
        assert!(refs.iter().all(|r| !r.contains('/')), "{refs:?}");
    }

    #[test]
    fn task_paths_split_into_slug_and_worktree() {
        let root = Path::new("/home/.jeet/tasks");
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix/api")),
            Some(("fix".into(), "api".into()))
        );
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix/sub/web")),
            Some(("fix".into(), "sub/web".into()))
        );
        // The task directory itself, and anything outside, are not worktrees of it.
        assert_eq!(
            split_task_path(root, Path::new("/home/.jeet/tasks/fix")),
            None
        );
        assert_eq!(split_task_path(root, Path::new("/elsewhere/fix/api")), None);
    }

    #[test]
    fn the_task_root_is_found_from_below() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("tasks/fix");
        std::fs::create_dir_all(task.join(".jeet")).unwrap();
        std::fs::write(task.join(".jeet/task-id"), "abc-123\n").unwrap();
        let deep = task.join("api/src/bin");
        std::fs::create_dir_all(&deep).unwrap();

        let (root, id) = find_task_root(&deep).unwrap();
        assert_eq!(root, task);
        assert_eq!(id, "abc-123");
        assert!(find_task_root(dir.path()).is_none());
    }

    #[test]
    fn summaries_count_worktrees_and_name_repos_once() {
        let row = |repo: &str, dir: &str| TaskWorktreeRecord {
            id: 0,
            task_id: "t".into(),
            repo_id: repo.into(),
            dir_name: dir.into(),
            branch: None,
            created_branch: false,
            head_oid: None,
            stash_ref: None,
            pr_number: None,
            created_at: 0,
            read_only: false,
        };
        assert_eq!(worktree_summary(&[]), "empty");
        assert_eq!(
            worktree_summary(&[
                row("github.com/acme/api", "api"),
                row("github.com/acme/web", "web"),
                row("github.com/acme/api", "api-2"),
            ]),
            "3 wt: api, web"
        );
    }

    /// A home with a repository in the store (cloned from a local origin,
    /// so there is an `origin/main`), the assistant, and under it a manager
    /// `m` and a worker `w`, each with its task directory.
    fn manager_lab() -> (tempfile::TempDir, App, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
        };
        let origin = root.join("origin");
        let trunk = root.join("trunk");
        std::fs::create_dir(&origin).unwrap();
        hermetic_git(&origin, &["init", "-q", "-b", "main"]);
        hermetic_git(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
        hermetic_git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                trunk.to_str().unwrap(),
            ],
        );
        app.db
            .upsert_repo(&RepoRecord {
                id: "example.com/o/api".into(),
                trunk_path: trunk.display().to_string(),
                remote_url: format!("file://{}", origin.display()),
                default_branch: "main".into(),
            })
            .unwrap();
        let assistant = crate::assistant::ensure(&app).unwrap();
        for (slug, kind) in [("m", TaskKind::Manager), ("w", TaskKind::Work)] {
            app.db
                .insert_task(&TaskRecord {
                    id: format!("{slug}-id"),
                    slug: slug.into(),
                    name: slug.into(),
                    project: None,
                    kind,
                    state: TaskState::Active,
                    created_at: 0,
                    last_activity: 0,
                    parent_id: Some(assistant.id.clone()),
                    placement: crate::placement::for_new(),
                    run_state: RunState::Run,
                    run_state_by: None,
                    max_children: None,
                })
                .unwrap();
            std::fs::create_dir_all(app.tasks_root().join(slug)).unwrap();
        }
        (dir, app, trunk)
    }

    fn hermetic_git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_manager_gets_detached_read_only_worktrees_only() {
        let (_dir, app, trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        let refused = add_worktree(&app, &m, "example.com/o/api", Some("fix"), None)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(refused, MANAGER_READ_ONLY);
        assert!(app.db.list_task_worktrees("m-id").unwrap().is_empty());
        assert!(!git::branch_exists(&trunk, "fix").unwrap());

        let added = add_worktree(&app, &m, "example.com/o/api", None, None).unwrap();
        assert!(added.record.read_only);
        assert_eq!(added.record.branch, None);
        assert_eq!(git::head_branch(&added.path), None);
        let row = app
            .db
            .get_task_worktree("m-id", &added.record.dir_name)
            .unwrap()
            .unwrap();
        assert!(row.read_only);
        assert_eq!(row.branch, None);

        // A worker's are as before.
        let w = app.db.get_task("w-id").unwrap().unwrap();
        let added = add_worktree(&app, &w, "example.com/o/api", Some("fix"), None).unwrap();
        assert!(!added.record.read_only);
        assert_eq!(added.record.branch.as_deref(), Some("fix"));
    }

    #[test]
    fn reconcile_adopts_a_managers_worktree_as_read_only_and_warns_on_a_branch() {
        let (_dir, app, trunk) = manager_lab();
        let by_hand = |slug: &str| {
            let path = app.tasks_root().join(slug).join("hand");
            hermetic_git(
                &trunk,
                &["worktree", "add", "-q", "--detach", path.to_str().unwrap()],
            );
            path
        };
        let m_hand = by_hand("m");
        let w_hand = by_hand("w");
        hermetic_git(&w_hand, &["switch", "-q", "-c", "w-branch"]);
        assert_eq!(reconcile(&app).unwrap(), Vec::<String>::new());
        let row = |id: &str| app.db.get_task_worktree(id, "hand").unwrap().unwrap();
        assert!(row("m-id").read_only);
        assert!(!row("w-id").read_only);

        // The manager checks a branch out itself: kept read-only, and named.
        hermetic_git(&m_hand, &["switch", "-q", "-c", "m-branch"]);
        let warnings = reconcile(&app).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("m/hand"), "{warnings:?}");
        assert!(warnings[0].contains("m-branch"), "{warnings:?}");
        assert!(warnings[0].contains(MANAGER_READ_ONLY), "{warnings:?}");
        assert!(row("m-id").read_only);
        assert_eq!(row("m-id").branch.as_deref(), Some("m-branch"));

        // The dir name is the agent's to pick: no escape reaches a terminal.
        let path = app.tasks_root().join("m").join("e\x1b[31mvil");
        hermetic_git(
            &trunk,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "e\u{9b}2J-branch",
                path.to_str().unwrap(),
            ],
        );
        let warnings = reconcile(&app).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("m/e[31mvil") && w.contains("e2J-branch")),
            "{warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.chars().any(char::is_control)),
            "{warnings:?}"
        );
    }

    /// A child of a manager with a project starts from it (and its repo,
    /// once even if asked for again) unless the caller took the choice
    /// itself; one whose project is gone is still made.
    #[test]
    fn a_child_inherits_its_managers_project_when_asked_to() {
        let (_dir, app, _trunk) = manager_lab();
        let project = app.home.join("projects/web");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.toml"),
            "repos = [\"example.com/o/api\"]\n",
        )
        .unwrap();
        std::fs::write(project.join("AGENTS.md"), "rules\n").unwrap();
        let m = TaskRecord {
            project: Some("web".into()),
            ..app.db.get_task("m-id").unwrap().unwrap()
        };
        app.db.delete_task("m-id").unwrap();
        app.db.insert_task(&m).unwrap();
        let d = Delegation {
            parent: Some("m".into()),
            ..Default::default()
        };
        let make = |name: &str, project: Option<&str>, inherit: bool, repos: &[String]| {
            let plan = plan_child(&app, &Caller::User, TaskKind::Work, name, &d, None).unwrap();
            create(
                &app,
                &NewTask {
                    name,
                    project,
                    repos,
                    branch: None,
                    child: Some((&Caller::User, &plan)),
                    inherit_project: inherit,
                },
            )
        };
        let worktrees = |t: &TaskRecord| app.db.list_task_worktrees(&t.id).unwrap().len();

        let again = ["example.com/o/api".to_string()];
        let c = make("c1", None, true, &again).unwrap().task;
        assert_eq!(c.project.as_deref(), Some("web"));
        assert_eq!(worktrees(&c), 1);
        assert!(task_dir(&app, &c).join("AGENTS.md").is_file());

        // The form's "no project" is no project.
        let c = make("c2", None, false, &[]).unwrap().task;
        assert_eq!(c.project, None);
        assert_eq!(worktrees(&c), 0);

        // Gone since: made without, and told.
        std::fs::remove_dir_all(&project).unwrap();
        let made = make("c3", None, true, &[]).unwrap();
        assert_eq!(made.task.project, None);
        assert!(
            made.warnings
                .iter()
                .any(|w| w.contains("made without a project")),
            "{:?}",
            made.warnings
        );
        // But asked for by name, it is an error and nothing is made.
        assert!(make("c4", Some("web"), true, &[]).is_err());
        assert!(app.db.get_task_by_slug("c4").unwrap().is_none());
    }

    /// A child gets `[managers] worker_default`, the `worktree.add` row
    /// that v6 gave the children before it, and its brief names who made
    /// it; a top-level task gets neither.
    #[test]
    fn a_new_child_is_granted_what_the_backfill_gave_the_rest() {
        let (_dir, app, _trunk) = manager_lab();
        let make = |name: &str, child: Option<(&Caller, &NewNode)>| {
            create(
                &app,
                &NewTask {
                    name,
                    project: None,
                    repos: &[],
                    branch: None,
                    child,
                    inherit_project: true,
                },
            )
            .unwrap()
            .task
        };
        let d = Delegation {
            parent: Some("assistant".into()),
            goal: Some("fix it".into()),
            ..Default::default()
        };
        let plan = plan_child(&app, &Caller::User, TaskKind::Work, "kid", &d, None).unwrap();
        let child = make("kid", Some((&Caller::User, &plan)));
        assert_eq!(app.db.grants(&child.id).unwrap(), vec!["worktree.add"]);
        assert_eq!(
            app.db.get_brief(&child.id).unwrap().unwrap().created_by,
            "user"
        );
        assert!(app.db.budget(&child.id).unwrap().is_none());
        let top = make("top", None);
        assert!(app.db.grants(&top.id).unwrap().is_empty());
    }

    /// Insert `slug` of `kind` under `parent`, with its directory.
    fn insert(app: &App, slug: &str, kind: TaskKind, parent: &str) -> TaskRecord {
        let task = TaskRecord {
            id: format!("{slug}-id"),
            slug: slug.into(),
            name: slug.into(),
            project: None,
            kind,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: Some(parent.into()),
            placement: crate::placement::for_new(),
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        };
        app.db.insert_task(&task).unwrap();
        std::fs::create_dir_all(task_dir(app, &task)).unwrap();
        task
    }

    fn under(parent: &str) -> Delegation {
        Delegation {
            parent: Some(parent.into()),
            ..Default::default()
        }
    }

    /// [`under`], with the goal a manager cannot be made without.
    fn briefed(parent: &str) -> Delegation {
        Delegation {
            goal: Some("g".into()),
            ..under(parent)
        }
    }

    /// Each check `plan_child` makes refuses on its own, in the order M2
    /// gives, and a refusal writes nothing.
    #[test]
    fn plan_child_refuses_each_thing_in_order_and_writes_nothing() {
        let (_dir, mut app, _trunk) = manager_lab();
        app.config.assistant.allow = vec!["task.create".into(), "worktree.add".into()];
        let m = app.db.get_task("m-id").unwrap().unwrap();
        app.db.set_grants(&m.id, &["task.create"]).unwrap();
        let m2 = insert(&app, "m2", TaskKind::Manager, &m.id);
        let m3 = insert(&app, "m3", TaskKind::Manager, &m2.id);
        let gone = insert(&app, "gone", TaskKind::Manager, &m.id);
        app.db
            .set_task_state(&gone.id, TaskState::Archived)
            .unwrap();
        let rows = || app.db.list_tasks(None).unwrap().len();
        let dirs = || std::fs::read_dir(app.tasks_root()).unwrap().count();
        let (before, before_dirs) = (rows(), dirs());
        let user = Caller::User;
        let as_m2 = Caller::Node(m2.clone());
        let refused = |by: &Caller, kind, d: &Delegation, says: &str| {
            let err = plan_child(&app, by, kind, "x", d, None).unwrap_err();
            assert!(format!("{err:#}").contains(says), "{says}: {err:#}");
        };

        // Outside the caller's subtree: its parent's sibling is not found.
        refused(&as_m2, TaskKind::Work, &under("w"), "--parent:");
        // Below it, but no manager.
        let w3 = insert(&app, "w3", TaskKind::Work, &m3.id);
        refused(&as_m2, TaskKind::Work, &under("w3"), "only a manager");
        refused(&user, TaskKind::Work, &under("gone"), "archived");
        refused(&user, TaskKind::Work, &under("w"), "not a manager");
        // Depth: a worker sits at most 3 below the assistant, a manager 2.
        refused(&user, TaskKind::Work, &under("m3"), "4 levels below");
        refused(&user, TaskKind::Manager, &briefed("m2"), "3 levels below");
        plan_child(&app, &user, TaskKind::Work, "x", &under("m2"), None).unwrap();
        // Fan-out: m holds m2 and four more, room for a sixth; a seventh is
        // too many.
        for n in 0..4 {
            insert(&app, &format!("k{n}"), TaskKind::Work, &m.id);
        }
        plan_child(&app, &user, TaskKind::Work, "x", &under("m"), None).unwrap();
        insert(&app, "k4", TaskKind::Work, &m.id);
        refused(&user, TaskKind::Work, &under("m"), "its limit of 6");
        app.db.set_task_state("k4-id", TaskState::Archived).unwrap();
        // The grant: within m's list.
        let allow = |caps: &[&str]| Delegation {
            allow: Some(caps.iter().map(|c| c.to_string()).collect()),
            ..under("m")
        };
        refused(&user, TaskKind::Work, &allow(&["task.send"]), "`task.send`");
        let plan = plan_child(
            &app,
            &user,
            TaskKind::Work,
            "x",
            &allow(&["task.create"]),
            None,
        )
        .unwrap();
        assert_eq!(plan.grant.caps, ["task.create"]);
        // The budget: m has 30 minutes left of its hour.
        app.db.set_budget_max(&m.id, Some(3600), None).unwrap();
        app.db.charge(&m.id, 1800, 0).unwrap();
        let budget = |b: &str| Delegation {
            budget: vec![crate::manager::budget::parse_part(b).unwrap()],
            ..under("m")
        };
        refused(&user, TaskKind::Work, &budget("1h"), "more than is left");
        let plan = plan_child(&app, &user, TaskKind::Work, "x", &budget("30m"), None).unwrap();
        assert_eq!(plan.budget.unwrap().secs, Some(1800));
        // Unasked, a manager's child gets `default_budget`, cut to what is left.
        let plan = plan_child(&app, &user, TaskKind::Work, "x", &under("m"), None).unwrap();
        assert_eq!(plan.budget.unwrap().secs, Some(1800));
        app.db.charge(&w3.id, 1800, 0).unwrap();
        refused(&user, TaskKind::Work, &under("m"), "nothing is left");

        assert_eq!((rows(), dirs()), (before + 6, before_dirs + 6));
    }

    /// A configured `[managers] max_depth` binds, not the default: at 2, a
    /// worker sits at most 2 below the assistant and a manager 1.
    #[test]
    fn plan_child_takes_its_depth_cap_from_config() {
        let (_dir, mut app, _trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        let m2 = insert(&app, "m2", TaskKind::Manager, &m.id);
        app.config.managers = Some(crate::config::ManagersConfig {
            max_depth: Some(2),
            ..Default::default()
        });
        let user = Caller::User;
        let refused = |kind, parent: &str, says: &str| {
            let err = plan_child(&app, &user, kind, "x", &briefed(parent), None).unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains(says), "{says}: {err}");
            assert!(err.contains("`[managers] max_depth` is 2"), "{err}");
        };
        refused(TaskKind::Work, &m2.slug, "3 levels below");
        refused(TaskKind::Manager, "m", "2 levels below");
        plan_child(&app, &user, TaskKind::Work, "x", &under("m"), None).unwrap();
        plan_child(
            &app,
            &user,
            TaskKind::Manager,
            "x",
            &briefed("assistant"),
            None,
        )
        .unwrap();
    }

    /// What is left is the least left anywhere above, in turns as in time:
    /// a grandparent's limit binds a grandchild.
    #[test]
    fn a_budget_is_cut_by_every_task_above_in_both_units() {
        let (_dir, app, _trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        insert(&app, "m2", TaskKind::Manager, &m.id);
        // m has 30 minutes and 5 turns left; m2 has no limit of its own.
        app.db.set_budget_max(&m.id, Some(3600), Some(20)).unwrap();
        app.db.charge(&m.id, 1800, 15).unwrap();
        let budget = |b: &str| Delegation {
            budget: vec![crate::manager::budget::parse_part(b).unwrap()],
            ..under("m2")
        };
        let user = Caller::User;
        for want in ["4h", "20turns"] {
            let err = plan_child(&app, &user, TaskKind::Work, "x", &budget(want), None)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("more than is left under task m2"),
                "{want}: {err}"
            );
            assert!(err.contains("(30m,5turns)"), "{want}: {err}");
        }
        let plan = plan_child(&app, &user, TaskKind::Work, "x", &budget("5turns"), None).unwrap();
        assert_eq!(plan.budget.unwrap().turns, Some(5));
        // `default_budget` (4h) is cut to what m has left.
        let plan = plan_child(&app, &user, TaskKind::Work, "x", &under("m2"), None).unwrap();
        assert_eq!(plan.budget.unwrap().secs, Some(1800));
        // Turns spent: refused whatever is asked, a unit it names or not.
        app.db.charge(&m.id, 0, 5).unwrap();
        for d in [budget("30m"), under("m2")] {
            let err = plan_child(&app, &user, TaskKind::Work, "x", &d, None).unwrap_err();
            assert!(
                err.to_string()
                    .contains("nothing is left of the budget under task m2"),
                "{err}"
            );
        }
    }

    /// First of the checks: a manager needs a goal, refused before its
    /// parent is resolved, so no assistant is made for it; a worker does
    /// without.
    #[test]
    fn plan_child_refuses_a_manager_without_a_goal_first() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
        };
        let user = Caller::User;
        // Even an invalid name or a done-when with no goal is reported
        // after it.
        let d = Delegation {
            done_when: Some("shipped".into()),
            ..Default::default()
        };
        let err = plan_child(&app, &user, TaskKind::Manager, "Bad Name", &d, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "a manager needs --goal or --brief-file: it is what it works towards"
        );
        let err = plan_child(
            &app,
            &user,
            TaskKind::Manager,
            "x",
            &under("assistant"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs --goal"), "{err:#}");
        assert!(app.db.get_assistant_task().unwrap().is_none());
        // A worker needs none.
        plan_child(&app, &user, TaskKind::Work, "x", &under("assistant"), None).unwrap();
    }

    /// Last of the checks: a manager needs a harness with hooks and the
    /// rest (M36); a worker does without.
    #[test]
    fn plan_child_refuses_a_manager_on_a_harness_without_hooks() {
        let (_dir, mut app, _trunk) = manager_lab();
        app.config.agent = Some("aider".into());
        let d = briefed("assistant");
        let err = plan_child(&app, &Caller::User, TaskKind::Manager, "x", &d, None).unwrap_err();
        let err = err.to_string();
        assert!(err.contains("hook events"), "{err}");
        assert!(err.contains("deny rules"), "{err}");
        plan_child(&app, &Caller::User, TaskKind::Work, "x", &d, None).unwrap();
    }

    /// A manager's own fan-out limit is at most `[managers] max_children`
    /// and is kept on its row; under the assistant, unasked, it gets
    /// `default_budget`. Its home has its role's files and its brief.
    #[test]
    fn a_manager_keeps_its_own_fan_out_limit_and_gets_the_default_budget() {
        let (_dir, app, _trunk) = manager_lab();
        let policy = app.config.manager_policy().unwrap();
        let user = Caller::User;
        let d = briefed("assistant");
        let above = policy.max_children + 1;
        let err = plan_child(&app, &user, TaskKind::Manager, "x", &d, Some(above)).unwrap_err();
        let err = err.to_string();
        assert!(err.contains(&format!("--max-children {above}")), "{err}");
        assert!(err.contains("`[managers] max_children`"), "{err}");

        let plan = plan_child(&app, &user, TaskKind::Manager, "x", &d, Some(2)).unwrap();
        assert_eq!(plan.record.max_children, Some(2));
        assert_eq!(plan.budget, Some(policy.default_budget));
        let (made, notes) = commit_child(&app, &plan, None).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        // Its role in place of the worker's skill, and its brief.
        let home = task_dir(&app, &made);
        for rel in [
            "AGENTS.md",
            ".claude/skills/jeet-manager/SKILL.md",
            "notes/worklog.md",
            "todo.md",
            crate::manager::BRIEF_MD,
        ] {
            assert!(home.join(rel).is_file(), "{rel}");
        }
        assert!(!home.join(".claude/skills/jeet").exists());
        let made = app.db.get_task(&made.id).unwrap().unwrap();
        assert_eq!(made.max_children, Some(2));
        assert_eq!(fan_out_of(&app, &made).unwrap(), Some(2));
        let row = app.db.budget(&made.id).unwrap().unwrap();
        assert_eq!(
            (row.max_secs, row.max_turns),
            (policy.default_budget.secs, policy.default_budget.turns)
        );
        // A worker under the assistant gets none unasked.
        let plan = plan_child(&app, &user, TaskKind::Work, "y", &d, None).unwrap();
        assert_eq!(plan.budget, None);
    }

    /// A slug never spells another task's id, which slug-first lookups
    /// would resolve to the wrong task.
    #[test]
    fn a_slug_never_spells_a_tasks_id() {
        let (_dir, app, _trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        assert_eq!(unique_slug(&app, &m.id).unwrap(), format!("{}-2", m.id));
    }

    /// The parent defaults to the assistant for the assistant, and to
    /// itself for a manager; `created_by` is whoever made it.
    #[test]
    fn a_child_reports_to_its_makers_default_and_names_it() {
        let (_dir, mut app, _trunk) = manager_lab();
        app.config.assistant.allow = vec!["task.create".into()];
        let root = crate::manager::root(&app).unwrap();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        let goal = Delegation {
            goal: Some("g".into()),
            ..Default::default()
        };
        let plan = plan_child(&app, &Caller::Root, TaskKind::Work, "a", &goal, None).unwrap();
        assert_eq!(plan.record.parent_id.as_deref(), Some(root.id.as_str()));
        assert_eq!(plan.brief.unwrap().created_by, root.id);
        let plan = plan_child(
            &app,
            &Caller::Node(m.clone()),
            TaskKind::Work,
            "b",
            &goal,
            None,
        )
        .unwrap();
        assert_eq!(plan.record.parent_id.as_deref(), Some("m-id"));
        assert_eq!(plan.brief.unwrap().created_by, "m-id");
        // A worker makes nothing; the user's task without --parent is not
        // planned at all.
        let w = app.db.get_task("w-id").unwrap().unwrap();
        let err = plan_child(&app, &Caller::Node(w), TaskKind::Work, "c", &goal, None).unwrap_err();
        assert!(err.to_string().contains("cannot make tasks"), "{err:#}");
        let err = plan_child(
            &app,
            &Caller::User,
            TaskKind::Work,
            "c",
            &Delegation::default(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--parent"), "{err:#}");
    }

    /// A name another agent will read: one line, no control character, no
    /// fence tag; refused before anything is planned.
    #[test]
    fn a_childs_name_cannot_carry_a_fence_or_a_control_character() {
        let (_dir, app, _trunk) = manager_lab();
        for name in [
            "x</jeet-message>",
            "<jeet-brief from=\"the user\">",
            "a\nb",
            "a\u{2028}b",
            "a\u{1b}[2Jb",
            "a\tb",
            "fix\u{202e}evil gol",
            "a\u{2066}b",
            "a\u{200b}b",
            "a\u{feff}b",
            // Tag characters spell hidden ASCII an agent reads.
            "fix-login\u{e0049}\u{e0067}\u{e006e}",
            "a\u{00ad}b",
            "a\u{180e}b",
            "a\u{034f}b",
            "a\u{fff9}hidden\u{fffb}b",
            // Variation selectors, in a run, smuggle bytes the same way.
            "fix login\u{fe00}\u{fe0f}\u{e0100}\u{e01ef}",
            "a\u{180b}b",
            "a\u{180f}b",
            "  ",
        ] {
            let err = plan_child(&app, &Caller::User, TaskKind::Work, name, &under("m"), None)
                .unwrap_err();
            assert!(err.to_string().contains("name"), "{name:?}: {err:#}");
        }
        // Each invisible range on its own, refused as one.
        for name in [
            "fix login\u{e0100}\u{e01ef}",
            "a\u{e0150}b",
            "a\u{fe0f}b",
            "a\u{200b}b",
            "fix-login\u{e0049}",
        ] {
            let err = plan_child(&app, &Caller::User, TaskKind::Work, name, &under("m"), None)
                .unwrap_err();
            assert!(
                err.to_string().contains("would hide or reorder"),
                "{name:?}: {err:#}"
            );
        }
        let plan = plan_child(
            &app,
            &Caller::User,
            TaskKind::Work,
            "  Fix the API (v2) ",
            &under("m"),
            None,
        )
        .unwrap();
        assert_eq!(plan.record.name, "Fix the API (v2)");
        // Text in other scripts, accents and emoji stay.
        for name in ["Réparer l'API", "修复登录", "ship it \u{1f680}"] {
            let plan =
                plan_child(&app, &Caller::User, TaskKind::Work, name, &under("m"), None).unwrap();
            assert_eq!(plan.record.name, name);
        }
    }

    /// The parent archived between plan and commit: the row is refused and
    /// the directory made for it goes again.
    #[test]
    fn commit_child_rechecks_the_parent_and_cleans_up() {
        let (_dir, app, _trunk) = manager_lab();
        let plan = plan_child(
            &app,
            &Caller::User,
            TaskKind::Work,
            "late",
            &under("m"),
            None,
        )
        .unwrap();
        app.db.set_task_state("m-id", TaskState::Archived).unwrap();
        let err = commit_child(&app, &plan, None).unwrap_err();
        assert!(err.to_string().contains("archived"), "{err:#}");
        assert!(!task_dir(&app, &plan.record).exists());
        assert!(app.db.get_task(&plan.record.id).unwrap().is_none());
    }

    /// A sibling made between plan and commit fills the parent: the
    /// commit counts again and refuses, and the directory goes again.
    #[test]
    fn commit_child_recounts_the_fan_out() {
        let (_dir, app, _trunk) = manager_lab();
        for n in 0..5 {
            insert(&app, &format!("k{n}"), TaskKind::Work, "m-id");
        }
        let plan = plan_child(
            &app,
            &Caller::User,
            TaskKind::Work,
            "late",
            &under("m"),
            None,
        )
        .unwrap();
        insert(&app, "k5", TaskKind::Work, "m-id");
        let err = commit_child(&app, &plan, None).unwrap_err();
        assert!(err.to_string().contains("its limit of 6"), "{err:#}");
        assert!(!task_dir(&app, &plan.record).exists());
        assert!(app.db.get_task(&plan.record.id).unwrap().is_none());
    }

    /// `tasks::archive` and `tasks::delete` refuse a parent before touching
    /// anything: the TUI and import reach them with no check of their own.
    #[test]
    fn teardown_with_tasks_under_it_is_refused_before_anything_goes() {
        let (_dir, app, _trunk) = manager_lab();
        let assistant = app.db.get_task("m-id").unwrap().unwrap().parent_id;
        // A slug no real tmux session has, should a regression reach it.
        let record = |id: &str, slug: &str, kind, parent: Option<String>| TaskRecord {
            id: id.into(),
            slug: slug.into(),
            name: slug.into(),
            project: None,
            kind,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: parent,
            placement: crate::placement::for_new(),
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        };
        let lead = record("lead-id", "jeet-unit-lead", TaskKind::Manager, assistant);
        app.db.insert_task(&lead).unwrap();
        let dir = task_dir(&app, &lead);
        std::fs::create_dir_all(&dir).unwrap();
        let wt = add_worktree(&app, &lead, "example.com/o/api", None, None)
            .unwrap()
            .path;
        let kid = record(
            "kid-id",
            "jeet-unit-kid",
            TaskKind::Work,
            Some("lead-id".into()),
        );
        app.db.insert_task(&kid).unwrap();
        // Archive removes the agent's settings long before its row changes.
        let settings = runtime::settings_dir(&app.home, "lead-id");
        std::fs::create_dir_all(&settings).unwrap();

        let untouched = |what: &str| {
            assert!(settings.is_dir(), "{what}: the agent's settings went");
            assert!(dir.is_dir(), "{what}: the task directory went");
            assert!(wt.is_dir(), "{what}: the worktree went");
            assert_eq!(app.db.list_task_worktrees("lead-id").unwrap().len(), 1);
            let row = app.db.get_task("lead-id").unwrap().unwrap();
            assert_eq!(row.state, TaskState::Active, "{what}");
        };
        let refused = archive(&app, &lead, false).unwrap_err().to_string();
        assert!(refused.contains("1 active task under it"), "{refused}");
        untouched("archive");
        let refused = delete(&app, &lead, false).unwrap_err().to_string();
        assert!(refused.contains("1 task under it"), "{refused}");
        untouched("delete");

        app.db
            .set_task_state("kid-id", TaskState::Archived)
            .unwrap();
        let refused = delete(&app, &lead, false).unwrap_err().to_string();
        assert!(refused.contains("archived ones included"), "{refused}");
        untouched("delete with the child archived");
    }

    /// `--reparent` plans every node of the moved subtrees down to what it
    /// may use now, so a capability a deleted manager cut is dropped from
    /// the grants below it, not freed (C14); a setter the deleted manager
    /// was becomes the caller; and what it may use is unchanged.
    #[test]
    fn a_reparent_cuts_every_moved_grant_to_what_it_may_use() {
        let (_dir, mut app, _trunk) = manager_lab();
        app.config.assistant.allow = ["task.create", "pr.merge", "worktree.add"]
            .map(String::from)
            .to_vec();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        app.db
            .set_grants(&m.id, &["task.create", "worktree.add"])
            .unwrap();
        let sub = insert(&app, "sub", TaskKind::Manager, "m-id");
        app.db
            .set_grants(&sub.id, &["pr.merge", "task.create", "worktree.add"])
            .unwrap();
        let kid = insert(&app, "kid", TaskKind::Work, "sub-id");
        app.db
            .set_grants(&kid.id, &["pr.merge", "worktree.add"])
            .unwrap();
        let w2 = insert(&app, "w2", TaskKind::Work, "m-id");
        app.db.set_grants(&w2.id, &["worktree.add"]).unwrap();
        app.db
            .set_run_state(&sub.id, RunState::Paused, Some(&Setter::Node(m.id.clone())))
            .unwrap();
        let effective = |id: &str| {
            let node = app.db.get_task(id).unwrap().unwrap();
            permissions::effective(&app, &node).unwrap()
        };
        let before: Vec<_> = ["sub-id", "kid-id", "w2-id"].map(effective).to_vec();

        let plan = plan_reparent(&app, &Caller::User, &m).unwrap();
        assert_eq!(plan.to.kind, TaskKind::Assistant);
        let movers: Vec<&str> = plan.movers.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(movers, ["sub", "w2"]);
        assert_eq!(
            plan.lost,
            [
                ("sub".to_string(), vec!["pr.merge".to_string()]),
                ("kid".to_string(), vec!["pr.merge".to_string()]),
            ]
        );
        assert_eq!(
            plan.rows.setters,
            [(sub.id.clone(), Setter::Node(m.id.clone()), Setter::User)]
        );
        // Planning wrote nothing.
        assert_eq!(
            app.db.get_task("sub-id").unwrap().unwrap().parent_id,
            Some(m.id.clone())
        );

        app.db.apply_move(&plan.rows).unwrap();
        assert_eq!(
            app.db.grants(&sub.id).unwrap(),
            ["task.create", "worktree.add"]
        );
        assert_eq!(app.db.grants(&kid.id).unwrap(), ["worktree.add"]);
        assert_eq!(app.db.grants(&w2.id).unwrap(), ["worktree.add"]);
        // The assistant holds `pr.merge`, and still nothing below it does.
        let after: Vec<_> = ["sub-id", "kid-id", "w2-id"].map(effective).to_vec();
        assert_eq!(before, after);
    }

    /// A plan gone stale between planning and deleting (a child made while
    /// the confirmation was open) is refused before anything of the manager
    /// is removed; a fresh plan then goes through.
    #[test]
    fn a_stale_reparent_plan_is_refused_with_the_files_untouched() {
        let (_dir, app, _trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        insert(&app, "w1", TaskKind::Work, "m-id");
        let plan = plan_reparent(&app, &Caller::User, &m).unwrap();
        insert(&app, "w2", TaskKind::Work, "m-id");
        let dir = task_dir(&app, &m);
        std::fs::create_dir_all(&dir).unwrap();

        let e = delete_reparenting(&app, &Caller::User, plan)
            .unwrap_err()
            .to_string();
        assert!(e.contains("nothing was deleted"), "{e}");
        assert!(dir.is_dir(), "the task directory went");
        assert!(app.db.get_task("m-id").unwrap().is_some());
        assert_eq!(app.db.child_count("m-id", false).unwrap(), 2);

        let plan = plan_reparent(&app, &Caller::User, &m).unwrap();
        delete_reparenting(&app, &Caller::User, plan).unwrap();
        assert!(app.db.get_task("m-id").unwrap().is_none());
        assert!(!dir.exists());
    }

    /// `--reparent` needs tasks under it, and a node does not free a task
    /// from a hold set above it by moving it.
    #[test]
    fn a_move_is_refused_a_childless_task_and_a_hold_from_above() {
        let (_dir, app, _trunk) = manager_lab();
        let m = app.db.get_task("m-id").unwrap().unwrap();
        let e = plan_reparent(&app, &Caller::User, &m)
            .unwrap_err()
            .to_string();
        assert!(e.contains("delete it without --reparent"), "{e}");
        let root = app.db.get_assistant_task().unwrap().unwrap();
        let e = plan_reparent(&app, &Caller::User, &root)
            .unwrap_err()
            .to_string();
        assert!(e.contains("the assistant cannot be deleted"), "{e}");

        let n = insert(&app, "n", TaskKind::Work, "m-id");
        app.db
            .set_run_state(&n.id, RunState::Paused, Some(&Setter::Node(m.id.clone())))
            .unwrap();
        let n = app.db.get_task(&n.id).unwrap().unwrap();
        let fe = insert(&app, "fe", TaskKind::Manager, &root.id);
        let e = plan_move(
            &app,
            &Caller::Node(fe.clone()),
            vec![n.clone()],
            fe.clone(),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("task n is paused by manager m, which ranks above you"),
            "{e}"
        );
        // The user outranks it, so the hold becomes the user's.
        let plan = plan_move(&app, &Caller::User, vec![n.clone()], fe, None).unwrap();
        assert_eq!(
            plan.rows.setters,
            [(n.id.clone(), Setter::Node(m.id.clone()), Setter::User)]
        );
    }

    /// A task moved twice within a second tells both new parents: `moved`
    /// is a discrete event, not deduplicated by time.
    #[test]
    fn a_task_moved_twice_in_a_second_tells_both_new_parents() {
        let (_dir, app, _trunk) = manager_lab();
        let root = app.db.get_assistant_task().unwrap().unwrap();
        let fe = insert(&app, "fe", TaskKind::Manager, &root.id);
        let be = insert(&app, "be", TaskKind::Manager, &root.id);
        let w = app.db.get_task("w-id").unwrap().unwrap();
        for to in [&fe, &be] {
            let w = app.db.get_task(&w.id).unwrap().unwrap();
            let plan = plan_move(&app, &Caller::User, vec![w], to.clone(), None).unwrap();
            app.db.apply_move(&plan.rows).unwrap();
            let lines = after_move(&app, &Caller::User, &plan, &[]);
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with(&format!("told {}", manager::label(to)))),
                "{lines:?}"
            );
            assert_eq!(app.db.reports_for(&to.id, 10).unwrap().len(), 1);
        }
    }

    /// A manager's own limit, else `[managers] max_children`; the assistant
    /// and a worker parent as before.
    #[test]
    fn unarchive_takes_a_managers_fan_out_from_config_unless_it_has_its_own() {
        let (_dir, mut app, _trunk) = manager_lab();
        let mut kid = app.db.get_task("w-id").unwrap().unwrap();
        assert_eq!(fan_out_of_parent(&app, &kid).unwrap(), None);
        kid.parent_id = Some("m-id".into());
        assert_eq!(
            fan_out_of_parent(&app, &kid).unwrap(),
            Some(crate::config::DEFAULT_MAX_CHILDREN)
        );
        app.config.managers = Some(crate::config::ManagersConfig {
            max_children: Some(2),
            ..Default::default()
        });
        assert_eq!(fan_out_of_parent(&app, &kid).unwrap(), Some(2));
        app.config.managers.as_mut().unwrap().max_children = Some(0);
        let err = fan_out_of_parent(&app, &kid).unwrap_err().to_string();
        assert!(err.contains("max_children"), "{err}");
        let mut own = app.db.get_task("m-id").unwrap().unwrap();
        own.id = "own-id".into();
        own.slug = "own".into();
        own.max_children = Some(1);
        app.db.insert_task(&own).unwrap();
        kid.parent_id = Some("own-id".into());
        assert_eq!(fan_out_of_parent(&app, &kid).unwrap(), Some(1));
    }

    /// Every task a recursive unarchive brings back is held to its parent's
    /// limit, an active target's archived children included.
    #[test]
    fn recursive_unarchive_holds_each_task_to_its_parents_limit() {
        let (_dir, app, _trunk) = manager_lab();
        let mut lead = app.db.get_task("m-id").unwrap().unwrap();
        lead.id = "lead-id".into();
        lead.slug = "lead".into();
        lead.max_children = Some(1);
        app.db.insert_task(&lead).unwrap();
        let mut kid = app.db.get_task("w-id").unwrap().unwrap();
        kid.id = "kid-id".into();
        kid.slug = "kid".into();
        kid.parent_id = Some("lead-id".into());
        app.db.insert_task(&kid).unwrap();
        let mut old = kid.clone();
        old.id = "old-id".into();
        old.slug = "old".into();
        old.state = TaskState::Archived;
        app.db.insert_task(&old).unwrap();
        let err = unarchive(&app, &lead, true).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("old"), "{err}");
        let row = app.db.get_task("old-id").unwrap().unwrap();
        assert_eq!(row.state, TaskState::Archived, "{err}");
    }

    #[test]
    fn the_pr_policy_is_the_tasks_then_the_projects_then_the_configs() {
        let (_dir, mut app, _trunk) = manager_lab();
        let mut w = app.db.get_task("w-id").unwrap().unwrap();
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Draft);
        app.config.tasks.pr = Some(PrMode::Auto);
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Auto);
        projects::create(&app, "launch", &[]).unwrap();
        let toml = projects::config_path(&app, "launch").unwrap();
        std::fs::write(&toml, "[tasks]\npr = \"ask\"\n").unwrap();
        w.project = Some("launch".into());
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Ask);
        app.db.set_task_pr(&w.id, Some(PrMode::Draft)).unwrap();
        assert_eq!(app.db.task_pr(&w.id).unwrap(), Some(PrMode::Draft));
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Draft);
        app.db.set_task_pr(&w.id, None).unwrap();
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Ask);
        // A broken project, or one since deleted, falls back to the config.
        std::fs::write(&toml, "[tasks]\npr = \"sometimes\"\n").unwrap();
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Auto);
        projects::remove(&app, "launch").unwrap();
        assert_eq!(pr_mode(&app, &w).unwrap(), PrMode::Auto);
        // One line each, naming the mode.
        for mode in [PrMode::Auto, PrMode::Draft, PrMode::Ask] {
            let line = pr_policy_line(mode);
            assert!(line.starts_with(&format!("PR policy: {} — ", mode.as_str())));
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(pr_policy_line(PrMode::Draft).contains("gh pr create --draft"));
        assert!(pr_policy_line(PrMode::Ask).contains("jeet report question"));
    }

    #[test]
    fn the_skill_tells_the_agent_how_to_get_code() {
        let dir = tempfile::tempdir().unwrap();
        install_skill(dir.path()).unwrap();
        let skill =
            std::fs::read_to_string(dir.path().join(".claude/skills/jeet/SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: jeet\n"), "{skill}");
        for needle in [
            "jeet worktree add",
            "jeet worktree ls",
            "jeet worktree rm",
            "jeet ls",
            "gh pr create",
            "$JEET_HOME/store",
            "jeet report done",
            "jeet report question",
        ] {
            assert!(skill.contains(needle), "skill does not mention {needle}");
        }
    }
}
