//! Notifications to the user, through the backends `[notify] via` names.
//!
//! [`event`] is the one way in. It only queues the notification, to
//! `$JEET_HOME/run/notify/outbox.jsonl` (owner-only), for the sender
//! (`jeet notify serve`, [`sender`]) to route and send at once. Routing —
//! `[[notify.rules]]`, deduplication by `(why, ref)`, `max_per_hour` — is
//! [`route`]'s. Only the sender reads a backend's token ([`Tokens`]); with
//! no sender running, [`when_no_sender`] decides what the process that
//! queued it does.
//!
//! A backend is one [`Notifier`] impl and its line in [`backend`].

mod command;
mod desktop;
mod route;
pub mod sender;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, NotifyConfig, NotifyLevel};
use crate::context::App;
use crate::db::tasks::TaskKind;
use crate::worktrees::now_secs;

pub const COMMAND_ENV: &str = "JEET_NOTIFY_CMD";

/// Longest title or subtitle shown; Notification Center cuts them shorter.
const TITLE_MAX: usize = 120;

/// Longest body passed on.
const BODY_MAX: usize = 500;

/// Longest text kept in the outbox: whatever a backend shows is shorter.
const QUEUED_MAX: usize = 4000;

/// How long a notifier is waited for before it is left running.
const NOTIFIER_WAIT: Duration = Duration::from_secs(2);

/// Why the user is told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Why {
    /// Only the user can move it on: a question, a prompt.
    NeedsUser,
    /// Work the user started is done.
    Finished,
    /// Something has stopped moving.
    Stuck,
    /// For the user's information.
    #[default]
    Info,
}

impl Why {
    pub fn name(self) -> &'static str {
        match self {
            Why::NeedsUser => "needs-user",
            Why::Finished => "finished",
            Why::Stuck => "stuck",
            Why::Info => "info",
        }
    }
}

/// One notification, as queued.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub why: Why,
    /// What happened, for `[[notify.rules]] kind`: `question`, `review`,
    /// `action`, a GitHub event's kind…
    pub kind: String,
    /// Where it comes from; `None` for what comes from no task (GitHub, a
    /// script).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<NotifyLevel>,
    /// The slug of the task it comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// What it is about, e.g. `r41`: one is sent per `(why, ref)`. `None`
    /// is never deduplicated.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub body: String,
    /// Opened when it is clicked, where the backend can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Set on one already routed and put back for the sender: the backends
    /// it is still to go through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
}

/// The level of a task of `kind`.
pub fn level(kind: TaskKind) -> NotifyLevel {
    match kind {
        TaskKind::Assistant => NotifyLevel::Assistant,
        TaskKind::Manager => NotifyLevel::Manager,
        TaskKind::Work => NotifyLevel::Task,
    }
}

/// A way to send notifications: one per `via` name.
pub trait Notifier {
    /// Its name in `[notify] via`.
    fn name(&self) -> &str;

    /// Whether it sends with a token, which only the sender reads: until
    /// one runs, what it would send waits in the outbox.
    fn needs_token(&self) -> bool {
        false
    }

    /// Send `event`, best effort: a backend that cannot still returns once
    /// it has tried.
    fn send(&self, event: &Event, tokens: &dyn Tokens) -> Result<()>;
}

/// Where a backend's token comes from. Only the sender is handed one that
/// gives any ([`sender::tokens`]).
pub trait Tokens {
    // Read by the first backend that needs a token.
    #[allow(dead_code)]
    fn token(&self, backend: &str) -> Result<Option<String>>;
}

/// No tokens: what every process but the sender has.
pub struct NoTokens;

impl Tokens for NoTokens {
    fn token(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// The backend `via` calls `name`, or `None` for a name jeet does not know
/// (or `command` with no `[notify] command`). `$JEET_NOTIFY_CMD`, in this
/// process, stands in for `desktop`.
fn backend(name: &str, config: &NotifyConfig) -> Option<Box<dyn Notifier>> {
    match name {
        "desktop" => match override_command() {
            Some(command) => Some(Box::new(command::Command {
                name: "desktop",
                command,
            })),
            None => Some(Box::new(desktop::Desktop)),
        },
        "command" => config.command().map(|command| {
            Box::new(command::Command {
                name: "command",
                command: command.to_string(),
            }) as Box<dyn Notifier>
        }),
        _ => None,
    }
}

fn backends(config: &NotifyConfig) -> Vec<Box<dyn Notifier>> {
    config
        .via()
        .iter()
        .filter_map(|name| {
            let found = backend(name, config);
            if found.is_none() {
                eprintln!("jeet: [notify] via: no backend `{name}`; skipped");
            }
            found
        })
        .collect()
}

fn override_command() -> Option<String> {
    std::env::var(COMMAND_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Tell the user `event`, best effort: a notification that cannot be
/// queued or sent is not worth failing anything over.
pub fn event(app: &App, event: Event) {
    event_in(&app.home, event)
}

/// [`event`], for a caller with only jeet's home.
pub fn event_in(home: &Path, event: Event) {
    if let Err(e) = enqueue(home, &event) {
        eprintln!("jeet: could not queue a notification: {e:#}");
        return;
    }
    if !sender_running(home) {
        when_no_sender(home);
    }
}

/// What the process that queued a notification does while no sender runs.
/// The user's choice: send through the backends that need no token, here
/// and now, as jeet did before it had a sender; what a token backend would
/// send waits in the outbox for one, and [`notice`] says so.
fn when_no_sender(home: &Path) {
    if let Err(e) = drain(home, &NoTokens, false) {
        eprintln!("jeet: could not send a notification: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------

/// `$JEET_HOME/run/notify`: the outbox, its lock, the routing state and the
/// sender's lock. Its owner's only.
pub fn dir(home: &Path) -> PathBuf {
    home.join("run").join("notify")
}

pub fn outbox_path(home: &Path) -> PathBuf {
    dir(home).join("outbox.jsonl")
}

fn state_path(home: &Path) -> PathBuf {
    dir(home).join("state.json")
}

fn sender_lock_path(home: &Path) -> PathBuf {
    dir(home).join("sender.lock")
}

/// [`dir`], made owner-only if need be.
fn make_dir(home: &Path) -> Result<PathBuf> {
    let dir = dir(home);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("make {} owner-only", dir.display()))?;
    }
    Ok(dir)
}

/// Open `path` in `dir`, owner-only, refusing anything but a regular file
/// (a FIFO would hang the open, a link would lead elsewhere).
fn open_owned(path: &Path, opts: &mut OpenOptions) -> Result<File> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        bail!("{} is not a regular file", path.display());
    }
    let file = opts
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(file)
}

/// The outbox, opened and locked: queueing and taking from it are under
/// this one lock.
fn lock_outbox(home: &Path) -> Result<File> {
    make_dir(home)?;
    let path = outbox_path(home);
    let file = open_owned(
        &path,
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .append(true),
    )?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

fn cut(text: &mut String) {
    if let Some((at, _)) = text.char_indices().nth(QUEUED_MAX) {
        text.truncate(at);
    }
}

/// Add `event` to the outbox.
pub fn enqueue(home: &Path, event: &Event) -> Result<()> {
    let mut event = event.clone();
    cut(&mut event.title);
    cut(&mut event.body);
    if let Some(subtitle) = &mut event.subtitle {
        cut(subtitle);
    }
    let mut line = serde_json::to_string(&event)?;
    line.push('\n');
    let mut outbox = lock_outbox(home)?;
    outbox
        .write_all(line.as_bytes())
        .context("write the outbox")?;
    Ok(())
}

/// Everything in the locked `outbox`, which is left empty. A line that is
/// not an event is dropped, with a word on stderr.
fn take(outbox: &mut File) -> Result<Vec<Event>> {
    let mut text = String::new();
    outbox
        .read_to_string(&mut text)
        .context("read the outbox")?;
    outbox.set_len(0).context("empty the outbox")?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(event) => Some(event),
            Err(e) => {
                eprintln!("jeet: dropped a notification jeet could not read: {e}");
                None
            }
        })
        .collect())
}

fn load_state(home: &Path) -> route::State {
    let path = state_path(home);
    let Ok(mut file) = open_owned(&path, OpenOptions::new().read(true)) else {
        return route::State::default();
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return route::State::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_state(home: &Path, state: &route::State) -> Result<()> {
    let path = state_path(home);
    let tmp = dir(home).join("state.json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = open_owned(&tmp, OpenOptions::new().create_new(true).write(true))?;
    file.write_all(serde_json::to_string(state)?.as_bytes())?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
}

/// Route and send what is in the outbox. The sender (`as_sender`) sends
/// through every backend; any other process only through those that need
/// no token, and puts the rest back for the sender.
pub fn drain(home: &Path, tokens: &dyn Tokens, as_sender: bool) -> Result<()> {
    let config = match config::load_or_create(home) {
        Ok(config) => config.notify,
        Err(e) => {
            eprintln!("jeet: [notify] unreadable, so its defaults: {e:#}");
            NotifyConfig::default()
        }
    };
    let backends = backends(&config);
    let sends = {
        let mut outbox = lock_outbox(home)?;
        let events = take(&mut outbox)?;
        let mut state = load_state(home);
        let now = now_secs();
        let mut ready: Vec<Event> = state.unfold(&config, now).into_iter().collect();
        for event in events {
            match event.only.is_empty() {
                true => ready.extend(state.admit(&config, event, now)),
                false => ready.push(event),
            }
        }
        let (sends, kept) = plan(ready, &backends, as_sender);
        for event in &kept {
            let mut line = serde_json::to_string(event)?;
            line.push('\n');
            outbox.write_all(line.as_bytes())?;
        }
        save_state(home, &state)?;
        sends
    };
    for (event, via) in sends {
        for i in via {
            let backend = &backends[i];
            if let Err(e) = backend.send(&event, tokens) {
                eprintln!("jeet: notify via {}: {e:#}", backend.name());
            }
        }
    }
    Ok(())
}

/// For each of `ready`, the backends (by index) to send it through now,
/// and those put back for the sender, as events marked `only` them.
fn plan(
    ready: Vec<Event>,
    backends: &[Box<dyn Notifier>],
    as_sender: bool,
) -> (Vec<(Event, Vec<usize>)>, Vec<Event>) {
    let mut sends = Vec::new();
    let mut kept = Vec::new();
    for event in ready {
        let mut now = Vec::new();
        let mut later = Vec::new();
        for (i, backend) in backends.iter().enumerate() {
            if !event.only.is_empty() && !event.only.iter().any(|n| n == backend.name()) {
                continue;
            }
            match as_sender || !backend.needs_token() {
                true => now.push(i),
                false => later.push(backend.name().to_string()),
            }
        }
        if !later.is_empty() {
            kept.push(Event {
                only: later,
                ..event.clone()
            });
        }
        if !now.is_empty() {
            sends.push((event, now));
        }
    }
    (sends, kept)
}

/// Whether a sender (`jeet notify serve`) holds its lock.
pub fn sender_running(home: &Path) -> bool {
    let path = sender_lock_path(home);
    let Ok(file) = open_owned(&path, OpenOptions::new().read(true)) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// A line saying notifications wait for a sender that is not running: only
/// while a backend in `via` needs a token, or something waits for one.
pub fn notice(home: &Path, config: &NotifyConfig) -> Option<String> {
    let waiting = std::fs::metadata(outbox_path(home)).is_ok_and(|m| m.len() > 0);
    let token = backends_needing_token(config);
    if token.is_empty() && !waiting {
        return None;
    }
    if sender_running(home) {
        return None;
    }
    let what = match token.is_empty() {
        true => "notifications".to_string(),
        false => format!("{} notifications", token.join(", ")),
    };
    Some(format!(
        "{what} wait in the outbox: no sender runs (`jeet notify install`)"
    ))
}

fn backends_needing_token(config: &NotifyConfig) -> Vec<String> {
    config
        .via()
        .iter()
        .filter(|name| backend(name, config).is_some_and(|b| b.needs_token()))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Running a notifier
// ---------------------------------------------------------------------------

/// A program to run: its argv, and what to add to its environment.
#[derive(Debug, PartialEq, Eq)]
struct Invocation {
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

/// Run `invocation`. The notifier gets [`NOTIFIER_WAIT`] to finish and is
/// then left to itself: `terminal-notifier` blocks until Notification
/// Center grants it permission, which may be never, and the action being
/// reported must not hang on that.
fn run(invocation: &Invocation) {
    let Some((program, args)) = invocation.argv.split_first() else {
        return;
    };
    let child = Command::new(program)
        .args(args)
        .envs(invocation.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return;
    };
    let deadline = Instant::now() + NOTIFIER_WAIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
    // Still running: reap it whenever it does finish, if this process lasts.
    thread::spawn(move || child.wait());
}

/// Whether `url` may be opened from a notification: a web page, nothing
/// else — not a file, not an app's own scheme.
pub fn openable_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `text` on one line: line breaks and tabs become spaces, other control
/// characters are dropped, and it is cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let cut: String = flat.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn text_is_flattened_and_cut() {
        assert_eq!(one_line(" a\nb\tc\u{1b}[2J ", 20), "a b c[2J");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn only_web_pages_are_openable() {
        assert!(openable_url("https://github.com/acme/widget/pull/7"));
        assert!(openable_url("http://localhost:3000"));
        for bad in [
            "file:///etc/passwd",
            "javascript:x",
            "https://a b",
            "ssh://x",
        ] {
            assert!(!openable_url(bad), "{bad}");
        }
    }

    #[test]
    fn an_event_is_queued_as_one_json_line_with_ref() {
        let home = tempfile::tempdir().unwrap();
        let event = Event {
            why: Why::NeedsUser,
            kind: "question".into(),
            level: Some(NotifyLevel::Manager),
            task: Some("api".into()),
            reference: Some("r41".into()),
            title: "Question #41".into(),
            link: Some("jeet://later".into()),
            ..Event::default()
        };
        enqueue(home.path(), &event).unwrap();
        let text = std::fs::read_to_string(outbox_path(home.path())).unwrap();
        assert_eq!(
            text,
            "{\"why\":\"needs-user\",\"kind\":\"question\",\"level\":\"manager\",\"task\":\"api\",\
             \"ref\":\"r41\",\"title\":\"Question #41\",\"body\":\"\",\"link\":\"jeet://later\"}\n"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&outbox_path(home.path())), 0o600);
        assert_eq!(mode(&dir(home.path())), 0o700);
        let mut outbox = lock_outbox(home.path()).unwrap();
        assert_eq!(take(&mut outbox).unwrap(), [event]);
        assert_eq!(
            std::fs::metadata(outbox_path(home.path())).unwrap().len(),
            0
        );
    }

    #[test]
    fn a_fifo_in_place_of_the_outbox_is_refused_not_waited_on() {
        let home = tempfile::tempdir().unwrap();
        make_dir(home.path()).unwrap();
        let path = outbox_path(home.path());
        let made = Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(made.success());
        let e = enqueue(home.path(), &Event::default()).unwrap_err();
        assert!(format!("{e:#}").contains("not a regular file"), "{e:#}");
    }

    /// A backend that notes what it is sent.
    struct Fake {
        name: &'static str,
        token: bool,
        sent: RefCell<Vec<String>>,
    }

    impl Notifier for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn needs_token(&self) -> bool {
            self.token
        }
        fn send(&self, event: &Event, _: &dyn Tokens) -> Result<()> {
            self.sent.borrow_mut().push(event.title.clone());
            Ok(())
        }
    }

    fn fake(name: &'static str, token: bool) -> Box<dyn Notifier> {
        Box::new(Fake {
            name,
            token,
            sent: RefCell::default(),
        })
    }

    #[test]
    fn without_the_sender_token_backends_wait_and_the_rest_send() {
        let backends = [fake("desktop", false), fake("slack", true)];
        let event = Event {
            title: "t".into(),
            ..Event::default()
        };
        let (sends, kept) = plan(vec![event.clone()], &backends, false);
        assert_eq!(sends, [(event.clone(), vec![0])]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].only, ["slack"]);

        // The sender, given the one put back, sends it only to slack.
        let (sends, kept) = plan(kept, &backends, true);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].1, [1]);
        assert!(kept.is_empty());

        // The sender sends a new one everywhere.
        let (sends, _) = plan(vec![event], &backends, true);
        assert_eq!(sends[0].1, [0, 1]);
    }

    #[test]
    fn no_sender_is_running_until_one_holds_the_lock() {
        let home = tempfile::tempdir().unwrap();
        assert!(!sender_running(home.path()));
        make_dir(home.path()).unwrap();
        let lock = File::create(sender_lock_path(home.path())).unwrap();
        assert!(!sender_running(home.path()));
        lock.lock().unwrap();
        // flock locks are per open file: another open of it sees it held.
        assert!(sender_running(home.path()));
        drop(lock);
        assert!(!sender_running(home.path()));
    }

    #[test]
    fn the_notice_is_only_for_what_waits_on_a_token() {
        let home = tempfile::tempdir().unwrap();
        let config = NotifyConfig::default();
        assert_eq!(notice(home.path(), &config), None);
        // Something left for the sender.
        make_dir(home.path()).unwrap();
        std::fs::write(outbox_path(home.path()), "{}\n").unwrap();
        let said = notice(home.path(), &config).unwrap();
        assert!(said.contains("jeet notify install"), "{said}");
    }
}
