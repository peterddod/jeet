//! The processes jeet launched for a task (schema v5, `task_anchors`): a
//! task's agent pane, or a review job's runner, keyed by pid and start time
//! so a reused pid never matches. Written only by the jeet that launched the
//! process, and never re-bound; see [`crate::identity`].

use anyhow::Result;
use rusqlite::params;

use super::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// The process tmux started in a task's pane.
    Pane,
    /// A review job's runner.
    Job,
}

impl AnchorKind {
    fn as_str(self) -> &'static str {
        match self {
            AnchorKind::Pane => "pane",
            AnchorKind::Job => "job",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "pane" => Ok(AnchorKind::Pane),
            "job" => Ok(AnchorKind::Job),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown anchor kind {other:?}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub pid: u32,
    /// When the process started, as [`crate::identity`] reads it.
    pub start: i64,
    pub task_id: String,
    pub kind: AnchorKind,
    /// When the row was written, in seconds.
    pub created_at: i64,
}

const ANCHOR_COLUMNS: &str = "pid, start, task_id, kind, created_at";

fn anchor_from_row(row: &rusqlite::Row) -> rusqlite::Result<Anchor> {
    Ok(Anchor {
        pid: row.get(0)?,
        start: row.get(1)?,
        task_id: row.get(2)?,
        kind: AnchorKind::parse(&row.get::<_, String>(3)?)?,
        created_at: row.get(4)?,
    })
}

impl Database {
    /// Record `anchor`, the process started in tmux pane `pane_id` for a
    /// pane. A plain insert: a row already there for the same process is an
    /// error, never re-bound to another task.
    pub fn insert_anchor(&self, anchor: &Anchor, pane_id: Option<&str>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO task_anchors (pid, start, task_id, kind, pane_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                anchor.pid,
                anchor.start,
                anchor.task_id,
                anchor.kind.as_str(),
                pane_id,
                anchor.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn all_anchors(&self) -> Result<Vec<Anchor>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {ANCHOR_COLUMNS} FROM task_anchors"))?;
        let rows = stmt.query_map([], anchor_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `task_id`'s anchors of `kind`.
    pub fn task_anchors(&self, task_id: &str, kind: AnchorKind) -> Result<Vec<Anchor>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ANCHOR_COLUMNS} FROM task_anchors WHERE task_id = ?1 AND kind = ?2"
        ))?;
        let rows = stmt.query_map(params![task_id, kind.as_str()], anchor_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_anchor(&self, pid: u32, start: i64) -> Result<()> {
        self.conn().execute(
            "DELETE FROM task_anchors WHERE pid = ?1 AND start = ?2",
            params![pid, start],
        )?;
        Ok(())
    }

    /// Forget `task_id`'s anchors of `kind`: an agent that outlives its task
    /// being archived is no longer that task's.
    pub fn delete_task_anchors(&self, task_id: &str, kind: AnchorKind) -> Result<()> {
        self.conn().execute(
            "DELETE FROM task_anchors WHERE task_id = ?1 AND kind = ?2",
            params![task_id, kind.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{TaskKind, TaskRecord, TaskState};
    use crate::placement::Placement;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        for id in ["a", "b"] {
            db.insert_task(&TaskRecord {
                id: id.into(),
                slug: id.into(),
                name: id.into(),
                project: None,
                kind: TaskKind::Work,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: Placement::Local,
            })
            .unwrap();
        }
        (dir, db)
    }

    fn anchor(pid: u32, start: i64, task: &str, kind: AnchorKind, at: i64) -> Anchor {
        Anchor {
            pid,
            start,
            task_id: task.into(),
            kind,
            created_at: at,
        }
    }

    #[test]
    fn a_process_is_anchored_once() {
        let (_dir, db) = open();
        let pane = anchor(10, 5, "a", AnchorKind::Pane, 100);
        db.insert_anchor(&pane, Some("%1")).unwrap();
        db.insert_anchor(&anchor(11, 6, "b", AnchorKind::Job, 50), None)
            .unwrap();
        // The same process again, even for another task, is refused.
        let again = anchor(10, 5, "b", AnchorKind::Pane, 200);
        assert!(db.insert_anchor(&again, None).is_err());
        // The same pid started at another time is another process.
        db.insert_anchor(&anchor(10, 7, "b", AnchorKind::Pane, 200), None)
            .unwrap();

        assert_eq!(db.all_anchors().unwrap().len(), 3);
        assert_eq!(db.task_anchors("b", AnchorKind::Job).unwrap().len(), 1);
        db.delete_task_anchors("b", AnchorKind::Pane).unwrap();
        assert_eq!(db.task_anchors("b", AnchorKind::Pane).unwrap(), vec![]);
        assert_eq!(db.task_anchors("b", AnchorKind::Job).unwrap().len(), 1);
        db.delete_anchor(11, 6).unwrap();
        assert_eq!(db.all_anchors().unwrap(), vec![pane]);
    }

    #[test]
    fn deleting_a_task_takes_its_anchors() {
        let (_dir, db) = open();
        db.insert_anchor(&anchor(10, 5, "a", AnchorKind::Pane, 1), None)
            .unwrap();
        db.insert_anchor(&anchor(11, 5, "b", AnchorKind::Pane, 1), None)
            .unwrap();
        db.delete_task("a").unwrap();
        let left = db.all_anchors().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].task_id, "b");
    }
}
