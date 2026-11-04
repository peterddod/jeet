//! `plete migrate-home`: move the data home from `~/.jeet` to `~/.plete`.
//!
//! Until then an existing home stays where it is and only new items go to
//! `~/.plete` (see [`crate::config::default_homes`]). The move touches
//! everything that holds the old path: the index, the worktrees' links to
//! their repositories, Claude's per-folder session store, and the jobs that
//! run from the home. All the path logic takes its homes as parameters, and
//! the machine around it (tmux, launchd, the keyring) comes in through
//! [`Machine`], so the tests need none of them.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config;
use crate::git;
use crate::harness::claude::project_slug;
use crate::notify::secrets::{self, Keyring};

/// The places a move is between.
pub struct Homes {
    /// Where `~/.jeet` and `~/.plete` are.
    pub user_home: PathBuf,
    /// Claude Code's `projects` directory.
    pub claude_projects: PathBuf,
}

impl Homes {
    fn old_home(&self) -> PathBuf {
        self.user_home.join(config::LEGACY_HOME_DIR)
    }

    fn new_home(&self) -> PathBuf {
        self.user_home.join(config::NEW_HOME_DIR)
    }
}

/// The jobs that run from the home, as they were installed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Jobs {
    pub watch: bool,
    pub notify: bool,
    pub link: bool,
}

/// What a move needs of the machine around it.
pub trait Machine {
    /// The names of the agent sessions that are alive.
    fn live_sessions(&self) -> Vec<String>;
    /// Which jobs are installed.
    fn installed(&self) -> Jobs;
    /// Files to keep a copy of, besides the index and the config: the
    /// jobs' plists.
    fn files_to_back_up(&self) -> Vec<PathBuf>;
    /// Install `jobs` again, from the new home.
    fn reinstall(&self, jobs: Jobs) -> Result<()>;
    /// Keep the notification tokens kept for `old` under `new` too: they
    /// are per home.
    fn copy_tokens(&self, old: &Path, new: &Path) -> Result<()>;
}

/// How deep a top-level entry of the home merges into one already in the
/// new home, rather than clashing with it.
fn merge_depth(name: &str) -> usize {
    match name {
        // host/owner/repo
        "store" => 3,
        "tasks" | "projects" | "reviews" | "backups" => 1,
        _ => 0,
    }
}

/// Every move that takes `from` into `to`: the whole of it if `to` is not
/// there, else, up to `depth` levels, its children one by one. A name that
/// both have, deeper than that, is a clash and nothing is planned.
fn plan_entry(
    from: &Path,
    to: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    if to.symlink_metadata().is_err() {
        out.push((from.to_path_buf(), to.to_path_buf()));
        return Ok(());
    }
    if depth == 0 || !from.is_dir() || !to.is_dir() || from.is_symlink() || to.is_symlink() {
        bail!(
            "{} and {} both exist; move one aside and run this again",
            from.display(),
            to.display()
        );
    }
    for child in read_names(from)? {
        plan_entry(&from.join(&child), &to.join(&child), depth - 1, out)?;
    }
    Ok(())
}

fn read_names(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}

/// What moving the old home into the new one comes to: `(from, to)` for each
/// move, or the clash that stops it.
pub fn plan_moves(old: &Path, new: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut moves = Vec::new();
    for name in read_names(old)? {
        let depth = merge_depth(&name.to_string_lossy());
        plan_entry(&old.join(&name), &new.join(&name), depth, &mut moves)?;
    }
    Ok(moves)
}

/// Do the moves; if one fails, put back those already done.
fn apply_moves(moves: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (i, (from, to)) in moves.iter().enumerate() {
        let moved = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(from, to));
        if let Err(e) = moved {
            for (from, to) in moves[..i].iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            return Err(e).with_context(|| format!("move {} to {}", from.display(), to.display()));
        }
    }
    Ok(())
}

/// Remove `dir` and the directories in it, if there is nothing else in
/// them. Whether it is all gone.
fn remove_empty_tree(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            remove_empty_tree(&path);
        }
    }
    std::fs::remove_dir(dir).is_ok()
}

/// `path` with the `old` home at its front replaced by `new`.
fn rebase(path: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    path.strip_prefix(old).ok().map(|rest| new.join(rest))
}

/// In every text column of the index at `db_path`, put `new` where a path
/// starts with `old`. Returns how many values changed. All or nothing.
pub fn rewrite_index(db_path: &Path, old: &Path, new: &Path) -> Result<usize> {
    let mut conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("open {}", db_path.display()))?;
    let (old, new) = (old.to_string_lossy(), new.to_string_lossy());
    let tx = conn.transaction()?;
    let tables: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut changed = 0;
    for table in tables {
        let columns: Vec<String> = {
            let mut stmt = tx.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, ty)| ty.to_uppercase().contains("TEXT"))
                .map(|(name, _)| name)
                .collect()
        };
        for column in columns {
            changed += tx.execute(
                &format!(
                    "UPDATE \"{table}\" SET \"{column}\" = ?2 || substr(\"{column}\", length(?1) + 1) \
                     WHERE typeof(\"{column}\") = 'text' AND (\"{column}\" = ?1 \
                     OR substr(\"{column}\", 1, length(?1) + 1) = ?1 || '/')"
                ),
                [old.as_ref(), new.as_ref()],
            )?;
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// The trunks the index at `db_path` lists.
fn trunks(db_path: &Path) -> Result<Vec<PathBuf>> {
    let conn = rusqlite::Connection::open(db_path)?;
    let mut stmt = conn.prepare("SELECT trunk_path FROM repos")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

/// Tell the repository at `trunk` where its linked worktrees went. Git
/// lists them where it last knew them, under the old home. Returns how many
/// it was told of.
fn repair_trunk(trunk: &Path, old: &Path, new: &Path) -> Result<usize> {
    let moved: Vec<PathBuf> = git::worktree_list(trunk)?
        .into_iter()
        .skip(1)
        .map(|w| rebase(&w.path, old, new).unwrap_or(w.path))
        .filter(|p| p.exists())
        .collect();
    if !moved.is_empty() {
        git::worktree_repair(trunk, &moved)?;
    }
    Ok(moved.len())
}

/// Rename the folders of Claude Code's session store named for a path under
/// the old home to the name of the same path under the new one, so
/// `--resume` still finds its transcripts. Returns `(from, to)` for each.
pub fn rename_claude_projects(
    projects: &Path,
    old: &Path,
    new: &Path,
) -> Result<Vec<(String, String)>> {
    let (from, to) = (project_slug(old), project_slug(new));
    let Ok(entries) = std::fs::read_dir(projects) else {
        return Ok(Vec::new());
    };
    let mut renamed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(&from) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with('-')) {
            continue;
        }
        let target = format!("{to}{rest}");
        if projects.join(&target).exists() {
            continue;
        }
        std::fs::rename(entry.path(), projects.join(&target))
            .with_context(|| format!("rename {name} to {target}"))?;
        renamed.push((name, target));
    }
    renamed.sort();
    Ok(renamed)
}

/// Copy the files worth having back to `dir`: the index (and its write-ahead
/// files), the config and `extra`.
fn back_up(old: &Path, dir: &Path, extra: &[PathBuf]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let index = config::index_path(old);
    let mut files: Vec<PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| PathBuf::from(format!("{}{suffix}", index.display())))
        .collect();
    files.push(config::config_path(old));
    files.extend(extra.iter().cloned());
    for file in files.iter().filter(|f| f.is_file()) {
        let name = file.file_name().context("a file with no name")?;
        std::fs::copy(file, dir.join(name))
            .with_context(|| format!("copy {} to {}", file.display(), dir.display()))?;
    }
    Ok(())
}

/// Copy the tokens kept for each backend under `old` to `new`.
pub fn copy_tokens(keyring: &dyn Keyring, old: &Path, new: &Path) -> Result<()> {
    for backend in ["slack", "ntfy"] {
        let Some(secret) = keyring.get(&secrets::account(backend, old))? else {
            continue;
        };
        keyring.set(&secrets::account(backend, new), &secret)?;
    }
    Ok(())
}

/// What a finished move did.
#[derive(Debug)]
pub struct Done {
    pub moves: usize,
    pub rows: usize,
    pub worktrees: usize,
    pub projects: Vec<(String, String)>,
    pub backup: PathBuf,
    pub reinstalled: Jobs,
}

/// What may break, said before anything moves.
const RISKS: &str = "\
moving ~/.jeet to ~/.plete may break, for as long as it takes:
  - running agent sessions: their hooks and Claude's --resume key on the folder they started in
  - tmux sessions started in a task folder keep a working directory that has moved
  - the launchd jobs (watch, notify) and the link handler, which are installed again
  - open task lists (TUIs) and anything else holding files under ~/.jeet
~/.jeet is left as a link to ~/.plete, so a path that still names it keeps working.";

/// Move the old home into the new one. `confirmed` is `--yes`, `force` is
/// `--force` (go on with sessions alive). Without `confirmed`, only the
/// plan is returned, in the text. Refuses before it changes anything when
/// there is a clash or a session alive.
pub fn migrate(
    homes: &Homes,
    machine: &dyn Machine,
    confirmed: bool,
    force: bool,
    stamp: i64,
) -> Result<Option<Done>> {
    let (old, new) = (homes.old_home(), homes.new_home());
    if old.is_symlink() {
        bail!("{} is already a link; nothing to move", old.display());
    }
    if !config::index_path(&old).is_file() {
        bail!("{} has no index; nothing to move", old.display());
    }
    let moves = plan_moves(&old, &new)?;
    println!("{RISKS}\n");
    println!("{} entries move into {}:", moves.len(), new.display());
    for (from, _) in &moves {
        println!("  {}", from.display());
    }
    if !confirmed {
        println!("\nnothing moved; run `plete migrate-home --yes` to go ahead");
        return Ok(None);
    }
    let sessions = machine.live_sessions();
    if !sessions.is_empty() && !force {
        bail!(
            "agent sessions are alive ({}); stop them, or give --force to move anyway",
            sessions.join(", ")
        );
    }

    let jobs = machine.installed();
    let backup = new.join("backups").join(format!("migrate-{stamp}"));
    back_up(&old, &backup, &machine.files_to_back_up())?;
    println!(
        "\nbacked up the index, config and plists to {}",
        backup.display()
    );
    std::fs::create_dir_all(&new).with_context(|| format!("create {}", new.display()))?;
    apply_moves(&moves)?;

    let after = |step: &str, e: &anyhow::Error| {
        format!(
            "{step} failed after the files moved: {e:#}; the backup is in {}",
            backup.display()
        )
    };
    let index = config::index_path(&new);
    let rows = rewrite_index(&index, &old, &new)
        .map_err(|e| anyhow::anyhow!(after("rewriting the index", &e)))?;
    let mut worktrees = 0;
    for trunk in trunks(&index)? {
        match repair_trunk(&trunk, &old, &new) {
            Ok(n) => worktrees += n,
            Err(e) => eprintln!("plete: git worktree repair in {}: {e:#}", trunk.display()),
        }
    }
    let projects = rename_claude_projects(&homes.claude_projects, &old, &new)?;

    if !remove_empty_tree(&old) {
        bail!(
            "{} still holds files that did not move; it was not replaced by a link",
            old.display()
        );
    }
    std::os::unix::fs::symlink(&new, &old)
        .with_context(|| format!("link {} to {}", old.display(), new.display()))?;
    if let Err(e) = machine.copy_tokens(&old, &new) {
        eprintln!("plete: could not copy the notification tokens to the new home: {e:#}");
    }
    if let Err(e) = machine.reinstall(jobs) {
        eprintln!("plete: could not install the jobs again: {e:#}");
    }
    Ok(Some(Done {
        moves: moves.len(),
        rows,
        worktrees,
        projects,
        backup,
        reinstalled: jobs,
    }))
}

/// The machine this is run on.
struct Real;

impl Machine for Real {
    fn live_sessions(&self) -> Vec<String> {
        crate::tmux::list_sessions()
            .into_iter()
            .filter(|name| crate::tmux::is_ours(name))
            .collect()
    }

    fn installed(&self) -> Jobs {
        Jobs {
            watch: crate::watch::installed(),
            notify: crate::notify::sender::installed(),
            link: crate::link::handler::installed() || crate::link::handler::old_installed(),
        }
    }

    fn files_to_back_up(&self) -> Vec<PathBuf> {
        let labels = [
            crate::watch::LABEL,
            crate::watch::LEGACY_LABEL,
            crate::notify::sender::LABEL,
            crate::notify::sender::LEGACY_LABEL,
        ];
        labels
            .iter()
            .filter_map(|label| crate::watch::plist_path(label).ok())
            .collect()
    }

    fn reinstall(&self, jobs: Jobs) -> Result<()> {
        let app = crate::context::App::open()?;
        if jobs.watch {
            crate::watch::install(&app)?;
        }
        if jobs.notify {
            crate::notify::sender::install(&app.home)?;
        }
        if jobs.link {
            crate::link::handler::install(&app.home)?;
        }
        Ok(())
    }

    fn copy_tokens(&self, old: &Path, new: &Path) -> Result<()> {
        copy_tokens(&secrets::OsKeyring, old, new)
    }
}

/// `plete migrate-home`.
pub fn run(yes: bool, force: bool) -> Result<()> {
    for var in ["PLETE_HOME", "JEET_HOME"] {
        if std::env::var_os(var).is_some() {
            bail!("${var} is set: migrate-home moves the default home, ~/.jeet");
        }
    }
    let user_home = dirs::home_dir().context("could not determine home directory")?;
    let homes = Homes {
        claude_projects: crate::harness::claude::projects_dir()
            .unwrap_or_else(|| user_home.join(".claude/projects")),
        user_home,
    };
    let stamp = crate::worktrees::now_secs();
    let Some(done) = migrate(&homes, &Real, yes, force, stamp)? else {
        return Ok(());
    };
    println!(
        "moved {} entries, rewrote {} values in the index, repaired {} worktrees, renamed {} Claude project folders",
        done.moves,
        done.rows,
        done.worktrees,
        done.projects.len()
    );
    let j = done.reinstalled;
    for (installed, what) in [
        (j.watch, "the watcher"),
        (j.notify, "the notification sender"),
        (j.link, "the link handler"),
    ] {
        if installed {
            println!("installed {what} again");
        }
    }
    println!(
        "~/.jeet is now a link to ~/.plete; the backup is in {}",
        done.backup.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;

    /// A machine with nothing installed, and calls recorded.
    #[derive(Default)]
    struct Fake {
        sessions: Vec<String>,
        jobs: Jobs,
        reinstalled: RefCell<Option<Jobs>>,
    }

    impl Machine for Fake {
        fn live_sessions(&self) -> Vec<String> {
            self.sessions.clone()
        }
        fn installed(&self) -> Jobs {
            self.jobs
        }
        fn files_to_back_up(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn reinstall(&self, jobs: Jobs) -> Result<()> {
            *self.reinstalled.borrow_mut() = Some(jobs);
            Ok(())
        }
        fn copy_tokens(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A user home with an old home that has an index (a repo whose trunk
    /// is under it), a task directory with a linked worktree of that trunk,
    /// and a Claude project folder named for the task.
    struct Lab {
        dir: tempfile::TempDir,
    }

    impl Lab {
        fn new() -> Lab {
            let lab = Lab {
                dir: tempfile::tempdir().unwrap(),
            };
            let old = lab.dir.path().canonicalize().unwrap().join(".jeet");
            let trunk = old.join("store/github.com/acme/widget");
            std::fs::create_dir_all(&trunk).unwrap();
            git(&trunk, &["init", "-q", "-b", "main"]);
            git(&trunk, &["commit", "-q", "--allow-empty", "-m", "m"]);
            let worktree = old.join("tasks/fix/widget");
            std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
            git(
                &trunk,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "fix",
                    &worktree.to_string_lossy(),
                ],
            );
            std::fs::create_dir_all(old.join("tasks/fix/.jeet")).unwrap();
            std::fs::write(old.join("config.toml"), "# mine\n").unwrap();
            let db = crate::db::Database::open(&config::index_path(&old)).unwrap();
            db.upsert_repo(&crate::db::RepoRecord {
                id: "acme/widget".into(),
                trunk_path: trunk.to_string_lossy().into(),
                remote_url: "https://github.com/acme/widget".into(),
                default_branch: "main".into(),
            })
            .unwrap();
            drop(db);
            let slug = project_slug(&worktree);
            std::fs::create_dir_all(lab.projects().join(&slug)).unwrap();
            std::fs::write(lab.projects().join(&slug).join("s.jsonl"), "{}\n").unwrap();
            std::fs::create_dir_all(lab.projects().join("-elsewhere")).unwrap();
            lab
        }

        fn user_home(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }

        fn projects(&self) -> PathBuf {
            self.user_home().join(".claude/projects")
        }

        fn homes(&self) -> Homes {
            Homes {
                user_home: self.user_home(),
                claude_projects: self.projects(),
            }
        }
    }

    #[test]
    fn a_fake_old_home_is_migrated_whole() {
        let lab = Lab::new();
        let (old, new) = (
            lab.user_home().join(".jeet"),
            lab.user_home().join(".plete"),
        );
        let machine = Fake {
            jobs: Jobs {
                watch: true,
                ..Jobs::default()
            },
            ..Fake::default()
        };

        // Without --yes: the plan, and nothing moves.
        assert!(migrate(&lab.homes(), &machine, false, false, 7)
            .unwrap()
            .is_none());
        assert!(config::index_path(&old).is_file() && !new.exists());

        let done = migrate(&lab.homes(), &machine, true, false, 7)
            .unwrap()
            .unwrap();
        assert_eq!(
            done.reinstalled,
            *machine.reinstalled.borrow().as_ref().unwrap()
        );
        assert!(done.reinstalled.watch && !done.reinstalled.notify);

        // Everything is in the new home; the old is a link to it.
        assert!(old.is_symlink());
        assert_eq!(std::fs::read_link(&old).unwrap(), new);
        assert!(config::index_path(&new).is_file());
        assert!(new.join("tasks/fix/.jeet").is_dir());
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "# mine\n"
        );

        // The backup holds the index and the config.
        let backup = new.join("backups/migrate-7");
        assert!(backup.join("index.db").is_file() && backup.join("config.toml").is_file());

        // The index names the new trunk path, and git finds the worktree.
        let db = crate::db::Database::open(&config::index_path(&new)).unwrap();
        let repo = db.list_repos(None).unwrap().remove(0);
        let trunk = new.join("store/github.com/acme/widget");
        assert_eq!(repo.trunk_path, trunk.to_string_lossy());
        assert_eq!(done.rows, 1);
        assert_eq!(done.worktrees, 1);
        let listed = git::worktree_list(&trunk).unwrap();
        assert_eq!(listed[1].path, new.join("tasks/fix/widget"));
        let status = Command::new("git")
            .arg("-C")
            .arg(new.join("tasks/fix/widget"))
            .args(["status", "--short"])
            .output()
            .unwrap();
        assert!(status.status.success());

        // Claude's folder for the task is under the new name; others stay.
        let renamed = project_slug(&new.join("tasks/fix/widget"));
        assert!(lab.projects().join(&renamed).join("s.jsonl").is_file());
        assert!(lab.projects().join("-elsewhere").is_dir());
        assert_eq!(done.projects.len(), 1);

        // Done once: it will not run on the link.
        assert!(migrate(&lab.homes(), &machine, true, false, 8).is_err());
    }

    #[test]
    fn a_live_session_stops_it_unless_forced() {
        let lab = Lab::new();
        let machine = Fake {
            sessions: vec!["jeet-fix".into()],
            ..Fake::default()
        };
        let e = migrate(&lab.homes(), &machine, true, false, 1).unwrap_err();
        assert!(format!("{e}").contains("jeet-fix"), "{e}");
        assert!(!lab.user_home().join(".jeet").is_symlink());
        assert!(!lab.user_home().join(".plete").exists());
        assert!(migrate(&lab.homes(), &machine, true, true, 1)
            .unwrap()
            .is_some());
    }

    #[test]
    fn dirs_merge_into_the_new_home_and_a_clash_moves_nothing() {
        let lab = Lab::new();
        let (old, new) = (
            lab.user_home().join(".jeet"),
            lab.user_home().join(".plete"),
        );
        // The new home already has a task, a store host, and another repo.
        std::fs::create_dir_all(new.join("tasks/other")).unwrap();
        std::fs::create_dir_all(new.join("store/github.com/acme/gadget")).unwrap();
        let moves = plan_moves(&old, &new).unwrap();
        assert!(moves.contains(&(old.join("tasks/fix"), new.join("tasks/fix"))));
        assert!(moves.contains(&(
            old.join("store/github.com/acme/widget"),
            new.join("store/github.com/acme/widget")
        )));
        assert!(moves.contains(&(old.join("index.db"), new.join("index.db"))));

        // The same repo on both sides: abort, with nothing moved.
        std::fs::create_dir_all(new.join("store/github.com/acme/widget")).unwrap();
        let e = migrate(&lab.homes(), &Fake::default(), true, false, 1).unwrap_err();
        assert!(format!("{e}").contains("widget"), "{e}");
        assert!(config::index_path(&old).is_file());
        assert!(old.join("tasks/fix").is_dir());
        assert!(!new.join("index.db").exists());
    }

    #[test]
    fn only_paths_under_the_old_home_are_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("i.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE a (id INTEGER, p TEXT, q TEXT);
             INSERT INTO a VALUES (1, '/h/.jeet', 'x'), (2, '/h/.jeet/store/r', '/h/.jeetle'),
                                  (3, '/h/.jeet-other', '/elsewhere/h/.jeet');",
        )
        .unwrap();
        drop(conn);
        let n = rewrite_index(&db, Path::new("/h/.jeet"), Path::new("/h/.plete")).unwrap();
        assert_eq!(n, 2);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT p, q FROM a ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("/h/.plete".into(), "x".into()),
                ("/h/.plete/store/r".into(), "/h/.jeetle".into()),
                ("/h/.jeet-other".into(), "/elsewhere/h/.jeet".into()),
            ]
        );
    }

    #[test]
    fn tokens_are_copied_to_the_new_homes_account() {
        let keyring = secrets::MemKeyring::default();
        keyring
            .set(&secrets::account("slack", Path::new("/h/.jeet")), "xoxb-1")
            .unwrap();
        copy_tokens(&keyring, Path::new("/h/.jeet"), Path::new("/h/.plete")).unwrap();
        assert_eq!(
            keyring
                .get(&secrets::account("slack", Path::new("/h/.plete")))
                .unwrap()
                .as_deref(),
            Some("xoxb-1")
        );
        assert_eq!(
            keyring
                .get(&secrets::account("ntfy", Path::new("/h/.plete")))
                .unwrap(),
            None
        );
    }
}
