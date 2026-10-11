//! `jeet report`: a task with a parent tells it how it is going.

use anyhow::{bail, Result};

use crate::context::App;
use crate::db::tasks::ReportKind;
use crate::messages::Delivered;
use crate::permissions::{self, Caller};
use crate::reports;
use crate::tasks::TASK_ID_ENV;

pub fn run(app: &App, kind: ReportKind, message: &str) -> Result<()> {
    let task = match permissions::caller(app)? {
        Caller::Node(task) => task,
        Caller::Root => bail!("the assistant has no parent; `jeet notify` tells the user"),
        Caller::User => match std::env::var(TASK_ID_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            Some(id) => match app.db.get_task(id.trim())? {
                Some(_) => bail!(
                    "this task has no parent; `jeet report` is for tasks made under the assistant"
                ),
                None => bail!(
                    "${TASK_ID_ENV} is {}, which is no task jeet knows; was it deleted?",
                    id.trim()
                ),
            },
            None => bail!("`jeet report` runs inside a task's agent (${TASK_ID_ENV} is not set)"),
        },
    };
    // Progress is not worth a session of the assistant's own: `send` says
    // which kinds are.
    let (id, delivered) = reports::send(app, &task, kind, message, true)?;
    eprintln!("jeet: reported {} to the assistant (#{id})", kind.as_str());
    match delivered {
        Ok(Delivered::Typed) => eprintln!("jeet: typed into the assistant's session"),
        Ok(Delivered::Queued) => eprintln!(
            "jeet: the assistant is busy, so the report is queued; it gets it when it finishes \
             or sits idle"
        ),
        Ok(Delivered::Started) => {
            eprintln!("jeet: started a session of the assistant's with the report")
        }
        Ok(Delivered::Held) => eprintln!(
            "jeet: the assistant is not running, so the report waits for its next session"
        ),
        Err(e) => eprintln!("jeet: report #{id} recorded, but not delivered: {e:#}"),
    }
    Ok(())
}
