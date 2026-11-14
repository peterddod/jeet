//! `plete agents …`: agent profiles from the command line.
//!
//! `new` prints the profile's path, and only that, on stdout; everything
//! else plete has to say goes to stderr.

use anyhow::{bail, Context, Result};

use crate::agent;
use crate::context::App;
use crate::profiles::{self, Profile, Source};
use crate::projects;

/// A project named on the command line, which must exist.
fn checked_project<'a>(app: &App, project: Option<&'a str>) -> Result<Option<&'a str>> {
    if let Some(name) = project {
        projects::load(app, name)?;
    }
    Ok(project)
}

pub fn ls(app: &App, project: Option<&str>, json: bool) -> Result<()> {
    let project = checked_project(app, project)?;
    let (profiles, broken) = profiles::list(app, project);
    if json {
        let mut rows: Vec<serde_json::Value> = profiles
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "source": p.source.label(),
                    "path": p.source.path(),
                    "description": p.settings.description,
                    "shadows_builtin": shadows(p),
                })
            })
            .collect();
        rows.extend(broken.iter().map(|b| {
            serde_json::json!({
                "name": b.name,
                "path": b.path,
                "error": format!("{:#}", b.error),
            })
        }));
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("{:<20} {:<16} DESCRIPTION", "PROFILE", "FROM");
    for p in &profiles {
        let from = match shadows(p) {
            true => format!("{} (over built-in)", p.source.label()),
            false => p.source.label(),
        };
        println!(
            "{:<20} {:<16} {}",
            p.name,
            from,
            p.settings.description.as_deref().unwrap_or("-")
        );
    }
    for b in &broken {
        println!("{:<20} {:<16} (broken: {:#})", b.name, "-", b.error);
    }
    Ok(())
}

/// Whether `p` is a file of the user's or a project's over a built-in.
fn shadows(p: &Profile) -> bool {
    p.source != Source::Builtin && profiles::is_builtin(&p.name)
}

pub fn show(app: &App, name: &str, project: Option<&str>) -> Result<()> {
    let project = checked_project(app, project)?;
    let profile = profiles::find(app, name, project)?;
    let text = match profile.source.path() {
        Some(path) => {
            eprintln!(
                "plete: profile {name}: {}, {}",
                profile.source.label(),
                path.display()
            );
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
        }
        None => {
            eprintln!(
                "plete: profile {name}: built-in; `plete agents new {name} --from {name}` \
                 makes a copy of your own to edit"
            );
            profiles::starter(name, Some(&profile))?
        }
    };
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub fn new(app: &App, name: &str, from: Option<&str>, project: Option<&str>) -> Result<()> {
    profiles::check_name(name)?;
    let project = checked_project(app, project)?;
    let path = profiles::new_path(app, name, project);
    let folder = path.with_extension("");
    if path.exists() || folder.exists() {
        bail!(
            "profile {name} exists already at {}; `plete agents edit {name}` opens it",
            path.display()
        );
    }
    let from = from
        .map(|from| profiles::find(app, from, project))
        .transpose()?;
    let text = profiles::starter(name, from.as_ref())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    if profiles::is_builtin(name) {
        eprintln!("plete: profile {name} now shadows the built-in one");
    }
    println!("{}", path.display());
    Ok(())
}

pub fn edit(app: &App, name: &str, project: Option<&str>) -> Result<()> {
    let project = checked_project(app, project)?;
    let profile = profiles::find(app, name, project).or_else(|e| {
        // A file that does not read is still the one to edit.
        let path = profiles::new_path(app, name, project);
        match path.is_file() {
            true => Ok(Profile {
                name: name.to_string(),
                source: Source::User(path),
                settings: Default::default(),
                body: String::new(),
            }),
            false => Err(e),
        }
    })?;
    let Some(path) = profile.source.path() else {
        bail!(
            "profile {name} is built in; `plete agents new {name} --from {name}` makes a copy \
             of your own to edit"
        );
    };
    let argv = agent::editor_argv(&app.config)?;
    let dir = path.parent().unwrap_or(path);
    let code = agent::run_in(&argv, dir, &[path.display().to_string()])?;
    if code != 0 {
        bail!("the editor exited with status {code}");
    }
    if let Err(e) = profiles::find(app, name, project) {
        eprintln!("plete: warning: profile {name} does not read now: {e:#}");
    }
    Ok(())
}
