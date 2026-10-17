use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};

pub mod anchors;
pub mod reviews;
pub mod tasks;

/// The schema as it stood before versioning: v0. Idempotent, so it runs on
/// every open and a fresh index and a legacy one start from the same place.
///
/// v0 also had a `worktrees` table for free-standing worktrees. It is not
/// made here any more — v2 drops it, and recreating it on every open would
/// undo that — but a legacy index still has it until v2 runs.
const BASELINE: &str = "
    CREATE TABLE IF NOT EXISTS repos (
        id TEXT PRIMARY KEY,
        trunk_path TEXT NOT NULL,
        remote_url TEXT NOT NULL,
        default_branch TEXT NOT NULL,
        managed INTEGER NOT NULL DEFAULT 0
    );
";

/// Schema steps after v0; entry `i` takes the index from v`i` to v`i + 1`.
/// Append only: a released step never changes, or indexes that already ran
/// it would disagree with ones that run the new text.
const MIGRATIONS: &[&str] = &[
    // v1: tasks. A task owns a workspace directory, any number of worktrees
    // added to it over its life, its agent sessions, and the PRs it opened.
    // The legacy `worktrees` table stays until import has consumed it.
    "
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        project TEXT,
        kind TEXT NOT NULL DEFAULT 'work' CHECK (kind IN ('work', 'review')),
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived')),
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL
    );
    CREATE TABLE task_worktrees (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        repo_id TEXT NOT NULL REFERENCES repos(id),
        dir_name TEXT NOT NULL,
        branch TEXT,
        created_branch INTEGER NOT NULL DEFAULT 0,
        head_oid TEXT,
        stash_ref TEXT,
        pr_number INTEGER,
        created_at INTEGER NOT NULL,
        UNIQUE(task_id, dir_name)
    );
    CREATE TABLE task_sessions (
        session_id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        parent_id TEXT,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL
    );
    CREATE TABLE task_prs (
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        repo_id TEXT NOT NULL,
        number INTEGER NOT NULL,
        title TEXT NOT NULL,
        url TEXT NOT NULL,
        state TEXT NOT NULL,
        is_draft INTEGER NOT NULL DEFAULT 0,
        additions INTEGER NOT NULL DEFAULT 0,
        deletions INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (task_id, repo_id, number)
    );
    CREATE INDEX task_worktrees_repo ON task_worktrees(repo_id, branch);
    CREATE INDEX task_sessions_task ON task_sessions(task_id);
    ",
    // v2: the hard cut to tasks. Every worktree belongs to a task now, so the
    // free-standing `worktrees` table goes. Nothing in it is lost: each row
    // named a worktree of an indexed trunk, which `git worktree list` on that
    // trunk still reports, and that is where `jeet import` finds them.
    "
    DROP TABLE IF EXISTS worktrees;
    ",
    // v3: the assistant, a task of kind 'assistant' — only ever one — whose
    // sessions are recorded like any task's; and the session of it the user
    // marked primary, which events go to first. SQLite cannot change a
    // CHECK in place, so `tasks` is rebuilt; foreign keys are off while
    // migrations run, so dropping the old table takes no rows with it.
    "
    CREATE TABLE tasks_v3 (
        id TEXT PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        project TEXT,
        kind TEXT NOT NULL DEFAULT 'work' CHECK (kind IN ('work', 'review', 'assistant')),
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived')),
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL
    );
    INSERT INTO tasks_v3 (id, slug, name, project, kind, state, created_at, last_activity)
        SELECT id, slug, name, project, kind, state, created_at, last_activity FROM tasks;
    DROP TABLE tasks;
    ALTER TABLE tasks_v3 RENAME TO tasks;
    CREATE UNIQUE INDEX tasks_one_assistant ON tasks(kind) WHERE kind = 'assistant';
    ALTER TABLE task_sessions ADD COLUMN is_primary INTEGER NOT NULL DEFAULT 0;
    ",
    // v4: hierarchy, phase 1. A task may have a parent (NULL: top level), what
    // its parent asked of it, and what it reported back. Additive only: the
    // kind CHECK is unchanged, so `tasks` is not rebuilt. When v6 rebuilds it
    // (for the manager kind), the rebuild must copy `parent_id` and
    // `placement` (v5) and re-create `tasks_one_assistant` and
    // `tasks_parent`. `needs_user`, `answer` and `answered_by` are for
    // questions escalated and answered (phase 4, no migration of its own).
    "
    ALTER TABLE tasks ADD COLUMN parent_id TEXT REFERENCES tasks(id) ON DELETE RESTRICT;
    CREATE INDEX tasks_parent ON tasks(parent_id);
    CREATE TABLE task_briefs (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
        goal TEXT NOT NULL,
        done_when TEXT,
        brief TEXT,
        created_by TEXT NOT NULL
    );
    CREATE TABLE task_reports (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        to_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        kind TEXT NOT NULL
            CHECK (kind IN ('progress', 'done', 'blocked', 'question', 'status')),
        body TEXT NOT NULL,
        at INTEGER NOT NULL,
        needs_user INTEGER NOT NULL DEFAULT 0,
        answer TEXT,
        answered_by TEXT
    );
    CREATE INDEX task_reports_to ON task_reports(to_id, answer);
    CREATE INDEX task_reports_task ON task_reports(task_id, id);
    ",
    // v5: where a task's agent runs (`placement`; only `local`, tmux on this
    // machine, for now: a confined placement widens the CHECK when it is
    // built, in a migration that rebuilds `tasks` as v6 does), and the processes jeet
    // launched for a task, keyed by pid and start time so a reused pid
    // never matches. Additive only.
    "
    ALTER TABLE tasks ADD COLUMN placement TEXT NOT NULL DEFAULT 'local'
        CHECK (placement IN ('local'));
    CREATE TABLE task_anchors (
        pid INTEGER NOT NULL,
        start INTEGER NOT NULL,
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        kind TEXT NOT NULL CHECK (kind IN ('pane', 'job')),
        pane_id TEXT,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (pid, start)
    );
    CREATE INDEX task_anchors_task ON task_anchors(task_id);
    ",
    // v6: hierarchy, phase 2. The `manager` kind, and on each task whether
    // it may run (`run_state`, with who held it: 'user', 'root', 'budget' or
    // a task id, NULL while it runs; no FK, as a node setter is an ancestor
    // of the row, which cannot go while the row exists) and how many active
    // children it may have. `tasks` is rebuilt for the kind CHECK, keeping
    // v5's `placement` CHECK as it is; a confined placement widens it in a
    // migration of its own. A task's grant (`worktree.add` for every
    // parented task that could add worktrees until now), its budget and what
    // it has used of it, the starts waiting for a free `max_live` slot, and
    // worktrees a manager holds read-only.
    "
    CREATE TABLE tasks_v6 (
        id TEXT PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        project TEXT,
        kind TEXT NOT NULL DEFAULT 'work'
            CHECK (kind IN ('work', 'review', 'assistant', 'manager')),
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived')),
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL,
        parent_id TEXT REFERENCES tasks(id) ON DELETE RESTRICT,
        placement TEXT NOT NULL DEFAULT 'local' CHECK (placement IN ('local')),
        run_state TEXT NOT NULL DEFAULT 'run'
            CHECK (run_state IN ('run', 'paused', 'stopped')),
        run_state_by TEXT,
        max_children INTEGER CHECK (max_children IS NULL OR max_children > 0),
        CHECK ((run_state = 'run') = (run_state_by IS NULL))
    );
    INSERT INTO tasks_v6 (id, slug, name, project, kind, state, created_at, last_activity,
                          parent_id, placement)
        SELECT id, slug, name, project, kind, state, created_at, last_activity,
               parent_id, placement FROM tasks;
    DROP TABLE tasks;
    ALTER TABLE tasks_v6 RENAME TO tasks;
    CREATE UNIQUE INDEX tasks_one_assistant ON tasks(kind) WHERE kind = 'assistant';
    CREATE INDEX tasks_parent ON tasks(parent_id);
    CREATE TABLE task_grants (
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        capability TEXT NOT NULL,
        PRIMARY KEY (task_id, capability)
    );
    INSERT INTO task_grants (task_id, capability)
        SELECT id, 'worktree.add' FROM tasks
        WHERE parent_id IS NOT NULL AND kind IN ('work', 'review');
    CREATE TABLE task_budgets (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
        max_secs INTEGER CHECK (max_secs IS NULL OR max_secs > 0),
        max_turns INTEGER CHECK (max_turns IS NULL OR max_turns > 0),
        used_secs INTEGER NOT NULL DEFAULT 0,
        used_turns INTEGER NOT NULL DEFAULT 0,
        warned INTEGER NOT NULL DEFAULT 0 CHECK (warned IN (0, 80, 100))
    );
    CREATE TABLE task_start_queue (
        task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
        queued_at INTEGER NOT NULL
    );
    ALTER TABLE task_worktrees ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
    ",
    // v7: every review is of kind 'review', which keeps it out of the task
    // lists. Only a review's checkout records a PR number, so a task of
    // kind 'work' holding one is a review made by hand or before the kind
    // was set everywhere.
    "
    UPDATE tasks SET kind = 'review'
        WHERE kind = 'work'
          AND EXISTS (SELECT 1 FROM task_worktrees w
                      WHERE w.task_id = tasks.id AND w.pr_number IS NOT NULL);
    ",
    // v8: reviews leave `tasks`. A review is one row in `reviews`, keyed by
    // (repo, PR): no parent, no children, no brief, one worktree and one
    // session, kept as columns of the row. A review task keeps its id and
    // slug as its review's, so a review agent running across the upgrade
    // keeps its tmux session, its hooks and `$JEET_TASK_ID`. Per PR the
    // newest review task is kept; the others' worktrees go to
    // `review_leftovers` for `review::migrate_leftovers` to archive, and a
    // review task with no PR is dropped the same way. Every row of a review
    // task goes explicitly: foreign keys are off while migrations run, so
    // the step ends with a `foreign_key_check` (see `apply_migrations`). A
    // kept review's reports stay; those of a superseded or dropped task,
    // and any sent to a review task, go. Then
    // `tasks` is rebuilt without the kind (v6's pattern), and `task_anchors`
    // and `task_reports` without the foreign key on `task_id`, which may be
    // a review's id now.
    "
    CREATE TABLE reviews (
        id TEXT PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        repo_id TEXT NOT NULL,
        pr_number INTEGER NOT NULL,
        title TEXT NOT NULL DEFAULT '',
        pr_state TEXT,
        pr_url TEXT,
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived')),
        requested_by TEXT REFERENCES tasks(id) ON DELETE SET NULL,
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL,
        dir_name TEXT,
        branch TEXT,
        created_branch INTEGER NOT NULL DEFAULT 0,
        head_oid TEXT,
        stash_ref TEXT,
        worktree_created_at INTEGER,
        session_id TEXT,
        asked_head TEXT,
        asked_at INTEGER,
        brought_back_at INTEGER,
        max_secs INTEGER,
        max_turns INTEGER,
        used_secs INTEGER NOT NULL DEFAULT 0,
        used_turns INTEGER NOT NULL DEFAULT 0,
        warned INTEGER NOT NULL DEFAULT 0 CHECK (warned IN (0, 80, 100)),
        paused INTEGER NOT NULL DEFAULT 0,
        queued_at INTEGER,
        UNIQUE (repo_id, pr_number)
    );
    CREATE TABLE review_leftovers (
        task_id TEXT NOT NULL,
        slug TEXT NOT NULL,
        repo_id TEXT NOT NULL,
        pr_number INTEGER,
        dir_name TEXT NOT NULL,
        branch TEXT,
        created_branch INTEGER NOT NULL DEFAULT 0,
        head_oid TEXT,
        stash_ref TEXT,
        was_archived INTEGER NOT NULL,
        done INTEGER NOT NULL DEFAULT 0,
        note TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (task_id, dir_name)
    );
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

    CREATE TEMP TABLE rv_pr AS
        SELECT t.id AS task_id, w.repo_id AS repo_id, w.pr_number AS pr_number
        FROM tasks t
        JOIN task_worktrees w ON w.id = (
            SELECT MIN(w2.id) FROM task_worktrees w2
            WHERE w2.task_id = t.id AND w2.pr_number IS NOT NULL)
        WHERE t.kind = 'review'
        UNION ALL
        SELECT t.id, p.repo_id, p.number
        FROM tasks t
        JOIN task_prs p ON p.rowid = (
            SELECT MIN(p2.rowid) FROM task_prs p2 WHERE p2.task_id = t.id)
        WHERE t.kind = 'review'
          AND NOT EXISTS (SELECT 1 FROM task_worktrees w
                          WHERE w.task_id = t.id AND w.pr_number IS NOT NULL);
    CREATE TEMP TABLE rv_kept AS
        SELECT p.task_id AS task_id, p.repo_id AS repo_id, p.pr_number AS pr_number
        FROM rv_pr p JOIN tasks t ON t.id = p.task_id
        WHERE NOT EXISTS (
            SELECT 1 FROM rv_pr q JOIN tasks u ON u.id = q.task_id
            WHERE q.repo_id = p.repo_id AND q.pr_number = p.pr_number
              AND q.task_id <> p.task_id
              AND (u.state = 'active', u.last_activity, u.created_at, u.id)
                  > (t.state = 'active', t.last_activity, t.created_at, t.id));
    CREATE TEMP TABLE rv_chosen AS
        SELECT k.task_id AS task_id,
               COALESCE(
                   (SELECT MIN(w.id) FROM task_worktrees w
                    WHERE w.task_id = k.task_id AND w.repo_id = k.repo_id
                      AND w.pr_number = k.pr_number),
                   (SELECT MIN(w.id) FROM task_worktrees w
                    WHERE w.task_id = k.task_id AND w.repo_id = k.repo_id)) AS wt_id
        FROM rv_kept k;

    INSERT INTO reviews (id, slug, repo_id, pr_number, title, pr_state, pr_url, state,
                         requested_by, created_at, last_activity,
                         dir_name, branch, created_branch, head_oid, stash_ref,
                         worktree_created_at, session_id,
                         max_secs, max_turns, used_secs, used_turns, warned, paused, queued_at)
        SELECT t.id, t.slug, k.repo_id, k.pr_number, COALESCE(p.title, ''), p.state, p.url,
               t.state,
               (SELECT par.id FROM tasks par WHERE par.id = t.parent_id AND par.kind = 'assistant'),
               t.created_at, t.last_activity,
               w.dir_name, w.branch, COALESCE(w.created_branch, 0), w.head_oid, w.stash_ref,
               w.created_at,
               (SELECT s.session_id FROM task_sessions s WHERE s.task_id = t.id
                ORDER BY s.is_primary DESC, s.last_activity DESC, s.created_at DESC,
                         s.session_id LIMIT 1),
               b.max_secs, b.max_turns, COALESCE(b.used_secs, 0), COALESCE(b.used_turns, 0),
               COALESCE(b.warned, 0),
               (t.run_state = 'paused' AND t.run_state_by = 'budget'),
               q.queued_at
        FROM rv_kept k
        JOIN tasks t ON t.id = k.task_id
        LEFT JOIN rv_chosen c ON c.task_id = t.id
        LEFT JOIN task_worktrees w ON w.id = c.wt_id
        LEFT JOIN task_prs p ON p.task_id = t.id AND p.repo_id = k.repo_id AND p.number = k.pr_number
        LEFT JOIN task_budgets b ON b.task_id = t.id
        LEFT JOIN task_start_queue q ON q.task_id = t.id;

    INSERT INTO review_leftovers (task_id, slug, repo_id, pr_number, dir_name, branch,
                                  created_branch, head_oid, stash_ref, was_archived, done)
        SELECT w.task_id, t.slug, w.repo_id, w.pr_number, w.dir_name, w.branch,
               w.created_branch, w.head_oid, w.stash_ref,
               (t.state = 'archived'), (t.state = 'archived')
        FROM task_worktrees w JOIN tasks t ON t.id = w.task_id
        WHERE t.kind = 'review'
          AND NOT EXISTS (SELECT 1 FROM rv_chosen c WHERE c.wt_id = w.id);

    INSERT INTO meta (key, value)
        SELECT 'reviews_v8_kept', group_concat(slug || ' ' || repo_id || '#' || pr_number, char(10))
        FROM reviews HAVING COUNT(*) > 0;
    INSERT INTO meta (key, value)
        SELECT 'reviews_v8_dropped', group_concat(t.slug, char(10))
        FROM tasks t
        WHERE t.kind = 'review' AND t.id NOT IN (SELECT task_id FROM rv_pr)
        HAVING COUNT(*) > 0;
    INSERT INTO meta (key, value)
        SELECT 'reviews_v8_superseded', group_concat(t.slug, char(10))
        FROM tasks t
        WHERE t.kind = 'review' AND t.id IN (SELECT task_id FROM rv_pr)
          AND t.id NOT IN (SELECT task_id FROM rv_kept)
        HAVING COUNT(*) > 0;
    INSERT INTO meta (key, value)
        SELECT 'reviews_v8_pending', '1'
        WHERE EXISTS (SELECT 1 FROM reviews) OR EXISTS (SELECT 1 FROM review_leftovers WHERE done = 0);

    DELETE FROM task_worktrees WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_sessions WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_prs WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_briefs WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_grants WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_budgets WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_start_queue WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM task_reports
        WHERE to_id IN (SELECT id FROM tasks WHERE kind = 'review')
           OR (task_id IN (SELECT id FROM tasks WHERE kind = 'review')
               AND task_id NOT IN (SELECT id FROM reviews));
    DELETE FROM task_anchors
        WHERE task_id IN (SELECT id FROM tasks WHERE kind = 'review')
          AND task_id NOT IN (SELECT id FROM reviews);
    UPDATE tasks SET parent_id = NULL
        WHERE parent_id IN (SELECT id FROM tasks WHERE kind = 'review');
    DELETE FROM tasks WHERE kind = 'review';
    DROP TABLE rv_chosen;
    DROP TABLE rv_kept;
    DROP TABLE rv_pr;

    CREATE TABLE tasks_v8 (
        id TEXT PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        project TEXT,
        kind TEXT NOT NULL DEFAULT 'work'
            CHECK (kind IN ('work', 'assistant', 'manager')),
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived')),
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL,
        parent_id TEXT REFERENCES tasks(id) ON DELETE RESTRICT,
        placement TEXT NOT NULL DEFAULT 'local' CHECK (placement IN ('local')),
        run_state TEXT NOT NULL DEFAULT 'run'
            CHECK (run_state IN ('run', 'paused', 'stopped')),
        run_state_by TEXT,
        max_children INTEGER CHECK (max_children IS NULL OR max_children > 0),
        CHECK ((run_state = 'run') = (run_state_by IS NULL))
    );
    INSERT INTO tasks_v8 (id, slug, name, project, kind, state, created_at, last_activity,
                          parent_id, placement, run_state, run_state_by, max_children)
        SELECT id, slug, name, project, kind, state, created_at, last_activity,
               parent_id, placement, run_state, run_state_by, max_children FROM tasks;
    DROP TABLE tasks;
    ALTER TABLE tasks_v8 RENAME TO tasks;
    CREATE UNIQUE INDEX tasks_one_assistant ON tasks(kind) WHERE kind = 'assistant';
    CREATE INDEX tasks_parent ON tasks(parent_id);

    CREATE TABLE task_anchors_v8 (
        pid INTEGER NOT NULL,
        start INTEGER NOT NULL,
        task_id TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('pane', 'job')),
        pane_id TEXT,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (pid, start)
    );
    INSERT INTO task_anchors_v8 (pid, start, task_id, kind, pane_id, created_at)
        SELECT pid, start, task_id, kind, pane_id, created_at FROM task_anchors;
    DROP TABLE task_anchors;
    ALTER TABLE task_anchors_v8 RENAME TO task_anchors;
    CREATE INDEX task_anchors_task ON task_anchors(task_id);

    CREATE TABLE task_reports_v8 (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id TEXT NOT NULL,
        to_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        kind TEXT NOT NULL
            CHECK (kind IN ('progress', 'done', 'blocked', 'question', 'status')),
        body TEXT NOT NULL,
        at INTEGER NOT NULL,
        needs_user INTEGER NOT NULL DEFAULT 0,
        answer TEXT,
        answered_by TEXT
    );
    INSERT INTO task_reports_v8 (id, task_id, to_id, kind, body, at, needs_user, answer,
                                 answered_by)
        SELECT id, task_id, to_id, kind, body, at, needs_user, answer, answered_by
        FROM task_reports;
    DROP TABLE task_reports;
    ALTER TABLE task_reports_v8 RENAME TO task_reports;
    CREATE INDEX task_reports_to ON task_reports(to_id, answer);
    CREATE INDEX task_reports_task ON task_reports(task_id, id);
    ",
    // v9: the deprecated `[review] command`'s job moves onto its review's row
    // (it was a JSON state file under `$JEET_HOME/reviews`). A job exists
    // while `job_started_at` is set. `review::adopt_jobs` makes reviews of
    // the files older jeets left, and `review_job_files` records each one it
    // has been through, so none is looked at twice.
    "
    ALTER TABLE reviews ADD COLUMN job_head TEXT;
    ALTER TABLE reviews ADD COLUMN job_command TEXT;
    ALTER TABLE reviews ADD COLUMN job_worktree TEXT;
    ALTER TABLE reviews ADD COLUMN job_log TEXT;
    ALTER TABLE reviews ADD COLUMN job_notify INTEGER;
    ALTER TABLE reviews ADD COLUMN job_env TEXT;
    ALTER TABLE reviews ADD COLUMN job_started_at INTEGER;
    ALTER TABLE reviews ADD COLUMN job_finished_at INTEGER;
    ALTER TABLE reviews ADD COLUMN job_exit_code INTEGER;
    ALTER TABLE reviews ADD COLUMN job_pid INTEGER;
    ALTER TABLE reviews ADD COLUMN job_runner_start INTEGER;
    CREATE TABLE review_job_files (
        path TEXT PRIMARY KEY,
        outcome TEXT NOT NULL,
        review_id TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        at INTEGER NOT NULL
    );
    ",
    // v10: a work task's own PR policy (`jeet task new --pr`), over its
    // project's and the global `[tasks] pr`; NULL when it names none.
    // Additive only.
    "
    ALTER TABLE tasks ADD COLUMN pr TEXT CHECK (pr IS NULL OR pr IN ('auto', 'draft', 'ask'));
    ",
];

/// A repository in the index. Its trunk is in the store, except for one
/// registered before the hard cut to tasks, which `jeet import` moves there.
/// (The `managed` column that told the two apart is no longer read.)
#[derive(Debug, Clone)]
pub struct RepoRecord {
    pub id: String,
    pub trunk_path: String,
    pub remote_url: String,
    pub default_branch: String,
}

/// The index.
///
/// The connection sits behind a mutex so `&App` is `Sync`: the explorer runs
/// slow git work on a background thread while it keeps drawing, and that thread
/// needs to share the app.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create index db parent")?;
        }
        let conn = Connection::open(path).context("open index db")?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Recover from a poisoned lock rather than propagating a panic: a failed
    /// query in another thread must not take the index down with it.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bring the schema up to date.
    ///
    /// The baseline (`repos`) predates versioning, so it is
    /// created idempotently and counts as v0; every change after it is a step
    /// in [`MIGRATIONS`], applied in order inside a transaction that also
    /// bumps `PRAGMA user_version`, so a crash part-way leaves the index at
    /// the last whole version rather than half-way through one.
    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn();
        // Off while the schema changes: rebuilding a table means dropping
        // the old one, which with them on would cascade to every row that
        // references it. Per connection, and a no-op inside a transaction,
        // so set outside one.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        conn.execute_batch(BASELINE)?;
        let current = effective_version(&conn)?;
        let latest = MIGRATIONS.len() as i64;
        if current > latest {
            bail!(
                "the index is at schema v{current} but this jeet only knows up to v{latest}; \
                 upgrade jeet"
            );
        }
        if current < latest {
            if let Some(path) = conn.path().filter(|p| !p.is_empty()) {
                refuse_dev_migration(Path::new(path), current, latest)?;
            }
            migrate_from(&mut conn, current, latest)?;
        }
        // Off by default in SQLite, and the task tables lean on ON DELETE
        // CASCADE.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(())
    }

    #[cfg(test)]
    fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    pub fn upsert_repo(&self, repo: &RepoRecord) -> Result<()> {
        self.conn().execute(
            "INSERT INTO repos (id, trunk_path, remote_url, default_branch)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
               trunk_path = excluded.trunk_path,
               remote_url = excluded.remote_url,
               default_branch = excluded.default_branch",
            params![
                repo.id,
                repo.trunk_path,
                repo.remote_url,
                repo.default_branch,
            ],
        )?;
        Ok(())
    }

    pub fn list_repos(&self, filter: Option<&str>) -> Result<Vec<RepoRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT id, trunk_path, remote_url, default_branch FROM repos ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(RepoRecord {
                id: row.get(0)?,
                trunk_path: row.get(1)?,
                remote_url: row.get(2)?,
                default_branch: row.get(3)?,
            })
        })?;
        let mut repos = Vec::new();
        for row in rows {
            repos.push(row?);
        }
        if let Some(f) = filter {
            let f = f.to_lowercase();
            repos.retain(|r| {
                r.id.to_lowercase().contains(&f) || r.remote_url.to_lowercase().contains(&f)
            });
        }
        Ok(repos)
    }

    pub fn get_repo(&self, id: &str) -> Result<Option<RepoRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, trunk_path, remote_url, default_branch FROM repos WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(RepoRecord {
                id: row.get(0)?,
                trunk_path: row.get(1)?,
                remote_url: row.get(2)?,
                default_branch: row.get(3)?,
            }));
        }
        Ok(None)
    }
}

/// Set to `1` to let a development build migrate the index at the default
/// home anyway.
const ALLOW_DEV_MIGRATE: &str = "JEET_ALLOW_DEV_MIGRATE";

/// Refuse to migrate the index at the default home (`~/.jeet`) from a
/// development build: one built from an unreleased main would take it to a
/// schema the installed release cannot open, and every jeet command would
/// fail until it was restored from the backup.
fn refuse_dev_migration(index: &Path, current: i64, latest: i64) -> Result<()> {
    let Some(exe) = dev_build() else {
        return Ok(());
    };
    if std::env::var(ALLOW_DEV_MIGRATE).as_deref() == Ok("1") {
        return Ok(());
    }
    let Some(default) = dirs::home_dir().map(|h| crate::config::index_path(&h.join(".jeet")))
    else {
        return Ok(());
    };
    if !same_file(index, &default) {
        return Ok(());
    }
    bail!(
        "this is a development build of jeet ({exe}); it will not migrate the index at {} \
         from schema v{current} to v{latest}, which the installed jeet could not then open. \
         Set JEET_HOME to a scratch directory (e.g. JEET_HOME=$(mktemp -d)), or \
         {ALLOW_DEV_MIGRATE}=1 to migrate it anyway",
        index.display()
    )
}

/// What makes this binary a development build, if it is one: a debug build,
/// or one run from inside a cargo target directory: `target/<profile>/`,
/// `target/<triple>/<profile>/` or `target/<profile>/deps/`, where cargo
/// marks `target` with a `CACHEDIR.TAG`. Only those two levels are looked
/// at, so a release unpacked somewhere under an unrelated tagged cache is
/// not mistaken for one. An installed release is neither.
fn dev_build() -> Option<String> {
    let exe = std::env::current_exe().ok();
    if cfg!(debug_assertions) {
        return Some(exe.map_or("debug build".into(), |e| e.display().to_string()));
    }
    let exe = exe?;
    exe.parent()?
        .ancestors()
        .skip(1)
        .take(2)
        .any(|dir| dir.join("CACHEDIR.TAG").is_file())
        .then(|| exe.display().to_string())
}

/// Whether two paths name the same file, through symlinks where they exist.
/// The file itself may not exist yet, so its directory is resolved then.
fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| {
        if let Ok(p) = p.canonicalize() {
            return p;
        }
        p.parent()
            .map(|d| {
                if d.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    d
                }
            })
            .and_then(|d| d.canonicalize().ok())
            .zip(p.file_name())
            .map_or(p.to_path_buf(), |(d, f)| d.join(f))
    };
    canon(a) == canon(b)
}

/// Take the index from `current` to `latest`, after a copy of it as it is:
/// `VACUUM INTO <index dir>/backups/index-v<current>-<unix time>.db`, so
/// that a migration which goes wrong costs nothing that cannot be put
/// back. Only for an index that has tables of its own; a fresh one has
/// nothing to lose. Migrating is refused when the copy cannot be made.
/// Under a lock beside the index, so of several processes opening an old
/// one at once only the first copies it, and none copies it after it has
/// been migrated.
fn migrate_from(conn: &mut Connection, current: i64, latest: i64) -> Result<()> {
    let Some(path) = conn.path().map(std::path::PathBuf::from) else {
        return apply_migrations(conn, current);
    };
    let lock = path.with_extension("db.migrate.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .with_context(|| format!("open {}", lock.display()))?;
    lock.lock()
        .with_context(|| "lock the index for migrating".to_string())?;
    // Another process may have migrated it while this one waited.
    let current = effective_version(conn)?;
    if current >= latest {
        return Ok(());
    }
    let has_tasks: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tasks')",
        [],
        |row| row.get(0),
    )?;
    // A pre-versioning index with no tasks table has only `repos` to lose.
    let has_repos: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'repos')
         AND EXISTS (SELECT 1 FROM repos)",
        [],
        |row| row.get(0),
    )?;
    if current > 0 || has_tasks || has_repos {
        let backup = back_up(conn, &path, current)?;
        eprintln!(
            "jeet: backed up the index to {} before migrating it to schema v{latest}",
            backup.display()
        );
    }
    apply_migrations(conn, current)
}

/// Copy the index to `backups/index-v<version>-<unix time>.db` beside it,
/// returning the copy's path. A copy is never reused: one already there
/// gets a `-2` (`-3`, ...) suffix on the new one.
fn back_up(conn: &Connection, index: &Path, version: i64) -> Result<std::path::PathBuf> {
    let dir = index.parent().unwrap_or(Path::new(".")).join("backups");
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "create {} for a backup of the index; not migrating it",
            dir.display()
        )
    })?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut backup = dir.join(format!("index-v{version}-{at}.db"));
    let mut n = 2;
    while backup.exists() {
        backup = dir.join(format!("index-v{version}-{at}-{n}.db"));
        n += 1;
    }
    conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])
        .with_context(|| {
            format!(
                "back up the index to {} before migrating it; not migrating it",
                backup.display()
            )
        })?;
    Ok(backup)
}

/// The schema version this framework last brought the index to.
///
/// Every step from v1 on leaves a `tasks` table, so a stamped version without
/// one was not written by us: the experimental workspace builds (v0.3) stamped
/// `user_version` under a numbering of their own, on top of the same `repos`
/// table. Such an index is migrated as v0 rather than refused as "newer".
/// Apply each step in [`MIGRATIONS`] after `current`. Another process
/// opening the index may be migrating it too, as the first hooks after an
/// upgrade all are: each step takes the write lock first and is skipped if
/// it turns out to be done already.
fn apply_migrations(conn: &mut Connection, current: i64) -> Result<()> {
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i + 1;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if effective_version(&tx)? >= version as i64 {
            continue;
        }
        // v8 deletes rows by hand with foreign keys off: a row it leaves
        // pointing at one of them fails the step, and the index is as it
        // was. One that pointed nowhere before, an older jeet's, is not v8's.
        let before = match version {
            8 => dangling(&tx)?,
            _ => Vec::new(),
        };
        tx.execute_batch(sql)
            .with_context(|| format!("migrate index to v{version}"))?;
        if version == 8 {
            let new: Vec<String> = dangling(&tx)?
                .into_iter()
                .filter_map(|(table, after)| {
                    let was = before.iter().find(|(t, _)| *t == table).map_or(0, |b| b.1);
                    (after > was).then(|| format!("{} in {table}", after - was))
                })
                .collect();
            if !new.is_empty() {
                bail!(
                    "migrate index to v{version}: it would leave rows pointing at a deleted \
                     row ({}); the index is unchanged, and was backed up first under backups/",
                    new.join(", ")
                );
            }
        }
        // PRAGMA takes no bound parameters; `version` is our own integer.
        tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
        tx.commit()?;
    }
    Ok(())
}

/// How many rows of each table point at no row through a foreign key, as
/// `(table -> parent, count)`. By table, not by rowid: a rebuilt table's
/// rows may be renumbered.
fn dangling(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT \"table\" || ' -> ' || parent, COUNT(*) FROM pragma_foreign_key_check
         GROUP BY \"table\", parent ORDER BY 1",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn effective_version(conn: &Connection) -> Result<i64> {
    let stamped: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if stamped == 0 {
        return Ok(0);
    }
    let has_tasks: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tasks')",
        [],
        |row| row.get(0),
    )?;
    Ok(if has_tasks { stamped } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn a_fresh_index_is_at_the_latest_version() {
        let (_dir, db) = open();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_step_another_process_applied_since_is_skipped() {
        let (_dir, db) = open();
        // As a process that read v3 before another took the index to the
        // latest: v4's ALTER would fail on the column already there.
        apply_migrations(&mut db.conn(), 3).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    /// What an index written before versioning holds: the v0 tables, with
    /// the free-standing `worktrees` table, and user_version 0.
    const LEGACY_V0: &str = "
        CREATE TABLE repos (
            id TEXT PRIMARY KEY,
            trunk_path TEXT NOT NULL,
            remote_url TEXT NOT NULL,
            default_branch TEXT NOT NULL,
            managed INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE worktrees (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
            branch TEXT NOT NULL,
            path TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(repo_id, branch)
        );
        INSERT INTO repos (id, trunk_path, remote_url, default_branch, managed)
            VALUES ('github.com/acme/api', '/t', 'u', 'main', 0);
        INSERT INTO worktrees (repo_id, branch, path, created_at)
            VALUES ('github.com/acme/api', 'feat', '/w', 1);
    ";

    fn has_table(db: &Database, name: &str) -> bool {
        db.conn()
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [name],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_legacy_index_is_migrated_and_keeps_its_repos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(LEGACY_V0)
            .unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(db.list_repos(None).unwrap().len(), 1);
        assert!(db.list_tasks(None).unwrap().is_empty());
        // v2: free-standing worktrees are gone with the hard cut.
        assert!(!has_table(&db, "worktrees"));
        drop(db);
        // Opening again is a no-op, and the baseline does not bring the
        // dropped table back.
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert!(!has_table(&db, "worktrees"));
    }

    #[test]
    fn a_v1_index_loses_only_the_legacy_worktrees_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(LEGACY_V0).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 1;
                 INSERT INTO tasks (id, slug, name, created_at, last_activity)
                     VALUES ('t1', 'fix', 'Fix', 1, 1);",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert!(!has_table(&db, "worktrees"));
        assert_eq!(db.list_tasks(None).unwrap().len(), 1);
        assert_eq!(db.list_repos(None).unwrap().len(), 1);
    }

    #[test]
    fn rebuilding_tasks_for_the_assistant_keeps_every_row_that_points_at_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(BASELINE).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 2;
                 PRAGMA foreign_keys = ON;
                 INSERT INTO repos (id, trunk_path, remote_url, default_branch)
                     VALUES ('github.com/acme/api', '/t', 'u', 'main');
                 INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                     VALUES ('t1', 'fix', 'Fix', 'work', 1, 1);
                 INSERT INTO task_worktrees (task_id, repo_id, dir_name, created_at)
                     VALUES ('t1', 'github.com/acme/api', 'api', 1);
                 INSERT INTO task_sessions (session_id, task_id, name, created_at, last_activity)
                     VALUES ('s1', 't1', 'Fix', 1, 1);
                 INSERT INTO task_prs (task_id, repo_id, number, title, url, state, updated_at)
                     VALUES ('t1', 'github.com/acme/api', 7, 'x', 'u', 'OPEN', 'z');",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let task = db.get_task("t1").unwrap().unwrap();
        assert_eq!(task.kind, tasks::TaskKind::Work);
        assert_eq!(db.list_task_worktrees("t1").unwrap().len(), 1);
        assert_eq!(db.list_task_sessions("t1").unwrap().len(), 1);
        assert_eq!(db.list_task_prs("t1").unwrap().len(), 1);
        // Foreign keys are back on, and still point at the rebuilt table.
        db.delete_task("t1").unwrap();
        assert!(db.list_task_worktrees("t1").unwrap().is_empty());
        assert!(db.list_task_sessions("t1").unwrap().is_empty());
    }

    /// An index as v3 left it, with the assistant `a` and a task `w`.
    fn v3_index(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(BASELINE).unwrap();
        for sql in &MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 3;
             PRAGMA foreign_keys = ON;
             INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('a', 'assistant', 'assistant', 'assistant', 1, 1);
             INSERT INTO tasks (id, slug, name, created_at, last_activity)
                 VALUES ('w', 'work', 'work', 1, 1);",
        )
        .unwrap();
    }

    #[test]
    fn many_processes_opening_an_old_index_at_once_all_migrate_it() {
        // As the first hooks after an upgrade: each step must take the write
        // lock before it reads the version, or one of them is refused.
        for _ in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("index.db");
            v3_index(&path);
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(|| {
                        start.wait();
                        let db = Database::open(&path).unwrap();
                        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
                    });
                }
            });
        }
    }

    #[test]
    fn a_v3_index_gains_parents_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v3_index(&path);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        // Existing tasks are top level, and still list.
        assert_eq!(db.get_task("w").unwrap().unwrap().parent_id, None);
        assert_eq!(db.list_tasks(None).unwrap().len(), 1);
        assert!(has_table(&db, "task_briefs"));
        assert!(has_table(&db, "task_reports"));
        // Not rebuilt, so the one-assistant index is still there.
        assert!(db
            .conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('b', 'assistant-2', 'x', 'assistant', 1, 1);"
            )
            .is_err());

        // A child's reports go with it; a parent with a child cannot go.
        db.conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, parent_id, created_at, last_activity)
                     VALUES ('c', 'child', 'child', 'a', 1, 1);
                 INSERT INTO task_reports (task_id, to_id, kind, body, at)
                     VALUES ('c', 'a', 'done', 'ok', 1);",
            )
            .unwrap();
        assert!(db.delete_task("a").is_err());
        db.delete_task("c").unwrap();
        let reports: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM task_reports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reports, 0);
        db.delete_task("a").unwrap();
    }

    #[test]
    fn a_v4_index_runs_every_task_locally_and_knows_no_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v3_index(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[3]).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 4;
                 INSERT INTO tasks (id, slug, name, parent_id, created_at, last_activity)
                     VALUES ('c', 'child', 'child', 'a', 1, 1);",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        for id in ["a", "w", "c"] {
            let task = db.get_task(id).unwrap().unwrap();
            assert_eq!(task.placement, crate::placement::Placement::Local, "{id}");
        }
        let anchors: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM task_anchors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(anchors, 0);
        for unknown in ["moon", "cloud", "local-sandbox"] {
            let sql = format!("UPDATE tasks SET placement = '{unknown}' WHERE id = 'c';");
            assert!(db.conn().execute_batch(&sql).is_err(), "{unknown}");
        }
        db.conn()
            .execute_batch("UPDATE tasks SET placement = 'local' WHERE id = 'c';")
            .unwrap();
    }

    #[test]
    fn a_v6_index_makes_work_holding_a_pr_checkout_a_review_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(BASELINE).unwrap();
            for sql in &MIGRATIONS[..6] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "PRAGMA user_version = 6;
                 INSERT INTO repos (id, trunk_path, remote_url, default_branch)
                     VALUES ('github.com/acme/api', '/t', 'u', 'main');
                 INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                     VALUES ('r', 'review-api-7', 'review-api-7', 'work', 1, 1);
                 INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                     VALUES ('w', 'feature', 'feature', 'work', 1, 1);
                 INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                     VALUES ('m', 'boss', 'boss', 'manager', 1, 1);
                 INSERT INTO task_worktrees
                     (task_id, repo_id, dir_name, branch, pr_number, created_at)
                     VALUES ('r', 'github.com/acme/api', 'api', 'pr/7', 7, 1),
                            ('w', 'github.com/acme/api', 'api', 'feat', NULL, 1);
                 INSERT INTO task_prs (task_id, repo_id, number, title, url, state, updated_at)
                     VALUES ('w', 'github.com/acme/api', 9, 'x', 'u', 'OPEN', 'z');",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let kind = |id: &str| db.get_task(id).unwrap().unwrap().kind;
        // v7 made it a review task, and v8 a review: no longer a task.
        assert!(db.get_task("r").unwrap().is_none());
        let review = db.get_review("r").unwrap().unwrap();
        assert_eq!(
            (review.pr_number, review.dir_name.as_deref()),
            (7, Some("api"))
        );
        assert_eq!(review.branch.as_deref(), Some("pr/7"));
        // A task with PRs of its own, but no PR checked out, is no review.
        assert_eq!(kind("w"), tasks::TaskKind::Work);
        assert_eq!(kind("m"), tasks::TaskKind::Manager);
    }

    #[test]
    fn a_v5_index_gains_managers_and_keeps_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v3_index(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[3]).unwrap();
            conn.execute_batch(MIGRATIONS[4]).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 5;
                 PRAGMA foreign_keys = ON;
                 INSERT INTO repos (id, trunk_path, remote_url, default_branch)
                     VALUES ('github.com/acme/api', '/t', 'u', 'main');
                 INSERT INTO tasks (id, slug, name, parent_id, created_at, last_activity)
                     VALUES ('c', 'child', 'child', 'a', 1, 1);
                 INSERT INTO tasks (id, slug, name, kind, parent_id, created_at, last_activity)
                     VALUES ('r', 'review', 'review', 'review', 'a', 1, 1);
                 INSERT INTO task_worktrees (task_id, repo_id, dir_name, branch, created_at)
                     VALUES ('c', 'github.com/acme/api', 'api', 'feat', 1);
                 INSERT INTO task_worktrees (task_id, repo_id, dir_name, branch, pr_number, created_at)
                     VALUES ('r', 'github.com/acme/api', 'pr', 'pr/9', 9, 1);
                 INSERT INTO task_sessions (session_id, task_id, name, created_at, last_activity)
                     VALUES ('s1', 'c', 'child', 1, 1);
                 INSERT INTO task_prs (task_id, repo_id, number, title, url, state, updated_at)
                     VALUES ('c', 'github.com/acme/api', 7, 'x', 'u', 'OPEN', 'z');
                 INSERT INTO task_reports (task_id, to_id, kind, body, at)
                     VALUES ('c', 'a', 'done', 'ok', 1);
                 INSERT INTO task_briefs (task_id, goal, created_by)
                     VALUES ('c', 'fix it', 'a');
                 INSERT INTO task_anchors (pid, start, task_id, kind, created_at)
                     VALUES (10, 20, 'c', 'pane', 1);",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let count = |table: &str| -> i64 {
            db.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        for (table, n) in [
            ("tasks", 3),
            ("task_worktrees", 1),
            ("task_sessions", 1),
            ("task_prs", 1),
            ("task_reports", 1),
            ("task_briefs", 1),
            ("task_anchors", 1),
            ("task_budgets", 0),
            ("task_start_queue", 0),
        ] {
            assert_eq!(count(table), n, "{table}");
        }
        for (id, parent) in [("a", None), ("w", None), ("c", Some("a"))] {
            let task = db.get_task(id).unwrap().unwrap();
            assert_eq!(task.parent_id.as_deref(), parent, "{id}");
            assert_eq!(task.placement, crate::placement::Placement::Local, "{id}");
            assert_eq!(task.run_state, tasks::RunState::Run, "{id}");
            assert_eq!(task.run_state_by, None, "{id}");
            assert_eq!(task.max_children, None, "{id}");
        }
        assert!(!db.list_task_worktrees("c").unwrap()[0].read_only);
        // Exactly the parented tasks that could add worktrees keep that.
        for (id, grants) in [("a", vec![]), ("w", vec![]), ("c", vec!["worktree.add"])] {
            assert_eq!(db.grants(id).unwrap(), grants, "{id}");
        }
        // The review task under the assistant is a review it asked for,
        // and holds no grant any more.
        assert!(db.get_task("r").unwrap().is_none());
        assert!(db.grants("r").unwrap().is_empty());
        let review = db.get_review("r").unwrap().unwrap();
        assert_eq!(review.requested_by.as_deref(), Some("a"));

        for bad in [
            "INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('b', 'assistant-2', 'x', 'assistant', 1, 1);",
            "INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('b', 'bogus', 'x', 'bogus', 1, 1);",
            "UPDATE tasks SET placement = 'cloud' WHERE id = 'c';",
            "UPDATE tasks SET placement = 'moon' WHERE id = 'c';",
            "UPDATE tasks SET run_state = 'paused' WHERE id = 'c';",
            "UPDATE tasks SET run_state_by = 'user' WHERE id = 'c';",
            "UPDATE tasks SET max_children = 0 WHERE id = 'c';",
        ] {
            assert!(db.conn().execute_batch(bad).is_err(), "{bad}");
        }
        db.conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, kind, parent_id, created_at, last_activity)
                     VALUES ('m', 'manager', 'm', 'manager', 'a', 1, 1);
                 UPDATE tasks SET run_state = 'paused', run_state_by = 'user' WHERE id = 'm';",
            )
            .unwrap();

        let violations: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        // A parent with a child cannot go; a child takes what is its own.
        assert!(db.delete_task("a").is_err());
        db.set_budget_max("c", Some(60), None).unwrap();
        db.charge("c", 5, 1).unwrap();
        db.delete_task("c").unwrap();
        assert!(db.budget("c").unwrap().is_none());
        // What it used stays with its parent.
        let kept = db.budget("a").unwrap().unwrap();
        assert_eq!((kept.used_secs, kept.used_turns), (5, 1));
        assert_eq!(count("task_budgets"), 1);
        for table in [
            "task_worktrees",
            "task_sessions",
            "task_prs",
            "task_reports",
            "task_briefs",
            "task_anchors",
        ] {
            assert_eq!(count(table), 0, "{table}");
        }
        assert!(db.grants("c").unwrap().is_empty());
    }

    /// An index as v7 left it, with work that is not a review (assistant
    /// `a`, manager `m`, worker `w`, with a worktree, session, budget, PR,
    /// report and anchor) and every review task shape v8 meets:
    ///
    /// - `ra`: PR 7, under the assistant, two sessions, a budget spent to
    ///   the pause, queued, a report and an anchor;
    /// - `rb0` (archived, the most recent), `rb1` and `rb2`: three review
    ///   tasks of PR 8, of which an active one is kept over the archived;
    /// - `rc`: PR 9, archived, its worktree kept at archive;
    /// - `rd` and `re`: PRs 10 and 11, top-level and under the manager;
    ///   `re` has a worktree that names no PR, which becomes its own, and
    ///   `rd` has a worker `wk` under it;
    /// - `rx`: a worktree and no PR at all.
    fn v7_index(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(BASELINE).unwrap();
        for sql in &MIGRATIONS[..7] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "PRAGMA user_version = 7;
             INSERT INTO repos (id, trunk_path, remote_url, default_branch)
                 VALUES ('github.com/acme/api', '/t', 'u', 'main');
             INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('a', 'assistant', 'assistant', 'assistant', 1, 1);
             INSERT INTO tasks (id, slug, name, kind, parent_id, created_at, last_activity)
                 VALUES ('m', 'boss', 'boss', 'manager', 'a', 1, 1),
                        ('w', 'work', 'work', 'work', 'm', 1, 1);
             INSERT INTO tasks (id, slug, name, kind, parent_id, state, created_at, last_activity)
                 VALUES ('ra', 'review-api-7', 'review-api-7', 'review', 'a', 'active', 5, 50),
                        ('rb0', 'review-api-8-3', 'review-api-8', 'review', NULL, 'archived', 1, 99),
                        ('rb1', 'review-api-8', 'review-api-8', 'review', NULL, 'active', 2, 10),
                        ('rb2', 'review-api-8-2', 'review-api-8', 'review', NULL, 'active', 3, 20),
                        ('rc', 'review-api-9', 'review-api-9', 'review', NULL, 'archived', 4, 30),
                        ('rd', 'review-api-10', 'review-api-10', 'review', NULL, 'active', 6, 60),
                        ('re', 'review-api-11', 'review-api-11', 'review', 'm', 'active', 7, 70),
                        ('rx', 'review-nothing', 'review-nothing', 'review', NULL, 'active', 8, 80);
             INSERT INTO tasks (id, slug, name, kind, parent_id, created_at, last_activity)
                 VALUES ('wk', 'under-review', 'under-review', 'work', 'rd', 1, 1);
             UPDATE tasks SET run_state = 'paused', run_state_by = 'budget' WHERE id = 'ra';
             INSERT INTO task_worktrees
                 (task_id, repo_id, dir_name, branch, created_branch, head_oid, stash_ref,
                  pr_number, created_at)
                 VALUES ('w', 'github.com/acme/api', 'api', 'feat', 1, NULL, NULL, NULL, 1),
                        ('ra', 'github.com/acme/api', 'api', 'pr/7', 1, NULL, NULL, 7, 6),
                        ('rb0', 'github.com/acme/api', 'api', 'pr/8', 1, 'h0', 's0', 8, 1),
                        ('rb1', 'github.com/acme/api', 'api', 'pr/8', 0, NULL, NULL, 8, 2),
                        ('rc', 'github.com/acme/api', 'api', 'pr/9', 1, 'h9', 's9', 9, 4),
                        ('rd', 'github.com/acme/api', 'api', 'pr/10', 0, NULL, NULL, 10, 6),
                        ('re', 'github.com/acme/api', 'e', NULL, 0, NULL, NULL, NULL, 7),
                        ('rx', 'github.com/acme/api', 'x', NULL, 0, NULL, NULL, NULL, 8);
             INSERT INTO task_prs (task_id, repo_id, number, title, url, state, updated_at)
                 VALUES ('w', 'github.com/acme/api', 3, 'mine', 'u3', 'OPEN', 'z'),
                        ('ra', 'github.com/acme/api', 7, 'Add x', 'u7', 'OPEN', 'z'),
                        ('rb2', 'github.com/acme/api', 8, 'Fix y', 'u8', 'MERGED', 'z'),
                        ('re', 'github.com/acme/api', 11, 'Sync', 'u11', 'OPEN', 'z');
             INSERT INTO task_sessions
                 (session_id, task_id, name, created_at, last_activity, is_primary)
                 VALUES ('ws', 'w', 'work', 1, 1, 0),
                        ('s-old', 'ra', 'review-api-7', 1, 9, 1),
                        ('s-new', 'ra', 'review-api-7', 2, 99, 0),
                        ('s-b2', 'rb2', 'review-api-8-2', 3, 20, 0);
             INSERT INTO task_budgets (task_id, max_secs, max_turns, used_secs, used_turns, warned)
                 VALUES ('w', 600, NULL, 5, 1, 0),
                        ('ra', 3600, 20, 100, 3, 80);
             INSERT INTO task_start_queue (task_id, queued_at) VALUES ('ra', 55);
             INSERT INTO task_grants (task_id, capability)
                 VALUES ('w', 'worktree.add'), ('ra', 'worktree.add');
             INSERT INTO task_briefs (task_id, goal, created_by)
                 VALUES ('w', 'do it', 'm'), ('ra', 'review it', 'a');
             INSERT INTO task_reports (task_id, to_id, kind, body, at)
                 VALUES ('w', 'm', 'done', 'shipped', 1), ('ra', 'a', 'done', 'reviewed', 2),
                        ('rb1', 'a', 'done', 'superseded', 3), ('w', 'rd', 'question', 'to a review', 4);
             INSERT INTO task_anchors (pid, start, task_id, kind, pane_id, created_at)
                 VALUES (10, 20, 'w', 'pane', '%1', 1),
                        (11, 20, 'ra', 'pane', '%2', 1),
                        (12, 20, 'rb1', 'pane', '%3', 1),
                        (13, 20, 'rx', 'job', NULL, 1);",
        )
        .unwrap();
    }

    fn scalar(db: &Database, sql: &str) -> i64 {
        db.conn().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_v7_index_moves_its_review_tasks_to_reviews_after_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v7_index(&path);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);

        // Backed up as it was, before anything ran.
        let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert!(backups[0].starts_with("index-v7-"), "{backups:?}");
        let backup = Connection::open(dir.path().join("backups").join(&backups[0])).unwrap();
        let (version, review_tasks): (i64, i64) = (
            backup
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            backup
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE kind = 'review'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        assert_eq!((version, review_tasks), (7, 8));

        // No review task is left, and the other tasks are as they were.
        let ids: Vec<String> = db
            .list_tasks(None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        for id in ["m", "w", "wk"] {
            assert!(ids.contains(&id.to_string()), "{ids:?}");
        }
        assert!(db.get_assistant_task().unwrap().is_some());
        assert_eq!(
            db.get_task("w").unwrap().unwrap().parent_id.as_deref(),
            Some("m")
        );
        assert_eq!(db.get_task("wk").unwrap().unwrap().parent_id, None);
        assert_eq!(db.list_task_worktrees("w").unwrap().len(), 1);
        assert_eq!(db.list_task_prs("w").unwrap().len(), 1);
        assert_eq!(db.list_task_sessions("w").unwrap().len(), 1);
        assert_eq!(db.budget("w").unwrap().unwrap().max_secs, Some(600));
        assert_eq!(db.grants("w").unwrap(), ["worktree.add"]);
        assert!(db.get_brief("w").unwrap().is_some());
        assert_eq!(db.reports_for("w", 10).unwrap().len(), 1);
        for table in [
            "task_worktrees",
            "task_sessions",
            "task_prs",
            "task_budgets",
            "task_grants",
            "task_briefs",
            "task_start_queue",
        ] {
            let foreign = scalar(
                &db,
                &format!(
                    "SELECT COUNT(*) FROM {table}
                     WHERE task_id NOT IN (SELECT id FROM tasks)"
                ),
            );
            assert_eq!(foreign, 0, "{table} keeps rows of a review task");
        }

        // One review per PR: the newest task of each.
        let mut reviewed: Vec<(String, u64)> = db
            .list_reviews(None)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.pr_number))
            .collect();
        reviewed.sort();
        let want: Vec<(String, u64)> = [("ra", 7), ("rb2", 8), ("rc", 9), ("rd", 10), ("re", 11)]
            .map(|(id, n)| (id.to_string(), n))
            .to_vec();
        assert_eq!(reviewed, want);

        // The one with everything: the id and slug, the PR's history, the
        // worktree, the primary session, the budget, the pause and the queue.
        let ra = db.find_review("github.com/acme/api", 7).unwrap().unwrap();
        assert_eq!((ra.id.as_str(), ra.slug.as_str()), ("ra", "review-api-7"));
        assert_eq!(ra.requested_by.as_deref(), Some("a"));
        assert_eq!(
            (
                ra.title.as_str(),
                ra.pr_state.as_deref(),
                ra.pr_url.as_deref()
            ),
            ("Add x", Some("OPEN"), Some("u7"))
        );
        assert_eq!(ra.state, tasks::TaskState::Active);
        assert_eq!((ra.created_at, ra.last_activity), (5, 50));
        assert_eq!(ra.dir_name.as_deref(), Some("api"));
        assert_eq!(ra.branch.as_deref(), Some("pr/7"));
        assert!(ra.created_branch);
        // The one the user marked primary, not the most recent.
        assert_eq!(ra.session_id.as_deref(), Some("s-old"));
        assert_eq!(
            (
                ra.max_secs,
                ra.max_turns,
                ra.used_secs,
                ra.used_turns,
                ra.warned
            ),
            (Some(3600), Some(20), 100, 3, 80)
        );
        assert!(ra.paused);
        assert_eq!(ra.queued_at, Some(55));
        assert_eq!(
            (ra.asked_head, ra.asked_at, ra.brought_back_at),
            (None, None, None)
        );

        // The newest of PR 8 has no worktree row of its own; its title is
        // the PR's, and nobody asked for it.
        let rb2 = db.get_review("rb2").unwrap().unwrap();
        assert_eq!(rb2.requested_by, None);
        assert_eq!(
            (rb2.title.as_str(), rb2.pr_state.as_deref()),
            ("Fix y", Some("MERGED"))
        );
        assert_eq!(rb2.dir_name, None);
        assert_eq!(rb2.session_id.as_deref(), Some("s-b2"));
        assert!(!rb2.paused && rb2.queued_at.is_none());

        // An archived review keeps what restoring it needs.
        let rc = db.get_review("rc").unwrap().unwrap();
        assert_eq!(rc.state, tasks::TaskState::Archived);
        assert_eq!(
            (
                rc.head_oid.as_deref(),
                rc.stash_ref.as_deref(),
                rc.branch.as_deref()
            ),
            (Some("h9"), Some("s9"), Some("pr/9"))
        );
        // Only the assistant's reviews are asked for; a manager's are not.
        assert_eq!(db.get_review("rd").unwrap().unwrap().requested_by, None);
        let re = db.get_review("re").unwrap().unwrap();
        assert_eq!(re.requested_by, None);
        // A worktree that names no PR is its review's own, not a leftover.
        assert_eq!(re.dir_name.as_deref(), Some("e"));

        // The duplicates' worktrees, and the PR-less task's, wait to be
        // archived; an archived one's work is in refs already.
        let pending: Vec<(String, String)> = db
            .pending_leftovers()
            .unwrap()
            .into_iter()
            .map(|l| (l.task_id, l.dir_name))
            .collect();
        assert_eq!(
            pending,
            [
                ("rb1".to_string(), "api".to_string()),
                ("rx".to_string(), "x".to_string())
            ]
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM review_leftovers
                 WHERE task_id = 'rb0' AND was_archived = 1 AND done = 1"
            ),
            1
        );
        assert_eq!(db.meta("reviews_v8_pending").unwrap().as_deref(), Some("1"));
        assert_eq!(
            db.meta("reviews_v8_dropped").unwrap().as_deref(),
            Some("review-nothing")
        );
        let superseded = db.meta("reviews_v8_superseded").unwrap().unwrap();
        assert!(superseded.contains("review-api-8-3") && superseded.contains("review-api-8"));
        let kept = db.meta("reviews_v8_kept").unwrap().unwrap();
        assert!(
            kept.contains("review-api-7 github.com/acme/api#7"),
            "{kept}"
        );

        // The panes that were the reviews' are theirs still; a duplicate's
        // and a dropped task's are gone; the worker's is untouched.
        let anchors: Vec<String> = db
            .all_anchors()
            .unwrap()
            .into_iter()
            .map(|a| a.task_id)
            .collect();
        assert_eq!(anchors.len(), 2, "{anchors:?}");
        assert!(anchors.contains(&"w".to_string()) && anchors.contains(&"ra".to_string()));
        // A kept review's unanswered report stays; a superseded task's and
        // one sent to a review task went; the worker's stay.
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM task_reports"), 2);
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM task_reports
                 WHERE task_id = 'ra' AND to_id = 'a' AND answer IS NULL"
            ),
            1
        );

        // Foreign keys are on, whole, and no task takes a review's name.
        assert_eq!(
            scalar(&db, "SELECT COUNT(*) FROM pragma_foreign_key_check"),
            0
        );
        assert!(db
            .insert_task(&tasks::TaskRecord {
                id: "n".into(),
                slug: "review-api-9".into(),
                name: "n".into(),
                project: None,
                kind: tasks::TaskKind::Work,
                state: tasks::TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: crate::placement::Placement::Local,
                run_state: tasks::RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .is_err());
        // A review is no task: `reviews` rows accept no kind of task.
        assert!(db
            .conn()
            .execute_batch(
                "INSERT INTO tasks (id, slug, name, kind, created_at, last_activity)
                 VALUES ('t2', 'rv2', 'x', 'review', 1, 1);"
            )
            .is_err());

        // Opening again changes nothing and backs up nothing.
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.list_reviews(None).unwrap().len(), 5);
        assert_eq!(
            std::fs::read_dir(dir.path().join("backups"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_fresh_index_is_not_backed_up_and_an_empty_one_has_no_pending_work() {
        let (dir, db) = open();
        assert!(!dir.path().join("backups").exists());
        assert_eq!(db.meta("reviews_v8_pending").unwrap(), None);
        assert!(db.pending_leftovers().unwrap().is_empty());
    }

    #[test]
    fn a_pre_versioning_index_with_repos_is_backed_up_and_a_backup_is_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(LEGACY_V0)
            .unwrap();
        let backups = dir.path().join("backups");
        // One by the name this migration would pick, from the same second.
        std::fs::create_dir_all(&backups).unwrap();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let taken: Vec<_> = (0..3)
            .map(|d| backups.join(format!("index-v0-{}.db", at + d)))
            .collect();
        for t in &taken {
            std::fs::write(t, "someone else's").unwrap();
        }
        Database::open(&path).unwrap();
        let names: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        for t in &taken {
            assert_eq!(std::fs::read_to_string(t).unwrap(), "someone else's");
        }
    }

    #[test]
    fn a_row_dangling_before_v8_does_not_stop_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v7_index(&path);
        // A session of a task that is not there, left by an older jeet:
        // not v8's doing, so not v8's to refuse over.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO task_sessions
                     (session_id, task_id, name, created_at, last_activity, is_primary)
                     VALUES ('ghost', 'nobody', 'x', 1, 1, 0);",
            )
            .unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let conn = db.conn();
        assert_eq!(
            dangling(&conn).unwrap(),
            [("task_sessions -> tasks".to_string(), 1)]
        );
    }

    #[test]
    fn an_index_that_cannot_be_backed_up_is_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        v7_index(&path);
        // A file where the backups directory goes.
        std::fs::write(dir.path().join("backups"), "in the way").unwrap();
        let err = Database::open(&path).err().expect("refused");
        assert!(format!("{err:#}").contains("not migrating"), "{err:#}");
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 7);
    }

    #[test]
    fn an_index_stamped_by_the_workspace_builds_is_migrated_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        // What v0.3 left behind: its own tables, and user_version 2 by its
        // own count — higher than ours, but no tasks table.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repos (id TEXT PRIMARY KEY, trunk_path TEXT NOT NULL,
                     remote_url TEXT NOT NULL, default_branch TEXT NOT NULL,
                     managed INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE projects (id TEXT PRIMARY KEY, workspace TEXT NOT NULL);
                 INSERT INTO repos VALUES ('github.com/acme/api', '/t', 'u', 'main', 1);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(db.list_repos(None).unwrap().len(), 1);
    }

    #[test]
    fn an_index_from_a_newer_jeet_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE tasks (id TEXT); PRAGMA user_version = 999;")
                .unwrap();
        }
        let err = Database::open(&path).err().expect("refused");
        assert!(err.to_string().contains("v999"), "{err}");
    }
}
