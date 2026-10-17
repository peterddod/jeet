//! tmux: where task agents run.
//!
//! Every task's agent lives in a detached tmux session named `jeet-<slug>`,
//! so it keeps working after the terminal that started it has gone, and can
//! be attached to again from anywhere. The assistant, which may have several
//! sessions going at once, has a window for each in one tmux session; see
//! [`new_window`].
//!
//! `$JEET_TMUX` names the tmux binary, for a tmux outside `$PATH` and for
//! tests, which point it at a stub so they never reach a real server.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

#[cfg_attr(test, allow(dead_code))]
pub const BINARY_ENV: &str = "JEET_TMUX";

/// Every tmux session jeet makes is named with this in front.
pub const SESSION_PREFIX: &str = "jeet-";

/// Set by jeet's launchd job and systemd unit. A tmux server keeps the
/// environment of whoever started it, and hands it to every pane it makes
/// from then on, the user's own included; a job runs with almost none of the
/// user's. So a process with this set never starts a server, only sessions
/// on one already running.
pub const BACKGROUND_ENV: &str = "JEET_BACKGROUND";

pub const INSTALL_HINT: &str = "jeet runs task agents in tmux, which was not found; install it \
     (`brew install tmux` on macOS, `sudo apt install tmux` on Debian and Ubuntu)";

#[cfg(not(test))]
fn binary() -> String {
    std::env::var(BINARY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "tmux".to_string())
}

/// Unit tests run in this process, where an environment variable set for
/// one would reach every test at once: they never reach a tmux server, the
/// user's or any other, whatever `$JEET_TMUX` says. To them tmux is not
/// installed.
#[cfg(test)]
fn binary() -> String {
    "/nonexistent/jeet-unit-test-tmux".to_string()
}

/// How long one tmux command gets before jeet gives up on it and kills it.
/// A server that has stopped answering must not hang jeet with it: a review
/// job's runner, detached and waiting on tmux to tell the assistant the
/// review ended, would wait forever.
const CALL_LIMIT: Duration = Duration::from_secs(20);

/// [`Command::output`] and [`Command::status`] for a tmux command, killed
/// and failed as [`std::io::ErrorKind::TimedOut`] once it has run for
/// [`CALL_LIMIT`]. Its stdin is nothing, and its output is read either way.
trait Bounded {
    fn bounded_output(&mut self) -> std::io::Result<Output>;
    fn bounded_status(&mut self) -> std::io::Result<ExitStatus>;
}

impl Bounded for Command {
    fn bounded_output(&mut self) -> std::io::Result<Output> {
        let child = self
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        finish(child, CALL_LIMIT)
    }

    fn bounded_status(&mut self) -> std::io::Result<ExitStatus> {
        self.bounded_output().map(|o| o.status)
    }
}

/// Wait for `child` and read what it writes, as
/// [`std::process::Child::wait_with_output`] does, for up to `limit`:
/// past it the child is killed, and its output given up on.
fn finish(mut child: Child, limit: Duration) -> std::io::Result<Output> {
    fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + limit;
    let timed_out = || {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("tmux did not finish within {}s", limit.as_secs()),
        )
    };
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out());
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    // Exited, but something it started may still hold its output open.
    let left = || deadline.saturating_duration_since(Instant::now());
    let stdout = out.recv_timeout(left()).map_err(|_| timed_out())?;
    let stderr = err.recv_timeout(left()).map_err(|_| timed_out())?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// A tmux command whose output jeet reads. `-u`, as tmux replaces the tabs
/// that jeet's formats split fields on, and anything else outside printable
/// ASCII, with `_` for a client whose locale is not UTF-8: a launchd job's
/// or systemd unit's, which sets none. Run so, `jeet watch` could start a
/// window and then not read which pane it started, so never anchored it.
fn command() -> Command {
    let mut cmd = Command::new(binary());
    cmd.arg("-u");
    cmd
}

/// A tmux command that cannot read the terminal and says nothing on it.
fn quiet() -> Command {
    let mut cmd = command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// An exact-match session target. A bare name is matched as a prefix, so
/// `jeet-api` would otherwise find `jeet-api-2` once `jeet-api` is gone.
pub fn target(name: &str) -> String {
    format!("={name}")
}

/// Whether tmux can be run at all.
pub fn available() -> bool {
    quiet()
        .arg("-V")
        .bounded_status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Fail with an install hint when tmux is missing.
pub fn require() -> Result<()> {
    if !available() {
        bail!(INSTALL_HINT);
    }
    Ok(())
}

/// Whether this process runs as a background job: see [`BACKGROUND_ENV`].
fn in_background() -> bool {
    std::env::var_os(BACKGROUND_ENV).is_some_and(|v| !v.is_empty())
}

/// Whether a tmux server is running, for a new session to join.
fn server_running() -> bool {
    quiet()
        .arg("list-sessions")
        .bounded_status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether a new tmux session may be started here: always, but in a
/// background job only on a server that is already running.
pub fn may_start_session() -> bool {
    !in_background() || server_running()
}

/// Fail rather than start a server from a background job.
fn refuse_new_server(name: &str) -> Result<()> {
    if !may_start_session() {
        bail!(
            "not starting tmux session {name} from a background job: no tmux server is \
             running, and one started here would have the job's environment, not yours"
        );
    }
    Ok(())
}

/// The names of the sessions on the tmux server: none when no server runs,
/// or tmux cannot be run.
pub fn list_sessions() -> Vec<String> {
    let Ok(output) = command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn has_session(name: &str) -> bool {
    quiet()
        .args(["has-session", "-t", &target(name)])
        .bounded_status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The pane tmux started a program in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// tmux's id for it (`%3`).
    pub id: String,
    /// The process tmux started in it: the program itself, or the shell
    /// that runs it.
    pub pid: u32,
}

/// A pane from tmux's `#{pane_id}` and `#{pane_pid}`.
fn parse_pane(fields: &[&str]) -> Result<Pane> {
    match fields {
        [id, pid] if id.starts_with('%') => Ok(Pane {
            id: id.to_string(),
            pid: pid
                .trim()
                .parse()
                .with_context(|| format!("tmux gave {pid:?} for a pane's process"))?,
        }),
        _ => bail!(
            "tmux did not say which pane it started ({:?})",
            fields.join("\t")
        ),
    }
}

/// Start `argv` in a new detached session `name`, in `cwd`, with `env` set
/// in it, returning its pane. `argv` is run directly, never through a
/// shell.
pub fn new_session(name: &str, cwd: &Path, env: &[(&str, &str)], argv: &[String]) -> Result<Pane> {
    if argv.is_empty() {
        bail!("nothing to run in tmux session {name}");
    }
    refuse_new_server(name)?;
    let mut cmd = command();
    cmd.args(extended_keys_first())
        .args(["new-session", "-d", "-P", "-F", "#{pane_id}\t#{pane_pid}"])
        .args(["-s", name, "-c"])
        .arg(cwd);
    for (key, value) in env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg("--").args(argv);
    // A server this starts keeps a copy of our environment for every pane it
    // ever makes: the shell wrapper's cd file, this task's id and the
    // assistant's restrictions must not leak into other sessions that way.
    cmd.env_remove(crate::cd::CD_FILE_ENV)
        .env_remove(crate::tasks::TASK_ID_ENV)
        .env_remove(crate::permissions::ASSISTANT_ENV)
        .env_remove(BACKGROUND_ENV)
        .stdin(Stdio::null());
    let output = cmd.bounded_output().context("run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux could not start session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    set_detach_hint(name);
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.trim().split('\t').collect();
    let pane = parse_pane(&fields).with_context(|| format!("start tmux session {name}"))?;
    record_agent_pane(name, &pane.id);
    Ok(pane)
}

/// The session option that holds the tmux id (`%3`) of the pane
/// [`new_session`] started the agent in.
const AGENT_PANE_OPTION: &str = "@jeet-agent-pane";

/// Note in session `name`, just started, the id `pane` of the pane its
/// agent runs in, as tmux reported it when it started the session. An id
/// names that pane for as long as the server runs, wherever the user splits
/// or moves it, and goes with the session.
fn record_agent_pane(name: &str, pane: &str) {
    let _ = quiet()
        .args(["set-option", "-t", &format!("{}:", target(name))])
        .args([AGENT_PANE_OPTION, pane])
        .bounded_status();
}

/// The pane of session `name` its agent runs in, by the id
/// [`new_session`] noted. The session's active pane may be one the user
/// opened there, splitting the agent's window or in a window of its own,
/// for a shell or an editor, which must not get the agent's keys. A
/// session started before jeet noted the id falls back to the active pane
/// of its first window, the one the agent was started in.
fn agent_pane(name: &str) -> String {
    pane_or_first_window(name, &recorded_agent_pane(name))
}

/// What session `name` holds as its agent's pane id, as noted; empty when
/// nothing is.
fn recorded_agent_pane(name: &str) -> String {
    command()
        .args(["show-options", "-qv", "-t", &format!("{}:", target(name))])
        .arg(AGENT_PANE_OPTION)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// The pane `recorded` names, when it is a pane id, or else the active pane
/// of session `name`'s first window.
fn pane_or_first_window(name: &str, recorded: &str) -> String {
    let recorded = recorded.trim();
    match recorded.strip_prefix('%') {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => recorded.to_string(),
        _ => format!("{}:^", target(name)),
    }
}

/// Type `text` into the agent's pane of session `name` (see
/// [`agent_pane`]), then press Enter.
///
/// The text goes in as one paste, not as keystrokes: Claude Code takes a
/// long burst of keys for several pastes and keeps only the end of it. The
/// caller removes control characters: pasted, they would still drive the
/// pane.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    type_into(&agent_pane(name), text)
}

/// [`send_text`] for the window with tmux id `window` (`@3`).
pub fn send_text_to_window(window: &str, text: &str) -> Result<()> {
    type_into(window, text)
}

fn type_into(pane: &str, text: &str) -> Result<()> {
    paste(pane, text)?;
    // A moment between the text and the Enter, or a pane still reading the
    // paste may take the Enter as part of it.
    std::thread::sleep(Duration::from_millis(100));
    send_keys(&["-t", pane, "Enter"])
}

/// Paste `text` into `pane` through a tmux buffer of its own, bracketed
/// (`-p`) for a program that asks for that, so it reads the text as one
/// paste. The text goes to tmux on stdin, never as an argument tmux would
/// parse, and the buffer is deleted once pasted.
fn paste(pane: &str, text: &str) -> Result<()> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let buffer = format!("jeet-{}-{nanos}", std::process::id());
    let mut load = command()
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("run tmux")?;
    // Written from a thread of its own: a tmux that stops reading must not
    // hold this up past the limit.
    let mut stdin = load.stdin.take().context("tmux took no input")?;
    let text = text.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&text));
    let output = finish(load, CALL_LIMIT).context("run tmux")?;
    let written = writer.join().unwrap_or(Ok(()));
    written.context("hand tmux the text")?;
    if !output.status.success() {
        bail!(
            "tmux could not take the text to type in: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output = command()
        .args(["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    if !output.status.success() {
        let _ = quiet()
            .args(["delete-buffer", "-b", &buffer])
            .bounded_status();
        bail!(
            "tmux could not type into the agent's pane: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The window option a window made by [`new_window`] is keyed by.
const KEY_OPTION: &str = "@jeet_key";

/// The window option holding the key a window made by [`new_window`] was
/// first given, which [`set_window_key`] never changes: what its program was
/// launched as, whatever it has gone on to since.
const LAUNCH_OPTION: &str = "@jeet_launch";

/// One window of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// tmux's id for it, e.g. `@3`: unique on the server, and a target as
    /// it is.
    pub id: String,
    /// Whether its program is still running.
    pub alive: bool,
    /// What it is keyed by now, if [`new_window`] made it.
    pub key: Option<String>,
    /// What [`new_window`] first keyed it by, if it made it.
    pub launch: Option<String>,
}

/// The windows of session `name`: none when it is not running.
pub fn windows(name: &str) -> Vec<Window> {
    let Ok(output) = command()
        .args([
            "list-windows",
            "-t",
            &target(name),
            "-F",
            &format!("#{{window_id}}\t#{{pane_dead}}\t#{{{KEY_OPTION}}}\t#{{{LAUNCH_OPTION}}}"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_windows(&String::from_utf8_lossy(&output.stdout))
}

fn parse_windows(listing: &str) -> Vec<Window> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            if !id.starts_with('@') {
                return None;
            }
            let alive = fields.next()?.trim() == "0";
            let mut next = || {
                fields
                    .next()
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(str::to_string)
            };
            let key = next();
            let launch = next();
            Some(Window {
                id: id.to_string(),
                alive,
                key,
                launch,
            })
        })
        .collect()
}

/// Start `argv` in a new window of session `name`, making the session if it
/// is not running, and key the window by `key` so [`windows`] can tell it
/// apart (and, for good, note it as what the window was launched as). `title` is what tmux shows for it. Returns the window's id and
/// its pane.
/// `argv` is run directly, never through a shell; the session's environment
/// is kept clean the way [`new_session`]'s is.
pub fn new_window(
    name: &str,
    key: &str,
    title: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    argv: &[String],
) -> Result<(String, Pane)> {
    if argv.is_empty() {
        bail!("nothing to run in tmux session {name}");
    }
    let mut creating = !has_session(name);
    if creating {
        refuse_new_server(name)?;
    }
    let first = extended_keys_first();
    let mut attempts = 0;
    let output = loop {
        attempts += 1;
        let mut cmd = command();
        cmd.args(&first);
        match creating {
            true => cmd.args(["new-session", "-d", "-P", "-F", WINDOW_FORMAT, "-s", name]),
            false => cmd.args([
                "new-window",
                "-d",
                "-P",
                "-F",
                WINDOW_FORMAT,
                "-t",
                &format!("{}:", target(name)),
            ]),
        };
        // tmux expands formats in a window name, and `#(…)` in one runs a
        // shell command: the name is the user's, so it goes in literally.
        cmd.args(["-n", &format_literal(title), "-c"]).arg(cwd);
        for (key, value) in env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        cmd.arg("--").args(argv);
        cmd.env_remove(crate::cd::CD_FILE_ENV)
            .env_remove(crate::tasks::TASK_ID_ENV)
            .env_remove(crate::permissions::ASSISTANT_ENV)
            .env_remove(BACKGROUND_ENV)
            .stdin(Stdio::null());
        let output = cmd.bounded_output().context("run tmux")?;
        // Another jeet made the session between the look and the start:
        // a window in it does as well.
        if !output.status.success() && creating && attempts == 1 && has_session(name) {
            creating = false;
            continue;
        }
        break output;
    };
    if !output.status.success() {
        bail!(
            "tmux could not start a window in session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let Some((id, pane)) = parse_window(line.trim()) else {
        bail!("tmux started a window in session {name} but did not say which ({line:?})");
    };
    if !(set_window_key(&id, key) && set_window_option(&id, LAUNCH_OPTION, key)) {
        let _ = kill_window(&id);
        bail!("tmux could not label its new window in session {name}");
    }
    if creating {
        set_detach_hint(name);
    }
    Ok((id, pane))
}

/// What [`new_window`] has tmux print of the window it starts.
const WINDOW_FORMAT: &str = "#{window_id}\t#{pane_id}\t#{pane_pid}";

/// The window and pane of a [`WINDOW_FORMAT`] line.
fn parse_window(line: &str) -> Option<(String, Pane)> {
    let fields: Vec<&str> = line.split('\t').collect();
    let (id, pane) = fields.split_first()?;
    if !id.starts_with('@') {
        return None;
    }
    Some((id.to_string(), parse_pane(pane).ok()?))
}

/// Key window `id` by `key`, as [`new_window`] does: for a window whose
/// program has gone on to something else to be known by.
pub fn set_window_key(id: &str, key: &str) -> bool {
    set_window_option(id, KEY_OPTION, key)
}

fn set_window_option(id: &str, option: &str, value: &str) -> bool {
    quiet()
        .args(["set-option", "-w", "-t", id, option, value])
        .bounded_status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The window this process runs in, by `$TMUX_PANE`, when it is one of
/// session `name`'s: its id and what it is keyed by.
pub fn current_window(name: &str) -> Option<(String, Option<String>)> {
    std::env::var_os("TMUX").filter(|v| !v.is_empty())?;
    // The pane this process runs in, never whichever the client last used.
    let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())?;
    let output = command()
        .args(["display-message", "-p", "-t", &pane])
        .arg(format!(
            "#{{session_name}}\t#{{window_id}}\t#{{{KEY_OPTION}}}"
        ))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success())?;
    parse_current_window(&String::from_utf8_lossy(&output.stdout), name)
}

fn parse_current_window(line: &str, name: &str) -> Option<(String, Option<String>)> {
    let mut fields = line.trim_end_matches(['\n', '\r']).split('\t');
    if fields.next()? != name {
        return None;
    }
    let id = fields.next()?.trim();
    if !id.starts_with('@') {
        return None;
    }
    let key = fields
        .next()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    Some((id.to_string(), key))
}

/// Split the agent's pane of session `name` and run `argv` beside it, in
/// `cwd`, with `env` set in it (an empty value blanks what the session
/// has), leaving the agent's pane selected. `argv` is run directly, never
/// through a shell.
pub fn split_window(name: &str, cwd: &Path, env: &[(&str, &str)], argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("nothing to run in tmux session {name}");
    }
    let recorded = recorded_agent_pane(name);
    let pane = pane_or_first_window(name, recorded.trim());
    let mut cmd = command();
    cmd.args(["split-window", "-d", "-h", "-t", &pane, "-c"])
        .arg(cwd);
    for (key, value) in env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg("--").args(argv).stdin(Stdio::null());
    let output = cmd.bounded_output().context("run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux could not split a pane in session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Make window `id` the one its session shows.
pub fn select_window(id: &str) -> Result<()> {
    let output = command()
        .args(["select-window", "-t", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux could not switch to window {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// End window `id` and what runs in it, the way [`kill`] ends a session; a
/// window that is already gone is fine.
pub fn kill_window(id: &str) -> Result<()> {
    let pids = process_tree(&pane_pids(&["-t", id]));
    let output = command()
        .args(["kill-window", "-t", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    if !output.status.success() && window_exists(id) {
        bail!(
            "tmux could not end window {id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    end_processes(&pids, EXIT_GRACE)
        .with_context(|| format!("end what was running in tmux window {id}"))
}

fn window_exists(id: &str) -> bool {
    quiet()
        .args(["list-panes", "-t", id])
        .bounded_status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the program in window `id` is still running.
pub fn window_alive(id: &str) -> bool {
    let Ok(output) = command()
        .args(["list-panes", "-t", id, "-F", "#{pane_dead}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return false;
    };
    output.status.success() && any_pane_alive(&String::from_utf8_lossy(&output.stdout))
}

/// Press the keys tmux calls `keys` (e.g. `Down`, `Enter`) in the agent's
/// pane of session `name` (see [`agent_pane`]).
pub fn press(name: &str, keys: &[&str]) -> Result<()> {
    let pane = agent_pane(name);
    let mut args = vec!["-t", pane.as_str()];
    args.extend(keys);
    send_keys(&args)
}

/// The text the agent's pane of session `name` shows (see [`agent_pane`]).
pub fn capture(name: &str) -> Result<String> {
    capture_pane(&agent_pane(name), false)
}

/// [`capture`], with the text's colours and styles kept as escape
/// sequences: to tell a dimmed placeholder from what was typed.
pub fn capture_styled(name: &str) -> Result<String> {
    capture_pane(&agent_pane(name), true)
}

/// [`capture_styled`] for the window with tmux id `window` (`@3`).
pub fn capture_window_styled(window: &str) -> Result<String> {
    capture_pane(window, true)
}

fn capture_pane(pane: &str, styled: bool) -> Result<String> {
    let mut cmd = command();
    cmd.args(["capture-pane", "-p"]);
    if styled {
        cmd.arg("-e");
    }
    let output = cmd
        .args(["-t", pane])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    if !output.status.success() {
        bail!("tmux could not read pane {pane}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether anyone is attached to session `name`.
pub fn attached(name: &str) -> bool {
    command()
        .args(["list-clients", "-t", &target(name)])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// When each client attached to session `name` last had input, in seconds
/// since the epoch, for those showing `window` (any, for `None`). `None`
/// when tmux cannot say.
pub fn client_activity(name: &str, window: Option<&str>) -> Option<Vec<i64>> {
    let output = command()
        .args([
            "list-clients",
            "-F",
            "#{client_activity}\t#{session_name}\t#{window_id}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(parse_client_activity(
        &String::from_utf8_lossy(&output.stdout),
        name,
        window,
    ))
}

fn parse_client_activity(listing: &str, name: &str, window: Option<&str>) -> Vec<i64> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (at, session, shown) = (fields.next()?, fields.next()?, fields.next()?);
            (session == name && window.is_none_or(|w| w == shown))
                .then(|| at.parse().ok())
                .flatten()
        })
        .collect()
}

/// A tmux client: a terminal attached to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    /// The terminal device it draws on.
    pub tty: String,
    pub session: String,
    /// When it last had input, in seconds since the epoch.
    pub activity: i64,
}

/// Every attached client; none when tmux cannot say.
pub fn clients() -> Vec<Client> {
    let output = command()
        .args([
            "list-clients",
            "-F",
            "#{client_name}\t#{client_tty}\t#{session_name}\t#{client_activity}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success());
    match output {
        Some(output) => parse_clients(&String::from_utf8_lossy(&output.stdout)),
        None => Vec::new(),
    }
}

fn parse_clients(listing: &str) -> Vec<Client> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (name, tty, session, at) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            Some(Client {
                name: name.to_string(),
                tty: tty.to_string(),
                session: session.to_string(),
                activity: at.parse().ok()?,
            })
        })
        .collect()
}

/// The client that had input last: the one the user is at.
pub fn most_recent(clients: &[Client]) -> Option<&Client> {
    clients.iter().max_by_key(|c| c.activity)
}

/// Point `client` at `target`: a session (`=jeet-api`), or a pane (`%3`),
/// which also changes its session and window.
pub fn switch_client(client: &str, target: &str) -> Result<()> {
    let status = quiet()
        .args(["switch-client", "-c", client, "-t", target])
        .bounded_status()
        .context("run tmux")?;
    if !status.success() {
        bail!("tmux could not switch {client} to {target}");
    }
    Ok(())
}

/// Detach `client`, ending the `tmux attach` it was.
pub fn detach_client(client: &str) -> Result<()> {
    let status = quiet()
        .args(["detach-client", "-t", client])
        .bounded_status()
        .context("run tmux")?;
    if !status.success() {
        bail!("tmux could not detach {client}");
    }
    Ok(())
}

fn send_keys(args: &[&str]) -> Result<()> {
    let output = command()
        .arg("send-keys")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux could not type into the agent's pane: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The way back to jeet that leaves the agent running: one key, no prefix.
/// Quitting Claude ends it; detaching is what keeps it going, and `C-b d`
/// is two steps that Claude Code's own `C-b` gets in the way of.
pub const DETACH_KEY: &str = "C-q";

/// What the status line says about [`DETACH_KEY`].
const DETACH_HINT: &str = " Ctrl-Q → back to jeet (keeps running) ";

/// Make [`DETACH_KEY`] detach from session `name`, and say so in its status
/// line. Key bindings belong to the whole server, so the key only detaches
/// from jeet's own sessions and is passed through everywhere else.
/// Cosmetic, so failures are ignored.
fn set_detach_hint(name: &str) {
    let _ = quiet()
        .args(["bind-key", "-n", DETACH_KEY, "if-shell", "-F"])
        .arg(format!("#{{m:{SESSION_PREFIX}*,#{{session_name}}}}"))
        .args(["detach-client", &format!("send-keys {DETACH_KEY}")])
        .bounded_status();
    let _ = quiet()
        .args([
            "set-option",
            "-t",
            // A pane target: a bare `=name` is no session to set-option.
            &format!("{}:", target(name)),
            "status-right",
            DETACH_HINT,
        ])
        .bounded_status();
}

/// The server option that has tmux pass modified keys such as Shift+Enter
/// on to a program that asks for them, rather than the plain key: Claude
/// Code asks, and takes Shift+Enter for a new line. tmux has it off.
const EXTENDED_KEYS: &str = "extended-keys";

/// Set on the server once jeet has turned [`EXTENDED_KEYS`] on there, so a
/// user who turns it off again is not overruled.
const EXTENDED_KEYS_MARK: &str = "@jeet-extended-keys";

/// A tmux command to run first, in the same call, wherever jeet starts a
/// session or window: it turns [`EXTENDED_KEYS`] on, and tells tmux the
/// outer terminal can send such keys too (which takes effect for a client
/// on attaching). It has to come first: tmux drops a program's request for
/// these keys while the option is off, and does not remember it.
///
/// The server is the user's own, so the option is changed only from tmux's
/// default, once per server, and never when the user's tmux.conf sets it.
/// A tmux that has no such option (before 3.2) expands it to nothing, and
/// nothing is done.
fn extended_keys_first() -> Vec<String> {
    if config_sets_extended_keys() {
        return Vec::new();
    }
    vec![
        "if-shell".into(),
        "-F".into(),
        format!("#{{==:#{{{EXTENDED_KEYS}}}#{{{EXTENDED_KEYS_MARK}}},off}}"),
        format!(
            "set-option -s {EXTENDED_KEYS} on ; \
             set-option -as terminal-features 'xterm*:extkeys' ; \
             set-option -s {EXTENDED_KEYS_MARK} on"
        ),
        ";".into(),
    ]
}

/// Whether one of the files tmux reads its configuration from sets
/// [`EXTENDED_KEYS`], to whatever value.
fn config_sets_extended_keys() -> bool {
    let mut files = Vec::new();
    if let Some(home) = dirs::home_dir() {
        files.push(home.join(".tmux.conf"));
        files.push(home.join(".config/tmux/tmux.conf"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        files.push(Path::new(&xdg).join("tmux/tmux.conf"));
    }
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .any(|conf| sets_extended_keys(&conf))
}

fn sets_extended_keys(conf: &str) -> bool {
    conf.lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with('#'))
        .any(|line| line.split_whitespace().any(|word| word == EXTENDED_KEYS))
}

/// `text` as tmux shows it where it expands formats: `#` starts one, and
/// `#(…)` runs a shell command, so each is doubled to stand for itself.
fn format_literal(text: &str) -> String {
    text.replace('#', "##")
}

/// Attach the terminal to session `name` until the user detaches, returning
/// tmux's exit code. Inside tmux already, the client switches to it instead
/// of nesting a second tmux.
pub fn attach(name: &str) -> Result<i32> {
    let inside = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let verb = if inside {
        "switch-client"
    } else {
        "attach-session"
    };
    let status = Command::new(binary())
        .args([verb, "-t", &target(name)])
        .status()
        .context("run tmux")?;
    Ok(status.code().unwrap_or(1))
}

/// End session `name` and everything running in it; a session that is
/// already gone is fine.
///
/// `kill-session` only hangs up on the panes and returns at once, while
/// what ran in them may still be shutting down: finishing a tool call,
/// running an exit hook, writing files. So this returns only once every
/// process the panes were running, their children included, has exited.
/// Whatever is still running after [`EXIT_GRACE`] is killed outright.
pub fn kill(name: &str) -> Result<()> {
    if !has_session(name) {
        return Ok(());
    }
    // Taken before the kill: once the panes are gone, tmux no longer knows
    // which processes were theirs.
    let pids = process_tree(&live_pane_pids(name));
    let output = command()
        .args(["kill-session", "-t", &target(name)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .bounded_output()
        .context("run tmux")?;
    // It may have ended on its own between the check and the kill.
    if !output.status.success() && has_session(name) {
        bail!(
            "tmux could not end session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    end_processes(&pids, EXIT_GRACE)
        .with_context(|| format!("end what was running in tmux session {name}"))
}

/// How long the processes of a killed session get to exit by themselves:
/// longer than Claude gives its exit hook.
pub const EXIT_GRACE: Duration = Duration::from_secs(15);

/// How long a process gets to go once it has been sent SIGKILL.
const KILL_WAIT: Duration = Duration::from_secs(3);

/// The pids of the programs still running in session `name`'s panes. A dead
/// pane's pid is left out: that process is gone and the pid may be reused.
fn live_pane_pids(name: &str) -> Vec<u32> {
    pane_pids(&["-s", "-t", &target(name)])
}

/// The processes tmux started in window `id`'s live panes.
pub fn window_pane_pids(id: &str) -> Vec<u32> {
    pane_pids(&["-t", id])
}

/// The pids of the live panes `list-panes` with `args` lists.
fn pane_pids(args: &[&str]) -> Vec<u32> {
    let Ok(output) = command()
        .arg("list-panes")
        .args(args)
        .args(["-F", "#{pane_dead} #{pane_pid}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_live_pane_pids(&String::from_utf8_lossy(&output.stdout))
}

fn parse_live_pane_pids(listing: &str) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| {
            let (dead, pid) = line.trim().split_once(' ')?;
            match dead {
                "0" => pid.trim().parse().ok(),
                _ => None,
            }
        })
        .collect()
}

/// One process in a `ps` listing.
struct Proc {
    pid: u32,
    ppid: u32,
    /// Exited, waiting only for its parent to reap it.
    zombie: bool,
}

/// Every process running now, or nothing when `ps` cannot be run.
fn processes() -> Vec<Proc> {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_processes(&String::from_utf8_lossy(&output.stdout))
}

fn parse_processes(listing: &str) -> Vec<Proc> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let zombie = fields.next().is_some_and(|stat| stat.starts_with('Z'));
            Some(Proc { pid, ppid, zombie })
        })
        .collect()
}

/// `roots` and all their descendants that are running now. This process is
/// never among them: waiting for itself would never end.
pub fn process_tree(roots: &[u32]) -> Vec<u32> {
    if roots.is_empty() {
        return Vec::new();
    }
    descendants(&processes(), roots, std::process::id())
}

fn descendants(procs: &[Proc], roots: &[u32], me: u32) -> Vec<u32> {
    let running = |pid: u32| procs.iter().any(|p| p.pid == pid && !p.zombie);
    let mut tree: Vec<u32> = roots.iter().copied().filter(|&p| running(p)).collect();
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        for p in procs {
            if p.ppid == parent && !p.zombie && !tree.contains(&p.pid) {
                tree.push(p.pid);
            }
        }
        i += 1;
    }
    tree.retain(|&p| p != me);
    tree
}

/// Those of `pids` still running.
fn still_running(pids: &[u32]) -> Vec<u32> {
    let procs = processes();
    pids.iter()
        .copied()
        .filter(|&pid| procs.iter().any(|p| p.pid == pid && !p.zombie))
        .collect()
}

/// Wait up to `limit` for every one of `pids` to exit; those still running
/// after it.
fn wait_for_exit(pids: &[u32], limit: Duration) -> Vec<u32> {
    let deadline = Instant::now() + limit;
    loop {
        let left = still_running(pids);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Let `pids` exit within `grace`, then kill whatever has not.
pub fn end_processes(pids: &[u32], grace: Duration) -> Result<()> {
    if pids.is_empty() {
        return Ok(());
    }
    let left = wait_for_exit(pids, grace);
    if left.is_empty() {
        return Ok(());
    }
    let _ = Command::new("kill")
        .arg("-KILL")
        .args(left.iter().map(u32::to_string))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let left = wait_for_exit(&left, KILL_WAIT);
    if !left.is_empty() {
        let list: Vec<String> = left.iter().map(u32::to_string).collect();
        bail!("process {} would not exit", list.join(", "));
    }
    Ok(())
}

/// The name of the tmux session this process runs in, if it runs in one.
pub fn current_session() -> Option<String> {
    std::env::var_os("TMUX").filter(|v| !v.is_empty())?;
    let mut cmd = command();
    cmd.args(["display-message", "-p"]);
    // The pane this process runs in, not whichever the client last used.
    if let Some(pane) = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()) {
        cmd.args(["-t", &pane]);
    }
    let output = cmd
        .arg("#{session_name}")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success())?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// What the window this process runs in is keyed by ([`new_window`]), if
/// it runs in one jeet made.
pub fn current_window_key() -> Option<String> {
    std::env::var_os("TMUX").filter(|v| !v.is_empty())?;
    let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())?;
    let output = command()
        .args(["display-message", "-p", "-t", &pane])
        .arg(format!("#{{{KEY_OPTION}}}"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()
        .filter(|o| o.status.success())?;
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// Whether anything is still running in session `name`: false when the
/// session is gone or every pane in it has exited.
pub fn pane_alive(name: &str) -> bool {
    let Ok(output) = command()
        .args([
            "list-panes",
            "-s",
            "-t",
            &target(name),
            "-F",
            "#{pane_dead}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return false;
    };
    output.status.success() && any_pane_alive(&String::from_utf8_lossy(&output.stdout))
}

/// `#{pane_dead}` is `1` for a pane whose program has exited, `0` otherwise.
fn any_pane_alive(listing: &str) -> bool {
    listing.lines().any(|l| l.trim() == "0")
}

/// Whether the agent of session `name` still runs, in the pane
/// [`new_session`] noted: a window the user opened there, a shell say,
/// keeps the session up but is not the agent. A session with no pane
/// noted falls back to [`pane_alive`].
pub fn agent_alive(name: &str) -> bool {
    let recorded = recorded_agent_pane(name);
    let recorded = recorded.trim();
    if pane_or_first_window(name, recorded) != recorded {
        return pane_alive(name);
    }
    let Ok(output) = command()
        .args([
            "list-panes",
            "-s",
            "-t",
            &target(name),
            "-F",
            "#{pane_id} #{pane_dead}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return false;
    };
    output.status.success() && pane_listed_alive(&String::from_utf8_lossy(&output.stdout), recorded)
}

/// Whether `pane` is in `listing` (`#{pane_id} #{pane_dead}` lines) and
/// alive: a pane closed as its agent exited is not listed at all.
fn pane_listed_alive(listing: &str, pane: &str) -> bool {
    listing
        .lines()
        .any(|l| l.split_whitespace().collect::<Vec<_>>() == [pane, "0"])
}

/// A pane whose program still runs, and what tmux says that program is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePane {
    pub pane: Pane,
    /// `#{pane_current_command}`: the program in the foreground there.
    pub command: String,
}

const LIVE_PANE_FORMAT: &str = "#{pane_dead}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}";

/// The pane of session `name` its agent runs in (see [`agent_pane`]),
/// while its program runs.
pub fn live_agent_pane(name: &str) -> Option<LivePane> {
    let output = command()
        .args(["display-message", "-p", "-t", &agent_pane(name)])
        .arg(LIVE_PANE_FORMAT)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()?;
    output.status.success().then_some(())?;
    parse_live_pane(String::from_utf8_lossy(&output.stdout).lines().next()?)
}

/// The first pane of window `id` (`@3`), the one [`new_window`] started
/// its program in, while that program runs.
pub fn live_window_pane(id: &str) -> Option<LivePane> {
    let output = command()
        .args(["list-panes", "-t", id, "-F", LIVE_PANE_FORMAT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
        .ok()?;
    output.status.success().then_some(())?;
    parse_live_pane(String::from_utf8_lossy(&output.stdout).lines().next()?)
}

fn parse_live_pane(line: &str) -> Option<LivePane> {
    let mut fields = line.trim_end_matches(['\n', '\r']).splitn(4, '\t');
    if fields.next()? != "0" {
        return None;
    }
    let pane = parse_pane(&[fields.next()?, fields.next()?]).ok()?;
    Some(LivePane {
        pane,
        command: fields.next().unwrap_or_default().to_string(),
    })
}

/// The processes tmux started in the live panes of jeet's sessions (those
/// named [`SESSION_PREFIX`]…): every task's and the assistant's, and the
/// windows the user opened in them.
pub fn jeet_pane_pids() -> Vec<u32> {
    let Ok(output) = command()
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_dead} #{pane_pid}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .bounded_output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_jeet_pane_pids(&String::from_utf8_lossy(&output.stdout))
}

fn parse_jeet_pane_pids(listing: &str) -> Vec<u32> {
    let ours: String = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(session, _)| session.starts_with(SESSION_PREFIX))
        .map(|(_, pane)| format!("{pane}\n"))
        .collect();
    parse_live_pane_pids(&ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_activity_is_that_of_the_clients_showing_the_window() {
        let listing = "1700000100\tjeet-a\t@1\n1700000200\tjeet-a\t@2\n\
                       1700000300\tother\t@1\nnot a client\n";
        assert_eq!(
            parse_client_activity(listing, "jeet-a", Some("@2")),
            [1_700_000_200]
        );
        assert_eq!(
            parse_client_activity(listing, "jeet-a", None),
            [1_700_000_100, 1_700_000_200]
        );
        assert!(parse_client_activity(listing, "jeet-a", Some("@9")).is_empty());
        assert!(parse_client_activity("", "jeet-a", None).is_empty());
    }

    /// A tmux that hangs is killed once its time is up, rather than
    /// holding up whoever called it; one that answers is read in full.
    #[test]
    fn a_tmux_call_that_hangs_is_given_up_on() {
        let child = Command::new("sh")
            .args(["-c", "echo hello; exec sleep 30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = Instant::now();
        let err = finish(child, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(still_running(&[pid]).is_empty(), "the hung call was left");

        let output = Command::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .bounded_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[test]
    fn a_live_pane_is_read_with_its_program() {
        assert_eq!(
            parse_live_pane("0\t%3\t4242\tclaude\n"),
            Some(LivePane {
                pane: Pane {
                    id: "%3".into(),
                    pid: 4242
                },
                command: "claude".into()
            })
        );
        assert_eq!(parse_live_pane("1\t%3\t4242\tclaude"), None);
        assert_eq!(parse_live_pane("0\tx\t4242\tclaude"), None);
        assert_eq!(parse_live_pane(""), None);
    }

    #[test]
    fn only_the_live_panes_of_jeets_sessions_are_its() {
        let listing = "jeet-api\t0 10\njeet-api\t1 11\nwork\t0 12\njeet-assistant\t0 13\n";
        assert_eq!(parse_jeet_pane_pids(listing), vec![10, 13]);
    }

    #[test]
    fn a_started_pane_is_read_with_its_process() {
        let pane = |id: &str, pid| Pane { id: id.into(), pid };
        assert_eq!(parse_pane(&["%1", "4242"]).unwrap(), pane("%1", 4242));
        assert_eq!(
            parse_window("@3\t%7\t99"),
            Some(("@3".to_string(), pane("%7", 99)))
        );
        for garbage in [
            &["%1"][..],
            &["@1", "5"],
            &["%1", "x"],
            &["%1", "5", "6"],
            &[],
        ] {
            assert!(parse_pane(garbage).is_err(), "{garbage:?}");
        }
        for garbage in ["", "@3", "%7\t99", "@3\t%7", "@3\t%7\tx", "x\t%7\t9"] {
            assert_eq!(parse_window(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn clients_are_read_with_their_activity() {
        let listing = "/dev/ttys001\t/dev/ttys001\tjeet-a\t100\n\
                       /dev/ttys002\t/dev/ttys002\twork\t300\n\
                       broken\n\
                       x\ty\tz\tsoon\n";
        let clients = parse_clients(listing);
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].session, "jeet-a");
        assert_eq!(most_recent(&clients).unwrap().name, "/dev/ttys002");
        assert_eq!(most_recent(&[]), None);
    }

    #[test]
    fn targets_match_exactly() {
        assert_eq!(target("jeet-api"), "=jeet-api");
        // The agent's pane, by its id, not whichever pane is active.
        assert_eq!(pane_or_first_window("jeet-api", "%12\n"), "%12");
        // No id noted: the agent's window, not whichever window is active.
        assert_eq!(pane_or_first_window("jeet-api", ""), "=jeet-api:^");
        assert_eq!(pane_or_first_window("jeet-api", "%\n"), "=jeet-api:^");
        assert_eq!(pane_or_first_window("jeet-api", "C-b"), "=jeet-api:^");
    }

    #[test]
    fn windows_are_read_with_their_keys() {
        let listing = "@1\t0\tabc\tabc\n@2\t1\t\t\n@3\t0\n@4\t0\tnew\told\n%junk\t0\tx\n";
        assert_eq!(
            parse_windows(listing),
            [
                Window {
                    id: "@1".into(),
                    alive: true,
                    key: Some("abc".into()),
                    launch: Some("abc".into()),
                },
                Window {
                    id: "@2".into(),
                    alive: false,
                    key: None,
                    launch: None,
                },
                Window {
                    id: "@3".into(),
                    alive: true,
                    key: None,
                    launch: None,
                },
                Window {
                    id: "@4".into(),
                    alive: true,
                    key: Some("new".into()),
                    launch: Some("old".into()),
                },
            ]
        );
        assert!(parse_windows("").is_empty());
    }

    #[test]
    fn a_tmux_conf_that_sets_extended_keys_is_found() {
        assert!(sets_extended_keys(
            "set -g mouse on\nset -s extended-keys off\n"
        ));
        assert!(sets_extended_keys("  set-option -s extended-keys always"));
        assert!(!sets_extended_keys("# set -s extended-keys on\n"));
        assert!(!sets_extended_keys("set -s extended-keys-format csi-u\n"));
        assert!(!sets_extended_keys(""));
    }

    #[test]
    fn window_names_cannot_run_formats() {
        assert_eq!(format_literal("plan the week"), "plan the week");
        assert_eq!(
            format_literal("#(touch /tmp/pwned) #{pane_pid} ##"),
            "##(touch /tmp/pwned) ##{pane_pid} ####"
        );
    }

    #[test]
    fn the_current_window_is_one_of_the_session() {
        assert_eq!(
            parse_current_window("jeet-assistant\t@4\tabc\n", "jeet-assistant"),
            Some(("@4".into(), Some("abc".into())))
        );
        assert_eq!(
            parse_current_window("jeet-assistant\t@4\t\n", "jeet-assistant"),
            Some(("@4".into(), None))
        );
        // Another session's window, or nothing sensible.
        assert!(parse_current_window("jeet-api\t@4\tabc\n", "jeet-assistant").is_none());
        assert!(parse_current_window("jeet-assistant\t%4\tabc", "jeet-assistant").is_none());
        assert!(parse_current_window("", "jeet-assistant").is_none());
    }

    #[test]
    fn a_session_is_alive_while_any_pane_is() {
        assert!(any_pane_alive("1\n0\n"));
        assert!(!any_pane_alive("1\n1\n"));
        assert!(!any_pane_alive(""));
        // The agent's own pane, not a shell the user opened beside it.
        assert!(pane_listed_alive("%3 0\n%7 1\n", "%3"));
        assert!(!pane_listed_alive("%3 1\n%7 0\n", "%3"));
        assert!(!pane_listed_alive("%7 0\n", "%3"));
        assert!(!pane_listed_alive("%33 0\n", "%3"));
    }

    #[test]
    fn only_live_panes_have_their_pids_taken() {
        assert_eq!(parse_live_pane_pids("0 42\n1 43\n0 44\n"), [42, 44]);
        assert!(parse_live_pane_pids("0\n0 \nnonsense\n").is_empty());
    }

    #[test]
    fn a_process_tree_takes_in_every_descendant_but_zombies_and_jeet() {
        let procs = parse_processes(
            "    1     0 Ss\n   10     1 S\n   11    10 S+\n   12    11 R\n   \
             13    10 Z\n   14     1 S\n   15    12 S\n   99    11 S\n",
        );
        let mut tree = descendants(&procs, &[10], 99);
        tree.sort();
        assert_eq!(tree, [10, 11, 12, 15]);
        assert!(descendants(&procs, &[13], 99).is_empty());
        assert!(descendants(&procs, &[77], 99).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ending_processes_waits_for_them_to_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 0.4"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = Instant::now();
        end_processes(&[pid], Duration::from_secs(10)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        // Exited by itself, not killed.
        assert!(child.wait().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    fn ending_processes_kills_what_outlives_the_grace() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        end_processes(&[child.id()], Duration::from_millis(200)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!child.wait().unwrap().success());
    }
}
