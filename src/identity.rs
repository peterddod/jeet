//! The processes jeet launches for a task, recorded so it can later tell
//! who runs it.
//!
//! The jeet that starts a task's pane, an assistant window or a review job
//! records the process it started as the task's anchor: its pid and start
//! time, read from the kernel, never re-bound. A process never records its
//! own.

use anyhow::{bail, Result};

use crate::db::anchors::{Anchor, AnchorKind};
use crate::db::Database;
use crate::worktrees::now_secs;

/// What the kernel says of a live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    /// When it started, in units of the platform's own: compared, never
    /// read as a time.
    pub start: i64,
}

/// `pid`'s parent and start time, or nothing for a process that is gone,
/// a zombie, or cannot be read.
#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, ppid, start) = parse_stat(&stat)?;
    match state {
        'Z' | 'X' | 'x' => None,
        _ => Some(ProcInfo { ppid, start }),
    }
}

/// The state, parent and start time (in clock ticks since boot) of a
/// `/proc/<pid>/stat` line. The command name, in parentheses, may hold
/// spaces and parentheses itself, so the fields are counted from the last
/// `)`.
#[cfg(target_os = "linux")]
fn parse_stat(stat: &str) -> Option<(char, u32, i64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((state, ppid, start))
}

#[cfg(target_os = "macos")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: an all-zero proc_bsdinfo is a valid value (plain integers and
    // byte arrays), and proc_pidinfo writes at most `size` bytes into it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as libc::c_int,
        )
    };
    if usize::try_from(wrote).ok()? != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    let start = i64::try_from(info.pbi_start_tvsec).ok()? * 1_000_000
        + i64::try_from(info.pbi_start_tvusec).ok()?;
    Some(ProcInfo {
        ppid: info.pbi_ppid,
        start,
    })
}

/// Elsewhere no process can be looked up, so nobody is identified and what
/// needs a caller fails closed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn proc_info(_pid: u32) -> Option<ProcInfo> {
    None
}

/// When the machine last booted, as anchors' `created_at` counts: anchors
/// from before it name processes that are gone. Linux start times count
/// from boot, so a pid reused after one could match an old row; macOS start
/// times are absolute, so none can.
fn boot_floor() -> i64 {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        stat.lines()
            .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Whether `anchor`'s process is still the one recorded.
pub fn live(anchor: &Anchor) -> bool {
    proc_info(anchor.pid).is_some_and(|info| info.start == anchor.start)
}

/// Record process `pid`, just started for task `task_id`, as its anchor
/// (in tmux pane `pane_id`, for a pane). Rows of processes that are gone go
/// first. Fails, writing nothing, when the process cannot be read: its
/// caller warns that the agent cannot be identified.
pub fn record(
    db: &Database,
    task_id: &str,
    kind: AnchorKind,
    pane_id: Option<&str>,
    pid: u32,
) -> Result<()> {
    if pid <= 1 {
        bail!("no process id for it ({pid})");
    }
    let Some(info) = proc_info(pid) else {
        bail!("its process {pid} cannot be read");
    };
    let floor = boot_floor();
    for old in db.all_anchors()? {
        if old.created_at < floor || !live(&old) {
            db.delete_anchor(old.pid, old.start)?;
        }
    }
    let anchor = Anchor {
        pid,
        start: info.start,
        task_id: task_id.to_string(),
        kind,
        created_at: now_secs(),
    };
    db.insert_anchor(&anchor, pane_id)
}

/// The warning for task `name`'s agent that [`record`] failed on.
pub fn unidentified(name: &str, e: &anyhow::Error) -> String {
    format!(
        "task {name}'s agent cannot be identified ({e:#}); its gated jeet commands will be \
         refused until it is reopened"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{TaskKind, TaskRecord, TaskState};
    use crate::placement::Placement;
    use std::process::{Command, Stdio};

    fn sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_process_is_known_by_its_parent_and_start_until_it_is_gone() {
        let mut child = sleeper();
        let info = proc_info(child.id()).unwrap();
        assert_eq!(info.ppid, std::process::id());
        assert_eq!(proc_info(child.id()).unwrap().start, info.start);
        child.kill().unwrap();
        // Killed but not yet reaped, it is a zombie: gone.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while proc_info(child.id()).is_some() {
            assert!(std::time::Instant::now() < deadline, "still alive");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.wait().unwrap();
        assert!(proc_info(child.id()).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_line_is_read_from_after_the_command_name() {
        let line = "42 (a) b (c) S 7 42 42 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 98765 1 2";
        assert_eq!(parse_stat(line), Some(('S', 7, 98765)));
        assert_eq!(parse_stat("42 (x"), None);
    }

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        db.insert_task(&TaskRecord {
            id: "t".into(),
            slug: "t".into(),
            name: "t".into(),
            project: None,
            kind: TaskKind::Work,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: None,
            placement: Placement::Local,
        })
        .unwrap();
        (dir, db)
    }

    #[test]
    fn only_a_live_process_is_recorded_and_the_dead_are_pruned() {
        let (_dir, db) = db();
        for pid in [0, 1] {
            assert!(record(&db, "t", AnchorKind::Pane, None, pid).is_err());
        }
        let mut first = sleeper();
        record(&db, "t", AnchorKind::Pane, Some("%1"), first.id()).unwrap();
        let rows = db.all_anchors().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(live(&rows[0]));
        // Once only.
        assert!(record(&db, "t", AnchorKind::Pane, None, first.id()).is_err());
        first.kill().unwrap();
        first.wait().unwrap();
        assert!(!live(&rows[0]));
        assert!(record(&db, "t", AnchorKind::Pane, None, first.id()).is_err());

        let mut second = sleeper();
        record(&db, "t", AnchorKind::Job, None, second.id()).unwrap();
        let rows = db.all_anchors().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].pid, second.id());
        second.kill().unwrap();
        second.wait().unwrap();
    }
}
