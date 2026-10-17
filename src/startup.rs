//! The first moments of a task's agent.
//!
//! A harness may stop at a prompt before its session starts — Claude Code
//! asks once per folder whether to trust it — and no hook says so: the task
//! would sit there looking busy. So when jeet launches an agent whose
//! harness has hooks, it also launches `jeet task-startup` in the
//! background, which watches the agent's pane until a hook reports, the
//! agent exits, or [`WATCH_SECS`] pass. On a trust prompt it:
//!
//! - accepts it, when `[harness] auto_trust` is on and the task is in scope
//!   (see [`may_auto_trust`]) and not paused or stopped, one key at a time,
//!   each only on what the pane showed steadily before it (see
//!   [`next_step`]), and checks that the prompt goes and a hook then
//!   reports, trying once more if not; a start that still fails is reported
//!   to whoever the task reports to (`start failed`);
//! - otherwise marks the task "needs input – trust folder" and tells the
//!   user with a notification, unless someone is attached to look at it.
//!
//! Reviews are out of scope unless `auto_trust_reviews` is on as well:
//! their checkouts are other people's branches, and trusting them lets their
//! `.claude/settings.json` hooks and `.mcp.json` servers run.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::agent::AgentSpec;
use crate::config::NotifyLevel;
use crate::config::{self, Config};
use crate::context::App;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::harness::{Harness, StartupPrompt, TrustStep};
use crate::manager::run;
use crate::notify::{self, Event, Why};
use crate::reports::{self, StatusEvent};
use crate::runtime;
use crate::status;
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

/// How long the pane is watched for a startup prompt.
pub const WATCH_SECS: u64 = 30;

/// How long a hook gets to report once a prompt is accepted.
const HOOK_WAIT_SECS: u64 = 15;

/// Set to `0` to launch no watcher.
pub const WATCH_ENV: &str = "JEET_STARTUP_WATCH";

/// What a watcher's log file is called, in the task's `.jeet`.
const LOG_FILE: &str = "startup.log";

/// Whose agent's trust prompt may be accepted: a task's, by its kind, or a
/// review's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Task(TaskKind),
    Review,
}

/// Whether jeet may accept the trust prompt by itself for `who`, in
/// `dir`: only with `auto_trust` on, only under `$JEET_HOME/tasks`, and for
/// a review only with `auto_trust_reviews` on too. A manager follows
/// `auto_trust` as a worker does: its parent starts it with nobody at its
/// pane. Never the assistant's or the setup agent's.
pub fn may_auto_trust(config: &Config, home: &Path, who: Trust, dir: &Path) -> bool {
    let (tasks, reviews) = config.auto_trust();
    let in_tasks = dir.starts_with(config::tasks_root(home));
    match who {
        Trust::Task(TaskKind::Work | TaskKind::Manager) => tasks && in_tasks,
        Trust::Review => reviews && in_tasks,
        Trust::Task(TaskKind::Assistant | TaskKind::Setup) => false,
    }
}

/// The agent's pane, as far as the watcher goes.
pub trait Pane {
    fn alive(&self) -> bool;
    fn capture(&self) -> Result<String>;
    fn press(&self, keys: &[&str]) -> Result<()>;
}

/// A task's tmux session.
struct TmuxPane(String);

impl Pane for TmuxPane {
    fn alive(&self) -> bool {
        tmux::pane_alive(&self.0)
    }

    fn capture(&self) -> Result<String> {
        tmux::capture(&self.0)
    }

    fn press(&self, keys: &[&str]) -> Result<()> {
        tmux::press(&self.0, keys)
    }
}

/// How long to look, and how often.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub watch: Duration,
    pub hook_wait: Duration,
    pub poll: Duration,
    /// How many looks the trust prompt gets to redraw after a key,
    /// `redraw_poll` apart: counted, not timed, so a slow machine gets the
    /// same number of looks.
    pub redraw_looks: u32,
    pub redraw_poll: Duration,
    /// How many looks in a row must show the same thing before a key is
    /// pressed on it: Claude Code draws its trust prompt, then draws it
    /// afresh with "No" selected again a moment later, losing a key sent
    /// in between.
    pub steady_looks: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            watch: Duration::from_secs(WATCH_SECS),
            hook_wait: Duration::from_secs(HOOK_WAIT_SECS),
            poll: Duration::from_millis(500),
            // About 2.5s.
            redraw_looks: 13,
            redraw_poll: Duration::from_millis(200),
            // About 0.6s: the redraw comes some 0.15s after the first.
            steady_looks: 4,
        }
    }
}

/// How many of the pane's last lines a failure logs.
const LOG_LINES: usize = 12;

/// How many times the trust prompt is tried: once more after a failure.
const ATTEMPTS: u32 = 2;

/// How many select keys one try may press.
const MAX_SELECTS: u32 = 2;

/// How the agent's start went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A hook reported: it is going.
    Started,
    /// It exited.
    Gone,
    /// Nothing reported, and no prompt jeet knows was seen.
    NoPrompt,
    /// A trust prompt is waiting for the user.
    NeedsUser,
    /// The trust prompt was accepted, and a hook reported.
    Accepted,
    /// The trust prompt was accepted, but no hook reported after.
    AcceptedNoHook,
    /// The trust prompt was not in a state jeet knows how to accept.
    CouldNotAccept,
    /// It exited after jeet pressed keys at its trust prompt, before any
    /// hook reported.
    Ended,
}

/// Watch `pane` until `reported` (a hook has said something since the
/// launch), the agent is gone, or a prompt is found, accepting a trust
/// prompt while `auto` says so, asked before each key. What accepting did,
/// and why it gave up if it did, goes to `log`.
pub fn watch(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    auto: &dyn Fn() -> bool,
    limits: Limits,
    log: &dyn Fn(&str),
) -> Result<Outcome> {
    let deadline = Instant::now() + limits.watch;
    loop {
        if reported() {
            return Ok(Outcome::Started);
        }
        if !pane.alive() {
            return Ok(Outcome::Gone);
        }
        if harness.startup_prompt(&pane.capture()?) == Some(StartupPrompt::Trust) {
            return match auto() {
                true => accept(harness, pane, reported, auto, limits, log),
                false => Ok(Outcome::NeedsUser),
            };
        }
        if Instant::now() >= deadline {
            return Ok(Outcome::NoPrompt);
        }
        thread::sleep(limits.poll);
    }
}

/// What one look at the pane shows of the trust prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Look {
    /// No trust prompt.
    Gone,
    /// The prompt, but not exactly one option jeet knows selected: a frame
    /// caught mid-redraw, say.
    Unsure,
    /// Not on accepting: the keys that move towards it.
    Select(Vec<&'static str>),
    /// Accepting is selected: the keys that confirm it.
    Confirm(Vec<&'static str>),
}

/// What `harness`'s pane text `pane` shows of the trust prompt.
fn look(harness: &dyn Harness, pane: &str) -> Look {
    if harness.startup_prompt(pane) != Some(StartupPrompt::Trust) {
        return Look::Gone;
    }
    match harness.accept_trust(pane) {
        Some(TrustStep::Select(keys)) => Look::Select(keys),
        Some(TrustStep::Confirm(keys)) => Look::Confirm(keys),
        None => Look::Unsure,
    }
}

/// The last kind of key pressed at the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pressed {
    Nothing,
    Select,
    Confirm,
}

/// What to do next at the trust prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Look again.
    Wait,
    /// Press these keys.
    Press(Vec<&'static str>),
    /// The prompt has gone.
    Gone,
    /// Out of looks: why.
    GiveUp(&'static str),
}

/// What to do next, from `looks` at the pane since the key last pressed
/// (`pressed`), oldest first; the last is the pane as it is now. Nothing is
/// pressed but on `steady` like looks in a row, nor anything after
/// `patience` looks but the select key, for one that was lost.
///
/// - Enter only on "Yes" seen selected each time, and never twice: an
///   Enter that left the prompt up gives up, for the caller to try again.
/// - Down at once on "No"; after a Down, again only once "Yes" was seen
///   and "No" came back (the prompt was drawn afresh), or when nothing
///   changed in `patience` looks (the key was lost). Not merely on "No"
///   still showing: a second Down that lands wraps round to "No".
fn next_step(looks: &[Look], pressed: Pressed, steady: usize, patience: usize) -> Next {
    let steady = steady.max(1);
    let out = looks.len() >= patience.max(steady);
    let wait = |why| match out {
        true => Next::GiveUp(why),
        false => Next::Wait,
    };
    let Some(tail) = looks.len().checked_sub(steady).map(|i| &looks[i..]) else {
        return Next::Wait;
    };
    if tail.iter().any(|l| l != &tail[0]) {
        return wait("the prompt never showed the same thing long enough");
    }
    match (&tail[0], pressed) {
        (Look::Gone, _) => Next::Gone,
        (Look::Unsure, _) => wait("no option it knows was seen selected"),
        (Look::Confirm(_) | Look::Select(_), Pressed::Confirm) => {
            wait("the prompt was still there after the confirming key")
        }
        (Look::Confirm(keys), _) => Next::Press(keys.clone()),
        (Look::Select(keys), Pressed::Nothing) => Next::Press(keys.clone()),
        (Look::Select(keys), Pressed::Select) => {
            let reset = looks.iter().any(|l| matches!(l, Look::Confirm(_)));
            match reset || out {
                true => Next::Press(keys.clone()),
                false => Next::Wait,
            }
        }
    }
}

/// How one try at the trust prompt ended.
enum Tried {
    /// For good.
    Done(Outcome),
    /// Worth one more try, else `outcome`.
    Failed {
        outcome: Outcome,
        why: String,
        screen: String,
    },
}

/// Accept the trust prompt, trying once more if the first try leaves it
/// up or no hook reports after. See [`next_step`] for when each key is
/// pressed.
fn accept(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    auto: &dyn Fn() -> bool,
    limits: Limits,
    log: &dyn Fn(&str),
) -> Result<Outcome> {
    let mut confirmed = false;
    let mut n = 0;
    loop {
        n += 1;
        let (outcome, why, screen) =
            match attempt(harness, pane, reported, auto, limits, log, &mut confirmed)? {
                Tried::Done(outcome) => return Ok(outcome),
                Tried::Failed {
                    outcome,
                    why,
                    screen,
                } => (outcome, why, screen),
            };
        // Answered some other way while jeet looked: not jeet's to report.
        if reported() {
            return Ok(started(confirmed));
        }
        if n < ATTEMPTS {
            log(&format!("{why}: trying once more"));
            continue;
        }
        log(&format!(
            "could not accept the trust prompt: {why}; the pane ended:"
        ));
        for line in last_lines(&screen, LOG_LINES) {
            log(&format!("  | {line}"));
        }
        return Ok(outcome);
    }
}

/// What a hook reporting means: accepted, if jeet confirmed.
fn started(confirmed: bool) -> Outcome {
    match confirmed {
        true => Outcome::Accepted,
        false => Outcome::Started,
    }
}

/// One try: look and press as [`next_step`] says until the prompt has gone,
/// then wait for a hook.
fn attempt(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    auto: &dyn Fn() -> bool,
    limits: Limits,
    log: &dyn Fn(&str),
    confirmed: &mut bool,
) -> Result<Tried> {
    let steady = limits.steady_looks as usize;
    let patience = limits.redraw_looks as usize;
    let mut looks = Vec::new();
    let mut pressed = Pressed::Nothing;
    let mut selects = 0;
    let mut since = Instant::now();
    loop {
        if reported() {
            return Ok(Tried::Done(started(*confirmed)));
        }
        if !pane.alive() {
            return Ok(Tried::Done(ended(pressed, *confirmed)));
        }
        let screen = pane.capture()?;
        looks.push(look(harness, &screen));
        let keys = match next_step(&looks, pressed, steady, patience) {
            Next::Wait => {
                thread::sleep(limits.redraw_poll);
                continue;
            }
            Next::GiveUp(why) => {
                return Ok(Tried::Failed {
                    outcome: Outcome::CouldNotAccept,
                    why: format!("{why} ({} looks over {}ms)", looks.len(), ms(since)),
                    screen,
                });
            }
            Next::Gone => {
                let keyed = pressed != Pressed::Nothing;
                return wait_for_hook(harness, pane, reported, limits, keyed, *confirmed);
            }
            Next::Press(keys) => keys,
        };
        // Pressed on the capture just taken: what it showed is selected.
        let confirm = matches!(looks.last(), Some(Look::Confirm(_)));
        if !confirm && selects >= MAX_SELECTS {
            return Ok(Tried::Failed {
                outcome: Outcome::CouldNotAccept,
                why: format!("still not on accepting after {selects} select keys"),
                screen,
            });
        }
        // No longer to accept, a task paused since, say: the user answers.
        if !auto() {
            log("no longer to accept: left for the user");
            return Ok(Tried::Done(Outcome::NeedsUser));
        }
        pane.press(&keys)?;
        log(&format!(
            "pressed {} after {}ms, on {} like looks",
            keys.join(" "),
            ms(since),
            steady
        ));
        match confirm {
            true => {
                pressed = Pressed::Confirm;
                *confirmed = true;
            }
            false => {
                pressed = Pressed::Select;
                selects += 1;
            }
        }
        looks.clear();
        since = Instant::now();
        thread::sleep(limits.redraw_poll);
    }
}

/// What the agent exiting means: it ended at startup if jeet pressed keys.
fn ended(pressed: Pressed, confirmed: bool) -> Outcome {
    match pressed != Pressed::Nothing || confirmed {
        true => Outcome::Ended,
        false => Outcome::Gone,
    }
}

fn ms(since: Instant) -> u128 {
    since.elapsed().as_millis()
}

/// The last `n` lines of `screen` with anything on them.
fn last_lines(screen: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Wait for a hook once the prompt has gone, while it stays gone; `keyed`
/// if this try pressed keys.
fn wait_for_hook(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    limits: Limits,
    keyed: bool,
    confirmed: bool,
) -> Result<Tried> {
    let deadline = Instant::now() + limits.hook_wait;
    let mut back = 0;
    loop {
        if reported() {
            return Ok(Tried::Done(started(confirmed)));
        }
        if !pane.alive() {
            let pressed = match keyed {
                true => Pressed::Select,
                false => Pressed::Nothing,
            };
            return Ok(Tried::Done(ended(pressed, confirmed)));
        }
        let screen = pane.capture()?;
        back = match look(harness, &screen) {
            Look::Gone => 0,
            _ => back + 1,
        };
        if back >= limits.steady_looks.max(1) {
            return Ok(Tried::Failed {
                outcome: Outcome::CouldNotAccept,
                why: "the prompt came back".into(),
                screen,
            });
        }
        if Instant::now() >= deadline {
            return Ok(match confirmed {
                true => Tried::Failed {
                    outcome: Outcome::AcceptedNoHook,
                    why: format!("no hook reported in {}s", limits.hook_wait.as_secs()),
                    screen,
                },
                // Answered, but not by jeet: not jeet's to report.
                false => Tried::Done(Outcome::NoPrompt),
            });
        }
        thread::sleep(limits.poll);
    }
}

/// Whether `task`'s trust prompt may be accepted now, `in_scope` of
/// `auto_trust`: not while it, or a task above it, is paused or stopped,
/// as accepting would let its brief run. Asked before each key.
fn may_accept_now(app: &App, task: &TaskRecord, in_scope: bool) -> bool {
    in_scope && matches!(run::hold(app, task), Ok(None))
}

/// The agent being watched: a task's, or a review's.
enum Watched {
    Task(TaskRecord),
    Review(Box<ReviewRecord>),
}

/// `jeet task-startup <task>`: watch the start of the agent of task (or
/// review) `task_id`, which runs session `session` when jeet picked its id.
pub fn run(app: &App, task_id: &str, session: Option<&str>) -> Result<()> {
    let watched = match app.db.get_task(task_id)? {
        Some(task) => Watched::Task(task),
        None => match app.db.get_review(task_id)? {
            Some(review) => Watched::Review(Box::new(review)),
            None => anyhow::bail!("no task {task_id}"),
        },
    };
    let spec = AgentSpec::from_config(&app.config)?;
    let (dir, name, slug, subtitle, trust) = match &watched {
        Watched::Task(task) => (
            tasks::task_dir(app, task),
            runtime::session_name(task),
            task.slug.clone(),
            task.name.clone(),
            Trust::Task(task.kind),
        ),
        Watched::Review(review) => (
            crate::review::dir(app, review),
            crate::review::session_name(review),
            review.slug.clone(),
            crate::review::subject(review),
            Trust::Review,
        ),
    };
    let since = status::launched_at(&dir).unwrap_or_else(now_secs);
    let reported = || {
        let latest = match session {
            Some(id) => status::read_status(&dir, id),
            None => status::current_status(&dir),
        };
        latest.is_some_and(|s| s.at >= since)
    };
    let in_scope = may_auto_trust(&app.config, &app.home, trust, &dir);
    let auto = || match &watched {
        Watched::Task(task) => may_accept_now(app, task, in_scope),
        // Not while paused: accepting would let its prompt run.
        Watched::Review(review) => {
            in_scope
                && app
                    .db
                    .get_review(&review.id)
                    .is_ok_and(|r| r.is_some_and(|r| !r.paused))
        }
    };
    let outcome = watch(
        spec.harness,
        &TmuxPane(name.clone()),
        &reported,
        &auto,
        Limits::default(),
        &|line| eprintln!("jeet task-startup {slug}: {line}"),
    )?;
    eprintln!("jeet task-startup {slug}: {outcome:?}");
    let level = match &watched {
        Watched::Task(task) => notify::level(task.kind),
        Watched::Review(_) => NotifyLevel::Review,
    };
    let tell = |why: Why, body: &str| {
        if !tmux::attached(&name) {
            notify::event(
                app,
                Event {
                    why,
                    kind: "startup-prompt".into(),
                    level: Some(level),
                    task: Some(slug.clone()),
                    title: "jeet".into(),
                    subtitle: Some(subtitle.clone()),
                    body: body.into(),
                    ..Event::default()
                },
            );
        }
    };
    // No hook fires for these, so whoever it reports to hears of them
    // here, once per launch however often startup is watched: one
    // `start failed` a launch, whichever way it failed first.
    let report = |event: StatusEvent, body: &str| {
        let told = match &watched {
            Watched::Task(task) if task.parent_id.is_some() => {
                reports::auto_status(app, task, event, body, since, 0).map(|_| ())
            }
            Watched::Task(_) => Ok(()),
            Watched::Review(review) => {
                reports::auto_status_review(app, review, event, body, since, 0).map(|_| ())
            }
        };
        if let Err(e) = told {
            eprintln!("jeet task-startup {slug}: warning: {e:#}");
        }
    };
    let harness = spec.harness.name();
    match outcome {
        Outcome::NeedsUser | Outcome::CouldNotAccept => {
            if let Some(id) = session {
                status::mark_startup_prompt(&dir, id, "trust folder", now_secs())?;
            }
            match outcome {
                Outcome::CouldNotAccept => report(
                    StatusEvent::StartFailed,
                    "start failed: the trust prompt was not accepted; \
                     it waits for the user to trust the folder",
                ),
                _ => report(
                    StatusEvent::Waiting,
                    "waiting for the user to trust the folder",
                ),
            }
            tell(
                Why::NeedsUser,
                &format!(
                    "{harness} asks whether to trust the folder; `jeet task open {slug}` to answer",
                ),
            );
        }
        Outcome::AcceptedNoHook => {
            report(
                StatusEvent::StartFailed,
                "start failed: jeet trusted the folder, but no hook has reported since",
            );
            tell(
                Why::Info,
                &format!(
                    "jeet trusted the folder for {harness}, but no hook has reported since; \
                     `jeet task open {slug}` to look",
                ),
            );
        }
        Outcome::Ended => {
            report(
                StatusEvent::StartFailed,
                "start failed: the agent ended at startup, before any hook reported",
            );
            tell(
                Why::Stuck,
                &format!("{harness} ended at startup: the task has no agent"),
            );
        }
        Outcome::Started | Outcome::Gone | Outcome::NoPrompt | Outcome::Accepted => {}
    }
    Ok(())
}

/// Start `jeet task-startup` for agent `id`, just launched in `dir`, in the
/// background: detached the way `store::launch` is, its output in
/// `dir`'s `.jeet/startup.log`.
pub fn launch(app: &App, id: &str, dir: &Path, session: Option<&str>) -> Result<()> {
    if std::env::var(WATCH_ENV).is_ok_and(|v| v.trim() == "0") {
        return Ok(());
    }
    let dir = dir.join(tasks::MARKER_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let log = dir.join(LOG_FILE);
    let out = std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let mut cmd = Command::new("nohup");
    cmd.arg(exe).args(["task-startup", id]);
    if let Some(session) = session {
        cmd.args(["--session", session]);
    }
    cmd.env("JEET_HOME", &app.home)
        .current_dir(&app.home)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("start jeet task-startup")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::db::tasks::RunState;
    use crate::harness::claude::ClaudeCode;

    fn pane_fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/panes/claude-code")
            .join(format!("{name}.txt"));
        std::fs::read_to_string(path).unwrap()
    }

    /// A Claude pane at its trust prompt, or past it: Down moves between
    /// "No" and "Yes", wrapping round, Enter on "Yes" trusts, Enter on "No"
    /// exits.
    struct FakeClaude {
        screen: RefCell<String>,
        alive: Cell<bool>,
        pressed: RefCell<Vec<String>>,
        /// Captures it takes to redraw after a key, showing `between`.
        lag: Cell<u32>,
        lagging: Cell<u32>,
        between: RefCell<String>,
        /// Whether Down moves the cursor at all.
        redraws: Cell<bool>,
        hooked: Cell<bool>,
        /// Captures after the first Down until the prompt is drawn afresh
        /// with "No" selected, as Claude Code does once; keys before then
        /// are lost.
        reset_in: Cell<Option<u32>>,
        /// Enters, on "Yes", that do nothing.
        lost_enters: Cell<u32>,
        /// Whether a hook reports once it trusts.
        hooks: Cell<bool>,
        /// Whether Enter was ever pressed on "No".
        enter_on_no: Cell<bool>,
    }

    impl FakeClaude {
        fn new(screen: &str, lag: u32) -> Self {
            Self {
                screen: RefCell::new(pane_fixture(screen)),
                alive: Cell::new(true),
                pressed: RefCell::default(),
                lag: Cell::new(lag),
                lagging: Cell::new(0),
                between: RefCell::new(pane_fixture("trust-no-selected")),
                redraws: Cell::new(true),
                hooked: Cell::new(false),
                reset_in: Cell::new(None),
                lost_enters: Cell::new(0),
                hooks: Cell::new(true),
                enter_on_no: Cell::new(false),
            }
        }

        fn pressed_down(&self) -> bool {
            self.pressed.borrow().iter().any(|k| k == "Down")
        }
    }

    impl Pane for FakeClaude {
        fn alive(&self) -> bool {
            self.alive.get()
        }

        fn capture(&self) -> Result<String> {
            if let Some(n) = self.reset_in.get().filter(|_| self.pressed_down()) {
                match n {
                    0 => {
                        *self.screen.borrow_mut() = pane_fixture("trust-no-selected");
                        self.reset_in.set(None);
                    }
                    _ => self.reset_in.set(Some(n - 1)),
                }
            }
            if self.lagging.get() > 0 {
                self.lagging.set(self.lagging.get() - 1);
                return Ok(self.between.borrow().clone());
            }
            Ok(self.screen.borrow().clone())
        }

        fn press(&self, keys: &[&str]) -> Result<()> {
            for key in keys {
                // Before the prompt is drawn afresh, keys are lost.
                let lost = self.pressed_down() && self.reset_in.get().is_some();
                self.pressed.borrow_mut().push(key.to_string());
                let screen = self.screen.borrow().clone();
                let yes = screen.contains("❯ Yes");
                match *key {
                    "Down" if self.redraws.get() => {
                        let to = match yes {
                            true => "trust-no-selected",
                            false => "trust-yes-selected",
                        };
                        *self.screen.borrow_mut() = pane_fixture(to);
                        self.lagging.set(self.lag.get());
                    }
                    "Enter" if lost => {}
                    "Enter" if yes && self.lost_enters.get() > 0 => {
                        self.lost_enters.set(self.lost_enters.get() - 1);
                    }
                    "Enter" if yes => {
                        *self.screen.borrow_mut() = pane_fixture("no-prompt");
                        self.hooked.set(self.hooks.get());
                    }
                    "Enter" => {
                        self.enter_on_no.set(true);
                        self.alive.set(false);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    }

    fn quick() -> Limits {
        Limits {
            watch: Duration::from_millis(50),
            hook_wait: Duration::from_millis(200),
            poll: Duration::from_millis(1),
            redraw_looks: 30,
            redraw_poll: Duration::ZERO,
            steady_looks: 3,
        }
    }

    fn run_watch(pane: &FakeClaude, auto: bool) -> Outcome {
        run_logged(pane, auto, quick()).0
    }

    fn run_logged(pane: &FakeClaude, auto: bool, limits: Limits) -> (Outcome, Vec<String>) {
        let log = RefCell::new(Vec::new());
        let outcome = watch(
            &ClaudeCode,
            pane,
            &|| pane.hooked.get(),
            &|| auto,
            limits,
            &|line| log.borrow_mut().push(line.to_string()),
        )
        .unwrap();
        (outcome, log.into_inner())
    }

    #[test]
    fn a_trust_prompt_is_accepted_with_down_then_enter_when_allowed() {
        let pane = FakeClaude::new("trust-no-selected", 0);
        assert_eq!(run_watch(&pane, true), Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Down", "Enter"]);
        assert!(pane.alive.get());
    }

    #[test]
    fn a_slow_redraw_gets_no_second_press_and_no_blind_enter() {
        let pane = FakeClaude::new("trust-no-selected", 3);
        assert_eq!(run_watch(&pane, true), Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Down", "Enter"]);
    }

    #[test]
    fn a_redraw_seen_only_on_a_later_look_is_waited_for() {
        let pane = FakeClaude::new("trust-no-selected", 20);
        let (outcome, log) = run_logged(&pane, true, quick());
        assert_eq!(outcome, Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Down", "Enter"]);
        assert!(log.iter().any(|l| l.starts_with("pressed Enter")));
    }

    #[test]
    fn a_down_that_shows_nothing_for_all_its_looks_is_pressed_again() {
        let limits = Limits {
            redraw_looks: 8,
            ..quick()
        };
        // Four looks still show "No", then three "Yes": in time.
        let in_time = FakeClaude::new("trust-no-selected", 4);
        assert_eq!(run_logged(&in_time, true, limits).0, Outcome::Accepted);
        assert_eq!(*in_time.pressed.borrow(), ["Down", "Enter"]);
        // "No" for all eight: taken as lost and pressed again, which here
        // wraps round to "No", each time: given up, never Enter.
        let late = FakeClaude::new("trust-no-selected", 8);
        assert_eq!(run_logged(&late, true, limits).0, Outcome::CouldNotAccept);
        assert!(late.pressed.borrow().iter().all(|k| k == "Down"));
        assert!(!late.enter_on_no.get());
    }

    #[test]
    fn a_frame_caught_mid_redraw_is_looked_past() {
        let pane = FakeClaude::new("trust-no-selected", 3);
        *pane.between.borrow_mut() =
            pane_fixture("trust-no-selected").replace("❯ No, exit", "  No, exit");
        assert_eq!(run_watch(&pane, true), Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Down", "Enter"]);
    }

    #[test]
    fn a_redraw_that_never_comes_gets_no_enter_and_says_why() {
        let pane = FakeClaude::new("trust-no-selected", 0);
        pane.redraws.set(false);
        let limits = Limits {
            redraw_looks: 5,
            ..quick()
        };
        let (outcome, log) = run_logged(&pane, true, limits);
        assert_eq!(outcome, Outcome::CouldNotAccept);
        // A bounded number of Downs a try, two tries, and no Enter.
        let downs = vec!["Down"; 2 * MAX_SELECTS as usize];
        assert_eq!(*pane.pressed.borrow(), downs);
        assert!(pane.alive.get());
        assert!(log.iter().any(|l| l.contains("trying once more")));
        assert!(log.iter().any(|l| l.contains("still not on accepting")));
        assert!(log.iter().any(|l| l.contains("❯ No, exit")));
    }

    /// As seen on Claude Code 2.1.296: a Down sent as the prompt first
    /// shows moves to "Yes", then the prompt is drawn afresh on "No", and
    /// an Enter sent before that is lost.
    #[test]
    fn a_prompt_drawn_afresh_on_no_gets_down_again_before_enter() {
        // Within the steady looks: seen, and Down again.
        // Later: the Enter is lost, and the second try gets it.
        for (reset_in, keys) in [
            (1, &["Down", "Down", "Enter"][..]),
            (2, &["Down", "Down", "Enter"]),
            (5, &["Down", "Enter", "Down", "Enter"]),
            (10, &["Down", "Enter", "Down", "Enter"]),
        ] {
            let pane = FakeClaude::new("trust-no-selected", 0);
            pane.reset_in.set(Some(reset_in));
            let (outcome, log) = run_logged(&pane, true, quick());
            assert_eq!(outcome, Outcome::Accepted, "{reset_in}: {log:?}");
            assert_eq!(*pane.pressed.borrow(), keys, "{reset_in}");
            assert!(!pane.enter_on_no.get());
        }
    }

    #[test]
    fn enter_is_never_pressed_on_no() {
        for lag in [0, 1, 3, 7] {
            for reset_in in [None, Some(0), Some(2), Some(4), Some(9)] {
                let pane = FakeClaude::new("trust-no-selected", lag);
                pane.reset_in.set(reset_in);
                let outcome = run_watch(&pane, true);
                assert!(!pane.enter_on_no.get(), "lag {lag}, reset {reset_in:?}");
                assert!(pane.alive.get());
                assert_eq!(outcome, Outcome::Accepted, "lag {lag}, reset {reset_in:?}");
            }
        }
    }

    #[test]
    fn an_enter_that_leaves_the_prompt_up_is_tried_once_more() {
        let pane = FakeClaude::new("trust-yes-selected", 0);
        pane.lost_enters.set(1);
        let (outcome, log) = run_logged(&pane, true, quick());
        assert_eq!(outcome, Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Enter", "Enter"]);
        assert!(log
            .iter()
            .any(|l| l.contains("still there") && l.contains("once more")));
        // Lost twice: given up, with no third Enter.
        let stuck = FakeClaude::new("trust-yes-selected", 0);
        stuck.lost_enters.set(2);
        let (outcome, log) = run_logged(&stuck, true, quick());
        assert_eq!(outcome, Outcome::CouldNotAccept);
        assert_eq!(*stuck.pressed.borrow(), ["Enter", "Enter"]);
        assert!(log.iter().any(|l| l.starts_with("could not accept")));
    }

    #[test]
    fn a_prompt_that_comes_back_after_enter_is_tried_once_more() {
        /// Gone for a few looks after the first Enter, then back.
        struct Returns(FakeClaude, Cell<u32>);
        impl Pane for Returns {
            fn alive(&self) -> bool {
                self.0.alive()
            }
            fn capture(&self) -> Result<String> {
                let gone = self.1.get();
                if gone > 0 {
                    self.1.set(gone - 1);
                    if gone == 1 {
                        *self.0.screen.borrow_mut() = pane_fixture("trust-yes-selected");
                    }
                }
                self.0.capture()
            }
            fn press(&self, keys: &[&str]) -> Result<()> {
                let first = !self.0.pressed.borrow().iter().any(|k| k == "Enter");
                self.0.press(keys)?;
                if first {
                    self.0.hooked.set(false);
                    self.1.set(5);
                }
                Ok(())
            }
        }
        let pane = Returns(FakeClaude::new("trust-yes-selected", 0), Cell::new(0));
        let log = RefCell::new(Vec::new());
        let outcome = watch(
            &ClaudeCode,
            &pane,
            &|| pane.0.hooked.get(),
            &|| true,
            quick(),
            &|l| log.borrow_mut().push(l.to_string()),
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Accepted);
        assert_eq!(*pane.0.pressed.borrow(), ["Enter", "Enter"]);
        assert!(log.borrow().iter().any(|l| l.contains("came back")));
    }

    #[test]
    fn an_agent_that_exits_after_the_keys_ended_at_startup() {
        struct Exits(FakeClaude);
        impl Pane for Exits {
            fn alive(&self) -> bool {
                self.0.alive()
            }
            fn capture(&self) -> Result<String> {
                self.0.capture()
            }
            fn press(&self, keys: &[&str]) -> Result<()> {
                self.0.press(keys)?;
                self.0.alive.set(false);
                Ok(())
            }
        }
        let pane = Exits(FakeClaude::new("trust-yes-selected", 0));
        let outcome = watch(&ClaudeCode, &pane, &|| false, &|| true, quick(), &|_| {}).unwrap();
        assert_eq!(outcome, Outcome::Ended);
    }

    #[test]
    fn yes_already_selected_is_just_confirmed() {
        let pane = FakeClaude::new("trust-yes-selected", 0);
        assert_eq!(run_watch(&pane, true), Outcome::Accepted);
        assert_eq!(*pane.pressed.borrow(), ["Enter"]);
    }

    #[test]
    fn without_auto_trust_nothing_is_pressed() {
        let pane = FakeClaude::new("trust-no-selected", 0);
        assert_eq!(run_watch(&pane, false), Outcome::NeedsUser);
        assert!(pane.pressed.borrow().is_empty());
    }

    #[test]
    fn a_task_held_while_its_prompt_is_accepted_gets_no_more_keys() {
        let pane = FakeClaude::new("trust-no-selected", 0);
        // Allowed when the prompt is seen and at the select key; paused
        // before the confirming one.
        let asked = Cell::new(0);
        let auto = || {
            asked.set(asked.get() + 1);
            asked.get() <= 2
        };
        let outcome = watch(&ClaudeCode, &pane, &|| false, &auto, quick(), &|_| {}).unwrap();
        assert_eq!(outcome, Outcome::NeedsUser);
        assert_eq!(*pane.pressed.borrow(), ["Down"]);
    }

    #[test]
    fn a_held_task_or_one_under_a_held_one_is_not_accepted_for() {
        use crate::db::tasks::{Setter, TaskKind, TaskState};
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
        };
        let root = crate::manager::root(&app).unwrap();
        for (id, kind, parent) in [
            ("m", TaskKind::Manager, root.id.as_str()),
            ("w", TaskKind::Work, "m"),
        ] {
            app.db
                .insert_task(&TaskRecord {
                    id: id.into(),
                    slug: id.into(),
                    name: id.into(),
                    project: None,
                    kind,
                    state: TaskState::Active,
                    created_at: 0,
                    last_activity: 0,
                    parent_id: Some(parent.into()),
                    placement: crate::placement::for_new(),
                    run_state: RunState::Run,
                    run_state_by: None,
                    max_children: None,
                })
                .unwrap();
        }
        let w = || app.db.get_task("w").unwrap().unwrap();
        assert!(may_accept_now(&app, &w(), true));
        assert!(!may_accept_now(&app, &w(), false));
        for (id, state) in [("m", RunState::Paused), ("w", RunState::Stopped)] {
            app.db
                .set_run_state(id, state, Some(&Setter::User))
                .unwrap();
            assert!(!may_accept_now(&app, &w(), true), "{id} {state:?}");
            app.db.set_run_state(id, RunState::Run, None).unwrap();
        }
        assert!(may_accept_now(&app, &w(), true));
    }

    #[test]
    fn no_prompt_and_a_hook_or_an_exit_leave_the_pane_alone() {
        let pane = FakeClaude::new("no-prompt", 0);
        assert_eq!(run_watch(&pane, true), Outcome::NoPrompt);
        pane.hooked.set(true);
        assert_eq!(run_watch(&pane, true), Outcome::Started);
        let gone = FakeClaude::new("trust-no-selected", 0);
        gone.alive.set(false);
        assert_eq!(run_watch(&gone, true), Outcome::Gone);
        assert!(pane.pressed.borrow().is_empty());
        assert!(gone.pressed.borrow().is_empty());
    }

    #[test]
    fn accepted_but_silent_is_reported() {
        let pane = FakeClaude::new("trust-yes-selected", 0);
        pane.hooks.set(false);
        let (outcome, log) = run_logged(&pane, true, quick());
        assert_eq!(outcome, Outcome::AcceptedNoHook);
        // Waited for twice, the second time with nothing to press.
        assert_eq!(*pane.pressed.borrow(), ["Enter"]);
        assert!(log
            .iter()
            .any(|l| l.contains("no hook") && l.contains("once more")));
    }

    fn sample(name: &str) -> Look {
        look(&ClaudeCode, &pane_fixture(name))
    }

    #[test]
    fn a_look_reads_the_selection_from_the_pane_text() {
        assert_eq!(sample("trust-yes-selected"), Look::Confirm(vec!["Enter"]));
        assert_eq!(sample("trust-no-selected"), Look::Select(vec!["Down"]));
        let mid = pane_fixture("trust-no-selected").replace("❯ No, exit", "  No, exit");
        assert_eq!(look(&ClaudeCode, &mid), Look::Unsure);
        // Both options' text is there, but the cursor is on "No".
        let text = pane_fixture("trust-no-selected");
        assert!(text.contains("Yes, I trust this folder"));
        assert_ne!(look(&ClaudeCode, &text), sample("trust-yes-selected"));
        assert_eq!(sample("no-prompt"), Look::Gone);
        assert_eq!(look(&ClaudeCode, ""), Look::Gone);
    }

    #[test]
    fn next_step_presses_only_on_steady_looks() {
        let yes = sample("trust-yes-selected");
        let no = sample("trust-no-selected");
        let mid = Look::Unsure;
        let gone = sample("no-prompt");
        let next = |looks: &[&Look], pressed| {
            let looks: Vec<Look> = looks.iter().map(|l| (*l).clone()).collect();
            next_step(&looks, pressed, 3, 6)
        };
        let enter = Next::Press(vec!["Enter"]);
        let down = Next::Press(vec!["Down"]);
        use Pressed::*;
        // Yes selected: Enter, once steady, and never with "No" last.
        assert_eq!(next(&[&yes, &yes], Nothing), Next::Wait);
        assert_eq!(next(&[&yes, &yes, &yes], Nothing), enter);
        assert_eq!(next(&[&yes, &yes, &yes], Select), enter);
        assert_eq!(next(&[&yes, &yes, &no], Select), Next::Wait);
        assert_eq!(next(&[&no, &yes, &yes, &yes], Select), enter);
        // No selected: Down at first; after a Down, only once "Yes" was
        // seen, or the looks ran out.
        assert_eq!(next(&[&no, &no, &no], Nothing), down);
        assert_eq!(next(&[&no, &no, &no], Select), Next::Wait);
        assert_eq!(next(&[&yes, &no, &no, &no], Select), down);
        assert_eq!(next(&[&no; 6], Select), down);
        // Mid-render: nothing, then giving up.
        assert_eq!(next(&[&mid, &mid, &mid], Nothing), Next::Wait);
        assert!(matches!(next(&[&mid; 6], Nothing), Next::GiveUp(_)));
        assert!(matches!(
            next(&[&yes, &mid, &yes, &mid, &yes, &mid], Nothing),
            Next::GiveUp(_)
        ));
        // Prompt gone.
        assert_eq!(next(&[&yes, &gone, &gone], Confirm), Next::Wait);
        assert_eq!(next(&[&gone, &gone, &gone], Confirm), Next::Gone);
        assert_eq!(next(&[&gone, &gone, &gone], Nothing), Next::Gone);
        // After Enter, never another: a prompt still up gives up.
        assert_eq!(next(&[&yes, &yes, &yes], Confirm), Next::Wait);
        assert!(matches!(next(&[&yes; 6], Confirm), Next::GiveUp(_)));
        assert!(matches!(next(&[&no; 6], Confirm), Next::GiveUp(_)));
    }

    #[test]
    fn auto_trust_covers_tasks_not_reviews_unless_asked() {
        let home = Path::new("/h");
        let in_tasks = config::tasks_root(home).join("t");
        let config = |text: &str| toml::from_str::<Config>(text).unwrap();
        let off = Config::default();
        let on = config("[harness]\nauto_trust = true\n");
        let reviews = config("[harness]\nauto_trust = true\nauto_trust_reviews = true\n");

        assert!(!may_auto_trust(
            &off,
            home,
            Trust::Task(TaskKind::Work),
            &in_tasks
        ));
        assert!(may_auto_trust(
            &on,
            home,
            Trust::Task(TaskKind::Work),
            &in_tasks
        ));
        assert!(!may_auto_trust(&on, home, Trust::Review, &in_tasks));
        assert!(may_auto_trust(&reviews, home, Trust::Review, &in_tasks));
        // A manager follows `auto_trust` as a worker does.
        assert!(!may_auto_trust(
            &off,
            home,
            Trust::Task(TaskKind::Manager),
            &in_tasks
        ));
        assert!(may_auto_trust(
            &on,
            home,
            Trust::Task(TaskKind::Manager),
            &in_tasks
        ));
        // Never outside the tasks, nor for the assistant.
        assert!(!may_auto_trust(
            &on,
            home,
            Trust::Task(TaskKind::Work),
            Path::new("/elsewhere")
        ));
        assert!(!may_auto_trust(
            &on,
            home,
            Trust::Task(TaskKind::Manager),
            Path::new("/elsewhere")
        ));
        assert!(!may_auto_trust(
            &reviews,
            home,
            Trust::Task(TaskKind::Assistant),
            &in_tasks
        ));
    }
}
