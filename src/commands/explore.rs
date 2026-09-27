//! `jeet` with no arguments: the interactive file explorer.

use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cd;
use crate::context::App;
use crate::resolve;
use crate::tui;
use crate::tui::state::Exit;

pub fn run(app: &App) -> Result<()> {
    let cwd = std::env::current_dir().context("get cwd")?;
    run_at(app, &cwd)
}

/// Open the explorer at `dir`, which need not be where the shell is.
///
/// Quitting in place leaves the shell in `dir` rather than where it started:
/// whoever sent us to `dir` meant the user to end up there.
pub fn run_at(app: &App, dir: &Path) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("the jeet explorer needs an interactive terminal; try `jeet ls` or `jeet path`");
    }

    let ctx = resolve::resolve_context(app, dir).map_err(|e| {
        anyhow::anyhow!("{e}\nrun `jeet cd <repo>` first, or `jeet ls` to see indexed repositories")
    })?;

    let start = tui::start_dir(&ctx, dir);
    let shell_cwd = std::env::current_dir().context("get cwd")?;
    let landing = match tui::run(app, &ctx, &start)? {
        Exit::ChangeDir(path) => Some(path),
        Exit::Stay if !resolve::same_path(dir, &shell_cwd) => Some(dir.to_path_buf()),
        Exit::Stay => None,
    };
    if let Some(path) = landing {
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
    Ok(())
}
