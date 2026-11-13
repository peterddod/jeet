use std::path::PathBuf;

use anyhow::Result;

use crate::agent::age_label;
use crate::context::App;
use crate::db::tasks::TaskState;
use crate::waiter::{RepairWhy, WakeState};
use crate::wake::{self, WakeRow};
use crate::worktrees::now_secs;

/// `plete doctor wake`: each live session's wake channel, read-only.
pub fn wake(app: &App) -> Result<()> {
    let rows = wake::sweep_wake(app, now_secs(), false)?;
    if rows.is_empty() {
        eprintln!("plete: no session is running");
        return Ok(());
    }
    println!(
        "{:<28} {:<10} {:<40} {:<14} REPAIR",
        "TASK", "SESSION", "STATE", "LAST REPAIR"
    );
    for row in rows {
        let repaired = row
            .repaired
            .as_ref()
            .map_or("-".to_string(), |r| age_label(r.at));
        println!(
            "{:<28} {:<10} {:<40} {:<14} {}",
            row.slug,
            row.session.chars().take(8).collect::<String>(),
            state_label(&row.state),
            repaired,
            repair_label(&row)
        );
    }
    Ok(())
}

fn state_label(state: &WakeState) -> String {
    match state {
        WakeState::Watched => "watched".to_string(),
        WakeState::Unwatched => "unwatched".to_string(),
        WakeState::Off {
            permanent: true, ..
        } => "off for good".to_string(),
        WakeState::Off {
            repair_due: true, ..
        } => "off: repair due".to_string(),
        WakeState::Off {
            refusals, retry_at, ..
        } => format!(
            "off: backing off until {} UTC ({refusals} refused)",
            clock(*retry_at)
        ),
    }
}

/// `HH:MM` of a unix time, in UTC: there is no local time to hand.
fn clock(at: i64) -> String {
    let secs = at.rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

fn repair_label(row: &WakeRow) -> &'static str {
    match (row.due, row.ready) {
        (Some(RepairWhy::Unwatched | RepairWhy::Refused), true) => "ready",
        (Some(_), false) => "repair due",
        (None, _) => "-",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_read_as_the_doctor_says_them() {
        let off = |refusals, retry_at, permanent, repair_due| WakeState::Off {
            refusals,
            retry_at,
            permanent,
            repair_due,
        };
        assert_eq!(state_label(&WakeState::Watched), "watched");
        assert_eq!(state_label(&WakeState::Unwatched), "unwatched");
        assert_eq!(state_label(&off(3, 0, true, false)), "off for good");
        assert_eq!(state_label(&off(3, 0, false, true)), "off: repair due");
        assert_eq!(
            state_label(&off(2, 3 * 3600 + 5 * 60 + 9, false, false)),
            "off: backing off until 03:05 UTC (2 refused)"
        );
    }
}

/// What is left from before the rename, found on this machine.
#[derive(Debug, Default)]
pub struct Leftovers {
    /// launchd plists and systemd units under the old names.
    pub jobs: Vec<PathBuf>,
    /// tmux sessions named `jeet-…`.
    pub sessions: Vec<String>,
    /// Live tasks whose marker directory is still `.jeet`.
    pub marked_tasks: Vec<String>,
    /// The rc file that still sources `jeet init-shell`.
    pub rc_line: Option<PathBuf>,
    /// The index home is `~/.jeet`.
    pub old_home: bool,
}

impl Leftovers {
    /// One line per leftover, each saying what to do about it.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for job in &self.jobs {
            lines.push(format!(
                "{}: the old job; `plete watch install` / `plete notify install` move it",
                job.display()
            ));
        }
        if !self.sessions.is_empty() {
            lines.push(format!(
                "tmux sessions still named jeet-*: {} (they end with their tasks)",
                self.sessions.join(", ")
            ));
        }
        if !self.marked_tasks.is_empty() {
            lines.push(format!(
                "live tasks with a .jeet/ marker: {} (they keep it for life)",
                self.marked_tasks.join(", ")
            ));
        }
        if let Some(rc) = &self.rc_line {
            lines.push(format!(
                "{} sources `jeet init-shell`; change it to `plete init-shell`",
                rc.display()
            ));
        }
        if self.old_home {
            lines.push("the data home is still ~/.jeet; `plete migrate-home` moves it".to_string());
        }
        lines
    }
}

/// Look for the leftovers: read-only.
fn leftovers(app: &App) -> Leftovers {
    let mut found = Leftovers::default();
    let old_labels = [
        crate::watch::LEGACY_LABEL,
        crate::notify::sender::LEGACY_LABEL,
    ];
    if cfg!(target_os = "macos") {
        found.jobs = old_labels
            .iter()
            .filter_map(|label| crate::watch::plist_path(label).ok())
            .filter(|p| p.exists())
            .collect();
    }
    found.sessions = crate::tmux::list_sessions()
        .into_iter()
        .filter(|name| name.starts_with(crate::tmux::LEGACY_SESSION_PREFIX))
        .collect();
    let active = app
        .db
        .list_tasks(Some(TaskState::Active))
        .unwrap_or_default();
    found.marked_tasks = active
        .iter()
        .filter(|t| {
            let dir = crate::tasks::task_dir(app, t);
            dir.join(crate::tasks::LEGACY_MARKER_DIR).is_dir()
                && !dir.join(crate::tasks::NEW_MARKER_DIR).is_dir()
        })
        .map(|t| t.slug.clone())
        .collect();
    found.rc_line =
        super::install_shell::detect_rc_file(&std::env::var("SHELL").unwrap_or_default())
            .map(PathBuf::from)
            .filter(|rc| {
                std::fs::read_to_string(rc)
                    .is_ok_and(|text| super::install_shell::has_old_integration(&text))
            });
    found.old_home = app
        .home
        .file_name()
        .is_some_and(|n| n == crate::config::LEGACY_HOME_DIR);
    found
}

/// `plete doctor rename`: what is left from jeet's old name, and the state
/// of the `pt` alias. Read-only.
pub fn rename(app: &App) -> Result<()> {
    let lines = leftovers(app).lines();
    println!("rename:");
    match lines.is_empty() {
        true => println!("  nothing is left from before the rename"),
        false => lines.iter().for_each(|l| println!("  {l}")),
    }
    match crate::paths::plete_exe() {
        Ok(exe) => {
            let path: Vec<PathBuf> = std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default();
            println!("  {}", super::alias::state(&path, &exe).describe());
        }
        Err(e) => println!("  pt: unknown ({e:#})"),
    }
    Ok(())
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn leftovers_are_listed_with_what_to_do() {
        assert!(Leftovers::default().lines().is_empty());
        let found = Leftovers {
            jobs: vec![PathBuf::from("/L/dev.jeet.watch.plist")],
            sessions: vec!["jeet-a".into(), "jeet-b".into()],
            marked_tasks: vec!["fix".into()],
            rc_line: Some(PathBuf::from("/h/.zshrc")),
            old_home: true,
        };
        let lines = found.lines();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("dev.jeet.watch.plist"));
        assert!(lines[1].contains("jeet-a, jeet-b"));
        assert!(lines[2].contains("fix"));
        assert!(lines[3].contains("/h/.zshrc"));
        assert!(lines[4].contains("migrate-home"));
    }
}
