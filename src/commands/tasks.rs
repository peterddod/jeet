//! `jeet tasks`, and `jeet` with no arguments: the tasks list.

use std::io::{self, IsTerminal};

use anyhow::{bail, Result};

use crate::commands::explore;
use crate::context::App;
use crate::link::Link;
use crate::store;
use crate::tui::state::Exit;
use crate::tui::tasks::{self, Start};
use crate::watch;

pub fn run(app: &App) -> Result<()> {
    open(app, Start::List)
}

/// `jeet projects` on a terminal: the tasks list on the projects tab.
pub fn run_projects(app: &App) -> Result<()> {
    open(app, Start::Projects)
}

/// `jeet open` on a terminal with no screen open: the tasks list, at `link`.
pub fn run_link(app: &App, link: Link) -> Result<()> {
    open(app, Start::Link(link))
}

fn open(app: &App, start: Start) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!(
            "the jeet tasks list needs an interactive terminal; try `jeet task ls` or \
             `jeet task new <name>`"
        );
    }
    // Every launch is a chance to bring the store up to date, in the
    // background; before the screen goes up, as it may say something.
    store::maybe_sync_background(app);
    // And GitHub to be polled, when `[watch]` asks for it.
    watch::maybe_background(&app.home, &app.config);
    match tasks::run(app, start)? {
        Exit::ChangeDir(path) => explore::hand_off(&path),
        Exit::Stay | Exit::Back => Ok(()),
    }
}
