//! Who runs this jeet: the task whose agent it was started under, found by
//! walking up the process tree to a process jeet launched for that task.
//!
//! The jeet that starts a task's pane, an assistant window or a review job
//! records the process it started as the task's anchor: its pid and start
//! time, read from the kernel, never re-bound. A process never records its
//! own. The one other writer is [`adopt`]: the user, at a terminal outside
//! every pane of jeet's, vouching for an agent an older jeet started. The
//! caller is the task of the nearest anchor among this process and
//! its ancestors. `$JEET_TASK_ID` and `$JEET_ASSISTANT` are hints: one that
//! disagrees with the anchor, or claims a task with a parent or the
//! assistant with no anchor behind it, is refused.
//!
//! This guards against mistakes and casual forging, not an agent set on
//! escaping: one that orphans a process with its environment stripped, or
//! starts one through tmux, gets past it. See design §5.7.

use anyhow::{bail, Result};

use crate::context::App;
use crate::db::anchors::{Anchor, AnchorKind};
use crate::db::reviews::ReviewRecord;
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
    let (state, ppid, ticks) = parse_stat(&stat)?;
    match state {
        'Z' | 'X' | 'x' => None,
        _ => Some(ProcInfo {
            ppid,
            start: on_boot(boot()?, ticks),
        }),
    }
}

/// This boot's id, which the kernel makes afresh at each boot, or nothing
/// when it cannot be read (and so nobody is identified).
#[cfg(target_os = "linux")]
fn boot() -> Option<&'static str> {
    static BOOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(id.trim().to_string()).filter(|id| !id.is_empty())
    })
    .as_deref()
}

/// A start time of `ticks` since boot `boot`, as one number: the low 39
/// bits the ticks (174 years at the 100 Hz `/proc` counts in), the 24 above
/// them a hash of the boot id. Linux counts start times from boot, so
/// without the boot a pid reused after a reboot, started as many ticks in,
/// would be the old process; with it, a row from an earlier boot matches
/// nothing (but for a one in 2^24 clash of hashes, which still needs the
/// same pid and tick). The boot id, unlike the boot time, does not move
/// when the clock is set.
#[cfg(any(target_os = "linux", test))]
fn on_boot(boot: &str, ticks: i64) -> i64 {
    // FNV-1a: stable across builds, unlike std's hasher.
    let hash = boot.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let tag = (hash & ((1 << 24) - 1)) as i64;
    (tag << 39) | (ticks & ((1 << 39) - 1))
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

/// `proc_bsdshortinfo` from `<sys/proc_info.h>`, which the libc crate
/// lacks.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcBsdShortInfo {
    pid: u32,
    ppid: u32,
    pgid: u32,
    status: u32,
    comm: [libc::c_char; 16],
    flags: u32,
    uid: libc::uid_t,
    gid: libc::gid_t,
    ruid: libc::uid_t,
    rgid: libc::gid_t,
    svuid: libc::uid_t,
    svgid: libc::gid_t,
    rfu: u32,
}

/// Whether `pid` is another user's process. macOS reads only the caller's
/// own processes in full (`proc_info` fails for root's, such as the setuid
/// `login` a terminal starts its shell under), but tells anyone a process's
/// owner.
#[cfg(target_os = "macos")]
fn foreign(pid: u32) -> bool {
    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    let size = std::mem::size_of::<ProcBsdShortInfo>();
    // SAFETY: as in `proc_info`: plain integers, written at most `size`
    // bytes.
    let mut info: ProcBsdShortInfo = unsafe { std::mem::zeroed() };
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut ProcBsdShortInfo).cast(),
            size as libc::c_int,
        )
    };
    usize::try_from(wrote).is_ok_and(|wrote| wrote == size)
        && info.uid != unsafe { libc::geteuid() }
}

/// Linux's `/proc` shows every user's processes, so one it cannot read is
/// hidden from it, not another user's.
#[cfg(not(target_os = "macos"))]
fn foreign(_pid: u32) -> bool {
    false
}

/// Elsewhere no process can be looked up, so nobody is identified and what
/// needs a caller fails closed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn proc_info(_pid: u32) -> Option<ProcInfo> {
    None
}

/// This process and its ancestors, nearest first, as (pid, start): up to
/// init, another user's process (no agent jeet started runs above one), a
/// process that cannot be read, a loop, or [`MAX_DEPTH`]. With it, the
/// process that could not be read, if that is where it stopped.
fn ancestry() -> (Vec<(u32, i64)>, Option<u32>) {
    let mut chain: Vec<(u32, i64)> = Vec::new();
    let mut pid = std::process::id();
    while pid > 1 && chain.len() < MAX_DEPTH && !chain.iter().any(|(p, _)| *p == pid) {
        let Some(info) = proc_info(pid) else {
            return (chain, (!foreign(pid)).then_some(pid));
        };
        chain.push((pid, info.start));
        pid = info.ppid;
    }
    (chain, None)
}

/// When process `pid` started, as an anchor records it, or nothing for a
/// process that is gone or cannot be read.
pub fn start_of(pid: u32) -> Option<i64> {
    proc_info(pid).map(|info| info.start)
}

/// Whether `anchor`'s process is still the one recorded.
pub fn live(anchor: &Anchor) -> bool {
    proc_info(anchor.pid).is_some_and(|info| info.start == anchor.start)
}

/// Whether this jeet runs under `anchor`'s process: is it, or descends
/// from it. Only as far up as [`ancestry`] can read.
pub fn runs_under(anchor: &Anchor) -> bool {
    ancestry().0.contains(&(anchor.pid, anchor.start))
}

/// Whether this jeet runs in any of `pids` or below one: the live
/// processes tmux started in panes, say. Only as far up as [`ancestry`] can
/// read.
pub fn runs_under_any(pids: &[u32]) -> bool {
    ancestry().0.iter().any(|(pid, _)| pids.contains(pid))
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
    /// The review it runs under, if any: a review has an id of its own
    /// that the anchors and `$JEET_TASK_ID` name, as a task's. The user's
    /// own review job is not one: it runs as the user.
    pub review: Option<ReviewRecord>,
    /// It is the review job runner jeet started for `task`.
    pub self_job: bool,
}

/// Who runs this jeet, by its process ancestry: see the module docs. A
/// claim jeet cannot confirm, or one that disagrees, is an error
/// ([`Refused`]), as is a database error. One that disagrees is also
/// written to `actions.log`, in jeet's words only: the claim is the
/// caller's, so none of it goes where the assistant reads.
pub fn identify(app: &App) -> Result<Identity> {
    let db = &app.db;
    let (chain, unreadable) = ancestry();
    let pids: Vec<u32> = chain.iter().map(|(pid, _)| *pid).collect();
    let anchors = db.anchors_for(&pids)?;
    let task = |id: &str| db.get_task(id).ok().flatten();
    let verdict = resolve(
        &chain,
        &anchors,
        &Hint::from_env(),
        |id| task(id).is_some_and(|t| t.kind == TaskKind::Assistant),
        |id| match db.get_task(id) {
            Ok(Some(t)) => t.parent_id.is_some() || t.kind == TaskKind::Assistant,
            // A review's agent is gated whoever asked for it; an unknown
            // task (deleted, say, under a review job still running for it)
            // is not taken for the user.
            Ok(None) | Err(_) => true,
        },
    );
    let label = |id: &str| match task(id) {
        Some(t) if t.kind == TaskKind::Assistant => "the assistant".to_string(),
        Some(t) => format!("task {}", t.slug),
        None => match db.get_review(id).ok().flatten() {
            Some(r) => format!("review {}", r.label()),
            None => format!("task {id}"),
        },
    };
    let (id, self_job) = match verdict {
        Verdict::SelfJob(id) => (id, true),
        Verdict::Anchored(id) => (id, false),
        Verdict::Nobody => {
            return Ok(Identity {
                task: None,
                review: None,
                self_job: false,
            })
        }
        Verdict::Mismatch { anchored, claimed } => {
            let claim = match claimed {
                Some(id) => format!("${TASK_ID_ENV} says it is {} ({id})", label(&id)),
                None => format!("${ASSISTANT_ENV} says it is the assistant"),
            };
            let who = match label(&anchored) {
                who if who == "the assistant" => who,
                task => format!("{task}'s agent"),
            };
            permissions::log_mismatch(&app.home, &format!("{who} ({anchored})"));
            return Err(Refused::Mismatch(format!(
                "this jeet runs under {who} ({anchored}), but {claim}; jeet refuses a caller \
                 that claims to be another"
            ))
            .into());
        }
        Verdict::Unconfirmed(claimed) => {
            // A lookup that fails is an index error, not a task that is gone.
            let found = claimed.as_deref().map(|id| db.get_task(id)).transpose()?;
            // A review is not a task, but its agent is claimed the same way.
            let review = match (&claimed, &found) {
                (Some(id), Some(None)) => db.get_review(id)?,
                _ => None,
            };
            if let (Some(id), Some(review)) = (&claimed, &review) {
                return Err(Refused::Unconfirmed(format!(
                    "jeet can't confirm this is review {}'s agent ({id}): jeet has no record of \
                     starting it, as for an agent an older jeet started; `jeet task open {}` \
                     starts it again in a new session, or run this outside the agent's tmux \
                     session",
                    review.label(),
                    review.slug
                ))
                .into());
            }
            let (who, reopen) = match (claimed, found) {
                (Some(id), Some(None)) => {
                    return Err(Refused::Unconfirmed(format!(
                        "${TASK_ID_ENV} is {id}, which is no task jeet knows (was it deleted?); \
                         jeet refuses a claim it cannot confirm, so run this outside that \
                         task's agent"
                    ))
                    .into())
                }
                (Some(id), Some(Some(t))) if t.kind != TaskKind::Assistant => (
                    format!("{}'s agent", label(&id)),
                    format!(
                        "the user can restart it (`jeet task restart {0}`, which keeps its \
                         conversation) or adopt it (`jeet task adopt {0}`)",
                        t.slug
                    ),
                ),
                _ => (
                    "the assistant".to_string(),
                    "the user can restart it (`jeet task restart assistant`, which keeps its \
                     conversation) or adopt it (`jeet task adopt assistant`)"
                        .to_string(),
                ),
            };
            let advice = match unreadable {
                // A sandbox that hides the processes outside it, say.
                Some(pid) => format!(
                    "it cannot read process {pid} above this jeet, so cannot reach the agent's; \
                     if the agent runs its commands in a sandbox, exclude jeet from it"
                ),
                None => format!(
                    "jeet has no record of starting it, as for an agent an older jeet \
                     started: {reopen}; or run this outside the agent's tmux session; if the agent runs its commands in a \
                     sandbox with its own process ids (Claude Code's on Linux), exclude jeet \
                     from it"
                ),
            };
            return Err(Refused::Unconfirmed(format!(
                "jeet can't confirm this is {who}: {advice}"
            ))
            .into());
        }
    };
    if let Some(task) = db.get_task(&id)? {
        return Ok(Identity {
            task: Some(task),
            review: None,
            self_job,
        });
    }
    let review = db.get_review(&id)?;
    // The command of a review nobody asked for is the user's own: it runs
    // as the user, as a top-level task's did.
    let users = review.as_ref().is_some_and(|r| {
        r.requested_by.is_none()
            && anchors.iter().any(|a| {
                a.task_id == id && a.kind == AnchorKind::Job && chain.contains(&(a.pid, a.start))
            })
    });
    Ok(Identity {
        task: None,
        review: review.filter(|_| !users),
        self_job,
    })
}

/// Forget the anchors of processes that are gone: a pid now another
/// process's, or one from an earlier boot, starts at another time.
fn prune(db: &Database) -> Result<()> {
    for old in db.all_anchors()? {
        if !live(&old) {
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
        // A row of a process that is gone can hold the same pid and start
        // (an earlier boot's, should its hash clash).
        prune(db)?;
        return db.insert_anchor(&anchor, pane_id);
    }
    // Recorded: a sweep that fails costs only stale rows.
    let _ = prune(db);
    Ok(())
}

/// Record process `pid`, the agent an older jeet started in tmux pane
/// `pane_id` for task `task_id`, as its anchor, on the user's word: what
/// `jeet task adopt` writes once the user has confirmed it. `jeet_panes`
/// are the processes in the panes of jeet's tmux sessions. Refused unless
/// this jeet runs outside every one of them, so no agent, nor anything it
/// starts in its pane, adopts itself or another; and when a live process
/// is anchored to the task already (`per_window`: that process only, for
/// the assistant, whose windows each have one), so none is ever re-bound or
/// doubled. What gets past this is what gets past identity anyway (design
/// §5.7): a process orphaned with its environment stripped, or started
/// through tmux, which already runs as the user.
pub fn adopt(
    db: &Database,
    task_id: &str,
    pane_id: &str,
    pid: u32,
    jeet_panes: &[u32],
    per_window: bool,
) -> Result<()> {
    if runs_under_any(jeet_panes) || runs_under_any(&[pid]) {
        bail!(
            "this jeet runs in a pane of jeet's tmux sessions; adopt an agent from a terminal \
             of your own, outside them"
        );
    }
    let anchored = db
        .task_anchors(task_id, AnchorKind::Pane)?
        .into_iter()
        .filter(live)
        .any(|a| !per_window || a.pid == pid);
    if anchored {
        bail!("jeet already knows this agent's process");
    }
    record(db, task_id, AnchorKind::Pane, Some(pane_id), pid)
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
    use crate::db::tasks::{RunState, TaskState};
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

    /// `resolve` where `root` is the assistant, `w` has a parent, `top`
    /// does not, and any other task is unknown.
    fn verdict(chain: &[(u32, i64)], anchors: &[Anchor], hint: &Hint) -> Verdict {
        resolve(chain, anchors, hint, |id| id == "root", |id| id != "top")
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
        let (chain, _) = ancestry();
        assert_eq!(chain[0].0, std::process::id());
        assert_eq!(chain[1].0, std::os::unix::process::parent_id());
        assert_eq!(chain[0].1, proc_info(std::process::id()).unwrap().start);
    }

    #[test]
    fn only_another_users_process_is_foreign() {
        assert!(!foreign(std::process::id()));
        // Root's launchd, which macOS will not read in full for anyone else.
        #[cfg(target_os = "macos")]
        if unsafe { libc::geteuid() } != 0 {
            assert!(proc_info(1).is_none());
            assert!(foreign(1));
        }
        let mut child = sleeper();
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!foreign(pid));
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
        assert_eq!(
            verdict(&chain, &none, &hint(Some("top"), false)),
            Verdict::Nobody
        );
        // A task jeet does not know, deleted under a job still running for
        // it say, is no claim to take for the user's.
        assert_eq!(
            verdict(&chain, &none, &hint(Some("unknown"), false)),
            Verdict::Unconfirmed(Some("unknown".into()))
        );
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
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
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

    #[test]
    fn the_user_adopts_an_agent_only_from_outside_its_pane_and_only_once() {
        let (_dir, db) = db();
        let mut agent = sleeper();
        let pid = agent.id();
        // From a pane of jeet's: this test's own process stands in for it.
        let here = std::process::id();
        let e = adopt(&db, "t", "%1", pid, &[here], false).unwrap_err();
        assert!(e.to_string().contains("outside"), "{e}");
        // Or from under the very process to adopt.
        let parent = std::os::unix::process::parent_id();
        assert!(adopt(&db, "t", "%1", parent, &[], false).is_err());
        assert!(db.all_anchors().unwrap().is_empty());

        adopt(&db, "t", "%1", pid, &[], false).unwrap();
        let rows = db.task_anchors("t", AnchorKind::Pane).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, pid);
        // A task has one agent: another process is not adopted beside it,
        // nor the same one twice.
        let mut other = sleeper();
        assert!(adopt(&db, "t", "%2", other.id(), &[], false).is_err());
        assert!(adopt(&db, "t", "%1", pid, &[], true).is_err());
        // The assistant's windows each have their own.
        adopt(&db, "t", "%2", other.id(), &[], true).unwrap();
        assert_eq!(db.task_anchors("t", AnchorKind::Pane).unwrap().len(), 2);
        for child in [&mut agent, &mut other] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// A row whose start is not its process's, as one from an earlier boot
    /// is not on Linux, is neither matched nor kept.
    #[test]
    fn a_row_another_process_started_is_neither_matched_nor_kept() {
        let (_dir, db) = db();
        let old = sleeper();
        let start = proc_info(old.id()).unwrap().start;
        let stale = anchor(old.id(), start + 1, "t", AnchorKind::Pane);
        db.insert_anchor(&stale, None).unwrap();
        assert!(!live(&stale));
        let chain = [(old.id(), start)];
        let rows = db.anchors_for(&[old.id()]).unwrap();
        assert_eq!(verdict(&chain, &rows, &hint(None, false)), Verdict::Nobody);

        let new = sleeper();
        record(&db, "t", AnchorKind::Pane, None, new.id()).unwrap();
        let rows = db.all_anchors().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].pid, new.id());

        // Nor does one under the pid of the process being recorded.
        db.insert_anchor(&stale, None).unwrap();
        record(&db, "t", AnchorKind::Job, None, old.id()).unwrap();
        let rows = db.anchors_for(&[old.id()]).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, AnchorKind::Job);
        assert_eq!(rows[0].start, start);
        for mut child in [old, new] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// The same tick count since another boot is another start, and setting
    /// the clock changes neither.
    #[test]
    fn a_start_is_tagged_with_its_boot() {
        let boot = "6f1c1f0e-3b9a-4c0e-9d7e-2b1f4a5c6d7e";
        let other = "0a9b8c7d-6e5f-4a3b-8c2d-1e0f9a8b7c6d";
        assert_eq!(on_boot(boot, 98765), on_boot(boot, 98765));
        assert_ne!(on_boot(boot, 98765), on_boot(other, 98765));
        assert_ne!(on_boot(boot, 98765), on_boot(boot, 98766));
        assert!(on_boot(boot, 98765) > 0);
        assert!(on_boot(other, i64::MAX) > 0);
    }

    /// On Linux a process's start is its ticks since this boot, tagged with
    /// this boot: a row written with another boot's tag is not its.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_row_from_another_boot_is_not_the_process() {
        let (_dir, db) = db();
        let mut child = sleeper();
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
        let (_, _, ticks) = parse_stat(&stat).unwrap();
        let start = proc_info(child.id()).unwrap().start;
        assert_eq!(start, on_boot(boot().unwrap(), ticks));
        let earlier = anchor(
            child.id(),
            on_boot("an-earlier-boot", ticks),
            "t",
            AnchorKind::Pane,
        );
        db.insert_anchor(&earlier, None).unwrap();
        assert!(!live(&earlier));
        let rows = db.anchors_for(&[child.id()]).unwrap();
        assert_eq!(
            verdict(&[(child.id(), start)], &rows, &hint(None, false)),
            Verdict::Nobody
        );
        prune(&db).unwrap();
        assert!(db.all_anchors().unwrap().is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
