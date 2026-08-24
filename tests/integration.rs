use std::process::Command;

use tempfile::TempDir;

fn jeet_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jeet"))
}

fn init_repo_with_remote(path: &std::path::Path, remote: &str) {
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["remote", "add", "origin", remote])
        .current_dir(path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .status()
        .unwrap();
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
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
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
    // Push and fetch both stay local; the https URL is only an identity.
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
    // If anything does try the https URL, fail immediately instead of dialing out.
    git(&["config", "http.proxy", "127.0.0.1:1"], repo.path());

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
