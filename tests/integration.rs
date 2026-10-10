use std::process::Command;

use tempfile::TempDir;

fn jeet_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jeet"));
    // jeet shells out to git, so the developer's global config would otherwise
    // reach in — commit.gpgsign alone fails most of this suite.
    hermetic_git(&mut cmd);
    // A background store sync would outlive the test and race its temp dirs;
    // the tests that want one turn it back on.
    cmd.env("JEET_SYNC_INTERVAL_SECS", "0");
    // Running the suite from inside a task's agent session must not point
    // `jeet worktree` at that real task, or the shell wrapper's cd file.
    cmd.env_remove("JEET_TASK_ID").env_remove("JEET_CD_FILE");
    cmd
}

/// Detach a command from the ambient git configuration.
fn hermetic_git(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
}

fn init_repo_with_remote(path: &std::path::Path, remote: &str) {
    init_repo_on_branch(path, remote, "main");
}

fn init_repo_on_branch(path: &std::path::Path, remote: &str, branch: &str) {
    git(&["init", "-b", branch], path);
    git(&["remote", "add", "origin", remote], path);
    // Nothing here should ever reach the network: if a code path does try the
    // https origin, fail immediately rather than hanging or hitting GitHub.
    git(&["config", "http.proxy", "127.0.0.1:1"], path);
    git(&["commit", "--allow-empty", "-m", "init"], path);
}

#[test]
fn path_resolves_after_adopt() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();

    let repo_dir = TempDir::new().unwrap();
    init_repo_with_remote(repo_dir.path(), "https://github.com/acme/widget.git");

    let output = jeet_bin()
        .args(["adopt", repo_dir.path().to_str().unwrap()])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success(), "adopt failed");

    let output = jeet_bin()
        .args(["path", "acme/widget"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        path,
        repo_dir.path().canonicalize().unwrap().to_string_lossy()
    );
}

#[test]
fn ls_after_adopt_and_scan() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();

    let scan_root = home.path().join("scan");
    std::fs::create_dir_all(&scan_root).unwrap();
    let repo_dir = scan_root.join("demo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_repo_with_remote(&repo_dir, "https://github.com/acme/scanned.git");

    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "scan_roots = [\"{}\"]\n",
            scan_root.to_string_lossy().replace('\\', "\\\\")
        ),
    )
    .unwrap();

    let output = jeet_bin()
        .args(["scan"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = jeet_bin()
        .args(["ls", "scanned"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("acme/scanned"));
}

#[test]
fn complete_repos_lists_adopted() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();

    let repo_dir = TempDir::new().unwrap();
    init_repo_with_remote(repo_dir.path(), "https://github.com/acme/complete.git");

    jeet_bin()
        .args(["adopt", repo_dir.path().to_str().unwrap()])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();

    let output = jeet_bin()
        .args(["complete", "repos"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("acme/complete"));
}

#[test]
fn completions_zsh_generates_dynamic() {
    let output = jeet_bin().args(["completions", "zsh"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("_clap_dynamic_completer_jeet"));
    assert!(stdout.contains("COMPLETE"));
}

#[test]
fn dynamic_completion_lists_repos_for_exec() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();
    let repo_dir = TempDir::new().unwrap();
    init_repo_with_remote(repo_dir.path(), "https://github.com/acme/dynamic.git");

    jeet_bin()
        .args(["adopt", repo_dir.path().to_str().unwrap()])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();

    let output = jeet_bin()
        .env("JEET_HOME", &env_home)
        .env("COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .env("_CLAP_IFS", "\n")
        .args(["--", "jeet", "exec", ""])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("acme/dynamic"));
}

#[test]
fn init_shell_includes_completion() {
    let output = jeet_bin().args(["init-shell"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("jeet()"));
    assert!(stdout.contains("_JEET_BIN"));
    assert!(stdout.contains("checkout"));
    assert!(stdout.contains("_jeet_wrapper"));
    assert!(stdout.contains("_clap_dynamic_completer_jeet"));
    assert!(
        !stdout.contains("source <"),
        "init-shell should inline completions, not use source"
    );
}

#[test]
fn cd_subcommand_not_in_binary() {
    let output = jeet_bin().args(["cd", "acme/widget"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cd") || stderr.contains("subcommand"));
}

#[test]
fn ephemeral_exec_cleans_up_and_warns_when_dirty() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();
    let ephemeral_root = home.path().join("ephemeral");

    let repo_dir = TempDir::new().unwrap();
    init_repo_with_remote(repo_dir.path(), "https://github.com/acme/ephemeral.git");

    jeet_bin()
        .args(["adopt", repo_dir.path().to_str().unwrap()])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();

    let output = jeet_bin()
        .args(["exec", "acme/ephemeral", "--ephemeral"])
        .env("JEET_HOME", &env_home)
        .env("JEET_SHELL", "/bin/sh")
        .env("JEET_EXEC_INIT", "touch dirty-file && exit 0")
        .output()
        .unwrap();

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success(), "ephemeral exec failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("uncommitted changes"));

    let remaining: Vec<_> = std::fs::read_dir(&ephemeral_root)
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        remaining.is_empty(),
        "ephemeral directory should be cleaned up"
    );
}

#[test]
fn list_subcommand_removed() {
    let output = jeet_bin().args(["list"]).output().unwrap();
    assert!(!output.status.success());
}

// ---------------------------------------------------------------------------
// explorer, sessions and shell integration
// ---------------------------------------------------------------------------

fn git(args: &[&str], cwd: &std::path::Path) {
    let mut cmd = Command::new("git");
    hermetic_git(&mut cmd);
    let status = cmd.args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// Read a path a jeet command printed, checking it really is one.
fn path_from(output: &std::process::Output) -> std::path::PathBuf {
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !text.contains('\n') && std::path::Path::new(&text).is_absolute(),
        "expected a single absolute path on stdout, got {text:?}"
    );
    std::path::PathBuf::from(text)
}

/// A repo whose origin *looks* like GitHub but pushes to a local bare repo, so
/// tests exercise the publish path without touching the network.
struct Lab {
    _home: TempDir,
    _repo: TempDir,
    _bare: TempDir,
    home: String,
    repo: std::path::PathBuf,
}

fn lab(name: &str) -> Lab {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let bare = TempDir::new().unwrap();

    git(&["init", "--bare", "-q"], bare.path());
    init_repo_with_remote(repo.path(), &format!("https://github.com/acme/{name}.git"));
    // Push stays local; the https URL is only an identity.
    git(
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            &bare.path().to_string_lossy(),
        ],
        repo.path(),
    );

    let home_str = home.path().to_string_lossy().to_string();
    let output = jeet_bin()
        .args(["adopt", repo.path().to_str().unwrap()])
        .env("JEET_HOME", &home_str)
        .output()
        .unwrap();
    assert!(output.status.success(), "adopt failed");

    Lab {
        home: home_str,
        repo: repo.path().canonicalize().unwrap(),
        _home: home,
        _repo: repo,
        _bare: bare,
    }
}

impl Lab {
    fn jeet(&self, args: &[&str], cwd: &std::path::Path) -> std::process::Output {
        jeet_bin()
            .args(args)
            .current_dir(cwd)
            .env("JEET_HOME", &self.home)
            .output()
            .unwrap()
    }
}

#[test]
fn explorer_needs_a_terminal() {
    let lab = lab("explorer");
    for args in [vec![], vec!["explore"]] {
        let output = lab.jeet(&args, &lab.repo);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("interactive terminal"), "{stderr}");
    }
}

#[test]
fn sessions_lists_recorded_agent_transcripts() {
    let lab = lab("sessions");
    let fake_home = TempDir::new().unwrap();
    let slug: String = lab
        .repo
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = fake_home.path().join(".claude").join("projects").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("0f0f0f0f-1111-2222-3333-444444444444.jsonl"),
        "{\"type\":\"user\",\"message\":{\"content\":[{\"text\":\"wire up the parser\"}]}}\n",
    )
    .unwrap();

    let output = jeet_bin()
        .args(["sessions"])
        .current_dir(&lab.repo)
        .env("JEET_HOME", &lab.home)
        .env("HOME", fake_home.path())
        // Pin both, so the caller's own JEET_AGENT/CLAUDE_CONFIG_DIR cannot
        // steer the test — or worse, point it at their real session store.
        .env("JEET_AGENT", "claude")
        .env("CLAUDE_CONFIG_DIR", fake_home.path().join(".claude"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wire up the parser"), "{stdout}");
    assert!(
        stdout.contains("0f0f0f0f-1111-2222-3333-444444444444"),
        "{stdout}"
    );
}

#[test]
fn init_shell_wires_up_the_cd_handoff() {
    let output = jeet_bin().args(["init-shell"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("JEET_CD_FILE"));
    assert!(stdout.contains("builtin cd --"));
}

// ---------------------------------------------------------------------------
// store sync
// ---------------------------------------------------------------------------

/// A store with real clones whose origins are local bare repos: git rewrites
/// `https://github.com/` to a directory, so jeet sees GitHub URLs and the
/// tests never leave the machine.
struct StoreLab {
    home: TempDir,
    origins: TempDir,
    seeds: TempDir,
}

impl StoreLab {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            origins: TempDir::new().unwrap(),
            seeds: TempDir::new().unwrap(),
        }
    }

    /// Point `cmd` at this lab: its home, and its stand-in for GitHub.
    fn wire<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let base = format!(
            "file://{}/",
            self.origins.path().canonicalize().unwrap().display()
        );
        cmd.env("JEET_HOME", self.home.path())
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("url.{base}.insteadOf"))
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
    }

    fn jeet(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = jeet_bin();
        self.wire(&mut cmd).args(args).output().unwrap()
    }

    /// A GitHub-looking origin with one commit on main.
    fn origin(&self, name: &str) {
        let bare = self.origins.path().join("acme").join(format!("{name}.git"));
        std::fs::create_dir_all(&bare).unwrap();
        git(&["init", "--bare", "-q", "-b", "main"], &bare);
        let seed = self.seed(name);
        std::fs::create_dir_all(&seed).unwrap();
        git(&["init", "-q", "-b", "main"], &seed);
        git(&["remote", "add", "origin", &bare.to_string_lossy()], &seed);
        self.commit(name, "init");
    }

    /// `jeet clone` of `name`, with background syncing set to `interval`.
    fn clone_with_interval(&self, name: &str, interval: &str) -> std::process::Output {
        let mut cmd = jeet_bin();
        let url = format!("https://github.com/acme/{name}.git");
        self.wire(&mut cmd)
            .env("JEET_SYNC_INTERVAL_SECS", interval)
            .args(["clone", &url])
            .output()
            .unwrap()
    }

    /// A new origin, cloned into the store. Returns the store trunk.
    fn clone_repo(&self, name: &str) -> std::path::PathBuf {
        self.origin(name);
        let output = self.clone_with_interval(name, "0");
        assert!(
            output.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.home().join("store/github.com/acme").join(name)
    }

    fn home(&self) -> std::path::PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    fn seed(&self, name: &str) -> std::path::PathBuf {
        self.seeds.path().join(name)
    }

    /// Land a commit on origin's main, as if someone else had pushed it.
    fn commit(&self, name: &str, message: &str) -> String {
        let seed = self.seed(name);
        git(&["commit", "-q", "--allow-empty", "-m", message], &seed);
        git(&["push", "-q", "origin", "main"], &seed);
        rev(&seed, "HEAD")
    }
}

fn rev(dir: &std::path::Path, rev: &str) -> String {
    let mut cmd = Command::new("git");
    hermetic_git(&mut cmd);
    let output = cmd
        .args(["rev-parse", rev])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "rev-parse {rev} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn sync_fast_forwards_a_clean_trunk() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let pushed = lab.commit("widget", "upstream work");
    assert_ne!(rev(&trunk, "HEAD"), pushed);

    let output = lab.jeet(&["sync"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "sync failed: {stderr}");
    assert!(output.stdout.is_empty(), "sync wrote to stdout");
    assert!(stderr.contains("fast-forwarded"), "{stderr}");
    assert_eq!(rev(&trunk, "HEAD"), pushed);

    // Nothing new: says so, and moves nothing.
    let output = lab.jeet(&["sync", "--repo", "widget"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("up to date"));
    assert_eq!(rev(&trunk, "HEAD"), pushed);
}

#[test]
fn sync_of_one_repo_leaves_the_others() {
    let lab = StoreLab::new();
    let widget = lab.clone_repo("widget");
    let gadget = lab.clone_repo("gadget");
    let widget_new = lab.commit("widget", "w");
    let gadget_before = rev(&gadget, "HEAD");
    lab.commit("gadget", "g");

    let output = lab.jeet(&["sync", "--repo", "acme/widget"]);
    assert!(output.status.success());
    assert_eq!(rev(&widget, "HEAD"), widget_new);
    assert_eq!(rev(&gadget, "HEAD"), gadget_before);
}

#[test]
fn sync_fetches_but_never_moves_a_trunk_holding_work() {
    let lab = StoreLab::new();
    let dirty = lab.clone_repo("dirty");
    let elsewhere = lab.clone_repo("elsewhere");
    let dirty_head = rev(&dirty, "HEAD");
    std::fs::write(dirty.join("wip.txt"), "unsaved").unwrap();
    git(&["checkout", "-q", "-b", "stray"], &elsewhere);
    let elsewhere_head = rev(&elsewhere, "HEAD");
    let dirty_new = lab.commit("dirty", "d");
    let elsewhere_new = lab.commit("elsewhere", "e");

    let output = lab.jeet(&["sync"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("uncommitted changes"), "{stderr}");
    assert!(stderr.contains("on stray, not main"), "{stderr}");

    // Fetched...
    assert_eq!(rev(&dirty, "origin/main"), dirty_new);
    assert_eq!(rev(&elsewhere, "origin/main"), elsewhere_new);
    // ...but left exactly where the user had them.
    assert_eq!(rev(&dirty, "HEAD"), dirty_head);
    assert_eq!(
        std::fs::read_to_string(dirty.join("wip.txt")).unwrap(),
        "unsaved"
    );
    assert_eq!(rev(&elsewhere, "HEAD"), elsewhere_head);
}

#[test]
fn sync_reports_a_repo_it_cannot_fetch() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let gone = lab.clone_repo("gone");
    std::fs::remove_dir_all(lab.origins.path().join("acme/gone.git")).unwrap();
    let pushed = lab.commit("widget", "w");

    let output = lab.jeet(&["sync"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a failed fetch must fail the sync"
    );
    assert!(stderr.contains("acme/gone: sync failed"), "{stderr}");
    assert!(stderr.contains("1 of 2"), "{stderr}");
    // One bad repo does not stop the rest.
    let widget = gone.parent().unwrap().join("widget");
    assert_eq!(rev(&widget, "HEAD"), pushed);
}

#[test]
fn sync_with_an_empty_store_is_fine() {
    let lab = StoreLab::new();
    let output = lab.jeet(&["sync"]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

/// Wait for a background sync to have run and let go of its lock.
fn wait_for_background_sync(home: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let log = home.join(".sync.log");
    loop {
        let finished = std::fs::read_to_string(&log)
            .map(|l| l.contains("store sync finished"))
            .unwrap_or(false);
        if finished && !home.join(".sync.lock").exists() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background sync never finished; log: {:?}",
            std::fs::read_to_string(&log)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn a_network_command_starts_a_background_sync_once_per_interval() {
    let lab = StoreLab::new();
    let widget = lab.clone_repo("widget");
    let pushed = lab.commit("widget", "upstream work");
    let home = lab.home();

    // Cloning is a network command: with syncing on it kicks a sync off, and
    // that brings the other trunk up to date without anyone waiting for it.
    lab.origin("gadget");
    let output = lab.clone_with_interval("gadget", "300");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("cloned"),
        "the trigger must not write to stdout"
    );
    wait_for_background_sync(&home);
    assert_eq!(rev(&widget, "HEAD"), pushed);
    let last = std::fs::read_to_string(home.join(".last-sync")).unwrap();

    // Inside the interval, another network command starts nothing.
    std::fs::remove_file(home.join(".sync.log")).unwrap();
    lab.origin("gizmo");
    let output = lab.clone_with_interval("gizmo", "300");
    assert!(output.status.success());
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !home.join(".sync.log").exists(),
        "a second sync was started"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".last-sync")).unwrap(),
        last
    );
}

#[test]
fn store_sync_steps_aside_for_a_running_sync() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let before = rev(&trunk, "HEAD");
    lab.commit("widget", "w");
    let home = lab.home();
    // Held by a live process: this test's own.
    std::fs::write(home.join(".sync.lock"), format!("{}\n", std::process::id())).unwrap();

    let output = lab.jeet(&["store-sync"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another sync is running"));
    assert_eq!(rev(&trunk, "HEAD"), before);
    assert!(
        home.join(".sync.lock").exists(),
        "someone else's lock was removed"
    );

    // Once that holder is gone the lock is fair game.
    std::fs::write(home.join(".sync.lock"), "4294967294\n").unwrap();
    let output = lab.jeet(&["store-sync"]);
    assert!(output.status.success());
    assert_ne!(rev(&trunk, "HEAD"), before);
    assert!(!home.join(".sync.lock").exists());
}

#[test]
fn sync_of_one_repo_waits_for_another_fetch_of_it() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let before = rev(&trunk, "HEAD");
    let pushed = lab.commit("widget", "w");
    let home = lab.home();
    // Someone — a live process, this test's own — is fetching widget.
    let lock = home.join(".sync-locks/github.com_acme_widget.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();

    let mut cmd = jeet_bin();
    let mut child = lab
        .wire(&mut cmd)
        .args(["sync", "--repo", "widget"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        child.try_wait().unwrap().is_none(),
        "fetched while another fetch of the repo held its lock"
    );
    assert_eq!(rev(&trunk, "HEAD"), before);

    std::fs::remove_file(&lock).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(rev(&trunk, "HEAD"), pushed);
    assert!(!lock.exists(), "the sync left its repo lock behind");
}

/// A stand-in `gh` for `jeet review`: `pr view` answers with a same-repo PR
/// on `feat-x`, and `pr checkout` notes what it found around it — whether a
/// store sync had been started yet, and which repo fetch locks were held —
/// before checking the branch out.
fn fake_gh(dir: &std::path::Path, report: &std::path::Path) {
    let script = format!(
        r#"#!/bin/sh
case "$1 $2" in
"pr view")
  echo '{{"number":42,"title":"Feature X","url":"https://github.com/acme/widget/pull/42","state":"OPEN","isDraft":false,"reviewDecision":"","headRefName":"feat-x","headRefOid":"","baseRefName":"main","isCrossRepository":false}}'
  ;;
"pr checkout")
  {{
    if [ -e "$JEET_HOME/.last-sync" ]; then echo "sync started"; else echo "no sync"; fi
    ls "$JEET_HOME/.sync-locks"
  }} > "{}"
  git checkout -q -b "$5"
  ;;
*)
  echo "fake gh: unexpected $*" >&2
  exit 1
  ;;
esac
"#,
        report.display()
    );
    let gh = dir.join("gh");
    std::fs::write(&gh, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn review_checks_out_before_starting_a_store_sync() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let home = lab.home();
    let bin = TempDir::new().unwrap();
    let report = bin.path().join("report");
    fake_gh(bin.path(), &report);
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A sync is due — none has ever run — so review will start one.
    let mut cmd = jeet_bin();
    let output = lab
        .wire(&mut cmd)
        .env("PATH", path)
        .env("JEET_SYNC_INTERVAL_SECS", "300")
        .args(["review", "42", "--no-command"])
        .current_dir(&trunk)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "review failed: {stderr}");

    let seen = std::fs::read_to_string(&report).unwrap();
    assert!(
        seen.starts_with("no sync"),
        "a store sync was started before gh fetched the PR: {seen}"
    );
    assert!(
        seen.contains("github.com_acme_widget.lock"),
        "gh fetched without holding the repo's fetch lock: {seen}"
    );
    // The sync still happens, just after.
    wait_for_background_sync(&home);
    assert!(!home
        .join(".sync-locks/github.com_acme_widget.lock")
        .exists());
}

/// A stand-in `gh` on PATH, so `jeet review` runs end to end with no network:
/// `pr view` answers from a fixture and `pr checkout` makes the branch.
fn stub_gh(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
# jeet must never let gh prompt.
[ "$GH_PROMPT_DISABLED" = 1 ] || { echo "gh run without GH_PROMPT_DISABLED" >&2; exit 3; }
echo "$*" >> "$(dirname "$0")/calls"
case "$1 $2" in
  "pr view")
    echo '{"number":7,"title":"Widen the store","url":"https://github.com/acme/review/pull/7","state":"OPEN","isDraft":false,"reviewDecision":"","headRefName":"wide-store","headRefOid":"abc123","baseRefName":"main","isCrossRepository":false}'
    ;;
  "pr checkout")
    exec git checkout -q -b "$5"
    ;;
  *)
    echo "unexpected gh $*" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Without a terminal `jeet review` opens no viewer: it is scripted, so it
/// prints the PR's worktree and nothing else on stdout.
#[test]
fn review_without_a_terminal_prints_only_the_worktree_path() {
    let lab = lab("review");
    let bin = stub_gh(std::path::Path::new(&lab.home));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = jeet_bin()
        .args(["review", "#7"])
        .current_dir(&lab.repo)
        .env("JEET_HOME", &lab.home)
        .env("PATH", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "review failed: {stderr}");
    let worktree = path_from(&output);
    assert!(worktree.is_dir());
    let head = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&worktree)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "wide-store");
    assert!(stderr.contains("#7 Widen the store"), "{stderr}");

    // Only the lookups a checkout needs: the viewer's diff and threads are
    // fetched by the viewer, which needs a terminal.
    let calls = std::fs::read_to_string(bin.join("calls")).unwrap();
    assert!(
        calls
            .lines()
            .all(|c| c.starts_with("pr view") || c.starts_with("pr checkout")),
        "{calls}"
    );
}

// ---------------------------------------------------------------------------
// tasks and their worktrees
// ---------------------------------------------------------------------------

impl StoreLab {
    /// `jeet` run from `cwd`, with extra environment on top of the lab's.
    fn jeet_at(
        &self,
        args: &[&str],
        cwd: &std::path::Path,
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = jeet_bin();
        self.wire(&mut cmd).args(args).current_dir(cwd);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    /// `jeet task new <name>`, run from outside any task; the task directory.
    fn new_task(&self, name: &str) -> std::path::PathBuf {
        let output = self.jeet_at(&["task", "new", name], &self.home(), &[]);
        succeeded(&output);
        path_from(&output)
    }

    /// `jeet worktree add <args…>` from inside `task`; the worktree's path.
    fn add(&self, task: &std::path::Path, args: &[&str]) -> std::path::PathBuf {
        let mut full = vec!["worktree", "add"];
        full.extend_from_slice(args);
        let output = self.jeet_at(&full, task, &[]);
        succeeded(&output);
        path_from(&output)
    }
}

/// Assert a command worked, showing its stderr when it did not.
fn succeeded(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn git_stdout(args: &[&str], cwd: &std::path::Path) -> String {
    let mut cmd = Command::new("git");
    hermetic_git(&mut cmd);
    let output = cmd.args(args).current_dir(cwd).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn is_detached(dir: &std::path::Path) -> bool {
    !Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success()
}

fn task_id(task: &std::path::Path) -> String {
    std::fs::read_to_string(task.join(".jeet/task-id"))
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn an_empty_task_takes_worktrees_on_demand() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    lab.clone_repo("gadget");
    // Pushed after the clone: a worktree must start from origin as it is now.
    let upstream = lab.commit("widget", "upstream work");

    let task = lab.new_task("Fix the login page");
    assert!(
        task.ends_with("tasks/fix-the-login-page"),
        "{}",
        task.display()
    );
    // An empty canvas: the marker and the jeet skill, nothing else.
    let mut entries: Vec<String> = std::fs::read_dir(&task)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(entries, [".claude", ".jeet"]);
    assert!(task.join(".claude/skills/jeet/SKILL.md").is_file());
    let id = task_id(&task);

    // From a subdirectory of the task, which is found by walking up.
    let notes = task.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let output = lab.jeet_at(&["worktree", "add", "widget"], &notes, &[]);
    succeeded(&output);
    let first = path_from(&output);
    assert_eq!(first, task.join("widget"));
    assert!(is_detached(&first));
    assert_eq!(rev(&first, "HEAD"), upstream);

    // From anywhere at all, with the task in the environment as an agent
    // session has it — and a second worktree of the same repository.
    let output = lab.jeet_at(
        &["worktree", "add", "acme/widget", "--branch", "feat-a"],
        &lab.home(),
        &[("JEET_TASK_ID", &id)],
    );
    succeeded(&output);
    let second = path_from(&output);
    assert_eq!(second, task.join("widget-2"));
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &second),
        "feat-a"
    );

    // From deep inside one of the task's own worktrees.
    let deep = first.join("src/deep");
    std::fs::create_dir_all(&deep).unwrap();
    let output = lab.jeet_at(&["worktree", "add", "gadget"], &deep, &[]);
    succeeded(&output);
    assert_eq!(path_from(&output), task.join("gadget"));

    let output = lab.jeet_at(&["worktree", "ls"], &task, &[]);
    succeeded(&output);
    let listing = stdout_of(&output);
    assert_eq!(listing.lines().count(), 3, "{listing}");
    assert!(listing.contains("feat-a"), "{listing}");
    assert!(listing.contains("github.com/acme/gadget"), "{listing}");

    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &[]);
    assert!(
        stdout_of(&output).contains("3 wt: widget, gadget"),
        "{}",
        stdout_of(&output)
    );

    for args in [
        vec![
            "worktree",
            "path",
            "widget-2",
            "--task",
            "fix-the-login-page",
        ],
        vec![
            "worktree",
            "--task",
            "fix-the-login-page",
            "path",
            "widget-2",
        ],
    ] {
        let output = lab.jeet_at(&args, &lab.home(), &[]);
        succeeded(&output);
        assert_eq!(path_from(&output), second);
    }
    // Without a terminal, opening a task just names its directory.
    for verb in ["path", "open"] {
        let output = lab.jeet_at(&["task", verb, "fix-the-login-page"], &lab.home(), &[]);
        succeeded(&output);
        assert_eq!(path_from(&output), task);
    }

    // Outside any task there is nothing to add a worktree to.
    let output = lab.jeet_at(&["worktree", "add", "widget"], &lab.home(), &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("not inside a task"),
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn reconcile_adopts_worktrees_made_by_hand_and_forgets_vanished_ones() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let task = lab.new_task("by hand");
    let made = lab.add(&task, &["widget"]);

    // What an agent might do without asking jeet.
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "manual",
            &task.join("manual").to_string_lossy(),
        ],
        &trunk,
    );
    std::fs::remove_dir_all(&made).unwrap();

    let output = lab.jeet_at(&["worktree", "ls"], &task, &[]);
    succeeded(&output);
    let listing = stdout_of(&output);
    assert!(listing.starts_with("manual "), "{listing}");
    assert_eq!(listing.lines().count(), 1, "{listing}");
    let registered = git_stdout(&["worktree", "list", "--porcelain"], &trunk);
    assert!(
        !registered.contains(made.to_str().unwrap()),
        "git still lists the vanished worktree: {registered}"
    );

    // Adopted means owned: it can be removed like any other, and its branch,
    // which the task did not make, stays.
    succeeded(&lab.jeet_at(&["worktree", "rm", "manual"], &task, &[]));
    assert!(!task.join("manual").exists());
    assert_eq!(
        git_stdout(&["branch", "--list", "manual"], &trunk),
        "manual"
    );
}

#[test]
fn archive_and_unarchive_bring_back_every_kind_of_work() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let seed = lab.seed("widget");
    std::fs::write(seed.join("tracked.txt"), "one\n").unwrap();
    git(&["add", "-A"], &seed);
    git(&["commit", "-qm", "tracked"], &seed);
    git(&["push", "-q", "origin", "main"], &seed);

    let task = lab.new_task("round trip");
    let id = task_id(&task);
    let branchy = lab.add(&task, &["widget", "--branch", "feat"]);
    let detached = lab.add(&task, &["widget"]);

    // A commit on a detached HEAD: on no branch at all.
    std::fs::write(detached.join("note.txt"), "kept\n").unwrap();
    git(&["add", "-A"], &detached);
    git(&["commit", "-qm", "detached work"], &detached);
    let detached_head = rev(&detached, "HEAD");
    // Every kind of uncommitted change.
    std::fs::write(branchy.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(branchy.join("staged.txt"), "staged\n").unwrap();
    git(&["add", "staged.txt"], &branchy);
    std::fs::write(branchy.join("untracked.txt"), "loose\n").unwrap();
    let branch_head = rev(&branchy, "HEAD");

    let output = lab.jeet_at(&["task", "archive", "round-trip"], &lab.home(), &[]);
    succeeded(&output);
    assert!(output.stdout.is_empty(), "archive wrote to stdout");
    assert!(
        !branchy.exists() && !detached.exists(),
        "archive left worktrees on disk"
    );
    assert!(
        task.join(".jeet/task-id").is_file(),
        "the task directory must stay"
    );
    let refs = git_stdout(
        &["for-each-ref", "--format=%(refname)", "refs/jeet/"],
        &trunk,
    );
    assert!(
        refs.contains(&format!("refs/jeet/stash/{id}/widget")),
        "{refs}"
    );
    assert!(
        refs.contains(&format!("refs/jeet/head/{id}/widget-2")),
        "{refs}"
    );
    // Nothing archived may depend on git keeping unreachable objects.
    git(&["gc", "-q", "--prune=now"], &trunk);

    let output = lab.jeet_at(&["task", "ls", "--archived"], &lab.home(), &[]);
    assert!(stdout_of(&output).contains("round-trip"));
    let output = lab.jeet_at(&["worktree", "add", "widget"], &task, &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("archived"),
        "{}",
        stderr_of(&output)
    );

    succeeded(&lab.jeet_at(&["task", "unarchive", "round-trip"], &lab.home(), &[]));
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &branchy),
        "feat"
    );
    assert_eq!(rev(&branchy, "HEAD"), branch_head);
    assert_eq!(
        std::fs::read_to_string(branchy.join("tracked.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(
        std::fs::read_to_string(branchy.join("untracked.txt")).unwrap(),
        "loose\n"
    );
    assert_eq!(
        git_stdout(&["diff", "--cached", "--name-only"], &branchy),
        "staged.txt",
        "staged changes should come back staged"
    );
    assert!(is_detached(&detached));
    assert_eq!(rev(&detached, "HEAD"), detached_head);
    assert_eq!(
        std::fs::read_to_string(detached.join("note.txt")).unwrap(),
        "kept\n"
    );
    assert_eq!(
        git_stdout(&["for-each-ref", "refs/jeet/"], &trunk),
        "",
        "unarchive left its refs behind"
    );
    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &[]);
    assert!(stdout_of(&output).contains("round-trip"));
}

#[test]
fn delete_takes_everything_the_task_owns_and_nothing_else() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    git(&["branch", "existing"], &trunk);

    let task = lab.new_task("doomed");
    let made = lab.add(&task, &["widget", "--branch", "made"]);
    let kept = lab.add(&task, &["widget", "--branch", "existing"]);
    std::fs::write(made.join("work.txt"), "work").unwrap();
    git(&["add", "-A"], &made);
    git(&["commit", "-qm", "unpushed"], &made);
    std::fs::write(kept.join("wip.txt"), "dirty").unwrap();
    // Archived first, so there are refs to clean up too.
    succeeded(&lab.jeet_at(&["task", "archive", "doomed"], &lab.home(), &[]));

    // Without a terminal, delete says what would be lost and does nothing.
    let output = lab.jeet_at(&["task", "delete", "doomed"], &lab.home(), &[]);
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("widget (made): 1 commit on no other branch"),
        "{stderr}"
    );
    assert!(stderr.contains("uncommitted changes saved"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    assert!(task.is_dir());

    succeeded(&lab.jeet_at(&["task", "delete", "doomed", "--force"], &lab.home(), &[]));
    assert!(!task.exists(), "the task directory survived");
    assert_eq!(
        git_stdout(&["branch", "--list", "made"], &trunk),
        "",
        "the branch the task made survived"
    );
    assert_eq!(
        git_stdout(&["branch", "--list", "existing"], &trunk),
        "existing",
        "a branch the task did not make was deleted"
    );
    assert_eq!(git_stdout(&["for-each-ref", "refs/jeet/"], &trunk), "");
    let registered = git_stdout(&["worktree", "list", "--porcelain"], &trunk);
    assert_eq!(
        registered
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "{registered}"
    );
    let output = lab.jeet_at(&["task", "ls", "--archived"], &lab.home(), &[]);
    assert!(!stdout_of(&output).contains("doomed"));
}

#[test]
fn a_branch_checked_out_in_another_task_is_refused() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let first = lab.new_task("first");
    let second = lab.new_task("second");
    lab.add(&first, &["widget", "--branch", "shared"]);

    let output = lab.jeet_at(
        &["worktree", "add", "widget", "--branch", "shared"],
        &second,
        &[],
    );
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(stderr.contains("in task first"), "{stderr}");
    assert!(!second.join("widget").exists());

    // Deleting a live task frees the branch it held.
    succeeded(&lab.jeet_at(&["task", "delete", "first", "-f"], &lab.home(), &[]));
    assert!(!first.exists());
    lab.add(&second, &["widget", "--branch", "shared"]);
}

#[test]
fn worktree_rm_refuses_to_lose_work_unless_forced() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let task = lab.new_task("tidy");

    // Nothing in it: removed, and the branch it made goes with it.
    lab.add(&task, &["widget", "--branch", "tidy-a"]);
    succeeded(&lab.jeet_at(&["worktree", "rm", "widget"], &task, &[]));
    assert_eq!(git_stdout(&["branch", "--list", "tidy-a"], &trunk), "");

    // An unpushed commit: refused, then forced — and the branch is kept.
    let wt = lab.add(&task, &["widget", "--branch", "tidy-b"]);
    std::fs::write(wt.join("work.txt"), "work").unwrap();
    git(&["add", "-A"], &wt);
    git(&["commit", "-qm", "work"], &wt);
    let output = lab.jeet_at(&["worktree", "rm", "widget"], &task, &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("1 unpushed commit"),
        "{}",
        stderr_of(&output)
    );
    assert!(wt.is_dir());
    let output = lab.jeet_at(&["worktree", "rm", "widget", "--force"], &task, &[]);
    succeeded(&output);
    assert!(!wt.exists());
    assert!(stderr_of(&output).contains("kept branch tidy-b"));
    assert_eq!(
        git_stdout(&["branch", "--list", "tidy-b"], &trunk),
        "tidy-b"
    );

    // Uncommitted changes: refused, and from inside it, refused outright.
    let wt = lab.add(&task, &["widget"]);
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    let output = lab.jeet_at(&["worktree", "rm", "widget"], &task, &[]);
    assert!(stderr_of(&output).contains("1 uncommitted change"));
    let output = lab.jeet_at(&["worktree", "rm", "widget", "-f"], &wt, &[]);
    assert!(
        stderr_of(&output).contains("cd out"),
        "{}",
        stderr_of(&output)
    );
    assert!(wt.join("wip.txt").exists());
}

#[test]
fn a_task_can_start_with_a_repo_on_a_branch() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let home = lab.home();

    let output = lab.jeet_at(
        &[
            "task", "new", "Preset", "--repo", "widget", "--branch", "feat-p",
        ],
        &home,
        &[],
    );
    succeeded(&output);
    let task = path_from(&output);
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &task.join("widget")),
        "feat-p"
    );

    // --branch names one repository's branch.
    let output = lab.jeet_at(
        &[
            "task", "new", "Two", "--repo", "widget", "--repo", "widget", "--branch", "x",
        ],
        &home,
        &[],
    );
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("exactly one"));

    // A preset that cannot be added leaves no half-made task behind.
    let output = lab.jeet_at(
        &["task", "new", "Broken", "--repo", "nosuchrepo"],
        &home,
        &[],
    );
    assert!(!output.status.success());
    assert!(!home.join("tasks/broken").exists());
    let output = lab.jeet_at(&["task", "ls"], &home, &[]);
    assert!(!stdout_of(&output).contains("broken"));
}

/// `git worktree list` reports symlink-resolved paths; a home reached through
/// a symlink must not stop jeet recognising worktrees inside its tasks.
#[cfg(unix)]
#[test]
fn tasks_work_through_a_symlinked_home() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let links = TempDir::new().unwrap();
    let link = links.path().join("home-link");
    std::os::unix::fs::symlink(lab.home.path(), &link).unwrap();
    let env = [("JEET_HOME", link.to_str().unwrap())];

    succeeded(&lab.jeet_at(&["task", "new", "linked"], links.path(), &env));
    let through_link = link.join("tasks/linked");
    git(
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &through_link.join("manual").to_string_lossy(),
        ],
        &trunk,
    );
    let output = lab.jeet_at(&["worktree", "ls"], &through_link, &env);
    succeeded(&output);
    assert!(
        stdout_of(&output).contains("manual"),
        "{}",
        stdout_of(&output)
    );
}

/// Offline, the store's last fetch is a fine base; and a repo whose default
/// branch is not `main` must still give worktrees.
#[test]
fn a_worktree_is_added_offline_from_a_repo_on_master() {
    let home = TempDir::new().unwrap();
    let home = home.path().canonicalize().unwrap();
    let repo = TempDir::new().unwrap();
    init_repo_on_branch(
        repo.path(),
        "https://github.com/acme/mastered.git",
        "master",
    );
    let jeet = |args: &[&str], cwd: &std::path::Path| {
        jeet_bin()
            .args(args)
            .current_dir(cwd)
            .env("JEET_HOME", &home)
            .output()
            .unwrap()
    };
    succeeded(&jeet(&["adopt", repo.path().to_str().unwrap()], &home));
    let output = jeet(&["task", "new", "offline"], &home);
    succeeded(&output);
    let task = path_from(&output);

    let output = jeet(&["worktree", "add", "mastered"], &task);
    succeeded(&output);
    assert!(
        stderr_of(&output).contains("could not fetch"),
        "{}",
        stderr_of(&output)
    );
    let wt = path_from(&output);
    assert_eq!(rev(&wt, "HEAD"), rev(repo.path(), "master"));
}

/// The shell wrapper is the only way the cd hand-off reaches a shell, so run
/// the emitted snippet for real rather than grepping it.
#[test]
fn the_emitted_shell_wrapper_actually_changes_directory() {
    let lab = StoreLab::new();
    let output = jeet_bin().args(["init-shell"]).output().unwrap();
    let script = lab.home().join("wrapper.sh");
    std::fs::write(&script, stdout_of(&output)).unwrap();

    let program = format!(
        r#"set -u
export PATH="{}:$PATH"
. "{}"
cd "{}"
jeet task new wrapped >/dev/null 2>&1
pwd"#,
        std::path::Path::new(env!("CARGO_BIN_EXE_jeet"))
            .parent()
            .unwrap()
            .display(),
        script.display(),
        lab.home().display(),
    );
    let mut cmd = Command::new("bash");
    hermetic_git(&mut cmd);
    let output = cmd
        .args(["--noprofile", "--norc", "-c", &program])
        .env("JEET_HOME", lab.home())
        .env("JEET_SYNC_INTERVAL_SECS", "0")
        .env_remove("JEET_CD_FILE")
        .output()
        .unwrap();
    let landed = stdout_of(&output).trim().to_string();
    assert!(
        landed.ends_with("tasks/wrapped"),
        "wrapper left the shell in {landed:?}; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn a_task_from_a_project_gets_a_copy_of_its_files() {
    let lab = StoreLab::new();
    let home = lab.home();
    let project = home.join("projects/web");
    std::fs::create_dir_all(project.join("skills/deploy")).unwrap();
    std::fs::write(project.join("CLAUDE.md"), "be careful\n").unwrap();
    std::fs::write(project.join("skills/deploy/SKILL.md"), "deploy it\n").unwrap();

    let output = lab.jeet_at(&["task", "new", "Launch", "--project", "web"], &home, &[]);
    succeeded(&output);
    let task = path_from(&output);
    assert_eq!(
        std::fs::read_to_string(task.join("CLAUDE.md")).unwrap(),
        "be careful\n"
    );
    assert_eq!(
        std::fs::read_to_string(task.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "deploy it\n"
    );
    assert!(task.join(".claude/skills/jeet/SKILL.md").is_file());
    // A snapshot: editing the project later leaves the task alone.
    std::fs::write(project.join("CLAUDE.md"), "changed\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(task.join("CLAUDE.md")).unwrap(),
        "be careful\n"
    );

    for bad in ["nope", "../projects/web"] {
        let output = lab.jeet_at(&["task", "new", "Bad", "--project", bad], &home, &[]);
        assert!(!output.status.success(), "{bad} was accepted");
    }
    assert!(!home.join("tasks/bad").exists());
}
