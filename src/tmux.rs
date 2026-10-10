//! tmux: where task agents run.
//!
//! Every task's agent lives in a detached tmux session named `jeet-<slug>`,
//! so it keeps working after the terminal that started it has gone, and can
//! be attached to again from anywhere.
//!
//! `$JEET_TMUX` names the tmux binary, for a tmux outside `$PATH` and for
//! tests, which point it at a stub so they never reach a real server.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

pub const BINARY_ENV: &str = "JEET_TMUX";

pub const INSTALL_HINT: &str = "jeet runs task agents in tmux, which was not found; install it \
     (`brew install tmux` on macOS, `sudo apt install tmux` on Debian and Ubuntu)";

fn binary() -> String {
    std::env::var(BINARY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "tmux".to_string())
}

/// A tmux command that cannot read the terminal and says nothing on it.
fn quiet() -> Command {
    let mut cmd = Command::new(binary());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// An exact-match session target. A bare name is matched as a prefix, so
/// `jeet-api` would otherwise find `jeet-api-2` once `jeet-api` is gone.
fn target(name: &str) -> String {
    format!("={name}")
}

/// Whether tmux can be run at all.
pub fn available() -> bool {
    quiet()
        .arg("-V")
        .status()
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

pub fn has_session(name: &str) -> bool {
    quiet()
        .args(["has-session", "-t", &target(name)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start `argv` in a new detached session `name`, in `cwd`, with `env` set
/// in it. `argv` is run directly, never through a shell.
pub fn new_session(name: &str, cwd: &Path, env: &[(&str, &str)], argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        bail!("nothing to run in tmux session {name}");
    }
    let mut cmd = Command::new(binary());
    cmd.args(["new-session", "-d", "-s", name, "-c"]).arg(cwd);
    for (key, value) in env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg("--").args(argv);
    // A server this starts keeps a copy of our environment for every pane it
    // ever makes: the shell wrapper's cd file and this task's id must not
    // leak into sessions of other tasks that way.
    cmd.env_remove(crate::cd::CD_FILE_ENV)
        .env_remove(crate::tasks::TASK_ID_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let output = cmd.output().context("run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux could not start session {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    set_detach_hint(name);
    Ok(())
}

/// Show how to get back to jeet in the session's status line. Cosmetic, so
/// failures are ignored.
fn set_detach_hint(name: &str) {
    let prefix = Command::new(binary())
        .args(["show-options", "-gv", "prefix"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty() && p != "None")
        .unwrap_or_else(|| "C-b".to_string());
    let _ = quiet()
        .args(["set-option", "-t", &target(name), "status-right"])
        .arg(detach_hint(&prefix))
        .status();
}

fn detach_hint(prefix: &str) -> String {
    // `#` starts a format in status-right; a prefix key could be `#`.
    format!(" {} d → jeet ", prefix.replace('#', "##"))
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
    let output = Command::new(binary())
        .args(["kill-session", "-t", &target(name)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
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
    let Ok(output) = Command::new(binary())
        .args([
            "list-panes",
            "-s",
            "-t",
            &target(name),
            "-F",
            "#{pane_dead} #{pane_pid}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
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
fn process_tree(roots: &[u32]) -> Vec<u32> {
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
fn end_processes(pids: &[u32], grace: Duration) -> Result<()> {
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
    let mut cmd = Command::new(binary());
    cmd.args(["display-message", "-p"]);
    // The pane this process runs in, not whichever the client last used.
    if let Some(pane) = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()) {
        cmd.args(["-t", &pane]);
    }
    let output = cmd
        .arg("#{session_name}")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Whether anything is still running in session `name`: false when the
/// session is gone or every pane in it has exited.
pub fn pane_alive(name: &str) -> bool {
    let Ok(output) = Command::new(binary())
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
        .output()
    else {
        return false;
    };
    output.status.success() && any_pane_alive(&String::from_utf8_lossy(&output.stdout))
}

/// `#{pane_dead}` is `1` for a pane whose program has exited, `0` otherwise.
fn any_pane_alive(listing: &str) -> bool {
    listing.lines().any(|l| l.trim() == "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_match_exactly() {
        assert_eq!(target("jeet-api"), "=jeet-api");
    }

    #[test]
    fn hint_escapes_format_characters() {
        assert_eq!(detach_hint("C-b"), " C-b d → jeet ");
        assert_eq!(detach_hint("#"), " ## d → jeet ");
    }

    #[test]
    fn a_session_is_alive_while_any_pane_is() {
        assert!(any_pane_alive("1\n0\n"));
        assert!(!any_pane_alive("1\n1\n"));
        assert!(!any_pane_alive(""));
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
