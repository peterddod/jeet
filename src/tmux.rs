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
pub fn kill(name: &str) -> Result<()> {
    if !has_session(name) {
        return Ok(());
    }
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
    Ok(())
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
}
