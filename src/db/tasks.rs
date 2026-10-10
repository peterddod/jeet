//! The task tables (schema v1): a task, the worktrees it holds, its agent
//! sessions and its PR history.

// The tasks milestone is the first caller; until it lands only the tests here
// use most of this.
#![allow(dead_code)]

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Work,
    /// Reviewing someone else's PR.
    Review,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Work => "work",
            TaskKind::Review => "review",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "work" => Ok(TaskKind::Work),
            "review" => Ok(TaskKind::Review),
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

const TASK_COLUMNS: &str = "id, slug, name, project, kind, state, created_at, last_activity";

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
    })
}

const TASK_WORKTREE_COLUMNS: &str = "id, task_id, repo_id, dir_name, branch, created_branch, \
     head_oid, stash_ref, pr_number, created_at";

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

impl Database {
    pub fn insert_task(&self, task: &TaskRecord) -> Result<()> {
        self.conn().execute(
            &format!("INSERT INTO tasks ({TASK_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
            params![
                task.id,
                task.slug,
                task.name,
                task.project,
                task.kind.as_str(),
                task.state.as_str(),
                task.created_at,
                task.last_activity,
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Option<TaskRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                params![id],
                task_from_row,
            )
            .optional()?)
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
    pub fn list_tasks(&self, state: Option<TaskState>) -> Result<Vec<TaskRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks
             WHERE ?1 IS NULL OR state = ?1
             ORDER BY last_activity DESC, created_at DESC, slug"
        ))?;
        let rows = stmt.query_map(params![state.map(TaskState::as_str)], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
                                         head_oid, stash_ref, pr_number, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Rewrite a worktree row's mutable fields (branch, archive state, PR),
    /// keyed by its id.
    pub fn update_task_worktree(&self, wt: &TaskWorktreeRecord) -> Result<()> {
        self.conn().execute(
            "UPDATE task_worktrees
             SET branch = ?2, created_branch = ?3, head_oid = ?4, stash_ref = ?5, pr_number = ?6
             WHERE id = ?1",
            params![
                wt.id,
                wt.branch,
                i32::from(wt.created_branch),
                wt.head_oid,
                wt.stash_ref,
                wt.pr_number.map(|n| n as i64),
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
            managed: true,
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
}
