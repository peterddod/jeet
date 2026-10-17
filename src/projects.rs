//! Projects: templates a task can start from.
//!
//! A project is a directory under `$JEET_HOME/projects/<name>`:
//! - `project.toml`: the repositories every task made from it starts with a
//!   worktree of, and extra arguments for the coding agent in those tasks.
//! - `AGENTS.md`: instructions for the agent, and a `CLAUDE.md` importing
//!   them for Claude Code (see [`crate::instructions`]).
//! - `skills/`: Claude Code skills, one directory each.
//!
//! A task gets a *copy* of the project's instruction files and skills — a
//! snapshot, so editing the project leaves tasks already started alone. What
//! was copied is recorded in the task's `.jeet/project-files`, with a hash of
//! each file, so a later [`resync`] can tell a file the task changed (kept)
//! from one it did not (brought up to date).
//!
//! A project from before AGENTS.md has only a `CLAUDE.md`; copying from it
//! first moves that to `AGENTS.md` ([`instructions::migrate`]). Should that
//! fail, its `CLAUDE.md` is copied as it is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::instructions::{self, AGENTS_MD, CLAUDE_MD};
use crate::tasks;

pub const PROJECT_FILE: &str = "project.toml";
const SKILLS_DIR: &str = "skills";

/// Where a project's skills land in a task.
const TASK_SKILLS: &str = ".claude/skills";
/// Inside the task directory: what [`copy_files`] last copied, and its hash.
const MANIFEST: &str = ".jeet/project-files";
/// The built-in skill every task gets; a project cannot replace it.
const RESERVED_SKILL: &str = "jeet";

/// Where a manager keeps the project's AGENTS.md: its own AGENTS.md is the
/// manager role's, so the project's sits beside it, imported from there.
pub const MANAGER_INSTRUCTIONS: &str = "PROJECT.md";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Repository specs, as `jeet worktree add` takes them: `api`,
    /// `owner/repo`, a store id, or a clone URL.
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub agent: AgentConfig,
    /// `[tasks] pr`, over the global one for the project's tasks.
    #[serde(default)]
    pub tasks: crate::config::TasksConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Appended to the agent's command line in the project's tasks.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub dir: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    /// The file holding the agent's instructions: AGENTS.md, or a CLAUDE.md
    /// not yet moved to it.
    pub fn instructions_file(&self) -> Option<&'static str> {
        instructions::files()
            .into_iter()
            .find(|name| self.dir.join(name).is_file())
    }

    /// Names of the project's skills: the directories under `skills/`.
    pub fn skills(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.dir.join(SKILLS_DIR)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    /// `AGENTS.md, 2 skills` — what a task made from it gets copied.
    pub fn contents_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(name) = self.instructions_file() {
            parts.push(name.to_string());
        }
        match self.skills().len() {
            0 => {}
            1 => parts.push("1 skill".to_string()),
            n => parts.push(format!("{n} skills")),
        }
        if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Read a `project.toml`. Unknown keys are refused, so a typo such as
/// `repo = […]` is caught rather than silently doing nothing.
pub fn parse(text: &str) -> Result<ProjectConfig> {
    let config: ProjectConfig = toml::from_str(text)?;
    if config.repos.iter().any(|r| r.trim().is_empty()) {
        bail!("repos cannot hold an empty name");
    }
    Ok(config)
}

/// A name that is safe to look a project up by: one plain path component,
/// so it can never climb out of the projects directory.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        bail!("{name:?} is not a project name");
    }
    Ok(())
}

/// A name for a new project: letters, digits, `-`, `_` and `.`.
fn check_new_name(name: &str) -> Result<()> {
    check_name(name)?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("{name:?} is not a usable project name: use letters, digits, `-`, `_` and `.`");
    }
    Ok(())
}

/// The directory a project of this name lives in, whether or not it exists.
pub fn dir(app: &App, name: &str) -> Result<PathBuf> {
    check_name(name)?;
    Ok(app.projects_root().join(name))
}

/// The directory of a project that exists.
pub fn existing_dir(app: &App, name: &str) -> Result<PathBuf> {
    let dir = dir(app, name)?;
    if !dir.is_dir() {
        bail!("no project named {name}; `jeet projects ls` lists them");
    }
    Ok(dir)
}

/// A project, with its `project.toml` read. A project without one is fine:
/// it just presets no repositories.
pub fn load(app: &App, name: &str) -> Result<Project> {
    let dir = existing_dir(app, name)?;
    let path = dir.join(PROJECT_FILE);
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("{} is not valid", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProjectConfig::default(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(Project {
        name: name.to_string(),
        dir,
        config,
    })
}

/// Every project's name, sorted.
pub fn list(app: &App) -> Result<Vec<String>> {
    let root = app.projects_root();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", root.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if check_name(&name).is_ok() && entry.path().is_dir() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Make a project: `project.toml` listing `repos`, a starter AGENTS.md, each
/// harness's file importing it (CLAUDE.md), and an empty `skills/`. Returns
/// its directory.
pub fn create(app: &App, name: &str, repos: &[String]) -> Result<PathBuf> {
    check_new_name(name)?;
    if repos.iter().any(|r| r.trim().is_empty()) {
        bail!("a repository name cannot be empty");
    }
    let root = app.projects_root();
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let dir = root.join(name);
    // create_dir, not create_dir_all: it claims the name, failing if taken.
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("project {name} already exists at {}", dir.display())
        }
        Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
    }
    let written = std::fs::write(dir.join(PROJECT_FILE), template(repos))
        .context("write project.toml")
        .and_then(|_| {
            std::fs::write(dir.join(AGENTS_MD), starter_agents_md(name)).context("write AGENTS.md")
        })
        .and_then(|_| {
            instructions::imports().into_iter().try_for_each(|import| {
                std::fs::write(dir.join(import.file), import.text)
                    .with_context(|| format!("write {}", import.file))
            })
        })
        .and_then(|_| std::fs::create_dir(dir.join(SKILLS_DIR)).context("create skills/"));
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(dir)
}

/// A new project's `project.toml`: the repositories given, and the optional
/// settings commented out so they are easy to find.
fn template(repos: &[String]) -> String {
    let list: Vec<String> = repos
        .iter()
        .map(|r| toml::Value::String(r.trim().to_string()).to_string())
        .collect();
    format!(
        "# Repositories every task made from this project starts with a worktree of:\n\
         # names like \"api\", \"owner/repo\", store ids, or clone URLs.\n\
         repos = [{}]\n\
         \n\
         # Extra arguments for the coding agent in this project's tasks.\n\
         # [agent]\n\
         # args = [\"--model\", \"opus\"]\n\
         \n\
         # What this project's tasks do about pull requests: auto, draft or ask.\n\
         # [tasks]\n\
         # pr = \"draft\"\n",
        list.join(", ")
    )
}

fn starter_agents_md(name: &str) -> String {
    let mut text = format!(
        "# {name}\n\n\
         Instructions for the coding agent in every task made from the {name} project.\n\
         jeet copies this file, and the skills next to it, into each new task.\n"
    );
    for import in instructions::imports() {
        text.push_str(&format!(
            "{} only imports this file, for {}.\n",
            import.file, import.harness
        ));
    }
    text
}

/// The project's `project.toml`, written from the template first if the
/// project has none yet, so there is always something to edit. It is not
/// parsed: a broken one is exactly what needs editing.
pub fn config_path(app: &App, name: &str) -> Result<PathBuf> {
    let path = existing_dir(app, name)?.join(PROJECT_FILE);
    if !path.exists() {
        std::fs::write(&path, template(&[]))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path)
}

/// Delete a project, broken or not. Tasks made from it keep their copies of
/// its files.
pub fn remove(app: &App, name: &str) -> Result<()> {
    let dir = existing_dir(app, name)?;
    // Removes a symlinked project's link, never what it points at.
    std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
}

/// Tasks, active or archived, that were made from the project.
pub fn tasks_using(app: &App, name: &str) -> Result<Vec<TaskRecord>> {
    Ok(app
        .db
        .list_tasks(None)?
        .into_iter()
        .filter(|t| t.project.as_deref() == Some(name))
        .collect())
}

/// Extra agent arguments for `task`, from the project it was made from — as
/// the project is now, unlike its files. None without a project, and none
/// for a manager.
pub fn agent_args(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    // A manager's launch is jeet's: a project's arguments are for the
    // coding agents of tasks, and could undo what a manager is run with.
    if task.kind == TaskKind::Manager {
        return Ok(Vec::new());
    }
    match &task.project {
        Some(name) => Ok(load(app, name)?.config.agent.args),
        None => Ok(Vec::new()),
    }
}

/// What [`copy_files`] did, relative to the task directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileSync {
    /// Written: new to the task, or brought up to date.
    pub written: Vec<String>,
    /// Gone from the project, and unchanged in the task, so deleted there.
    pub removed: Vec<String>,
    /// Left alone because the task changed them; each with the reason.
    pub kept: Vec<String>,
    /// In the project, but never copied; each with the reason.
    pub skipped: Vec<String>,
    /// What happened to the project itself: a CLAUDE.md moved to AGENTS.md.
    pub migrated: Vec<String>,
}

impl FileSync {
    /// Lines worth telling the user: a move to AGENTS.md, and what was kept
    /// back and why.
    pub fn notes(&self) -> Vec<String> {
        self.migrated
            .iter()
            .chain(&self.kept)
            .chain(&self.skipped)
            .cloned()
            .collect()
    }
}

/// Copy the project's instruction files and skills into a task directory,
/// recording what was copied. A project with only a CLAUDE.md has it moved
/// to AGENTS.md first.
///
/// A file the task has changed since it was last copied is kept as the task
/// has it, unless `force`. Likewise a file the project no longer has is
/// deleted from the task only if the task never changed it. A file with no
/// record — the task made it, or predates the record — counts as changed.
#[cfg(test)]
pub fn copy_files(project: &Project, task_dir: &Path, force: bool) -> Result<FileSync> {
    copy_files_for(project, task_dir, force, false)
}

/// [`copy_files`] for a task's directory, or with `manager` a manager's: the
/// project's skills as they are, but its AGENTS.md as `PROJECT.md`, and not
/// its other instruction files, so the manager's own are never taken over.
pub fn copy_files_for(
    project: &Project,
    task_dir: &Path,
    force: bool,
    manager: bool,
) -> Result<FileSync> {
    let mut sync = FileSync::default();
    match instructions::migrate(&project.dir) {
        Ok(true) => sync.migrated.push(format!(
            "moved the project's {CLAUDE_MD} to {AGENTS_MD}; its {CLAUDE_MD} now imports it"
        )),
        Ok(false) => {}
        Err(e) => sync.migrated.push(format!(
            "could not move the project's {CLAUDE_MD} to {AGENTS_MD} ({e:#}); copied it as it is"
        )),
    }
    let wanted = project_files(&project.dir, manager, &mut sync.skipped)?;
    let before = read_manifest(task_dir);
    let mut after: BTreeMap<String, u64> = BTreeMap::new();

    for (rel, source) in &wanted {
        let content =
            std::fs::read(source).with_context(|| format!("read {}", source.display()))?;
        let target = task_dir.join(rel);
        if !reachable_without_links(task_dir, rel) {
            sync.kept.push(format!("{rel}: a symlink is in the way"));
            continue;
        }
        let current = match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_file() => {
                Some(std::fs::read(&target).with_context(|| format!("read {}", target.display()))?)
            }
            Ok(_) => {
                sync.kept
                    .push(format!("{rel}: something other than a file is there"));
                continue;
            }
            Err(_) => None,
        };
        let unchanged = |held: &[u8]| before.get(rel) == Some(&fnv1a(held));
        match current {
            Some(held) if held == content => {}
            Some(held) if !force && !unchanged(&held) => {
                sync.kept
                    .push(format!("{rel}: changed in the task; kept the task's copy"));
                // Still what the project gave last time, if it is known.
                if let Some(hash) = before.get(rel) {
                    after.insert(rel.clone(), *hash);
                }
                continue;
            }
            _ => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(&target, &content)
                    .with_context(|| format!("write {}", target.display()))?;
                sync.written.push(rel.clone());
            }
        }
        after.insert(rel.clone(), fnv1a(&content));
    }

    // Files the project gave before and no longer does.
    let skills_root = task_dir.join(TASK_SKILLS);
    for (rel, hash) in &before {
        if wanted.contains_key(rel) {
            continue;
        }
        let target = task_dir.join(rel);
        if !reachable_without_links(task_dir, rel) {
            continue;
        }
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        let held = std::fs::read(&target).with_context(|| format!("read {}", target.display()))?;
        if !force && fnv1a(&held) != *hash {
            sync.kept.push(format!(
                "{rel}: no longer in the project, but changed in the task; kept"
            ));
            continue;
        }
        std::fs::remove_file(&target).with_context(|| format!("remove {}", target.display()))?;
        crate::worktrees::prune_empty_parents(
            &target,
            &[skills_root.clone(), task_dir.to_path_buf()],
        );
        sync.removed.push(rel.clone());
    }

    write_manifest(task_dir, &after)?;
    Ok(sync)
}

/// Bring a task's copies of its project's files up to date: see
/// [`copy_files`]. The built-in jeet skill is put back too.
pub fn resync(app: &App, task: &TaskRecord, force: bool) -> Result<FileSync> {
    let Some(name) = &task.project else {
        bail!("task {} was not made from a project", task.slug);
    };
    let project = load(app, name)?;
    let dir = tasks::task_dir(app, task);
    if !dir.is_dir() {
        bail!("the task directory {} is missing", dir.display());
    }
    let manager = task.kind == TaskKind::Manager;
    let sync = copy_files_for(&project, &dir, force, manager)?;
    if manager {
        tasks::link_project_instructions(&dir, &project.name)?;
    } else {
        tasks::install_skill(&dir)?;
    }
    Ok(sync)
}

/// The files a task gets from a project, keyed by where they go in the task.
/// Symlinks are skipped rather than followed out of the project, as is a
/// skill named like the built-in one. A manager gets the project's AGENTS.md
/// as [`MANAGER_INSTRUCTIONS`] and no other instruction file.
fn project_files(
    project: &Path,
    manager: bool,
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    let names = match manager {
        true => vec![AGENTS_MD],
        false => instructions::files(),
    };
    for name in names {
        let path = project.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                let rel = match manager {
                    true => MANAGER_INSTRUCTIONS,
                    false => name,
                };
                out.insert(rel.to_string(), path);
            }
            Ok(_) => skipped.push(format!("{name}: not a plain file; not copied")),
            Err(_) => {}
        }
    }

    let skills = project.join(SKILLS_DIR);
    if !skills.is_dir() {
        return Ok(out);
    }
    let walk = walkdir::WalkDir::new(&skills)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !(e.depth() == 1 && e.file_name() == RESERVED_SKILL));
    if skills.join(RESERVED_SKILL).exists() {
        skipped.push(format!(
            "{SKILLS_DIR}/{RESERVED_SKILL}: the built-in skill's name; not copied"
        ));
    }
    for entry in walk {
        let entry = entry.with_context(|| format!("read {}", skills.display()))?;
        let rel = entry.path().strip_prefix(&skills)?;
        let Some(rel) = manifest_path(rel) else {
            skipped.push(format!(
                "{SKILLS_DIR}/{}: an unusable file name; not copied",
                rel.display()
            ));
            continue;
        };
        if entry.file_type().is_symlink() {
            skipped.push(format!("{SKILLS_DIR}/{rel}: a symlink; not copied"));
        } else if entry.file_type().is_file() {
            out.insert(format!("{TASK_SKILLS}/{rel}"), entry.into_path());
        }
    }
    Ok(out)
}

/// `rel` as a `/`-separated manifest path, if it can be one: UTF-8, plain
/// components, and nothing that would break a manifest line.
fn manifest_path(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        let std::path::Component::Normal(part) = c else {
            return None;
        };
        let part = part.to_str()?;
        if part.contains(['\t', '\n', '\r']) {
            return None;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Whether a manifest entry names a place a project file can go. The
/// manifest sits in the task, where anything can edit it; an entry anywhere
/// else is ignored rather than trusted.
fn is_project_path(rel: &str) -> bool {
    let plain = manifest_path(Path::new(rel)).as_deref() == Some(rel);
    plain
        && (instructions::files().contains(&rel)
            || rel == MANAGER_INSTRUCTIONS
            || rel
                .strip_prefix(TASK_SKILLS)
                .is_some_and(|rest| rest.len() > 1 && rest.starts_with('/')))
}

/// Whether `rel` under `root` can be reached without passing through a
/// symlink, which could lead out of the task.
fn reachable_without_links(root: &Path, rel: &str) -> bool {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            Ok(_) => {}
            // Nothing there yet, so nothing further down either.
            Err(_) => return true,
        }
    }
    true
}

fn read_manifest(task_dir: &Path) -> BTreeMap<String, u64> {
    let Ok(text) = std::fs::read_to_string(task_dir.join(MANIFEST)) else {
        return BTreeMap::new();
    };
    parse_manifest(&text)
}

/// `<hash>\t<path>` per line; anything malformed is dropped, which only
/// makes resync more careful.
fn parse_manifest(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (hash, rel) = line.split_once('\t')?;
            let hash = u64::from_str_radix(hash, 16).ok()?;
            is_project_path(rel).then(|| (rel.to_string(), hash))
        })
        .collect()
}

fn render_manifest(entries: &BTreeMap<String, u64>) -> String {
    entries
        .iter()
        .map(|(rel, hash)| format!("{hash:016x}\t{rel}\n"))
        .collect()
}

/// Write the manifest whole or not at all: tmp + rename.
fn write_manifest(task_dir: &Path, entries: &BTreeMap<String, u64>) -> Result<()> {
    let path = task_dir.join(MANIFEST);
    let dir = path.parent().unwrap_or(task_dir);
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, render_manifest(entries))
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

/// 64-bit FNV-1a: stable across Rust releases, unlike `DefaultHasher`, and
/// plenty to notice that a file was edited.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::CLAUDE_SHIM;

    fn project_at(dir: &Path) -> Project {
        Project {
            name: "web".into(),
            dir: dir.to_path_buf(),
            config: ProjectConfig::default(),
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn project_toml_takes_repos_and_agent_args() {
        let config = parse(
            "repos = [\"github.com/acme/api\", \"https://github.com/acme/web\"]\n\
             [agent]\nargs = [\"--model\", \"opus\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.repos,
            ["github.com/acme/api", "https://github.com/acme/web"]
        );
        assert_eq!(config.agent.args, ["--model", "opus"]);
        assert_eq!(parse("").unwrap(), ProjectConfig::default());
    }

    #[test]
    fn project_toml_refuses_typos_and_empty_repos() {
        for bad in [
            "repo = [\"api\"]",
            "[agent]\narg = []",
            "repos = [\"\"]",
            "repos = \"api\"",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn the_template_parses_back_to_what_was_asked() {
        let repos = vec![
            "api".to_string(),
            "https://github.com/acme/web".to_string(),
            "odd \"quoted\" \\ name".to_string(),
        ];
        assert_eq!(parse(&template(&repos)).unwrap().repos, repos);
        assert!(parse(&template(&[])).unwrap().repos.is_empty());
        // The commented-out [agent] table parses to its defaults.
        assert!(parse(&template(&[])).unwrap().agent.args.is_empty());
    }

    #[test]
    fn project_names_stay_inside_the_projects_directory() {
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a\\b", "a\nb"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name("my project").is_ok());
        assert!(check_new_name("my project").is_err());
        for ok in ["web", "api-v2", "docs_site", "v1.2"] {
            assert!(check_new_name(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn manifests_round_trip_and_ignore_entries_outside_the_task_files() {
        let mut entries = BTreeMap::new();
        entries.insert("AGENTS.md".to_string(), 1);
        entries.insert("CLAUDE.md".to_string(), 2);
        entries.insert(".claude/skills/deploy/SKILL.md".to_string(), u64::MAX);
        assert_eq!(parse_manifest(&render_manifest(&entries)), entries);

        let hostile = "0000000000000001\t../../etc/passwd\n\
                       0000000000000001\t/etc/passwd\n\
                       0000000000000001\t.claude/skills\n\
                       0000000000000001\t.claude/skills/../../x\n\
                       0000000000000001\tsrc/main.rs\n\
                       nothex\tCLAUDE.md\n\
                       no tab here\n";
        assert!(parse_manifest(hostile).is_empty());
    }

    #[test]
    fn a_projects_agent_args_are_for_tasks_not_managers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home: home.clone(),
        };
        let project = home.join("projects/web");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.toml"),
            "[agent]\nargs = [\"--model\", \"opus\"]\n",
        )
        .unwrap();
        let task = |kind| TaskRecord {
            id: "t".into(),
            slug: "t".into(),
            name: "t".into(),
            project: Some("web".into()),
            kind,
            state: crate::db::tasks::TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: None,
            placement: crate::placement::for_new(),
            run_state: crate::db::tasks::RunState::Run,
            run_state_by: None,
            max_children: None,
        };
        assert_eq!(
            agent_args(&app, &task(TaskKind::Work)).unwrap(),
            ["--model", "opus"]
        );
        assert!(agent_args(&app, &task(TaskKind::Manager))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fnv_is_the_published_function() {
        // Test vectors from the FNV reference.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_manager_gets_the_projects_agents_md_as_project_md_and_nothing_over_its_own() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(&project.path().join("AGENTS.md"), "rules\n");
        write(&project.path().join("CLAUDE.md"), CLAUDE_SHIM);
        write(&project.path().join("skills/deploy/SKILL.md"), "deploy\n");
        write(&home.path().join("AGENTS.md"), "the manager's own\n");

        let sync = copy_files_for(&project_at(project.path()), home.path(), false, true).unwrap();
        assert_eq!(
            sync.written,
            [".claude/skills/deploy/SKILL.md", "PROJECT.md"]
        );
        assert_eq!(read(&home.path().join("PROJECT.md")), "rules\n");
        assert_eq!(read(&home.path().join("AGENTS.md")), "the manager's own\n");
        assert!(!home.path().join("CLAUDE.md").exists());
        let manifest = read_manifest(home.path());
        assert!(manifest.contains_key("PROJECT.md"));
        assert!(!manifest.contains_key("AGENTS.md"));

        // Linked once, after what the manager has.
        tasks::link_project_instructions(home.path(), "web").unwrap();
        tasks::link_project_instructions(home.path(), "web").unwrap();
        let agents = read(&home.path().join("AGENTS.md"));
        assert!(agents.starts_with("the manager's own\n"));
        assert_eq!(agents.matches("@PROJECT.md").count(), 1, "{agents}");
    }

    #[test]
    fn copying_takes_the_instructions_and_skills_but_not_the_reserved_skill() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        write(&project.path().join("AGENTS.md"), "rules\n");
        write(&project.path().join("CLAUDE.md"), CLAUDE_SHIM);
        write(&project.path().join("skills/deploy/SKILL.md"), "deploy\n");
        write(
            &project.path().join("skills/deploy/ref/notes.md"),
            "notes\n",
        );
        write(&project.path().join("skills/jeet/SKILL.md"), "mine\n");
        write(&project.path().join("project.toml"), "repos = []\n");

        let sync = copy_files(&project_at(project.path()), task.path(), false).unwrap();
        assert_eq!(
            sync.written,
            [
                ".claude/skills/deploy/SKILL.md",
                ".claude/skills/deploy/ref/notes.md",
                "AGENTS.md",
                "CLAUDE.md"
            ]
        );
        assert!(sync.kept.is_empty());
        assert_eq!(sync.skipped.len(), 1, "{:?}", sync.skipped);
        assert!(!task.path().join(".claude/skills/jeet").exists());
        assert!(!task.path().join("project.toml").exists());
        assert_eq!(read(&task.path().join("AGENTS.md")), "rules\n");
        assert_eq!(read(&task.path().join("CLAUDE.md")), CLAUDE_SHIM);

        // Nothing changed: nothing to do.
        let again = copy_files(&project_at(project.path()), task.path(), false).unwrap();
        assert_eq!(
            again,
            FileSync {
                skipped: sync.skipped.clone(),
                ..FileSync::default()
            }
        );
    }

    #[test]
    fn resyncing_updates_untouched_files_and_keeps_edited_ones() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let p = project.path();
        let t = task.path();
        write(&p.join("AGENTS.md"), "v1\n");
        write(&p.join("skills/a/SKILL.md"), "a1\n");
        write(&p.join("skills/b/SKILL.md"), "b1\n");
        write(&p.join("skills/gone/SKILL.md"), "g1\n");
        write(&p.join("skills/edited-gone/SKILL.md"), "e1\n");
        copy_files(&project_at(p), t, false).unwrap();

        // The task edits some copies…
        write(&t.join("AGENTS.md"), "the task's own\n");
        write(&t.join(".claude/skills/edited-gone/SKILL.md"), "mine\n");
        // …and the project moves on.
        write(&p.join("AGENTS.md"), "v2\n");
        write(&p.join("skills/a/SKILL.md"), "a2\n");
        write(&p.join("skills/new/SKILL.md"), "n1\n");
        std::fs::remove_dir_all(p.join("skills/gone")).unwrap();
        std::fs::remove_dir_all(p.join("skills/edited-gone")).unwrap();

        let sync = copy_files(&project_at(p), t, false).unwrap();
        assert_eq!(
            sync.written,
            [".claude/skills/a/SKILL.md", ".claude/skills/new/SKILL.md"]
        );
        assert_eq!(sync.removed, [".claude/skills/gone/SKILL.md"]);
        assert_eq!(sync.kept.len(), 2, "{:?}", sync.kept);
        assert_eq!(read(&t.join("AGENTS.md")), "the task's own\n");
        assert_eq!(read(&t.join(".claude/skills/a/SKILL.md")), "a2\n");
        assert_eq!(read(&t.join(".claude/skills/b/SKILL.md")), "b1\n");
        assert!(!t.join(".claude/skills/gone").exists());
        assert_eq!(
            read(&t.join(".claude/skills/edited-gone/SKILL.md")),
            "mine\n"
        );

        // Still the task's on the next resync, until it is forced.
        let sync = copy_files(&project_at(p), t, false).unwrap();
        assert!(sync.kept.iter().any(|k| k.starts_with("AGENTS.md")));
        let sync = copy_files(&project_at(p), t, true).unwrap();
        assert_eq!(sync.written, ["AGENTS.md"]);
        assert_eq!(read(&t.join("AGENTS.md")), "v2\n");
        // edited-gone was dropped from the record when kept: the task owns it.
        assert!(t.join(".claude/skills/edited-gone/SKILL.md").exists());
    }

    #[test]
    fn a_file_the_task_had_before_any_record_is_never_overwritten() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        write(&project.path().join("AGENTS.md"), "project\n");
        write(&task.path().join("AGENTS.md"), "already here\n");

        let sync = copy_files(&project_at(project.path()), task.path(), false).unwrap();
        assert!(sync.written.is_empty());
        assert_eq!(sync.kept.len(), 1);
        assert_eq!(read(&task.path().join("AGENTS.md")), "already here\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_neither_copied_nor_written_through() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let p = project.path();
        write(&outside.path().join("secret"), "secret\n");
        write(&p.join("skills/a/SKILL.md"), "a\n");
        std::os::unix::fs::symlink(outside.path().join("secret"), p.join("skills/a/leak")).unwrap();
        // The task's skills directory leads somewhere else entirely.
        std::fs::create_dir_all(task.path().join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.path(), task.path().join(".claude/skills")).unwrap();

        let sync = copy_files(&project_at(p), task.path(), true).unwrap();
        assert!(sync.written.is_empty(), "{:?}", sync.written);
        assert!(sync.skipped.iter().any(|s| s.contains("leak")));
        assert!(sync.kept.iter().any(|k| k.contains("symlink")));
        assert!(!outside.path().join("a").exists());
    }

    #[test]
    fn summaries_name_what_a_task_gets() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_at(dir.path());
        assert_eq!(project.contents_summary(), "-");
        // A project from before AGENTS.md, until it is moved.
        write(&dir.path().join("CLAUDE.md"), "x");
        write(&dir.path().join("skills/a/SKILL.md"), "x");
        assert_eq!(project.contents_summary(), "CLAUDE.md, 1 skill");
        write(&dir.path().join("AGENTS.md"), "x");
        write(&dir.path().join("skills/b/SKILL.md"), "x");
        assert_eq!(project.skills(), ["a", "b"]);
        assert_eq!(project.contents_summary(), "AGENTS.md, 2 skills");
    }

    #[test]
    fn a_project_with_only_claude_md_is_moved_to_agents_md_and_its_tasks_follow() {
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        write(&p.join("CLAUDE.md"), "v1\n");
        // Tasks copied from it before AGENTS.md: one untouched, one edited.
        let tasks = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        for task in &tasks {
            write(&task.path().join("CLAUDE.md"), "v1\n");
            let mut record = BTreeMap::new();
            record.insert("CLAUDE.md".to_string(), fnv1a(b"v1\n"));
            write_manifest(task.path(), &record).unwrap();
        }
        let (untouched, edited) = (tasks[0].path(), tasks[1].path());
        write(&edited.join("CLAUDE.md"), "the task's own\n");

        let sync = copy_files(&project_at(p), untouched, false).unwrap();
        assert_eq!(read(&p.join("AGENTS.md")), "v1\n");
        assert_eq!(read(&p.join("CLAUDE.md")), CLAUDE_SHIM);
        assert_eq!(sync.migrated.len(), 1, "{:?}", sync.migrated);
        assert!(sync.notes().iter().any(|n| n.contains("moved")));
        assert_eq!(sync.written, ["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(read(&untouched.join("AGENTS.md")), "v1\n");
        assert_eq!(read(&untouched.join("CLAUDE.md")), CLAUDE_SHIM);

        // Moved once: the next task finds nothing to move, and keeps its edit.
        let sync = copy_files(&project_at(p), edited, false).unwrap();
        assert!(sync.migrated.is_empty());
        assert_eq!(sync.written, ["AGENTS.md"]);
        assert_eq!(read(&edited.join("CLAUDE.md")), "the task's own\n");
        assert!(sync.kept.iter().any(|k| k.starts_with("CLAUDE.md")));
    }

    #[test]
    fn a_project_with_both_files_keeps_both_as_they_are() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let p = project.path();
        write(&p.join("AGENTS.md"), "everyone\n");
        write(&p.join("CLAUDE.md"), "@AGENTS.md\n\nclaude only\n");
        let sync = copy_files(&project_at(p), task.path(), false).unwrap();
        assert!(sync.migrated.is_empty());
        assert_eq!(read(&p.join("CLAUDE.md")), "@AGENTS.md\n\nclaude only\n");
        assert_eq!(
            read(&task.path().join("CLAUDE.md")),
            "@AGENTS.md\n\nclaude only\n"
        );
        assert_eq!(read(&task.path().join("AGENTS.md")), "everyone\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_md_that_cannot_be_moved_is_copied_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let p = project.path();
        write(&p.join("CLAUDE.md"), "rules\n");
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o555)).unwrap();
        let sync = copy_files(&project_at(p), task.path(), false);
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sync = sync.unwrap();
        if p.join("AGENTS.md").exists() {
            return; // Run as root: permissions do not stop it.
        }
        assert!(sync.migrated[0].starts_with("could not move"), "{sync:?}");
        assert_eq!(read(&p.join("CLAUDE.md")), "rules\n");
        assert_eq!(sync.written, ["CLAUDE.md"]);
        assert_eq!(read(&task.path().join("CLAUDE.md")), "rules\n");
    }
}
