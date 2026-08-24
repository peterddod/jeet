use std::process::Command;

use tempfile::TempDir;

fn jeet_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jeet"));
    // jeet shells out to git, so the developer's global config would otherwise
    // reach in — commit.gpgsign alone fails most of this suite.
    hermetic_git(&mut cmd);
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

    let output = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &program])
        .env("JEET_HOME", &lab.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    let landed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        landed.ends_with("wrapped"),
        "wrapper left the shell in {landed:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
