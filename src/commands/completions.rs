use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{CommandFactory, ValueEnum};
use clap_complete::env::{Bash, Elvish, EnvCompleter, Fish, Powershell, Zsh};

use crate::cli::Cli;
use crate::completion_candidates;

#[derive(Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
}

/// Dynamic shell registration (`COMPLETE=$shell jeet`) with index-backed candidates.
pub fn generate_script(shell: CompletionShell) -> Result<String> {
    let mut cmd = Cli::command();
    cmd.build();
    let name = cmd.get_name();
    let bin = cmd.get_bin_name().unwrap_or(name);

    let mut buf = Vec::new();
    match shell {
        CompletionShell::Bash => {
            Bash.write_registration("COMPLETE", name, bin, bin, &mut buf)?;
        }
        CompletionShell::Elvish => {
            Elvish.write_registration("COMPLETE", name, bin, bin, &mut buf)?;
        }
        CompletionShell::Fish => {
            Fish.write_registration("COMPLETE", name, bin, bin, &mut buf)?;
        }
        CompletionShell::PowerShell => {
            Powershell.write_registration("COMPLETE", name, bin, bin, &mut buf)?;
        }
        CompletionShell::Zsh => {
            Zsh.write_registration("COMPLETE", name, bin, bin, &mut buf)?;
        }
    }

    String::from_utf8(buf).context("completion script was not valid utf-8")
}

pub fn run(shell: CompletionShell) -> Result<()> {
    io::stdout().write_all(generate_script(shell)?.as_bytes())?;
    Ok(())
}

pub fn run_complete(what: &str, filter: Option<&str>) -> Result<()> {
    let lines = match what {
        "repos" => completion_candidates::repo_filters(),
        "branches" => completion_candidates::branch_names(filter),
        other => anyhow::bail!("unknown complete target {other:?} (expected repos or branches)"),
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}
