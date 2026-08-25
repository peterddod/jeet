//! `jeet` with no arguments: the interactive file explorer.

use std::io::{self, IsTerminal};

use anyhow::{bail, Context, Result};

use crate::cd;
use crate::context::App;
use crate::resolve;
use crate::tui;
use crate::tui::state::Exit;

pub fn run(app: &App) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the jeet explorer needs an interactive terminal; try `jeet ls` or `jeet path`");
    }

    let cwd = std::env::current_dir().context("get cwd")?;
    let ctx = resolve::resolve_context(app, &cwd).map_err(|e| {
        anyhow::anyhow!("{e}\nrun `jeet cd <repo>` first, or `jeet ls` to see indexed repositories")
    })?;

    let start = tui::start_dir(&ctx, &cwd);
    match tui::run(app, &ctx, &start)? {
        Exit::Stay => {}
        Exit::ChangeDir(path) => {
            cd::request(&path)?;
            if cd::wrapper_active() {
                eprintln!("jeet: {}", path.display());
            } else {
                println!("{}", path.display());
                eprintln!(
                    "jeet: add `eval \"$(jeet init-shell)\"` to your shell rc to cd here automatically"
                );
            }
        }
    }
    Ok(())
}
