//! Keeping the canonical store fetched.
//!
//! Every indexed repo is fetched from origin, and a trunk in the store that
//! sits clean on its default branch is fast-forwarded to match, so a worktree
//! started from it starts from something recent. Commands that go to the
//! network anyway, and the explorer on launch, call [`maybe_sync_background`]:
//! at most once per `sync_interval_secs` it launches the hidden
//! `jeet store-sync` detached, and the command carries on without waiting.
//! `jeet sync` does the same work in the foreground.
//!
//! Two files under `$JEET_HOME` coordinate it: `.last-sync` holds when the
//! last sync started, and `.sync.lock` the pid of the one running now.
//!
//! A third kind, `.sync-locks/<repo>.lock`, serializes fetches of one repo.
//! Two fetches into the same repository race for its ref locks and one of
//! them fails, so anything that fetches a repo the store sync also fetches —
//! checking a pull request out, say — takes [`lock_repo`] for the duration.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use crate::context::App;
use crate::db::RepoRecord;
use crate::git;
use crate::review::process_alive;
use crate::worktrees::now_secs;

const LOCK_FILE: &str = ".sync.lock";
const LAST_SYNC_FILE: &str = ".last-sync";
const LOG_FILE: &str = ".sync.log";
const REPO_LOCK_DIR: &str = ".sync-locks";

/// Fetches are network-bound; a few at once is quicker without making a
/// store of fifty repos open fifty connections.
const POOL_SIZE: usize = 4;

/// A lock older than this is abandoned whatever its pid says: pids get
/// reused, and no sync legitimately runs for an hour.
const STALE_LOCK_SECS: u64 = 60 * 60;

/// How long a lock with no pid in it yet counts as held: the moment between
/// a sync creating the file and writing to it.
const UNWRITTEN_LOCK_SECS: u64 = 60;

/// How long `jeet sync` waits for a background sync to finish first, and
/// anything waits for another fetch of the same repo.
const LOCK_WAIT: Duration = Duration::from_secs(120);

/// What syncing one repo did, short of failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Fetched; the trunk already matched origin.
    UpToDate,
    /// Fetched, and the trunk moved forward between these commits.
    FastForwarded { from: String, to: String },
    /// Fetched only: the trunk is not jeet's to move — it lives outside the
    /// store, where it is somebody's working copy.
    Fetched,
    /// Fetched, but the trunk was left where it is, for this reason.
    Held(String),
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::UpToDate => "up to date".to_string(),
            Outcome::FastForwarded { from, to } => {
                format!("fast-forwarded {}..{}", short(from), short(to))
            }
            Outcome::Fetched => "fetched".to_string(),
            Outcome::Held(why) => format!("fetched, but {why}"),
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Fetch `repo` and, if its trunk is in the store and clean on the default
/// branch, fast-forward it to origin.
///
/// Anything else about the trunk — another branch checked out, uncommitted
/// changes, commits origin does not have — is reported as [`Outcome::Held`]
/// rather than repaired: those are someone's work, and jeet does not move it.
pub fn sync_repo(app: &App, repo: &RepoRecord) -> Result<Outcome> {
    let trunk = Path::new(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("trunk is missing: {}", trunk.display());
    }
    let _repo_lock = lock_repo(&app.home, &repo.id)?;
    git::fetch_prune(trunk)?;
    if !in_store(&app.store_root(), trunk) {
        return Ok(Outcome::Fetched);
    }

    let default = &repo.default_branch;
    let upstream = format!("origin/{default}");
    let Some(target) = git::rev_parse(trunk, &format!("refs/remotes/{upstream}")) else {
        return Ok(Outcome::Held(format!("origin has no {default} branch")));
    };
    match git::head_branch(trunk) {
        Some(branch) if &branch == default => {}
        Some(branch) => {
            return Ok(Outcome::Held(format!(
                "the trunk is on {branch}, not {default}"
            )))
        }
        None => {
            return Ok(Outcome::Held(format!(
                "the trunk is detached, not on {default}"
            )))
        }
    }
    let (dirty, _) = git::status_counts(trunk)?;
    if dirty > 0 {
        return Ok(Outcome::Held(
            "the trunk has uncommitted changes".to_string(),
        ));
    }
    let Some(head) = git::rev_parse(trunk, "HEAD") else {
        return Ok(Outcome::Held("the trunk has no commits".to_string()));
    };
    if head == target {
        return Ok(Outcome::UpToDate);
    }
    if !git::is_ancestor(trunk, &head, &target) {
        return Ok(Outcome::Held(format!(
            "the trunk has commits {upstream} does not"
        )));
    }
    git::merge_ff_only(trunk, &upstream)?;
    Ok(Outcome::FastForwarded {
        from: head,
        to: target,
    })
}

/// Paths compare as given: both come from the index and the canonicalized
/// home, so a trunk in the store always shares the store root's spelling.
fn in_store(store_root: &Path, trunk: &Path) -> bool {
    trunk.starts_with(store_root)
}

/// One line on stderr for a repo's sync.
pub fn report(repo: &RepoRecord, result: &Result<Outcome>) {
    match result {
        Ok(Outcome::Held(why)) => {
            eprintln!("jeet: warning: {}: {why}; not fast-forwarded", repo.id)
        }
        Ok(outcome) => eprintln!("jeet: {}: {}", repo.id, outcome.describe()),
        Err(e) => eprintln!("jeet: {}: sync failed: {e:#}", repo.id),
    }
}

/// Sync every indexed repo, a few at a time, reporting each as it finishes.
pub fn sync_all(app: &App) -> Result<Vec<(RepoRecord, Result<Outcome>)>> {
    let repos = app.db.list_repos(None)?;
    let next = AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::with_capacity(repos.len()));
    std::thread::scope(|scope| {
        for _ in 0..POOL_SIZE.min(repos.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(repo) = repos.get(i) else {
                    break;
                };
                let result = sync_repo(app, repo);
                report(repo, &result);
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((i, repo.clone(), result));
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(i, _, _)| *i);
    Ok(results.into_iter().map(|(_, r, res)| (r, res)).collect())
}

/// Whether a background sync is due: one has never run, or the last started
/// at least `interval` seconds ago. An interval of zero turns them off. A
/// last sync in the future means the clock moved; sync rather than wait it out.
pub fn is_due(last: Option<i64>, now: i64, interval: u64) -> bool {
    if interval == 0 {
        return false;
    }
    match last {
        None => true,
        Some(last) if last > now => true,
        Some(last) => (now - last) as u64 >= interval,
    }
}

fn last_sync_path(home: &Path) -> PathBuf {
    home.join(LAST_SYNC_FILE)
}

fn lock_path(home: &Path) -> PathBuf {
    home.join(LOCK_FILE)
}

/// One repo's fetch lock. The id is flattened to a single file name; two ids
/// that flatten alike only share a lock, which costs a wait, never a clash.
fn repo_lock_path(home: &Path, repo_id: &str) -> PathBuf {
    let name: String = repo_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect();
    home.join(REPO_LOCK_DIR).join(format!("{name}.lock"))
}

pub fn last_sync(home: &Path) -> Option<i64> {
    std::fs::read_to_string(last_sync_path(home))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Note that a sync started at `at`. Through a temporary file so a reader
/// never sees half a number.
pub fn record_sync(home: &Path, at: i64) -> Result<()> {
    let path = last_sync_path(home);
    let tmp = home.join(format!("{LAST_SYNC_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{at}\n")).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

/// The sync lock, released when dropped.
pub struct SyncLock {
    path: PathBuf,
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Who holds the lock, if anyone still does. A lock whose process is gone,
/// or that is older than any sync could be, is abandoned and counts as free.
pub fn lock_holder(home: &Path) -> Option<u32> {
    holder_at(&lock_path(home))
}

fn holder_at(path: &Path) -> Option<u32> {
    let age = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default()
        .as_secs();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    holder_from(&text, age, process_alive)
}

/// [`lock_holder`]'s judgement, given the lock's contents and age. A lock
/// with no pid yet is held by a sync that has just made it — 0 standing in
/// for that unknown pid — until it is too old for that to be true.
fn holder_from(text: &str, age_secs: u64, alive: impl Fn(u32) -> bool) -> Option<u32> {
    if age_secs >= STALE_LOCK_SECS {
        return None;
    }
    match text.trim().parse::<u32>() {
        Ok(pid) if alive(pid) => Some(pid),
        Ok(_) => None,
        Err(_) if age_secs < UNWRITTEN_LOCK_SECS => Some(0),
        Err(_) => None,
    }
}

/// Take the sync lock, or `None` if a live sync holds it.
pub fn try_lock(home: &Path) -> Result<Option<SyncLock>> {
    try_lock_at(&lock_path(home))
}

fn try_lock_at(path: &Path) -> Result<Option<SyncLock>> {
    // Twice at most: once as found, once more after clearing an abandoned lock.
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let lock = SyncLock {
                    path: path.to_path_buf(),
                };
                writeln!(file, "{}", std::process::id())
                    .with_context(|| format!("write {}", path.display()))?;
                return Ok(Some(lock));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if holder_at(path).is_some() {
                    return Ok(None);
                }
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
                }
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
    Ok(None)
}

/// Take `repo_id`'s fetch lock, waiting for whoever is fetching it now.
///
/// Silent while it waits — the explorer calls this behind its own progress
/// line, where stderr would land on the screen — and it waits for one repo's
/// fetch, which is short.
pub fn lock_repo(home: &Path, repo_id: &str) -> Result<SyncLock> {
    let path = repo_lock_path(home, repo_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let started = Instant::now();
    loop {
        if let Some(lock) = try_lock_at(&path)? {
            return Ok(lock);
        }
        if started.elapsed() >= LOCK_WAIT {
            bail!(
                "{repo_id} is still being fetched; remove {} if it is not",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Take the lock, waiting a while for a running sync to finish first.
fn wait_for_lock(home: &Path) -> Result<SyncLock> {
    let started = Instant::now();
    let mut said = false;
    loop {
        if let Some(lock) = try_lock(home)? {
            return Ok(lock);
        }
        if started.elapsed() >= LOCK_WAIT {
            bail!(
                "another sync is still running; remove {} if it is not",
                lock_path(home).display()
            );
        }
        if !said {
            match lock_holder(home) {
                Some(pid) if pid != 0 => {
                    eprintln!("jeet: waiting for the sync already running (pid {pid})")
                }
                _ => eprintln!("jeet: waiting for the sync already running"),
            }
            said = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Sync the whole store now, in the foreground. Returns how many repos
/// failed, having reported each.
pub fn sync_now(app: &App) -> Result<(usize, usize)> {
    let _lock = wait_for_lock(&app.home)?;
    record_sync(&app.home, now_secs())?;
    let results = sync_all(app)?;
    let failed = results.iter().filter(|(_, r)| r.is_err()).count();
    Ok((failed, results.len()))
}

/// `jeet store-sync`: the background sync. Quietly does nothing when another
/// sync is already running — two launches racing is expected, not an error.
pub fn run_background(app: &App) -> Result<()> {
    let Some(_lock) = try_lock(&app.home)? else {
        eprintln!("jeet: another sync is running");
        return Ok(());
    };
    record_sync(&app.home, now_secs())?;
    eprintln!("jeet: store sync started");
    let results = sync_all(app)?;
    let failed = results.iter().filter(|(_, r)| r.is_err()).count();
    eprintln!(
        "jeet: store sync finished: {} synced, {failed} failed",
        results.len() - failed
    );
    Ok(())
}

/// Start a background sync if one is due and none is running.
///
/// Best-effort and quick: it reads two small files and maybe spawns a
/// process, so it is safe to call on the way into the explorer. Failing to
/// start is worth a warning, never an error — the command the user ran is
/// what matters.
pub fn maybe_sync_background(app: &App) {
    let interval = app.config.sync_interval_secs();
    let now = now_secs();
    if !is_due(last_sync(&app.home), now, interval) || lock_holder(&app.home).is_some() {
        return;
    }
    match app.db.list_repos(None) {
        Ok(repos) if !repos.is_empty() => {}
        _ => return,
    }
    // Claim this interval before the runner has started and taken the lock,
    // so commands run in quick succession launch one sync between them.
    if let Err(e) = record_sync(&app.home, now) {
        eprintln!("jeet: could not start a background sync: {e:#}");
        return;
    }
    if let Err(e) = launch(&app.home) {
        eprintln!("jeet: could not start a background sync: {e:#}");
    }
}

/// Launch `jeet store-sync` detached, as `review::launch` does a review: under
/// `nohup` so closing the terminal does not kill it, in its own process group
/// so a ctrl-c meant for the explorer does not either.
fn launch(home: &Path) -> Result<()> {
    let log = home.join(LOG_FILE);
    let out = File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let mut cmd = Command::new("nohup");
    cmd.arg(exe)
        .arg("store-sync")
        .env("JEET_HOME", home)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("start jeet store-sync")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_is_due_once_the_interval_has_passed() {
        assert!(is_due(None, 1000, 300));
        assert!(!is_due(Some(900), 1000, 300));
        assert!(is_due(Some(700), 1000, 300));
        assert!(is_due(Some(600), 1000, 300));
        // A clock that went backwards does not stall syncing until it catches up.
        assert!(is_due(Some(5000), 1000, 300));
        // Zero turns background syncing off altogether.
        assert!(!is_due(None, 1000, 0));
        assert!(!is_due(Some(0), 1000, 0));
    }

    #[test]
    fn an_abandoned_lock_counts_as_free() {
        let alive = |pid: u32| pid == 42;
        assert_eq!(holder_from("42\n", 10, alive), Some(42));
        assert_eq!(holder_from("43\n", 10, alive), None, "its process is gone");
        assert_eq!(
            holder_from("42\n", STALE_LOCK_SECS, alive),
            None,
            "too old to be real"
        );
        assert_eq!(holder_from("", 1, alive), Some(0), "being written");
        assert_eq!(holder_from("", UNWRITTEN_LOCK_SECS, alive), None);
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let home = tempfile::tempdir().unwrap();
        let lock = try_lock(home.path()).unwrap().expect("free at first");
        assert_eq!(lock_holder(home.path()), Some(std::process::id()));
        assert!(try_lock(home.path()).unwrap().is_none(), "held");
        drop(lock);
        assert!(!lock_path(home.path()).exists());
        assert!(try_lock(home.path()).unwrap().is_some());
    }

    #[test]
    fn a_dead_holders_lock_is_taken_over() {
        let home = tempfile::tempdir().unwrap();
        // No pid is this large on any system jeet runs on.
        std::fs::write(lock_path(home.path()), "4294967294\n").unwrap();
        assert!(lock_holder(home.path()).is_none());
        let lock = try_lock(home.path()).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock_holder(home.path()), Some(std::process::id()));
    }

    #[test]
    fn repo_locks_are_per_repo_and_exclusive() {
        let home = tempfile::tempdir().unwrap();
        let widget = lock_repo(home.path(), "github.com/acme/widget").unwrap();
        let path = repo_lock_path(home.path(), "github.com/acme/widget");
        assert_eq!(
            path.file_name().unwrap(),
            "github.com_acme_widget.lock",
            "one flat file under the lock dir"
        );
        assert_eq!(holder_at(&path), Some(std::process::id()));
        assert!(try_lock_at(&path).unwrap().is_none(), "held");
        // Another repo, and the store lock, are not held up by it.
        let _gadget = lock_repo(home.path(), "github.com/acme/gadget").unwrap();
        assert!(try_lock(home.path()).unwrap().is_some());
        drop(widget);
        assert!(!path.exists());
        assert!(lock_repo(home.path(), "github.com/acme/widget").is_ok());
    }

    #[test]
    fn a_repo_lock_waits_for_its_holder() {
        let home = tempfile::tempdir().unwrap();
        let held = lock_repo(home.path(), "acme/widget").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = home.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            let lock = lock_repo(&dir, "acme/widget").unwrap();
            tx.send(()).unwrap();
            drop(lock);
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "took a lock that was held"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("never took the lock once it was free");
        waiter.join().unwrap();
    }

    #[test]
    fn the_last_sync_round_trips() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(last_sync(home.path()), None);
        record_sync(home.path(), 1234).unwrap();
        assert_eq!(last_sync(home.path()), Some(1234));
        std::fs::write(last_sync_path(home.path()), "garbage").unwrap();
        assert_eq!(last_sync(home.path()), None);
    }

    #[test]
    fn only_trunks_under_the_store_are_moved() {
        let store = Path::new("/home/.jeet/store");
        assert!(in_store(
            store,
            Path::new("/home/.jeet/store/github.com/a/b")
        ));
        assert!(!in_store(store, Path::new("/home/me/Projects/b")));
        // A sibling that merely shares the prefix as text is not inside.
        assert!(!in_store(store, Path::new("/home/.jeet/store-old/a/b")));
    }

    #[test]
    fn outcomes_describe_themselves_briefly() {
        let ff = Outcome::FastForwarded {
            from: "0123456789abcdef".into(),
            to: "fedcba9876543210".into(),
        };
        assert_eq!(ff.describe(), "fast-forwarded 0123456..fedcba9");
        assert_eq!(
            Outcome::Held("the trunk is on x, not main".into()).describe(),
            "fetched, but the trunk is on x, not main"
        );
    }
}
