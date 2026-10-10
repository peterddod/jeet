//! Messages for a task's agent: `jeet task send`.
//!
//! How a message gets there depends on what the agent is doing:
//! - idle: it is typed into the agent's tmux pane, as if by the user, and
//!   submitted;
//! - busy, or waiting on the user: it is queued in
//!   `<task>/.jeet/inbox.jsonl`, and the agent's Stop hook hands the queue
//!   over when it finishes, so it carries straight on. Typed into a
//!   permission prompt, a message would answer it — `1` and Enter is yes —
//!   and that answer is the user's to give;
//! - not running: the agent is started with it as its first prompt.
//!
//! Anything still queued when a message is typed in or starts the agent goes
//! with it, so nothing waits for a Stop that may never come.
//!
//! tmux sits behind [`Agent`], so the routing is tested without it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::context::App;
use crate::db::tasks::TaskRecord;
use crate::runtime;
use crate::status::{self, Activity};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

/// More than any message needs; a runaway one is refused rather than typed.
const MAX_MESSAGE: usize = 100_000;

/// A message waiting in a task's inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub at: i64,
    pub message: String,
}

pub fn inbox_path(task_dir: &Path) -> PathBuf {
    task_dir.join(tasks::MARKER_DIR).join("inbox.jsonl")
}

/// Held while the inbox is read or written, so a message queued just as
/// the Stop hook takes the queue is neither lost nor handed over twice.
fn lock(task_dir: &Path) -> Result<File> {
    let dir = task_dir.join(tasks::MARKER_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("inbox.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

/// Add `message` to the end of the task's inbox.
pub fn enqueue(task_dir: &Path, message: &str) -> Result<()> {
    let _lock = lock(task_dir)?;
    let path = inbox_path(task_dir);
    let mut line = serde_json::to_string(&Queued {
        at: now_secs(),
        message: message.to_string(),
    })?;
    line.push('\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
        .with_context(|| format!("write {}", path.display()))
}

/// Take everything in the task's inbox, oldest first, leaving it empty.
pub fn drain(task_dir: &Path) -> Result<Vec<Queued>> {
    let _lock = lock(task_dir)?;
    let path = inbox_path(task_dir);
    let queued = read(&path)?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("empty {}", path.display()))
        }
        _ => Ok(queued),
    }
}

/// Put messages taken by [`drain`] back in front of whatever came since.
fn requeue(task_dir: &Path, taken: &[Queued]) -> Result<()> {
    if taken.is_empty() {
        return Ok(());
    }
    let _lock = lock(task_dir)?;
    let path = inbox_path(task_dir);
    let mut all = taken.to_vec();
    all.extend(read(&path)?);
    let mut text = String::new();
    for queued in &all {
        text.push_str(&serde_json::to_string(queued)?);
        text.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

/// The messages in an inbox file; lines that are not one are skipped.
fn read(path: &Path) -> Result<Vec<Queued>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// What the Stop hook prints to hand the queue over: Claude reads the
/// `block` decision as "do not stop yet", and the reason as what to do next.
pub fn stop_block(queued: &[Queued]) -> String {
    let messages: Vec<&str> = queued.iter().map(|q| q.message.as_str()).collect();
    let reason = format!(
        "Messages were sent to you with `jeet task send` while you were working. \
         Deal with them now:\n\n{}",
        messages.join("\n\n---\n\n")
    );
    serde_json::json!({ "decision": "block", "reason": reason }).to_string()
}

/// How a message reaches the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Typed into its pane.
    Type,
    /// Queued for its Stop hook.
    Queue,
    /// Its first prompt.
    Start,
}

pub fn route(activity: &Activity) -> Route {
    match activity {
        Activity::Idle { .. } => Route::Type,
        // Running, but saying nothing of what it is doing: an agent other
        // than Claude, which has no hooks to queue for.
        Activity::Live => Route::Type,
        Activity::Running { .. } | Activity::Waiting { .. } => Route::Queue,
        Activity::NotStarted | Activity::NotRunning | Activity::Ended => Route::Start,
    }
}

/// A task's agent, as far as sending it messages goes.
pub trait Agent {
    /// Whether its tmux pane has anything running in it.
    fn alive(&self) -> bool;
    /// Type `text` into its pane and press Enter.
    fn type_in(&self, text: &str) -> Result<()>;
    /// Start it with `prompt` as its first; false when it turned out to be
    /// running already, and nothing was started.
    fn start(&self, prompt: &str) -> Result<bool>;
}

/// What became of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivered {
    Typed,
    Queued,
    Started,
}

/// Get `message` to the agent of the task in `task_dir`.
pub fn deliver(task_dir: &Path, agent: &dyn Agent, message: &str) -> Result<Delivered> {
    let message = clean(message);
    if message.trim().is_empty() {
        bail!("there is no message to send");
    }
    if message.len() > MAX_MESSAGE {
        bail!("the message is too long ({} bytes)", message.len());
    }
    let activity = status::activity(status::current_status(task_dir).as_ref(), agent.alive());
    let route = route(&activity);
    if route == Route::Queue {
        enqueue(task_dir, &message)?;
        return Ok(Delivered::Queued);
    }

    let earlier = drain(task_dir)?;
    let text = with_earlier(&earlier, &message);
    let sent = match route {
        Route::Start => match agent.start(&text) {
            Ok(true) => Ok(Delivered::Started),
            // Came up between looking and starting: it can take it typed.
            Ok(false) => agent.type_in(&typed(&text)).map(|()| Delivered::Typed),
            Err(e) => Err(e),
        },
        _ => agent.type_in(&typed(&text)).map(|()| Delivered::Typed),
    };
    if sent.is_err() {
        // Not delivered, so not taken either.
        if let Err(e) = requeue(task_dir, &earlier) {
            eprintln!("jeet: warning: could not put queued messages back: {e:#}");
        }
    }
    sent
}

/// `message`, after whatever was queued before it.
fn with_earlier(earlier: &[Queued], message: &str) -> String {
    let mut parts: Vec<&str> = earlier.iter().map(|q| q.message.as_str()).collect();
    parts.push(message);
    parts.join("\n\n")
}

/// `text` without control characters other than line breaks and tabs: an
/// escape or a ctrl-c in a message would drive the agent's terminal, not
/// say anything to it.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect()
}

/// `text` as it can be typed: on one line, since a line break typed into
/// the agent's prompt would submit it there and then.
fn typed(text: &str) -> String {
    let flat: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    flat.trim().to_string()
}

/// A task's agent in its tmux session.
pub struct TmuxAgent<'a> {
    app: &'a App,
    task: &'a TaskRecord,
}

impl<'a> TmuxAgent<'a> {
    pub fn new(app: &'a App, task: &'a TaskRecord) -> Self {
        Self { app, task }
    }
}

impl Agent for TmuxAgent<'_> {
    fn alive(&self) -> bool {
        tmux::available() && tmux::pane_alive(&runtime::session_name(self.task))
    }

    fn type_in(&self, text: &str) -> Result<()> {
        tmux::send_text(&runtime::session_name(self.task), text)
    }

    fn start(&self, prompt: &str) -> Result<bool> {
        let running = runtime::ensure_running(self.app, self.task, None, Some(prompt))?;
        for warning in &running.warnings {
            eprintln!("jeet: {warning}");
        }
        Ok(running.started)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::status::{SessionStatus, State};

    /// An agent that records what it is asked to do.
    #[derive(Default)]
    struct Fake {
        alive: bool,
        running_already: bool,
        fail: bool,
        typed: RefCell<Vec<String>>,
        started: RefCell<Vec<String>>,
    }

    impl Agent for Fake {
        fn alive(&self) -> bool {
            self.alive
        }
        fn type_in(&self, text: &str) -> Result<()> {
            if self.fail {
                bail!("tmux went away");
            }
            self.typed.borrow_mut().push(text.to_string());
            Ok(())
        }
        fn start(&self, prompt: &str) -> Result<bool> {
            if self.fail {
                bail!("tmux went away");
            }
            self.started.borrow_mut().push(prompt.to_string());
            Ok(!self.running_already)
        }
    }

    fn task_in(state: Option<State>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(state) = state {
            let status_dir = status::status_dir(dir.path());
            std::fs::create_dir_all(&status_dir).unwrap();
            let status = SessionStatus {
                session_id: "s".into(),
                state,
                detail: None,
                event: "x".into(),
                at: 1,
            };
            std::fs::write(
                status_dir.join("s.json"),
                serde_json::to_vec(&status).unwrap(),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn routes_follow_what_the_agent_is_doing() {
        assert_eq!(route(&Activity::Idle { since: 0 }), Route::Type);
        // Never typed into a permission prompt, which it would answer.
        assert_eq!(route(&Activity::Waiting { detail: None }), Route::Queue);
        assert_eq!(route(&Activity::Live), Route::Type);
        assert_eq!(route(&Activity::Running { detail: None }), Route::Queue);
        assert_eq!(route(&Activity::NotRunning), Route::Start);
        assert_eq!(route(&Activity::NotStarted), Route::Start);
        assert_eq!(route(&Activity::Ended), Route::Start);
    }

    #[test]
    fn an_idle_agent_has_it_typed_in_on_one_line() {
        let task = task_in(Some(State::Idle));
        let agent = Fake {
            alive: true,
            ..Fake::default()
        };
        let got = deliver(task.path(), &agent, "also add tests\nplease\u{1b}[A\u{3}").unwrap();
        assert_eq!(got, Delivered::Typed);
        assert_eq!(*agent.typed.borrow(), ["also add tests please[A"]);
        assert!(agent.started.borrow().is_empty());
    }

    #[test]
    fn a_busy_agent_gets_it_queued_for_its_stop_hook() {
        let task = task_in(Some(State::Running));
        let agent = Fake {
            alive: true,
            ..Fake::default()
        };
        assert_eq!(
            deliver(task.path(), &agent, "one\ntwo").unwrap(),
            Delivered::Queued
        );
        assert_eq!(
            deliver(task.path(), &agent, "three").unwrap(),
            Delivered::Queued
        );
        assert!(agent.typed.borrow().is_empty());
        let queued = drain(task.path()).unwrap();
        let messages: Vec<&str> = queued.iter().map(|q| q.message.as_str()).collect();
        assert_eq!(messages, ["one\ntwo", "three"]);
        assert!(drain(task.path()).unwrap().is_empty());
    }

    #[test]
    fn an_agent_not_running_is_started_with_it_and_what_was_queued() {
        // The status says running, but the pane is gone.
        let task = task_in(Some(State::Running));
        enqueue(task.path(), "earlier").unwrap();
        let agent = Fake::default();
        assert_eq!(
            deliver(task.path(), &agent, "now").unwrap(),
            Delivered::Started
        );
        assert_eq!(*agent.started.borrow(), ["earlier\n\nnow"]);
        assert!(drain(task.path()).unwrap().is_empty());

        // Never started at all.
        let fresh = task_in(None);
        let agent = Fake::default();
        assert_eq!(
            deliver(fresh.path(), &agent, "go").unwrap(),
            Delivered::Started
        );

        // Up by the time it was started: typed in instead.
        let agent = Fake {
            running_already: true,
            ..Fake::default()
        };
        assert_eq!(
            deliver(fresh.path(), &agent, "go").unwrap(),
            Delivered::Typed
        );
        assert_eq!(*agent.typed.borrow(), ["go"]);
    }

    #[test]
    fn what_could_not_be_delivered_stays_queued() {
        let task = task_in(Some(State::Idle));
        enqueue(task.path(), "earlier").unwrap();
        let agent = Fake {
            alive: true,
            fail: true,
            ..Fake::default()
        };
        assert!(deliver(task.path(), &agent, "now").is_err());
        let left: Vec<String> = drain(task.path())
            .unwrap()
            .into_iter()
            .map(|q| q.message)
            .collect();
        assert_eq!(left, ["earlier"]);
    }

    #[test]
    fn nothing_is_sent_for_an_empty_message() {
        let task = task_in(Some(State::Idle));
        let agent = Fake {
            alive: true,
            ..Fake::default()
        };
        assert!(deliver(task.path(), &agent, " \u{1b} \n").is_err());
        assert!(agent.typed.borrow().is_empty());
    }

    #[test]
    fn the_stop_hook_blocks_with_every_message() {
        let queued = [
            Queued {
                at: 1,
                message: "add tests".into(),
            },
            Queued {
                at: 2,
                message: "and docs".into(),
            },
        ];
        let value: serde_json::Value = serde_json::from_str(&stop_block(&queued)).unwrap();
        assert_eq!(value["decision"], "block");
        let reason = value["reason"].as_str().unwrap();
        assert!(reason.contains("add tests\n\n---\n\nand docs"), "{reason}");
    }

    #[test]
    fn junk_in_the_inbox_is_skipped() {
        let task = task_in(None);
        enqueue(task.path(), "kept").unwrap();
        let path = inbox_path(task.path());
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json\n");
        std::fs::write(&path, text).unwrap();
        let left = drain(task.path()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "kept");
    }
}
