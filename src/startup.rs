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
//!   looking at the pane before each, and checks that a hook then reports;
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
use crate::manager::run;
use crate::notify::{self, Notification};
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

/// Whether jeet may accept the trust prompt by itself for `task`, in
/// `dir`: only with `auto_trust` on, only under `$JEET_HOME/tasks`, and for
/// a review task only with `auto_trust_reviews` on too. A manager follows
/// `auto_trust` as a worker does: its parent starts it with nobody at its
/// pane. Never the assistant's.
pub fn may_auto_trust(config: &Config, home: &Path, task: &TaskRecord, dir: &Path) -> bool {
    let (tasks, reviews) = config.auto_trust();
    let in_tasks = dir.starts_with(config::tasks_root(home));
    match task.kind {
        TaskKind::Work | TaskKind::Manager => tasks && in_tasks,
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
    /// How many looks the trust prompt gets to redraw after a key,
    /// `redraw_poll` apart: counted, not timed, so a slow machine gets the
    /// same number of looks.
    pub redraw_looks: u32,
    pub redraw_poll: Duration,
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
        }
    }
}

/// How many of the pane's last lines a failure logs.
const LOG_LINES: usize = 12;

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

/// Accept the trust prompt: Down at most once, since a second could wrap
/// the selection back to "No", and Enter only once accepting is seen
/// selected. Each key waits for the pane to show what the harness expects
/// it to be pressed on, looking again while it redraws.
fn accept(
    harness: &dyn Harness,
    pane: &dyn Pane,
    reported: &dyn Fn() -> bool,
    auto: &dyn Fn() -> bool,
    limits: Limits,
    log: &dyn Fn(&str),
) -> Result<Outcome> {
    let gave_up = |why: &str, screen: &str| {
        // Answered some other way while jeet looked: not jeet's to report.
        if reported() {
            return Outcome::Started;
        }
        log(&format!(
            "could not accept the trust prompt: {why}; the pane ended:"
        ));
        for line in last_lines(screen, LOG_LINES) {
            log(&format!("  | {line}"));
        }
        Outcome::CouldNotAccept
    };
    // No longer to accept, a task paused since, say: the user answers.
    let withdrawn = || -> Result<Outcome> {
        log("no longer to accept: left for the user");
        Ok(Outcome::NeedsUser)
    };
    let (step, screen) = settle(harness, pane, limits, |step| step.is_some())?;
    if step.is_some() && !auto() {
        return withdrawn();
    }
    match step {
        Some(TrustStep::Confirm(keys)) => return confirm(pane, &keys, reported, limits, log),
        Some(TrustStep::Select(keys)) => {
            pane.press(&keys)?;
            log(&format!("pressed {}", keys.join(" ")));
        }
        None => return Ok(gave_up("no option it knows was seen selected", &screen)),
    }
    let started = Instant::now();
    let (step, screen) = settle(harness, pane, limits, |step| {
        matches!(step, Some(TrustStep::Confirm(_)))
    })?;
    match step {
        Some(TrustStep::Confirm(_)) if !auto() => withdrawn(),
        Some(TrustStep::Confirm(keys)) => {
            log(&format!(
                "accepting seen selected after {}ms",
                started.elapsed().as_millis()
            ));
            confirm(pane, &keys, reported, limits, log)
        }
        Some(TrustStep::Select(_)) => Ok(gave_up(
            &format!(
                "still not on accepting after {} looks over {}ms since the select key",
                limits.redraw_looks,
                started.elapsed().as_millis()
            ),
            &screen,
        )),
        None => Ok(gave_up(
            "no option it knows was seen selected after the select key",
            &screen,
        )),
    }
}

/// Look at the pane until `done` with the step it shows, or for
/// `limits.redraw_looks` looks: the last step and the pane it came from.
fn settle(
    harness: &dyn Harness,
    pane: &dyn Pane,
    limits: Limits,
    done: impl Fn(&Option<TrustStep>) -> bool,
) -> Result<(Option<TrustStep>, String)> {
    let mut looks = 0;
    loop {
        let screen = pane.capture()?;
        let step = harness.accept_trust(&screen);
        looks += 1;
        if done(&step) || looks >= limits.redraw_looks.max(1) {
            return Ok((step, screen));
        }
        thread::sleep(limits.redraw_poll);
    }
}

fn confirm(
    pane: &dyn Pane,
    keys: &[&str],
    reported: &dyn Fn() -> bool,
    limits: Limits,
    log: &dyn Fn(&str),
) -> Result<Outcome> {
    pane.press(keys)?;
    log(&format!("pressed {}", keys.join(" ")));
    Ok(wait_for_hook(pane, reported, limits))
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
    let in_scope = may_auto_trust(&app.config, &app.home, &task, &dir);
    // Not while it is paused or stopped: accepting would let its brief run.
    let auto = || in_scope && matches!(run::hold(app, &task), Ok(None));
    let outcome = watch(
        spec.harness,
        &TmuxPane(name.clone()),
        &reported,
        &auto,
        Limits::default(),
        &|line| eprintln!("jeet task-startup {}: {line}", task.slug),
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
            // No hook fires for this prompt, so its parent hears of it here,
            // once per launch however often startup is watched.
            if task.parent_id.is_some() {
                let body = "waiting for the user to trust the folder";
                let waiting = StatusEvent::Waiting;
                if let Err(e) = reports::auto_status(app, &task, waiting, body, since, 0) {
                    eprintln!("jeet task-startup {}: warning: {e:#}", task.slug);
                }
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
    use crate::db::tasks::RunState;
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
        /// Captures it takes to redraw after a key, showing `between`.
        lag: Cell<u32>,
        lagging: Cell<u32>,
        between: RefCell<String>,
        /// Whether Down moves the cursor at all.
        redraws: Cell<bool>,
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
                between: RefCell::new(pane_fixture("trust-no-selected")),
                redraws: Cell::new(true),
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
                return Ok(self.between.borrow().clone());
            }
            Ok(self.screen.borrow().clone())
        }

        fn press(&self, keys: &[&str]) -> Result<()> {
            for key in keys {
                self.pressed.borrow_mut().push(key.to_string());
                let screen = self.screen.borrow().clone();
                let yes = screen.contains("❯ Yes");
                match *key {
                    "Down" if self.redraws.get() => {
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
            redraw_looks: 30,
            redraw_poll: Duration::ZERO,
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
        assert!(log.iter().any(|l| l.starts_with("accepting seen selected")));
    }

    #[test]
    fn the_redraw_gets_its_looks_and_no_more() {
        let limits = Limits {
            redraw_looks: 5,
            ..quick()
        };
        // Four looks still show "No"; the fifth, the last, shows "Yes".
        let in_time = FakeClaude::new("trust-no-selected", 4);
        assert_eq!(run_logged(&in_time, true, limits).0, Outcome::Accepted);
        assert_eq!(*in_time.pressed.borrow(), ["Down", "Enter"]);
        let late = FakeClaude::new("trust-no-selected", 5);
        assert_eq!(run_logged(&late, true, limits).0, Outcome::CouldNotAccept);
        assert_eq!(*late.pressed.borrow(), ["Down"]);
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
        // One Down only: a second could wrap round to "No".
        assert_eq!(*pane.pressed.borrow(), ["Down"]);
        assert!(pane.alive.get());
        assert!(log.iter().any(|l| l.contains("still not on accepting")));
        assert!(log.iter().any(|l| l.contains("❯ No, exit")));
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
        let outcome = watch(&ClaudeCode, &pane, &|| false, &|| true, quick(), &|_| {}).unwrap();
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
            parent_id: None,
            placement: crate::placement::Placement::Local,
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
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
        // A manager follows `auto_trust` as a worker does.
        assert!(!may_auto_trust(
            &off,
            home,
            &task(TaskKind::Manager),
            &in_tasks
        ));
        assert!(may_auto_trust(
            &on,
            home,
            &task(TaskKind::Manager),
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
            &on,
            home,
            &task(TaskKind::Manager),
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
