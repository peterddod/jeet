//! The jeet screens that are open, so `jeet open` can find one.
//!
//! A tasks screen the user runs at their own terminal notes itself in
//! `<JEET_HOME>/tui/<pid>.json` and takes it back out when it ends. To send
//! one to a link, `jeet open` writes `<pid>.goto`, which the screen reads on
//! its next wake-up. Nothing here is trusted beyond that: a record whose
//! process has gone is swept, and a request is only a link, read as one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity;
use crate::link::Link;
use crate::review::process_alive;
use crate::worktrees::now_secs;

/// One open screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub pid: u32,
    /// The terminal device it draws on (`/dev/ttys003`), when it has one.
    pub tty: Option<String>,
    /// The tmux pane it runs in, when it is in one.
    pub tmux_pane: Option<String>,
    /// The macOS app hosting it (`com.mitchellh.ghostty`), when known.
    pub app: Option<String>,
    /// Unix seconds.
    pub started: i64,
    /// When its process started, as [`identity::start_of`] has it: a pid
    /// reused by another process is not this screen.
    #[serde(default)]
    pub process_start: Option<i64>,
}

impl Screen {
    /// This process, as it runs now.
    pub fn here() -> Screen {
        let in_tmux = env("TMUX").is_some();
        Screen {
            pid: std::process::id(),
            tty: own_tty(),
            tmux_pane: env("TMUX_PANE").filter(|_| in_tmux),
            app: env("__CFBundleIdentifier").filter(|_| cfg!(target_os = "macos")),
            started: now_secs(),
            process_start: identity::start_of(std::process::id()),
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// What `tty` says of the terminal this process reads, which is the
/// screen's own, as stdin is inherited.
fn own_tty() -> Option<String> {
    let output = Command::new("tty")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(tty).filter(|t| t.starts_with('/'))
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("tui")
}

fn record_path(home: &Path, pid: u32) -> PathBuf {
    dir(home).join(format!("{pid}.json"))
}

fn goto_path(home: &Path, pid: u32) -> PathBuf {
    dir(home).join(format!("{pid}.goto"))
}

/// Write `text` to `path` so that a reader sees all of it or none.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no directory to write in")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("no file name")?.to_string_lossy();
    // Named for this process too: two writers must not share a temp file.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// Keeps a screen registered until it is dropped.
pub struct Registration {
    home: PathBuf,
    pid: u32,
}

/// Note `screen` as open, until the returned guard is dropped.
pub fn register(home: &Path, screen: &Screen) -> Result<Registration> {
    // A request left by an earlier process of this pid is not for this one.
    let _ = fs::remove_file(goto_path(home, screen.pid));
    write_atomic(
        &record_path(home, screen.pid),
        &serde_json::to_string(screen)?,
    )?;
    Ok(Registration {
        home: home.to_path_buf(),
        pid: screen.pid,
    })
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(record_path(&self.home, self.pid));
        let _ = fs::remove_file(goto_path(&self.home, self.pid));
    }
}

/// The screens open now, newest first. A record whose process has gone (or
/// is another one under the same pid), or that cannot be read, is removed
/// on the way: windows closed and processes killed leave records behind.
pub fn live(home: &Path) -> Vec<Screen> {
    live_with(home, |pid, start| {
        process_alive(pid) && start.is_none_or(|s| identity::start_of(pid) == Some(s))
    })
}

fn live_with(home: &Path, alive: impl Fn(u32, Option<i64>) -> bool) -> Vec<Screen> {
    let Ok(entries) = fs::read_dir(dir(home)) else {
        return Vec::new();
    };
    let mut screens = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_suffix(".json")
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        let screen = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Screen>(&text).ok())
            .filter(|screen| screen.pid == pid && alive(pid, screen.process_start));
        match screen {
            Some(screen) => screens.push(screen),
            None => {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(goto_path(home, pid));
            }
        }
    }
    screens.sort_by(|a, b| b.started.cmp(&a.started).then(b.pid.cmp(&a.pid)));
    screens
}

/// Ask the screen of `pid` to go to `link`.
pub fn request(home: &Path, pid: u32, link: &Link) -> Result<()> {
    write_atomic(&goto_path(home, pid), &link.to_string())
}

/// Whether the screen of `pid` has a request it has not taken yet.
pub fn requested(home: &Path, pid: u32) -> bool {
    goto_path(home, pid).exists()
}

/// Withdraw a request that was not taken. The screen's record stays: it
/// runs ([`live`] checked its start time) but is busy elsewhere — in the
/// explorer, the review viewer, a slow job — and takes the next link once
/// it is back.
pub fn withdraw(home: &Path, pid: u32) {
    let _ = fs::remove_file(goto_path(home, pid));
}

/// What the screen of `pid` was asked to go to, taking the request: the
/// text of a link, which the caller reads. The file is renamed away first,
/// so a request written meanwhile is not deleted unread.
pub fn take_request(home: &Path, pid: u32) -> Option<String> {
    let path = goto_path(home, pid);
    let mine = dir(home).join(format!(".{pid}.goto.taken"));
    fs::rename(&path, &mine).ok()?;
    let text = fs::read_to_string(&mine);
    let _ = fs::remove_file(&mine);
    Some(text.ok()?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(pid: u32, started: i64) -> Screen {
        Screen {
            pid,
            tty: Some("/dev/ttys001".into()),
            tmux_pane: None,
            app: None,
            started,
            process_start: None,
        }
    }

    #[test]
    fn a_screen_is_listed_while_registered() {
        let home = tempfile::tempdir().unwrap();
        let old = register(home.path(), &screen(10, 100)).unwrap();
        let new = register(home.path(), &screen(11, 200)).unwrap();
        let pids = |screens: Vec<Screen>| screens.iter().map(|s| s.pid).collect::<Vec<_>>();
        assert_eq!(pids(live_with(home.path(), |_, _| true)), [11, 10]);
        drop(new);
        assert_eq!(pids(live_with(home.path(), |_, _| true)), [10]);
        drop(old);
        assert!(live_with(home.path(), |_, _| true).is_empty());
    }

    #[test]
    fn a_record_of_a_dead_process_is_swept() {
        let home = tempfile::tempdir().unwrap();
        let _gone = register(home.path(), &screen(20, 100)).unwrap();
        let _here = register(home.path(), &screen(21, 50)).unwrap();
        request(home.path(), 20, &Link::task("x")).unwrap();
        let found = live_with(home.path(), |pid, _| pid == 21);
        assert_eq!(found, [screen(21, 50)]);
        assert!(!record_path(home.path(), 20).exists());
        assert!(!goto_path(home.path(), 20).exists());
    }

    #[test]
    fn a_reused_pid_is_not_the_screen() {
        let home = tempfile::tempdir().unwrap();
        let mut mine = screen(40, 1);
        mine.process_start = Some(500);
        let _reg = register(home.path(), &mine).unwrap();
        assert!(live_with(home.path(), |_, start| start == Some(501)).is_empty());
        assert!(!record_path(home.path(), 40).exists());
        let _reg = register(home.path(), &mine).unwrap();
        assert_eq!(
            live_with(home.path(), |_, start| start == Some(500)).len(),
            1
        );
    }

    #[test]
    fn junk_in_the_directory_is_not_a_screen() {
        let home = tempfile::tempdir().unwrap();
        let dir = dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("30.json"), "not json").unwrap();
        // Named for one pid, claiming another.
        fs::write(
            dir.join("31.json"),
            serde_json::to_string(&screen(99, 1)).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        assert!(live_with(home.path(), |_, _| true).is_empty());
        assert!(!dir.join("30.json").exists());
        assert!(dir.join("notes.txt").exists());
    }

    #[test]
    fn a_request_is_taken_once() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(take_request(home.path(), 5), None);
        request(home.path(), 5, &Link::Report(9)).unwrap();
        request(home.path(), 5, &Link::task("fix")).unwrap();
        assert_eq!(
            take_request(home.path(), 5).as_deref(),
            Some("jeet://task/fix")
        );
        assert_eq!(take_request(home.path(), 5), None);
    }

    #[test]
    fn an_untaken_request_is_withdrawn_and_its_record_kept() {
        let home = tempfile::tempdir().unwrap();
        let _reg = register(home.path(), &screen(8, 1)).unwrap();
        request(home.path(), 8, &Link::task("x")).unwrap();
        assert!(requested(home.path(), 8));
        withdraw(home.path(), 8);
        assert!(!requested(home.path(), 8));
        assert_eq!(live_with(home.path(), |_, _| true).len(), 1);
    }

    #[test]
    fn registering_clears_a_request_left_behind() {
        let home = tempfile::tempdir().unwrap();
        request(home.path(), 6, &Link::task("old")).unwrap();
        let _reg = register(home.path(), &screen(6, 1)).unwrap();
        assert_eq!(take_request(home.path(), 6), None);
    }
}
