//! Reviews (schema v8, `reviews`): one row per pull request, keyed by
//! (repo, PR number). Not a task: it has no parent, no children and no
//! brief. It owns one worktree and one session, kept as columns of its row,
//! and what a review task used to keep in files under its directory (what
//! its agent was asked, when it was brought back). Its limits are
//! `[review]`'s time and turns, given afresh each time a review is asked;
//! `requested_by` is the task, if any, that asked for it, which the review
//! agent's reports go to.

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use super::tasks::{BudgetCheck, BudgetRow, TaskState, TaskWorktreeRecord};
use super::Database;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    /// A uuid; a migrated review keeps its old task's id, which its hooks
    /// and `$JEET_TASK_ID` name.
    pub id: String,
    /// The directory under `tasks/` and the tmux session's name. Never
    /// changes.
    pub slug: String,
    pub repo_id: String,
    pub pr_number: u64,
    /// The PR's title: whoever opened it wrote it.
    pub title: String,
    /// GitHub's state, as last recorded: OPEN, CLOSED or MERGED.
    pub pr_state: Option<String>,
    pub pr_url: Option<String>,
    pub state: TaskState,
    /// The task that asked for it, whose reports it sends. Not a parent.
    pub requested_by: Option<String>,
    pub created_at: i64,
    pub last_activity: i64,
    /// Its one worktree's directory in the review directory; `None`: it has
    /// none (archived and forgotten, or not made yet).
    pub dir_name: Option<String>,
    pub branch: Option<String>,
    /// The review made the branch, so removing it may delete it.
    pub created_branch: bool,
    /// Where HEAD was when the worktree was archived.
    pub head_oid: Option<String>,
    /// The ref holding uncommitted work saved at archive time.
    pub stash_ref: Option<String>,
    pub worktree_created_at: Option<i64>,
    /// Its one session.
    pub session_id: Option<String>,
    /// The PR head its agent was last asked to review, and when.
    pub asked_head: Option<String>,
    pub asked_at: Option<i64>,
    /// When it was last unarchived, so one brought back after its PR was
    /// done is not archived again behind the user's back.
    pub brought_back_at: Option<i64>,
    /// `[review]`'s limits as of the last review asked: what it used then
    /// and the cap more.
    pub max_secs: Option<i64>,
    pub max_turns: Option<i64>,
    pub used_secs: i64,
    pub used_turns: i64,
    /// The last threshold told: 0, 80 or 100 (per cent).
    pub warned: u8,
    /// A limit is used up: it is handed nothing until asked again.
    pub paused: bool,
    /// When its start began waiting on `[review] max_live`.
    pub queued_at: Option<i64>,
}

impl ReviewRecord {
    /// A review of PR `number` of `repo_id` just made, with no worktree.
    pub fn new(id: String, slug: String, repo_id: &str, number: u64, now: i64) -> Self {
        Self {
            id,
            slug,
            repo_id: repo_id.to_string(),
            pr_number: number,
            title: String::new(),
            pr_state: None,
            pr_url: None,
            state: TaskState::Active,
            requested_by: None,
            created_at: now,
            last_activity: now,
            dir_name: None,
            branch: None,
            created_branch: false,
            head_oid: None,
            stash_ref: None,
            worktree_created_at: None,
            session_id: None,
            asked_head: None,
            asked_at: None,
            brought_back_at: None,
            max_secs: None,
            max_turns: None,
            used_secs: 0,
            used_turns: 0,
            warned: 0,
            paused: false,
            queued_at: None,
        }
    }

    /// Its worktree as a task's rows describe one, so the helpers that
    /// archive, restore and remove a task's worktrees serve it too. `id`
    /// is 0: it has no row of its own.
    pub fn worktree(&self) -> Option<TaskWorktreeRecord> {
        Some(TaskWorktreeRecord {
            id: 0,
            task_id: self.id.clone(),
            repo_id: self.repo_id.clone(),
            dir_name: self.dir_name.clone()?,
            branch: self.branch.clone(),
            created_branch: self.created_branch,
            head_oid: self.head_oid.clone(),
            stash_ref: self.stash_ref.clone(),
            pr_number: Some(self.pr_number),
            created_at: self.worktree_created_at.unwrap_or(self.created_at),
            read_only: false,
        })
    }

    /// `repo#n`, as it is shown in lists: `jeet#55`.
    pub fn label(&self) -> String {
        let repo = self.repo_id.rsplit('/').next().unwrap_or(&self.repo_id);
        format!("{repo}#{}", self.pr_number)
    }

    /// Its limits and use as a budget row reads.
    pub fn budget(&self) -> BudgetRow {
        BudgetRow {
            max_secs: self.max_secs,
            max_turns: self.max_turns,
            used_secs: self.used_secs,
            used_turns: self.used_turns,
            warned: self.warned,
        }
    }
}

/// A duplicate review task's worktree that migration to v8 left for
/// [`crate::review::migrate_leftovers`] to archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub task_id: String,
    pub slug: String,
    pub repo_id: String,
    pub pr_number: Option<u64>,
    pub dir_name: String,
    pub branch: Option<String>,
    pub created_branch: bool,
    pub head_oid: Option<String>,
    pub stash_ref: Option<String>,
    /// Its task was archived: its work is in `refs/jeet` already.
    pub was_archived: bool,
}

const REVIEW_COLUMNS: &str = "id, slug, repo_id, pr_number, title, pr_state, pr_url, state, \
     requested_by, created_at, last_activity, dir_name, branch, created_branch, head_oid, \
     stash_ref, worktree_created_at, session_id, asked_head, asked_at, brought_back_at, \
     max_secs, max_turns, used_secs, used_turns, warned, paused, queued_at";

fn review_from_row(row: &Row<'_>) -> rusqlite::Result<ReviewRecord> {
    let state: String = row.get(7)?;
    let state = match state.as_str() {
        "active" => TaskState::Active,
        "archived" => TaskState::Archived,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                format!("unknown review state {other:?}").into(),
            ))
        }
    };
    Ok(ReviewRecord {
        id: row.get(0)?,
        slug: row.get(1)?,
        repo_id: row.get(2)?,
        pr_number: row.get::<_, i64>(3)? as u64,
        title: row.get(4)?,
        pr_state: row.get(5)?,
        pr_url: row.get(6)?,
        state,
        requested_by: row.get(8)?,
        created_at: row.get(9)?,
        last_activity: row.get(10)?,
        dir_name: row.get(11)?,
        branch: row.get(12)?,
        created_branch: row.get::<_, i64>(13)? != 0,
        head_oid: row.get(14)?,
        stash_ref: row.get(15)?,
        worktree_created_at: row.get(16)?,
        session_id: row.get(17)?,
        asked_head: row.get(18)?,
        asked_at: row.get(19)?,
        brought_back_at: row.get(20)?,
        max_secs: row.get(21)?,
        max_turns: row.get(22)?,
        used_secs: row.get(23)?,
        used_turns: row.get(24)?,
        warned: row.get(25)?,
        paused: row.get::<_, i64>(26)? != 0,
        queued_at: row.get(27)?,
    })
}

fn get_review_in(conn: &Connection, id: &str) -> Result<Option<ReviewRecord>> {
    Ok(conn
        .query_row(
            &format!("SELECT {REVIEW_COLUMNS} FROM reviews WHERE id = ?1"),
            params![id],
            review_from_row,
        )
        .optional()?)
}

impl Database {
    /// Record a new review. Refused for a PR that has one already, or a
    /// slug a task or review has.
    pub fn insert_review(&self, r: &ReviewRecord) -> Result<()> {
        let conn = self.conn();
        let taken: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE slug = ?1 OR id = ?2)",
            params![r.slug, r.id],
            |row| row.get(0),
        )?;
        if taken {
            bail!("{} is the name of a task already", r.slug);
        }
        conn.execute(
            &format!(
                "INSERT INTO reviews ({REVIEW_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)"
            ),
            params![
                r.id,
                r.slug,
                r.repo_id,
                r.pr_number as i64,
                r.title,
                r.pr_state,
                r.pr_url,
                r.state.as_str(),
                r.requested_by,
                r.created_at,
                r.last_activity,
                r.dir_name,
                r.branch,
                i32::from(r.created_branch),
                r.head_oid,
                r.stash_ref,
                r.worktree_created_at,
                r.session_id,
                r.asked_head,
                r.asked_at,
                r.brought_back_at,
                r.max_secs,
                r.max_turns,
                r.used_secs,
                r.used_turns,
                r.warned,
                i32::from(r.paused),
                r.queued_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_review(&self, id: &str) -> Result<Option<ReviewRecord>> {
        get_review_in(&self.conn(), id)
    }

    /// The review of PR `number` of `repo_id`: there is at most one.
    pub fn find_review(&self, repo_id: &str, number: u64) -> Result<Option<ReviewRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {REVIEW_COLUMNS} FROM reviews WHERE repo_id = ?1 AND pr_number = ?2"
                ),
                params![repo_id, number as i64],
                review_from_row,
            )
            .optional()?)
    }

    pub fn find_review_by_slug(&self, slug: &str) -> Result<Option<ReviewRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {REVIEW_COLUMNS} FROM reviews WHERE slug = ?1"),
                params![slug],
                review_from_row,
            )
            .optional()?)
    }

    /// Reviews in `state` (all for `None`), most recently active first.
    pub fn list_reviews(&self, state: Option<TaskState>) -> Result<Vec<ReviewRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REVIEW_COLUMNS} FROM reviews WHERE (?1 IS NULL OR state = ?1)
             ORDER BY last_activity DESC, created_at DESC, slug"
        ))?;
        let rows = stmt.query_map(params![state.map(TaskState::as_str)], review_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The worktrees of the reviews of `repo_id`, as a task's rows describe
    /// them.
    pub fn review_worktrees_of_repo(&self, repo_id: &str) -> Result<Vec<TaskWorktreeRecord>> {
        Ok(self
            .list_reviews(None)?
            .iter()
            .filter(|r| r.repo_id == repo_id)
            .filter_map(ReviewRecord::worktree)
            .collect())
    }

    pub fn set_review_state(&self, id: &str, state: TaskState) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// Move a review's last activity forward to `at`; never back.
    pub fn touch_review(&self, id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET last_activity = MAX(last_activity, ?2) WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// Set (or, with `None`, clear) a review's one worktree.
    pub fn set_review_worktree(&self, id: &str, wt: Option<&TaskWorktreeRecord>) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews
             SET dir_name = ?2, branch = ?3, created_branch = ?4, head_oid = ?5, stash_ref = ?6,
                 worktree_created_at = ?7
             WHERE id = ?1",
            params![
                id,
                wt.map(|w| w.dir_name.as_str()),
                wt.and_then(|w| w.branch.as_deref()),
                wt.map_or(0, |w| i32::from(w.created_branch)),
                wt.and_then(|w| w.head_oid.as_deref()),
                wt.and_then(|w| w.stash_ref.as_deref()),
                wt.map(|w| w.created_at),
            ],
        )?;
        Ok(())
    }

    /// Record the review's one session.
    pub fn set_review_session(&self, id: &str, session_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET session_id = ?2 WHERE id = ?1",
            params![id, session_id],
        )?;
        Ok(())
    }

    /// Record the PR head its agent was asked to review.
    pub fn set_review_asked(&self, id: &str, head: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET asked_head = ?2, asked_at = ?3 WHERE id = ?1",
            params![id, head, at],
        )?;
        Ok(())
    }

    pub fn set_review_brought_back(&self, id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET brought_back_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
    }

    /// Who asked for the review: its reports go to them from now on.
    pub fn set_review_requested_by(&self, id: &str, by: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET requested_by = ?2 WHERE id = ?1",
            params![id, by],
        )?;
        Ok(())
    }

    /// What GitHub last said of the review's PR.
    pub fn set_review_pr(
        &self,
        id: &str,
        title: &str,
        state: &str,
        url: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET title = ?2, pr_state = ?3, pr_url = COALESCE(?4, pr_url)
             WHERE id = ?1",
            params![id, title, state, url],
        )?;
        Ok(())
    }

    /// Set a review's limits, keeping what it has used. The thresholds are
    /// told again against the new limits.
    pub fn set_review_budget_max(
        &self,
        id: &str,
        secs: Option<i64>,
        turns: Option<i64>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET max_secs = ?2, max_turns = ?3, warned = 0 WHERE id = ?1",
            params![id, secs, turns],
        )?;
        Ok(())
    }

    /// Add to what a review has used.
    pub fn charge_review(&self, id: &str, secs: i64, turns: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET used_secs = used_secs + ?2, used_turns = used_turns + ?3
             WHERE id = ?1",
            params![id, secs, turns],
        )?;
        Ok(())
    }

    /// Lift a pause a limit set, if it is set: false when the review was
    /// not paused, or has used a limit up still.
    pub fn resume_review(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(r) = get_review_in(&tx, id)? else {
            return Ok(false);
        };
        let b = r.budget();
        if !r.paused
            || b.max_secs.is_some_and(|max| b.used_secs >= max)
            || b.max_turns.is_some_and(|max| b.used_turns >= max)
        {
            return Ok(false);
        }
        tx.execute("UPDATE reviews SET paused = 0 WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(true)
    }

    /// The threshold review `id` is at, by `level` of its row, read afresh
    /// under one write lock: at 100 it is paused, and a threshold not told
    /// yet is recorded as told. With what was read, and whether this call
    /// is the one that recorded it; `None` with no limit. So of two checks
    /// crossing the same threshold at once only one tells it.
    pub fn check_review_budget(
        &self,
        id: &str,
        level: impl Fn(&BudgetRow, (i64, i64)) -> u8,
    ) -> Result<Option<BudgetCheck>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(r) = get_review_in(&tx, id)? else {
            return Ok(None);
        };
        let row = r.budget();
        if row.max_secs.is_none() && row.max_turns.is_none() {
            return Ok(None);
        }
        let used = (row.used_secs, row.used_turns);
        let at = level(&row, used);
        if at >= 100 {
            tx.execute("UPDATE reviews SET paused = 1 WHERE id = ?1", params![id])?;
        }
        let newly = at > row.warned;
        if newly {
            tx.execute(
                "UPDATE reviews SET warned = ?2 WHERE id = ?1",
                params![id, at],
            )?;
        }
        tx.commit()?;
        Ok(Some(BudgetCheck {
            row,
            used,
            level: at,
            newly,
        }))
    }

    /// Queue review `id` to start once a `[review] max_live` slot frees; one
    /// queued already keeps its place. True when it was not queued already.
    pub fn queue_review(&self, id: &str, at: i64) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE reviews SET queued_at = ?2 WHERE id = ?1 AND queued_at IS NULL",
            params![id, at],
        )?;
        Ok(changed == 1)
    }

    pub fn unqueue_review(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE reviews SET queued_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// The reviews waiting on `max_live`, oldest first.
    pub fn queued_reviews(&self) -> Result<Vec<ReviewRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REVIEW_COLUMNS} FROM reviews WHERE queued_at IS NOT NULL
             ORDER BY queued_at, created_at, slug"
        ))?;
        let rows = stmt.query_map([], review_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove a review's row, its anchors and the reports it sent. The
    /// caller deals with what is on disk.
    pub fn delete_review(&self, id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM task_anchors WHERE task_id = ?1", params![id])?;
        tx.execute("DELETE FROM task_reports WHERE task_id = ?1", params![id])?;
        tx.execute("DELETE FROM reviews WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// The migration's `meta` value for `key`, if it is there.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Set `key` to `1`, as the migration sets its flags.
    #[cfg(test)]
    pub fn set_meta_for_test(&self, key: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            params![key],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Worktrees of duplicate review tasks, not archived yet.
    pub fn pending_leftovers(&self) -> Result<Vec<Leftover>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT task_id, slug, repo_id, pr_number, dir_name, branch, created_branch,
                    head_oid, stash_ref, was_archived
             FROM review_leftovers WHERE done = 0 ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Leftover {
                task_id: row.get(0)?,
                slug: row.get(1)?,
                repo_id: row.get(2)?,
                pr_number: row.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                dir_name: row.get(4)?,
                branch: row.get(5)?,
                created_branch: row.get::<_, i64>(6)? != 0,
                head_oid: row.get(7)?,
                stash_ref: row.get(8)?,
                was_archived: row.get::<_, i64>(9)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Note how a leftover ended: done, with `note`.
    pub fn finish_leftover(&self, task_id: &str, dir_name: &str, note: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE review_leftovers SET done = 1, note = ?3
             WHERE task_id = ?1 AND dir_name = ?2",
            params![task_id, dir_name, note],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        (dir, db)
    }

    fn review(id: &str, number: u64) -> ReviewRecord {
        ReviewRecord::new(
            id.into(),
            format!("review-api-{number}"),
            "github.com/acme/api",
            number,
            10,
        )
    }

    #[test]
    fn a_pr_has_one_review() {
        let (_dir, db) = open();
        db.insert_review(&review("r1", 7)).unwrap();
        let mut again = review("r2", 7);
        again.slug = "review-api-7-2".into();
        assert!(db.insert_review(&again).is_err());
        assert_eq!(
            db.find_review("github.com/acme/api", 7)
                .unwrap()
                .unwrap()
                .id,
            "r1"
        );
        assert!(db.find_review("github.com/other/api", 7).unwrap().is_none());
        assert!(db.find_review("github.com/acme/api", 8).unwrap().is_none());
        db.insert_review(&review("r3", 8)).unwrap();
        assert_eq!(db.list_reviews(None).unwrap().len(), 2);
    }

    #[test]
    fn a_review_takes_no_slug_a_task_has() {
        let (_dir, db) = open();
        db.conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, created_at, last_activity)
                 VALUES ('t', 'review-api-7', 'x', 1, 1);",
            )
            .unwrap();
        assert!(db.insert_review(&review("r", 7)).is_err());
        // Nor does a task take a review's.
        db.insert_review(&review("r", 8)).unwrap();
        let err = db
            .insert_task(&tasks::TaskRecord {
                id: "n".into(),
                slug: "review-api-8".into(),
                name: "n".into(),
                project: None,
                kind: tasks::TaskKind::Work,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: crate::placement::Placement::Local,
                run_state: tasks::RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("name of a review"), "{err}");
    }

    #[test]
    fn a_review_keeps_its_worktree_and_session_on_its_row() {
        let (_dir, db) = open();
        db.insert_review(&review("r", 7)).unwrap();
        let wt = TaskWorktreeRecord {
            id: 0,
            task_id: "r".into(),
            repo_id: "github.com/acme/api".into(),
            dir_name: "api".into(),
            branch: Some("pr/7".into()),
            created_branch: true,
            head_oid: None,
            stash_ref: None,
            pr_number: Some(7),
            created_at: 11,
            read_only: false,
        };
        db.set_review_worktree("r", Some(&wt)).unwrap();
        db.set_review_session("r", "s1").unwrap();
        let got = db.get_review("r").unwrap().unwrap();
        assert_eq!(got.worktree(), Some(wt));
        assert_eq!(got.session_id.as_deref(), Some("s1"));
        db.set_review_worktree("r", None).unwrap();
        assert_eq!(db.get_review("r").unwrap().unwrap().worktree(), None);
    }

    #[test]
    fn a_limit_used_up_pauses_a_review_once_and_asking_again_lifts_it() {
        let (_dir, db) = open();
        db.insert_review(&review("r", 7)).unwrap();
        let level = |row: &BudgetRow, used: (i64, i64)| {
            let pct = row.max_turns.map_or(0, |max| used.1 * 100 / max);
            match pct {
                100.. => 100,
                80.. => 80,
                _ => 0,
            }
        };
        assert!(db.check_review_budget("r", level).unwrap().is_none());
        db.set_review_budget_max("r", None, Some(10)).unwrap();
        db.charge_review("r", 0, 8).unwrap();
        let check = db.check_review_budget("r", level).unwrap().unwrap();
        assert!(check.newly && check.level == 80);
        assert!(!db.get_review("r").unwrap().unwrap().paused);
        assert!(!db.check_review_budget("r", level).unwrap().unwrap().newly);
        db.charge_review("r", 0, 2).unwrap();
        let check = db.check_review_budget("r", level).unwrap().unwrap();
        assert!(check.newly && check.level == 100);
        assert!(db.get_review("r").unwrap().unwrap().paused);
        // Still used up: it stays paused.
        assert!(!db.resume_review("r").unwrap());
        db.set_review_budget_max("r", None, Some(30)).unwrap();
        assert!(db.resume_review("r").unwrap());
        assert!(!db.get_review("r").unwrap().unwrap().paused);
    }

    #[test]
    fn a_review_queues_once_and_leaves_the_queue() {
        let (_dir, db) = open();
        db.insert_review(&review("a", 1)).unwrap();
        db.insert_review(&review("b", 2)).unwrap();
        assert!(db.queue_review("b", 5).unwrap());
        assert!(db.queue_review("a", 6).unwrap());
        assert!(!db.queue_review("b", 9).unwrap());
        let queued: Vec<String> = db
            .queued_reviews()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(queued, ["b", "a"]);
        db.unqueue_review("b").unwrap();
        assert_eq!(db.queued_reviews().unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_review_takes_its_anchors_and_reports_and_unsets_requested_by() {
        let (_dir, db) = open();
        db.conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                     VALUES ('a', 'assistant', 'a', 'assistant', 1, 1);",
            )
            .unwrap();
        let mut r = review("r", 7);
        r.requested_by = Some("a".into());
        db.insert_review(&r).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO task_anchors (pid, start, task_id, kind, created_at)
                     VALUES (10, 20, 'r', 'pane', 1);
                 INSERT INTO task_reports (task_id, to_id, kind, body, at)
                     VALUES ('r', 'a', 'done', 'ok', 1);",
            )
            .unwrap();
        // The requester going leaves the review, asked by no one.
        let mut other = review("r2", 8);
        other.requested_by = Some("a".into());
        db.insert_review(&other).unwrap();
        db.delete_task("a").unwrap();
        assert_eq!(db.get_review("r2").unwrap().unwrap().requested_by, None);
        db.delete_review("r").unwrap();
        assert!(db.get_review("r").unwrap().is_none());
        assert!(db.all_anchors().unwrap().is_empty());
        assert!(db.reports_for("r", 10).unwrap().is_empty());
    }
}
