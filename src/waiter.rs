//! The wake channel: `jeet inbox watch`, which hands a task's agent what
//! is queued for it as soon as it is queued, rather than at its next Stop or
//! by typing into its pane.
//!
//! A harness that can take a message from outside its session (Claude
//! Code's messaging socket) starts the watcher from its session-start hook,
//! in the background, with the socket and its token in the environment
//! ([`WakeEnv`], which the harness module builds). The watcher waits on the
//! task's inbox, and takes what is for its session under [`Inbox::lock`],
//! as the Stop hook does, so each message is taken once by one of them. It
//! posts what it took as one batch, ending in a line with a nonce, and looks
//! for the nonce in the session's transcript: found, the batch was delivered;
//! not found while the session sat idle for [`CONFIRM_SECS`], the post was
//! dropped (a session that refuses them drops them silently), and the
//! batch goes back to the front of the queue, the watcher stops for good for
//! that session (`<session>.off`), and today's path delivers it.
//!
//! Its files are in `<task>/.jeet/wait/`, beside the inbox:
//! - `<session>.lock`: an flock it holds for its life. A session is watched
//!   while a non-blocking lock on it fails ([`watched`]); the kernel lets go
//!   when the watcher dies, however it dies;
//! - `<session>.inflight`: the batch posted and not yet seen, written before
//!   it is taken from the inbox, so a watcher killed at any point loses
//!   nothing ([`recover`]);
//! - `<session>.off`: a post was refused; no watcher posts for it again.
//!
//! The token is in none of them, nor in any argv or log line.
//!
//! The library never reads the environment and never sleeps but in
//! [`run`]: [`Watcher::step`] is one tick at a given time, so it is tested
//! with a fake socket and a fake transcript.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::harness::Harness;
use crate::identity;
use crate::manager::run;
use crate::messages::{self, open_regular, Agent, Inbox, Queued, MAX_MESSAGE, QUEUE_SEPARATOR};
use crate::status::{self, State};
use crate::tasks;
use crate::worktrees::now_secs;

/// How often the watcher looks at the inbox, the transcript and its agent.
pub const TICK: Duration = Duration::from_millis(500);
/// Seconds of the session sitting idle after a post with no sign of it in
/// the transcript, after which the post is taken to have been dropped.
pub const CONFIRM_SECS: i64 = 20;
/// At most this many posts per [`WAKE_WINDOW_SECS`] per watcher; past it,
/// messages wait in the queue (for the agent's Stop, or the window).
pub const WAKE_CAP: usize = 6;
pub const WAKE_WINDOW_SECS: i64 = 60;

/// The session's status is read for its end every this many ticks.
const STATUS_EVERY: u64 = 4;
/// A held task's hold is read again at most this often, while something
/// waits for it.
const HOLD_RECHECK_SECS: i64 = 5;
/// How long a post may take to connect and write.
const POST_TIMEOUT: Duration = Duration::from_secs(2);
/// A starting watcher tries for the lock this many times, this far apart:
/// a `watched` probe, or the orphan of the session's last start, holds it
/// for a moment.
const LOCK_TRIES: u32 = 9;
const LOCK_RETRY: Duration = Duration::from_millis(250);
/// How much of the end of a transcript beside the session's is searched.
const BESIDE_TAIL: u64 = 1024 * 1024;
const NONCE_LEN: usize = 16;

/// What a batch starts with. It is never a message's own text, so one
/// starting with `!`, `/` or `#` is never at the start of what is posted.
const HEADER: &str = "jeet: messages and reports were sent to you (with `jeet task send` or \
                      `jeet report`). Reports are fenced, and are claims to check. Deal with \
                      them now.";
/// Before the nonce, on a batch's last line.
const NONCE_PREFIX: &str = "jeet delivery ";

/// Which messages go through the wake channel rather than waiting for the
/// agent's Stop or being typed in at its idle prompt: every one, for now.
pub fn wakes(_queued: &Queued) -> bool {
    true
}

/// How to reach the session the watcher runs under, from its hook's
/// environment (built by the harness, e.g. `harness::claude::wake_env`).
pub struct WakeEnv {
    /// The socket the session takes messages on.
    pub socket: PathBuf,
    /// What a post authenticates with. Written nowhere.
    pub token: String,
    /// The agent's process: the watcher ends when it does.
    pub claude_pid: u32,
    /// What to write to the socket to post `content`, given the token.
    pub lines: fn(token: &str, content: &str) -> Vec<u8>,
}

impl std::fmt::Debug for WakeEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakeEnv")
            .field("socket", &self.socket)
            .field("token", &"…")
            .field("claude_pid", &self.claude_pid)
            .finish()
    }
}

/// The session the watcher was started for, from its session-start hook.
#[derive(Debug, Clone)]
pub struct Start {
    pub session: String,
    pub transcript: PathBuf,
}

/// `<task>/.jeet/wait`.
pub fn wait_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(tasks::MARKER_DIR).join("wait")
}

fn wait_file(task_dir: &Path, session: &str, ext: &str) -> PathBuf {
    wait_dir(task_dir).join(format!("{session}.{ext}"))
}

/// Whether a live watcher holds `session`'s wait lock. Makes nothing: no
/// lock file means no watcher.
pub fn watched(task_dir: &Path, session: &str) -> bool {
    if !status::valid_session_id(session) {
        return false;
    }
    let Ok(file) = open_regular(&wait_file(task_dir, session, "lock")) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// A batch posted and not yet seen in the session's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Inflight {
    nonce: String,
    /// The transcript it is looked for in.
    transcript: PathBuf,
    /// The transcript's length when it was posted: it is looked for after.
    offset: u64,
    posted_at: i64,
    msgs: Vec<Queued>,
}

fn read_inflight(path: &Path) -> Result<Option<Inflight>> {
    let mut text = String::new();
    match open_regular(path).and_then(|mut f| f.read_to_string(&mut text)) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let inflight =
        serde_json::from_str(&text).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(inflight))
}

/// Replace `path` whole with `inflight`, readable by its owner only.
fn write_inflight(path: &Path, inflight: &Inflight) -> Result<()> {
    let tmp = path.with_extension("inflight.tmp");
    let written = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&serde_json::to_vec(inflight)?));
    written.with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Put `msgs` back in the inbox, those not already there: at their own
/// `at`, so in front of anything queued since. True when any was missing.
fn push_back(inbox: &Inbox, msgs: &[Queued]) -> Result<bool> {
    let mut queued = inbox.read()?;
    let missing: Vec<Queued> = msgs
        .iter()
        .filter(|m| !queued.contains(m))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    queued.extend(missing);
    queued.sort_by_key(|q| q.at);
    inbox.replace(&queued)?;
    Ok(true)
}

/// Push back an unseen inflight batch of `session`'s when no watcher holds
/// its lock, and it was posted [`CONFIRM_SECS`] ago or more: one younger
/// may still be waiting in the session to be delivered. One seen is let go.
/// What is still queued (a watcher killed before its take) is not queued
/// twice. True when anything was pushed back. The caller holds the lock.
pub fn recover(inbox: &Inbox, task_dir: &Path, session: &str, now: i64) -> Result<bool> {
    if !status::valid_session_id(session) || watched(task_dir, session) {
        return Ok(false);
    }
    recover_unwatched(inbox, task_dir, session, now)
}

/// [`recover`], for a session known to have no other watcher.
fn recover_unwatched(inbox: &Inbox, task_dir: &Path, session: &str, now: i64) -> Result<bool> {
    let path = wait_file(task_dir, session, "inflight");
    let Some(inflight) = read_inflight(&path)? else {
        return Ok(false);
    };
    let mut pos = inflight.offset;
    let found = seen(&inflight.transcript, &mut pos, &inflight.nonce).unwrap_or(false)
        || seen_beside(&inflight.transcript, inflight.posted_at, &inflight.nonce);
    if found {
        remove(&path)?;
        return Ok(false);
    }
    // Still all queued: the watcher died before its take, so nothing was
    // posted. Let it go now, or a Stop that takes them first would leave
    // it to put them back, delivered twice.
    let queued = inbox.read()?;
    if inflight.msgs.iter().all(|m| queued.contains(m)) {
        remove(&path)?;
        return Ok(false);
    }
    if now - inflight.posted_at < CONFIRM_SECS {
        return Ok(false);
    }
    let pushed = push_back(inbox, &inflight.msgs)?;
    remove(&path)?;
    Ok(pushed)
}

/// [`recover`] every session's inflight batch in the task, `own` (the
/// caller's, whose lock it holds) included.
fn recover_all(inbox: &Inbox, task_dir: &Path, own: &str, now: i64) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(wait_dir(task_dir)) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(session) = name.strip_suffix(".inflight") else {
            continue;
        };
        let recovered = match session == own {
            true => recover_unwatched(inbox, task_dir, session, now),
            false => recover(inbox, task_dir, session, now),
        };
        if let Err(e) = recovered {
            eprintln!("jeet inbox watch: {e:#}");
        }
    }
    Ok(())
}

/// The batch posted: the header, the messages as the Stop hook hands them
/// over, and the nonce on the last line.
pub(crate) fn batch(queue: &[Queued], nonce: &str) -> String {
    let messages: Vec<&str> = queue.iter().map(|q| q.message.as_str()).collect();
    format!(
        "{HEADER}\n\n{}\n\n{NONCE_PREFIX}{nonce}",
        messages.join(QUEUE_SEPARATOR)
    )
}

/// What [`batch`] adds to the messages.
fn batch_overhead() -> usize {
    HEADER.len() + 4 + NONCE_PREFIX.len() + NONCE_LEN
}

/// Hex, so it reads the same however the transcript escapes it.
fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..NONCE_LEN].to_string()
}

/// Write `lines` to the session's `socket`. Errors name the socket only.
pub(crate) fn post(socket: &Path, lines: &[u8]) -> Result<()> {
    // A local socket connects or fails at once; std has no timeout for it.
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    stream
        .set_write_timeout(Some(POST_TIMEOUT))
        .and_then(|()| stream.write_all(lines))
        .and_then(|()| stream.flush())
        .with_context(|| format!("post to {}", socket.display()))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Whether `nonce` is in `transcript` past `*pos`, which moves on to its
/// end. Each read starts a nonce's length short of `*pos`, so one written
/// across two reads is found. No transcript yet: not seen.
pub(crate) fn seen(transcript: &Path, pos: &mut u64, nonce: &str) -> Result<bool> {
    let mut file = match open_regular(transcript) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("read {}", transcript.display())),
    };
    let len = file.metadata()?.len();
    let from = pos
        .saturating_sub(nonce.len().saturating_sub(1) as u64)
        .min(len);
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(from))
        .and_then(|_| file.take(len - from).read_to_end(&mut tail))
        .with_context(|| format!("read {}", transcript.display()))?;
    *pos = from + tail.len() as u64;
    Ok(contains(&tail, nonce.as_bytes()))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `nonce` is in the last [`BESIDE_TAIL`] of a transcript in
/// `transcript`'s directory modified since `since`: after `/clear` the same
/// process, socket and all, records the next conversation in another.
pub(crate) fn seen_beside(transcript: &Path, since: i64, nonce: &str) -> bool {
    let Some(entries) = transcript.parent().and_then(|d| std::fs::read_dir(d).ok()) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            return false;
        }
        let Ok(mut file) = open_regular(&path) else {
            return false;
        };
        let Ok(meta) = file.metadata() else {
            return false;
        };
        if crate::agent::mtime_secs(&meta).is_none_or(|m| m < since) {
            return false;
        }
        let mut tail = Vec::new();
        let from = meta.len().saturating_sub(BESIDE_TAIL);
        file.seek(SeekFrom::Start(from))
            .and_then(|_| file.read_to_end(&mut tail))
            .is_ok_and(|_| contains(&tail, nonce.as_bytes()))
    })
}

/// What the watcher does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Wait,
    Exit(Why),
}

/// Why the watcher ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The agent's process is gone.
    Gone,
    /// Its session ended (`/clear`, `/exit`).
    Ended,
    /// A post failed, or was never seen: dropped by the session.
    Refused,
}

/// The batch in flight, as the watcher keeps it.
struct Flight {
    record: Inflight,
    /// How far the transcript has been read.
    pos: u64,
    /// Seconds the session has sat idle since the post.
    waited: i64,
    last: i64,
    /// The `at` of the status the watcher wrote for it, if it did.
    wake_at: Option<i64>,
}

/// One session's watcher.
pub struct Watcher<'a> {
    app: &'a App,
    task: TaskRecord,
    task_dir: PathBuf,
    env: WakeEnv,
    start: Start,
    /// The agent's process's start time when the watcher began.
    s0: i64,
    /// When the watcher began: an `Ended` older than this is a previous run
    /// of the same session's, which a resume has yet to overwrite.
    born: i64,
    lock: File,
    /// When each post in the window was made.
    posts: VecDeque<i64>,
    flight: Option<Flight>,
    /// The inbox file as last seen: inode, length, mtime.
    last_inbox: Option<(u64, u64, SystemTime)>,
    /// Something is queued for the session that was not taken.
    pending: bool,
    /// Held when last asked: not asked again until then.
    held_until: i64,
    tick: u64,
}

impl<'a> Watcher<'a> {
    /// The watcher of `start`'s session of `task`: `None` when the session
    /// is off, its agent's process cannot be read, or another watcher holds
    /// its lock. Recovers what earlier watchers left in flight.
    pub fn open(
        app: &'a App,
        task: TaskRecord,
        env: WakeEnv,
        start: Start,
        now: i64,
    ) -> Result<Option<Self>> {
        Self::open_retrying(app, task, env, start, now, LOCK_RETRY)
    }

    fn open_retrying(
        app: &'a App,
        task: TaskRecord,
        env: WakeEnv,
        start: Start,
        now: i64,
        retry: Duration,
    ) -> Result<Option<Self>> {
        if !status::valid_session_id(&start.session) {
            return Ok(None);
        }
        let task_dir = tasks::task_dir(app, &task);
        if wait_file(&task_dir, &start.session, "off").exists() {
            return Ok(None);
        }
        let Some(s0) = identity::start_of(env.claude_pid) else {
            return Ok(None);
        };
        let dir = wait_dir(&task_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = wait_file(&task_dir, &start.session, "lock");
        let Some(mut lock) = lock_within(&path, LOCK_TRIES, retry)? else {
            return Ok(None);
        };
        // For whoever looks; never the token.
        let note = serde_json::json!({
            "claude_pid": env.claude_pid,
            "start": s0,
            "pid": std::process::id(),
        });
        if let Err(e) = lock
            .set_len(0)
            .and_then(|()| lock.write_all(note.to_string().as_bytes()))
        {
            eprintln!("jeet inbox watch: write {}: {e}", path.display());
        }
        let inbox = Inbox::lock(&task_dir)?;
        recover_all(&inbox, &task_dir, &start.session, now)?;
        // One of its own too young to recover is carried on with.
        let flight =
            read_inflight(&wait_file(&task_dir, &start.session, "inflight"))?.map(|record| {
                Flight {
                    pos: record.offset,
                    record,
                    waited: 0,
                    last: now,
                    wake_at: None,
                }
            });
        drop(inbox);
        Ok(Some(Self {
            app,
            task,
            task_dir,
            env,
            start,
            s0,
            born: now,
            lock,
            posts: VecDeque::new(),
            flight,
            last_inbox: None,
            pending: true,
            held_until: i64::MIN,
            tick: 0,
        }))
    }

    fn inflight_path(&self) -> PathBuf {
        wait_file(&self.task_dir, &self.start.session, "inflight")
    }

    fn status(&self) -> Option<status::SessionStatus> {
        status::read_status(&self.task_dir, &self.start.session)
    }

    /// Whether `status` is this run's end of the session: a resumed session
    /// keeps its id, and its old `Ended` stands until its SessionStart hook
    /// writes over it, which can come after the watcher's first look.
    fn ended(&self, status: Option<&status::SessionStatus>) -> bool {
        status.is_some_and(|s| s.state == State::Ended && s.at >= self.born)
    }

    /// Whether the inbox file changed since last asked.
    fn inbox_changed(&mut self) -> bool {
        let seen = std::fs::metadata(messages::inbox_path(&self.task_dir))
            .ok()
            .map(|m| {
                (
                    m.ino(),
                    m.len(),
                    m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            });
        let changed = seen != self.last_inbox;
        self.last_inbox = seen;
        changed
    }

    /// Whether the window holds [`WAKE_CAP`] posts already.
    fn capped(&mut self, now: i64) -> bool {
        while self
            .posts
            .front()
            .is_some_and(|&at| now - at >= WAKE_WINDOW_SECS)
        {
            self.posts.pop_front();
        }
        self.posts.len() >= WAKE_CAP
    }

    /// One tick at `now`: look for the batch in flight, else take and post
    /// what waits, and say whether to carry on.
    pub fn step(&mut self, now: i64) -> Result<Next> {
        let tick = self.tick;
        self.tick += 1;
        if identity::start_of(self.env.claude_pid) != Some(self.s0) {
            return Ok(Next::Exit(Why::Gone));
        }
        if tick.is_multiple_of(STATUS_EVERY) && self.ended(self.status().as_ref()) {
            return Ok(Next::Exit(Why::Ended));
        }
        let born = self.born;
        if let Some(flight) = &mut self.flight {
            let record = &flight.record;
            let found = seen(&record.transcript, &mut flight.pos, &record.nonce)
                .inspect_err(|e| eprintln!("jeet inbox watch: {e:#}"))
                .unwrap_or(false);
            if !found {
                let status = status::read_status(&self.task_dir, &self.start.session);
                if status
                    .as_ref()
                    .is_some_and(|s| s.state == State::Ended && s.at >= born)
                {
                    return Ok(Next::Exit(Why::Ended));
                }
                // Only while it sits idle: a post waits behind a prompt, or
                // for the next tool call, while it is busy.
                let idle = status.as_ref().is_none_or(|s| {
                    s.state == State::Idle
                        || (s.state == State::Running && s.event == status::WAKE_EVENT)
                });
                if idle {
                    flight.waited += (now - flight.last).max(0);
                }
                flight.last = now;
                return Ok(match flight.waited >= CONFIRM_SECS {
                    true => Next::Exit(Why::Refused),
                    false => Next::Wait,
                });
            }
            remove(&self.inflight_path())?;
            self.flight = None;
            self.pending = true;
        }
        if self.inbox_changed() {
            self.pending = true;
        }
        if !self.pending || now < self.held_until || self.capped(now) {
            return Ok(Next::Wait);
        }
        self.take_and_post(now)
    }

    /// Under the inbox lock: take what is for the session, unless it is
    /// held, waiting on a prompt, ended or not the task's current session,
    /// and post it.
    fn take_and_post(&mut self, now: i64) -> Result<Next> {
        let session = self.start.session.clone();
        let inbox = Inbox::lock(&self.task_dir)?;
        let queue: Vec<Queued> = inbox
            .read_for(Some(&session))?
            .into_iter()
            .filter(wakes)
            .collect();
        self.pending = !queue.is_empty();
        if queue.is_empty() {
            return Ok(Next::Wait);
        }
        if run::hold(self.app, &self.task)?.is_some() {
            self.held_until = now + HOLD_RECHECK_SECS;
            return Ok(Next::Wait);
        }
        let Some(status) = self.status() else {
            return Ok(Next::Wait);
        };
        match status.state {
            State::Ended if self.ended(Some(&status)) => return Ok(Next::Exit(Why::Ended)),
            // The last run's: the hook that says this one's has yet to run.
            State::Ended => return Ok(Next::Wait),
            // A post while it waits on a prompt starts a turn once the
            // prompt goes, even when a pause's Esc is what made it go.
            State::Waiting => return Ok(Next::Wait),
            State::Idle | State::Running => {}
        }
        if !self.current() {
            return Ok(Next::Wait);
        }
        let n = messages::fitting_within(&queue, MAX_MESSAGE - batch_overhead(), QUEUE_SEPARATOR);
        let nonce = new_nonce();
        let text = batch(&queue[..n], &nonce);
        let record = Inflight {
            nonce,
            transcript: self.start.transcript.clone(),
            offset: std::fs::metadata(&self.start.transcript).map_or(0, |m| m.len()),
            posted_at: now,
            msgs: queue[..n].to_vec(),
        };
        // Before the take: a watcher killed between the two leaves the batch
        // both queued and in flight, which `recover` puts right.
        write_inflight(&self.inflight_path(), &record)?;
        let mut rest = inbox.read_for(Some(&session))?;
        for taken in &queue[..n] {
            if let Some(i) = rest.iter().position(|q| q == taken) {
                rest.remove(i);
            }
        }
        inbox.replace_for(Some(&session), &rest)?;
        if let Err(e) = status::mark_typed(&self.task_dir) {
            eprintln!("jeet inbox watch: {e:#}");
        }
        let wake_at = match status.state {
            State::Idle => match status::wake_running(&self.task_dir, &session, now) {
                Ok(()) => Some(now),
                Err(e) => {
                    eprintln!("jeet inbox watch: {e:#}");
                    None
                }
            },
            _ => None,
        };
        self.posts.push_back(now);
        self.flight = Some(Flight {
            pos: record.offset,
            record,
            waited: 0,
            last: now,
            wake_at,
        });
        let lines = (self.env.lines)(&self.env.token, &text);
        if let Err(e) = post(&self.env.socket, &lines) {
            eprintln!("jeet inbox watch: {e:#}");
            return Ok(Next::Exit(Why::Refused));
        }
        Ok(Next::Wait)
    }

    /// Whether the session is the one that speaks for the task: after a
    /// `/clear`, the old one is not. Each of the assistant's sessions is
    /// its own.
    fn current(&self) -> bool {
        self.task.kind == TaskKind::Assistant
            || status::current_status(&self.task_dir)
                .is_some_and(|s| s.session_id == self.start.session)
    }

    /// End the watcher, under the inbox lock: an unseen batch in flight
    /// goes back to the front of the queue, and the lock is let go before
    /// the inbox's, so a delivery either found it watched with its message
    /// queued before this, or finds it unwatched. Refused, the session is
    /// marked off and, when it is idle, not held and the task's current
    /// one, what is queued is typed into `agent`. Gone or ended, nothing is
    /// typed: the queue waits for the next session.
    pub fn close(self, why: Why, agent: &dyn Agent) -> Result<()> {
        let Watcher {
            app,
            task,
            task_dir,
            start,
            lock,
            flight,
            ..
        } = self;
        let session = start.session.as_str();
        let inbox = Inbox::lock(&task_dir)?;
        // A take that failed after writing its batch: still queued, and
        // typed below, so not to be put back later.
        if flight.is_none() {
            remove(&wait_file(&task_dir, session, "inflight"))?;
        }
        if let Some(mut flight) = flight {
            let record = &flight.record;
            let found = seen(&record.transcript, &mut flight.pos, &record.nonce).unwrap_or(false)
                || (why == Why::Ended
                    && seen_beside(&record.transcript, record.posted_at, &record.nonce));
            if !found {
                let msgs = match (why, task.kind) {
                    (Why::Ended, TaskKind::Assistant) => {
                        readdressed(app, &task, session, &record.msgs)
                    }
                    _ => record.msgs.clone(),
                };
                push_back(&inbox, &msgs)?;
            }
            remove(&wait_file(&task_dir, session, "inflight"))?;
            if let (Why::Refused, Some(at)) = (why, flight.wake_at) {
                status::revert_wake(&task_dir, session, at)?;
            }
        }
        if why == Why::Refused {
            let off = wait_file(&task_dir, session, "off");
            std::fs::write(&off, b"").with_context(|| format!("write {}", off.display()))?;
        }
        drop(lock);
        if why != Why::Refused || run::hold(app, &task)?.is_some() {
            return Ok(());
        }
        let idle = status::read_status(&task_dir, session).is_some_and(|s| s.state == State::Idle);
        let current = task.kind == TaskKind::Assistant
            || status::current_status(&task_dir).is_some_and(|s| s.session_id == session);
        if idle && current {
            messages::hand_over_queue(&inbox, agent)?;
        }
        Ok(())
    }
}

/// `msgs` of the assistant's ended `session`, readdressed as its session
/// start readdresses the queue: to the session that follows on from it, if
/// one is recorded.
fn readdressed(app: &App, task: &TaskRecord, session: &str, msgs: &[Queued]) -> Vec<Queued> {
    let next = app
        .db
        .list_task_sessions(&task.id)
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|s| s.parent_id.as_deref() == Some(session))
        });
    let Some(next) = next else {
        return msgs.to_vec();
    };
    msgs.iter()
        .map(|q| match q.to.as_deref() == Some(session) {
            true => Queued {
                to: Some(next.session_id.clone()),
                ..q.clone()
            },
            false => q.clone(),
        })
        .collect()
}

/// The lock at `path`, made 0600 if need be, tried `tries` times `retry`
/// apart; `None` while another holds it.
fn lock_within(path: &Path, tries: u32, retry: Duration) -> Result<Option<File>> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    for attempt in 0..tries {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
        if attempt + 1 < tries {
            std::thread::sleep(retry);
        }
    }
    Ok(None)
}

/// `jeet inbox watch --task <id>`, under `harness`'s session-start hook:
/// read the hook's stdin, check this runs under that session's agent and
/// as `task_id`'s, then watch until the agent goes, its session ends or a
/// post is refused. Prints nothing; anything wrong ends it quietly.
pub fn run(harness: &dyn Harness, task_id: &str, env: WakeEnv) -> Result<()> {
    let mut raw = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin()
            .take(status::MAX_INPUT)
            .read_to_string(&mut raw);
    }
    let hook = harness.parse_hook("SessionStart", &raw)?;
    let (Some(session), Some(transcript)) = (hook.session_id, hook.transcript) else {
        return Ok(());
    };
    // Its agent's, through whatever shell ran the hook.
    if !identity::runs_under_any(&[env.claude_pid]) {
        return Ok(());
    }
    let app = App::open()?;
    let Some(task) = app.db.get_task(task_id)? else {
        return Ok(());
    };
    // Not another task's agent posting this one's messages to itself.
    let me = identity::identify(&app)?;
    if me.task.is_none_or(|t| t.id != task.id) {
        return Ok(());
    }
    let start = Start {
        session,
        transcript,
    };
    let Some(mut watcher) = Watcher::open(&app, task.clone(), env, start.clone(), now_secs())?
    else {
        return Ok(());
    };
    let why = loop {
        match watcher.step(now_secs()) {
            Ok(Next::Wait) => std::thread::sleep(TICK),
            Ok(Next::Exit(why)) => break why,
            // Today's path takes over.
            Err(e) => {
                eprintln!("jeet inbox watch: {e:#}");
                break Why::Refused;
            }
        }
    };
    let agent = status::idle_agent(&app, &task, &start.session);
    watcher.close(why, &*agent)
}

/// For other modules' tests: `session`'s wait lock, held as a watcher
/// holds it.
#[cfg(test)]
pub(crate) fn held_for_test(task_dir: &Path, session: &str) -> File {
    std::fs::create_dir_all(wait_dir(task_dir)).unwrap();
    lock_within(&wait_file(task_dir, session, "lock"), 1, Duration::ZERO)
        .unwrap()
        .unwrap()
}

/// For other modules' tests: `msgs` posted to `session` at `posted_at`, to
/// be looked for in `transcript` (made empty), and not seen there yet.
#[cfg(test)]
pub(crate) fn left_in_flight_for_test(
    task_dir: &Path,
    session: &str,
    transcript: &Path,
    msgs: Vec<Queued>,
    posted_at: i64,
) {
    std::fs::create_dir_all(wait_dir(task_dir)).unwrap();
    std::fs::write(transcript, "").unwrap();
    let inflight = Inflight {
        nonce: new_nonce(),
        transcript: transcript.to_path_buf(),
        offset: 0,
        posted_at,
        msgs,
    };
    write_inflight(&wait_file(task_dir, session, "inflight"), &inflight).unwrap();
}

/// For other modules' tests: whether `session` has a batch in flight.
#[cfg(test)]
pub(crate) fn in_flight_for_test(task_dir: &Path, session: &str) -> bool {
    wait_file(task_dir, session, "inflight").exists()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::config;
    use crate::db::tasks::{RunState, Setter, TaskState};
    use crate::db::Database;
    use crate::harness::claude::wake_lines;
    use crate::manager::run::StartGuard;
    use crate::runtime::Started;
    use crate::status::SessionStatus;

    /// An index in a tempdir with worker `w` under the assistant.
    struct Lab {
        _home: tempfile::TempDir,
        app: App,
        /// Holds the fake socket: `/tmp`, as a socket's path is short.
        sock_dir: tempfile::TempDir,
    }

    impl Lab {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().canonicalize().unwrap();
            let app = App {
                config: config::load_or_create(&path).unwrap(),
                db: Database::open(&config::index_path(&path)).unwrap(),
                home: path,
            };
            let root = crate::manager::root(&app).unwrap();
            app.db
                .insert_task(&TaskRecord {
                    id: "w".into(),
                    slug: "w".into(),
                    name: "w".into(),
                    project: None,
                    kind: TaskKind::Work,
                    state: TaskState::Active,
                    created_at: 0,
                    last_activity: 0,
                    parent_id: Some(root.id),
                    placement: crate::placement::for_new(),
                    run_state: RunState::Run,
                    run_state_by: None,
                    max_children: None,
                })
                .unwrap();
            let sock_dir = tempfile::Builder::new()
                .prefix("jw")
                .tempdir_in("/tmp")
                .unwrap();
            let lab = Lab {
                _home: home,
                app,
                sock_dir,
            };
            std::fs::create_dir_all(lab.dir().join(".jeet")).unwrap();
            std::fs::create_dir_all(lab.projects()).unwrap();
            lab
        }

        fn task(&self) -> TaskRecord {
            self.app.db.get_task("w").unwrap().unwrap()
        }

        fn dir(&self) -> PathBuf {
            tasks::task_dir(&self.app, &self.task())
        }

        fn projects(&self) -> PathBuf {
            self.app.home.join("projects")
        }

        fn transcript(&self) -> PathBuf {
            self.projects().join("s.jsonl")
        }

        fn socket(&self) -> PathBuf {
            self.sock_dir.path().join("s.sock")
        }

        fn listen(&self) -> UnixListener {
            let listener = UnixListener::bind(self.socket()).unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        }

        fn env(&self, pid: u32) -> WakeEnv {
            WakeEnv {
                socket: self.socket(),
                token: SECRET.to_string(),
                claude_pid: pid,
                lines: wake_lines,
            }
        }

        fn start(&self) -> Start {
            Start {
                session: "s".into(),
                transcript: self.transcript(),
            }
        }

        fn watcher(&self) -> Watcher<'_> {
            self.watcher_for(std::process::id())
        }

        fn watcher_for(&self, pid: u32) -> Watcher<'_> {
            std::fs::write(self.transcript(), b"{\"type\":\"start\"}\n").unwrap();
            Watcher::open_retrying(
                &self.app,
                self.task(),
                self.env(pid),
                self.start(),
                0,
                Duration::ZERO,
            )
            .unwrap()
            .unwrap()
        }

        fn send(&self, message: &str) {
            Inbox::lock(&self.dir()).unwrap().push(message).unwrap();
        }

        fn queued(&self) -> Vec<Queued> {
            Inbox::lock(&self.dir()).unwrap().read().unwrap()
        }

        fn status(&self, state: State, event: &str, at: i64) {
            status_of(&self.dir(), "s", state, event, at);
        }

        fn state(&self) -> SessionStatus {
            status::read_status(&self.dir(), "s").unwrap()
        }

        fn inflight(&self) -> Option<Inflight> {
            read_inflight(&wait_file(&self.dir(), "s", "inflight")).unwrap()
        }

        /// Say in the transcript what was posted, as the session does.
        fn deliver(&self) {
            let nonce = self.inflight().unwrap().nonce;
            let mut f = OpenOptions::new()
                .append(true)
                .open(self.transcript())
                .unwrap();
            writeln!(f, "{{\"content\":\"... jeet delivery {nonce}\"}}").unwrap();
        }
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn status_of(dir: &Path, session: &str, state: State, event: &str, at: i64) {
        status::write_status(
            dir,
            &SessionStatus {
                session_id: session.into(),
                state,
                detail: None,
                event: event.into(),
                at,
                busy_since: None,
            },
        )
        .unwrap();
    }

    /// What one post wrote, if one was made.
    fn received(listener: &UnixListener) -> Option<String> {
        let (mut stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => panic!("{e}"),
        };
        stream.set_nonblocking(false).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        Some(text)
    }

    /// The batch a post carried.
    fn content(posted: &str) -> String {
        let user: serde_json::Value = serde_json::from_str(posted.lines().nth(1).unwrap()).unwrap();
        user["message"]["content"].as_str().unwrap().to_string()
    }

    /// An agent that records what is typed into it.
    #[derive(Default)]
    struct Typist {
        typed: RefCell<Vec<String>>,
    }

    impl Agent for Typist {
        fn alive(&self) -> bool {
            true
        }
        fn has_hooks(&self) -> bool {
            true
        }
        fn type_in(&self, text: &str) -> Result<()> {
            self.typed.borrow_mut().push(text.to_string());
            Ok(())
        }
        fn start(&self, _: &str, _: Option<&StartGuard>) -> Result<Started> {
            anyhow::bail!("the fake starts nothing")
        }
    }

    fn queued(at: i64, message: &str) -> Queued {
        Queued {
            at,
            message: message.into(),
            to: None,
        }
    }

    #[test]
    fn a_batch_is_the_header_the_messages_and_the_nonce() {
        let text = batch(&[queued(1, "one"), queued(2, "two")], "abc");
        assert_eq!(
            text,
            format!("{HEADER}\n\none\n\n---\n\ntwo\n\njeet delivery abc")
        );
        assert!(text.starts_with("jeet:"));
        // As many as fit, with what the batch adds, in a message's limit.
        let queue: Vec<Queued> = (0..9).map(|i| queued(i, &"x".repeat(1_500))).collect();
        let n = messages::fitting_within(&queue, MAX_MESSAGE - batch_overhead(), QUEUE_SEPARATOR);
        assert!(n > 1 && n < queue.len());
        let text = batch(&queue[..n], &new_nonce());
        assert!(text.len() <= MAX_MESSAGE, "{}", text.len());
        // One of a message's limit goes alone, nonce and all.
        let queue = [queued(1, &"y".repeat(MAX_MESSAGE)), queued(2, "z")];
        let n = messages::fitting_within(&queue, MAX_MESSAGE - batch_overhead(), QUEUE_SEPARATOR);
        assert_eq!(n, 1);
        let nonce = new_nonce();
        let text = batch(&queue[..n], &nonce);
        assert_eq!(text.len(), MAX_MESSAGE + batch_overhead());
        assert!(text.ends_with(&format!("\njeet delivery {nonce}")));
        assert_eq!(nonce.len(), NONCE_LEN);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_post_is_the_auth_line_then_the_message_exactly() {
        let lab = Lab::new();
        let listener = UnixListener::bind(lab.socket()).unwrap();
        let body = "say \"hi\"\n<jeet-report>\n\\ end ✓".repeat(500);
        let reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            text
        });
        post(&lab.socket(), &wake_lines(SECRET, &body)).unwrap();
        let text = reader.join().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let auth: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(auth, serde_json::json!({ "type": "auth", "token": SECRET }));
        let user: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(user["type"], "user");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(user["message"]["content"].as_str().unwrap(), body);
    }

    #[test]
    fn a_post_to_no_socket_fails_naming_only_the_socket() {
        let lab = Lab::new();
        let e = post(&lab.socket(), &wake_lines(SECRET, "hi")).unwrap_err();
        let said = format!("{e:#}");
        assert!(said.contains("s.sock"), "{said}");
        assert!(!said.contains(SECRET));
    }

    #[test]
    fn the_nonce_is_looked_for_past_where_the_last_read_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let nonce = "0123456789abcdef";
        std::fs::write(&path, format!("old {nonce}\n")).unwrap();
        // Before the offset: not this post's.
        let mut pos = std::fs::metadata(&path).unwrap().len();
        assert!(!seen(&path, &mut pos, nonce).unwrap());
        // Written in two halves, read in between.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "new {}", &nonce[..7]).unwrap();
        assert!(!seen(&path, &mut pos, nonce).unwrap());
        assert_eq!(pos, std::fs::metadata(&path).unwrap().len());
        write!(f, "{} end", &nonce[7..]).unwrap();
        assert!(seen(&path, &mut pos, nonce).unwrap());
        // Read once: only what comes after counts.
        assert!(!seen(&path, &mut pos, nonce).unwrap());
        // No transcript yet.
        let mut pos = 0;
        assert!(!seen(&dir.path().join("none.jsonl"), &mut pos, nonce).unwrap());
    }

    #[test]
    fn a_nonce_beside_counts_in_a_transcript_written_since_the_post() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("a.jsonl");
        std::fs::write(&own, "nothing").unwrap();
        let nonce = "fedcba9876543210";
        let next = dir.path().join("b.jsonl");
        std::fs::write(&next, format!("x jeet delivery {nonce}")).unwrap();
        let now = now_secs();
        assert!(seen_beside(&own, now - 5, nonce));
        // One last written before the post is another conversation's.
        let old = SystemTime::now() - Duration::from_secs(600);
        File::options()
            .write(true)
            .open(next)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(!seen_beside(&own, now - 5, nonce));
    }

    #[test]
    fn watched_means_a_lock_someone_holds_and_never_makes_one() {
        let lab = Lab::new();
        let dir = lab.dir();
        assert!(!watched(&dir, "s"));
        assert!(!wait_dir(&dir).exists());
        assert!(!watched(&dir, "../s"));
        std::fs::create_dir_all(wait_dir(&dir)).unwrap();
        let held = lock_within(&wait_file(&dir, "s", "lock"), 1, Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(watched(&dir, "s"));
        drop(held);
        assert!(!watched(&dir, "s"));
    }

    #[test]
    fn a_starting_watcher_waits_out_a_probe_but_not_a_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.lock");
        let probe = lock_within(&path, 1, Duration::ZERO).unwrap().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(probe);
        });
        assert!(lock_within(&path, LOCK_TRIES, LOCK_RETRY)
            .unwrap()
            .is_some());
        release.join().unwrap();
        let _watcher = lock_within(&path, 1, Duration::ZERO).unwrap().unwrap();
        assert!(lock_within(&path, LOCK_TRIES, Duration::ZERO)
            .unwrap()
            .is_none());
    }

    #[test]
    fn one_watcher_per_session_and_none_once_it_is_off() {
        let lab = Lab::new();
        let first = lab.watcher();
        let again = Watcher::open_retrying(
            &lab.app,
            lab.task(),
            lab.env(std::process::id()),
            lab.start(),
            0,
            Duration::ZERO,
        )
        .unwrap();
        assert!(again.is_none());
        drop(first);
        std::fs::write(wait_file(&lab.dir(), "s", "off"), b"").unwrap();
        let off = Watcher::open_retrying(
            &lab.app,
            lab.task(),
            lab.env(std::process::id()),
            lab.start(),
            0,
            Duration::ZERO,
        )
        .unwrap();
        assert!(off.is_none());
    }

    #[test]
    fn the_token_is_in_no_file_and_not_in_debug() {
        let lab = Lab::new();
        assert!(!format!("{:?}", lab.env(1)).contains(SECRET));
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        lab.send("hi");
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        assert!(received(&listener).unwrap().contains(SECRET));
        for entry in std::fs::read_dir(wait_dir(&lab.dir())).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!text.contains(SECRET), "{text}");
        }
    }

    #[test]
    fn a_queued_message_is_posted_taken_and_confirmed() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        assert_eq!(w.step(5).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        lab.send("hello <there>");
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        let posted = content(&received(&listener).unwrap());
        assert!(posted.starts_with(HEADER));
        assert!(posted.contains("\n\nhello <there>\n\n"));
        assert!(lab.queued().is_empty());
        let inflight = lab.inflight().unwrap();
        assert!(posted.ends_with(&format!("jeet delivery {}", inflight.nonce)));
        assert_eq!(inflight.msgs.len(), 1);
        assert_eq!(
            inflight.offset,
            std::fs::metadata(lab.transcript()).unwrap().len()
        );
        // Idle, it reads as running from now, by the wake channel.
        let state = lab.state();
        assert_eq!(
            (state.state, state.event.as_str(), state.busy_since),
            (State::Running, status::WAKE_EVENT, Some(10))
        );
        assert!(lab.dir().join(".jeet/typed").exists());
        lab.deliver();
        assert_eq!(w.step(11).unwrap(), Next::Wait);
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn only_one_batch_is_in_flight() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Running, "PreToolUse", 1);
        let mut w = lab.watcher();
        lab.send("one");
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        // Busy: its status is the hook's, not the wake's.
        assert_eq!(lab.state().event, "PreToolUse");
        lab.send("two");
        assert_eq!(w.step(11).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
        // Seen: the next goes at once.
        lab.deliver();
        w.step(12).unwrap();
        assert!(content(&received(&listener).unwrap()).contains("two"));
        assert!(lab.queued().is_empty());
    }

    #[test]
    fn nothing_is_taken_while_it_is_held_or_waiting_on_a_prompt() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        lab.app
            .db
            .set_run_state("w", RunState::Paused, Some(&Setter::User))
            .unwrap();
        lab.send("hi");
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
        lab.app.db.set_run_state("w", RunState::Run, None).unwrap();
        // The hold is not read again for a while.
        assert_eq!(w.step(12).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        lab.status(State::Waiting, "Notification", 13);
        assert_eq!(w.step(16).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
        lab.status(State::Idle, "Stop", 17);
        w.step(17).unwrap();
        assert!(received(&listener).is_some());
        assert!(lab.queued().is_empty());
    }

    #[test]
    fn a_session_not_the_tasks_current_one_takes_nothing() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        status_of(&lab.dir(), "t", State::Idle, "SessionStart", 2);
        let mut w = lab.watcher();
        lab.send("hi");
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
    }

    #[test]
    fn a_message_for_another_session_is_not_taken() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        Inbox::lock(&lab.dir())
            .unwrap()
            .push_to("for t", Some("t"))
            .unwrap();
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
    }

    #[test]
    fn posts_are_capped_per_minute() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Running, "PreToolUse", 1);
        let mut w = lab.watcher();
        for i in 0..WAKE_CAP as i64 {
            lab.send(&format!("m{i}"));
            w.step(10 + i).unwrap();
            assert!(received(&listener).is_some(), "post {i}");
            lab.deliver();
        }
        lab.send("over");
        assert_eq!(w.step(30).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        assert_eq!(lab.queued().len(), 1);
        assert_eq!(w.step(71).unwrap(), Next::Wait);
        assert!(content(&received(&listener).unwrap()).contains("over"));
    }

    #[test]
    fn a_post_never_seen_while_idle_is_refused_and_falls_back() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        Inbox::lock(&lab.dir())
            .unwrap()
            .replace(&[queued(3, "first")])
            .unwrap();
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        lab.send("later");
        assert_eq!(w.step(29).unwrap(), Next::Wait);
        assert_eq!(w.step(30).unwrap(), Next::Exit(Why::Refused));
        let typist = Typist::default();
        w.close(Why::Refused, &typist).unwrap();
        // Back in front, at its own time, then typed in with the rest.
        assert_eq!(*typist.typed.borrow(), ["first  later"]);
        assert!(lab.queued().is_empty());
        let state = lab.state();
        assert_eq!((state.state, state.busy_since), (State::Idle, None));
        assert!(wait_file(&lab.dir(), "s", "off").exists());
        assert!(lab.inflight().is_none());
        assert!(!watched(&lab.dir(), "s"));
    }

    #[test]
    fn a_refusal_types_nothing_into_a_held_or_busy_agent() {
        for busy in [false, true] {
            let lab = Lab::new();
            let listener = lab.listen();
            lab.status(State::Idle, "SessionStart", 1);
            let mut w = lab.watcher();
            Inbox::lock(&lab.dir())
                .unwrap()
                .replace(&[queued(3, "first")])
                .unwrap();
            w.step(10).unwrap();
            assert!(received(&listener).is_some());
            assert_eq!(w.step(30).unwrap(), Next::Exit(Why::Refused));
            match busy {
                // A hook since: never overwritten by the revert.
                true => lab.status(State::Running, "UserPromptSubmit", 31),
                false => lab
                    .app
                    .db
                    .set_run_state("w", RunState::Paused, Some(&Setter::User))
                    .unwrap(),
            }
            let typist = Typist::default();
            w.close(Why::Refused, &typist).unwrap();
            assert!(typist.typed.borrow().is_empty(), "busy {busy}");
            assert_eq!(lab.queued(), [queued(3, "first")]);
            let state = lab.state();
            match busy {
                true => assert_eq!(state.event, "UserPromptSubmit"),
                false => assert_eq!(
                    (state.state, state.event.as_str()),
                    (State::Idle, status::WAKE_EVENT)
                ),
            }
        }
    }

    #[test]
    fn the_deadline_counts_only_the_time_it_sits_idle() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Running, "PreToolUse", 1);
        let mut w = lab.watcher();
        lab.send("hi");
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        // At a permission prompt, the post waits behind it.
        lab.status(State::Waiting, "Notification", 11);
        for t in (11..=70).step_by(5) {
            assert_eq!(w.step(t).unwrap(), Next::Wait, "at {t}");
        }
        lab.status(State::Idle, "Stop", 71);
        assert_eq!(w.step(80).unwrap(), Next::Wait);
        assert_eq!(w.step(90).unwrap(), Next::Exit(Why::Refused));
    }

    #[test]
    fn a_post_that_cannot_connect_is_refused_at_once() {
        let lab = Lab::new();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        lab.send("hi");
        assert_eq!(w.step(10).unwrap(), Next::Exit(Why::Refused));
        let typist = Typist::default();
        w.close(Why::Refused, &typist).unwrap();
        assert_eq!(*typist.typed.borrow(), ["hi"]);
        assert!(lab.queued().is_empty());
    }

    #[test]
    fn an_ended_session_takes_nothing_and_types_nothing() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        w.step(5).unwrap();
        lab.status(State::Ended, "SessionEnd", 6);
        lab.send("hi");
        assert_eq!(w.step(7).unwrap(), Next::Exit(Why::Ended));
        assert!(received(&listener).is_none());
        let typist = Typist::default();
        w.close(Why::Ended, &typist).unwrap();
        assert!(typist.typed.borrow().is_empty());
        assert_eq!(lab.queued().len(), 1);
        assert!(!wait_file(&lab.dir(), "s", "off").exists());
        assert!(!watched(&lab.dir(), "s"));
    }

    #[test]
    fn the_last_runs_end_of_a_resumed_session_is_not_this_ones() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.send("hi");
        // Its id kept, and the SessionEnd of the run before still there.
        lab.status(State::Ended, "SessionEnd", 5);
        let mut w = Watcher::open_retrying(
            &lab.app,
            lab.task(),
            lab.env(std::process::id()),
            lab.start(),
            10,
            Duration::ZERO,
        )
        .unwrap()
        .unwrap();
        assert_eq!(w.step(10).unwrap(), Next::Wait);
        assert!(received(&listener).is_none());
        // Its hook writes over it, and the message goes.
        lab.status(State::Idle, "SessionStart", 11);
        w.pending = true;
        assert_eq!(w.step(11).unwrap(), Next::Wait);
        assert!(received(&listener).is_some());
        // An end since is this run's.
        lab.status(State::Ended, "SessionEnd", 12);
        w.tick = 0;
        assert_eq!(w.step(12).unwrap(), Next::Exit(Why::Ended));
    }

    #[test]
    fn an_end_finds_a_batch_the_next_conversation_got() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut w = lab.watcher();
        lab.send("hi");
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        let nonce = lab.inflight().unwrap().nonce;
        // `/clear`: the same process records the next one elsewhere.
        std::fs::write(
            lab.projects().join("t.jsonl"),
            format!("jeet delivery {nonce}"),
        )
        .unwrap();
        lab.status(State::Ended, "SessionEnd", 11);
        assert_eq!(w.step(11).unwrap(), Next::Exit(Why::Ended));
        w.close(Why::Ended, &Typist::default()).unwrap();
        assert!(lab.queued().is_empty());
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn a_gone_agent_ends_the_watcher_with_its_batch_back() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut w = lab.watcher_for(sleeper.id());
        lab.send("hi");
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(w.step(11).unwrap(), Next::Exit(Why::Gone));
        let typist = Typist::default();
        w.close(Why::Gone, &typist).unwrap();
        assert!(typist.typed.borrow().is_empty());
        assert_eq!(lab.queued().len(), 1);
        assert!(lab.inflight().is_none());
        assert!(!watched(&lab.dir(), "s"));
    }

    #[test]
    fn the_assistants_lost_batch_goes_to_the_session_after_a_clear() {
        let lab = Lab::new();
        let listener = lab.listen();
        let root = crate::manager::root(&lab.app).unwrap();
        assert_eq!(root.kind, TaskKind::Assistant);
        let dir = tasks::task_dir(&lab.app, &root);
        std::fs::create_dir_all(dir.join(".jeet")).unwrap();
        status_of(&dir, "s", State::Idle, "SessionStart", 1);
        std::fs::write(lab.transcript(), b"").unwrap();
        let mut w = Watcher::open_retrying(
            &lab.app,
            root.clone(),
            lab.env(std::process::id()),
            lab.start(),
            0,
            Duration::ZERO,
        )
        .unwrap()
        .unwrap();
        Inbox::lock(&dir).unwrap().push_to("hi", Some("s")).unwrap();
        w.step(10).unwrap();
        assert!(received(&listener).is_some());
        // `/clear`: session t follows on from s, and never got the batch.
        lab.app
            .db
            .upsert_task_session(&crate::db::tasks::TaskSessionRecord {
                session_id: "t".into(),
                task_id: root.id.clone(),
                parent_id: Some("s".into()),
                name: "t".into(),
                created_at: 11,
                last_activity: 11,
            })
            .unwrap();
        status_of(&dir, "s", State::Ended, "SessionEnd", 11);
        assert_eq!(w.step(11).unwrap(), Next::Exit(Why::Ended));
        let typist = Typist::default();
        w.close(Why::Ended, &typist).unwrap();
        assert!(typist.typed.borrow().is_empty());
        let left = Inbox::lock(&dir).unwrap().read().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            (left[0].message.as_str(), left[0].to.as_deref()),
            ("hi", Some("t"))
        );
    }

    /// `s`'s batch of `msgs` in flight, posted at `posted_at`, as a watcher
    /// killed after writing it leaves it.
    fn left_in_flight(lab: &Lab, msgs: Vec<Queued>, posted_at: i64) -> Inflight {
        left_in_flight_for_test(&lab.dir(), "s", &lab.transcript(), msgs, posted_at);
        lab.inflight().unwrap()
    }

    #[test]
    fn a_batch_killed_before_its_take_is_queued_once() {
        let lab = Lab::new();
        lab.send("hi");
        let msgs = lab.queued();
        left_in_flight(&lab, msgs.clone(), 10);
        let inbox = Inbox::lock(&lab.dir()).unwrap();
        assert!(!recover(&inbox, &lab.dir(), "s", 40).unwrap());
        drop(inbox);
        assert_eq!(lab.queued(), msgs);
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn a_batch_stop_took_before_its_recovery_is_not_put_back() {
        let lab = Lab::new();
        lab.send("hi");
        left_in_flight(&lab, lab.queued(), 10);
        let dir = lab.dir();
        // Stop recovers first, then takes what is queued.
        let inbox = Inbox::lock(&dir).unwrap();
        assert!(!recover(&inbox, &dir, "s", 12).unwrap());
        assert_eq!(inbox.take_for(Some("s")).unwrap().len(), 1);
        assert!(recover(&inbox, &dir, "s", 40).is_ok());
        drop(inbox);
        assert!(lab.queued().is_empty());
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn a_take_that_failed_leaves_nothing_in_flight_to_put_back() {
        let lab = Lab::new();
        let w = lab.watcher();
        lab.send("hi");
        left_in_flight(&lab, lab.queued(), 10);
        w.close(Why::Refused, &Typist::default()).unwrap();
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn recovery_waits_for_the_deadline_and_for_the_lock() {
        let lab = Lab::new();
        let msgs = vec![queued(5, "hi")];
        left_in_flight(&lab, msgs.clone(), 10);
        let dir = lab.dir();
        let inbox = Inbox::lock(&dir).unwrap();
        // Young: the session may hold it still.
        assert!(!recover(&inbox, &dir, "s", 29).unwrap());
        assert!(lab.inflight().is_some());
        // Watched: the watcher's own.
        let held = lock_within(&wait_file(&dir, "s", "lock"), 1, Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(!recover(&inbox, &dir, "s", 40).unwrap());
        drop(held);
        assert!(recover(&inbox, &dir, "s", 40).unwrap());
        drop(inbox);
        assert_eq!(lab.queued(), msgs);
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn recovery_lets_a_seen_batch_go() {
        let lab = Lab::new();
        left_in_flight(&lab, vec![queued(5, "hi")], 10);
        lab.deliver();
        let inbox = Inbox::lock(&lab.dir()).unwrap();
        assert!(!recover(&inbox, &lab.dir(), "s", 40).unwrap());
        drop(inbox);
        assert!(lab.queued().is_empty());
        assert!(lab.inflight().is_none());
    }

    #[test]
    fn a_starting_watcher_carries_on_with_a_young_batch_of_its_session() {
        let lab = Lab::new();
        let listener = lab.listen();
        lab.status(State::Idle, "SessionStart", 1);
        let left = left_in_flight(&lab, vec![queued(5, "hi")], now_secs());
        let mut w = Watcher::open_retrying(
            &lab.app,
            lab.task(),
            lab.env(std::process::id()),
            lab.start(),
            now_secs(),
            Duration::ZERO,
        )
        .unwrap()
        .unwrap();
        assert_eq!(lab.inflight(), Some(left));
        lab.send("next");
        // Not posted over it.
        w.step(now_secs()).unwrap();
        assert!(received(&listener).is_none());
        lab.deliver();
        w.step(now_secs()).unwrap();
        assert!(content(&received(&listener).unwrap()).contains("next"));
    }
}
