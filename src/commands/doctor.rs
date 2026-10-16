use anyhow::Result;

use crate::agent::age_label;
use crate::context::App;
use crate::waiter::{RepairWhy, WakeState};
use crate::wake::{self, WakeRow};
use crate::worktrees::now_secs;

/// `jeet doctor wake`: each live session's wake channel, read-only.
pub fn wake(app: &App) -> Result<()> {
    let rows = wake::sweep_wake(app, now_secs(), false)?;
    if rows.is_empty() {
        eprintln!("jeet: no session is running");
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
