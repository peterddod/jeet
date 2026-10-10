use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

pub mod tasks;

/// The schema as it stood before versioning: v0. Idempotent, so it runs on
/// every open and a fresh index and a legacy one start from the same place.
const BASELINE: &str = "
    CREATE TABLE IF NOT EXISTS repos (
        id TEXT PRIMARY KEY,
        trunk_path TEXT NOT NULL,
        remote_url TEXT NOT NULL,
        default_branch TEXT NOT NULL,
        managed INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS worktrees (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        branch TEXT NOT NULL,
        path TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE(repo_id, branch)
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
];

#[derive(Debug, Clone)]
pub struct RepoRecord {
    pub id: String,
    pub trunk_path: String,
    pub remote_url: String,
    pub default_branch: String,
    pub managed: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeRecord {
    pub repo_id: String,
    pub branch: String,
    pub path: String,
    pub created_at: i64,
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
    /// The baseline (`repos` + `worktrees`) predates versioning, so it is
    /// created idempotently and counts as v0; every change after it is a step
    /// in [`MIGRATIONS`], applied in order inside a transaction that also
    /// bumps `PRAGMA user_version`, so a crash part-way leaves the index at
    /// the last whole version rather than half-way through one.
    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn();
        // Off by default in SQLite, and the task tables lean on ON DELETE
        // CASCADE. Per connection, and a no-op inside a transaction, so first.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(BASELINE)?;
        let current = effective_version(&conn)?;
        let latest = MIGRATIONS.len() as i64;
        if current > latest {
            bail!(
                "the index is at schema v{current} but this jeet only knows up to v{latest}; \
                 upgrade jeet"
            );
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = i + 1;
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migrate index to v{version}"))?;
            // PRAGMA takes no bound parameters; `version` is our own integer.
            tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
            tx.commit()?;
        }
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
            "INSERT INTO repos (id, trunk_path, remote_url, default_branch, managed)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               trunk_path = excluded.trunk_path,
               remote_url = excluded.remote_url,
               default_branch = excluded.default_branch,
               managed = excluded.managed",
            params![
                repo.id,
                repo.trunk_path,
                repo.remote_url,
                repo.default_branch,
                i32::from(repo.managed),
            ],
        )?;
        Ok(())
    }

    pub fn delete_worktrees_for_repo(&self, repo_id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM worktrees WHERE repo_id = ?1", params![repo_id])?;
        Ok(())
    }

    pub fn upsert_worktree(&self, wt: &WorktreeRecord) -> Result<()> {
        self.conn().execute(
            "INSERT INTO worktrees (repo_id, branch, path, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repo_id, branch) DO UPDATE SET
               path = excluded.path,
               created_at = excluded.created_at",
            params![wt.repo_id, wt.branch, wt.path, wt.created_at],
        )?;
        Ok(())
    }

    pub fn remove_worktree(&self, repo_id: &str, branch: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM worktrees WHERE repo_id = ?1 AND branch = ?2",
            params![repo_id, branch],
        )?;
        Ok(())
    }

    pub fn list_repos(&self, filter: Option<&str>) -> Result<Vec<RepoRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, trunk_path, remote_url, default_branch, managed FROM repos ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RepoRecord {
                id: row.get(0)?,
                trunk_path: row.get(1)?,
                remote_url: row.get(2)?,
                default_branch: row.get(3)?,
                managed: row.get::<_, i32>(4)? != 0,
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
            "SELECT id, trunk_path, remote_url, default_branch, managed FROM repos WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(RepoRecord {
                id: row.get(0)?,
                trunk_path: row.get(1)?,
                remote_url: row.get(2)?,
                default_branch: row.get(3)?,
                managed: row.get::<_, i32>(4)? != 0,
            }));
        }
        Ok(None)
    }

    pub fn list_worktrees(&self, repo_id: &str) -> Result<Vec<WorktreeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT repo_id, branch, path, created_at FROM worktrees WHERE repo_id = ?1 ORDER BY branch",
        )?;
        let rows = stmt.query_map(params![repo_id], |row| {
            Ok(WorktreeRecord {
                repo_id: row.get(0)?,
                branch: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let mut wts = Vec::new();
        for row in rows {
            wts.push(row?);
        }
        Ok(wts)
    }

    pub fn worktree_count(&self, repo_id: &str) -> Result<usize> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM worktrees WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn get_worktree(&self, repo_id: &str, branch: &str) -> Result<Option<WorktreeRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT repo_id, branch, path, created_at FROM worktrees WHERE repo_id = ?1 AND branch = ?2",
        )?;
        let mut rows = stmt.query(params![repo_id, branch])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(WorktreeRecord {
                repo_id: row.get(0)?,
                branch: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
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
    fn a_legacy_index_is_migrated_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        // An index written before versioning: the v0 tables, user_version 0.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(BASELINE).unwrap();
            conn.execute(
                "INSERT INTO repos (id, trunk_path, remote_url, default_branch, managed)
                 VALUES ('github.com/acme/api', '/t', 'u', 'main', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO worktrees (repo_id, branch, path, created_at)
                 VALUES ('github.com/acme/api', 'feat', '/w', 1)",
                [],
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(db.list_repos(None).unwrap().len(), 1);
        assert_eq!(db.worktree_count("github.com/acme/api").unwrap(), 1);
        assert!(db.list_tasks(None).unwrap().is_empty());
        drop(db);
        // Opening again is a no-op, not a second attempt at v1.
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as i64);
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
