//! Who runs this jeet: the task whose agent it was started under, found by
//! walking up the process tree to a process jeet launched for that task.
//!
//! The jeet that starts a task's pane, an assistant window or a review job
//! records the process it started as the task's anchor: its pid and start
//! time, read from the kernel, never re-bound. A process never records its
//! own. The caller is the task of the nearest anchor among this process and
//! its ancestors. `$JEET_TASK_ID` and `$JEET_ASSISTANT` are hints: one that
//! disagrees with the anchor, or claims a task with a parent or the
//! assistant with no anchor behind it, is refused.
//!
//! This guards against mistakes and casual forging, not an agent set on
//! escaping: one that orphans a process with its environment stripped, or
//! starts one through tmux, gets past it. See design §5.7.

use anyhow::{bail, Result};

use crate::db::anchors::{Anchor, AnchorKind};
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::db::Database;
use crate::permissions::{self, ASSISTANT_ENV};
use crate::tasks::TASK_ID_ENV;
use crate::worktrees::now_secs;

/// How far up the process tree to look.
const MAX_DEPTH: usize = 64;

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

/// This process and its ancestors, nearest first, as (pid, start): up to
/// init, a process that cannot be read, a loop, or [`MAX_DEPTH`].
fn ancestry() -> Vec<(u32, i64)> {
    let mut chain: Vec<(u32, i64)> = Vec::new();
    let mut pid = std::process::id();
    while pid > 1 && chain.len() < MAX_DEPTH && !chain.iter().any(|(p, _)| *p == pid) {
        let Some(info) = proc_info(pid) else {
            break;
        };
        chain.push((pid, info.start));
        pid = info.ppid;
    }
    chain
}

/// Whether `anchor`'s process is still the one recorded.
pub fn live(anchor: &Anchor) -> bool {
    proc_info(anchor.pid).is_some_and(|info| info.start == anchor.start)
}

/// What the environment says the caller is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Hint {
    /// `$JEET_TASK_ID`.
    task: Option<String>,
    /// `$JEET_ASSISTANT`.
    assistant: bool,
}

impl Hint {
    fn from_env() -> Self {
        Hint {
            task: std::env::var(TASK_ID_ENV)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            assistant: permissions::assistant_mode(),
        }
    }
}

/// Who the caller is, before the index is asked about the task.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// This process is the review job runner anchored to the task.
    SelfJob(String),
    /// The nearest anchor is the task's, and the hints agree.
    Anchored(String),
    /// The nearest anchor is `anchored`'s, but the hints claim `claimed`
    /// (`None`: the assistant).
    Mismatch {
        anchored: String,
        claimed: Option<String>,
    },
    /// The hints claim a task that needs an anchor (`None`: the assistant),
    /// and there is none.
    Unconfirmed(Option<String>),
    Nobody,
}

/// Who the caller with ancestry `chain` is, given the `anchors` of those
/// processes and the `hint`. `is_root` says whether a task is the
/// assistant; `needs_anchor` whether one may only be claimed from under
/// an anchor (a task with a parent, or the assistant). Both fail closed.
fn resolve(
    chain: &[(u32, i64)],
    anchors: &[Anchor],
    hint: &Hint,
    is_root: impl Fn(&str) -> bool,
    needs_anchor: impl Fn(&str) -> bool,
) -> Verdict {
    let nearest = chain.iter().enumerate().find_map(|(depth, (pid, start))| {
        anchors
            .iter()
            .find(|a| a.pid == *pid && a.start == *start)
            .map(|a| (depth, a))
    });
    if let Some((depth, anchor)) = nearest {
        let anchored = anchor.task_id.clone();
        // The runner inherits whoever started it's environment: only its
        // anchor counts.
        if depth == 0 && anchor.kind == AnchorKind::Job {
            return Verdict::SelfJob(anchored);
        }
        if let Some(claimed) = hint.task.as_ref().filter(|t| **t != anchored) {
            return Verdict::Mismatch {
                anchored,
                claimed: Some(claimed.clone()),
            };
        }
        if hint.assistant && !is_root(&anchored) {
            return Verdict::Mismatch {
                anchored,
                claimed: None,
            };
        }
        return Verdict::Anchored(anchored);
    }
    match &hint.task {
        Some(task) if needs_anchor(task) => Verdict::Unconfirmed(Some(task.clone())),
        _ if hint.assistant => Verdict::Unconfirmed(None),
        _ => Verdict::Nobody,
    }
}

/// Why a caller is refused whatever it runs.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    /// Its environment claims another task than the one it runs under:
    /// logged as a refusal.
    #[error("{0}")]
    Mismatch(String),
    /// Its environment claims a task jeet cannot find it under.
    #[error("{0}")]
    Unconfirmed(String),
}

/// Who runs this jeet.
pub struct Identity {
    /// The task it runs under, if any.
    pub task: Option<TaskRecord>,
    /// It is the review job runner jeet started for `task`.
    pub self_job: bool,
}

/// Who runs this jeet, by its process ancestry: see the module docs. A
/// claim jeet cannot confirm, or one that disagrees, is an error
/// ([`Refused`]), as is a database error.
pub fn identify(db: &Database) -> Result<Identity> {
    let chain = ancestry();
    let pids: Vec<u32> = chain.iter().map(|(pid, _)| *pid).collect();
    let anchors = anchors_of(db, &pids)?;
    let task = |id: &str| db.get_task(id).ok().flatten();
    let verdict = resolve(
        &chain,
        &anchors,
        &Hint::from_env(),
        |id| task(id).is_some_and(|t| t.kind == TaskKind::Assistant),
        |id| match db.get_task(id) {
            Ok(Some(t)) => t.parent_id.is_some() || t.kind == TaskKind::Assistant,
            Ok(None) => false,
            Err(_) => true,
        },
    );
    let label = |id: &str| match task(id) {
        Some(t) if t.kind == TaskKind::Assistant => "the assistant".to_string(),
        Some(t) => format!("task {}", t.slug),
        None => format!("task {id}"),
    };
    let (id, self_job) = match verdict {
        Verdict::SelfJob(id) => (id, true),
        Verdict::Anchored(id) => (id, false),
        Verdict::Nobody => {
            return Ok(Identity {
                task: None,
                self_job: false,
            })
        }
        Verdict::Mismatch { anchored, claimed } => {
            let claim = match claimed {
                Some(id) => format!("${TASK_ID_ENV} says it is {} ({id})", label(&id)),
                None => format!("${ASSISTANT_ENV} says it is the assistant"),
            };
            return Err(Refused::Mismatch(format!(
                "this jeet runs under {}'s agent ({anchored}), but {claim}; jeet refuses a \
                 caller that claims to be another",
                label(&anchored)
            ))
            .into());
        }
        Verdict::Unconfirmed(claimed) => {
            let is_assistant =
                |id: &String| task(id).is_some_and(|t| t.kind == TaskKind::Assistant);
            let (who, reopen) = match claimed.filter(|id| !is_assistant(id)) {
                Some(id) => (format!("{}'s agent", label(&id)), "reopen the task"),
                None => (
                    "the assistant".to_string(),
                    "exit the assistant in its window and run `jeet chat` again",
                ),
            };
            return Err(Refused::Unconfirmed(format!(
                "jeet can't confirm this is {who}: {reopen} (agents started before this jeet \
                 must be restarted), or run this outside the agent's tmux session"
            ))
            .into());
        }
    };
    Ok(Identity {
        task: db.get_task(&id)?,
        self_job,
    })
}

/// The anchors of the processes `pids` that can still be theirs: none
/// written before the machine last booted.
fn anchors_of(db: &Database, pids: &[u32]) -> Result<Vec<Anchor>> {
    db.anchors_for(pids, boot_floor())
}

/// Forget the anchors of processes that are gone, or from before the
/// machine last booted.
fn prune(db: &Database) -> Result<()> {
    let floor = boot_floor();
    for old in db.all_anchors()? {
        if old.created_at < floor || !live(&old) {
            db.delete_anchor(old.pid, old.start)?;
        }
    }
    Ok(())
}

/// Record process `pid`, just started for task `task_id`, as its anchor
/// (in tmux pane `pane_id`, for a pane). The process is already running
/// and may ask who it is at once, so its row goes in first; rows of
/// processes that are gone go after, or first when one is in its way.
/// Fails, writing nothing, when the process cannot be read: its caller
/// warns that the agent cannot be identified.
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
    let anchor = Anchor {
        pid,
        start: info.start,
        task_id: task_id.to_string(),
        kind,
        created_at: now_secs(),
    };
    if db.insert_anchor(&anchor, pane_id).is_err() {
        // A row from before a reboot can hold the same pid and start.
        prune(db)?;
        return db.insert_anchor(&anchor, pane_id);
    }
    // Recorded: a sweep that fails costs only stale rows.
    let _ = prune(db);
    Ok(())
}

/// The warning for task `name`'s agent that [`record`] failed on. With no
/// anchor, only its environment says who it is: what claims the task is
/// refused, but a command that drops the claim runs as the user.
pub fn unidentified(name: &str, e: &anyhow::Error) -> String {
    format!(
        "task {name}'s agent cannot be identified ({e:#}): its jeet commands that name the \
         task are refused, but one run without ${TASK_ID_ENV} and ${ASSISTANT_ENV} runs as \
         the user; stop it and open the task again"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::TaskState;
    use crate::placement::Placement;
    use std::process::{Command, Stdio};

    fn anchor(pid: u32, start: i64, task: &str, kind: AnchorKind) -> Anchor {
        Anchor {
            pid,
            start,
            task_id: task.into(),
            kind,
            created_at: 0,
        }
    }

    fn hint(task: Option<&str>, assistant: bool) -> Hint {
        Hint {
            task: task.map(Into::into),
            assistant,
        }
    }

    /// `resolve` where `root` is the assistant, `w` has a parent and `top`
    /// does not.
    fn verdict(chain: &[(u32, i64)], anchors: &[Anchor], hint: &Hint) -> Verdict {
        resolve(
            chain,
            anchors,
            hint,
            |id| id == "root",
            |id| id == "root" || id == "w",
        )
    }

    fn sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn the_walk_starts_here_and_goes_up() {
        let chain = ancestry();
        assert_eq!(chain[0].0, std::process::id());
        assert_eq!(chain[1].0, std::os::unix::process::parent_id());
        assert_eq!(chain[0].1, proc_info(std::process::id()).unwrap().start);
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

    #[test]
    fn the_nearest_anchor_names_the_caller_and_the_hints_must_agree() {
        let chain = [(30, 3), (20, 2), (10, 1)];
        let w = anchor(20, 2, "w", AnchorKind::Pane);
        let root = anchor(10, 1, "root", AnchorKind::Pane);
        let both = [root.clone(), w.clone()];
        let anchored = |id: &str| Verdict::Anchored(id.into());
        assert_eq!(verdict(&chain, &both, &hint(None, false)), anchored("w"));
        // Another start time is another process.
        let stale = [anchor(20, 9, "w", AnchorKind::Pane), root.clone()];
        assert_eq!(
            verdict(&chain, &stale, &hint(None, false)),
            anchored("root")
        );
        assert_eq!(
            verdict(&chain, &both, &hint(Some("w"), false)),
            anchored("w")
        );
        assert_eq!(
            verdict(&chain, &both, &hint(Some("top"), false)),
            Verdict::Mismatch {
                anchored: "w".into(),
                claimed: Some("top".into())
            }
        );
        // A Node, or a top-level task, claiming to be the assistant.
        assert_eq!(
            verdict(&chain, &both, &hint(None, true)),
            Verdict::Mismatch {
                anchored: "w".into(),
                claimed: None
            }
        );
        let top = [anchor(20, 2, "top", AnchorKind::Pane)];
        assert!(matches!(
            verdict(&chain, &top, &hint(None, true)),
            Verdict::Mismatch { .. }
        ));
        let only_root = [root];
        assert_eq!(
            verdict(&chain, &only_root, &hint(Some("root"), true)),
            anchored("root")
        );
    }

    #[test]
    fn a_review_runner_is_its_own_jobs_whatever_it_inherited() {
        let chain = [(30, 3), (20, 2), (10, 1)];
        let root = anchor(10, 1, "root", AnchorKind::Pane);
        let job = anchor(30, 3, "w", AnchorKind::Job);
        assert_eq!(
            verdict(&chain, &[root.clone(), job], &hint(Some("root"), true)),
            Verdict::SelfJob("w".into())
        );
        // Its command, one process down, is the job's task like any other.
        let job = anchor(20, 2, "w", AnchorKind::Job);
        assert_eq!(
            verdict(&chain, &[root, job.clone()], &hint(Some("w"), false)),
            Verdict::Anchored("w".into())
        );
        assert!(matches!(
            verdict(&chain, &[job], &hint(Some("root"), true)),
            Verdict::Mismatch { .. }
        ));
    }

    #[test]
    fn a_claim_with_no_anchor_is_unconfirmed_unless_it_needs_none() {
        let chain = [(30, 3), (20, 2)];
        let none: [Anchor; 0] = [];
        assert_eq!(
            verdict(&chain, &none, &hint(Some("w"), false)),
            Verdict::Unconfirmed(Some("w".into()))
        );
        assert_eq!(
            verdict(&chain, &none, &hint(Some("root"), false)),
            Verdict::Unconfirmed(Some("root".into()))
        );
        assert_eq!(
            verdict(&chain, &none, &hint(None, true)),
            Verdict::Unconfirmed(None)
        );
        assert_eq!(
            verdict(&chain, &none, &hint(Some("top"), true)),
            Verdict::Unconfirmed(None)
        );
        for task in ["top", "unknown"] {
            assert_eq!(
                verdict(&chain, &none, &hint(Some(task), false)),
                Verdict::Nobody
            );
        }
        assert_eq!(verdict(&chain, &none, &hint(None, false)), Verdict::Nobody);
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

    /// A row from before the last boot can match a live process on Linux,
    /// whose start times count from boot: it is neither matched nor kept.
    #[test]
    fn rows_from_before_the_last_boot_are_neither_matched_nor_kept() {
        let (_dir, db) = db();
        let old = sleeper();
        let stale = Anchor {
            created_at: boot_floor() - 1,
            ..anchor(
                old.id(),
                proc_info(old.id()).unwrap().start,
                "t",
                AnchorKind::Pane,
            )
        };
        db.insert_anchor(&stale, None).unwrap();
        assert!(live(&stale));
        assert!(anchors_of(&db, &[old.id()]).unwrap().is_empty());

        let new = sleeper();
        record(&db, "t", AnchorKind::Pane, None, new.id()).unwrap();
        let rows = db.all_anchors().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].pid, new.id());
        assert_eq!(anchors_of(&db, &[new.id()]).unwrap(), rows);

        // One in the way of the process being recorded goes too.
        db.insert_anchor(&stale, None).unwrap();
        record(&db, "t", AnchorKind::Job, None, old.id()).unwrap();
        let rows = anchors_of(&db, &[old.id()]).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, AnchorKind::Job);
        for mut child in [old, new] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
