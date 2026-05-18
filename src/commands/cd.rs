use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::Command;

use anyhow::Result;

use crate::commands::path;
use crate::context::App;

pub fn run(app: &App, filter: &str, branch: Option<&str>, print: bool) -> Result<()> {
    let target = path::resolve_target(app, filter, branch)?;

    if print {
        let escaped = target.to_string_lossy().replace('\'', "'\\''");
        println!("cd '{escaped}'");
        return Ok(());
    }

    if io::stdout().is_terminal() {
        exec_shell(&target)?;
    } else {
        let escaped = target.to_string_lossy().replace('\'', "'\\''");
        println!("cd '{escaped}'");
    }
    Ok(())
}

fn exec_shell(target: &std::path::Path) -> Result<()> {
    let shell = env::var("JEET_SHELL")
        .or_else(|_| env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/sh".to_string());

    eprintln!("jeet: starting subshell in {} ({shell})", target.display());
    eprintln!("jeet: type 'exit' to return to your previous directory");
    eprintln!(
        "jeet: for native cd, run: eval \"$(jeet init-shell)\" and use jeet cd via the wrapper"
    );

    let err = Command::new(&shell)
        .current_dir(target)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to exec shell {shell}: {e}"))?;

    if !err.success() {
        std::process::exit(err.code().unwrap_or(1));
    }
    Ok(())
}

pub fn run_init_shell() -> Result<()> {
    let snippet = r#"# jeet shell integration — add to ~/.zshrc or ~/.bashrc
jeet() {
  if [[ "$1" == "cd" ]]; then
    shift
    builtin cd -- "$(command jeet path "$@")"
  else
    command jeet "$@"
  fi
}
"#;
    io::stdout().write_all(snippet.as_bytes())?;
    Ok(())
}
