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
    // No test may reach the developer's tmux server or Claude sessions: tmux
    // is missing unless a test supplies one, and Claude's store is empty.
    cmd.env("JEET_TMUX", "/nonexistent/jeet-test-tmux")
        .env_remove("TMUX")
        .env("CLAUDE_CONFIG_DIR", "/nonexistent/jeet-test-claude");
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

/// A stand-in `gh` that answers everything the review viewer asks — the
/// diff, threads, pending review and fingerprint from the fixtures — and
/// records each call's arguments, NUL-separated, one file per call.
#[cfg(unix)]
fn stub_viewer_gh(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(bin.join("calls")).unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["pr.diff", "review_threads.json", "pr_fingerprint.json"] {
        std::fs::copy(fixtures.join(name), bin.join(name)).unwrap();
    }
    let script = bin.join("gh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
here=$(dirname "$0")
# Written whole, then renamed, so a reader never sees half a call.
printf '%s\0' "$@" > "$here/calls/.call.$$"
mv "$here/calls/.call.$$" "$here/calls/call.$$"
[ "$GH_PROMPT_DISABLED" = 1 ] || { echo "gh run without GH_PROMPT_DISABLED" >&2; exit 3; }
case "$1 $2" in
  "pr view")
    echo '{"number":7,"title":"Widen the store","url":"https://github.com/acme/viewer/pull/7","state":"OPEN","isDraft":false,"reviewDecision":"","headRefName":"wide-store","headRefOid":"abc123","baseRefName":"main","isCrossRepository":false}'
    exit 0 ;;
  "pr checkout") exec git checkout -q -b "$5" ;;
  "pr diff") exec cat "$here/pr.diff" ;;
  "api graphql")
    case "$3" in
      --paginate) exec cat "$here/review_threads.json" ;;
      *) exec cat "$here/pr_fingerprint.json" ;;
    esac ;;
esac
for arg in "$@"; do
  case "$arg" in
    */reviews\?per_page=100) echo '[[]]'; exit 0 ;;
  esac
done
echo '{}'
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The calls the stub `gh` has had so far, each as its arguments.
#[cfg(unix)]
fn gh_calls(bin: &std::path::Path) -> Vec<Vec<String>> {
    let Ok(dir) = std::fs::read_dir(bin.join("calls")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("call."))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .map(|text| text.split_terminator('\0').map(str::to_string).collect())
        .collect()
}

/// Wait up to 30s for the stub `gh` to have been called `count` times in a
/// way `pick` likes.
#[cfg(unix)]
fn wait_for_calls(
    bin: &std::path::Path,
    count: usize,
    what: &str,
    pick: impl Fn(&[String]) -> bool,
) -> Vec<Vec<String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let calls: Vec<_> = gh_calls(bin).into_iter().filter(|c| pick(c)).collect();
        if calls.len() >= count {
            return calls;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gave up waiting for {what}; calls so far: {:?}",
            gh_calls(bin)
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// `jeet review` on a pseudo-terminal, typed at like a person would: the
/// viewer loads the PR through `gh`; `+` reacts to a comment, `R` replies to
/// its thread and `c` comments on a line, each through `gh` and each followed
/// by a fresh fetch of the review; and Esc leaves.
///
/// `script` provides the terminal; where it is missing the test says so and
/// passes, since there is nothing to drive the viewer with.
#[cfg(unix)]
#[test]
fn the_review_viewer_reacts_replies_and_comments_through_gh() {
    use std::io::Write;
    use std::process::Stdio;

    if Command::new("script")
        .arg("-q")
        .arg("/dev/null")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("no `script` to run a terminal with; skipping");
        return;
    }
    let lab = lab("viewer");
    let bin = stub_viewer_gh(std::path::Path::new(&lab.home));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // A terminal of a known size, then the viewer in it.
    let inner = format!(
        "stty rows 40 cols 120 && exec '{}' review 7",
        env!("CARGO_BIN_EXE_jeet")
    );
    let mut cmd = Command::new("script");
    if cfg!(target_os = "linux") {
        cmd.args(["-q", "-e", "-c", &inner, "/dev/null"]);
    } else {
        cmd.args(["-q", "/dev/null", "sh", "-c", &inner]);
    }
    hermetic_git(&mut cmd);
    let mut child = cmd
        .current_dir(&lab.repo)
        .env("JEET_HOME", &lab.home)
        .env("JEET_SYNC_INTERVAL_SECS", "0")
        .env("PATH", &path)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut keys = child.stdin.take().unwrap();
    let mut type_keys = |text: &[u8]| {
        keys.write_all(text).unwrap();
        keys.flush().unwrap();
    };
    let is_diff = |c: &[String]| c.starts_with(&["pr".into(), "diff".into()]);
    // Each load asks for the diff once; wait for the `n`th to have been
    // answered and drawn.
    let loaded = |n: usize| {
        wait_for_calls(&bin, n, "a load of the review", is_diff);
        wait_for_calls(&bin, 2 * n, "the threads and fingerprint", |c| {
            c.starts_with(&["api".into(), "graphql".into()])
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
    };
    loaded(1);

    // The cursor starts on the outdated thread heading the page: grace's
    // comment, REST id 1004. React to it with the first reaction, 👍.
    type_keys(b"+\r");
    let reactions = wait_for_calls(&bin, 1, "the reaction", |c| {
        c.iter().any(|a| a.ends_with("/reactions"))
    });
    assert!(
        reactions[0].contains(&"repos/{owner}/{repo}/pulls/comments/1004/reactions".to_string()),
        "{:?}",
        reactions[0]
    );
    assert!(reactions[0].contains(&"content=+1".to_string()));
    loaded(2);

    // The refetch kept the cursor on the comment: reply to its thread.
    type_keys(b"Rthanks, grace\r");
    let replies = wait_for_calls(&bin, 1, "the reply", |c| {
        c.iter().any(|a| a.ends_with("/replies"))
    });
    let reply = &replies[0];
    assert!(
        reply.contains(&"repos/{owner}/{repo}/pulls/7/comments/1004/replies".to_string()),
        "{reply:?}"
    );
    assert!(reply.contains(&"POST".to_string()), "{reply:?}");
    // The text is one argument, whatever it says.
    assert!(
        reply.contains(&"body=thanks, grace".to_string()),
        "{reply:?}"
    );
    loaded(3);

    // Down past the hunk header to head line 8, and comment on it. There is
    // no pending review, so this starts one.
    type_keys(b"jjc$(touch pwned) `id`\r");
    let comments = wait_for_calls(&bin, 1, "the inline comment", |c| {
        c.iter().any(|a| a.starts_with("comments[][body]="))
    });
    let comment = &comments[0];
    for arg in [
        "repos/{owner}/{repo}/pulls/7/reviews",
        "comments[][path]=src/lib.rs",
        "comments[][line]=8",
        "comments[][side]=RIGHT",
        "comments[][body]=$(touch pwned) `id`",
    ] {
        assert!(comment.contains(&arg.to_string()), "{arg}: {comment:?}");
    }
    loaded(4);

    type_keys(b"\x1b");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the viewer did not close on Esc");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    drop(keys);
    assert!(status.success(), "jeet review exited with {status}");

    // Nothing went to GitHub but reads and those three changes.
    let writes: Vec<_> = gh_calls(&bin)
        .into_iter()
        .filter(|c| {
            c.iter()
                .any(|a| a == "POST" || a == "PATCH" || a == "DELETE")
        })
        .collect();
    assert_eq!(writes.len(), 3, "{writes:?}");
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
    let output = lab.jeet_at(&["task", "path", "fix-the-login-page"], &lab.home(), &[]);
    succeeded(&output);
    assert_eq!(path_from(&output), task);
    // The agent runs in tmux, and there is none here.
    let output = lab.jeet_at(&["task", "open", "fix-the-login-page"], &lab.home(), &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("brew install tmux"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(stdout_of(&output), "");

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

/// Work unarchive cannot put back — changes that no longer apply, a commit
/// its branch moved away from — survives the next archive, gc and all, and
/// delete lists it before removing it.
#[test]
fn work_unarchive_could_not_put_back_survives_the_next_archive() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let seed = lab.seed("widget");
    std::fs::write(seed.join("tracked.txt"), "one\n").unwrap();
    git(&["add", "-A"], &seed);
    git(&["commit", "-qm", "tracked"], &seed);
    git(&["push", "-q", "origin", "main"], &seed);

    let task = lab.new_task("kept");
    let id = task_id(&task);
    // A commit only its branch holds; the branch is moved away from it below.
    let moved = lab.add(&task, &["widget", "--branch", "feat-a"]);
    std::fs::write(moved.join("a.txt"), "a\n").unwrap();
    git(&["add", "-A"], &moved);
    git(&["commit", "-qm", "only on feat-a"], &moved);
    let lost_commit = rev(&moved, "HEAD");
    // Uncommitted changes that will conflict with where its branch goes.
    let dirty = lab.add(&task, &["widget", "--branch", "feat-b"]);
    std::fs::write(dirty.join("tracked.txt"), "theirs\n").unwrap();
    git(&["commit", "-qam", "theirs"], &dirty);
    git(&["branch", "theirs"], &dirty);
    git(&["reset", "-q", "--hard", "HEAD~1"], &dirty);
    std::fs::write(dirty.join("tracked.txt"), "mine\n").unwrap();

    succeeded(&lab.jeet_at(&["task", "archive", "kept"], &lab.home(), &[]));
    git(&["branch", "-f", "feat-a", "origin/main"], &trunk);
    git(&["branch", "-f", "feat-b", "theirs"], &trunk);

    let output = lab.jeet_at(&["task", "unarchive", "kept"], &lab.home(), &[]);
    succeeded(&output);
    let stderr = stderr_of(&output);
    let kept_head = format!("refs/jeet/kept/{id}/widget/head-1");
    let kept_stash = format!("refs/jeet/kept/{id}/widget-2/stash-1");
    assert!(stderr.contains(&kept_head), "{stderr}");
    assert!(stderr.contains(&kept_stash), "{stderr}");

    // The user reads the warnings, does nothing about them, tidies the
    // conflicted worktree and archives again with a clean tree.
    git(&["reset", "-q", "--hard"], &dirty);
    succeeded(&lab.jeet_at(&["task", "archive", "kept"], &lab.home(), &[]));
    git(&["gc", "-q", "--prune=now"], &trunk);
    assert_eq!(rev(&trunk, &kept_head), lost_commit);
    assert_eq!(
        git_stdout(&["show", &format!("{kept_stash}:tracked.txt")], &trunk),
        "mine"
    );

    // And they survive coming back again, too.
    succeeded(&lab.jeet_at(&["task", "unarchive", "kept"], &lab.home(), &[]));
    assert_eq!(rev(&trunk, &kept_head), lost_commit);

    let output = lab.jeet_at(&["task", "delete", "kept"], &lab.home(), &[]);
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!("{kept_head}: 1 commit on no branch")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("{kept_stash}: uncommitted changes")),
        "{stderr}"
    );
    succeeded(&lab.jeet_at(&["task", "delete", "kept", "--force"], &lab.home(), &[]));
    assert_eq!(git_stdout(&["for-each-ref", "refs/jeet/"], &trunk), "");
}

/// Refs left under the names archive reuses — by an unarchive that stopped
/// part-way — are listed by delete and set aside, not overwritten, by archive.
#[test]
fn archive_sets_aside_refs_an_unfinished_unarchive_left() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let task = lab.new_task("leftover");
    let id = task_id(&task);
    let wt = lab.add(&task, &["widget", "--branch", "feat"]);

    // A saved change and a pinned commit that this worktree no longer has.
    std::fs::write(wt.join("saved.txt"), "saved\n").unwrap();
    git(&["add", "-A"], &wt);
    let saved = git_stdout(&["stash", "create"], &wt);
    git(&["reset", "-q", "--hard"], &wt);
    git(&["commit", "-q", "--allow-empty", "-m", "pinned"], &wt);
    let pinned = rev(&wt, "HEAD");
    git(&["reset", "-q", "--hard", "HEAD~1"], &wt);
    let stash_ref = format!("refs/jeet/stash/{id}/widget");
    let head_ref = format!("refs/jeet/head/{id}/widget");
    git(&["update-ref", &stash_ref, &saved], &trunk);
    git(&["update-ref", &head_ref, &pinned], &trunk);

    let output = lab.jeet_at(&["task", "delete", "leftover"], &lab.home(), &[]);
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(stderr.contains(&stash_ref), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "1 commit on no branch or remote kept at {head_ref}"
        )),
        "{stderr}"
    );

    let output = lab.jeet_at(&["task", "archive", "leftover"], &lab.home(), &[]);
    succeeded(&output);
    git(&["gc", "-q", "--prune=now"], &trunk);
    let kept = format!("refs/jeet/kept/{id}/widget");
    assert_eq!(rev(&trunk, &format!("{kept}/head-1")), pinned);
    assert_eq!(
        git_stdout(&["show", &format!("{kept}/stash-1:saved.txt")], &trunk),
        "saved"
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains(&format!("{kept}/stash-1")), "{stderr}");
}

/// Worktree names that differ only in punctuation keep separate refs, so
/// each comes back from an archive with its own changes.
#[test]
fn similar_worktree_names_keep_their_own_changes() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let task = lab.new_task("names");
    let dotted = lab.add(&task, &["widget", "--name", "docs.site"]);
    let dashed = lab.add(&task, &["widget", "--name", "docs-site"]);
    for wt in [&dotted, &dashed] {
        std::fs::write(wt.join("mine.txt"), wt.display().to_string()).unwrap();
        git(
            &["commit", "-q", "--allow-empty", "-m", "detached work"],
            wt,
        );
    }
    let heads = [rev(&dotted, "HEAD"), rev(&dashed, "HEAD")];

    succeeded(&lab.jeet_at(&["task", "archive", "names"], &lab.home(), &[]));
    let output = lab.jeet_at(&["task", "unarchive", "names"], &lab.home(), &[]);
    succeeded(&output);
    assert!(
        !stderr_of(&output).contains("could not"),
        "{}",
        stderr_of(&output)
    );
    for (wt, head) in [&dotted, &dashed].into_iter().zip(heads) {
        assert_eq!(rev(wt, "HEAD"), head);
        assert_eq!(
            std::fs::read_to_string(wt.join("mine.txt")).unwrap(),
            wt.display().to_string()
        );
    }
}

/// `task delete` takes a task's exact name: with --force nothing asks, so a
/// name that is only the start of another task's must not delete that one.
#[test]
fn delete_never_guesses_which_task_is_meant() {
    let lab = StoreLab::new();
    let task = lab.new_task("fix login");
    let output = lab.jeet_at(&["task", "delete", "fix", "--force"], &lab.home(), &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("did you mean fix-login"),
        "{}",
        stderr_of(&output)
    );
    assert!(task.is_dir(), "a task was deleted by a prefix of its name");
    // Elsewhere a unique prefix is still enough.
    let output = lab.jeet_at(&["task", "path", "fix"], &lab.home(), &[]);
    succeeded(&output);
    assert_eq!(path_from(&output), task);
    succeeded(&lab.jeet_at(&["task", "delete", "Fix Login", "-f"], &lab.home(), &[]));
    assert!(!task.exists());
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

/// A stand-in `$EDITOR`: a script that writes `text` to the file it is
/// given, appending (`>>`) or replacing (`>`) per `redirect`.
#[cfg(unix)]
fn fake_editor(
    dir: &std::path::Path,
    name: &str,
    redirect: &str,
    text: &str,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join(name);
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat {redirect} \"$1\" <<'JEET_EOF'\n{text}\nJEET_EOF\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn projects_are_made_listed_edited_and_removed() {
    let lab = StoreLab::new();
    let home = lab.home();

    // None yet: bare `jeet projects` lists, and says how to make one.
    let output = lab.jeet_at(&["projects"], &home, &[]);
    succeeded(&output);
    assert!(output.stdout.is_empty());
    assert!(stderr_of(&output).contains("jeet projects new"));

    let output = lab.jeet_at(
        &[
            "projects",
            "new",
            "web",
            "--repo",
            "widget",
            "--repo",
            "acme/gadget",
        ],
        &home,
        &[],
    );
    succeeded(&output);
    let dir = path_from(&output);
    assert_eq!(dir, home.join("projects/web"));
    assert!(dir.join("CLAUDE.md").is_file());
    assert!(dir.join("skills").is_dir());
    let toml = std::fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(
        toml.contains(r#"repos = ["widget", "acme/gadget"]"#),
        "{toml}"
    );

    for bad in ["web", "../escape", ".hidden", "two words"] {
        let output = lab.jeet_at(&["projects", "new", bad], &home, &[]);
        assert!(!output.status.success(), "{bad} was accepted");
    }
    assert!(!home.join("escape").exists());

    for args in [vec!["projects"], vec!["projects", "ls"]] {
        let output = lab.jeet_at(&args, &home, &[]);
        succeeded(&output);
        let listing = stdout_of(&output);
        let row = listing.lines().find(|l| l.starts_with("web ")).unwrap();
        assert!(row.contains("widget, acme/gadget"), "{listing}");
        assert!(row.contains("CLAUDE.md"), "{listing}");
    }

    // An edit that parses is accepted…
    let scripts = TempDir::new().unwrap();
    let good = fake_editor(
        scripts.path(),
        "good",
        ">>",
        "[agent]\nargs = [\"--verbose\"]",
    );
    let output = lab.jeet_at(
        &["projects", "edit", "web"],
        &home,
        &[("JEET_EDITOR", good.to_str().unwrap())],
    );
    succeeded(&output);
    assert!(output.stdout.is_empty());
    assert!(std::fs::read_to_string(dir.join("project.toml"))
        .unwrap()
        .contains("--verbose"));

    // …and one that does not is reported straight away, file left for fixing.
    let bad = fake_editor(scripts.path(), "bad", ">>", "repo = [\"typo\"]");
    let output = lab.jeet_at(
        &["projects", "edit", "web"],
        &home,
        &[("JEET_EDITOR", bad.to_str().unwrap())],
    );
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("jeet projects edit web"),
        "{}",
        stderr_of(&output)
    );
    let output = lab.jeet_at(&["projects"], &home, &[]);
    assert!(
        stdout_of(&output).contains("broken"),
        "{}",
        stdout_of(&output)
    );
    // A broken project is still editable: that is how it gets fixed.
    let fix = fake_editor(scripts.path(), "fix", ">", "repos = []");
    succeeded(&lab.jeet_at(
        &["projects", "edit", "web"],
        &home,
        &[("JEET_EDITOR", fix.to_str().unwrap())],
    ));
    let output = lab.jeet_at(&["projects"], &home, &[]);
    assert!(!stdout_of(&output).contains("broken"));
    // Broken again, and removable all the same.
    std::fs::write(dir.join("project.toml"), "not toml at all [\n").unwrap();

    // Removing needs a yes, which takes a terminal, or --force.
    let output = lab.jeet_at(&["projects", "rm", "web"], &home, &[]);
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("--force"));
    assert!(dir.is_dir());
    succeeded(&lab.jeet_at(&["projects", "rm", "web", "--force"], &home, &[]));
    assert!(!dir.exists());
    let output = lab.jeet_at(&["projects", "rm", "web", "--force"], &home, &[]);
    assert!(!output.status.success());
}

#[test]
fn a_task_from_a_project_starts_with_its_repositories() {
    let lab = StoreLab::new();
    let widget = lab.clone_repo("widget");
    lab.clone_repo("gadget");
    let home = lab.home();
    succeeded(&lab.jeet_at(
        &[
            "projects",
            "new",
            "web",
            "--repo",
            "widget",
            "--repo",
            "https://github.com/acme/gadget.git",
        ],
        &home,
        &[],
    ));

    // The project's worktrees first, detached; then --repo's, on --branch.
    let output = lab.jeet_at(
        &[
            "task",
            "new",
            "Launch",
            "--project",
            "web",
            "--repo",
            "widget",
            "--branch",
            "feat-l",
        ],
        &home,
        &[],
    );
    succeeded(&output);
    let task = path_from(&output);
    assert!(is_detached(&task.join("widget")));
    assert!(is_detached(&task.join("gadget")));
    assert_eq!(
        git_stdout(
            &["rev-parse", "--abbrev-ref", "HEAD"],
            &task.join("widget-2")
        ),
        "feat-l"
    );
    assert!(task.join("CLAUDE.md").is_file());
    assert!(task.join(".claude/skills/jeet/SKILL.md").is_file());
    let output = lab.jeet_at(&["worktree", "ls"], &task, &[]);
    assert_eq!(
        stdout_of(&output).lines().count(),
        3,
        "{}",
        stdout_of(&output)
    );

    let output = lab.jeet_at(&["projects"], &home, &[]);
    let row = stdout_of(&output)
        .lines()
        .find(|l| l.starts_with("web "))
        .map(str::to_string)
        .unwrap();
    assert!(row.split_whitespace().nth(1) == Some("1"), "{row}");

    // A repository the project names but jeet cannot get: no half-made task.
    std::fs::write(
        home.join("projects/web/project.toml"),
        "repos = [\"widget\", \"nosuchrepo\"]\n",
    )
    .unwrap();
    let output = lab.jeet_at(&["task", "new", "Broken", "--project", "web"], &home, &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("nosuchrepo"),
        "{}",
        stderr_of(&output)
    );
    assert!(!home.join("tasks/broken").exists());
    let worktrees = git_stdout(&["worktree", "list"], &widget);
    assert!(!worktrees.contains("tasks/broken"), "{worktrees}");

    // A project.toml that does not parse stops the task before it starts.
    std::fs::write(home.join("projects/web/project.toml"), "repo = []\n").unwrap();
    let output = lab.jeet_at(&["task", "new", "Typo", "--project", "web"], &home, &[]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("project.toml"),
        "{}",
        stderr_of(&output)
    );
    assert!(!home.join("tasks/typo").exists());
}

#[test]
fn resync_updates_what_the_task_left_alone() {
    let lab = StoreLab::new();
    let home = lab.home();
    let project = home.join("projects/web");
    std::fs::create_dir_all(project.join("skills/deploy")).unwrap();
    std::fs::create_dir_all(project.join("skills/old")).unwrap();
    std::fs::write(project.join("CLAUDE.md"), "v1\n").unwrap();
    std::fs::write(project.join("skills/deploy/SKILL.md"), "deploy v1\n").unwrap();
    std::fs::write(project.join("skills/old/SKILL.md"), "old\n").unwrap();

    let output = lab.jeet_at(&["task", "new", "Launch", "--project", "web"], &home, &[]);
    succeeded(&output);
    let task = path_from(&output);

    // The agent makes CLAUDE.md its own; the project moves on.
    std::fs::write(task.join("CLAUDE.md"), "the task's notes\n").unwrap();
    std::fs::write(project.join("CLAUDE.md"), "v2\n").unwrap();
    std::fs::write(project.join("skills/deploy/SKILL.md"), "deploy v2\n").unwrap();
    std::fs::create_dir_all(project.join("skills/review")).unwrap();
    std::fs::write(project.join("skills/review/SKILL.md"), "review\n").unwrap();
    std::fs::remove_dir_all(project.join("skills/old")).unwrap();
    // Whatever happened to the built-in skill, resync puts it back.
    std::fs::remove_dir_all(task.join(".claude/skills/jeet")).unwrap();

    let output = lab.jeet_at(&["task", "resync", "launch"], &home, &[]);
    succeeded(&output);
    assert!(output.stdout.is_empty(), "{}", stdout_of(&output));
    let read = |rel: &str| std::fs::read_to_string(task.join(rel)).unwrap();
    assert_eq!(read("CLAUDE.md"), "the task's notes\n");
    assert_eq!(read(".claude/skills/deploy/SKILL.md"), "deploy v2\n");
    assert_eq!(read(".claude/skills/review/SKILL.md"), "review\n");
    assert!(!task.join(".claude/skills/old").exists());
    assert!(task.join(".claude/skills/jeet/SKILL.md").is_file());
    assert!(
        stderr_of(&output).contains("CLAUDE.md: changed in the task"),
        "{}",
        stderr_of(&output)
    );

    let output = lab.jeet_at(&["task", "resync", "launch", "--force"], &home, &[]);
    succeeded(&output);
    assert_eq!(read("CLAUDE.md"), "v2\n");

    // Only a task made from a project has anything to resync from.
    lab.new_task("Plain");
    let output = lab.jeet_at(&["task", "resync", "plain"], &home, &[]);
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("not made from a project"));
}

// ---------------------------------------------------------------------------
// the agent runtime: tmux, hooks and status
// ---------------------------------------------------------------------------

/// A stand-in `tmux` that records each call, in order, and keeps one
/// session's liveness in a file: `new-session` makes it, `kill-session`
/// ends it, and `has-session` and `list-panes` report it. The pane's pid is
/// whatever the `pid` file holds, and `kill-session` hangs up on it, as tmux
/// does. `display-message` prints the `current` file: the session the
/// caller runs in.
#[cfg(unix)]
fn stub_tmux(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("tmux-stub");
    std::fs::create_dir_all(bin.join("calls")).unwrap();
    let script = bin.join("tmux");
    std::fs::write(
        &script,
        r#"#!/bin/sh
here=$(dirname "$0")
n=$(cat "$here/count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$here/count"
printf '%s\0' "$@" > "$here/calls/$(printf %04d "$n")"
case "$1" in
  -V) echo "tmux 3.4" ;;
  has-session) [ -e "$here/alive" ] ;;
  new-session) touch "$here/alive" ;;
  kill-session)
    rm -f "$here/alive"
    if [ -e "$here/pid" ]; then kill -HUP "$(cat "$here/pid")"; fi ;;
  list-panes)
    [ -e "$here/alive" ] || exit 1
    case "$*" in
      *pane_pid*) echo "0 $(cat "$here/pid" 2>/dev/null)" ;;
      *) echo 0 ;;
    esac ;;
  display-message) cat "$here/current" 2>/dev/null ;;
  show-options) echo C-b ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Every call the stub `tmux` has had, oldest first, each as its arguments.
#[cfg(unix)]
fn tmux_calls(script: &std::path::Path) -> Vec<Vec<String>> {
    let dir = script.parent().unwrap().join("calls");
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .split_terminator('\0')
                .map(str::to_string)
                .collect()
        })
        .collect()
}

#[cfg(unix)]
fn calls_of(script: &std::path::Path, verb: &str) -> Vec<Vec<String>> {
    tmux_calls(script)
        .into_iter()
        .filter(|c| c.first().map(String::as_str) == Some(verb))
        .collect()
}

/// A hook fixture, as Claude would send it for `session` working in `cwd`.
/// `SessionStart-<source>` is the SessionStart fixture with that source.
fn hook_input(event_fixture: &str, session: &str, cwd: &std::path::Path) -> String {
    let file = match event_fixture.strip_prefix("SessionStart-") {
        Some(_) => "SessionStart",
        None => event_fixture,
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks")
        .join(format!("{file}.json"));
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    value["session_id"] = session.into();
    value["cwd"] = cwd.to_string_lossy().to_string().into();
    if let Some(source) = event_fixture.strip_prefix("SessionStart-") {
        value["source"] = source.into();
    }
    value.to_string()
}

impl StoreLab {
    /// `jeet task-hook <event>` with `input` on stdin, the way Claude runs it.
    fn hook(&self, event: &str, input: &str, env: &[(&str, &str)]) -> std::process::Output {
        use std::io::Write;
        let mut cmd = jeet_bin();
        self.wire(&mut cmd)
            .args(["task-hook", event])
            .current_dir(self.home())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn index(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home().join("index.db")).unwrap()
    }

    /// A task's sessions as (id, parent), oldest first.
    fn sessions(&self, task_id: &str) -> Vec<(String, Option<String>)> {
        let db = self.index();
        let mut stmt = db
            .prepare(
                "SELECT session_id, parent_id FROM task_sessions WHERE task_id = ?1
                 ORDER BY created_at, rowid",
            )
            .unwrap();
        stmt.query_map([task_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

fn status_file(task: &std::path::Path, session: &str) -> serde_json::Value {
    let path = task.join(".jeet/status").join(format!("{session}.json"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// A hook must never put anything on stdout — Claude would read it — and
/// never fail.
fn hook_ok(output: &std::process::Output) {
    succeeded(output);
    assert_eq!(stdout_of(output), "", "a hook printed to stdout");
}

#[test]
fn task_hook_writes_the_status_each_event_leaves() {
    let lab = StoreLab::new();
    let task = lab.new_task("hooked");
    let id = task_id(&task);
    let session = "11111111-2222-4333-8444-555555555555";
    // Claude's directory may be anywhere in the task.
    let cwd = task.join("notes");
    std::fs::create_dir_all(&cwd).unwrap();

    let cases: [(&str, &str, Option<&str>); 8] = [
        ("SessionStart", "idle", None),
        (
            "UserPromptSubmit",
            "running",
            Some("Fix the login page so that the session cookie survives a pa…"),
        ),
        ("PreToolUse", "running", Some("Bash")),
        ("SubagentStop", "running", None),
        (
            "Notification",
            "waiting",
            Some("Claude needs your permission to use Bash"),
        ),
        ("Notification-idle", "idle", None),
        ("Stop", "idle", None),
        ("SessionEnd", "ended", None),
    ];
    for (fixture, state, detail) in cases {
        let event = fixture.split('-').next().unwrap();
        let output = lab.hook(event, &hook_input(fixture, session, &cwd), &[]);
        hook_ok(&output);
        let status = status_file(&task, session);
        assert_eq!(status["state"], state, "{fixture}: {status}");
        assert_eq!(status["detail"].as_str(), detail, "{fixture}: {status}");
        assert_eq!(status["event"], event);
        assert_eq!(status["session_id"], session);
    }
    // Written whole: nothing half-done is left beside the status files.
    let leftovers: Vec<_> = std::fs::read_dir(task.join(".jeet/status"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n != &format!("{session}.json"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // SessionStart recorded the session; clearing the conversation starts
    // another, which follows on from it.
    assert_eq!(lab.sessions(&id), [(session.to_string(), None)]);
    let cleared = "66666666-7777-4888-8999-000000000000";
    hook_ok(&lab.hook(
        "SessionStart",
        &hook_input("SessionStart-clear", cleared, &task),
        &[],
    ));
    assert_eq!(
        lab.sessions(&id),
        [
            (session.to_string(), None),
            (cleared.to_string(), Some(session.to_string()))
        ]
    );
    // Seen again on resume, it keeps its parent.
    hook_ok(&lab.hook(
        "SessionStart",
        &hook_input("SessionStart-resume", cleared, &task),
        &[],
    ));
    assert_eq!(lab.sessions(&id)[1].1.as_deref(), Some(session));
}

#[test]
fn task_hook_finds_its_task_or_quietly_does_nothing() {
    let lab = StoreLab::new();
    let task = lab.new_task("elsewhere");
    let id = task_id(&task);
    let outside = TempDir::new().unwrap();
    let session = "abcdefab-1111-4222-8333-444444444444";

    // Outside any task, a hook has nobody to report to.
    hook_ok(&lab.hook("Stop", &hook_input("Stop", session, outside.path()), &[]));
    assert!(!task.join(".jeet/status").exists());
    assert!(!outside.path().join(".jeet").exists());

    // The task in the environment is enough.
    hook_ok(&lab.hook(
        "PreToolUse",
        &hook_input("PreToolUse", session, outside.path()),
        &[("JEET_TASK_ID", &id)],
    ));
    assert_eq!(status_file(&task, session)["state"], "running");

    // Garbage, nothing at all, a session id that climbs out of the status
    // directory, an event jeet does not know: none of it fails the agent.
    hook_ok(&lab.hook("Stop", "not json", &[]));
    hook_ok(&lab.hook("Stop", "", &[]));
    hook_ok(&lab.hook("Stop", &hook_input("Stop", "../../escape", &task), &[]));
    assert!(!task.join(".jeet/escape.json").exists());
    assert!(!task.join("escape.json").exists());
    hook_ok(&lab.hook(
        "PostToolUse",
        &hook_input("PreToolUse", "other-session", &task),
        &[],
    ));
    assert!(!task.join(".jeet/status/other-session.json").exists());
}

#[cfg(unix)]
#[test]
fn task_open_runs_claude_in_tmux_with_jeet_hooks() {
    let lab = StoreLab::new();
    let task = lab.new_task("Fix login");
    let id = task_id(&task);
    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let claude_dir = scratch.path().join("claude");
    let tmux_env = tmux.to_string_lossy().to_string();
    let claude_env = claude_dir.to_string_lossy().to_string();
    let env = [
        ("JEET_TMUX", tmux_env.as_str()),
        ("JEET_AGENT", "claude --model opus"),
        ("CLAUDE_CONFIG_DIR", claude_env.as_str()),
    ];

    // Started detached; without a terminal only the directory is printed.
    let output = lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env);
    succeeded(&output);
    assert_eq!(path_from(&output), task);
    assert!(
        stderr_of(&output).contains("tmux attach -t jeet-fix-login"),
        "{}",
        stderr_of(&output)
    );
    let started = calls_of(&tmux, "new-session");
    assert_eq!(started.len(), 1);
    let argv = &started[0];
    let home = lab.home().canonicalize().unwrap();
    let dash = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(
        argv[..dash],
        [
            "new-session".to_string(),
            "-d".into(),
            "-s".into(),
            "jeet-fix-login".into(),
            "-c".into(),
            task.to_string_lossy().to_string(),
            "-e".into(),
            format!("JEET_TASK_ID={id}"),
            "-e".into(),
            format!("JEET_HOME={}", home.display()),
            "-e".into(),
            "JEET_CD_FILE=".into(),
        ]
    );
    let agent = &argv[dash + 1..];
    assert_eq!(agent[..4], ["claude", "--model", "opus", "--session-id"]);
    let session = agent[4].clone();
    assert_eq!(agent[5..7], ["-n".to_string(), "Fix login".to_string()]);
    assert_eq!(agent[7], "--settings");
    assert_eq!(agent.len(), 9, "{agent:?}");
    let settings: serde_json::Value = serde_json::from_str(&agent[8]).unwrap();
    let stop = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        stop.starts_with('\'') && stop.ends_with("' task-hook Stop"),
        "{stop}"
    );
    assert_eq!(
        settings["permissions"]["allow"],
        serde_json::json!(["Bash(jeet worktree:*)", "Bash(jeet ls:*)"])
    );
    // Known before the agent ever ran.
    assert_eq!(lab.sessions(&id), [(session.clone(), None)]);
    let hint = calls_of(&tmux, "set-option");
    assert_eq!(
        hint[0],
        [
            "set-option",
            "-t",
            "=jeet-fix-login",
            "status-right",
            " C-b d → jeet "
        ]
    );

    // Running already: nothing new starts, and the session asked for is not
    // forced on it.
    let output = lab.jeet_at(
        &["task", "open", "fix-login", "--session", &session],
        &lab.home(),
        &env,
    );
    succeeded(&output);
    assert_eq!(path_from(&output), task);
    assert!(stderr_of(&output).contains("already running"));
    assert_eq!(calls_of(&tmux, "new-session").len(), 1);
    assert!(calls_of(&tmux, "has-session")
        .iter()
        .all(|c| c == &["has-session", "-t", "=jeet-fix-login"]));

    // What the hooks report shows in the task list.
    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &env);
    assert!(
        stdout_of(&output).contains("running"),
        "{}",
        stdout_of(&output)
    );
    hook_ok(&lab.hook(
        "UserPromptSubmit",
        &hook_input("UserPromptSubmit", &session, &task),
        &[],
    ));
    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &env);
    assert!(
        stdout_of(&output).contains("running – Fix the login page"),
        "{}",
        stdout_of(&output)
    );
    // The agent dies without a word: its status is stale, and says so.
    std::fs::remove_file(tmux.parent().unwrap().join("alive")).unwrap();
    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &env);
    assert!(
        stdout_of(&output).contains("not running"),
        "{}",
        stdout_of(&output)
    );

    // Opened again, the session is resumed once Claude has a transcript of
    // it; until then it is started afresh under the same id.
    let output = lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env);
    succeeded(&output);
    let again = calls_of(&tmux, "new-session").pop().unwrap();
    assert!(
        again.windows(2).any(|w| w == ["--session-id", &session]),
        "{again:?}"
    );
    std::fs::remove_file(tmux.parent().unwrap().join("alive")).unwrap();
    let slug: String = task
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = claude_dir.join("projects").join(slug);
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(transcripts.join(format!("{session}.jsonl")), "{}\n").unwrap();
    let output = lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env);
    succeeded(&output);
    let resumed = calls_of(&tmux, "new-session").pop().unwrap();
    let dash = resumed.iter().position(|a| a == "--").unwrap();
    assert_eq!(
        resumed[dash + 1..dash + 6],
        ["claude", "--model", "opus", "--resume", &session]
    );
    assert!(!resumed.contains(&"--session-id".to_string()));
    assert_eq!(lab.sessions(&id).len(), 1);

    // A session the task never had is refused, before anything starts.
    std::fs::remove_file(tmux.parent().unwrap().join("alive")).unwrap();
    let before = calls_of(&tmux, "new-session").len();
    let output = lab.jeet_at(
        &["task", "open", "fix-login", "--session", "nope"],
        &lab.home(),
        &env,
    );
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("has no session nope"));
    assert_eq!(calls_of(&tmux, "new-session").len(), before);

    // Archiving ends the agent; so does deleting.
    succeeded(&lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env));
    succeeded(&lab.jeet_at(&["task", "archive", "fix-login"], &lab.home(), &env));
    assert_eq!(
        calls_of(&tmux, "kill-session"),
        [["kill-session", "-t", "=jeet-fix-login"]]
    );
    let output = lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env);
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("archived"));
    succeeded(&lab.jeet_at(&["task", "unarchive", "fix-login"], &lab.home(), &env));
    succeeded(&lab.jeet_at(&["task", "open", "fix-login"], &lab.home(), &env));
    succeeded(&lab.jeet_at(
        &["task", "delete", "fix-login", "--force"],
        &lab.home(),
        &env,
    ));
    assert_eq!(calls_of(&tmux, "kill-session").len(), 2);
    assert!(!task.exists());
}

#[cfg(unix)]
#[test]
fn an_agent_other_than_claude_just_runs_in_tmux() {
    let lab = StoreLab::new();
    lab.new_task("plain");
    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let tmux_env = tmux.to_string_lossy().to_string();
    let env = [
        ("JEET_TMUX", tmux_env.as_str()),
        ("JEET_AGENT", "aider --yes"),
    ];

    let output = lab.jeet_at(
        &["task", "open", "plain", "--session", "x"],
        &lab.home(),
        &env,
    );
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("keeps no sessions"));

    succeeded(&lab.jeet_at(&["task", "open", "plain"], &lab.home(), &env));
    let argv = calls_of(&tmux, "new-session").pop().unwrap();
    let dash = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[dash + 1..], ["aider", "--yes"]);
}

/// `kill-session` returns at once, while the agent may still be finishing a
/// tool call. Archive waits for it to be gone before it saves anything, so
/// what it writes on the way out is kept.
#[cfg(unix)]
#[test]
fn archive_keeps_what_the_agent_writes_as_it_exits() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let task = lab.new_task("slow exit");
    let worktree = lab.add(&task, &["widget"]);
    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let stub_dir = tmux.parent().unwrap().to_path_buf();
    let ready = scratch.path().join("ready");

    // An agent that takes a moment, once hung up on, to write its last file.
    let mut agent = Command::new("sh")
        .arg("-c")
        .arg(
            r#"trap 'sleep 1; echo late > "$WT/late.txt"; exit 0' HUP
               touch "$READY"
               while :; do sleep 0.1; done"#,
        )
        .env("WT", &worktree)
        .env("READY", &ready)
        .spawn()
        .unwrap();
    while !ready.exists() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(stub_dir.join("pid"), agent.id().to_string()).unwrap();
    std::fs::write(stub_dir.join("alive"), "").unwrap();

    let tmux_env = tmux.to_string_lossy().to_string();
    let env = [("JEET_TMUX", tmux_env.as_str())];
    succeeded(&lab.jeet_at(&["task", "archive", "slow-exit"], &lab.home(), &env));
    assert!(
        agent.try_wait().unwrap().is_some(),
        "archive returned while the agent was still running"
    );
    assert!(!worktree.exists());

    succeeded(&lab.jeet_at(&["task", "unarchive", "slow-exit"], &lab.home(), &env));
    assert_eq!(
        std::fs::read_to_string(worktree.join("late.txt")).unwrap(),
        "late\n"
    );
}

/// Ending the agent's session ends everything in it, so jeet run from
/// inside it would die before archiving or deleting anything.
#[cfg(unix)]
#[test]
fn archive_and_delete_refuse_inside_the_agents_session() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let task = lab.new_task("inside");
    let id = task_id(&task);
    let worktree = lab.add(&task, &["widget"]);
    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let stub_dir = tmux.parent().unwrap().to_path_buf();
    std::fs::write(stub_dir.join("alive"), "").unwrap();
    let tmux_env = tmux.to_string_lossy().to_string();
    let base = ("JEET_TMUX", tmux_env.as_str());
    let refused = |output: &std::process::Output| {
        assert!(!output.status.success());
        assert!(
            stderr_of(output).contains("inside tmux session jeet-inside"),
            "{}",
            stderr_of(output)
        );
        assert!(calls_of(&tmux, "kill-session").is_empty());
        assert!(worktree.exists());
    };

    // Run by the agent, or anything it started.
    refused(&lab.jeet_at(
        &["task", "archive", "inside"],
        &lab.home(),
        &[base, ("JEET_TASK_ID", &id)],
    ));

    // Run in a window the user opened in the agent's session.
    std::fs::write(stub_dir.join("current"), "jeet-inside\n").unwrap();
    let in_tmux = [
        base,
        ("TMUX", "/tmp/tmux-1/default,1,0"),
        ("TMUX_PANE", "%3"),
    ];
    refused(&lab.jeet_at(&["task", "archive", "inside"], &lab.home(), &in_tmux));
    assert!(calls_of(&tmux, "display-message")
        .iter()
        .all(|c| c == &["display-message", "-p", "-t", "%3", "#{session_name}"]));
    refused(&lab.jeet_at(
        &["task", "delete", "inside", "--force"],
        &lab.home(),
        &in_tmux,
    ));
    let state = |id: &str| -> String {
        lab.index()
            .query_row("SELECT state FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(state(&id), "active");
    assert!(task.is_dir());

    // From any other session it goes ahead.
    std::fs::write(stub_dir.join("current"), "elsewhere\n").unwrap();
    succeeded(&lab.jeet_at(&["task", "archive", "inside"], &lab.home(), &in_tmux));
    assert_eq!(calls_of(&tmux, "kill-session").len(), 1);
    assert!(!worktree.exists());
    assert_eq!(state(&id), "archived");
}

/// Kills the private tmux server a test started, however the test ends.
struct TmuxServer(std::path::PathBuf);

impl TmuxServer {
    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .args(args)
            .env("TMUX_TMPDIR", &self.0)
            .env_remove("TMUX")
            .output()
            .unwrap()
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

/// The real thing, on a private tmux server. Where tmux is missing the test
/// says so and passes.
#[test]
fn a_task_agent_lives_in_a_real_tmux_session() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let lab = StoreLab::new();
    lab.new_task("real");
    let sockets = TempDir::new().unwrap();
    let server = TmuxServer(sockets.path().to_path_buf());
    let sockets_env = sockets.path().to_string_lossy().to_string();
    let env = [
        ("JEET_TMUX", "tmux"),
        ("TMUX_TMPDIR", sockets_env.as_str()),
        ("JEET_AGENT", "sleep 300"),
    ];

    succeeded(&lab.jeet_at(&["task", "open", "real"], &lab.home(), &env));
    assert!(server
        .tmux(&["has-session", "-t", "=jeet-real"])
        .status
        .success());
    let output = lab.jeet_at(&["task", "ls"], &lab.home(), &env);
    assert!(
        stdout_of(&output).contains("running"),
        "{}",
        stdout_of(&output)
    );

    succeeded(&lab.jeet_at(&["task", "archive", "real"], &lab.home(), &env));
    assert!(!server
        .tmux(&["has-session", "-t", "=jeet-real"])
        .status
        .success());
}

/// The project's agent arguments follow the configured agent's into the
/// task's tmux session.
#[cfg(unix)]
#[test]
fn a_project_task_agent_gets_the_projects_arguments() {
    let lab = StoreLab::new();
    let home = lab.home();
    let output = lab.jeet_at(&["projects", "new", "web"], &home, &[]);
    succeeded(&output);
    let toml = path_from(&output).join("project.toml");
    let mut text = std::fs::read_to_string(&toml).unwrap();
    text.push_str("\n[agent]\nargs = [\"--verbose\"]\n");
    std::fs::write(&toml, text).unwrap();
    succeeded(&lab.jeet_at(&["task", "new", "Site", "--project", "web"], &home, &[]));

    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let tmux_env = tmux.to_string_lossy().to_string();
    let env = [
        ("JEET_TMUX", tmux_env.as_str()),
        ("JEET_AGENT", "aider --yes"),
    ];
    succeeded(&lab.jeet_at(&["task", "open", "site"], &home, &env));
    let argv = calls_of(&tmux, "new-session").pop().unwrap();
    let dash = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[dash + 1..], ["aider", "--yes", "--verbose"]);
}

// ---------------------------------------------------------------------------
// the tasks list
// ---------------------------------------------------------------------------

#[test]
fn the_tasks_list_needs_a_terminal_and_prints_nothing() {
    let lab = StoreLab::new();
    for args in [vec![], vec!["tasks"]] {
        let output = lab.jeet_at(&args, &lab.home(), &[]);
        assert!(!output.status.success());
        assert!(
            stderr_of(&output).contains("interactive terminal"),
            "{}",
            stderr_of(&output)
        );
        assert_eq!(stdout_of(&output), "", "stdout stays clean");
    }
    // Off a terminal, `jeet projects` is still the list it always was.
    let output = lab.jeet_at(&["projects"], &lab.home(), &[]);
    succeeded(&output);
    assert!(stderr_of(&output).contains("no projects"));
}

/// Whether `script` can give a command a terminal here.
fn have_script() -> bool {
    use std::process::Stdio;
    Command::new("script")
        .arg("-q")
        .arg("/dev/null")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// A jeet screen running in a terminal of its own, typed at through `script`.
struct Pty {
    child: std::process::Child,
    keys: std::process::ChildStdin,
    /// Everything the screen drew.
    log: std::path::PathBuf,
    _dir: TempDir,
}

impl Pty {
    /// `jeet <args>` in `cwd`, with `env` on top of the hermetic defaults.
    fn open(lab: &StoreLab, args: &str, cwd: &std::path::Path, env: &[(&str, &str)]) -> Pty {
        use std::process::Stdio;
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("typescript");
        // The tasks list looks PRs up through gh as it opens: never the real
        // one, which would reach GitHub. A test that wants answers passes a
        // PATH with a stub of its own.
        let no_gh = dir.path().join("bin");
        std::fs::create_dir_all(&no_gh).unwrap();
        write_script(
            &no_gh.join("gh"),
            "#!/bin/sh\necho 'no GitHub in tests' >&2\nexit 1\n",
        );
        let path = format!(
            "{}:{}",
            no_gh.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let inner = format!(
            "stty rows 40 cols 140 && exec '{}' {args}",
            env!("CARGO_BIN_EXE_jeet")
        );
        let mut cmd = Command::new("script");
        if cfg!(target_os = "linux") {
            cmd.args(["-q", "-f", "-e", "-c", &inner]).arg(&log);
        } else {
            cmd.args(["-q", "-F"]).arg(&log).args(["sh", "-c", &inner]);
        }
        hermetic_git(&mut cmd);
        lab.wire(&mut cmd);
        cmd.current_dir(cwd)
            .env("JEET_SYNC_INTERVAL_SECS", "0")
            .env_remove("JEET_TASK_ID")
            .env_remove("JEET_CD_FILE")
            .env("JEET_TMUX", "/nonexistent/jeet-test-tmux")
            .env_remove("TMUX")
            .env("CLAUDE_CONFIG_DIR", "/nonexistent/jeet-test-claude")
            .env("TERM", "xterm-256color")
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        let keys = child.stdin.take().unwrap();
        Pty {
            child,
            keys,
            log,
            _dir: dir,
        }
    }

    fn type_keys(&mut self, text: &[u8]) {
        use std::io::Write;
        self.keys.write_all(text).unwrap();
        self.keys.flush().unwrap();
    }

    /// Esc on its own: straight after it, a key would read as alt-key.
    fn esc(&mut self) {
        self.type_keys(b"\x1b");
        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log).unwrap_or_default()).to_string()
    }

    /// Wait for the screen to have drawn `text`, then a moment more for a
    /// spinner drawn alongside it to finish.
    fn wait_for_text(&self, text: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !self.screen().contains(text) {
            if std::time::Instant::now() > deadline {
                // What was drawn, without the escape sequences.
                let mut plain = String::new();
                let mut escaped = false;
                for c in self.screen().chars() {
                    match (escaped, c) {
                        (false, '\x1b') => escaped = true,
                        (false, c) => plain.push(c),
                        (true, c) if c.is_ascii_alphabetic() => escaped = false,
                        _ => {}
                    }
                }
                let tail: String = plain.chars().rev().take(3000).collect();
                let tail: String = tail.chars().rev().collect();
                panic!("timed out waiting for the screen to show {text:?}; it drew:\n{tail}");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the screen did not close:\n{}", self.screen());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Whatever made it true may still be finishing on screen.
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn the_tasks_list_makes_archives_and_deletes_tasks() {
    if !have_script() {
        eprintln!("no `script` to run a terminal with; skipping");
        return;
    }
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let listed = |archived: bool| {
        let mut args = vec!["task", "ls"];
        if archived {
            args.push("--archived");
        }
        stdout_of(&lab.jeet(&args))
    };

    let mut screen = Pty::open(&lab, "tasks", &lab.home(), &[]);
    screen.wait_for_text("no tasks yet");

    // A blank form makes an empty canvas under a made-up name.
    screen.type_keys(b"n");
    screen.wait_for_text("new task");
    screen.type_keys(b"\r");
    wait_until("the empty task", || listed(false).contains("task-1"));
    assert!(listed(false).contains("empty"), "{}", listed(false));

    // One named, with a worktree of the repository picked from the store.
    screen.type_keys(b"n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    screen.type_keys(b"Widget work\t\twidg ");
    screen.type_keys(b"\r");
    wait_until("the task with a worktree", || {
        listed(false).contains("widget-work")
    });
    assert!(listed(false).contains("1 wt: widget"), "{}", listed(false));
    let task = lab.home().join("tasks/widget-work");
    assert!(task.join("widget/.git").exists());

    // Its worktrees panel, then the explorer straight on its only worktree,
    // and esc back to the list.
    screen.type_keys(b"w");
    screen.wait_for_text("worktrees · ⏎ explore");
    screen.esc();
    screen.type_keys(b"e");
    screen.wait_for_text("acme/widget · type to filter");
    screen.esc();
    // Back on the list, which carries on: `q` there quits.

    // A project, from the projects panel.
    screen.type_keys(b"P");
    screen.wait_for_text("no projects yet");
    screen.type_keys(b"n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    screen.type_keys(b"webproj\r");
    wait_until("the project", || {
        stdout_of(&lab.jeet(&["projects", "ls"])).contains("webproj")
    });
    screen.esc();

    // The new task is selected; archiving it frees its worktree.
    screen.type_keys(b"a");
    wait_until("the archive", || listed(true).contains("widget-work"));
    assert!(!task.join("widget").exists());

    // Over in the archived view, delete it, confirming.
    screen.type_keys(b"\tD");
    screen.wait_for_text("delete task Widget work");
    screen.type_keys(b"y");
    wait_until("the delete", || !listed(true).contains("widget-work"));
    assert!(!task.exists());
    assert!(
        listed(false).contains("task-1"),
        "the other task is untouched"
    );

    screen.type_keys(b"q");
    let status = screen.wait_for_exit();
    assert!(status.success(), "jeet tasks exited with {status}");
}

#[test]
fn enter_in_the_tasks_list_offers_the_sessions_then_attaches() {
    if !have_script() {
        eprintln!("no `script` to run a terminal with; skipping");
        return;
    }
    let lab = StoreLab::new();
    let task = lab.new_task("Fix login");
    let scratch = TempDir::new().unwrap();
    let tmux = stub_tmux(scratch.path());
    let claude_dir = scratch.path().join("claude");
    let tmux_env = tmux.to_string_lossy().to_string();
    let claude_env = claude_dir.to_string_lossy().to_string();
    let env = [
        ("JEET_TMUX", tmux_env.as_str()),
        ("JEET_AGENT", "claude"),
        ("CLAUDE_CONFIG_DIR", claude_env.as_str()),
    ];
    // Two sessions the agent reported, a second apart.
    let (older, newer) = (
        "aaaaaaaa-1111-4111-8111-111111111111",
        "bbbbbbbb-2222-4222-8222-222222222222",
    );
    for session in [older, newer] {
        let output = lab.hook(
            "SessionStart",
            &hook_input("SessionStart", session, &task),
            &[],
        );
        hook_ok(&output);
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    let mut screen = Pty::open(&lab, "tasks", &lab.home(), &env);
    screen.wait_for_text("jeet · tasks");
    // No agent is running, and there are two sessions: which one?
    screen.type_keys(b"\r");
    screen.wait_for_text("which session?");
    assert!(
        calls_of(&tmux, "new-session").is_empty(),
        "nothing started yet"
    );
    // The newest is first; take the older one.
    screen.type_keys(b"j\r");
    wait_until("the attach", || {
        !calls_of(&tmux, "attach-session").is_empty()
    });

    let started = calls_of(&tmux, "new-session");
    assert_eq!(started.len(), 1, "{started:?}");
    let argv = &started[0];
    let at = argv.iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(argv[at + 1], older, "{argv:?}");
    let attached = calls_of(&tmux, "attach-session");
    assert_eq!(
        attached[0][1..],
        ["-t".to_string(), "=jeet-fix-login".into()]
    );

    // The agent is running now: ⏎ attaches straight away, no picker.
    screen.type_keys(b"\r");
    wait_until("the second attach", || {
        calls_of(&tmux, "attach-session").len() == 2
    });
    assert_eq!(calls_of(&tmux, "new-session").len(), 1);

    screen.type_keys(b"q");
    let status = screen.wait_for_exit();
    assert!(status.success(), "jeet tasks exited with {status}");
}

#[test]
fn explore_inside_a_task_directory_browses_its_canvas() {
    if !have_script() {
        eprintln!("no `script` to run a terminal with; skipping");
        return;
    }
    let lab = StoreLab::new();
    let task = lab.new_task("Fix login");
    std::fs::create_dir(task.join("notes")).unwrap();
    let mut screen = Pty::open(&lab, "explore", &task.join("notes"), &[]);
    screen.wait_for_text("jeet · task Fix login");
    screen.esc();
    let status = screen.wait_for_exit();
    assert!(status.success(), "jeet explore exited with {status}");
}

/// Write an executable script.
fn write_script(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ---------------------------------------------------------------------------
// PR history and review tasks
// ---------------------------------------------------------------------------

/// A stand-in `gh` for PR history and review tasks, recording each call's
/// arguments NUL-separated, one file per call, under `bin/calls`:
///
/// - `pr view <n>`: PR `n` on branch `wide-store` (#7) or `pr-<n>`;
///   with `--web`, nothing.
/// - `pr checkout <n> --branch <b>` checks `b` out; `--detach` stays put.
/// - `pr list --head feat`: #3, merged, plus a fork's #4 from a branch of the
///   same name; `--search <sha>`: #5, an open draft; anything else, none.
#[cfg(unix)]
fn stub_history_gh(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("gh-bin");
    std::fs::create_dir_all(bin.join("calls")).unwrap();
    write_script(
        &bin.join("gh"),
        r#"#!/bin/sh
here=$(dirname "$0")
printf '%s\0' "$@" > "$here/calls/.call.$$"
mv "$here/calls/.call.$$" "$here/calls/call.$$"
[ "$GH_PROMPT_DISABLED" = 1 ] || { echo "gh run without GH_PROMPT_DISABLED" >&2; exit 3; }
pr() {
  # number title state draft branch cross additions deletions
  printf '{"number":%s,"title":"%s","url":"https://github.com/acme/widget/pull/%s","state":"%s","isDraft":%s,"reviewDecision":"","headRefName":"%s","headRefOid":"abc123","baseRefName":"main","isCrossRepository":%s,"additions":%s,"deletions":%s,"updatedAt":"2026-10-0%sT00:00:00Z"}' \
    "$1" "$2" "$1" "$3" "$4" "$5" "$6" "$7" "$8" "$(( $1 % 9 + 1 ))"
}
case "$1 $2" in
  "pr view")
    for arg in "$@"; do [ "$arg" = --web ] && exit 0; done
    case "$3" in
      7) pr 7 "Widen the store" OPEN false wide-store false 12 3 ;;
      *) pr "$3" "PR $3" OPEN false "pr-$3" false 1 1 ;;
    esac
    exit 0 ;;
  "pr checkout")
    case "$4" in
      --detach) exit 0 ;;
      *) exec git checkout -q -B "$5" ;;
    esac ;;
  "pr list")
    while [ $# -gt 0 ]; do
      case "$1" in
        --head)
          [ "$2" = feat ] || { echo '[]'; exit 0; }
          printf '['; pr 3 "Fix the feature" MERGED false feat false 10 2
          printf ','; pr 4 "A fork with a feat branch" OPEN false feat true 99 99
          echo ']'; exit 0 ;;
        --search)
          printf '['; pr 5 "Detached work" OPEN true x false 7 0; echo ']'; exit 0 ;;
      esac
      shift
    done
    echo '[]'; exit 0 ;;
esac
echo "unexpected gh $*" >&2
exit 1
"#,
    );
    bin
}

/// `PATH` with `bin` in front.
fn path_with(bin: &std::path::Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[cfg(unix)]
#[test]
fn review_makes_a_review_task_and_comes_back_to_it() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let bin = stub_history_gh(&lab.home());
    let path = path_with(&bin);
    let env = [("PATH", path.as_str())];

    let output = lab.jeet_at(&["review", "7", "--no-command"], &trunk, &env);
    succeeded(&output);
    let worktree = path_from(&output);
    let task = lab.home().join("tasks/review-widget-7");
    assert_eq!(worktree, task.join("widget"));
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &worktree),
        "wide-store"
    );
    assert!(stderr_of(&output).contains("new task review-widget-7"));
    // A task like any other, marked as a review, with its PR in the history.
    let listed = stdout_of(&lab.jeet_at(&["task", "ls"], &lab.home(), &[]));
    assert!(listed.contains("review-widget-7"), "{listed}");
    assert!(listed.contains("1 wt: widget"), "{listed}");
    let prs = lab.jeet_at(
        &["task", "prs", "review-widget-7", "--offline"],
        &lab.home(),
        &[],
    );
    succeeded(&prs);
    let prs = stdout_of(&prs);
    assert!(prs.contains("#7"), "{prs}");
    assert!(prs.contains("Widen the store"), "{prs}");
    assert!(prs.contains("+12") && prs.contains("-3"), "{prs}");

    // Again: the same task and checkout, brought up to date with the PR.
    let output = lab.jeet_at(&["review", "#7", "--no-command"], &trunk, &env);
    succeeded(&output);
    assert_eq!(path_from(&output), worktree);
    assert!(
        !stderr_of(&output).contains("new task"),
        "{}",
        stderr_of(&output)
    );
    assert!(!lab.home().join("tasks/review-widget-7-2").exists());

    // Its checkout deleted behind jeet's back, the next review makes it
    // again, on the branch.
    std::fs::remove_dir_all(&worktree).unwrap();
    let output = lab.jeet_at(&["review", "7", "--no-command"], &trunk, &env);
    succeeded(&output);
    assert_eq!(path_from(&output), worktree);
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &worktree),
        "wide-store"
    );

    // Archived, it is brought back rather than made again.
    succeeded(&lab.jeet_at(&["task", "archive", "review-widget-7"], &lab.home(), &[]));
    assert!(!worktree.exists());
    let output = lab.jeet_at(&["review", "7", "--no-command"], &trunk, &env);
    succeeded(&output);
    assert_eq!(path_from(&output), worktree);
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &worktree),
        "wide-store"
    );
    let listed = stdout_of(&lab.jeet_at(&["task", "ls"], &lab.home(), &[]));
    let reviews = listed.lines().filter(|l| l.contains("review-widget"));
    assert_eq!(reviews.count(), 1, "{listed}");

    // Deleting the task takes the branch it made for the review.
    succeeded(&lab.jeet_at(
        &["task", "delete", "review-widget-7", "--force"],
        &lab.home(),
        &[],
    ));
    assert!(!task.exists());
    assert_eq!(git_stdout(&["branch", "--list", "wide-store"], &trunk), "");
}

#[cfg(unix)]
#[test]
fn a_review_of_a_branch_another_task_has_is_detached() {
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let bin = stub_history_gh(&lab.home());
    let path = path_with(&bin);
    // The author's own task has the PR's branch.
    let mine = lab.new_task("Wide store");
    let own = lab.add(&mine, &["widget", "--branch", "wide-store"]);

    let output = lab.jeet_at(&["review", "7", "--no-command"], &trunk, &[("PATH", &path)]);
    succeeded(&output);
    let worktree = path_from(&output);
    assert!(worktree.starts_with(lab.home().join("tasks/review-widget-7")));
    assert!(is_detached(&worktree), "the branch stays where it was");
    assert!(
        stderr_of(&output).contains("checked out in task Wide store"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"], &own),
        "wide-store"
    );
    let checkouts: Vec<_> = gh_calls(&bin)
        .into_iter()
        .filter(|c| c.starts_with(&["pr".into(), "checkout".into()]))
        .collect();
    assert_eq!(checkouts.len(), 1, "{checkouts:?}");
    assert!(
        checkouts[0].contains(&"--detach".to_string()),
        "{checkouts:?}"
    );
}

#[cfg(unix)]
#[test]
fn task_prs_finds_prs_by_branch_and_by_detached_commit() {
    let lab = StoreLab::new();
    lab.clone_repo("widget");
    let bin = stub_history_gh(&lab.home());
    let path = path_with(&bin);
    let task = lab.new_task("Feature");
    let on_branch = lab.add(&task, &["widget", "--branch", "feat"]);
    // Work on a detached HEAD that origin does not have.
    let detached = lab.add(&task, &["widget"]);
    git(
        &["commit", "-q", "--allow-empty", "-m", "detached work"],
        &detached,
    );
    let work = rev(&detached, "HEAD");
    // A fresh checkout of the default branch: nothing of this task's.
    lab.add(&task, &["widget"]);

    let output = lab.jeet_at(&["task", "prs"], &task, &[("PATH", &path)]);
    succeeded(&output);
    let listed = stdout_of(&output);
    assert!(
        listed.contains("#3") && listed.contains("merged"),
        "{listed}"
    );
    assert!(listed.contains("+10") && listed.contains("-2"), "{listed}");
    assert!(
        listed.contains("#5") && listed.contains("draft"),
        "{listed}"
    );
    assert!(
        !listed.contains("#4"),
        "a fork's PR is not this task's: {listed}"
    );

    let calls = gh_calls(&bin);
    let searches: Vec<_> = calls
        .iter()
        .filter(|c| c.contains(&"--search".to_string()))
        .collect();
    assert_eq!(searches.len(), 1, "{calls:?}");
    assert!(searches[0].contains(&work), "{searches:?}");
    assert!(calls
        .iter()
        .any(|c| c.contains(&"--head".to_string()) && c.contains(&"feat".to_string())));

    // The history outlives the branch.
    succeeded(&lab.jeet_at(&["worktree", "rm", "widget", "--force"], &task, &[]));
    assert!(!on_branch.exists());
    let output = lab.jeet_at(&["task", "prs", "--offline"], &task, &[]);
    succeeded(&output);
    assert!(stdout_of(&output).contains("#3"), "{}", stdout_of(&output));
}

#[cfg(unix)]
#[test]
fn the_tasks_list_shows_pr_history_with_links_and_opens_the_viewer() {
    if !have_script() {
        eprintln!("no `script` to run a terminal with; skipping");
        return;
    }
    let lab = StoreLab::new();
    let trunk = lab.clone_repo("widget");
    let bin = stub_history_gh(&lab.home());
    let path = path_with(&bin);
    let task = lab.new_task("Feature");
    lab.add(&task, &["widget", "--branch", "feat"]);

    let mut screen = Pty::open(&lab, "tasks", &lab.home(), &[("PATH", &path)]);
    screen.wait_for_text("Feature");
    screen.type_keys(b"p");
    screen.wait_for_text("Fix the feature");
    let drawn = screen.screen();
    assert!(
        drawn.contains("\x1b]8;;https://github.com/acme/widget/pull/3\x1b\\"),
        "the title is a link"
    );
    assert!(!drawn.contains("A fork with a feat branch"));
    // o opens it in the browser, through gh.
    screen.type_keys(b"o");
    wait_until("the browser", || {
        gh_calls(&bin)
            .iter()
            .any(|c| c.contains(&"--web".to_string()) && c.contains(&"3".to_string()))
    });
    screen.esc();
    screen.type_keys(b"q");
    assert!(screen.wait_for_exit().success());

    // A review task opens in the review viewer on v, and esc comes back.
    let viewer = stub_viewer_gh(&lab.home());
    let path = path_with(&viewer);
    succeeded(&lab.jeet_at(&["review", "7", "--no-command"], &trunk, &[("PATH", &path)]));
    let mut screen = Pty::open(&lab, "tasks", &lab.home(), &[("PATH", &path)]);
    screen.wait_for_text("review-widget-7 (review)");
    screen.type_keys(b"/review\r");
    std::thread::sleep(std::time::Duration::from_millis(300));
    screen.type_keys(b"v");
    wait_for_calls(&viewer, 1, "the viewer's diff", |c| {
        c.starts_with(&["pr".into(), "diff".into()])
    });
    screen.wait_for_text("src/lib.rs");
    screen.esc();
    screen.type_keys(b"q");
    assert!(screen.wait_for_exit().success());
}
