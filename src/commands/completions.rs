use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{CommandFactory, ValueEnum};
use clap_complete::{generate, Shell};

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

pub fn generate_script(shell: CompletionShell) -> Result<String> {
    let mut cmd = Cli::command();
    let generator = match shell {
        CompletionShell::Bash => Shell::Bash,
        CompletionShell::Elvish => Shell::Elvish,
        CompletionShell::Fish => Shell::Fish,
        CompletionShell::PowerShell => Shell::PowerShell,
        CompletionShell::Zsh => Shell::Zsh,
    };
    let mut buf = Vec::new();
    generate(generator, &mut cmd, "jeet", &mut buf);
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
