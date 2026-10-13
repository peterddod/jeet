//! `jeet report`: a task with a parent tells it how it is going, and a
//! node answers, passes on or lists the questions that reach it.

use anyhow::{bail, Result};

use crate::agent::age_label;
use crate::assistant;
use crate::cli::ReportCommands;
use crate::commands::task::printable_text;
use crate::context::App;
use crate::db::tasks::{ReportKind, TaskReportRecord};
use crate::manager;
use crate::messages::Delivered;
use crate::permissions::{self, Caller};
use crate::reports;
use crate::tasks::TASK_ID_ENV;

pub fn run(app: &App, what: ReportCommands) -> Result<()> {
    match what {
        ReportCommands::Progress { message } => send_kind(app, ReportKind::Progress, &message),
        ReportCommands::Done { message } => send_kind(app, ReportKind::Done, &message),
        ReportCommands::Blocked { message } => send_kind(app, ReportKind::Blocked, &message),
        ReportCommands::Question { message } => send_kind(app, ReportKind::Question, &message),
        ReportCommands::Answer { id, message } => answer(app, id, &message),
        ReportCommands::Escalate { id } => escalate(app, id),
        ReportCommands::Ls => ls(app),
    }
}

fn send_kind(app: &App, kind: ReportKind, message: &str) -> Result<()> {
    let task = match permissions::caller(app)? {
        Caller::Node(task) => task,
        Caller::Root => bail!("the assistant has no parent; `jeet notify` tells the user"),
        Caller::User => match std::env::var(TASK_ID_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            Some(id) => match app.db.get_task(id.trim())? {
                Some(_) => bail!(
                    "this task has no parent; `jeet report` is for tasks made under the assistant or a \
                     manager"
                ),
                None => bail!(
                    "${TASK_ID_ENV} is {}, which is no task jeet knows; was it deleted?",
                    id.trim()
                ),
            },
            None => bail!("`jeet report` runs inside a task's agent (${TASK_ID_ENV} is not set)"),
        },
    };
    // Progress is not worth a session of the parent's own: `send` says
    // which kinds are.
    let (id, delivered) = reports::send(app, &task, kind, message, true)?;
    // `send` found the parent, so it is there.
    let to = match task.parent_id.as_deref().map(|p| app.db.get_task(p)) {
        Some(Ok(Some(parent))) => manager::label(&parent),
        _ => "its parent".to_string(),
    };
    eprintln!("jeet: reported {} to {to} (#{id})", kind.as_str());
    eprintln!(
        "{}",
        delivered_line(
            &to,
            "the report",
            &format!("report #{id} recorded, but not delivered"),
            &delivered
        )
    );
    Ok(())
}

/// Who runs this, refused when it is the user's word and this is not the
/// user's own terminal: a top-level task's agent, or a window in one of
/// jeet's sessions, is [`Caller::User`] too (R8).
fn caller_at_terminal(app: &App, doing: &str) -> Result<Caller> {
    let caller = permissions::caller(app)?;
    if caller == Caller::User && !permissions::at_terminal(app)? {
        bail!(
            "{doing} as you runs at your own terminal, outside jeet's tmux sessions and any \
             task's pane"
        );
    }
    Ok(caller)
}

fn answer(app: &App, id: i64, message: &str) -> Result<()> {
    let caller = caller_at_terminal(app, "answering")?;
    let answered = reports::answer(app, &caller, id, message)?;
    let id = answered.row.id;
    let asker = manager::label(&answered.asker);
    let by = match caller {
        Caller::User => " (as the user)",
        Caller::Root | Caller::Node(_) => "",
    };
    eprintln!("jeet: answered #{id} for {asker}{by}");
    eprintln!(
        "{}",
        delivered_line(
            &asker,
            "the answer",
            &format!("the answer to #{id} is kept, but not delivered to {asker}"),
            &answered.to_asker
        )
    );
    for (to, delivered) in &answered.copies {
        eprintln!(
            "{}",
            delivered_line(
                to,
                "its copy",
                &format!("the copy for {to} was not delivered"),
                delivered
            )
        );
    }
    Ok(())
}

fn escalate(app: &App, id: i64) -> Result<()> {
    let caller = permissions::caller(app)?;
    let escalated = reports::escalate(app, &caller, id)?;
    let id = escalated.row.id;
    let (Some(to), Some(delivered)) = (escalated.to, escalated.delivered) else {
        eprintln!("jeet: #{id} is with the user now; notified");
        return Ok(());
    };
    let to = manager::label(&to);
    eprintln!("jeet: passed #{id} to {to}");
    eprintln!(
        "{}",
        delivered_line(
            &to,
            "the question",
            &format!("#{id} is passed to {to}, but not delivered"),
            &delivered
        )
    );
    Ok(())
}

fn ls(app: &App) -> Result<()> {
    let caller = caller_at_terminal(app, "listing every open question")?;
    let held = reports::held(app, &caller)?;
    if held.is_empty() {
        eprintln!("jeet: no open question reaches you");
        return Ok(());
    }
    let me = match &caller {
        Caller::User => None,
        Caller::Root => Some(manager::root(app)?.id),
        Caller::Node(task) => Some(task.id.clone()),
    };
    for row in &held {
        print!("{}", listed(app, me.as_deref(), row)?);
    }
    Ok(())
}

/// Ask `row` as `jeet report ls` lists it to `me` (None: the user): `#42
/// question task assistant/api/w 12m ago, with you`, then the question,
/// as written for the user, fenced as the asker's claim for an agent.
fn listed(app: &App, me: Option<&str>, row: &TaskReportRecord) -> Result<String> {
    let Some(asker) = app.db.get_task(&row.task_id)? else {
        return Ok(String::new());
    };
    let path = manager::tree_path(app, &asker)?;
    let at = match (row.needs_user, me) {
        (true, None) => ", with you".to_string(),
        (true, Some(_)) => ", with the user".to_string(),
        (false, me) if me == Some(row.to_id.as_str()) => String::new(),
        (false, _) => {
            let holder = app
                .db
                .get_task(&row.to_id)?
                .map_or_else(|| "a deleted task".to_string(), |t| manager::label(&t));
            format!(", held by {holder}")
        }
    };
    let mut out = format!(
        "#{} {} {} {}{at}\n",
        row.id,
        row.kind.as_str(),
        manager::label_at(&asker, &path),
        age_label(row.at)
    );
    match me {
        None => {
            for line in printable_text(&row.body).lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        Some(_) => {
            out.push_str(&assistant::fence_report(
                row.id,
                &manager::label(&asker),
                &path,
                row.kind,
                &row.body,
            ));
            out.push('\n');
        }
    }
    Ok(out)
}

/// How `what` (`the report`, `the answer`) got to `to`, as a line for
/// stderr; `failed` says what was kept when it did not.
fn delivered_line(to: &str, what: &str, failed: &str, d: &Result<Delivered>) -> String {
    format!("jeet: {}", delivered(to, what, failed, d))
}

/// [`delivered_line`] without its `jeet: `, as the agents pane shows it.
pub(crate) fn delivered(to: &str, what: &str, failed: &str, d: &Result<Delivered>) -> String {
    match d {
        Ok(Delivered::Typed) => format!("typed into {to}'s session"),
        Ok(Delivered::Wake) => format!("delivered to {to}'s session"),
        Ok(Delivered::Queued) => {
            format!("{to} is busy, so {what} is queued; it gets it when it finishes or sits idle")
        }
        Ok(Delivered::Started) => format!("started a session of {to}'s with {what}"),
        Ok(Delivered::Held) => {
            format!("{to} is not running, so {what} waits for its next session")
        }
        Ok(Delivered::Paused) => {
            format!("{to} is paused or stopped, so {what} waits until it is resumed")
        }
        Ok(Delivered::StartQueued { live, max }) => {
            format!("{to} waits to start ({live}/{max} live), so {what} waits for its start")
        }
        Err(e) => format!("{failed}: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parsed(args: &[&str]) -> ReportCommands {
        let cli =
            crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(args.iter().copied()))
                .unwrap();
        match cli.command {
            Some(crate::cli::Commands::Report { what }) => what,
            _ => panic!("not a report: {args:?}"),
        }
    }

    #[test]
    fn report_parses_its_kinds_and_the_question_commands() {
        assert_eq!(
            parsed(&["report", "done", "-x"]),
            ReportCommands::Done {
                message: "-x".into()
            }
        );
        assert_eq!(
            parsed(&["report", "answer", "42", "-1"]),
            ReportCommands::Answer {
                id: 42,
                message: "-1".into()
            }
        );
        assert_eq!(
            parsed(&["report", "escalate", "42"]),
            ReportCommands::Escalate { id: 42 }
        );
        assert_eq!(parsed(&["report", "ls"]), ReportCommands::Ls);
        for bad in [
            &["report", "status", "x"][..],
            &["report", "answer", "x", "y"],
            &["report", "escalate"],
        ] {
            assert!(
                crate::cli::Cli::try_parse_from(std::iter::once("jeet").chain(bad.iter().copied()))
                    .is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_report_is_told_as_before_and_an_answer_in_its_own_words() {
        let queued = delivered_line(
            "manager api",
            "the report",
            "report #1 recorded, but not delivered",
            &Ok(Delivered::Queued),
        );
        assert_eq!(
            queued,
            "jeet: manager api is busy, so the report is queued; it gets it when it finishes or \
             sits idle"
        );
        let failed = delivered_line(
            "task w",
            "the answer",
            "the answer to #4 is kept, but not delivered to task w",
            &Err(anyhow::anyhow!("no inbox")),
        );
        assert_eq!(
            failed,
            "jeet: the answer to #4 is kept, but not delivered to task w: no inbox"
        );
    }
}
