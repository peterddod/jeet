//! `jeet exec <repo>`: an interactive subshell in a repository's trunk.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

use crate::commands::path;
use crate::context::App;

pub fn run(app: &App, filter: &str) -> Result<()> {
    if !io::stdout().is_terminal() && std::env::var("JEET_EXEC_INIT").is_err() {
        bail!("jeet exec requires an interactive terminal; use `jeet path` for scripting");
    }
    let target = path::resolve_target(app, filter)?;
    let exit_code = exec_shell(&target)?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn exec_shell(target: &Path) -> Result<i32> {
    let shell = std::env::var("JEET_SHELL")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/sh".to_string());

    eprintln!("jeet: starting subshell in {} ({shell})", target.display());
    eprintln!("jeet: type 'exit' to return to your previous directory");
    eprintln!("jeet: for native cd, run: eval \"$(jeet init-shell)\"");

    let mut cmd = Command::new(&shell);
    cmd.current_dir(target);

    if let Ok(init) = std::env::var("JEET_EXEC_INIT") {
        cmd.arg("-c").arg(init);
    }

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to exec shell {shell}: {e}"))?;

    Ok(status.code().unwrap_or(1))
}
