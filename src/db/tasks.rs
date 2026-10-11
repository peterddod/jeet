//! The task tables (schema v1, v3, v4, v5, v6): a task, where its agent
//! runs, the worktrees it holds, its agent sessions, its PR history, what its
//! parent asked of it and heard back from it, and, for the tree under the
//! assistant, what each node may do, may spend and whether it may run.

// The tasks milestone is the first caller; until it lands only the tests here
// use most of this.
#![allow(dead_code)]

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::Database;
use crate::placement::Placement;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Work,
    /// Reviewing someone else's PR.
    Review,
    /// The global assistant: there is one, and it is kept out of the tasks
    /// list; see [`crate::assistant`].
    Assistant,
    /// A node below the assistant that delegates rather than codes: it has
    /// children of its own and holds its worktrees read-only.
    Manager,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Work => "work",
            TaskKind::Review => "review",
            TaskKind::Assistant => "assistant",
            TaskKind::Manager => "manager",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "work" => Ok(TaskKind::Work),
            "review" => Ok(TaskKind::Review),
            "assistant" => Ok(TaskKind::Assistant),
            "manager" => Ok(TaskKind::Manager),
            other => Err(bad_enum("kind", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Active,
    /// Worktrees removed to free the disk; enough kept to bring them back.
    Archived,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Active => "active",
            TaskState::Archived => "archived",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "active" => Ok(TaskState::Active),
            "archived" => Ok(TaskState::Archived),
            other => Err(bad_enum("state", other)),
        }
    }
}

/// Whether a task may run. Ordered by severity: a node's effective state is
/// the most severe over itself and its ancestors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunState {
    Run,
    /// Nothing new starts or is handed over; what runs finishes its turn.
    Paused,
    /// Its agent is ended and never restarted on its own.
    Stopped,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Run => "run",
            RunState::Paused => "paused",
            RunState::Stopped => "stopped",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "run" => Ok(RunState::Run),
            "paused" => Ok(RunState::Paused),
            "stopped" => Ok(RunState::Stopped),
            other => Err(bad_enum("run state", other)),
        }
    }
}

/// Who paused or stopped a task (`tasks.run_state_by`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setter {
    User,
    Root,
    /// jeet, when the task's subtree used up its budget.
    Budget,
    /// A manager above the task, or the task itself, by id.
    Node(String),
}

impl Setter {
    pub fn as_str(&self) -> &str {
        match self {
            Setter::User => "user",
            Setter::Root => "root",
            Setter::Budget => "budget",
            Setter::Node(id) => id,
        }
    }

    /// Task ids are uuids, so no id reads as one of the words.
    pub fn parse(s: &str) -> Self {
        match s {
            "user" => Setter::User,
            "root" => Setter::Root,
            "budget" => Setter::Budget,
            id => Setter::Node(id.to_string()),
        }
    }
}

/// What a task told its parent with `jeet report`, or, as `Status`, what
/// jeet told the parent about it on its own (waiting, idle, ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ReportKind {
    Progress,
    Done,
    Blocked,
    Question,
    /// jeet's, never the task's: not offered to `jeet report`.
    #[value(skip)]
    Status,
}

impl ReportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Progress => "progress",
            ReportKind::Done => "done",
            ReportKind::Blocked => "blocked",
            ReportKind::Question => "question",
            ReportKind::Status => "status",
        }
    }

    pub fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "progress" => Ok(ReportKind::Progress),
            "done" => Ok(ReportKind::Done),
            "blocked" => Ok(ReportKind::Blocked),
            "question" => Ok(ReportKind::Question),
            "status" => Ok(ReportKind::Status),
            other => Err(bad_enum("report kind", other)),
        }
    }
}

/// The CHECK constraints keep these out, so seeing one means the index was
/// edited by hand; say which column rather than failing anonymously.
fn bad_enum(column: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown task {column} {value:?}").into(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// A uuid; stable for the task's whole life, unlike its name.
    pub id: String,
    /// The task directory's name under `tasks/`. Never changes either:
    /// Claude keys its sessions by the working directory.
    pub slug: String,
    pub name: String,
    pub project: Option<String>,
    pub kind: TaskKind,
    pub state: TaskState,
    pub created_at: i64,
    pub last_activity: i64,
    /// The task this one reports to; `None` for a top-level task. Not
    /// [`TaskSessionRecord::parent_id`], which is a session's.
    pub parent_id: Option<String>,
    /// Where its agent runs; fixed when the task is made.
    pub placement: Placement,
    /// Its own run state; see [`Database::effective_run_state`] for the one
    /// that applies.
    pub run_state: RunState,
    /// Who set a run state other than `Run`; `None` while it runs.
    pub run_state_by: Option<Setter>,
    /// How many active children it may have, below the configured limit.
    pub max_children: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorktreeRecord {
    /// Assigned by the index; ignored on insert.
    pub id: i64,
    pub task_id: String,
    pub repo_id: String,
    /// The worktree's directory inside the task directory.
    pub dir_name: String,
    /// `None` for a detached checkout.
    pub branch: Option<String>,
    /// The task made the branch, so removing the task may delete it.
    pub created_branch: bool,
    /// Where HEAD was when the worktree was archived.
    pub head_oid: Option<String>,
    /// The ref holding uncommitted work saved at archive time.
    pub stash_ref: Option<String>,
    pub pr_number: Option<u64>,
    pub created_at: i64,
    /// Held by a manager, which reads and does not write; jeet keeps it
    /// detached.
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSessionRecord {
    pub session_id: String,
    pub task_id: String,
    /// The session this one was cleared or forked from.
    pub parent_id: Option<String>,
    pub name: String,
    pub created_at: i64,
    pub last_activity: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPrRecord {
    pub task_id: String,
    pub repo_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    /// GitHub's state: OPEN, CLOSED or MERGED.
    pub state: String,
    pub is_draft: bool,
    pub additions: u64,
    pub deletions: u64,
    /// GitHub's ISO-8601 timestamp, which sorts as text.
    pub updated_at: String,
}

/// One report, kept after it is delivered: the record a parent's session
/// context and the tasks list read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReportRecord {
    /// Assigned by the index; ignored on insert.
    pub id: i64,
    /// The task it is about.
    pub task_id: String,
    /// The task it went to: the parent, when it was sent.
    pub to_id: String,
    pub kind: ReportKind,
    pub body: String,
    pub at: i64,
}

/// What a task's parent asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBriefRecord {
    pub task_id: String,
    pub goal: String,
    /// The definition of done.
    pub done_when: Option<String>,
    /// The parent's task id, or `user` when the user made the task.
    pub created_by: String,
}

/// A task's limits and what it has used itself; its subtree's use is
/// [`Database::subtree_used`]. `None`: no limit of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRow {
    pub max_secs: Option<i64>,
    pub max_turns: Option<i64>,
    pub used_secs: i64,
    pub used_turns: i64,
    /// The last threshold told: 0, 80 or 100 (per cent).
    pub warned: u8,
}

const TASK_COLUMNS: &str = "id, slug, name, project, kind, state, created_at, last_activity, \
     parent_id, placement, run_state, run_state_by, max_children";

/// [`TASK_COLUMNS`], each qualified by `alias`, for a query that joins.
fn prefixed_columns(alias: &str) -> String {
    TASK_COLUMNS
        .split(", ")
        .map(|c| format!("{alias}.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How far up or down the tree a walk goes before it calls the index
/// corrupt: far deeper than any limit jeet lets a tree grow to, so only a
/// cycle made by hand reaches it.
const MAX_HOPS: u32 = 64;

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        slug: row.get(1)?,
        name: row.get(2)?,
        project: row.get(3)?,
        kind: TaskKind::parse(&row.get::<_, String>(4)?)?,
        state: TaskState::parse(&row.get::<_, String>(5)?)?,
        created_at: row.get(6)?,
        last_activity: row.get(7)?,
        parent_id: row.get(8)?,
        placement: {
            let value = row.get::<_, String>(9)?;
            Placement::parse(&value).ok_or_else(|| bad_enum("placement", &value))?
        },
        run_state: RunState::parse(&row.get::<_, String>(10)?)?,
        run_state_by: row
            .get::<_, Option<String>>(11)?
            .as_deref()
            .map(Setter::parse),
        max_children: row.get(12)?,
    })
}

const TASK_WORKTREE_COLUMNS: &str = "id, task_id, repo_id, dir_name, branch, created_branch, \
     head_oid, stash_ref, pr_number, created_at, read_only";

fn task_worktree_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskWorktreeRecord> {
    Ok(TaskWorktreeRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        repo_id: row.get(2)?,
        dir_name: row.get(3)?,
        branch: row.get(4)?,
        created_branch: row.get::<_, i32>(5)? != 0,
        head_oid: row.get(6)?,
        stash_ref: row.get(7)?,
        pr_number: row.get::<_, Option<i64>>(8)?.map(|n| n as u64),
        created_at: row.get(9)?,
        read_only: row.get::<_, i32>(10)? != 0,
    })
}

fn task_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSessionRecord> {
    Ok(TaskSessionRecord {
        session_id: row.get(0)?,
        task_id: row.get(1)?,
        parent_id: row.get(2)?,
        name: row.get(3)?,
        created_at: row.get(4)?,
        last_activity: row.get(5)?,
    })
}

fn task_pr_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskPrRecord> {
    Ok(TaskPrRecord {
        task_id: row.get(0)?,
        repo_id: row.get(1)?,
        number: row.get::<_, i64>(2)? as u64,
        title: row.get(3)?,
        url: row.get(4)?,
        state: row.get(5)?,
        is_draft: row.get::<_, i32>(6)? != 0,
        additions: row.get::<_, i64>(7)? as u64,
        deletions: row.get::<_, i64>(8)? as u64,
        updated_at: row.get(9)?,
    })
}

const TASK_REPORT_COLUMNS: &str = "id, task_id, to_id, kind, body, at";

fn task_report_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskReportRecord> {
    Ok(TaskReportRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        to_id: row.get(2)?,
        kind: ReportKind::parse(&row.get::<_, String>(3)?)?,
        body: row.get(4)?,
        at: row.get(5)?,
    })
}

fn insert_task_in(conn: &Connection, task: &TaskRecord) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO tasks ({TASK_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        ),
        params![
            task.id,
            task.slug,
            task.name,
            task.project,
            task.kind.as_str(),
            task.state.as_str(),
            task.created_at,
            task.last_activity,
            task.parent_id,
            task.placement.as_str(),
            task.run_state.as_str(),
            task.run_state_by.as_ref().map(Setter::as_str),
            task.max_children,
        ],
    )?;
    Ok(())
}

fn get_task_in(conn: &Connection, id: &str) -> Result<Option<TaskRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            params![id],
            task_from_row,
        )
        .optional()?)
}

/// `id`'s ancestors, nearest first. A link to a missing task, or a chain
/// longer than [`MAX_HOPS`] (a cycle), is an error: callers decide what a
/// node may do from this, so a broken tree must not read as a short one.
fn ancestors_in(conn: &Connection, id: &str) -> Result<Vec<TaskRecord>> {
    let Some(task) = get_task_in(conn, id)? else {
        bail!("task {id} is not in the index");
    };
    let mut chain = Vec::new();
    let mut next = task.parent_id;
    while let Some(parent_id) = next {
        if chain.len() as u32 >= MAX_HOPS {
            bail!(
                "task {} has more than {MAX_HOPS} ancestors; the tree has a cycle",
                task.slug
            );
        }
        let Some(parent) = get_task_in(conn, &parent_id)? else {
            bail!("an ancestor of task {} ({parent_id}) is gone", task.slug);
        };
        next = parent.parent_id.clone();
        chain.push(parent);
    }
    Ok(chain)
}

fn child_count_in(conn: &Connection, id: &str, active_only: bool) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE parent_id = ?1 AND (?2 = 0 OR state = 'active')",
        params![id, active_only],
        |row| row.get(0),
    )?;
    Ok(n as usize)
}

/// How a run state's setter ranks when two hold a task at the same severity:
/// whether `new` outranks `old`. Both are the task or its ancestors, so of two
/// nodes the one nearer the root ranks higher.
fn outranks_in(conn: &Connection, new: &Setter, old: &Setter) -> Result<bool> {
    let rank = |s: &Setter| match s {
        Setter::User => 3,
        Setter::Root => 2,
        Setter::Node(_) => 1,
        Setter::Budget => 0,
    };
    match (new, old) {
        (Setter::Node(new), Setter::Node(old)) => {
            Ok(ancestors_in(conn, old)?.iter().any(|a| &a.id == new))
        }
        _ => Ok(rank(new) > rank(old)),
    }
}

impl Database {
    pub fn insert_task(&self, task: &TaskRecord) -> Result<()> {
        insert_task_in(&self.conn(), task)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<TaskRecord>> {
        get_task_in(&self.conn(), id)
    }

    pub fn get_task_by_slug(&self, slug: &str) -> Result<Option<TaskRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE slug = ?1"),
                params![slug],
                task_from_row,
            )
            .optional()?)
    }

    /// Tasks in `state` (all of them for `None`), most recently active first.
    /// The assistant is not among them: it is no task to list, open, archive
    /// or delete.
    pub fn list_tasks(&self, state: Option<TaskState>) -> Result<Vec<TaskRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks
             WHERE (?1 IS NULL OR state = ?1) AND kind != 'assistant'
             ORDER BY last_activity DESC, created_at DESC, slug"
        ))?;
        let rows = stmt.query_map(params![state.map(TaskState::as_str)], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The assistant's row, once it has one.
    pub fn get_assistant_task(&self) -> Result<Option<TaskRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE kind = 'assistant'"),
                [],
                task_from_row,
            )
            .optional()?)
    }

    /// The review task for PR `number` of `repo_id`: the one with a worktree
    /// checked out for it, else one whose history records it. Oldest first,
    /// should there ever be two.
    pub fn find_review_task(&self, repo_id: &str, number: u64) -> Result<Option<TaskRecord>> {
        let columns = prefixed_columns("t");
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {columns},
                       EXISTS (SELECT 1 FROM task_worktrees w
                               WHERE w.task_id = t.id AND w.repo_id = ?1 AND w.pr_number = ?2)
                         AS checked_out
                     FROM tasks t
                     WHERE t.kind = 'review' AND (checked_out OR EXISTS (
                       SELECT 1 FROM task_prs p
                       WHERE p.task_id = t.id AND p.repo_id = ?1 AND p.number = ?2))
                     ORDER BY checked_out DESC, t.created_at, t.slug
                     LIMIT 1"
                ),
                params![repo_id, number as i64],
                task_from_row,
            )
            .optional()?)
    }

    pub fn set_task_state(&self, id: &str, state: TaskState) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// Move a task's last activity forward to `at`; never back, so a late
    /// report of something older cannot reorder the list.
    pub fn touch_task(&self, id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET last_activity = MAX(last_activity, ?2) WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// Remove a task's rows: its worktrees, sessions and PR history go with
    /// it. The caller deals with what is on disk.
    pub fn delete_task(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Record a worktree added to a task, returning its row id.
    pub fn insert_task_worktree(&self, wt: &TaskWorktreeRecord) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO task_worktrees (task_id, repo_id, dir_name, branch, created_branch,
                                         head_oid, stash_ref, pr_number, created_at, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                wt.task_id,
                wt.repo_id,
                wt.dir_name,
                wt.branch,
                i32::from(wt.created_branch),
                wt.head_oid,
                wt.stash_ref,
                wt.pr_number.map(|n| n as i64),
                wt.created_at,
                i32::from(wt.read_only),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Rewrite a worktree row's mutable fields (branch, archive state, PR,
    /// read-only), keyed by its id.
    pub fn update_task_worktree(&self, wt: &TaskWorktreeRecord) -> Result<()> {
        self.conn().execute(
            "UPDATE task_worktrees
             SET branch = ?2, created_branch = ?3, head_oid = ?4, stash_ref = ?5, pr_number = ?6,
                 read_only = ?7
             WHERE id = ?1",
            params![
                wt.id,
                wt.branch,
                i32::from(wt.created_branch),
                wt.head_oid,
                wt.stash_ref,
                wt.pr_number.map(|n| n as i64),
                i32::from(wt.read_only),
            ],
        )?;
        Ok(())
    }

    pub fn list_task_worktrees(&self, task_id: &str) -> Result<Vec<TaskWorktreeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_WORKTREE_COLUMNS} FROM task_worktrees
             WHERE task_id = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(params![task_id], task_worktree_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_task_worktree(
        &self,
        task_id: &str,
        dir_name: &str,
    ) -> Result<Option<TaskWorktreeRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {TASK_WORKTREE_COLUMNS} FROM task_worktrees
                     WHERE task_id = ?1 AND dir_name = ?2"
                ),
                params![task_id, dir_name],
                task_worktree_from_row,
            )
            .optional()?)
    }

    /// Every task worktree of `repo_id` on `branch`: who holds a branch, for
    /// refusing to check it out twice.
    pub fn task_worktrees_on_branch(
        &self,
        repo_id: &str,
        branch: &str,
    ) -> Result<Vec<TaskWorktreeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_WORKTREE_COLUMNS} FROM task_worktrees
             WHERE repo_id = ?1 AND branch = ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![repo_id, branch], task_worktree_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every task worktree of `repo_id`, in any task.
    pub fn task_worktrees_of_repo(&self, repo_id: &str) -> Result<Vec<TaskWorktreeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_WORKTREE_COLUMNS} FROM task_worktrees
             WHERE repo_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![repo_id], task_worktree_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_task_worktree(&self, task_id: &str, dir_name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM task_worktrees WHERE task_id = ?1 AND dir_name = ?2",
            params![task_id, dir_name],
        )?;
        Ok(())
    }

    /// Insert a session, or refresh its name and activity if it is known.
    /// The parent and creation time are fixed by whoever saw it first.
    pub fn upsert_task_session(&self, s: &TaskSessionRecord) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_sessions (session_id, task_id, parent_id, name, created_at, last_activity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
               name = excluded.name,
               last_activity = MAX(last_activity, excluded.last_activity)",
            params![
                s.session_id,
                s.task_id,
                s.parent_id,
                s.name,
                s.created_at,
                s.last_activity,
            ],
        )?;
        Ok(())
    }

    /// A task's sessions, most recently active first.
    pub fn list_task_sessions(&self, task_id: &str) -> Result<Vec<TaskSessionRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, task_id, parent_id, name, created_at, last_activity
             FROM task_sessions WHERE task_id = ?1
             ORDER BY last_activity DESC, created_at DESC",
        )?;
        let rows = stmt.query_map(params![task_id], task_session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Give a session a new display name.
    pub fn rename_task_session(&self, session_id: &str, name: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE task_sessions SET name = ?2 WHERE session_id = ?1",
            params![session_id, name],
        )?;
        Ok(())
    }

    /// Mark `session_id` as the task's primary session, unmarking any other;
    /// `None` leaves it with none.
    pub fn set_primary_session(&self, task_id: &str, session_id: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE task_sessions SET is_primary = (session_id IS ?2) WHERE task_id = ?1",
            params![task_id, session_id],
        )?;
        Ok(())
    }

    /// The session marked primary, if any.
    pub fn primary_session(&self, task_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT session_id FROM task_sessions WHERE task_id = ?1 AND is_primary != 0
                 LIMIT 1",
                params![task_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Forget a session.
    pub fn delete_task_session(&self, session_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM task_sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn touch_task_session(&self, session_id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE task_sessions SET last_activity = MAX(last_activity, ?2) WHERE session_id = ?1",
            params![session_id, at],
        )?;
        Ok(())
    }

    /// Record what GitHub last said about a PR the task opened or reviewed.
    pub fn upsert_task_pr(&self, pr: &TaskPrRecord) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_prs (task_id, repo_id, number, title, url, state, is_draft,
                                   additions, deletions, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(task_id, repo_id, number) DO UPDATE SET
               title = excluded.title,
               url = excluded.url,
               state = excluded.state,
               is_draft = excluded.is_draft,
               additions = excluded.additions,
               deletions = excluded.deletions,
               updated_at = excluded.updated_at",
            params![
                pr.task_id,
                pr.repo_id,
                pr.number as i64,
                pr.title,
                pr.url,
                pr.state,
                i32::from(pr.is_draft),
                pr.additions as i64,
                pr.deletions as i64,
                pr.updated_at,
            ],
        )?;
        Ok(())
    }

    /// The tasks whose history records PR `number` of `repo_id`.
    pub fn tasks_with_pr(&self, repo_id: &str, number: u64) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT task_id FROM task_prs WHERE repo_id = ?1 AND number = ?2 ORDER BY task_id",
        )?;
        let rows = stmt.query_map(params![repo_id, number as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A task's PR history, most recently updated first.
    pub fn list_task_prs(&self, task_id: &str) -> Result<Vec<TaskPrRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT task_id, repo_id, number, title, url, state, is_draft, additions, deletions,
                    updated_at
             FROM task_prs WHERE task_id = ?1
             ORDER BY updated_at DESC, number DESC",
        )?;
        let rows = stmt.query_map(params![task_id], task_pr_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record a report, returning its id.
    pub fn insert_report(
        &self,
        task_id: &str,
        to_id: &str,
        kind: ReportKind,
        body: &str,
        at: i64,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO task_reports (task_id, to_id, kind, body, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, to_id, kind.as_str(), body, at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Record a `status` report about `task_id`, unless its last one starts
    /// with `word` and was sent at or after `since`: the same event told
    /// already. Checked and written in one write transaction, so two
    /// processes telling the same event record it once. Returns the new
    /// report's id, or None when it was told already.
    pub fn insert_status_report_unless_told(
        &self,
        task_id: &str,
        to_id: &str,
        word: &str,
        body: &str,
        since: i64,
        at: i64,
    ) -> Result<Option<i64>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last = tx
            .query_row(
                &format!(
                    "SELECT {TASK_REPORT_COLUMNS} FROM task_reports
                     WHERE task_id = ?1 AND kind = 'status' ORDER BY id DESC LIMIT 1"
                ),
                params![task_id],
                task_report_from_row,
            )
            .optional()?;
        if last.is_some_and(|r| r.body.starts_with(word) && r.at >= since) {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO task_reports (task_id, to_id, kind, body, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, to_id, ReportKind::Status.as_str(), body, at],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    /// The latest report about a task matching `filter`, a condition on
    /// `kind`. By id, not `at`: two in the same second keep their order.
    fn last_report_where(&self, task_id: &str, filter: &str) -> Result<Option<TaskReportRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {TASK_REPORT_COLUMNS} FROM task_reports
                     WHERE task_id = ?1 AND {filter} ORDER BY id DESC LIMIT 1"
                ),
                params![task_id],
                task_report_from_row,
            )
            .optional()?)
    }

    /// The latest report about a task, of any kind.
    pub fn last_report(&self, task_id: &str) -> Result<Option<TaskReportRecord>> {
        self.last_report_where(task_id, "1")
    }

    /// The latest report jeet sent about a task on its own.
    pub fn last_status_report(&self, task_id: &str) -> Result<Option<TaskReportRecord>> {
        self.last_report_where(task_id, "kind = 'status'")
    }

    /// The latest report the task sent itself.
    pub fn last_own_report(&self, task_id: &str) -> Result<Option<TaskReportRecord>> {
        self.last_report_where(task_id, "kind <> 'status'")
    }

    pub fn insert_brief(&self, brief: &TaskBriefRecord) -> Result<()> {
        insert_brief_in(&self.conn(), brief)
    }

    pub fn get_brief(&self, task_id: &str) -> Result<Option<TaskBriefRecord>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT task_id, goal, done_when, created_by FROM task_briefs WHERE task_id = ?1",
                params![task_id],
                |row| {
                    Ok(TaskBriefRecord {
                        task_id: row.get(0)?,
                        goal: row.get(1)?,
                        done_when: row.get(2)?,
                        created_by: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// `id`'s children in any state, oldest first.
    pub fn children(&self, id: &str) -> Result<Vec<TaskRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE parent_id = ?1 ORDER BY created_at, slug"
        ))?;
        let rows = stmt.query_map(params![id], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn child_count(&self, id: &str, active_only: bool) -> Result<usize> {
        child_count_in(&self.conn(), id, active_only)
    }

    /// Everything below `id`, with its depth under it (a child is 1), deepest
    /// first and then by slug: the order to work a subtree leaves first. A
    /// subtree deeper than [`MAX_HOPS`] (a cycle) is an error.
    pub fn descendants(&self, id: &str) -> Result<Vec<(TaskRecord, u32)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "WITH RECURSIVE sub(id, depth) AS (
                 SELECT id, 1 FROM tasks WHERE parent_id = ?1
                 UNION
                 SELECT t.id, sub.depth + 1 FROM tasks t JOIN sub ON t.parent_id = sub.id
                 WHERE sub.depth <= ?2
             )
             SELECT {}, sub.depth FROM sub JOIN tasks t ON t.id = sub.id
             ORDER BY sub.depth DESC, t.slug",
            prefixed_columns("t")
        ))?;
        let rows = stmt.query_map(params![id, MAX_HOPS], |row| {
            Ok((task_from_row(row)?, row.get::<_, u32>(13)?))
        })?;
        let found: Vec<(TaskRecord, u32)> = rows.collect::<rusqlite::Result<_>>()?;
        if found.first().is_some_and(|(_, depth)| *depth > MAX_HOPS) {
            bail!("the tree under task {id} is more than {MAX_HOPS} deep; it has a cycle");
        }
        Ok(found)
    }

    /// `id`'s ancestors, nearest first; see [`ancestors_in`].
    pub fn ancestors(&self, id: &str) -> Result<Vec<TaskRecord>> {
        ancestors_in(&self.conn(), id)
    }

    /// How many ancestors `id` has: 0 for the assistant and for a top-level
    /// task.
    pub fn depth(&self, id: &str) -> Result<u32> {
        Ok(self.ancestors(id)?.len() as u32)
    }

    /// Whether `id` is below `of`; never for `of` itself.
    pub fn is_strict_descendant(&self, id: &str, of: &str) -> Result<bool> {
        Ok(self.ancestors(id)?.iter().any(|a| a.id == of))
    }

    /// The capabilities granted to `id` itself, sorted; what it may use is
    /// this narrowed by its ancestors'.
    pub fn grants(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT capability FROM task_grants WHERE task_id = ?1 ORDER BY capability")?;
        let rows = stmt.query_map(params![id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replace `id`'s grant with `caps`.
    pub fn set_grants(&self, id: &str, caps: &[&str]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM task_grants WHERE task_id = ?1", params![id])?;
        insert_grants_in(&tx, id, caps)?;
        tx.commit()?;
        Ok(())
    }

    /// Record a child of `rec.parent_id` with its grant, budget (max seconds,
    /// max turns) and brief, all or nothing. The parent is read again under
    /// the write lock, so a child is never made under a parent archived since
    /// the caller looked, nor past `fan_out` active children by two makers at
    /// once.
    pub fn insert_child(
        &self,
        rec: &TaskRecord,
        grants: &[&str],
        budget: Option<(Option<i64>, Option<i64>)>,
        brief: Option<&TaskBriefRecord>,
        fan_out: Option<u32>,
    ) -> Result<()> {
        let Some(parent_id) = &rec.parent_id else {
            bail!("task {} has no parent to be made under", rec.slug);
        };
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(parent) = get_task_in(&tx, parent_id)? else {
            bail!("the parent of task {} is gone", rec.slug);
        };
        if parent.state != TaskState::Active {
            bail!("{} is archived; it takes no new tasks", parent.slug);
        }
        if let Some(max) = fan_out {
            let active = child_count_in(&tx, parent_id, true)?;
            if active >= max as usize {
                bail!(
                    "{} already has {active} active tasks under it, its limit of {max}; \
                     archive one first",
                    parent.slug
                );
            }
        }
        insert_task_in(&tx, rec)?;
        insert_grants_in(&tx, &rec.id, grants)?;
        if let Some((max_secs, max_turns)) = budget {
            tx.execute(
                "INSERT INTO task_budgets (task_id, max_secs, max_turns) VALUES (?1, ?2, ?3)",
                params![rec.id, max_secs, max_turns],
            )?;
        }
        if let Some(brief) = brief {
            insert_brief_in(&tx, brief)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Archive `id`'s row, unless it has an active child: checked and written
    /// under one write lock, so no child slips in between.
    pub fn archive_task(&self, id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(task) = get_task_in(&tx, id)? else {
            bail!("task {id} is not in the index");
        };
        let active = child_count_in(&tx, id, true)?;
        if active > 0 {
            bail!(
                "{} has {active} active tasks under it; archive or delete them first",
                task.slug
            );
        }
        tx.execute(
            "UPDATE tasks SET state = 'archived' WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Make `id`'s row active again, unless its parent is archived or already
    /// has `fan_out` active children; checked and written under one write
    /// lock.
    pub fn unarchive_task(&self, id: &str, fan_out: Option<u32>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(task) = get_task_in(&tx, id)? else {
            bail!("task {id} is not in the index");
        };
        if let Some(parent_id) = &task.parent_id {
            let Some(parent) = get_task_in(&tx, parent_id)? else {
                bail!("the parent of task {} is gone", task.slug);
            };
            if parent.state != TaskState::Active {
                bail!(
                    "{} is under {}, which is archived; unarchive {} first",
                    task.slug,
                    parent.slug,
                    parent.slug
                );
            }
            if let Some(max) = fan_out {
                let active: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM tasks
                     WHERE parent_id = ?1 AND state = 'active' AND id != ?2",
                    params![parent_id, id],
                    |row| row.get(0),
                )?;
                if active >= i64::from(max) {
                    bail!(
                        "{} already has {active} active tasks under it, its limit of {max}; \
                         archive one first",
                        parent.slug
                    );
                }
            }
        }
        tx.execute(
            "UPDATE tasks SET state = 'active' WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `id`'s own budget row, if it has a limit or has used anything.
    pub fn budget(&self, id: &str) -> Result<Option<BudgetRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT max_secs, max_turns, used_secs, used_turns, warned
                 FROM task_budgets WHERE task_id = ?1",
                params![id],
                |row| {
                    Ok(BudgetRow {
                        max_secs: row.get(0)?,
                        max_turns: row.get(1)?,
                        used_secs: row.get(2)?,
                        used_turns: row.get(3)?,
                        warned: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Set `id`'s limits, keeping what it has used. The thresholds are told
    /// again against the new limits.
    pub fn set_budget_max(&self, id: &str, secs: Option<i64>, turns: Option<i64>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_budgets (task_id, max_secs, max_turns) VALUES (?1, ?2, ?3)
             ON CONFLICT(task_id) DO UPDATE SET
               max_secs = excluded.max_secs,
               max_turns = excluded.max_turns,
               warned = 0",
            params![id, secs, turns],
        )?;
        Ok(())
    }

    /// Add to what `id` itself has used; a task with no limit of its own
    /// still counts toward its ancestors'.
    pub fn charge(&self, id: &str, secs: i64, turns: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_budgets (task_id, used_secs, used_turns) VALUES (?1, ?2, ?3)
             ON CONFLICT(task_id) DO UPDATE SET
               used_secs = used_secs + excluded.used_secs,
               used_turns = used_turns + excluded.used_turns",
            params![id, secs, turns],
        )?;
        Ok(())
    }

    /// What `id` and everything below it have used: (seconds, turns).
    pub fn subtree_used(&self, id: &str) -> Result<(i64, i64)> {
        Ok(self.conn().query_row(
            "WITH RECURSIVE sub(id, depth) AS (
                 SELECT ?1, 0
                 UNION
                 SELECT t.id, sub.depth + 1 FROM tasks t JOIN sub ON t.parent_id = sub.id
                 WHERE sub.depth < ?2
             )
             SELECT COALESCE(SUM(b.used_secs), 0), COALESCE(SUM(b.used_turns), 0)
             FROM (SELECT DISTINCT id FROM sub) s JOIN task_budgets b ON b.task_id = s.id",
            params![id, MAX_HOPS],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Record the last threshold told about `id`'s budget: 0, 80 or 100.
    pub fn set_warned(&self, id: &str, level: u8) -> Result<()> {
        self.conn().execute(
            "UPDATE task_budgets SET warned = ?2 WHERE task_id = ?1",
            params![id, level],
        )?;
        Ok(())
    }

    /// Set `id`'s own run state. `Run` (a resume) takes no setter and always
    /// applies; whether the caller may is its own check. Otherwise the state
    /// never drops in severity, and at the same severity the setter changes
    /// only to one that outranks it: user, then root, then the node nearest
    /// the root, then budget. So a user's stop outlives a later pause.
    pub fn set_run_state(&self, id: &str, state: RunState, by: Option<&Setter>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(task) = get_task_in(&tx, id)? else {
            bail!("task {id} is not in the index");
        };
        let write = match (state, by) {
            (RunState::Run, None) => true,
            (RunState::Run, Some(_)) => bail!("a running task has no setter"),
            (_, None) => bail!("a {} task needs a setter", state.as_str()),
            (_, Some(_)) if state > task.run_state => true,
            (_, Some(by)) if state == task.run_state => match &task.run_state_by {
                Some(old) => outranks_in(&tx, by, old)?,
                None => true,
            },
            _ => false,
        };
        if write {
            tx.execute(
                "UPDATE tasks SET run_state = ?2, run_state_by = ?3 WHERE id = ?1",
                params![id, state.as_str(), by.map(Setter::as_str)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The run state that applies to `id`: the most severe over it and its
    /// ancestors, with the nearest task holding it (`None` while it runs).
    pub fn effective_run_state(&self, id: &str) -> Result<(RunState, Option<TaskRecord>)> {
        let conn = self.conn();
        let Some(task) = get_task_in(&conn, id)? else {
            bail!("task {id} is not in the index");
        };
        let mut held: Option<TaskRecord> = None;
        for node in std::iter::once(task).chain(ancestors_in(&conn, id)?) {
            let more = held.as_ref().map_or(RunState::Run, |h| h.run_state);
            if node.run_state > more {
                held = Some(node);
            }
        }
        Ok(match held {
            Some(holder) => (holder.run_state, Some(holder)),
            None => (RunState::Run, None),
        })
    }

    /// Queue `id` to start once a `max_live` slot frees; a task queued
    /// already keeps its place.
    pub fn queue_start(&self, id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_start_queue (task_id, queued_at) VALUES (?1, ?2)
             ON CONFLICT(task_id) DO NOTHING",
            params![id, at],
        )?;
        Ok(())
    }

    /// The queued tasks, oldest first.
    pub fn queued_starts(&self) -> Result<Vec<TaskRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM task_start_queue q JOIN tasks t ON t.id = q.task_id
             ORDER BY q.queued_at, q.rowid",
            prefixed_columns("t")
        ))?;
        let rows = stmt.query_map([], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unqueue(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM task_start_queue WHERE task_id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Remove the rows of `ids`, in order, all or nothing: a parent may not
    /// go before its children, so the caller lists leaves first.
    pub fn delete_rows_leaves_first(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn insert_grants_in(conn: &Connection, id: &str, caps: &[&str]) -> Result<()> {
    for cap in caps {
        conn.execute(
            "INSERT OR IGNORE INTO task_grants (task_id, capability) VALUES (?1, ?2)",
            params![id, cap],
        )?;
    }
    Ok(())
}

fn insert_brief_in(conn: &Connection, brief: &TaskBriefRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO task_briefs (task_id, goal, done_when, created_by)
         VALUES (?1, ?2, ?3, ?4)",
        params![brief.task_id, brief.goal, brief.done_when, brief.created_by],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RepoRecord;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        (dir, db)
    }

    fn repo(id: &str) -> RepoRecord {
        RepoRecord {
            id: id.into(),
            trunk_path: format!("/store/{id}"),
            remote_url: format!("https://{id}.git"),
            default_branch: "main".into(),
        }
    }

    fn task(id: &str, slug: &str, last_activity: i64) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            slug: slug.into(),
            name: slug.replace('-', " "),
            project: None,
            kind: TaskKind::Work,
            state: TaskState::Active,
            created_at: 100,
            last_activity,
            parent_id: None,
            placement: Placement::Local,
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        }
    }

    fn worktree(task_id: &str, dir_name: &str, branch: Option<&str>) -> TaskWorktreeRecord {
        TaskWorktreeRecord {
            id: 0,
            task_id: task_id.into(),
            repo_id: "github.com/acme/api".into(),
            dir_name: dir_name.into(),
            branch: branch.map(Into::into),
            created_branch: branch.is_some(),
            head_oid: None,
            stash_ref: None,
            pr_number: None,
            created_at: 100,
            read_only: false,
        }
    }

    fn pr(task_id: &str, number: u64, updated_at: &str) -> TaskPrRecord {
        TaskPrRecord {
            task_id: task_id.into(),
            repo_id: "github.com/acme/api".into(),
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/acme/api/pull/{number}"),
            state: "OPEN".into(),
            is_draft: false,
            additions: 1,
            deletions: 2,
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn tasks_round_trip_and_list_by_activity() {
        let (_dir, db) = open();
        db.insert_task(&task("a", "first", 10)).unwrap();
        let mut review = task("b", "second", 30);
        review.kind = TaskKind::Review;
        review.project = Some("web".into());
        db.insert_task(&review).unwrap();
        db.insert_task(&task("c", "third", 20)).unwrap();

        assert_eq!(db.get_task("b").unwrap(), Some(review.clone()));
        assert_eq!(db.get_task_by_slug("second").unwrap(), Some(review));
        assert!(db.get_task("missing").unwrap().is_none());

        let slugs = |tasks: Vec<TaskRecord>| tasks.into_iter().map(|t| t.slug).collect::<Vec<_>>();
        assert_eq!(
            slugs(db.list_tasks(None).unwrap()),
            ["second", "third", "first"]
        );

        db.set_task_state("c", TaskState::Archived).unwrap();
        assert_eq!(
            slugs(db.list_tasks(Some(TaskState::Active)).unwrap()),
            ["second", "first"]
        );
        assert_eq!(
            slugs(db.list_tasks(Some(TaskState::Archived)).unwrap()),
            ["third"]
        );

        db.touch_task("a", 50).unwrap();
        db.touch_task("a", 5).unwrap();
        assert_eq!(db.get_task("a").unwrap().unwrap().last_activity, 50);
    }

    #[test]
    fn slugs_are_unique() {
        let (_dir, db) = open();
        db.insert_task(&task("a", "same", 1)).unwrap();
        assert!(db.insert_task(&task("b", "same", 1)).is_err());
    }

    #[test]
    fn task_worktrees_are_added_updated_and_removed() {
        let (_dir, db) = open();
        db.upsert_repo(&repo("github.com/acme/api")).unwrap();
        db.insert_task(&task("t", "work", 1)).unwrap();

        let id = db
            .insert_task_worktree(&worktree("t", "api", Some("feat")))
            .unwrap();
        db.insert_task_worktree(&worktree("t", "api-2", None))
            .unwrap();
        // Directory names are unique within a task.
        assert!(db
            .insert_task_worktree(&worktree("t", "api", None))
            .is_err());

        let rows = db.list_task_worktrees("t").unwrap();
        assert_eq!(
            rows.iter().map(|w| w.dir_name.as_str()).collect::<Vec<_>>(),
            ["api", "api-2"]
        );
        assert_eq!(rows[0].id, id);

        let mut archived = rows[0].clone();
        archived.head_oid = Some("abc".into());
        archived.stash_ref = Some("refs/jeet/stash/t/api".into());
        archived.pr_number = Some(42);
        db.update_task_worktree(&archived).unwrap();
        assert_eq!(db.get_task_worktree("t", "api").unwrap(), Some(archived));

        let holders = db
            .task_worktrees_on_branch("github.com/acme/api", "feat")
            .unwrap();
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].task_id, "t");
        assert_eq!(
            db.task_worktrees_of_repo("github.com/acme/api")
                .unwrap()
                .len(),
            2
        );
        assert!(db
            .task_worktrees_of_repo("github.com/acme/web")
            .unwrap()
            .is_empty());

        db.delete_task_worktree("t", "api").unwrap();
        assert!(db.get_task_worktree("t", "api").unwrap().is_none());
        assert_eq!(db.list_task_worktrees("t").unwrap().len(), 1);
    }

    #[test]
    fn a_task_worktree_needs_a_known_repo() {
        let (_dir, db) = open();
        db.insert_task(&task("t", "work", 1)).unwrap();
        assert!(db
            .insert_task_worktree(&worktree("t", "api", None))
            .is_err());
    }

    #[test]
    fn sessions_keep_their_parent_and_only_move_forward() {
        let (_dir, db) = open();
        db.insert_task(&task("t", "work", 1)).unwrap();
        let first = TaskSessionRecord {
            session_id: "s1".into(),
            task_id: "t".into(),
            parent_id: None,
            name: "work".into(),
            created_at: 10,
            last_activity: 10,
        };
        db.upsert_task_session(&first).unwrap();
        db.upsert_task_session(&TaskSessionRecord {
            session_id: "s2".into(),
            parent_id: Some("s1".into()),
            created_at: 20,
            last_activity: 20,
            ..first.clone()
        })
        .unwrap();
        // Seen again later, by a hook that knows nothing of the parent.
        db.upsert_task_session(&TaskSessionRecord {
            name: "renamed".into(),
            last_activity: 5,
            ..first.clone()
        })
        .unwrap();

        let sessions = db.list_task_sessions("t").unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "s2");
        assert_eq!(sessions[0].parent_id.as_deref(), Some("s1"));
        assert_eq!(sessions[1].name, "renamed");
        assert_eq!(sessions[1].last_activity, 10);

        db.touch_task_session("s1", 30).unwrap();
        assert_eq!(db.list_task_sessions("t").unwrap()[0].session_id, "s1");
    }

    #[test]
    fn there_is_one_assistant_and_it_is_never_listed() {
        let (_dir, db) = open();
        assert!(db.get_assistant_task().unwrap().is_none());
        let mut assistant = task("a", "assistant", 50);
        assistant.kind = TaskKind::Assistant;
        db.insert_task(&assistant).unwrap();
        db.insert_task(&task("w", "work", 10)).unwrap();
        assert_eq!(db.get_assistant_task().unwrap(), Some(assistant.clone()));
        let listed: Vec<String> = db
            .list_tasks(None)
            .unwrap()
            .into_iter()
            .map(|t| t.slug)
            .collect();
        assert_eq!(listed, ["work"]);
        // Found by id, as its hooks look it up.
        assert_eq!(db.get_task("a").unwrap(), Some(assistant));
        let mut second = task("b", "assistant-2", 1);
        second.kind = TaskKind::Assistant;
        assert!(db.insert_task(&second).is_err());
    }

    #[test]
    fn one_session_at_most_is_primary() {
        let (_dir, db) = open();
        db.insert_task(&task("t", "work", 1)).unwrap();
        db.insert_task(&task("u", "other", 1)).unwrap();
        for (id, task_id) in [("s1", "t"), ("s2", "t"), ("s3", "u")] {
            db.upsert_task_session(&TaskSessionRecord {
                session_id: id.into(),
                task_id: task_id.into(),
                parent_id: None,
                name: "x".into(),
                created_at: 1,
                last_activity: 1,
            })
            .unwrap();
        }
        assert_eq!(db.primary_session("t").unwrap(), None);
        db.set_primary_session("t", Some("s1")).unwrap();
        db.set_primary_session("u", Some("s3")).unwrap();
        assert_eq!(db.primary_session("t").unwrap().as_deref(), Some("s1"));
        db.set_primary_session("t", Some("s2")).unwrap();
        assert_eq!(db.primary_session("t").unwrap().as_deref(), Some("s2"));
        // Another task's mark is its own.
        assert_eq!(db.primary_session("u").unwrap().as_deref(), Some("s3"));
        db.set_primary_session("t", None).unwrap();
        assert_eq!(db.primary_session("t").unwrap(), None);

        db.rename_task_session("s1", "planning").unwrap();
        db.delete_task_session("s2").unwrap();
        let sessions = db.list_task_sessions("t").unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "planning");
    }

    #[test]
    fn pr_history_is_upserted() {
        let (_dir, db) = open();
        db.insert_task(&task("t", "work", 1)).unwrap();
        db.upsert_task_pr(&pr("t", 1, "2026-01-01T00:00:00Z"))
            .unwrap();
        db.upsert_task_pr(&pr("t", 2, "2026-02-01T00:00:00Z"))
            .unwrap();
        let mut merged = pr("t", 1, "2026-03-01T00:00:00Z");
        merged.state = "MERGED".into();
        merged.additions = 10;
        db.upsert_task_pr(&merged).unwrap();

        let prs = db.list_task_prs("t").unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0], merged);
        assert_eq!(prs[1].number, 2);

        assert_eq!(db.tasks_with_pr("github.com/acme/api", 1).unwrap(), ["t"]);
        assert!(db
            .tasks_with_pr("github.com/acme/web", 1)
            .unwrap()
            .is_empty());
        assert!(db
            .tasks_with_pr("github.com/acme/api", 3)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_review_task_is_found_by_its_pr() {
        let (_dir, db) = open();
        db.upsert_repo(&repo("github.com/acme/api")).unwrap();
        db.upsert_repo(&repo("github.com/other/api")).unwrap();
        let mut review = task("r", "review-api-7", 2);
        review.kind = TaskKind::Review;
        db.insert_task(&review).unwrap();
        // A work task that happens to have the same PR is not a review of it.
        db.insert_task(&task("w", "work", 1)).unwrap();
        db.upsert_task_pr(&pr("w", 7, "2026-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(db.find_review_task("github.com/acme/api", 7).unwrap(), None);

        let mut wt = worktree("r", "api", Some("feat"));
        wt.pr_number = Some(7);
        db.insert_task_worktree(&wt).unwrap();
        let found = db.find_review_task("github.com/acme/api", 7).unwrap();
        assert_eq!(found.map(|t| t.id), Some("r".to_string()));
        // Same number, another repository: another PR.
        assert_eq!(
            db.find_review_task("github.com/other/api", 7).unwrap(),
            None
        );
        assert_eq!(db.find_review_task("github.com/acme/api", 8).unwrap(), None);

        // With its worktree gone, the history still says which PR it reviews.
        db.delete_task_worktree("r", "api").unwrap();
        db.upsert_task_pr(&pr("r", 7, "2026-01-01T00:00:00Z"))
            .unwrap();
        let found = db.find_review_task("github.com/acme/api", 7).unwrap();
        assert_eq!(found.map(|t| t.id), Some("r".to_string()));
    }

    #[test]
    fn deleting_a_task_takes_everything_it_owns() {
        let (_dir, db) = open();
        db.upsert_repo(&repo("github.com/acme/api")).unwrap();
        db.insert_task(&task("t", "work", 1)).unwrap();
        db.insert_task(&task("other", "other", 1)).unwrap();
        db.insert_task_worktree(&worktree("t", "api", None))
            .unwrap();
        db.insert_task_worktree(&worktree("other", "api", None))
            .unwrap();
        db.upsert_task_session(&TaskSessionRecord {
            session_id: "s".into(),
            task_id: "t".into(),
            parent_id: None,
            name: "work".into(),
            created_at: 1,
            last_activity: 1,
        })
        .unwrap();
        db.upsert_task_pr(&pr("t", 1, "2026-01-01T00:00:00Z"))
            .unwrap();

        db.delete_task("t").unwrap();
        assert!(db.get_task("t").unwrap().is_none());
        assert!(db.list_task_worktrees("t").unwrap().is_empty());
        assert!(db.list_task_sessions("t").unwrap().is_empty());
        assert!(db.list_task_prs("t").unwrap().is_empty());
        // Nothing of the other task's goes with it.
        assert_eq!(db.list_task_worktrees("other").unwrap().len(), 1);
    }

    /// The assistant, and a task under it.
    fn parent_and_child(db: &Database) {
        let mut assistant = task("a", "assistant", 1);
        assistant.kind = TaskKind::Assistant;
        db.insert_task(&assistant).unwrap();
        let mut child = task("c", "child", 1);
        child.parent_id = Some("a".into());
        db.insert_task(&child).unwrap();
    }

    #[test]
    fn a_task_keeps_its_parent() {
        let (_dir, db) = open();
        parent_and_child(&db);
        assert_eq!(
            db.get_task("c").unwrap().unwrap().parent_id.as_deref(),
            Some("a")
        );
        assert_eq!(db.get_task("a").unwrap().unwrap().parent_id, None);
        // The parent must exist.
        let mut orphan = task("o", "orphan", 1);
        orphan.parent_id = Some("missing".into());
        assert!(db.insert_task(&orphan).is_err());
    }

    #[test]
    fn a_report_kind_must_be_known() {
        let (_dir, db) = open();
        parent_and_child(&db);
        assert!(db
            .conn()
            .execute_batch(
                "INSERT INTO task_reports (task_id, to_id, kind, body, at)
                 VALUES ('c', 'a', 'other', 'x', 1);"
            )
            .is_err());
    }

    #[test]
    fn an_event_told_at_once_by_many_processes_is_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        parent_and_child(&Database::open(&path).unwrap());
        let start = std::sync::Barrier::new(8);
        let told: Vec<Option<i64>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let db = Database::open(&path).unwrap();
                        start.wait();
                        db.insert_status_report_unless_told("c", "a", "idle", "idle 10m", 5, 6)
                            .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(told.iter().flatten().count(), 1, "{told:?}");

        let db = Database::open(&path).unwrap();
        // Told already: the same word since then.
        assert_eq!(
            db.insert_status_report_unless_told("c", "a", "idle", "idle 11m", 6, 7)
                .unwrap(),
            None
        );
        // Another event, or the same one since later, is news.
        let waiting = db
            .insert_status_report_unless_told("c", "a", "waiting", "waiting", 6, 7)
            .unwrap();
        assert!(waiting.is_some());
        let idle = db
            .insert_status_report_unless_told("c", "a", "idle", "idle 1m", 8, 8)
            .unwrap();
        assert!(idle > waiting);
    }

    #[test]
    fn the_last_report_is_the_last_written_even_in_the_same_second() {
        let (_dir, db) = open();
        parent_and_child(&db);
        assert_eq!(db.last_report("c").unwrap(), None);
        let first = db
            .insert_report("c", "a", ReportKind::Status, "waiting", 5)
            .unwrap();
        let second = db
            .insert_report("c", "a", ReportKind::Progress, "half", 5)
            .unwrap();
        let third = db
            .insert_report("c", "a", ReportKind::Status, "idle", 5)
            .unwrap();
        assert!(first < second && second < third);

        let last = db.last_report("c").unwrap().unwrap();
        assert_eq!(
            last,
            TaskReportRecord {
                id: third,
                task_id: "c".into(),
                to_id: "a".into(),
                kind: ReportKind::Status,
                body: "idle".into(),
                at: 5,
            }
        );
        assert_eq!(db.last_status_report("c").unwrap().unwrap().id, third);
        let own = db.last_own_report("c").unwrap().unwrap();
        assert_eq!((own.id, own.kind), (second, ReportKind::Progress));
        // Another task's reports are its own.
        assert_eq!(db.last_report("a").unwrap(), None);
    }

    #[test]
    fn a_brief_round_trips_and_goes_with_its_task() {
        let (_dir, db) = open();
        parent_and_child(&db);
        assert_eq!(db.get_brief("c").unwrap(), None);
        let brief = TaskBriefRecord {
            task_id: "c".into(),
            goal: "fix it".into(),
            done_when: Some("PR open".into()),
            created_by: "a".into(),
        };
        db.insert_brief(&brief).unwrap();
        assert_eq!(db.get_brief("c").unwrap(), Some(brief));
        db.insert_report("c", "a", ReportKind::Done, "ok", 1)
            .unwrap();

        db.delete_task("c").unwrap();
        assert_eq!(db.get_brief("c").unwrap(), None);
        assert_eq!(db.last_report("c").unwrap(), None);
    }

    /// Insert `id` under `parent`, of `kind`.
    fn node(db: &Database, id: &str, kind: TaskKind, parent: Option<&str>) {
        let mut rec = task(id, id, 1);
        rec.kind = kind;
        rec.parent_id = parent.map(Into::into);
        db.insert_task(&rec).unwrap();
    }

    /// The assistant `a`; under it the manager `m` and the worker `w1`;
    /// under `m` the manager `n` and the worker `w2`; under `n` the worker
    /// `w3`.
    fn tree(db: &Database) {
        node(db, "a", TaskKind::Assistant, None);
        node(db, "m", TaskKind::Manager, Some("a"));
        node(db, "w1", TaskKind::Work, Some("a"));
        node(db, "n", TaskKind::Manager, Some("m"));
        node(db, "w2", TaskKind::Work, Some("m"));
        node(db, "w3", TaskKind::Work, Some("n"));
    }

    fn ids(tasks: &[TaskRecord]) -> Vec<&str> {
        tasks.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn the_tree_is_walked_in_order_and_at_depth() {
        let (_dir, db) = open();
        tree(&db);
        assert_eq!(db.get_task("m").unwrap().unwrap().kind, TaskKind::Manager);
        assert_eq!(ids(&db.children("a").unwrap()), ["m", "w1"]);
        assert_eq!(db.child_count("m", false).unwrap(), 2);
        db.set_task_state("w2", TaskState::Archived).unwrap();
        assert_eq!(db.child_count("m", true).unwrap(), 1);

        let below = db.descendants("a").unwrap();
        let below: Vec<(&str, u32)> = below.iter().map(|(t, d)| (t.id.as_str(), *d)).collect();
        assert_eq!(below, [("w3", 3), ("n", 2), ("w2", 2), ("m", 1), ("w1", 1)]);
        assert!(db.descendants("w3").unwrap().is_empty());

        assert_eq!(ids(&db.ancestors("w3").unwrap()), ["n", "m", "a"]);
        assert!(db.ancestors("a").unwrap().is_empty());
        assert_eq!(db.depth("w3").unwrap(), 3);
        assert_eq!(db.depth("a").unwrap(), 0);
        assert!(db.ancestors("missing").is_err());

        assert!(db.is_strict_descendant("w3", "m").unwrap());
        assert!(db.is_strict_descendant("w3", "a").unwrap());
        assert!(!db.is_strict_descendant("m", "m").unwrap());
        assert!(!db.is_strict_descendant("w1", "m").unwrap());
        assert!(!db.is_strict_descendant("m", "w3").unwrap());
    }

    #[test]
    fn a_broken_tree_is_an_error_not_a_short_one() {
        let (_dir, db) = open();
        tree(&db);
        // A cycle made by hand: a walk stops at the cap and says so.
        db.conn()
            .execute_batch("UPDATE tasks SET parent_id = 'w3' WHERE id = 'm';")
            .unwrap();
        let err = db.ancestors("w3").unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
        let err = db.descendants("m").unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
        assert!(db.is_strict_descendant("w3", "x").is_err());

        // A parent that is gone.
        let (_dir, db) = open();
        tree(&db);
        db.conn()
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DELETE FROM tasks WHERE id = 'm';
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
        let err = db.ancestors("w3").unwrap_err();
        assert!(err.to_string().contains("gone"), "{err}");
        assert!(db.depth("w2").is_err());
    }

    #[test]
    fn grants_are_replaced_whole_and_go_with_the_task() {
        let (_dir, db) = open();
        tree(&db);
        assert!(db.grants("m").unwrap().is_empty());
        db.set_grants("m", &["worktree.add", "manager.create", "worktree.add"])
            .unwrap();
        assert_eq!(db.grants("m").unwrap(), ["manager.create", "worktree.add"]);
        db.set_grants("m", &["task.create"]).unwrap();
        assert_eq!(db.grants("m").unwrap(), ["task.create"]);
        db.set_grants("w3", &["worktree.add"]).unwrap();
        db.delete_task("w3").unwrap();
        assert!(db.grants("w3").unwrap().is_empty());
    }

    fn child(id: &str, parent: &str) -> TaskRecord {
        let mut rec = task(id, id, 1);
        rec.parent_id = Some(parent.into());
        rec
    }

    #[test]
    fn a_child_is_made_with_everything_or_nothing() {
        let (_dir, db) = open();
        tree(&db);
        let brief = TaskBriefRecord {
            task_id: "c".into(),
            goal: "fix it".into(),
            done_when: None,
            created_by: "m".into(),
        };
        db.insert_child(
            &child("c", "m"),
            &["worktree.add"],
            Some((Some(60), None)),
            Some(&brief),
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task("c").unwrap().unwrap().parent_id.as_deref(),
            Some("m")
        );
        assert_eq!(db.grants("c").unwrap(), ["worktree.add"]);
        assert_eq!(db.get_brief("c").unwrap(), Some(brief));
        let budget = db.budget("c").unwrap().unwrap();
        assert_eq!((budget.max_secs, budget.max_turns), (Some(60), None));

        // A brief for another task fails the insert, and nothing is left.
        let stray = TaskBriefRecord {
            task_id: "elsewhere".into(),
            goal: "x".into(),
            done_when: None,
            created_by: "m".into(),
        };
        assert!(db
            .insert_child(
                &child("d", "m"),
                &["worktree.add"],
                None,
                Some(&stray),
                None
            )
            .is_err());
        assert!(db.get_task("d").unwrap().is_none());
        assert!(db.grants("d").unwrap().is_empty());

        // Under a parent archived since the caller planned: refused.
        db.set_task_state("w3", TaskState::Archived).unwrap();
        db.set_task_state("n", TaskState::Archived).unwrap();
        let err = db
            .insert_child(&child("e", "n"), &[], None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("archived"), "{err}");
        assert!(db.get_task("e").unwrap().is_none());
        assert!(db
            .insert_child(&task("t", "t", 1), &[], None, None, None)
            .is_err());
    }

    #[test]
    fn two_makers_at_once_cannot_pass_the_fan_out() {
        for _ in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("index.db");
            tree(&Database::open(&path).unwrap());
            // `n` has one active child; room for one more.
            let start = std::sync::Barrier::new(2);
            let made: Vec<bool> = std::thread::scope(|s| {
                let handles: Vec<_> = ["x", "y"]
                    .into_iter()
                    .map(|id| {
                        let (path, start) = (&path, &start);
                        s.spawn(move || {
                            let db = Database::open(path).unwrap();
                            start.wait();
                            db.insert_child(&child(id, "n"), &[], None, None, Some(2))
                                .is_ok()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert_eq!(made.iter().filter(|m| **m).count(), 1, "{made:?}");
            let db = Database::open(&path).unwrap();
            assert_eq!(db.child_count("n", true).unwrap(), 2);
        }
    }

    #[test]
    fn archive_and_unarchive_keep_the_tree_whole() {
        let (_dir, db) = open();
        tree(&db);
        let err = db.archive_task("n").unwrap_err();
        assert!(err.to_string().contains("active"), "{err}");
        db.archive_task("w3").unwrap();
        db.archive_task("n").unwrap();
        assert_eq!(
            db.get_task("n").unwrap().unwrap().state,
            TaskState::Archived
        );

        // Not back under an archived parent.
        let err = db.unarchive_task("w3", None).unwrap_err();
        assert!(err.to_string().contains("archived"), "{err}");
        db.unarchive_task("n", None).unwrap();
        db.unarchive_task("w3", None).unwrap();
        assert_eq!(db.get_task("w3").unwrap().unwrap().state, TaskState::Active);

        // Not past the parent's fan-out: `m` at its limit of 2 (n, w2),
        // archive w2, add one, and w2 cannot come back.
        db.archive_task("w2").unwrap();
        db.insert_child(&child("w4", "m"), &[], None, None, Some(2))
            .unwrap();
        let err = db.unarchive_task("w2", Some(2)).unwrap_err();
        assert!(err.to_string().contains("limit of 2"), "{err}");
        assert_eq!(
            db.get_task("w2").unwrap().unwrap().state,
            TaskState::Archived
        );
        db.unarchive_task("w2", Some(3)).unwrap();
        // A top-level task has no parent to check.
        db.insert_task(&task("t", "top", 1)).unwrap();
        db.archive_task("t").unwrap();
        db.unarchive_task("t", Some(1)).unwrap();
    }

    #[test]
    fn budgets_count_a_whole_subtree() {
        let (_dir, db) = open();
        tree(&db);
        assert_eq!(db.budget("m").unwrap(), None);
        assert_eq!(db.subtree_used("m").unwrap(), (0, 0));
        db.set_budget_max("m", Some(600), Some(10)).unwrap();
        // Use is counted whether or not the task has a limit of its own.
        db.charge("m", 10, 1).unwrap();
        db.charge("w3", 20, 2).unwrap();
        db.charge("w3", 5, 1).unwrap();
        db.charge("w2", 100, 1).unwrap();
        db.charge("w1", 1000, 9).unwrap();
        assert_eq!(db.subtree_used("m").unwrap(), (135, 5));
        assert_eq!(db.subtree_used("n").unwrap(), (25, 3));
        assert_eq!(db.subtree_used("a").unwrap(), (1135, 14));
        let own = db.budget("w3").unwrap().unwrap();
        assert_eq!((own.max_secs, own.used_secs, own.used_turns), (None, 25, 3));

        db.set_warned("m", 80).unwrap();
        assert_eq!(db.budget("m").unwrap().unwrap().warned, 80);
        assert!(db.set_warned("m", 50).is_err());
        // A new limit keeps the use and tells the thresholds afresh.
        db.set_budget_max("m", Some(1200), None).unwrap();
        assert_eq!(
            db.budget("m").unwrap().unwrap(),
            BudgetRow {
                max_secs: Some(1200),
                max_turns: None,
                used_secs: 10,
                used_turns: 1,
                warned: 0,
            }
        );
        assert!(db.set_budget_max("m", Some(0), None).is_err());
    }

    #[test]
    fn the_most_severe_run_state_above_a_task_holds_it() {
        let (_dir, db) = open();
        tree(&db);
        assert_eq!(db.effective_run_state("w3").unwrap(), (RunState::Run, None));

        db.set_run_state("m", RunState::Paused, Some(&Setter::Root))
            .unwrap();
        let (state, holder) = db.effective_run_state("w3").unwrap();
        assert_eq!(
            (state, holder.map(|h| h.id)),
            (RunState::Paused, Some("m".into()))
        );
        db.set_run_state("n", RunState::Paused, Some(&Setter::Node("m".into())))
            .unwrap();
        let (_, holder) = db.effective_run_state("w3").unwrap();
        assert_eq!(holder.map(|h| h.id).as_deref(), Some("n"));
        db.set_run_state("w3", RunState::Stopped, Some(&Setter::Node("n".into())))
            .unwrap();
        let (state, holder) = db.effective_run_state("w3").unwrap();
        assert_eq!(
            (state, holder.map(|h| h.id)),
            (RunState::Stopped, Some("w3".into()))
        );
        assert_eq!(db.effective_run_state("w1").unwrap(), (RunState::Run, None));

        let rec = db.get_task("n").unwrap().unwrap();
        assert_eq!(rec.run_state, RunState::Paused);
        assert_eq!(rec.run_state_by, Some(Setter::Node("m".into())));
    }

    #[test]
    fn a_run_state_never_drops_and_keeps_its_higher_setter() {
        let (_dir, db) = open();
        tree(&db);
        let state = |id: &str| {
            let t = db.get_task(id).unwrap().unwrap();
            (t.run_state, t.run_state_by)
        };
        db.set_run_state("w3", RunState::Stopped, Some(&Setter::User))
            .unwrap();
        db.set_run_state("w3", RunState::Paused, Some(&Setter::Budget))
            .unwrap();
        assert_eq!(state("w3"), (RunState::Stopped, Some(Setter::User)));
        db.set_run_state("w3", RunState::Stopped, Some(&Setter::Root))
            .unwrap();
        assert_eq!(state("w3"), (RunState::Stopped, Some(Setter::User)));

        // At the same severity a higher setter takes over, a lower one not.
        db.set_run_state("n", RunState::Paused, Some(&Setter::Budget))
            .unwrap();
        db.set_run_state("n", RunState::Paused, Some(&Setter::Node("n".into())))
            .unwrap();
        assert_eq!(
            state("n"),
            (RunState::Paused, Some(Setter::Node("n".into())))
        );
        db.set_run_state("n", RunState::Paused, Some(&Setter::Node("m".into())))
            .unwrap();
        assert_eq!(
            state("n"),
            (RunState::Paused, Some(Setter::Node("m".into())))
        );
        db.set_run_state("n", RunState::Paused, Some(&Setter::Node("n".into())))
            .unwrap();
        db.set_run_state("n", RunState::Paused, Some(&Setter::Budget))
            .unwrap();
        assert_eq!(
            state("n"),
            (RunState::Paused, Some(Setter::Node("m".into())))
        );
        db.set_run_state("n", RunState::Paused, Some(&Setter::Root))
            .unwrap();
        assert_eq!(state("n"), (RunState::Paused, Some(Setter::Root)));
        // More severe wins whoever sets it.
        db.set_run_state("n", RunState::Stopped, Some(&Setter::Budget))
            .unwrap();
        assert_eq!(state("n"), (RunState::Stopped, Some(Setter::Budget)));

        // A resume clears it; whether the caller may is not the index's call.
        db.set_run_state("w3", RunState::Run, None).unwrap();
        assert_eq!(state("w3"), (RunState::Run, None));
        assert!(db
            .set_run_state("w3", RunState::Run, Some(&Setter::User))
            .is_err());
        assert!(db.set_run_state("w3", RunState::Paused, None).is_err());
        assert!(db
            .set_run_state("missing", RunState::Paused, Some(&Setter::User))
            .is_err());
    }

    #[test]
    fn starts_wait_in_order_and_keep_their_place() {
        let (_dir, db) = open();
        tree(&db);
        assert!(db.queued_starts().unwrap().is_empty());
        db.queue_start("w2", 20).unwrap();
        db.queue_start("w3", 10).unwrap();
        db.queue_start("w1", 20).unwrap();
        db.queue_start("w3", 30).unwrap();
        assert_eq!(ids(&db.queued_starts().unwrap()), ["w3", "w2", "w1"]);
        db.unqueue("w2").unwrap();
        assert_eq!(ids(&db.queued_starts().unwrap()), ["w3", "w1"]);
        db.delete_task("w1").unwrap();
        assert_eq!(ids(&db.queued_starts().unwrap()), ["w3"]);
    }

    #[test]
    fn a_subtree_is_deleted_leaves_first_or_not_at_all() {
        let (_dir, db) = open();
        tree(&db);
        // Parent before child: refused, and nothing goes.
        assert!(db
            .delete_rows_leaves_first(&["n".into(), "w3".into()])
            .is_err());
        assert!(db.get_task("w3").unwrap().is_some());
        let leaves_first: Vec<String> = db
            .descendants("m")
            .unwrap()
            .into_iter()
            .map(|(t, _)| t.id)
            .chain(["m".to_string()])
            .collect();
        db.delete_rows_leaves_first(&leaves_first).unwrap();
        assert_eq!(ids(&db.children("a").unwrap()), ["w1"]);
    }

    #[test]
    fn a_worktree_remembers_it_is_read_only() {
        let (_dir, db) = open();
        db.upsert_repo(&repo("github.com/acme/api")).unwrap();
        db.insert_task(&task("t", "work", 1)).unwrap();
        let mut wt = worktree("t", "api", None);
        wt.read_only = true;
        db.insert_task_worktree(&wt).unwrap();
        let mut row = db.get_task_worktree("t", "api").unwrap().unwrap();
        assert!(row.read_only);
        row.read_only = false;
        db.update_task_worktree(&row).unwrap();
        assert!(!db.get_task_worktree("t", "api").unwrap().unwrap().read_only);
    }

    #[test]
    fn a_review_task_found_by_its_pr_reads_every_column() {
        let (_dir, db) = open();
        db.upsert_repo(&repo("github.com/acme/api")).unwrap();
        let mut review = task("r", "review-api-7", 2);
        review.kind = TaskKind::Review;
        review.max_children = Some(3);
        db.insert_task(&review).unwrap();
        db.upsert_task_pr(&pr("r", 7, "2026-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(
            db.find_review_task("github.com/acme/api", 7).unwrap(),
            Some(review)
        );
    }
}
