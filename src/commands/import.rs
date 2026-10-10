//! `jeet import`: bring work from outside jeet into the store and into tasks.
//!
//! The way in for a new user, and the way out of the layout jeet had before
//! tasks. It looks for git checkouts under the paths given, else under
//! `scan_roots`, jeet's old `worktrees/` and `ephemeral/` directories and
//! every trunk the index knows, and then:
//!
//! - a repository outside the store is cloned into it from its origin, and
//!   each of its branches or checkouts with work in it — commits origin does
//!   not have, or uncommitted changes — is offered as a task. The task gets a
//!   worktree of the store's copy: the branch is fetched across from the
//!   original, and uncommitted changes (untracked files included) follow as a
//!   stash-shaped commit, applied in the new worktree. The original is only
//!   read, and stays as it was unless `--remove-originals` is given;
//! - a worktree of a store trunk that sits outside any task — one jeet made
//!   before tasks, or one made by hand — is jeet's own, so it is moved into a
//!   task with `git worktree move`, work and all.
//!
//! Claude Code sessions started in a checkout are copied to the task, so they
//! can be resumed there. Everything said goes to stderr.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::agent;
use crate::commands::clone;
use crate::config;
use crate::context::App;
use crate::db::tasks::{TaskRecord, TaskSessionRecord, TaskWorktreeRecord};
use crate::db::RepoRecord;
use crate::git;
use crate::remote;
use crate::resolve::same_path;
use crate::store;
use crate::tasks::{self, NewTask};
use crate::worktrees::{self, now_secs};

pub fn run(app: &App, paths: &[String], yes: bool, remove_originals: bool) -> Result<()> {
    let interactive = io::stdin().is_terminal();
    if !yes && !interactive {
        bail!("jeet import asks what to import; pass --yes to import everything it finds");
    }
    eprintln!("jeet: looking for repositories…");
    let plan = discover(app, paths)?;
    for note in &plan.notes {
        eprintln!("jeet: {note}");
    }
    if plan.repos.iter().all(|r| r.in_store) && plan.items.is_empty() {
        eprintln!("jeet: nothing to import");
        return Ok(());
    }
    print_plan(&plan);

    let groups = if yes {
        each_alone(plan.items.len())
    } else {
        match ask(PICK)?.as_deref().map(str::trim) {
            // End of input is no answer, and certainly not "all".
            None | Some("q" | "quit") => {
                eprintln!("jeet: nothing imported");
                return Ok(());
            }
            Some(answer) => parse_selection(answer, plan.items.len())?,
        }
    };

    let outcome = execute(app, &plan, &groups);
    for line in &outcome.lines {
        eprintln!("jeet: {line}");
    }
    let tasks = outcome.tasks;
    let failed = outcome.failed;
    eprintln!(
        "jeet: imported {} into {} task{}{}",
        outcome.imported,
        tasks,
        if tasks == 1 { "" } else { "s" },
        match failed {
            0 => String::new(),
            n => format!("; {n} could not be imported (see above)"),
        }
    );

    if outcome.originals.is_empty() {
        return Ok(());
    }
    if !remove_originals {
        eprintln!(
            "jeet: the originals are untouched; `jeet import --remove-originals` removes the \
             ones it has copied"
        );
        return Ok(());
    }
    eprintln!("jeet: originals now copied into tasks:");
    for original in &outcome.originals {
        eprintln!("  {}", original.describe());
    }
    if !yes {
        let answer = ask("Remove them? Type `yes` to remove: ")?;
        if answer.as_deref().map(str::trim) != Some("yes") {
            eprintln!("jeet: originals left in place");
            return Ok(());
        }
    }
    for original in &outcome.originals {
        match original.remove() {
            Ok(()) => eprintln!("jeet: removed {}", original.describe()),
            Err(e) => eprintln!("jeet: could not remove {}: {e:#}", original.describe()),
        }
    }
    Ok(())
}

const PICK: &str = "Import which? ⏎ all, one task each · `1+3` puts 1 and 3 in one task, \
                    `1+3=name` names it · `none` only brings the repositories into the store · \
                    `q` quits: ";

/// One line from the terminal, the prompt on stderr; `None` at the end of
/// input (ctrl-d).
fn ask(prompt: &str) -> Result<Option<String>> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read the answer")?;
    Ok((read > 0).then_some(line))
}

/// A repository found on disk: the checkouts sharing one git directory.
struct FoundRepo {
    /// The index id, from origin: `github.com/acme/api`.
    id: String,
    origin: String,
    /// Its git directory, which every checkout of it shares.
    common: PathBuf,
    /// Already in the store: the store's trunk is this repository, or holds
    /// a copy of it.
    in_store: bool,
}

/// Something offered as (part of) a task.
struct Item {
    /// Into [`Plan::repos`].
    repo: usize,
    /// What the task is called unless the user names it.
    name: String,
    action: Action,
}

enum Action {
    /// Copy work from a repository outside the store into a new worktree of
    /// the store's trunk.
    Copy {
        /// The branch to bring across; `None`, or the store's default branch
        /// (which its trunk has checked out), brings `head` across detached.
        branch: Option<String>,
        head: String,
        /// The checkout it is in, whose uncommitted changes come too.
        checkout: Option<PathBuf>,
        /// A linked worktree, which `--remove-originals` may remove; the
        /// main working tree is the repository itself and never is.
        linked: bool,
        dirty: usize,
        /// Commits nothing else has: none on origin (for a branch), or on no
        /// branch at all (for a detached HEAD). `None` when git could not say.
        unpushed: Option<usize>,
    },
    /// Move a worktree of a store trunk, as it is, into the task.
    Move {
        path: PathBuf,
        branch: Option<String>,
    },
}

#[derive(Default)]
struct Plan {
    repos: Vec<FoundRepo>,
    items: Vec<Item>,
    /// Things passed over, and why.
    notes: Vec<String>,
}

/// What to scan: the paths given, else `scan_roots` and jeet's old
/// directories. The store, the tasks and the projects are never scanned.
fn discover(app: &App, paths: &[String]) -> Result<Plan> {
    let explicit = !paths.is_empty();
    let roots: Vec<PathBuf> = if explicit {
        paths.iter().map(|p| config::expand_path(p)).collect()
    } else {
        let mut roots: Vec<PathBuf> = app
            .config
            .scan_roots
            .iter()
            .map(|p| config::expand_path(p))
            .collect();
        roots.push(app.worktrees_root());
        roots.push(app.ephemeral_root());
        roots
    };
    if explicit {
        for root in &roots {
            if !root.is_dir() {
                bail!("{} is not a directory", root.display());
            }
        }
    }
    let skip: Vec<PathBuf> = [
        app.store_root(),
        app.tasks_root(),
        app.projects_root(),
        app.home.join("reviews"),
    ]
    .into_iter()
    .map(|p| p.canonicalize().unwrap_or(p))
    .collect();
    let mut checkouts = find_checkouts(&roots, &skip);

    // Every trunk jeet knows, so a repository registered before tasks, and
    // the worktrees of the store's own trunks, are found wherever they are.
    let mut store_trunks: HashMap<PathBuf, RepoRecord> = HashMap::new();
    let store_root = app.store_root();
    for repo in app.db.list_repos(None)? {
        let trunk = PathBuf::from(&repo.trunk_path);
        if !trunk.is_dir() {
            continue;
        }
        if trunk.starts_with(&store_root) {
            if let Ok(common) = git::git_common_dir(&trunk) {
                store_trunks.insert(canonical(&common), repo.clone());
            }
        }
        if !explicit {
            checkouts.push(trunk);
        }
    }

    // One entry per repository, however many of its checkouts were found.
    let mut by_common: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for checkout in checkouts {
        let Ok(common) = git::git_common_dir(&checkout) else {
            continue;
        };
        let common = canonical(&common);
        if is_submodule(&common) {
            continue;
        }
        by_common.entry(common).or_insert(checkout);
    }

    let tasks_root = canonical(&app.tasks_root());
    let mut plan = Plan::default();
    for (common, checkout) in by_common {
        let entries = match git::worktree_list(&checkout) {
            Ok(entries) => entries,
            Err(e) => {
                plan.notes
                    .push(format!("skipped {}: {e:#}", checkout.display()));
                continue;
            }
        };
        match store_trunks.get(&common) {
            Some(record) => plan_store_repo(&mut plan, record, &common, &entries, &tasks_root),
            None => {
                if let Err(e) = plan_external_repo(app, &mut plan, &common, &checkout, &entries) {
                    plan.notes
                        .push(format!("skipped {}: {e:#}", checkout.display()));
                }
            }
        }
    }
    Ok(plan)
}

/// A store trunk's worktrees outside any task are moved into tasks.
fn plan_store_repo(
    plan: &mut Plan,
    record: &RepoRecord,
    common: &Path,
    entries: &[git::GitWorktreeEntry],
    tasks_root: &Path,
) {
    let trunk = Path::new(&record.trunk_path);
    let strays: Vec<&git::GitWorktreeEntry> = entries
        .iter()
        .filter(|e| !e.bare && e.path.exists())
        .filter(|e| !same_path(&e.path, trunk) && !e.path.starts_with(tasks_root))
        .collect();
    if strays.is_empty() {
        return;
    }
    let repo = plan.repos.len();
    plan.repos.push(FoundRepo {
        id: record.id.clone(),
        origin: record.remote_url.clone(),
        common: common.to_path_buf(),
        in_store: true,
    });
    for entry in strays {
        plan.items.push(Item {
            repo,
            name: suggest_name(entry.branch.as_deref(), Some(&entry.path), &record.id),
            action: Action::Move {
                path: entry.path.clone(),
                branch: entry.branch.clone(),
            },
        });
    }
}

/// A repository outside the store: what in it has work worth a task.
fn plan_external_repo(
    app: &App,
    plan: &mut Plan,
    common: &Path,
    checkout: &Path,
    entries: &[git::GitWorktreeEntry],
) -> Result<()> {
    let origin = git::origin_url(checkout).map_err(|_| {
        anyhow::anyhow!("it has no origin remote, so jeet cannot tell where it belongs")
    })?;
    let identity = remote::parse_remote_url_anyhow(&origin)?;
    let id = identity.id();
    let store_record = app
        .db
        .get_repo(&id)?
        .filter(|r| Path::new(&r.trunk_path).starts_with(app.store_root()));
    let default_branch = match &store_record {
        Some(record) => record.default_branch.clone(),
        None => git::default_branch(checkout)?,
    };
    // What the store's tasks already hold of it, so a second import does
    // not offer the same work again.
    let imported = match &store_record {
        Some(record) => in_tasks(app, Path::new(&record.trunk_path)),
        None => Vec::new(),
    };

    let repo = plan.repos.len();
    let mut items = Vec::new();
    let mut checked_out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.bare || !entry.path.exists() {
            continue;
        }
        if let Some(branch) = &entry.branch {
            checked_out.push(branch.clone());
        }
        let dirty = match git::status_counts(&entry.path) {
            Ok((dirty, _)) => dirty,
            Err(e) => {
                plan.notes
                    .push(format!("skipped {}: {e:#}", entry.path.display()));
                continue;
            }
        };
        let unpushed = match &entry.branch {
            Some(branch) => git::count_commits_not_in(
                &entry.path,
                &format!("refs/heads/{branch}"),
                &["--remotes"],
            ),
            None => git::count_commits_not_in(
                &entry.path,
                "HEAD",
                &["--remotes", "--branches", "--tags"],
            ),
        };
        if !has_work(dirty, unpushed) {
            continue;
        }
        // Work on the default branch comes across detached (the store's
        // trunk has that branch), so it is known by its commit and named
        // after its checkout.
        let named = entry.branch.as_deref().filter(|b| *b != default_branch);
        if let Some(task) = already_imported(&imported, named, &entry.head) {
            plan.notes.push(format!(
                "{} is already in task {task}",
                entry.branch.as_deref().unwrap_or(&entry.head)
            ));
            continue;
        }
        items.push(Item {
            repo,
            name: suggest_name(named, Some(&entry.path), &id),
            action: Action::Copy {
                branch: entry.branch.clone(),
                head: entry.head.clone(),
                checkout: Some(entry.path.clone()),
                // The first entry is the main working tree.
                linked: i > 0,
                dirty,
                unpushed,
            },
        });
    }

    // Branches checked out nowhere, with commits origin does not have.
    let branches = git::refs_under(checkout, "refs/heads/")?;
    for name in branches {
        let branch = name.trim_start_matches("refs/heads/").to_string();
        if branch == default_branch || checked_out.contains(&branch) {
            continue;
        }
        let unpushed = git::count_commits_not_in(checkout, &name, &["--remotes"]);
        if !has_work(0, unpushed) {
            continue;
        }
        let Some(head) = git::rev_parse(checkout, &name) else {
            continue;
        };
        if let Some(task) = already_imported(&imported, Some(&branch), &head) {
            plan.notes
                .push(format!("{branch} is already in task {task}"));
            continue;
        }
        items.push(Item {
            repo,
            name: suggest_name(Some(&branch), None, &id),
            action: Action::Copy {
                branch: Some(branch),
                head,
                checkout: None,
                linked: false,
                dirty: 0,
                unpushed,
            },
        });
    }

    plan.repos.push(FoundRepo {
        id,
        origin,
        common: common.to_path_buf(),
        in_store: store_record.is_some(),
    });
    plan.items.extend(items);
    Ok(())
}

/// Uncommitted changes, or commits nothing else has (or that git could not
/// rule out).
fn has_work(dirty: usize, unpushed: Option<usize>) -> bool {
    dirty > 0 || unpushed != Some(0)
}

/// `(branch, head, task slug)` for each worktree a task has of this trunk.
fn in_tasks(app: &App, trunk: &Path) -> Vec<(Option<String>, String, String)> {
    let root = canonical(&app.tasks_root());
    let Ok(entries) = git::worktree_list(trunk) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|e| {
            let rel = e.path.strip_prefix(&root).ok()?;
            let slug = rel.components().next()?.as_os_str().to_string_lossy();
            Some((e.branch.clone(), e.head.clone(), slug.to_string()))
        })
        .collect()
}

/// The task already holding this branch — or, for detached work, this
/// commit — if one does.
fn already_imported<'a>(
    imported: &'a [(Option<String>, String, String)],
    branch: Option<&str>,
    head: &str,
) -> Option<&'a str> {
    imported
        .iter()
        .find(|(b, h, _)| match branch {
            Some(branch) => b.as_deref() == Some(branch),
            None => h == head,
        })
        .map(|(_, _, slug)| slug.as_str())
}

/// Directories under `roots` holding a `.git`: checkouts. A checkout is not
/// searched for more inside it, and neither is anything under `skip` (jeet's
/// own store, tasks and projects) or a `.git` itself. Symlinks are not
/// followed.
fn find_checkouts(roots: &[PathBuf], skip: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        let mut walk = WalkDir::new(&root).follow_links(false).into_iter();
        while let Some(entry) = walk.next() {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            if entry.file_name() == ".git" || skip.iter().any(|s| path.starts_with(s)) {
                walk.skip_current_dir();
                continue;
            }
            if path.join(".git").exists() {
                out.push(path.to_path_buf());
                walk.skip_current_dir();
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A submodule's git directory lives under its superproject's
/// `.git/modules`. Its work belongs to the superproject's checkout.
fn is_submodule(common: &Path) -> bool {
    let parts: Vec<Component> = common.components().collect();
    parts
        .windows(2)
        .any(|w| w[0].as_os_str() == ".git" && w[1].as_os_str() == "modules")
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A task name for an item: its branch, else the name of the checkout it is
/// in, else the repository's.
fn suggest_name(branch: Option<&str>, checkout: Option<&Path>, repo_id: &str) -> String {
    if let Some(branch) = branch {
        return branch.to_string();
    }
    if let Some(name) = checkout.and_then(|c| c.file_name()) {
        return name.to_string_lossy().to_string();
    }
    repo_id.rsplit('/').next().unwrap_or(repo_id).to_string()
}

impl Item {
    /// `feat/login · 2 unpushed commits, 3 uncommitted changes · ~/api-login`
    fn describe(&self) -> String {
        match &self.action {
            Action::Copy {
                branch,
                head,
                checkout,
                dirty,
                unpushed,
                ..
            } => {
                let what = match branch {
                    Some(branch) => branch.clone(),
                    None => format!("detached @ {}", short(head)),
                };
                let mut work = Vec::new();
                match unpushed {
                    Some(0) => {}
                    Some(n) => work.push(plural(*n, "unpushed commit")),
                    None => work.push("commits jeet could not check".to_string()),
                }
                if *dirty > 0 {
                    work.push(plural(*dirty, "uncommitted change"));
                }
                let from = match checkout {
                    Some(path) => path.display().to_string(),
                    None => "(branch)".to_string(),
                };
                format!("{what} · {} · copied from {from}", work.join(", "))
            }
            Action::Move { path, branch } => {
                let what = match branch {
                    Some(branch) => branch.clone(),
                    None => "detached".to_string(),
                };
                format!("{what} · moved from {}", path.display())
            }
        }
    }
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn print_plan(plan: &Plan) {
    let new: Vec<&FoundRepo> = plan.repos.iter().filter(|r| !r.in_store).collect();
    if !new.is_empty() {
        eprintln!("jeet: into the store, cloned from origin:");
        let mut seen = Vec::new();
        for repo in new {
            if seen.contains(&&repo.id) {
                continue;
            }
            seen.push(&repo.id);
            eprintln!("  {:<40} {}", repo.id, repo.origin);
        }
    }
    if plan.items.is_empty() {
        eprintln!("jeet: no branches or worktrees with work in them to make tasks of");
        return;
    }
    eprintln!("jeet: to import, one task each unless grouped:");
    let width = plan.items.len().to_string().len();
    for (i, item) in plan.items.iter().enumerate() {
        eprintln!(
            "  {:>width$}  {:<32} {}",
            i + 1,
            plan.repos[item.repo].id,
            item.describe()
        );
    }
}

/// Which items go in which task, and what the task is called.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    /// Indexes into [`Plan::items`].
    items: Vec<usize>,
    name: Option<String>,
}

fn each_alone(count: usize) -> Vec<Group> {
    (0..count)
        .map(|i| Group {
            items: vec![i],
            name: None,
        })
        .collect()
}

/// The answer to "import which?": groups separated by spaces or commas, `+`
/// joining items (numbered from 1) into one task, and `=name` naming it.
/// Nothing, or `all`, is every item in a task of its own; `none` is no task.
fn parse_selection(answer: &str, count: usize) -> Result<Vec<Group>> {
    let answer = answer.trim();
    match answer {
        "" | "all" => return Ok(each_alone(count)),
        "none" => return Ok(Vec::new()),
        _ => {}
    }
    let mut groups = Vec::new();
    let mut used = Vec::new();
    for word in answer
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
    {
        let (numbers, name) = match word.split_once('=') {
            Some((numbers, name)) if !name.trim().is_empty() => {
                (numbers, Some(name.trim().to_string()))
            }
            Some(_) => bail!("{word:?}: say what the task is called after `=`"),
            None => (word, None),
        };
        let mut items = Vec::new();
        for number in numbers.split('+') {
            let n: usize = number
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{number:?} is not one of the numbers listed"))?;
            if n == 0 || n > count {
                bail!("there is no {n}: pick from 1 to {count}");
            }
            if used.contains(&(n - 1)) {
                bail!("{n} is in two tasks; each goes in one");
            }
            used.push(n - 1);
            items.push(n - 1);
        }
        groups.push(Group { items, name });
    }
    Ok(groups)
}

/// What importing came to.
#[derive(Default)]
struct Outcome {
    lines: Vec<String>,
    imported: usize,
    failed: usize,
    tasks: usize,
    /// Originals whose task holds a verified copy, for `--remove-originals`.
    originals: Vec<Original>,
}

/// A worktree or branch outside jeet whose work a task now has.
struct Original {
    /// The repository's git directory, where git can act on it.
    common: PathBuf,
    kind: OriginalKind,
    /// What was copied: HEAD, and the snapshot of its uncommitted changes.
    /// Removal checks the original still holds exactly this.
    head: String,
    snapshot: Option<String>,
}

enum OriginalKind {
    Worktree(PathBuf),
    Branch(String),
}

impl Original {
    fn describe(&self) -> String {
        match &self.kind {
            OriginalKind::Worktree(path) => format!("worktree {}", path.display()),
            OriginalKind::Branch(branch) => {
                format!("branch {branch} in {}", self.common.display())
            }
        }
    }

    /// Remove the original, unless it has changed since it was copied:
    /// whatever is new in it would go with it.
    fn remove(&self) -> Result<()> {
        match &self.kind {
            OriginalKind::Worktree(path) => {
                let (_, ignored) = git::status_counts(path)?;
                let now = git::snapshot_changes(path)?;
                let same = match (&self.snapshot, &now) {
                    (None, None) => true,
                    (Some(then), Some(now)) => same_changes(&self.common, then, now),
                    _ => false,
                };
                if !same || ignored > 0 || git::rev_parse(path, "HEAD") != Some(self.head.clone()) {
                    bail!("it has changed since it was copied; left in place");
                }
                git::worktree_remove(&self.common, path, true)
            }
            OriginalKind::Branch(branch) => {
                let tip = git::rev_parse(&self.common, &format!("refs/heads/{branch}"));
                if tip.as_deref() != Some(self.head.as_str()) {
                    bail!("it has moved since it was copied; left in place");
                }
                git::delete_local_branch(&self.common, branch)
            }
        }
    }
}

/// Bring the repositories into the store, then make the tasks.
fn execute(app: &App, plan: &Plan, groups: &[Group]) -> Outcome {
    let mut outcome = Outcome::default();

    // Store records by id: two clones of one repository share a trunk.
    let mut records: HashMap<String, Result<RepoRecord, String>> = HashMap::new();
    for repo in &plan.repos {
        if records.contains_key(&repo.id) {
            continue;
        }
        let record = ensure_in_store(app, repo).map_err(|e| format!("{e:#}"));
        match &record {
            Ok(record) if !repo.in_store => {
                outcome.lines.push(format!(
                    "{} is in the store at {}",
                    record.id, record.trunk_path
                ));
            }
            Ok(_) => {}
            Err(e) => outcome
                .lines
                .push(format!("could not bring {} into the store: {e}", repo.id)),
        }
        records.insert(repo.id.clone(), record);
    }

    for group in groups {
        let first = &plan.items[group.items[0]];
        let name = group.name.clone().unwrap_or_else(|| first.name.clone());
        let created = tasks::create(
            app,
            &NewTask {
                name: &name,
                project: None,
                repos: &[],
                branch: None,
            },
        );
        let task = match created {
            Ok(created) => created.task,
            Err(e) => {
                outcome
                    .lines
                    .push(format!("could not make task {name}: {e:#}"));
                outcome.failed += group.items.len();
                continue;
            }
        };
        let mut added = 0;
        for &index in &group.items {
            let item = &plan.items[index];
            let repo = &plan.repos[item.repo];
            let record = match records.get(&repo.id) {
                Some(Ok(record)) => record,
                _ => {
                    outcome.lines.push(format!(
                        "{}: not imported, as its repository is not in the store",
                        item.describe()
                    ));
                    outcome.failed += 1;
                    continue;
                }
            };
            let result = match &item.action {
                Action::Copy { .. } => import_copy(app, &task, record, repo, item),
                Action::Move { path, branch } => {
                    import_move(app, &task, record, path, branch.as_ref())
                }
            };
            match result {
                Ok(done) => {
                    added += 1;
                    outcome.imported += 1;
                    outcome.lines.push(format!(
                        "task {}: {} <- {}",
                        task.slug,
                        done.path.display(),
                        item.describe()
                    ));
                    outcome.lines.extend(
                        done.warnings
                            .into_iter()
                            .map(|w| format!("task {}: {w}", task.slug)),
                    );
                    outcome.originals.extend(done.original);
                }
                Err(e) => {
                    outcome.failed += 1;
                    outcome
                        .lines
                        .push(format!("{}: not imported: {e:#}", item.describe()));
                }
            }
        }
        if added == 0 {
            // Made a moment ago for this, and nothing came of it.
            if let Err(e) = tasks::delete(app, &task) {
                outcome.lines.push(format!(
                    "the empty task {} could not be removed ({e:#}); `jeet task delete {} \
                     --force` cleans it up",
                    task.slug, task.slug
                ));
            }
            continue;
        }
        outcome.tasks += 1;
    }
    outcome
}

/// The store's record for `repo`, cloning it from origin if need be.
///
/// A repository registered before tasks has its trunk outside the store; it
/// is cloned into the store and the index pointed there. Its original stays
/// where it is — and is imported from like any other — unless tasks already
/// have worktrees of it, which would be orphaned by the switch.
fn ensure_in_store(app: &App, repo: &FoundRepo) -> Result<RepoRecord> {
    let existing = app.db.get_repo(&repo.id)?;
    if let Some(record) = &existing {
        if Path::new(&record.trunk_path).starts_with(app.store_root())
            && Path::new(&record.trunk_path).is_dir()
        {
            return Ok(record.clone());
        }
        let in_use = app.db.task_worktrees_of_repo(&repo.id)?.len();
        if in_use > 0 {
            bail!(
                "{} task worktree{} use its trunk at {}; jeet leaves it there",
                in_use,
                if in_use == 1 { "" } else { "s" },
                record.trunk_path
            );
        }
    }
    let identity = remote::parse_remote_url_anyhow(&repo.origin)?;
    let trunk = crate::paths::trunk_path(&app.store_root(), &identity);
    if trunk.exists() {
        // A clone the index lost track of: take it back if it is this one.
        let origin = git::origin_url(&trunk)
            .with_context(|| format!("{} is in the way", trunk.display()))?;
        if remote::parse_remote_url_anyhow(&origin)?.id() != repo.id {
            bail!("{} holds another repository", trunk.display());
        }
        let record = RepoRecord {
            id: repo.id.clone(),
            trunk_path: trunk.to_string_lossy().to_string(),
            remote_url: origin,
            default_branch: git::default_branch(&trunk)?,
        };
        app.db.upsert_repo(&record)?;
        return Ok(record);
    }
    eprintln!("jeet: cloning {}…", repo.origin);
    clone::clone_into_store(app, &repo.origin)
}

/// An item now in a task.
struct Imported {
    path: PathBuf,
    warnings: Vec<String>,
    original: Option<Original>,
}

/// Copy an item's work into a new worktree of the store's trunk: the branch
/// (or commit) fetched across from the original, its uncommitted changes
/// reapplied on top. The original is only read.
fn import_copy(
    app: &App,
    task: &TaskRecord,
    record: &RepoRecord,
    repo: &FoundRepo,
    item: &Item,
) -> Result<Imported> {
    let Action::Copy {
        branch,
        head,
        checkout,
        linked,
        dirty,
        ..
    } = &item.action
    else {
        bail!("not a copy");
    };
    let trunk = PathBuf::from(&record.trunk_path);
    let source = checkout.clone().unwrap_or_else(|| repo.common.clone());
    let mut warnings = Vec::new();
    // Decided against the store's own default branch, now it is known.
    let branch = branch.as_deref().filter(|b| *b != record.default_branch);

    // Read first, so a checkout that cannot be saved costs nothing.
    let (snapshot, ignored) = match checkout {
        Some(path) if *dirty > 0 => (
            git::snapshot_changes(path).context("save its uncommitted changes")?,
            git::status_counts(path).map(|(_, i)| i).unwrap_or(0),
        ),
        Some(path) => (None, git::status_counts(path).map(|(_, i)| i).unwrap_or(0)),
        None => (None, 0),
    };

    // Into the trunk under names of our own; the branch under its own name.
    let scratch = format!("refs/jeet/import/{}", Uuid::new_v4());
    let mut specs = Vec::new();
    match branch {
        Some(branch) => specs.push(format!("refs/heads/{branch}:refs/heads/{branch}")),
        None => specs.push(format!("{head}:{scratch}/head")),
    }
    if let Some(snapshot) = &snapshot {
        specs.push(format!("{snapshot}:{scratch}/stash"));
    }
    {
        // A store sync fetching the same trunk would fight this for its refs.
        let _lock = store::lock_repo(&app.home, &record.id)?;
        let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
        git::fetch_local(&trunk, &source, &specs).context("copy it into the store")?;
    }
    let result = finish_copy(
        app,
        task,
        record,
        branch,
        head,
        snapshot.as_deref(),
        &mut warnings,
    );
    for name in git::refs_under(&trunk, &format!("{scratch}/")).unwrap_or_default() {
        let _ = git::delete_ref(&trunk, &name);
    }
    let (path, verified) = result?;

    if ignored > 0 {
        warnings.push(format!(
            "{} not copied (build output, .env and the like)",
            plural(ignored, "ignored file or directory")
        ));
    }
    if let Some(checkout) = checkout {
        copy_sessions(app, task, checkout, &mut warnings);
    }
    // Ignored files (.env, build output) were not copied, and removing the
    // original would delete them, so such a one stays.
    let original = match (verified && ignored == 0, checkout, linked, branch) {
        (true, Some(path), true, _) => Some(OriginalKind::Worktree(path.clone())),
        (true, None, _, Some(branch)) => Some(OriginalKind::Branch(branch.to_string())),
        _ => None,
    }
    .map(|kind| Original {
        common: repo.common.clone(),
        kind,
        head: head.clone(),
        snapshot: snapshot.clone(),
    });
    Ok(Imported {
        path,
        warnings,
        original,
    })
}

/// The worktree half of [`import_copy`], once everything it needs is in the
/// trunk. Returns the worktree, and whether it holds exactly what the
/// original did.
fn finish_copy(
    app: &App,
    task: &TaskRecord,
    record: &RepoRecord,
    branch: Option<&str>,
    head: &str,
    snapshot: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<(PathBuf, bool)> {
    let added = tasks::add_worktree(app, task, &record.id, branch, None)?;
    warnings.extend(added.warnings);
    let dir_name = added.record.dir_name;
    let path = added.path;
    match fill(
        &path, record, task, &dir_name, branch, head, snapshot, warnings,
    ) {
        Ok(verified) => Ok((path, verified)),
        Err(e) => {
            // Half made: take the worktree out of the task again.
            let _ = tasks::remove_worktree(app, task, &dir_name, true);
            Err(e)
        }
    }
}

/// Put the original's commit and changes into the new worktree at `path`.
/// Returns whether it now holds exactly what the original did.
#[allow(clippy::too_many_arguments)]
fn fill(
    path: &Path,
    record: &RepoRecord,
    task: &TaskRecord,
    dir_name: &str,
    branch: Option<&str>,
    head: &str,
    snapshot: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let trunk = PathBuf::from(&record.trunk_path);
    if branch.is_none() {
        git::checkout_detached(path, head).with_context(|| format!("check out {head}"))?;
    }
    let mut verified = git::rev_parse(path, "HEAD").as_deref() == Some(head);
    if !verified {
        warnings.push(format!(
            "the worktree is not at {} as the original is: the store's {} differs",
            short(head),
            branch.unwrap_or("copy")
        ));
    }
    let Some(snapshot) = snapshot else {
        return Ok(verified);
    };
    let applied =
        git::stash_apply(path, snapshot, true).or_else(|_| git::stash_apply(path, snapshot, false));
    match applied {
        Ok(()) => {
            // Only a byte-for-byte copy lets the original go.
            let copy = git::snapshot_changes(path).ok().flatten();
            verified &= copy.is_some_and(|copy| same_changes(&trunk, snapshot, &copy));
        }
        Err(e) => {
            verified = false;
            let at = tasks::keep(&trunk, &task.id, dir_name, "stash", snapshot)?;
            warnings.push(format!(
                "could not reapply the uncommitted changes ({e:#}); they are kept at {at}: \
                 `git stash apply {at}` in the worktree retries"
            ));
        }
    }
    Ok(verified)
}

/// Whether two stash-shaped commits hold the same files, tracked and
/// untracked.
fn same_changes(repo: &Path, a: &str, b: &str) -> bool {
    let trees = |c: &str| (git::tree_of(repo, c), git::tree_of(repo, &format!("{c}^3")));
    let first = trees(a);
    first.0.is_some() && first == trees(b)
}

/// Move a store trunk's worktree into the task, work and all.
fn import_move(
    app: &App,
    task: &TaskRecord,
    record: &RepoRecord,
    from: &Path,
    branch: Option<&String>,
) -> Result<Imported> {
    let trunk = PathBuf::from(&record.trunk_path);
    let dir = tasks::task_dir(app, task);
    let taken: Vec<String> = app
        .db
        .list_task_worktrees(&task.id)?
        .into_iter()
        .map(|r| r.dir_name)
        .collect();
    let dir_name = tasks::pick_dir_name(&dir, &taken, &tasks::repo_short_name(record));
    let dest = dir.join(&dir_name);
    git::worktree_move(&trunk, from, &dest).context("move the worktree")?;
    let row = TaskWorktreeRecord {
        id: 0,
        task_id: task.id.clone(),
        repo_id: record.id.clone(),
        dir_name,
        branch: branch.cloned(),
        // Whoever made it, it was not this task.
        created_branch: false,
        head_oid: None,
        stash_ref: None,
        pr_number: None,
        created_at: now_secs(),
    };
    if let Err(e) = app.db.insert_task_worktree(&row) {
        let _ = git::worktree_move(&trunk, &dest, from);
        return Err(e.context("record the worktree"));
    }
    worktrees::prune_empty_parents(from, &[app.worktrees_root(), app.ephemeral_root()]);
    let mut warnings = Vec::new();
    copy_sessions(app, task, from, &mut warnings);
    Ok(Imported {
        path: dest,
        warnings,
        original: None,
    })
}

/// Copy the Claude Code sessions started in `from` to the task, and record
/// them so the task can resume them. Best effort: a failure is a warning.
fn copy_sessions(app: &App, task: &TaskRecord, from: &Path, warnings: &mut Vec<String>) {
    let dir = tasks::task_dir(app, task);
    let copied = match agent::copy_claude_sessions(from, &dir) {
        Ok(copied) => copied,
        Err(e) => {
            warnings.push(format!("could not copy its Claude sessions ({e:#})"));
            return;
        }
    };
    for session in &copied {
        let row = TaskSessionRecord {
            session_id: session.id.clone(),
            task_id: task.id.clone(),
            parent_id: None,
            name: session.summary.clone(),
            created_at: session.modified,
            last_activity: session.modified,
        };
        if let Err(e) = app.db.upsert_task_session(&row) {
            warnings.push(format!("could not record session {} ({e:#})", session.id));
        }
    }
    if !copied.is_empty() {
        warnings.push(format!(
            "{} copied; `jeet task open {}` resumes the latest",
            plural(copied.len(), "Claude session"),
            task.slug
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_answer_takes_everything_one_task_each() {
        assert_eq!(parse_selection("", 3).unwrap(), each_alone(3));
        assert_eq!(parse_selection(" all ", 2).unwrap(), each_alone(2));
        assert!(parse_selection("none", 4).unwrap().is_empty());
    }

    #[test]
    fn plus_groups_items_and_equals_names_the_task() {
        let groups = parse_selection("1+3=login, 2 4", 4).unwrap();
        assert_eq!(
            groups,
            vec![
                Group {
                    items: vec![0, 2],
                    name: Some("login".into())
                },
                Group {
                    items: vec![1],
                    name: None
                },
                Group {
                    items: vec![3],
                    name: None
                },
            ]
        );
        // Leaving some out imports only the rest.
        assert_eq!(parse_selection("2", 4).unwrap().len(), 1);
    }

    #[test]
    fn a_selection_must_name_listed_items_once_each() {
        assert!(parse_selection("0", 3).is_err());
        assert!(parse_selection("4", 3).is_err());
        assert!(parse_selection("1 1", 3).is_err());
        assert!(parse_selection("1+2 2", 3).is_err());
        assert!(parse_selection("x", 3).is_err());
        assert!(parse_selection("1+=", 3).is_err());
        assert!(parse_selection("1=", 3).is_err());
    }

    #[test]
    fn work_is_changes_or_commits_nothing_else_has() {
        assert!(!has_work(0, Some(0)));
        assert!(has_work(2, Some(0)));
        assert!(has_work(0, Some(1)));
        // Unknown is not "nothing": it is offered rather than passed over.
        assert!(has_work(0, None));
    }

    #[test]
    fn a_task_is_named_after_its_branch_else_its_checkout() {
        let checkout = Path::new("/home/me/Projects/api-wip");
        assert_eq!(
            suggest_name(Some("feat/login"), Some(checkout), "github.com/acme/api"),
            "feat/login"
        );
        assert_eq!(
            suggest_name(None, Some(checkout), "github.com/acme/api"),
            "api-wip"
        );
        assert_eq!(suggest_name(None, None, "github.com/acme/api"), "api");
    }

    #[test]
    fn submodules_are_told_apart_from_linked_worktrees() {
        assert!(is_submodule(Path::new("/p/super/.git/modules/lib")));
        assert!(!is_submodule(Path::new("/p/api/.git")));
        assert!(!is_submodule(Path::new("/p/modules/.git")));
        assert!(!is_submodule(Path::new("/srv/repo.git")));
    }

    #[test]
    fn work_already_in_a_task_is_recognised_by_branch_or_commit() {
        let imported = vec![
            (
                Some("feat".to_string()),
                "aaa".to_string(),
                "feat".to_string(),
            ),
            (None, "bbb".to_string(), "scratch".to_string()),
        ];
        assert_eq!(
            already_imported(&imported, Some("feat"), "zzz"),
            Some("feat")
        );
        assert_eq!(already_imported(&imported, Some("other"), "aaa"), None);
        assert_eq!(already_imported(&imported, None, "bbb"), Some("scratch"));
        assert_eq!(already_imported(&imported, None, "ccc"), None);
    }

    #[test]
    fn checkouts_are_found_without_descending_into_them_or_jeets_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for path in [
            "a/.git",
            "a/nested/.git",
            "group/b/.git",
            "jeet/store/c/.git",
            "plain/src",
        ] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        // A linked worktree's `.git` is a file.
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/.git"), "gitdir: /elsewhere").unwrap();

        let found = find_checkouts(std::slice::from_ref(&root), &[root.join("jeet/store")]);
        assert_eq!(
            found,
            vec![root.join("a"), root.join("group/b"), root.join("wt")]
        );
    }
}
