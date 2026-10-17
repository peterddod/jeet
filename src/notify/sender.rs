//! The sender: `jeet notify serve`, a small long-running process that
//! watches the outbox and sends what is queued at once. It is the only
//! process that reads a backend's token. `jeet notify install` has launchd
//! (macOS) or systemd (a user unit, elsewhere) keep it running; nothing
//! else installs it.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::watch::{first_line, launchctl, launchd_domain, systemd_word, xml};
use crate::worktrees::now_secs;

use super::{drain, make_dir, open_owned, outbox_path, secrets, sender_lock_path, Drained};

/// The launchd job's label, and its plist's name.
pub const LABEL: &str = "dev.jeet.notify";

/// The systemd user unit.
pub const UNIT: &str = "jeet-notify.service";

/// How often the outbox is looked at.
const POLL: Duration = Duration::from_millis(250);

/// How long it waits after a failed send before trying again.
const RETRY: Duration = Duration::from_secs(10);

/// How often it routes with nothing queued, to send an "N more" once the
/// hour allows.
const UNFOLD_SECS: i64 = 60;

/// `jeet notify serve`: send what is queued, until killed. One runs per
/// jeet home; another refuses to start. Run in what jeet takes for an
/// agent's process (`agent`), it reads no token, and what needs one waits.
pub fn serve(home: &Path, agent: bool) -> Result<()> {
    make_dir(home)?;
    let path = sender_lock_path(home);
    let lock = open_owned(
        &path,
        OpenOptions::new().create(true).truncate(false).write(true),
    )?;
    // A probe ([`super::sender_running`]) holds it shared for a moment:
    // only one held throughout is another sender.
    let mut tries = 0;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tries < 20 => tries += 1,
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("a sender runs already for {}", home.display())
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if agent {
        eprintln!(
            "jeet notify serve: this runs as an agent's process, so it reads no token; \
             run it from your own terminal or with `jeet notify install`"
        );
    }
    let tokens = secrets::for_sender(home, agent);
    let outbox = outbox_path(home);
    eprintln!(
        "jeet notify serve: sending what is queued in {}",
        outbox.display()
    );
    let mut routed = 0;
    let mut left = Drained::default();
    loop {
        // Something queued since the last drain, or a failed send due again.
        let len = std::fs::metadata(&outbox).map_or(0, |m| m.len());
        let now = now_secs();
        let due = left.retry_at.is_some_and(|at| at <= now);
        if len != left.left || due || now - routed >= UNFOLD_SECS {
            routed = now;
            match drain(home, tokens.as_ref(), true) {
                Ok(drained) => left = drained,
                Err(e) => {
                    eprintln!("jeet notify serve: {e:#}");
                    // Tried again once things may have changed, not every poll.
                    std::thread::sleep(RETRY);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

fn log_path(home: &Path) -> PathBuf {
    super::dir(home).join("sender.log")
}

/// Where the installed sender's output goes, for the user to look.
pub fn log_hint(home: &Path) -> String {
    match cfg!(target_os = "macos") {
        true => log_path(home).display().to_string(),
        false => format!("`journalctl --user -u {UNIT}`"),
    }
}

/// The launchd job: `exe notify serve`, started at load and kept running,
/// with jeet's home and the `PATH` it was installed from (launchd's own
/// has no `terminal-notifier`).
pub fn plist(exe: &Path, home: &Path, path_env: &str) -> String {
    let log = log_path(home);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>notify</string>
		<string>serve</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>EnvironmentVariables</key>
	<dict>
		<key>JEET_HOME</key>
		<string>{home}</string>
		<key>PATH</key>
		<string>{path}</string>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        exe = xml(&exe.to_string_lossy()),
        home = xml(&home.to_string_lossy()),
        path = xml(path_env),
        log = xml(&log.to_string_lossy()),
    )
}

/// The systemd user service that does what the launchd job does.
pub fn systemd_unit(exe: &Path, home: &Path, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=jeet: send notifications\n\n\
         [Service]\nEnvironment={home}\nEnvironment={path}\n\
         ExecStart={exe} notify serve\nRestart=always\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        home = systemd_word(&format!("JEET_HOME={}", home.display())),
        path = systemd_word(&format!("PATH={path_env}")),
        exe = systemd_word(&exe.to_string_lossy()),
    )
}

fn plist_path() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| {
            h.join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist"))
        })
        .context("could not determine home directory")
}

/// `$XDG_CONFIG_HOME/systemd/user/jeet-notify.service`.
fn unit_path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("could not determine home directory")?;
    Ok(config.join("systemd/user").join(UNIT))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("run systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            first_line(&output.stderr)
        );
    }
    Ok(())
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// `jeet notify install`.
pub fn install(home: &Path) -> Result<()> {
    make_dir(home)?;
    // As run, not canonicalized: through a package manager's link, such as
    // Homebrew's, it outlives an upgrade.
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    if !cfg!(target_os = "macos") {
        let path = unit_path()?;
        write_file(&path, &systemd_unit(&exe, home, &path_env))?;
        systemctl(&["daemon-reload"])?;
        // Installed before: restarted, with this jeet.
        systemctl(&["enable", UNIT])?;
        systemctl(&["restart", UNIT])?;
        eprintln!(
            "jeet: systemd runs the notification sender ({UNIT}, {}); `journalctl --user -u \
             {UNIT}` has what it says",
            path.display()
        );
        return Ok(());
    }
    let path = plist_path()?;
    write_file(&path, &plist(&exe, home, &path_env))?;
    let domain = launchd_domain()?;
    // Installed before: replaced, with this jeet.
    let replaced =
        launchctl(&["bootout", &format!("{domain}/{LABEL}")]).is_ok_and(|o| o.status.success());
    let bootstrap = || launchctl(&["bootstrap", &domain, &path.to_string_lossy()]);
    let mut output = bootstrap()?;
    // launchd can still be tearing the old job down; once more, after it has.
    if !output.status.success() && replaced {
        std::thread::sleep(Duration::from_secs(1));
        output = bootstrap()?;
    }
    if !output.status.success() {
        bail!(
            "launchctl could not load {}: {}",
            path.display(),
            first_line(&output.stderr)
        );
    }
    eprintln!(
        "jeet: launchd runs the notification sender ({LABEL}); it logs to {}",
        log_path(home).display()
    );
    Ok(())
}

/// `jeet notify uninstall`.
pub fn uninstall() -> Result<()> {
    let removed = |path: &Path| match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    };
    let gone = if !cfg!(target_os = "macos") {
        let path = unit_path()?;
        if !path.exists() {
            false
        } else {
            // Stopped whether or not it was enabled.
            let stopped = systemctl(&["disable", "--now", UNIT]);
            let removed = removed(&path)?;
            systemctl(&["daemon-reload"])?;
            stopped?;
            removed
        }
    } else {
        let path = plist_path()?;
        let domain = launchd_domain()?;
        let unloaded = launchctl(&["bootout", &format!("{domain}/{LABEL}")])?
            .status
            .success();
        removed(&path)? || unloaded
    };
    match gone {
        true => eprintln!("jeet: the notification sender is no longer installed"),
        false => eprintln!("jeet: the notification sender was not installed"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launchd_plist_keeps_one_sender_running() {
        let text = plist(
            Path::new("/opt/jeet & co/jeet"),
            Path::new("/Users/me/.jeet"),
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(text.contains(
            "<array>\n\t\t<string>/opt/jeet &amp; co/jeet</string>\n\t\t<string>notify</string>\n\t\t<string>serve</string>\n\t</array>"
        ), "{text}");
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"), "{text}");
        assert!(
            text.contains("<string>/opt/homebrew/bin:/usr/bin</string>"),
            "{text}"
        );
        assert!(
            text.contains("<string>/Users/me/.jeet/run/notify/sender.log</string>"),
            "{text}"
        );
    }

    /// launchd's own parser takes it.
    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_reads_the_plist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.plist");
        let text = plist(Path::new("/a <b>/jeet"), Path::new("/h&/.jeet"), "/x:/y");
        std::fs::write(&path, text).unwrap();
        let output = Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let output = Command::new("plutil")
            .args(["-extract", "ProgramArguments.0", "raw"])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "/a <b>/jeet"
        );
    }

    /// systemd's own parser takes it, where there is one to ask.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_analyze_reads_the_unit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(UNIT);
        std::fs::write(
            &path,
            systemd_unit(Path::new("/bin/true"), dir.path(), "/usr/bin"),
        )
        .unwrap();
        let Ok(output) = Command::new("systemd-analyze")
            .arg("verify")
            .arg(&path)
            .output()
        else {
            return;
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(!stderr.contains(UNIT), "{stderr}");
    }

    #[test]
    fn the_systemd_unit_restarts_the_sender() {
        let text = systemd_unit(
            Path::new("/opt/jeet 100%/jeet"),
            Path::new("/home/me/.jeet"),
            "/usr/bin",
        );
        assert_eq!(
            text,
            "[Unit]\nDescription=jeet: send notifications\n\n\
             [Service]\nEnvironment=\"JEET_HOME=/home/me/.jeet\"\nEnvironment=\"PATH=/usr/bin\"\n\
             ExecStart=\"/opt/jeet 100%%/jeet\" notify serve\nRestart=always\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n"
        );
    }
}
