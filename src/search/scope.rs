//! Which sources of documents a caller may search. The index holds
//! documents of every source; a query is always cut to the sources listed
//! here, so what a task may not read never turns up, even as a title.

use anyhow::Result;

use crate::cli::SearchKind;
use crate::context::App;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::identity;
use crate::projects;
use crate::resolve;

/// Repositories that hold the tool's own docs, matched by the end of their
/// store id. Every caller may search them, so an agent can look up how jeet
/// and plete work.
const PLETE_REPOS: &[&str] = &["github.com/peterddod/jeet", "github.com/peterddod/plete"];

/// A set of documents that are indexed and allowed together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The assistant's home: its notes, briefs and archives.
    Assistant,
    /// A work or manager task's own folder, by task id.
    Task(String),
    /// A project's folder, by name.
    Project(String),
    /// A repository's trunk, by store id.
    Repo(String),
}

impl Source {
    /// The name stored in `docs.source` and shown in results.
    pub fn key(&self) -> String {
        match self {
            Source::Assistant => "assistant".to_string(),
            Source::Task(id) => format!("task:{id}"),
            Source::Project(name) => format!("project:{name}"),
            Source::Repo(id) => format!("repo:{id}"),
        }
    }

    /// The kind `--in` names it by.
    pub fn kind(&self) -> SearchKind {
        match self {
            Source::Assistant => SearchKind::Assistant,
            Source::Task(_) => SearchKind::Tasks,
            Source::Project(_) => SearchKind::Projects,
            Source::Repo(_) => SearchKind::Repos,
        }
    }
}

/// What one search may look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub sources: Vec<Source>,
}

impl Scope {
    /// Everything the caller may search, cut to `kinds` when any are given.
    /// `kinds` narrow the scope and never add to it.
    pub fn of_caller(app: &App, kinds: &[SearchKind]) -> Result<Scope> {
        let mut sources = for_caller(app)?;
        narrow(&mut sources, kinds);
        Ok(Scope { sources })
    }

    /// The sources' names, as stored with each document.
    pub fn keys(&self) -> Vec<String> {
        self.sources.iter().map(Source::key).collect()
    }
}

/// The sources the caller may search, in the order they are listed, none
/// twice:
/// - a review's agent: the repository under review;
/// - the user at a shell, and the assistant: everything;
/// - any other task: its own folder, those of the tasks below it, their
///   projects and repositories. Never the assistant's notes, nor the folder
///   of a task above it.
///
/// Every caller also gets the tool's own docs, and nobody gets a repository
/// `[search] exclude_repos` names.
pub fn for_caller(app: &App) -> Result<Vec<Source>> {
    let me = identity::identify(app)?;
    let mut sources = match (&me.review, &me.task) {
        (Some(review), _) => for_review(review),
        (None, Some(task)) if task.kind != TaskKind::Assistant => for_task(app, task)?,
        (None, _) => everything(app)?,
    };
    sources.extend(plete_repos(app)?);
    let mut out: Vec<Source> = Vec::new();
    for source in sources {
        if !out.contains(&source) {
            out.push(source);
        }
    }
    let excluded = excluded_repo_ids(app)?;
    out.retain(|s| !matches!(s, Source::Repo(id) if excluded.contains(id)));
    Ok(out)
}

/// Keep the sources of `kinds`; no kinds keeps all.
fn narrow(sources: &mut Vec<Source>, kinds: &[SearchKind]) {
    if !kinds.is_empty() {
        sources.retain(|s| kinds.contains(&s.kind()));
    }
}

/// Whether the caller is the user or the assistant, who may reset the index.
pub fn is_user_or_assistant(app: &App) -> Result<bool> {
    let me = identity::identify(app)?;
    Ok(me.review.is_none() && me.task.is_none_or(|t| t.kind == TaskKind::Assistant))
}

/// A review's agent reads the repository its pull request is on, and
/// nothing else.
fn for_review(review: &ReviewRecord) -> Vec<Source> {
    vec![Source::Repo(review.repo_id.clone())]
}

/// Everything there is: the assistant, every task, every project and every
/// repository in the store.
fn everything(app: &App) -> Result<Vec<Source>> {
    let mut out = vec![Source::Assistant];
    for task in app.db.list_tasks(None)? {
        if has_folder(&task) {
            out.push(Source::Task(task.id));
        }
    }
    out.extend(projects::list(app)?.into_iter().map(Source::Project));
    out.extend(
        app.db
            .list_repos(None)?
            .into_iter()
            .map(|r| Source::Repo(r.id)),
    );
    Ok(out)
}

/// What `task` reaches: itself and the tasks below it, the projects those
/// belong to, and the repositories those hold worktrees of or list in their
/// `project.toml`.
fn for_task(app: &App, task: &TaskRecord) -> Result<Vec<Source>> {
    let mut tree = vec![task.clone()];
    for (below, _) in app.db.descendants(&task.id)? {
        if has_folder(&below) {
            tree.push(below);
        }
    }
    let mut out: Vec<Source> = tree.iter().map(|t| Source::Task(t.id.clone())).collect();

    let mut project_names: Vec<&str> = Vec::new();
    for name in tree.iter().filter_map(|t| t.project.as_deref()) {
        if !project_names.contains(&name) && projects::existing_dir(app, name).is_ok() {
            project_names.push(name);
        }
    }
    out.extend(project_names.iter().map(|n| Source::Project(n.to_string())));

    for t in &tree {
        for wt in app.db.list_task_worktrees(&t.id)? {
            out.push(Source::Repo(wt.repo_id));
        }
    }
    for name in project_names {
        let Ok(project) = projects::load(app, name) else {
            continue;
        };
        for spec in &project.config.repos {
            if let Ok(repo) = resolve::resolve_repo_filter(&app.db, spec) {
                out.push(Source::Repo(repo.id));
            }
        }
    }
    Ok(out)
}

/// Whether a task keeps documents in a folder of its own: work and manager
/// tasks do; the assistant has its own source and the setup agent none.
fn has_folder(task: &TaskRecord) -> bool {
    matches!(task.kind, TaskKind::Work | TaskKind::Manager)
}

/// The tool's own repositories that are in the store.
fn plete_repos(app: &App) -> Result<Vec<Source>> {
    Ok(app
        .db
        .list_repos(None)?
        .into_iter()
        .filter(|r| PLETE_REPOS.contains(&r.id.as_str()))
        .map(|r| Source::Repo(r.id))
        .collect())
}

/// The ids of the repositories `[search] exclude_repos` names: each filter
/// is a whole id or its last parts (`repo`, `owner/repo`), and leaves out
/// every repository it matches, so an ambiguous one hides more, not less.
fn excluded_repo_ids(app: &App) -> Result<Vec<String>> {
    let filters = app.config.search.excluded_repos();
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    Ok(app
        .db
        .list_repos(None)?
        .into_iter()
        .filter(|r| filters.iter().any(|f| repo_matches(&r.id, f)))
        .map(|r| r.id)
        .collect())
}

/// Whether the repository `id` is `filter` or ends in `/` and `filter`,
/// ignoring case.
fn repo_matches(id: &str, filter: &str) -> bool {
    let (id, filter) = (id.to_lowercase(), filter.trim().to_lowercase());
    !filter.is_empty() && (id == filter || id.ends_with(&format!("/{filter}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{RunState, TaskState, TaskWorktreeRecord};
    use crate::db::RepoRecord;

    fn lab() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home,
            new_home: None,
        };
        (dir, app)
    }

    fn task(app: &App, id: &str, kind: TaskKind, parent: Option<&str>, project: Option<&str>) {
        app.db
            .insert_task(&TaskRecord {
                id: id.into(),
                slug: id.into(),
                name: id.into(),
                project: project.map(str::to_string),
                kind,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: parent.map(str::to_string),
                placement: crate::placement::for_new(),
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
    }

    fn repo(app: &App, id: &str) {
        app.db
            .upsert_repo(&RepoRecord {
                id: id.into(),
                trunk_path: format!("/nowhere/{id}"),
                remote_url: String::new(),
                default_branch: "main".into(),
            })
            .unwrap();
    }

    #[test]
    fn an_excluded_repo_filter_matches_whole_parts() {
        assert!(repo_matches("github.com/a/api", "api"));
        assert!(repo_matches("github.com/a/api", "A/Api"));
        assert!(repo_matches("github.com/a/api", "github.com/a/api"));
        assert!(!repo_matches("github.com/a/rapi", "api"));
        assert!(!repo_matches("github.com/a/api", " "));
    }

    fn keys(sources: &[Source]) -> Vec<String> {
        sources.iter().map(Source::key).collect()
    }

    #[test]
    fn a_task_sees_its_subtree_and_never_the_assistant_or_its_parent() {
        let (_dir, app) = lab();
        repo(&app, "github.com/acme/api");
        repo(&app, "github.com/acme/other");
        repo(&app, "github.com/peterddod/plete");
        std::fs::create_dir_all(crate::config::projects_root(&app.home).join("proj")).unwrap();
        task(&app, "root", TaskKind::Assistant, None, None);
        task(&app, "mgr", TaskKind::Manager, Some("root"), None);
        task(&app, "w1", TaskKind::Work, Some("mgr"), Some("proj"));
        task(&app, "w2", TaskKind::Work, Some("mgr"), None);
        app.db
            .insert_task_worktree(&TaskWorktreeRecord {
                id: 0,
                task_id: "w1".into(),
                repo_id: "github.com/acme/api".into(),
                dir_name: "api".into(),
                branch: None,
                created_branch: false,
                head_oid: None,
                stash_ref: None,
                pr_number: None,
                created_at: 0,
                read_only: false,
            })
            .unwrap();

        let w1 = app.db.get_task("w1").unwrap().unwrap();
        let mut got = for_task(&app, &w1).unwrap();
        got.extend(plete_repos(&app).unwrap());
        assert_eq!(
            keys(&got),
            [
                "task:w1",
                "project:proj",
                "repo:github.com/acme/api",
                "repo:github.com/peterddod/plete"
            ]
        );

        let mgr = app.db.get_task("mgr").unwrap().unwrap();
        let got = keys(&for_task(&app, &mgr).unwrap());
        assert_eq!(got[0], "task:mgr");
        assert!(got.contains(&"task:w1".to_string()));
        assert!(got.contains(&"task:w2".to_string()));
        assert!(!got.contains(&"assistant".to_string()));
        assert!(!got.contains(&"task:root".to_string()));
        assert!(got.contains(&"project:proj".to_string()));
    }

    #[test]
    fn the_user_and_the_assistant_see_everything() {
        let (_dir, app) = lab();
        repo(&app, "github.com/acme/api");
        std::fs::create_dir_all(crate::config::projects_root(&app.home).join("proj")).unwrap();
        task(&app, "root", TaskKind::Assistant, None, None);
        task(&app, "w1", TaskKind::Work, Some("root"), None);
        let got = keys(&everything(&app).unwrap());
        assert_eq!(
            got,
            [
                "assistant",
                "task:w1",
                "project:proj",
                "repo:github.com/acme/api"
            ]
        );
    }

    #[test]
    fn a_kind_filter_narrows_and_exclude_repos_removes() {
        let (_dir, mut app) = lab();
        repo(&app, "github.com/acme/api");
        repo(&app, "github.com/acme/web");
        app.config.search.exclude_repos = Some(vec!["acme/web".into()]);
        assert_eq!(excluded_repo_ids(&app).unwrap(), ["github.com/acme/web"]);
        let mut sources = vec![Source::Assistant, Source::Repo("x".into())];
        narrow(&mut sources, &[]);
        assert_eq!(sources.len(), 2);
        narrow(&mut sources, &[SearchKind::Repos]);
        assert_eq!(sources, [Source::Repo("x".into())]);
        let scope = Scope {
            sources: vec![Source::Assistant, Source::Repo("x".into())],
        };
        assert_eq!(scope.keys(), ["assistant", "repo:x"]);
    }
}
