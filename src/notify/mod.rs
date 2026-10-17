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
pub mod ntfy;
mod route;
pub mod scope;
pub mod secrets;
pub mod sender;
pub mod slack;
mod web;

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

/// How long one left running is given before it is ended.
const NOTIFIER_KILL: Duration = Duration::from_secs(120);

/// How long a token backend's first failed send waits to be tried again;
/// each failure after doubles it, up to [`RETRY_MAX_SECS`].
const RETRY_FIRST_SECS: i64 = 30;
const RETRY_MAX_SECS: i64 = 3600;

/// Failed sends after which a notification is dropped, with a word in the
/// log: with the waits above, about a day, when it is news to nobody.
const GIVE_UP_TRIES: u32 = 30;

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
    /// The PR it is about, as a web address, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    /// When what it tells of happened, in unix seconds; unset, [`enqueue`]
    /// sets when it was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    /// Set on one already routed and put back for the sender: the backends
    /// it is still to go through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
    /// Failed sends so far, for one put back after a failure.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tries: u32,
    /// Not tried again before this, in unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A backend's error that is already logged: not logged again.
#[derive(Debug)]
pub struct Said;

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("already said")
    }
}

impl std::error::Error for Said {}

/// A backend's error that is not this notification's but the backend's
/// (unreachable, no token, refused): the rest of what it has to send in
/// the round waits too.
#[derive(Debug)]
pub struct Down(pub anyhow::Error);

impl std::fmt::Display for Down {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Down {}

/// The level of a task of `kind`.
pub fn level(kind: TaskKind) -> NotifyLevel {
    match kind {
        TaskKind::Assistant => NotifyLevel::Assistant,
        TaskKind::Manager => NotifyLevel::Manager,
        TaskKind::Work | TaskKind::Setup => NotifyLevel::Task,
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
/// gives any ([`secrets::for_sender`]).
pub trait Tokens {
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
/// process, stands in for `desktop`. `home` is the jeet home it sends for.
fn backend(name: &str, config: &NotifyConfig, home: &Path) -> Option<Box<dyn Notifier>> {
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
        slack::NAME => Some(Box::new(slack::Slack {
            user: config.slack_user().map(str::to_string),
            home: home.to_path_buf(),
            // The test binary never calls Slack.
            #[cfg(not(test))]
            http: Box::new(slack::http()),
            #[cfg(test)]
            http: Box::new(web::Offline),
        })),
        ntfy::NAME => Some(Box::new(ntfy::Ntfy {
            topic: config.ntfy_topic().map(str::to_string),
            home: home.to_path_buf(),
            // The test binary never calls a server.
            #[cfg(not(test))]
            http: Box::new(ntfy::http(config.ntfy_server())),
            #[cfg(test)]
            http: Box::new(web::Offline),
        })),
        _ => None,
    }
}

/// The backend names `via` may list.
pub const BACKENDS: &[&str] = &["desktop", "command", "slack", "ntfy"];

/// What in `[notify]` would send nothing where it was meant to: a `via`
/// name jeet has no backend for, or `command` listed with no `[notify]
/// command` to run. Sending skips either, with a warning, so this is for
/// checking a config before it is used.
pub fn check(config: &NotifyConfig) -> Result<()> {
    for name in config.via() {
        if !BACKENDS.contains(&name.as_str()) {
            bail!(
                "[notify] via: no backend `{name}`; {} has {}",
                crate::setup::PRODUCT,
                BACKENDS.join(", ")
            );
        }
        if name == "command" && config.command().is_none() {
            bail!("[notify] via lists `command`, but `[notify] command` is not set");
        }
        if name == slack::NAME && config.slack_user().is_none() {
            bail!(
                "[notify] via lists `slack`, but `[notify.slack] user` is not set (`jeet notify setup slack`)"
            );
        }
        if name == ntfy::NAME {
            check_ntfy(config)?;
        }
    }
    Ok(())
}

/// `[notify.ntfy]`: a topic ntfy takes, on an http(s) server.
fn check_ntfy(config: &NotifyConfig) -> Result<()> {
    let Some(topic) = config.ntfy_topic() else {
        bail!("[notify] via lists `ntfy`, but `[notify.ntfy] topic` is not set (`jeet notify setup ntfy`)");
    };
    if !ntfy::valid_topic(topic) {
        bail!("[notify.ntfy] topic: {topic:?} is not an ntfy topic (1 to 64 letters, digits, `-` or `_`)");
    }
    let server = config.ntfy_server();
    if !openable_url(server) {
        bail!("[notify.ntfy] server: {server:?} is not an http(s) address");
    }
    Ok(())
}

fn backends(config: &NotifyConfig, home: &Path) -> Vec<Box<dyn Notifier>> {
    config
        .via()
        .iter()
        .filter_map(|name| {
            let found = backend(name, config, home);
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
    // The test binary sends nothing: what it queues stays in the outbox,
    // for its tests to read.
    if !sender_running(home) && !cfg!(test) {
        when_no_sender(home);
    }
}

/// What the outbox holds, for tests.
#[cfg(test)]
pub fn queued(home: &Path) -> Vec<Event> {
    match lock_outbox(home) {
        Ok(mut outbox) => read_all(&mut outbox).unwrap(),
        Err(_) => Vec::new(),
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
    event.at.get_or_insert_with(now_secs);
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

/// Everything in the locked `outbox`, left as it is. A line that is not
/// an event (or not text) is dropped, with a word on stderr.
fn read_all(outbox: &mut File) -> Result<Vec<Event>> {
    let mut bytes = Vec::new();
    outbox.read_to_end(&mut bytes).context("read the outbox")?;
    let text = String::from_utf8_lossy(&bytes);
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

/// What [`drain`] left in the outbox.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// The outbox's length once written back: anything longer has been
    /// queued since.
    pub left: u64,
    /// When the soonest of what waits after a failed send is due.
    pub retry_at: Option<i64>,
}

/// Route and send what is in the outbox. The sender (`as_sender`) sends
/// through every backend; any other process only through those that need
/// no token, and puts the rest back for the sender. What a token backend
/// fails to send is put back for it, to be tried again after a while
/// ([`retry`]).
pub fn drain(home: &Path, tokens: &dyn Tokens, as_sender: bool) -> Result<Drained> {
    // A config jeet cannot read keeps what is queued: sending it without
    // the user's rules could send what they said not to.
    let config = config::load_or_create(home)
        .context("read config.toml, so notifications wait in the outbox")?
        .notify;
    let backends = backends(&config, home);
    let now = now_secs();
    let (sends, mut kept, written) = {
        let mut outbox = lock_outbox(home)?;
        let events = read_all(&mut outbox)?;
        let mut state = load_state(home);
        let mut ready: Vec<Event> = state.unfold(&config, now).into_iter().collect();
        let mut waiting = Vec::new();
        for mut event in events {
            match event.only.is_empty() {
                true => ready.extend(state.admit(&config, event, now)),
                // Only ever put back for a token backend: `only` naming
                // any other would let a line skip the rules.
                false => {
                    event.only.retain(|name| needs_token(name, &config, home));
                    if event.only.is_empty() {
                        continue;
                    }
                    match event.retry_at.is_some_and(|at| at > now) {
                        true => waiting.push(event),
                        false => ready.push(event),
                    }
                }
            }
        }
        let (sends, mut kept) = plan(ready, &backends, as_sender);
        kept.extend(waiting);
        let mut lines = String::new();
        for event in &kept {
            lines.push_str(&serde_json::to_string(event)?);
            lines.push('\n');
        }
        // Emptied only once all of it is routed, then what waits for the
        // sender written back, under the one lock.
        outbox.set_len(0).context("empty the outbox")?;
        // What is routed is sent whatever happens here.
        if let Err(e) = outbox.write_all(lines.as_bytes()) {
            eprintln!("jeet: could not keep notifications for the sender: {e}");
        }
        if let Err(e) = save_state(home, &state) {
            eprintln!("jeet: could not save what notifications were sent: {e:#}");
        }
        (sends, kept, lines.len() as u64)
    };
    let mut failed = Vec::new();
    // A token backend that failed once in this drain is not tried again in
    // it: Slack down or rate limiting would otherwise cost a timeout each.
    // When each backend found down in this round is to be tried again.
    let mut down: Vec<Option<i64>> = vec![None; backends.len()];
    for (event, via) in sends {
        for i in via {
            let backend = &backends[i];
            // Not tried: put back to wait as long as the one that failed,
            // its own tries as they were.
            if let Some(at) = down[i] {
                failed.push(Event {
                    only: vec![backend.name().to_string()],
                    retry_at: Some(at),
                    ..event.clone()
                });
                continue;
            }
            let Err(e) = backend.send(&event, tokens) else {
                continue;
            };
            if !e.is::<Said>() {
                eprintln!("jeet: notify via {}: {e:#}", backend.name());
            }
            if !backend.needs_token() {
                continue;
            }
            let again = retry(&event, backend.name(), now_secs());
            if e.is::<Down>() || e.is::<Said>() {
                down[i] = again.retry_at;
            }
            match again.tries >= GIVE_UP_TRIES {
                true => eprintln!(
                    "jeet: gave up sending {:?} via {} after {} tries",
                    event.title,
                    backend.name(),
                    again.tries
                ),
                false => failed.push(again),
            }
        }
    }
    let mut outbox = lock_outbox(home)?;
    // What was queued while it sent makes the outbox longer than this.
    let mut left = written;
    for event in &failed {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        outbox
            .write_all(line.as_bytes())
            .context("put a notification back in the outbox")?;
        left += line.len() as u64;
    }
    kept.extend(failed);
    Ok(Drained {
        left,
        retry_at: kept.iter().filter_map(|e| e.retry_at).min(),
    })
}

/// `event`, put back for the backend `name` after a failed send: tried
/// again after [`RETRY_FIRST_SECS`], doubled each failure after.
fn retry(event: &Event, name: &str, now: i64) -> Event {
    let tries = event.tries.saturating_add(1);
    let wait = (RETRY_FIRST_SECS << (tries - 1).min(16)).min(RETRY_MAX_SECS);
    Event {
        only: vec![name.to_string()],
        tries,
        retry_at: Some(now + wait),
        ..event.clone()
    }
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
    // Shared, so probes never hold each other off; a sender starting just
    // then retries ([`sender::serve`]).
    match file.try_lock_shared() {
        // Unlocked, not only closed: a child forked meanwhile would hold
        // this probe's lock until it execs, and a sender find it taken.
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// A line saying notifications wait for a sender that is not running: only
/// while a backend in `via` needs a token, or something waits for one.
pub fn notice(home: &Path, config: &NotifyConfig) -> Option<String> {
    let waiting = std::fs::metadata(outbox_path(home)).is_ok_and(|m| m.len() > 0);
    let token = backends_needing_token(config, home);
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

fn backends_needing_token(config: &NotifyConfig, home: &Path) -> Vec<String> {
    config
        .via()
        .into_iter()
        .filter(|name| needs_token(name, config, home))
        .collect()
}

fn needs_token(name: &str, config: &NotifyConfig, home: &Path) -> bool {
    backend(name, config, home).is_some_and(|b| b.needs_token())
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
    // Still running: reap it whenever it does finish, if this process lasts,
    // and end it after a while, so a long-lived sender does not gather them.
    thread::spawn(move || {
        let deadline = Instant::now() + NOTIFIER_KILL;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(None) => thread::sleep(Duration::from_secs(1)),
                _ => return,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });
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
    fn every_backend_check_allows_is_one_jeet_sends_through() {
        let config = NotifyConfig {
            command: Some("page-me".into()),
            ..NotifyConfig::default()
        };
        let home = Path::new("/nowhere");
        for name in BACKENDS {
            assert!(backend(name, &config, home).is_some(), "{name}");
        }
        assert!(backend("gotify", &config, home).is_none());
        let no_user = NotifyConfig {
            via: Some(vec!["slack".into()]),
            ..NotifyConfig::default()
        };
        assert!(check(&no_user).is_err());
        let ntfy = |table: &str| -> NotifyConfig {
            toml::from_str(&format!("via = [\"ntfy\"]\n{table}")).unwrap()
        };
        assert!(check(&ntfy("")).is_err());
        assert!(check(&ntfy("[ntfy]\ntopic = \"a/b\"")).is_err());
        assert!(check(&ntfy("[ntfy]\ntopic = \"jeet-x\"\nserver = \"ftp://x\"")).is_err());
        let good = ntfy("[ntfy]\ntopic = \"jeet-x\"");
        assert!(check(&good).is_ok());
        assert_eq!(good.ntfy_server(), "https://ntfy.sh");
        let own = ntfy("[ntfy]\ntopic = \"jeet-x\"\nserver = \"https://ntfy.example.com/\"");
        assert!(check(&own).is_ok());
        assert_eq!(own.ntfy_server(), "https://ntfy.example.com");
        let unset = NotifyConfig {
            via: Some(vec!["Desktop".into()]),
            ..NotifyConfig::default()
        };
        assert!(check(&unset).is_err());
    }

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
            at: Some(5),
            ..Event::default()
        };
        enqueue(home.path(), &event).unwrap();
        let text = std::fs::read_to_string(outbox_path(home.path())).unwrap();
        assert_eq!(
            text,
            "{\"why\":\"needs-user\",\"kind\":\"question\",\"level\":\"manager\",\"task\":\"api\",\
             \"ref\":\"r41\",\"title\":\"Question #41\",\"body\":\"\",\"link\":\"jeet://later\",\"at\":5}\n"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&outbox_path(home.path())), 0o600);
        assert_eq!(mode(&dir(home.path())), 0o700);
        let mut outbox = lock_outbox(home.path()).unwrap();
        assert_eq!(read_all(&mut outbox).unwrap(), [event]);
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
    fn a_broken_config_keeps_the_outbox_and_a_forged_only_is_dropped() {
        let home = tempfile::tempdir().unwrap();
        let config = config::config_path(home.path());
        std::fs::write(&config, "[notify\n").unwrap();
        enqueue(home.path(), &Event::default()).unwrap();
        assert!(drain(home.path(), &NoTokens, false).is_err());
        let queued = std::fs::read_to_string(outbox_path(home.path())).unwrap();
        assert_eq!(queued.lines().count(), 1);

        // `only` naming a backend that needs no token skips no rule: it
        // is dropped, not sent.
        std::fs::write(
            &config,
            "[notify]\nvia = [\"command\"]\ncommand = \"true\"\n",
        )
        .unwrap();
        std::fs::write(outbox_path(home.path()), "").unwrap();
        let forged = Event {
            only: vec!["command".into()],
            ..Event::default()
        };
        enqueue(home.path(), &forged).unwrap();
        drain(home.path(), &NoTokens, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(outbox_path(home.path())).unwrap(),
            ""
        );
        assert_eq!(load_state(home.path()).sent.len(), 0);
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
        // Unlocked, not only closed: another test's forked child holds a
        // copy of the file, and the flock with it, until it execs.
        lock.unlock().unwrap();
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

    #[test]
    fn a_failed_token_send_waits_in_the_outbox_and_backs_off() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            config::config_path(home.path()),
            "[notify]\nvia = [\"slack\"]\n[notify.slack]\nuser = \"U012AB3CD\"\n",
        )
        .unwrap();
        let event = Event {
            kind: "question".into(),
            reference: Some("r1".into()),
            title: "t".into(),
            ..Event::default()
        };
        enqueue(home.path(), &event).unwrap();
        // No token: the send fails, and it is put back for slack alone.
        let drained = drain(home.path(), &NoTokens, true).unwrap();
        let first_queue = queued(home.path());
        assert_eq!(first_queue.len(), 1);
        assert_eq!(first_queue[0].only, ["slack"]);
        assert_eq!(first_queue[0].tries, 1);
        let first = first_queue[0].retry_at.unwrap();
        assert!(first > now_secs() + RETRY_FIRST_SECS - 5, "{first}");
        assert_eq!(drained.retry_at, Some(first));
        let len = std::fs::metadata(outbox_path(home.path())).unwrap().len();
        assert_eq!(drained.left, len);

        // Not due: left as it is.
        drain(home.path(), &NoTokens, true).unwrap();
        assert_eq!(queued(home.path()), first_queue);
        // Nor sent by any other process.
        drain(home.path(), &NoTokens, false).unwrap();
        assert_eq!(queued(home.path()), first_queue);

        // Due: tried again, and waits twice as long.
        let due = Event {
            retry_at: Some(0),
            ..first_queue[0].clone()
        };
        std::fs::write(
            outbox_path(home.path()),
            serde_json::to_string(&due).unwrap() + "\n",
        )
        .unwrap();
        drain(home.path(), &NoTokens, true).unwrap();
        let again = queued(home.path());
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].tries, 2);
        assert!(again[0].retry_at.unwrap() >= now_secs() + 2 * RETRY_FIRST_SECS - 5);
        assert_eq!(
            retry(&again[0], "slack", 0).retry_at,
            Some(4 * RETRY_FIRST_SECS)
        );
        let late = Event {
            tries: 40,
            ..Event::default()
        };
        assert_eq!(retry(&late, "slack", 0).retry_at, Some(RETRY_MAX_SECS));

        // Failing still after its last try: dropped.
        let stale = Event {
            tries: GIVE_UP_TRIES - 1,
            ..due
        };
        std::fs::write(
            outbox_path(home.path()),
            serde_json::to_string(&stale).unwrap() + "\n",
        )
        .unwrap();
        let drained = drain(home.path(), &NoTokens, true).unwrap();
        assert!(queued(home.path()).is_empty());
        assert_eq!(drained, Drained::default());
    }

    #[test]
    fn a_backend_found_down_defers_the_rest_of_the_round_untried() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            config::config_path(home.path()),
            "[notify]\nvia = [\"slack\"]\n[notify.slack]\nuser = \"U012AB3CD\"\n",
        )
        .unwrap();
        for title in ["a", "b"] {
            let event = Event {
                title: title.into(),
                ..Event::default()
            };
            enqueue(home.path(), &event).unwrap();
        }
        // No token: the first fails, and the backend is down for the round.
        drain(home.path(), &NoTokens, true).unwrap();
        let queued = queued(home.path());
        assert_eq!(queued.len(), 2);
        assert_eq!((queued[0].title.as_str(), queued[0].tries), ("a", 1));
        assert_eq!((queued[1].title.as_str(), queued[1].tries), ("b", 0));
        assert_eq!(queued[1].only, ["slack"]);
        assert_eq!(queued[1].retry_at, queued[0].retry_at);
    }
}
