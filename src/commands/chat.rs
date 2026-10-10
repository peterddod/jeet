//! `jeet chat`: talk to the assistant. Its home is set up the first time;
//! after that it resumes the most recently active session, starts a new one
//! with `--new`, or offers them all with `--pick`.
//!
//! On a terminal it attaches to the session's tmux window. Without one the
//! session is left running detached and the assistant's home is printed,
//! the way `jeet task open` does.

use std::io::IsTerminal;

use anyhow::Result;

use crate::agent::age_label;
use crate::assistant::{self, Open};
use crate::commands::task::printable;
use crate::context::App;
use crate::tui::chat::{self as picker, Pick};

pub fn run(app: &App, new: Option<Option<String>>, pick: bool) -> Result<()> {
    let task = assistant::ensure(app)?;
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let how = match (pick, new) {
        (true, _) if !terminal => return list(app, &task),
        (true, _) => match picker::run(app, &task)? {
            Pick::Session(id) => Choice::Session(id),
            Pick::New => Choice::New(None),
            Pick::Quit => return Ok(()),
        },
        (false, Some(name)) => Choice::New(name),
        (false, None) => Choice::Latest,
    };
    let opened = match &how {
        Choice::Latest => assistant::open(app, &task, Open::Latest)?,
        Choice::New(name) => assistant::open(
            app,
            &task,
            Open::New {
                name: name.as_deref(),
                prompt: None,
            },
        )?,
        Choice::Session(id) => assistant::open(app, &task, Open::Session(id))?,
    };
    if opened.started {
        eprintln!(
            "jeet: started the assistant's session {} in tmux session {}",
            printable(&opened.name),
            assistant::TMUX_SESSION
        );
    }
    let dir = assistant::home_dir(&app.home);
    if !terminal {
        eprintln!(
            "jeet: `tmux attach -t {}` to talk to the assistant",
            assistant::TMUX_SESSION
        );
        println!("{}", dir.display());
        return Ok(());
    }
    let code = assistant::attach(&opened.window)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

enum Choice {
    Latest,
    New(Option<String>),
    Session(String),
}

/// The sessions, for `--pick` without a terminal to pick on.
fn list(app: &App, task: &crate::db::tasks::TaskRecord) -> Result<()> {
    let sessions = assistant::sessions(app, task)?;
    if sessions.is_empty() {
        eprintln!("jeet: the assistant has no sessions yet; `jeet chat` starts one");
        return Ok(());
    }
    println!(
        "  {:<36}  {:<10}  {:<24}  NAME",
        "SESSION", "ACTIVE", "STATUS"
    );
    for s in sessions {
        println!(
            "{} {:<36}  {:<10}  {:<24}  {}",
            if s.primary { '*' } else { ' ' },
            s.id,
            age_label(s.last),
            s.activity.label(),
            printable(&s.name)
        );
    }
    Ok(())
}
