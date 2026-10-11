use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};

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
    // questions escalated to the user, not read yet.
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
    // v5: where a task's agent runs (`placement`; every existing task ran
    // in local tmux), and the processes jeet launched for a task, keyed by
    // pid and start time so a reused pid never matches. Additive only.
    "
    ALTER TABLE tasks ADD COLUMN placement TEXT NOT NULL DEFAULT 'local'
        CHECK (placement IN ('local', 'cloud', 'local-sandbox'));
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
        apply_migrations(&mut conn, current)?;
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
        tx.execute_batch(sql)
            .with_context(|| format!("migrate index to v{version}"))?;
        // PRAGMA takes no bound parameters; `version` is our own integer.
        tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
        tx.commit()?;
    }
    Ok(())
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
                     VALUES ('t1', 'fix', 'Fix', 'review', 1, 1);
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
        assert_eq!(task.kind, tasks::TaskKind::Review);
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
        assert_eq!(db.schema_version().unwrap(), 5);
        for id in ["a", "w", "c"] {
            let task = db.get_task(id).unwrap().unwrap();
            assert_eq!(task.placement, crate::placement::Placement::Local, "{id}");
        }
        let anchors: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM task_anchors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(anchors, 0);
        assert!(db
            .conn()
            .execute_batch("UPDATE tasks SET placement = 'moon' WHERE id = 'w';")
            .is_err());
        db.conn()
            .execute_batch("UPDATE tasks SET placement = 'cloud' WHERE id = 'c';")
            .unwrap();
        assert_eq!(
            db.get_task("c").unwrap().unwrap().placement,
            crate::placement::Placement::Cloud
        );
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
