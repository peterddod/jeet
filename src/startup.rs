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
//!   (see [`may_auto_trust`]), one key at a time, looking at the pane before
//!   each, and checks that a hook then reports;
//! - otherwise marks the task "needs input – trust folder" and tells the
//!   user with a notification, unless someone is attached to look at it.
//!
//! Review tasks are out of scope unless `auto_trust_reviews` is on as well:
//! their checkouts are other people's branches, and trusting them lets their
//! `.claude/settings.json` hooks and `.mcp.json` servers run.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::agent::AgentSpec;
use crate::config::{self, Config};
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::harness::{Harness, StartupPrompt, TrustStep};
use crate::notify::{self, Notification};
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

/// Whether jeet may accept the trust prompt by itself for `task`, in
/// `dir`: only with `auto_trust` on, only under `$JEET_HOME/tasks`, and for
/// a review task only with `auto_trust_reviews` on too. Never the
/// assistant's.
pub fn may_auto_trust(config: &Config, home: &Path, task: &TaskRecord, dir: &Path) -> bool {
    let (tasks, reviews) = config.auto_trust();
    let in_tasks = dir.starts_with(config::tasks_root(home));
    match task.kind {
        TaskKind::Work => tasks && in_tasks,
        TaskKind::Review => reviews && in_tasks,
        TaskKind::Assistant => false,
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
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            watch: Duration::from_secs(WATCH_SECS),
            hook_wait: Duration::from_secs(HOOK_WAIT_SECS),
            poll: Duration::from_millis(500),
        }
    }
}

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
}

/// Watch `pane` until `reported` (a hook has said something since the
/// launch), the agent is gone, or a prompt is found, accepting a trust
/// prompt when `auto`.
pub fn watch(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    auto: bool,
    limits: Limits,
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
            return match auto {
                true => accept(harness, pane, reported, limits),
                false => Ok(Outcome::NeedsUser),
            };
        }
        if Instant::now() >= deadline {
            return Ok(Outcome::NoPrompt);
        }
        thread::sleep(limits.poll);
    }
}

/// Accept the trust prompt: each key only once the pane shows what the
/// harness expects it to be pressed on, and confirm only once accepting is
/// seen selected.
fn accept(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    limits: Limits,
) -> Result<Outcome> {
    // A few presses at most, each given a moment to redraw.
    let mut presses = 0;
    let mut last: Option<TrustStep> = None;
    let deadline = Instant::now() + limits.hook_wait;
    loop {
        let step = harness.accept_trust(&pane.capture()?);
        match step.clone() {
            Some(TrustStep::Confirm(keys)) => {
                pane.press(&keys)?;
                return Ok(wait_for_hook(pane, reported, limits));
            }
            // The same step again before the pane has redrawn: look again
            // rather than press twice, unless it has had long enough.
            Some(TrustStep::Select(_)) if step == last && Instant::now() < deadline => {}
            Some(TrustStep::Select(keys)) if presses < 3 => {
                pane.press(&keys)?;
                presses += 1;
                last = step;
            }
            _ => return Ok(Outcome::CouldNotAccept),
        }
        if Instant::now() >= deadline {
            return Ok(Outcome::CouldNotAccept);
        }
        thread::sleep(limits.poll);
    }
}

fn wait_for_hook(pane: &dyn Pane, reported: &dyn Fn() -> bool, limits: Limits) -> Outcome {
    let deadline = Instant::now() + limits.hook_wait;
    while Instant::now() < deadline {
        if reported() {
            return Outcome::Accepted;
        }
        if !pane.alive() {
            break;
        }
        thread::sleep(limits.poll);
    }
    match reported() {
        true => Outcome::Accepted,
        false => Outcome::AcceptedNoHook,
    }
}

/// `jeet task-startup <task>`: watch the start of the task's agent, which
/// runs session `session` when jeet picked its id.
pub fn run(app: &App, task_id: &str, session: Option<&str>) -> Result<()> {
    let Some(task) = app.db.get_task(task_id)? else {
        anyhow::bail!("no task {task_id}");
    };
    let spec = AgentSpec::from_config(&app.config)?;
    let dir = tasks::task_dir(app, &task);
    let since = status::launched_at(&dir).unwrap_or_else(now_secs);
    let reported = || {
        let latest = match session {
            Some(id) => status::read_status(&dir, id),
            None => status::current_status(&dir),
        };
        latest.is_some_and(|s| s.at >= since)
    };
    let name = runtime::session_name(&task);
    let auto = may_auto_trust(&app.config, &app.home, &task, &dir);
    let outcome = watch(
        spec.harness,
        &TmuxPane(name.clone()),
        &reported,
        auto,
        Limits::default(),
    )?;
    eprintln!("jeet task-startup {}: {outcome:?}", task.slug);
    let tell = |body: &str| {
        if !tmux::attached(&name) {
            notify::notify(&Notification {
                title: "jeet",
                subtitle: Some(&task.name),
                body,
                url: None,
            });
        }
    };
    match outcome {
        Outcome::NeedsUser | Outcome::CouldNotAccept => {
            if let Some(id) = session {
                status::mark_startup_prompt(&dir, id, "trust folder", now_secs())?;
            }
            tell(&format!(
                "{} asks whether to trust the task's folder; `jeet task open {}` to answer",
                spec.harness.name(),
                task.slug
            ));
        }
        Outcome::AcceptedNoHook => tell(&format!(
            "jeet trusted the task's folder for {}, but no hook has reported since; \
             `jeet task open {}` to look",
            spec.harness.name(),
            task.slug
        )),
        Outcome::Started | Outcome::Gone | Outcome::NoPrompt | Outcome::Accepted => {}
    }
    Ok(())
}

/// Start `jeet task-startup` for the task's agent, just launched, in the
/// background: detached the way `store::launch` is, its output in the
/// task's `.jeet/startup.log`.
pub fn launch(app: &App, task: &TaskRecord, session: Option<&str>) -> Result<()> {
    if std::env::var(WATCH_ENV).is_ok_and(|v| v.trim() == "0") {
        return Ok(());
    }
    let dir = tasks::task_dir(app, task).join(tasks::MARKER_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let log = dir.join(LOG_FILE);
    let out = std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let mut cmd = Command::new("nohup");
    cmd.arg(exe).args(["task-startup", &task.id]);
    if let Some(id) = session {
        cmd.args(["--session", id]);
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
    use crate::harness::claude::ClaudeCode;

    fn pane_fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/panes/claude-code")
            .join(format!("{name}.txt"));
        std::fs::read_to_string(path).unwrap()
    }

    /// A Claude pane at its trust prompt, or past it: Down moves to "Yes",
    /// Enter on "Yes" trusts, Enter on "No" exits.
    struct FakeClaude {
        screen: RefCell<String>,
        alive: Cell<bool>,
        pressed: RefCell<Vec<String>>,
        /// Captures it takes to redraw after a key.
        lag: Cell<u32>,
        lagging: Cell<u32>,
        hooked: Cell<bool>,
    }

    impl FakeClaude {
        fn new(screen: &str, lag: u32) -> Self {
            Self {
                screen: RefCell::new(pane_fixture(screen)),
                alive: Cell::new(true),
                pressed: RefCell::default(),
                lag: Cell::new(lag),
                lagging: Cell::new(0),
                hooked: Cell::new(false),
            }
        }
    }

    impl Pane for FakeClaude {
        fn alive(&self) -> bool {
            self.alive.get()
        }

        fn capture(&self) -> Result<String> {
            if self.lagging.get() > 0 {
                self.lagging.set(self.lagging.get() - 1);
                return Ok(pane_fixture("trust-no-selected"));
            }
            Ok(self.screen.borrow().clone())
        }

        fn press(&self, keys: &[&str]) -> Result<()> {
            for key in keys {
                self.pressed.borrow_mut().push(key.to_string());
                let screen = self.screen.borrow().clone();
                let yes = screen.contains("❯ Yes");
                match *key {
                    "Down" => {
                        *self.screen.borrow_mut() = pane_fixture("trust-yes-selected");
                        self.lagging.set(self.lag.get());
                    }
                    "Enter" if yes => {
                        *self.screen.borrow_mut() = pane_fixture("no-prompt");
                        self.hooked.set(true);
                    }
                    "Enter" => self.alive.set(false),
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
        }
    }

    fn run_watch(pane: &FakeClaude, auto: bool) -> Outcome {
        watch(&ClaudeCode, pane, &|| pane.hooked.get(), auto, quick()).unwrap()
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
        let outcome = watch(&ClaudeCode, &pane, &|| false, true, quick()).unwrap();
        assert_eq!(outcome, Outcome::AcceptedNoHook);
    }

    #[test]
    fn auto_trust_covers_tasks_not_reviews_unless_asked() {
        let home = Path::new("/h");
        let task = |kind| TaskRecord {
            id: "t".into(),
            slug: "t".into(),
            name: "t".into(),
            project: None,
            kind,
            state: crate::db::tasks::TaskState::Active,
            created_at: 0,
            last_activity: 0,
        };
        let in_tasks = config::tasks_root(home).join("t");
        let config = |text: &str| toml::from_str::<Config>(text).unwrap();
        let off = Config::default();
        let on = config("[harness]\nauto_trust = true\n");
        let reviews = config("[harness]\nauto_trust = true\nauto_trust_reviews = true\n");

        assert!(!may_auto_trust(
            &off,
            home,
            &task(TaskKind::Work),
            &in_tasks
        ));
        assert!(may_auto_trust(&on, home, &task(TaskKind::Work), &in_tasks));
        assert!(!may_auto_trust(
            &on,
            home,
            &task(TaskKind::Review),
            &in_tasks
        ));
        assert!(may_auto_trust(
            &reviews,
            home,
            &task(TaskKind::Review),
            &in_tasks
        ));
        // Never outside the tasks, nor for the assistant.
        assert!(!may_auto_trust(
            &on,
            home,
            &task(TaskKind::Work),
            Path::new("/elsewhere")
        ));
        assert!(!may_auto_trust(
            &reviews,
            home,
            &task(TaskKind::Assistant),
            &in_tasks
        ));
    }
}
