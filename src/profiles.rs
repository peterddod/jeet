//! Agent profiles: named definitions a task's agent is started as
//! (`plete task new --agent <name>`), which the user writes and edits.
//!
//! A profile is `<name>.md`, or `<name>/AGENT.md` beside a `skills/` folder
//! of its own: TOML between `+++` lines (what the agent runs with, what the
//! task is granted and may spend), then the instructions its brief gains.
//! The first found wins, in this order: the task's project's `agents/`,
//! `$PLETE_HOME/agents/`, the built-ins. Profiles are never read from a
//! repository: a checkout's text is untrusted, and a profile is trusted
//! instructions and a grant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{PrMode, RoleRules};
use crate::context::App;
use crate::manager::budget;
use crate::permissions;

/// The fence around a profile's settings.
const FENCE: &str = "+++";

/// The file a profile kept as a folder is read from.
const FOLDER_FILE: &str = "AGENT.md";

/// Most bytes a profile's file may hold.
pub const MAX_BYTES: u64 = 64 * 1024;

/// The built-in profiles, by name: what a task is started as when none is
/// named, and what a manager does.
const BUILTINS: &[(&str, &str)] = &[("default", DEFAULT), ("planner", PLANNER)];

const DEFAULT: &str = r#"+++
description = "A task's agent as plete starts one: its brief, and nothing added"
+++
"#;

const PLANNER: &str = include_str!("profiles/planner.md");

/// Where a profile was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Builtin,
    /// `$PLETE_HOME/agents/`.
    User(PathBuf),
    /// A project's `agents/`.
    Project {
        project: String,
        path: PathBuf,
    },
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Builtin => "built-in".into(),
            Source::User(_) => "user".into(),
            Source::Project { project, .. } => format!("project {project}"),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Source::Builtin => None,
            Source::User(path) | Source::Project { path, .. } => Some(path),
        }
    }
}

/// A profile's settings, the TOML between its `+++` lines. Each is unset
/// unless the file sets it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// One line for `plete agents ls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The model its agent runs, over `[models]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What a task made as it may do, unless `--allow` names it; cut to its
    /// parent's as any grant is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// What it may spend unless `--budget` names it: `4h`, `2h,40turns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
    /// How many active tasks it may keep under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
    /// Its PR policy unless `--pr` names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrMode>,
    /// Skills copied into the task: the profile's own `skills/`, the
    /// project's, or plete's built-in ones, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// MCP servers for its agent: a JSON file's path, relative to the
    /// profile, or the servers inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Mcp>,
    /// Environment its agent starts with.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Harness rules added for its agent, as `[permissions.task]`'s.
    #[serde(default, skip_serializing_if = "RoleRules::is_empty")]
    pub permissions: RoleRules,
}

/// `mcp` in a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Mcp {
    /// A file in the harness's own format, relative to the profile.
    File(String),
    /// Servers by name, each as the harness takes it.
    Servers {
        servers: BTreeMap<String, toml::Value>,
    },
}

/// A profile, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub source: Source,
    pub settings: Settings,
    /// The instructions its agent's brief gains; may be empty.
    pub body: String,
}

/// A profile file that did not read, for `plete agents ls` to show.
#[derive(Debug)]
pub struct Broken {
    pub name: String,
    pub path: PathBuf,
    pub error: anyhow::Error,
}

/// Refuse a name that is no profile's: one plete can use as a file name
/// and a command-line word.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !name.starts_with(['-', '_']);
    if !ok {
        bail!(
            "{name:?} is no profile name: use lowercase letters, digits, `-` and `_`, \
             at most 64, starting with a letter or digit"
        );
    }
    Ok(())
}

/// Split `text` into its settings and its body. A file with no `+++` line
/// first is all body, with nothing set.
pub fn parse(text: &str) -> Result<(Settings, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = strip_fence_line(text) else {
        return Ok((Settings::default(), text.trim().to_string()));
    };
    let (front, body) = match find_fence(rest) {
        Some((front, body)) => (front, body),
        None => bail!("its settings start with `{FENCE}` but no `{FENCE}` line ends them"),
    };
    let settings: Settings = toml::from_str(front).context("its settings")?;
    check(&settings)?;
    Ok((settings, body.trim().to_string()))
}

/// `text` after a first line that is the fence alone.
fn strip_fence_line(text: &str) -> Option<&str> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    (first.trim_end() == FENCE).then_some(rest)
}

/// The settings and what follows the fence line that ends them.
fn find_fence(rest: &str) -> Option<(&str, &str)> {
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == FENCE {
            return Some((&rest[..at], &rest[at + line.len()..]));
        }
        at += line.len();
    }
    None
}

/// Refuse settings plete cannot honour, so a mistake shows when the file is
/// read rather than when a task starts.
fn check(s: &Settings) -> Result<()> {
    if let Some(names) = &s.allow {
        for name in names {
            if permissions::capability(name.trim()).is_none() {
                bail!("allow: {name} is not a capability");
            }
        }
    }
    if let Some(b) = &s.budget {
        budget::parse_limits(b).map_err(|e| anyhow::anyhow!("budget: {e}"))?;
    }
    if s.max_children == Some(0) {
        bail!("max_children = 0 allows nothing; give at least 1");
    }
    for skill in &s.skills {
        if skill.is_empty() || skill.contains(['/', '\\']) || skill.starts_with('.') {
            bail!("skills: {skill:?} is no skill's name");
        }
    }
    for key in s.env.keys() {
        let valid = !key.is_empty()
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!("env: {key:?} is no variable's name");
        }
        if key.starts_with("PLETE_") || key.starts_with("JEET_") {
            bail!("env: {key} is plete's own; a profile may not set it");
        }
    }
    if let Some(Mcp::File(path)) = &s.mcp {
        if path.trim().is_empty() {
            bail!("mcp: give a file's path, or [mcp.servers.<name>] tables");
        }
    }
    Ok(())
}

/// The folder user profiles are kept in, in each home.
pub fn user_dirs(app: &App) -> Vec<PathBuf> {
    app.roots(crate::config::agents_root)
}

/// The folder project `project`'s profiles are kept in.
pub fn project_dir(app: &App, project: &str) -> PathBuf {
    app.item_dir(crate::config::projects_root, project)
        .join("agents")
}

/// Where a new user (or, with `project`, project) profile goes.
pub fn new_path(app: &App, name: &str, project: Option<&str>) -> PathBuf {
    let dir = match project {
        Some(project) => project_dir(app, project),
        None => app.new_root(crate::config::agents_root),
    };
    dir.join(format!("{name}.md"))
}

/// The file of profile `name` in `dir`, as a file or a folder.
fn in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let file = dir.join(format!("{name}.md"));
    if file.is_file() {
        return Some(file);
    }
    let folder = dir.join(name).join(FOLDER_FILE);
    folder.is_file().then_some(folder)
}

/// Read the profile at `path`.
fn read(name: &str, path: &Path, source: Source) -> Result<Profile> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("read {}", path.display()))?
        .len();
    if len > MAX_BYTES {
        bail!(
            "{} is {len} bytes; a profile holds at most {MAX_BYTES}",
            path.display()
        );
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (settings, body) = parse(&text).with_context(|| format!("profile {name}"))?;
    Ok(Profile {
        name: name.to_string(),
        source,
        settings,
        body,
    })
}

/// The built-in profile `name`.
fn builtin(name: &str) -> Option<Profile> {
    let (_, text) = BUILTINS.iter().find(|(n, _)| *n == name)?;
    let (settings, body) = parse(text).expect("a built-in profile reads");
    Some(Profile {
        name: name.to_string(),
        source: Source::Builtin,
        settings,
        body,
    })
}

/// Profile `name` as a task in `project` sees it: the first found of the
/// project's, the user's and the built-ins. A file that does not read is an
/// error, not a fall through to the next.
pub fn find(app: &App, name: &str, project: Option<&str>) -> Result<Profile> {
    check_name(name)?;
    if let Some(project) = project {
        if let Some(path) = in_dir(&project_dir(app, project), name) {
            let source = Source::Project {
                project: project.to_string(),
                path: path.clone(),
            };
            return read(name, &path, source);
        }
    }
    for dir in user_dirs(app) {
        if let Some(path) = in_dir(&dir, name) {
            return read(name, &path, Source::User(path.clone()));
        }
    }
    match builtin(name) {
        Some(profile) => Ok(profile),
        None => bail!("no profile named {name}; `plete agents ls` lists them"),
    }
}

/// Every profile a task in `project` may be started as, by name, the one
/// that wins for each; and the files that did not read.
pub fn list(app: &App, project: Option<&str>) -> (Vec<Profile>, Vec<Broken>) {
    let mut names: Vec<(String, PathBuf, Source)> = Vec::new();
    let mut dirs: Vec<(PathBuf, Option<&str>)> = Vec::new();
    if let Some(project) = project {
        dirs.push((project_dir(app, project), Some(project)));
    }
    dirs.extend(user_dirs(app).into_iter().map(|d| (d, None)));
    for (dir, project) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let file = e.file_name().to_string_lossy().to_string();
                match file.strip_suffix(".md") {
                    Some(name) if path.is_file() => Some((name.to_string(), path)),
                    _ if path.join(FOLDER_FILE).is_file() => Some((file, path.join(FOLDER_FILE))),
                    _ => None,
                }
            })
            .collect();
        found.sort();
        for (name, path) in found {
            if names.iter().any(|(n, _, _)| *n == name) {
                continue;
            }
            let source = match project {
                Some(project) => Source::Project {
                    project: project.to_string(),
                    path: path.clone(),
                },
                None => Source::User(path.clone()),
            };
            names.push((name, path, source));
        }
    }
    let (mut profiles, mut broken) = (Vec::new(), Vec::new());
    for (name, path, source) in names {
        match check_name(&name).and_then(|_| read(&name, &path, source)) {
            Ok(profile) => profiles.push(profile),
            Err(error) => broken.push(Broken { name, path, error }),
        }
    }
    for (name, _) in BUILTINS {
        let shadowed =
            profiles.iter().any(|p| p.name == *name) || broken.iter().any(|b| b.name == *name);
        if !shadowed {
            profiles.extend(builtin(name));
        }
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    (profiles, broken)
}

/// Whether a built-in profile of `name` exists, which a user's file of the
/// same name shadows.
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|(n, _)| *n == name)
}

/// The text a new profile starts as: `from`'s whole file, or a starter.
pub fn starter(name: &str, from: Option<&Profile>) -> Result<String> {
    if let Some(from) = from {
        return match from.source.path() {
            Some(path) => {
                std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
            }
            None => Ok(BUILTINS
                .iter()
                .find(|(n, _)| *n == from.name)
                .map(|(_, text)| text.to_string())
                .unwrap_or_default()),
        };
    }
    Ok(format!(
        r#"+++
description = "What a task started as {name} is for"
# model = "sonnet"                 # the model its agent runs, over [models]
# allow = ["task.create", "worktree.add"]   # its grant unless --allow names one
# budget = "2h,40turns"            # what it may spend unless --budget names it
# max_children = 4                 # active tasks it may keep under it
# pr = "draft"                     # auto, draft or ask
# skills = ["my-skill"]            # from this profile's skills/, the project's, or built-in
# mcp = "{name}.mcp.json"          # MCP servers, a file beside this one
# env = {{ RUST_LOG = "info" }}
# [permissions]                    # harness rules, as [permissions.task]
# allow = ["Bash(cargo test:*)"]
+++
Instructions for an agent started as {name}: they follow its brief each
time a session starts.
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_is_its_settings_then_its_body() {
        let (s, body) = parse(
            "+++\ndescription = \"plans\"\nmodel = \"opus\"\nallow = [\"task.create\"]\n\
             budget = \"2h,40turns\"\nmax_children = 3\npr = \"ask\"\nskills = [\"a\"]\n\
             env = { RUST_LOG = \"info\" }\nmcp = \"x.json\"\n[permissions]\n\
             allow = [\"Bash(make:*)\"]\n+++\n\nPlan first.\n",
        )
        .unwrap();
        assert_eq!(s.description.as_deref(), Some("plans"));
        assert_eq!(s.model.as_deref(), Some("opus"));
        assert_eq!(s.max_children, Some(3));
        assert_eq!(s.pr, Some(PrMode::Ask));
        assert_eq!(s.env["RUST_LOG"], "info");
        assert_eq!(s.mcp, Some(Mcp::File("x.json".into())));
        assert_eq!(s.permissions.allow, ["Bash(make:*)"]);
        assert_eq!(body, "Plan first.");

        // Inline servers.
        let (s, _) = parse("+++\n[mcp.servers.docs]\ncommand = \"docs-mcp\"\n+++\n").unwrap();
        match s.mcp {
            Some(Mcp::Servers { servers }) => assert!(servers.contains_key("docs")),
            other => panic!("{other:?}"),
        }
        // No settings: all body.
        let (s, body) = parse("Just do it.\n").unwrap();
        assert_eq!((s, body.as_str()), (Settings::default(), "Just do it."));
    }

    #[test]
    fn a_profile_plete_cannot_honour_is_refused_as_it_is_read() {
        let refused = |text: &str, says: &str| {
            let err = format!("{:#}", parse(text).unwrap_err());
            assert!(err.contains(says), "{text}: {err}");
        };
        refused("+++\nmodel = \"x\"\n", "no `+++` line ends them");
        refused("+++\nmodle = \"x\"\n+++\n", "unknown field");
        refused(
            "+++\nallow = [\"task.fly\"]\n+++\n",
            "task.fly is not a capability",
        );
        refused("+++\nbudget = \"$5\"\n+++\n", "budget");
        refused("+++\nmax_children = 0\n+++\n", "allows nothing");
        refused("+++\nskills = [\"../x\"]\n+++\n", "no skill's name");
        refused("+++\nenv = { PLETE_HOME = \"/x\" }\n+++\n", "plete's own");
        refused(
            "+++\nenv = { \"A B\" = \"x\" }\n+++\n",
            "no variable's name",
        );
        // An old capability name is still one.
        parse("+++\nallow = [\"manager.create\"]\n+++\n").unwrap();
    }

    #[test]
    fn names_are_file_and_word_safe() {
        for ok in ["planner", "a", "doc-writer", "x_2"] {
            check_name(ok).unwrap();
        }
        for bad in ["", "Planner", "../x", "a b", "-x", "a/b", "a.md"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_built_ins_read() {
        for (name, _) in BUILTINS {
            let p = builtin(name).unwrap();
            assert!(p.settings.description.is_some(), "{name}");
        }
        let planner = builtin("planner").unwrap();
        assert_eq!(planner.settings.skills, ["plete-planner"]);
        assert!(!planner.body.is_empty());
        // The skill it names, which plete installs with it.
        let skill = include_str!("profiles/planner-SKILL.md");
        assert!(skill.starts_with("---\nname: plete-planner\n"));
    }

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let app = App {
            config: crate::config::Config::default(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            new_home: None,
            home,
        };
        (dir, app)
    }

    /// A project's profile shadows the user's, which shadows a built-in; a
    /// folder works as a file does; a broken file is listed, not skipped,
    /// and found as an error.
    #[test]
    fn profiles_are_found_project_then_user_then_built_in() {
        let (_dir, app) = app();
        let user = app.home.join("agents");
        std::fs::create_dir_all(user.join("docs")).unwrap();
        std::fs::write(
            user.join("planner.md"),
            "+++\ndescription = \"mine\"\n+++\nMine.",
        )
        .unwrap();
        std::fs::write(user.join("docs/AGENT.md"), "Write docs.").unwrap();
        std::fs::write(user.join("bad.md"), "+++\nnope = 1\n+++\n").unwrap();
        let proj = project_dir(&app, "web");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("docs.md"), "Web docs.").unwrap();

        let planner = find(&app, "planner", None).unwrap();
        assert_eq!(planner.body, "Mine.");
        assert!(matches!(planner.source, Source::User(_)));
        assert_eq!(find(&app, "docs", None).unwrap().body, "Write docs.");
        let docs = find(&app, "docs", Some("web")).unwrap();
        assert_eq!(docs.body, "Web docs.");
        assert_eq!(docs.source.label(), "project web");
        assert_eq!(find(&app, "default", None).unwrap().source, Source::Builtin);
        assert!(format!("{:#}", find(&app, "bad", None).unwrap_err()).contains("unknown field"));
        assert!(find(&app, "none", None)
            .unwrap_err()
            .to_string()
            .contains("no profile named none"));

        let (profiles, broken) = list(&app, Some("web"));
        let names: Vec<(&str, String)> = profiles
            .iter()
            .map(|p| (p.name.as_str(), p.source.label()))
            .collect();
        assert_eq!(
            names,
            [
                ("default", "built-in".to_string()),
                ("docs", "project web".to_string()),
                ("planner", "user".to_string()),
            ]
        );
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].name, "bad");
    }

    #[test]
    fn a_new_profile_starts_from_another_or_a_starter_that_reads() {
        let (s, _) = parse(&starter("docs", None).unwrap()).unwrap();
        assert!(s.description.unwrap().contains("docs"));
        let planner = builtin("planner").unwrap();
        assert_eq!(starter("p2", Some(&planner)).unwrap(), PLANNER);
    }
}
