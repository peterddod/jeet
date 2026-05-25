use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct RepoRecord {
    pub id: String,
    pub trunk_path: String,
    pub remote_url: String,
    pub default_branch: String,
    pub managed: bool,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,             // UUID
    pub name: String,           // User-defined session name
    pub repo_id: String,
    pub branch: Option<String>,
    pub trunk_path: String,     // Path to workspace copy
    pub created_at: i64,
    pub status: String,         // running|stopped|error
}

#[derive(Debug, Clone)]
pub struct WorktreeRecord {
    pub repo_id: String,
    pub branch: String,
    pub path: String,
    pub created_at: i64,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create index db parent")?;
        }
        let conn = Connection::open(path).context("open index db")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                repo_id TEXT NOT NULL REFERENCES repos(id),
                branch TEXT,
                trunk_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                status TEXT DEFAULT 'running'
            );
            ",
        )?;
        Ok(())
    }

    pub fn upsert_repo(&self, repo: &RepoRecord) -> Result<()> {
        self.conn.execute(
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
        self.conn.execute("DELETE FROM worktrees WHERE repo_id = ?1", params![repo_id])?;
        Ok(())
    }

    pub fn upsert_worktree(&self, wt: &WorktreeRecord) -> Result<()> {
        self.conn.execute(
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
        self.conn.execute(
            "DELETE FROM worktrees WHERE repo_id = ?1 AND branch = ?2",
            params![repo_id, branch],
        )?;
        Ok(())
    }

    pub fn list_repos(&self, filter: Option<&str>) -> Result<Vec<RepoRecord>> {
        let mut stmt = self.conn.prepare(
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
        let mut stmt = self.conn.prepare(
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
        let mut stmt = self.conn.prepare(
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
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM worktrees WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn get_worktree(&self, repo_id: &str, branch: &str) -> Result<Option<WorktreeRecord>> {
        let mut stmt = self.conn.prepare(
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

    pub fn upsert_session(&self, session: &SessionRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, name, repo_id, branch, trunk_path, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
               repo_id = excluded.repo_id,
               branch = excluded.branch,
               trunk_path = excluded.trunk_path,
               created_at = excluded.created_at,
               status = excluded.status",
            params![
                session.id,
                session.name,
                session.repo_id,
                session.branch.as_deref(),
                session.trunk_path,
                session.created_at,
                session.status,
            ],
        )?;
        Ok(())
    }

    pub fn get_session_by_name(&self, name: &str) -> Result<Option<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, repo_id, branch, trunk_path, created_at, status FROM sessions WHERE name = ?1",
        )?;
        let mut rows = stmt.query(params![name])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(SessionRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                repo_id: row.get(2)?,
                branch: row.get::<_, Option<String>>(3)?,
                trunk_path: row.get(4)?,
                created_at: row.get(5)?,
                status: row.get(6)?,
            }));
        }
        Ok(None)
    }

    pub fn list_sessions(&self, filter: Option<&str>) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, repo_id, branch, trunk_path, created_at, status FROM sessions ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                repo_id: row.get(2)?,
                branch: row.get::<_, Option<String>>(3)?,
                trunk_path: row.get(4)?,
                created_at: row.get(5)?,
                status: row.get(6)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        if let Some(f) = filter {
            let f = f.to_lowercase();
            sessions.retain(|s| {
                s.name.to_lowercase().contains(&f) || s.repo_id.to_lowercase().contains(&f)
            });
        }
        Ok(sessions)
    }

    pub fn delete_session(&self, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM sessions WHERE name = ?1", params![name])?;
        Ok(())
    }
}
