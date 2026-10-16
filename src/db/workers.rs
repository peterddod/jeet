//! Worker jobs (schema v10, `worker_jobs`): one row per headless `claude`
//! run `jeet worker run` makes. The row is the job's whole record: what ran,
//! who asked, its runner (pid and start, as [`crate::identity`] reads them),
//! and how it ended. The runner is anchored under the job's id with the role
//! `worker` ([`super::anchors::AnchorRole`]).

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use super::Database;

/// Where a worker job is. `Queued` is written by no one yet: a job takes its
/// pool slot before its row is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    Queued,
    Running,
    /// Ended, and its output passed the schema.
    Done,
    /// Ended, but its output did not pass the schema (or was no JSON).
    Invalid,
    /// `claude` failed, timed out, or the runner was lost.
    Failed,
    /// The runner was started by something other than `jeet worker run`.
    Refused,
}

impl WorkerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerStatus::Queued => "queued",
            WorkerStatus::Running => "running",
            WorkerStatus::Done => "done",
            WorkerStatus::Invalid => "invalid",
            WorkerStatus::Failed => "failed",
            WorkerStatus::Refused => "refused",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "queued" => Ok(WorkerStatus::Queued),
            "running" => Ok(WorkerStatus::Running),
            "done" => Ok(WorkerStatus::Done),
            "invalid" => Ok(WorkerStatus::Invalid),
            "failed" => Ok(WorkerStatus::Failed),
            "refused" => Ok(WorkerStatus::Refused),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown worker job status {other:?}").into(),
            )),
        }
    }

    /// Whether the job is over, whichever way.
    pub fn ended(self) -> bool {
        !matches!(self, WorkerStatus::Queued | WorkerStatus::Running)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerJobRecord {
    /// `wk-` and twelve hex digits.
    pub id: String,
    /// The named job, as in `$JEET_HOME/workers/jobs/<name>.md`.
    pub job: String,
    /// The task whose agent asked for it; none for the user.
    pub requested_by: Option<String>,
    /// The runner, and when it started as [`crate::identity`] reads it.
    pub pid: Option<u32>,
    pub start: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: WorkerStatus,
    /// The validated output, as JSON text.
    pub result: Option<String>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
}

impl WorkerJobRecord {
    /// A job just started, with no runner yet.
    pub fn new(id: String, job: &str, requested_by: Option<String>, now: i64) -> Self {
        Self {
            id,
            job: job.to_string(),
            requested_by,
            pid: None,
            start: None,
            started_at: now,
            finished_at: None,
            status: WorkerStatus::Running,
            result: None,
            error: None,
            cost_usd: None,
        }
    }
}

/// How a job ended, for [`Database::finish_worker_job`].
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerEnd {
    pub status: WorkerStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
}

const COLUMNS: &str = "id, job, requested_by, pid, start, started_at, finished_at, status, \
                       result, error, cost_usd";

fn from_row(row: &Row<'_>) -> rusqlite::Result<WorkerJobRecord> {
    Ok(WorkerJobRecord {
        id: row.get(0)?,
        job: row.get(1)?,
        requested_by: row.get(2)?,
        pid: row.get(3)?,
        start: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        status: WorkerStatus::parse(&row.get::<_, String>(7)?)?,
        result: row.get(8)?,
        error: row.get(9)?,
        cost_usd: row.get(10)?,
    })
}

impl Database {
    pub fn insert_worker_job(&self, job: &WorkerJobRecord) -> Result<()> {
        self.conn().execute(
            &format!(
                "INSERT INTO worker_jobs ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            params![
                job.id,
                job.job,
                job.requested_by,
                job.pid.map(i64::from),
                job.start,
                job.started_at,
                job.finished_at,
                job.status.as_str(),
                job.result,
                job.error,
                job.cost_usd,
            ],
        )?;
        Ok(())
    }

    pub fn get_worker_job(&self, id: &str) -> Result<Option<WorkerJobRecord>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM worker_jobs WHERE id = ?1"),
                params![id],
                from_row,
            )
            .optional()?)
    }

    /// The `limit` most recently started jobs, newest first.
    pub fn list_worker_jobs(&self, limit: usize) -> Result<Vec<WorkerJobRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM worker_jobs ORDER BY started_at DESC, rowid DESC LIMIT ?1"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![limit], from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The jobs not ended, oldest first.
    pub fn running_worker_jobs(&self) -> Result<Vec<WorkerJobRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM worker_jobs WHERE finished_at IS NULL
             ORDER BY started_at, rowid"
        ))?;
        let rows = stmt.query_map([], from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record the runner of job `id`: its pid and its start as
    /// [`crate::identity`] reads it. Only for a job not yet ended, and not
    /// another runner's (none recorded yet, or this pid): so two runners
    /// cannot both run one job. Whether it was recorded.
    pub fn set_worker_job_runner(&self, id: &str, pid: u32, start: Option<i64>) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE worker_jobs SET pid = ?2, start = ?3
             WHERE id = ?1 AND finished_at IS NULL AND (pid IS NULL OR pid = ?2)",
            params![id, i64::from(pid), start],
        )?;
        Ok(changed == 1)
    }

    /// Record that job `id` ended at `at` as `end` says. Only a job not yet
    /// ended and, given `runner`, one whose runner is that pid or not
    /// recorded yet: whether it was recorded.
    pub fn finish_worker_job(
        &self,
        id: &str,
        at: i64,
        end: &WorkerEnd,
        runner: Option<u32>,
    ) -> Result<bool> {
        if !end.status.ended() {
            bail!("a worker job ends as done, invalid, failed or refused");
        }
        let changed = self.conn().execute(
            "UPDATE worker_jobs
             SET finished_at = ?2, status = ?3, result = ?4, error = ?5, cost_usd = ?6
             WHERE id = ?1 AND finished_at IS NULL AND (?7 IS NULL OR pid IS NULL OR pid = ?7)",
            params![
                id,
                at,
                end.status.as_str(),
                end.result,
                end.error,
                end.cost_usd,
                runner.map(i64::from),
            ],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        (dir, db)
    }

    fn failed(error: &str) -> WorkerEnd {
        WorkerEnd {
            status: WorkerStatus::Failed,
            result: None,
            error: Some(error.into()),
            cost_usd: None,
        }
    }

    #[test]
    fn a_job_is_kept_and_found() {
        let (_dir, db) = open();
        let job = WorkerJobRecord::new("wk-1".into(), "pr-status", Some("t".into()), 5);
        db.insert_worker_job(&job).unwrap();
        assert!(db.insert_worker_job(&job).is_err());
        assert_eq!(db.get_worker_job("wk-1").unwrap(), Some(job));
        assert_eq!(db.get_worker_job("wk-2").unwrap(), None);
    }

    #[test]
    fn one_runner_has_a_job_and_it_ends_once() {
        let (_dir, db) = open();
        db.insert_worker_job(&WorkerJobRecord::new("wk-1".into(), "j", None, 5))
            .unwrap();
        assert!(db.set_worker_job_runner("wk-1", 42, Some(9)).unwrap());
        // Not another runner, but the same one again.
        assert!(!db.set_worker_job_runner("wk-1", 43, Some(10)).unwrap());
        assert!(db.set_worker_job_runner("wk-1", 42, Some(9)).unwrap());
        // Another runner's end is not recorded; its own is, once.
        assert!(!db
            .finish_worker_job("wk-1", 8, &failed("x"), Some(43))
            .unwrap());
        let done = WorkerEnd {
            status: WorkerStatus::Done,
            result: Some("{\"summary\":\"ok\"}".into()),
            error: None,
            cost_usd: Some(0.25),
        };
        assert!(db.finish_worker_job("wk-1", 9, &done, Some(42)).unwrap());
        assert!(!db
            .finish_worker_job("wk-1", 10, &failed("late"), None)
            .unwrap());
        assert!(!db.set_worker_job_runner("wk-1", 42, Some(9)).unwrap());
        let got = db.get_worker_job("wk-1").unwrap().unwrap();
        assert_eq!(
            (got.pid, got.start, got.finished_at, got.status),
            (Some(42), Some(9), Some(9), WorkerStatus::Done)
        );
        assert_eq!(got.result.as_deref(), done.result.as_deref());
        assert_eq!(got.cost_usd, Some(0.25));
        // A job does not end as running.
        db.insert_worker_job(&WorkerJobRecord::new("wk-2".into(), "j", None, 6))
            .unwrap();
        let running = WorkerEnd {
            status: WorkerStatus::Running,
            ..failed("x")
        };
        assert!(db.finish_worker_job("wk-2", 7, &running, None).is_err());
    }

    #[test]
    fn jobs_list_newest_first_and_running_ones_apart() {
        let (_dir, db) = open();
        for (n, at) in [(1, 10), (2, 20), (3, 30)] {
            db.insert_worker_job(&WorkerJobRecord::new(format!("wk-{n}"), "j", None, at))
                .unwrap();
        }
        db.finish_worker_job("wk-2", 25, &failed("x"), None)
            .unwrap();
        let ids = |jobs: Vec<WorkerJobRecord>| jobs.into_iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(ids(db.list_worker_jobs(2).unwrap()), ["wk-3", "wk-2"]);
        assert_eq!(ids(db.running_worker_jobs().unwrap()), ["wk-1", "wk-3"]);
    }
}
