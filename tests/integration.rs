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
fn worktree_add_ls_remove_flow() {
    let home = TempDir::new().unwrap();
    let env_home = home.path().to_string_lossy().to_string();

    let repo_dir = TempDir::new().unwrap();
    init_repo_with_remote(repo_dir.path(), "https://github.com/acme/demo.git");

    jeet_bin()
        .args(["adopt", repo_dir.path().to_str().unwrap()])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();

    let output = jeet_bin()
        .args(["worktree", "add", "acme/demo", "feature-a"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success(), "worktree add failed");

    let output = jeet_bin()
        .args(["path", "acme/demo", "--branch", "feature-a"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    let wt_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(std::path::Path::new(&wt_path).exists());

    let output = jeet_bin()
        .args(["worktree", "ls", "acme/demo"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("feature-a"));

    let output = jeet_bin()
        .args(["worktree", "remove", "acme/demo", "feature-a"])
        .env("JEET_HOME", &env_home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!std::path::Path::new(&wt_path).exists());
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
// explorer, worktree creation from anywhere, and cleaning
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
    bare: std::path::PathBuf,
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
        bare: bare.path().canonicalize().unwrap(),
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

    fn branches_on_remote(&self) -> String {
        let output = Command::new("git")
            .args(["branch", "--list"])
            .current_dir(&self.bare)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

#[test]
fn worktree_with_name_creates_branch_and_publishes_it() {
    let lab = lab("named");
    let nested = lab.repo.join("nested/deep");
    std::fs::create_dir_all(&nested).unwrap();

    let output = lab.jeet(&["worktree", "feature-x"], &nested);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(std::path::Path::new(&path).exists(), "worktree not created");
    assert!(path.contains("feature-x"));
    assert!(
        lab.branches_on_remote().contains("feature-x"),
        "branch was not published: {}",
        lab.branches_on_remote()
    );
}

#[test]
fn worktree_without_name_creates_a_detached_checkout() {
    let lab = lab("detached");

    let output = lab.jeet(&["worktree"], &lab.repo);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        path.contains("ephemeral"),
        "expected ephemeral path: {path}"
    );

    assert!(
        std::path::Path::new(&path).is_dir(),
        "path {path:?} missing; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let head = Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .current_dir(&path)
        .output()
        .unwrap();
    assert!(!head.status.success(), "HEAD should be detached");
}

#[test]
fn worktree_creation_hands_a_directory_back_to_the_shell() {
    let lab = lab("handoff");
    let cd_file = std::path::Path::new(&lab.home).join("cd-target");

    let output = jeet_bin()
        .args(["worktree", "handed-off", "--no-push"])
        .current_dir(&lab.repo)
        .env("JEET_HOME", &lab.home)
        .env("JEET_CD_FILE", &cd_file)
        .output()
        .unwrap();
    assert!(output.status.success());

    let target = std::fs::read_to_string(&cd_file).unwrap();
    assert!(target.contains("handed-off"), "cd hand-off was {target:?}");
    assert!(std::path::Path::new(target.trim()).is_dir());
}

#[test]
fn worktree_ls_reports_uncommitted_work_and_a_diff_counter() {
    let lab = lab("counters");
    let output = lab.jeet(&["worktree", "counted", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let wt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let wt = std::path::Path::new(&wt);

    std::fs::write(wt.join("tracked.txt"), "one\ntwo\n").unwrap();
    git(&["add", "-A"], wt);
    git(&["commit", "-qm", "add tracked"], wt);
    std::fs::write(wt.join("dirty.txt"), "x").unwrap();

    let output = lab.jeet(&["worktree", "ls", "acme/counters"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 uncommitted"), "{stdout}");
    assert!(stdout.contains("+2/-0 in 1 file"), "{stdout}");
}

#[test]
fn worktree_clean_keeps_work_and_removes_throwaway_checkouts() {
    let lab = lab("cleaning");

    // A branch worktree with uncommitted work must survive.
    let output = lab.jeet(&["worktree", "keep-me", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let keep = String::from_utf8_lossy(&output.stdout).trim().to_string();
    std::fs::write(std::path::Path::new(&keep).join("wip.txt"), "wip").unwrap();

    // A detached ephemeral checkout is disposable.
    let output = lab.jeet(&["worktree"], &lab.repo);
    assert!(output.status.success());
    let ephemeral = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let output = lab.jeet(&["worktree", "clean", "--dry-run"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[keep]"), "{stdout}");
    assert!(stdout.contains("uncommitted change"), "{stdout}");
    assert!(std::path::Path::new(&ephemeral).exists(), "dry run deleted");

    let output = lab.jeet(&["worktree", "clean", "--yes"], &lab.repo);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!std::path::Path::new(&ephemeral).exists(), "not cleaned up");
    assert!(std::path::Path::new(&keep).exists(), "work was discarded");
}

#[test]
fn worktree_clean_refuses_to_discard_work_without_a_terminal() {
    let lab = lab("prompting");
    let output = lab.jeet(&["worktree"], &lab.repo);
    assert!(output.status.success());

    let output = lab.jeet(&["worktree", "clean"], &lab.repo);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
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
fn commands_work_from_inside_a_linked_worktree() {
    let lab = lab("nested");
    let output = lab.jeet(&["worktree", "inner", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let wt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let nested = std::path::Path::new(&wt).join("deep/deeper");
    std::fs::create_dir_all(&nested).unwrap();

    // Creating a worktree from inside another worktree still targets the trunk.
    let output = lab.jeet(&["worktree", "sibling", "--no-push"], &nested);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sibling = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(sibling.contains("sibling"));
    assert!(std::path::Path::new(&sibling).exists());
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
// renaming worktrees, including promoting a detached scratchpad
// ---------------------------------------------------------------------------

#[test]
fn rename_promotes_a_detached_scratchpad_and_keeps_the_work() {
    let lab = lab("scratchpad");

    let output = lab.jeet(&["worktree"], &lab.repo);
    assert!(output.status.success());
    let scratch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let scratch = std::path::Path::new(&scratch);
    assert!(scratch.to_string_lossy().contains("ephemeral"));

    // Work that must survive the promotion: one commit and one untracked file.
    std::fs::write(scratch.join("idea.txt"), "an idea\n").unwrap();
    git(&["add", "-A"], scratch);
    git(&["commit", "-qm", "the idea"], scratch);
    std::fs::write(scratch.join("still-thinking.txt"), "wip").unwrap();

    let output = lab.jeet(&["worktree", "rename", "login-page"], scratch);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let promoted = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let promoted = std::path::Path::new(&promoted);

    assert!(!scratch.exists(), "the scratchpad should have moved");
    assert!(promoted.is_dir(), "promoted worktree missing");
    assert!(
        !promoted.to_string_lossy().contains("ephemeral"),
        "still ephemeral: {}",
        promoted.display()
    );
    assert!(promoted.join("idea.txt").exists(), "commit lost");
    assert!(
        promoted.join("still-thinking.txt").exists(),
        "uncommitted work lost"
    );

    let head = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(promoted)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "login-page");
    assert!(
        lab.branches_on_remote().contains("login-page"),
        "branch was not published: {}",
        lab.branches_on_remote()
    );

    // The promoted worktree is no longer disposable.
    let output = lab.jeet(&["worktree", "clean", "--dry-run"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[keep]"), "{stdout}");
    assert!(stdout.contains("login-page"), "{stdout}");
}

#[test]
fn rename_moves_a_named_worktree_and_flags_the_stale_remote_branch() {
    let lab = lab("renaming");

    let output = lab.jeet(&["worktree", "old-name"], &lab.repo);
    assert!(output.status.success());
    let old = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let output = lab.jeet(&["worktree", "rename", "old-name", "new-name"], &lab.repo);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(new.ends_with("new-name"), "{new}");
    assert!(!std::path::Path::new(&old).exists());
    assert!(std::path::Path::new(&new).is_dir());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("origin/old-name still exists"), "{stderr}");
    assert!(lab.branches_on_remote().contains("new-name"));
}

#[test]
fn rename_refuses_names_that_are_already_taken() {
    let lab = lab("collision");
    lab.jeet(&["worktree", "first", "--no-push"], &lab.repo);
    lab.jeet(&["worktree", "second", "--no-push"], &lab.repo);

    let output = lab.jeet(&["worktree", "rename", "first", "second"], &lab.repo);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists"), "{stderr}");
}

#[test]
fn rename_takes_the_shell_with_it() {
    let lab = lab("following");
    let output = lab.jeet(&["worktree", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let scratch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let nested = std::path::Path::new(&scratch).join("nested");
    std::fs::create_dir_all(&nested).unwrap();

    let cd_file = std::path::Path::new(&lab.home).join("rename-cd");
    let output = jeet_bin()
        .args(["worktree", "rename", "named-now", "--no-push"])
        .current_dir(&nested)
        .env("JEET_HOME", &lab.home)
        .env("JEET_CD_FILE", &cd_file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // We were inside the worktree, in a subdirectory: land in the same place.
    let landed = std::fs::read_to_string(&cd_file).unwrap();
    assert!(landed.ends_with("named-now/nested"), "landed in {landed:?}");
    assert!(std::path::Path::new(landed.trim()).is_dir());
}

#[test]
fn renaming_someone_elses_worktree_leaves_the_shell_alone() {
    let lab = lab("elsewhere");
    lab.jeet(&["worktree", "target", "--no-push"], &lab.repo);

    let cd_file = std::path::Path::new(&lab.home).join("no-cd");
    let output = jeet_bin()
        .args(["worktree", "rename", "target", "retarget", "--no-push"])
        .current_dir(&lab.repo)
        .env("JEET_HOME", &lab.home)
        .env("JEET_CD_FILE", &cd_file)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        !cd_file.exists() || std::fs::read_to_string(&cd_file).unwrap().is_empty(),
        "should not move the shell into a worktree it was not in"
    );
}

// ---------------------------------------------------------------------------
// regressions found in review
// ---------------------------------------------------------------------------

/// A worktree must not be classified by a path comparison that symlinks break —
/// macOS resolves TempDir through /private/var, which used to make every
/// jeet-managed worktree look external and silently disable clean and rename.
#[test]
fn worktrees_are_recognised_through_a_symlinked_home() {
    let real = TempDir::new().unwrap();
    let link_parent = TempDir::new().unwrap();
    let link = link_parent.path().join("home-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    #[cfg(not(unix))]
    return;

    let repo = TempDir::new().unwrap();
    init_repo_with_remote(repo.path(), "https://github.com/acme/symlinked.git");
    let home = link.to_string_lossy().to_string();

    let ok = jeet_bin()
        .args(["adopt", repo.path().to_str().unwrap()])
        .env("JEET_HOME", &home)
        .output()
        .unwrap();
    assert!(ok.status.success());

    let output = jeet_bin()
        .args(["worktree", "through-link", "--no-push"])
        .current_dir(repo.path())
        .env("JEET_HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = jeet_bin()
        .args(["worktree", "ls", "acme/symlinked"])
        .env("JEET_HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[worktree]") && !stdout.contains("[external]"),
        "jeet did not recognise its own worktree: {stdout}"
    );
}

/// `default_branch` used to fall back to the literal "main", which left every
/// repo on another default branch unable to create a worktree at all.
#[test]
fn worktrees_work_on_a_repo_whose_default_branch_is_master() {
    let home = TempDir::new().unwrap();
    let home = home.path().to_string_lossy().to_string();
    let repo = TempDir::new().unwrap();
    init_repo_on_branch(
        repo.path(),
        "https://github.com/acme/mastered.git",
        "master",
    );
    jeet_bin()
        .args(["adopt", repo.path().to_str().unwrap()])
        .env("JEET_HOME", &home)
        .output()
        .unwrap();

    for args in [
        vec!["worktree", "on-master", "--no-push"],
        vec!["worktree", "--no-push"],
    ] {
        let output = jeet_bin()
            .args(&args)
            .current_dir(repo.path())
            .env("JEET_HOME", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(path_from(&output).is_dir());
    }
}

/// Branch slugs are lossy: `feat/x` and `feat-x` share a directory. Handing the
/// second one the first one's worktree drops you on the wrong branch.
#[test]
fn a_slug_collision_is_refused_rather_than_silently_reused() {
    let lab = lab("slugs");

    let output = lab.jeet(&["worktree", "feat/x", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let first = path_from(&output);
    assert!(first.ends_with("feat-x"));

    let output = lab.jeet(&["worktree", "feat-x", "--no-push"], &lab.repo);
    assert!(!output.status.success(), "collision was silently accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not the worktree for feat-x"), "{stderr}");
}

/// A directory that is not a worktree at all must never be adopted as one.
#[test]
fn a_stray_directory_is_not_mistaken_for_a_worktree() {
    let lab = lab("stray");
    let stray = std::path::Path::new(&lab.home).join("worktrees/github.com/acme/stray/leftover");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("junk.txt"), "left behind").unwrap();

    let output = lab.jeet(&["worktree", "leftover", "--no-push"], &lab.repo);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not the worktree for leftover"), "{stderr}");
}

/// `git status --porcelain` never lists ignored files, so a worktree holding
/// only a .env used to read as "nothing to lose" and be deleted unattended.
#[test]
fn clean_does_not_silently_discard_ignored_files() {
    let lab = lab("ignored");
    std::fs::write(lab.repo.join(".gitignore"), ".env\n").unwrap();
    git(&["add", "-A"], &lab.repo);
    git(&["commit", "-qm", "ignore env"], &lab.repo);

    let output = lab.jeet(&["worktree", "has-secrets", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let wt = path_from(&output);
    std::fs::write(wt.join(".env"), "AWS_SECRET=hunter2").unwrap();

    // Unattended: nobody reads the report, so it must not decide to destroy.
    let output = lab.jeet(&["worktree", "clean", "--yes"], &lab.repo);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[keep]"), "{stdout}");
    assert!(stdout.contains("ignored file"), "{stdout}");
    assert!(wt.join(".env").exists(), "the .env was destroyed");

    // ...but it does say what would go, so an interactive run can decide.
    let output = lab.jeet(&["worktree", "clean", "--dry-run"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 ignored file will be deleted"),
        "{stdout}"
    );
    assert!(!stdout.contains("nothing to lose"), "{stdout}");
}

/// Removal must not pass --force on jeet's own judgement: git independently
/// re-checks at removal time and catches anything written since.
#[test]
fn clean_defers_to_gits_own_check_at_removal_time() {
    let lab = lab("toctou");
    let output = lab.jeet(&["worktree", "racy", "--no-push"], &lab.repo);
    assert!(output.status.success());
    let wt = path_from(&output);

    // Appears clean when classified...
    let output = lab.jeet(&["worktree", "clean", "--dry-run"], &lab.repo);
    assert!(String::from_utf8_lossy(&output.stdout).contains("[remove]"));

    // ...but gains work before the removal actually runs.
    std::fs::write(wt.join("late.txt"), "written after the check").unwrap();
    let output = lab.jeet(&["worktree", "clean", "--yes"], &lab.repo);
    assert!(output.status.success());
    assert!(
        wt.join("late.txt").exists(),
        "work written after classification was destroyed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[keep]"), "{stdout}");

    // And the last line of defence: an explicit remove still defers to git.
    let output = lab.jeet(&["worktree", "remove", "acme/toctou", "racy"], &lab.repo);
    assert!(!output.status.success());
    assert!(wt.join("late.txt").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("uncommitted change"), "{stderr}");
}

/// --force decides whether work may be discarded. It must not also widen scope
/// to worktrees the user created outside ~/.jeet.
#[test]
fn force_does_not_widen_clean_scope_to_external_worktrees() {
    let lab = lab("scope");
    let outside = TempDir::new().unwrap();
    let external = outside.path().join("mine");
    git(
        &[
            "worktree",
            "add",
            "-q",
            &external.to_string_lossy(),
            "-b",
            "mine",
        ],
        &lab.repo,
    );
    std::fs::write(external.join("wip.txt"), "uncommitted").unwrap();

    let output = lab.jeet(&["worktree", "clean", "--force", "--yes"], &lab.repo);
    assert!(output.status.success());
    assert!(
        external.exists(),
        "--force deleted a worktree outside ~/.jeet without --all"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("jeet did not create this worktree"),
        "{stdout}"
    );
}

/// A worktree jeet cannot assess is not a worktree jeet may delete.
#[test]
fn clean_keeps_worktrees_it_cannot_assess() {
    let lab = lab("failclosed");
    let output = lab.jeet(&["worktree", "unassessable", "--no-push"], &lab.repo);
    assert!(output.status.success());

    // The recorded default branch stops resolving, so the comparison fails.
    git(&["branch", "-m", "main", "renamed-away"], &lab.repo);

    let output = lab.jeet(&["worktree", "clean", "--dry-run"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[keep]"), "{stdout}");
    assert!(stdout.contains("could not compare"), "{stdout}");
    assert!(!stdout.contains("nothing to lose"), "{stdout}");
}

/// The diff counter is documented as including uncommitted edits.
#[test]
fn the_diff_counter_includes_uncommitted_edits() {
    let lab = lab("counter");
    std::fs::write(lab.repo.join("tracked.txt"), "one\n").unwrap();
    git(&["add", "-A"], &lab.repo);
    git(&["commit", "-qm", "seed"], &lab.repo);

    let output = lab.jeet(&["worktree", "counting", "--no-push"], &lab.repo);
    let wt = path_from(&output);
    std::fs::write(wt.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();

    let output = lab.jeet(&["worktree", "ls", "acme/counter"], &lab.repo);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("+2/-0 in 1 file"), "{stdout}");
}

/// Renaming onto a branch that already exists on the remote would publish this
/// worktree over somebody else's work.
#[test]
fn rename_refuses_to_publish_over_an_existing_remote_branch() {
    let lab = lab("remotecollide");
    let output = lab.jeet(&["worktree", "taken"], &lab.repo);
    assert!(output.status.success());
    assert!(lab.branches_on_remote().contains("taken"));

    let output = lab.jeet(&["worktree", "scratch", "--no-push"], &lab.repo);
    assert!(output.status.success());

    let output = lab.jeet(&["worktree", "rename", "scratch", "taken"], &lab.repo);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists"), "{stderr}");
}

/// A branch that was never published tracks origin/<default>; telling the user
/// to delete that would be catastrophic advice.
#[test]
fn rename_only_warns_about_a_remote_branch_that_exists() {
    let lab = lab("nowarn");
    let output = lab.jeet(&["worktree", "unpublished", "--no-push"], &lab.repo);
    assert!(output.status.success());

    let output = lab.jeet(
        &["worktree", "rename", "unpublished", "renamed", "--no-push"],
        &lab.repo,
    );
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("still exists"),
        "warned about a branch that was never pushed: {stderr}"
    );
}

/// The shell wrapper is the only way the cd hand-off reaches a shell, so run
/// the emitted snippet for real rather than grepping it.
#[test]
fn the_emitted_shell_wrapper_actually_changes_directory() {
    let lab = lab("wrapper");
    let output = jeet_bin().args(["init-shell"]).output().unwrap();
    let snippet = String::from_utf8_lossy(&output.stdout).to_string();
    let script = std::path::Path::new(&lab.home).join("wrapper.sh");
    std::fs::write(&script, snippet).unwrap();

    let program = format!(
        r#"set -u
export PATH="{}:$PATH"
. "{}"
cd "{}"
jeet worktree wrapped --no-push >/dev/null 2>&1
pwd"#,
        std::path::Path::new(env!("CARGO_BIN_EXE_jeet"))
            .parent()
            .unwrap()
            .display(),
        script.display(),
        lab.repo.display(),
    );

    let mut cmd = Command::new("bash");
    hermetic_git(&mut cmd);
    let output = cmd
        .args(["--noprofile", "--norc", "-c", &program])
        .env("JEET_HOME", &lab.home)
        .output()
        .unwrap();
    let landed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        landed.ends_with("wrapped"),
        "wrapper left the shell in {landed:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A dry run must report the same verdict as the run it previews — otherwise
/// `--dry-run` says "remove" for a worktree that the real command then keeps.
#[test]
fn dry_run_previews_the_same_policy_it_would_apply() {
    let lab = lab("preview");
    std::fs::write(lab.repo.join(".gitignore"), ".env\n").unwrap();
    git(&["add", "-A"], &lab.repo);
    git(&["commit", "-qm", "ignore env"], &lab.repo);

    let output = lab.jeet(&["worktree", "previewed", "--no-push"], &lab.repo);
    let wt = path_from(&output);
    std::fs::write(wt.join(".env"), "SECRET=1").unwrap();

    let verdict = |args: &[&str]| -> String {
        let out = lab.jeet(args, &lab.repo);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| l.contains("previewed"))
            .unwrap_or("")
            .to_string();
        if line.contains("[remove]") {
            "remove".into()
        } else if line.contains("[keep]") {
            "keep".into()
        } else {
            format!("unrecognised: {line}")
        }
    };

    // Unattended: the preview and the run must agree...
    assert_eq!(
        verdict(&["worktree", "clean", "--dry-run", "--yes"]),
        "keep"
    );
    assert!(wt.join(".env").exists());
    assert_eq!(verdict(&["worktree", "clean", "--yes"]), "keep");
    assert!(
        wt.join(".env").exists(),
        "the run discarded what the preview kept"
    );

    // ...and an interactive preview reports the interactive policy.
    assert_eq!(verdict(&["worktree", "clean", "--dry-run"]), "remove");
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
