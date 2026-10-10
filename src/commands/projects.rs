//! `jeet projects …`: project templates from the command line.
//!
//! `new` prints the project's directory, and only that, on stdout; everything
//! else jeet has to say goes to stderr.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Result};

use crate::agent;
use crate::context::App;
use crate::projects;

pub fn ls(app: &App) -> Result<()> {
    let names = projects::list(app)?;
    if names.is_empty() {
        eprintln!("jeet: no projects; `jeet projects new <name>` makes one");
        return Ok(());
    }
    println!("{:<20} {:<6} {:<20} REPOS", "PROJECT", "TASKS", "FILES");
    for name in names {
        let tasks = projects::tasks_using(app, &name)?.len();
        match projects::load(app, &name) {
            Ok(project) => {
                let repos = match project.config.repos.as_slice() {
                    [] => "-".to_string(),
                    repos => repos.join(", "),
                };
                println!(
                    "{:<20} {:<6} {:<20} {}",
                    name,
                    tasks,
                    project.contents_summary(),
                    repos
                );
            }
            Err(e) => println!("{name:<20} {tasks:<6} (broken: {e:#})"),
        }
    }
    Ok(())
}

pub fn new(app: &App, name: &str, repos: &[String]) -> Result<()> {
    let dir = projects::create(app, name, repos)?;
    eprintln!("jeet: project {name} -> {}", dir.display());
    eprintln!(
        "jeet: put agent instructions in AGENTS.md and skills in skills/; \
         `jeet projects edit {name}` sets its repositories"
    );
    println!("{}", dir.display());
    Ok(())
}

/// Open the project's `project.toml` in the editor, then check that it still
/// parses, so a mistake shows up now rather than at the next `task new`.
pub fn edit(app: &App, name: &str) -> Result<()> {
    let path = projects::config_path(app, name)?;
    let dir = projects::existing_dir(app, name)?;
    let argv = agent::editor_argv(&app.config)?;
    let code = agent::run_in(&argv, &dir, &[path.display().to_string()])?;
    if code != 0 {
        bail!("the editor exited with status {code}");
    }
    match projects::load(app, name) {
        Ok(project) => {
            eprintln!(
                "jeet: project {name}: {} repositor{}",
                project.config.repos.len(),
                if project.config.repos.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            Ok(())
        }
        Err(e) => bail!("{e:#}\n`jeet projects edit {name}` to fix it"),
    }
}

/// Delete a project once the user agrees — or passed --force. Tasks made from
/// it keep their copies of its files.
pub fn rm(app: &App, name: &str, force: bool) -> Result<()> {
    let dir = projects::existing_dir(app, name)?;
    let users = projects::tasks_using(app, name)?;
    if !force {
        let mut report = format!("deleting project {name} removes {}", dir.display());
        if !users.is_empty() {
            report.push_str(&format!(
                "; {} made from it keep their files but can no longer resync",
                tasks_phrase(users.len())
            ));
        }
        if !std::io::stdin().is_terminal() {
            bail!("{report}\npass --force to delete it without a terminal");
        }
        eprintln!("jeet: {report}");
        eprint!("delete it? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            bail!("kept project {name}");
        }
    }
    projects::remove(app, name)?;
    eprintln!("jeet: deleted project {name}");
    Ok(())
}

fn tasks_phrase(n: usize) -> String {
    match n {
        1 => "1 task".to_string(),
        n => format!("{n} tasks"),
    }
}
