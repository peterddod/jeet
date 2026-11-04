//! The sender: `plete notify serve`, a small long-running process that
//! watches the outbox and sends what is queued at once. It is the only
//! process that reads a backend's token. `plete notify install` has launchd
//! (macOS) or systemd (a user unit, elsewhere) keep it running; nothing
//! else installs it.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::watch::{
    first_line, launchctl, launchd_domain, plist_path, retire_job, systemd_word, xml,
};
use crate::worktrees::now_secs;

use super::{drain, make_dir, open_owned, outbox_path, secrets, sender_lock_path, Drained};

/// The launchd job's label, and its plist's name.
pub const LABEL: &str = "dev.plete.notify";

/// The systemd user unit.
pub const UNIT: &str = "plete-notify.service";

/// The label and unit before the rename.
pub const LEGACY_LABEL: &str = "dev.jeet.notify";
pub const LEGACY_UNIT: &str = "jeet-notify.service";

/// How often the outbox is looked at.
const POLL: Duration = Duration::from_millis(250);

/// How long it waits after a failed send before trying again.
const RETRY: Duration = Duration::from_secs(10);

/// How often it routes with nothing queued, to send an "N more" once the
/// hour allows.
const UNFOLD_SECS: i64 = 60;

/// `plete notify serve`: send what is queued, until killed. One runs per
/// plete home; another refuses to start. Run in what plete takes for an
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
            "plete notify serve: this runs as an agent's process, so it reads no token; \
             run it from your own terminal or with `plete notify install`"
        );
    }
    let tokens = secrets::for_sender(home, agent);
    let outbox = outbox_path(home);
    eprintln!(
        "plete notify serve: sending what is queued in {}",
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
                    eprintln!("plete notify serve: {e:#}");
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
/// with plete's home and the `PATH` it was installed from (launchd's own
/// has no `terminal-notifier`).
pub fn plist(exe: &Path, home: &Path, new_home: Option<&Path>, path_env: &str) -> String {
    let log = log_path(home);
    let new_home = new_home.map_or(String::new(), |h| {
        format!(
            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
            crate::env::NEW_HOME_VAR,
            xml(&h.to_string_lossy())
        )
    });
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
		<key>PLETE_HOME</key>
		<string>{home}</string>
{new_home}		<key>PATH</key>
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
pub fn systemd_unit(exe: &Path, home: &Path, new_home: Option<&Path>, path_env: &str) -> String {
    let split = new_home.map_or(String::new(), |h| {
        let var = format!("{}={}", crate::env::NEW_HOME_VAR, h.display());
        format!("Environment={}\n", systemd_word(&var))
    });
    format!(
        "[Unit]\nDescription=plete: send notifications\n\n\
         [Service]\nEnvironment={home}\nEnvironment={new_home}\n{split}Environment={path}\n\
         ExecStart={exe} notify serve\nRestart=always\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        home = systemd_word(&format!("JEET_HOME={}", home.display())),
        new_home = systemd_word(&format!("PLETE_HOME={}", home.display())),
        path = systemd_word(&format!("PATH={path_env}")),
        exe = systemd_word(&exe.to_string_lossy()),
    )
}

/// `$XDG_CONFIG_HOME/systemd/user/<unit>`.
fn unit_path(unit: &str) -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("could not determine home directory")?;
    Ok(config.join("systemd/user").join(unit))
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

/// Whether the sender is installed, under either name.
pub fn installed() -> bool {
    match cfg!(target_os = "macos") {
        true => [LABEL, LEGACY_LABEL]
            .iter()
            .any(|label| plist_path(label).is_ok_and(|p| p.exists())),
        false => [UNIT, LEGACY_UNIT]
            .iter()
            .any(|unit| unit_path(unit).is_ok_and(|p| p.exists())),
    }
}

/// `plete notify install`.
pub fn install(home: &Path, new_home: Option<&Path>) -> Result<()> {
    make_dir(home)?;
    // As run, not canonicalized: through a package manager's link, such as
    // Homebrew's, it outlives an upgrade.
    let exe = crate::paths::plete_exe()?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    if !cfg!(target_os = "macos") {
        let path = unit_path(UNIT)?;
        write_file(&path, &systemd_unit(&exe, home, new_home, &path_env))?;
        systemctl(&["daemon-reload"])?;
        // Installed before: restarted, with this plete.
        systemctl(&["enable", UNIT])?;
        systemctl(&["restart", UNIT])?;
        // Only now is the unit from before the rename replaced by this one.
        // The unit from before the rename is replaced by this one.
        let old = unit_path(LEGACY_UNIT)?;
        if old.exists() {
            let _ = systemctl(&["disable", "--now", LEGACY_UNIT]);
            std::fs::remove_file(&old).with_context(|| format!("remove {}", old.display()))?;
            eprintln!("plete: replaced the {LEGACY_UNIT} unit");
        }
        eprintln!(
            "plete: systemd runs the notification sender ({UNIT}, {}); `journalctl --user -u \
             {UNIT}` has what it says",
            path.display()
        );
        return Ok(());
    }
    let path = plist_path(LABEL)?;
    write_file(&path, &plist(&exe, home, new_home, &path_env))?;
    let domain = launchd_domain()?;
    // Installed before: replaced, with this plete.
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
    // Only now that this one is loaded is the job from before the rename
    // replaced by it.
    if retire_job(LEGACY_LABEL)? {
        eprintln!("plete: replaced the {LEGACY_LABEL} job");
    }
    eprintln!(
        "plete: launchd runs the notification sender ({LABEL}); it logs to {}",
        log_path(home).display()
    );
    Ok(())
}

/// `plete notify uninstall`.
pub fn uninstall() -> Result<()> {
    let removed = |path: &Path| match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    };
    let gone = if !cfg!(target_os = "macos") {
        let mut gone = false;
        for unit in [UNIT, LEGACY_UNIT] {
            let path = unit_path(unit)?;
            if !path.exists() {
                continue;
            }
            // Stopped whether or not it was enabled.
            let stopped = systemctl(&["disable", "--now", unit]);
            let removed = removed(&path)?;
            systemctl(&["daemon-reload"])?;
            stopped?;
            gone |= removed;
        }
        gone
    } else {
        let new = retire_job(LABEL)?;
        let old = retire_job(LEGACY_LABEL)?;
        new || old
    };
    match gone {
        true => eprintln!("plete: the notification sender is no longer installed"),
        false => eprintln!("plete: the notification sender was not installed"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launchd_plist_keeps_one_sender_running() {
        let text = plist(
            Path::new("/opt/plete & co/plete"),
            Path::new("/Users/me/.plete"),
            None,
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(text.contains(
            "<array>\n\t\t<string>/opt/plete &amp; co/plete</string>\n\t\t<string>notify</string>\n\t\t<string>serve</string>\n\t</array>"
        ), "{text}");
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"), "{text}");
        assert!(
            text.contains("<string>/opt/homebrew/bin:/usr/bin</string>"),
            "{text}"
        );
        assert!(
            text.contains("<string>/Users/me/.plete/run/notify/sender.log</string>"),
            "{text}"
        );
    }

    /// launchd's own parser takes it.
    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_reads_the_plist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.plist");
        let text = plist(
            Path::new("/a <b>/plete"),
            Path::new("/h&/.plete"),
            None,
            "/x:/y",
        );
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
            "/a <b>/plete"
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
            systemd_unit(Path::new("/bin/true"), dir.path(), None, "/usr/bin"),
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
    fn a_split_home_is_baked_into_the_sender() {
        let new = Path::new("/home/me/.plete");
        let text = plist(
            Path::new("/bin/plete"),
            Path::new("/home/me/.jeet"),
            Some(new),
            "/usr/bin",
        );
        assert!(
            text.contains("<key>PLETE_NEW_HOME</key>\n\t\t<string>/home/me/.plete</string>"),
            "{text}"
        );
        let unit = systemd_unit(
            Path::new("/bin/plete"),
            Path::new("/home/me/.jeet"),
            Some(new),
            "/usr/bin",
        );
        assert!(
            unit.contains("Environment=\"PLETE_NEW_HOME=/home/me/.plete\"\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_systemd_unit_restarts_the_sender() {
        let text = systemd_unit(
            Path::new("/opt/plete 100%/plete"),
            Path::new("/home/me/.plete"),
            None,
            "/usr/bin",
        );
        assert_eq!(
            text,
            "[Unit]\nDescription=plete: send notifications\n\n\
             [Service]\nEnvironment=\"JEET_HOME=/home/me/.plete\"\n\
             Environment=\"PLETE_HOME=/home/me/.plete\"\nEnvironment=\"PATH=/usr/bin\"\n\
             ExecStart=\"/opt/plete 100%%/plete\" notify serve\nRestart=always\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n"
        );
    }
}
