//! `jeet setup apply|status|discard`: the user's side of setup. Apply shows
//! each staged file as a diff and writes only what the user says yes to,
//! after a backup.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{
    applied_log, backups_root, check_config, current_hash, proposed_dir, read_manifest, same_path,
    target_path, write_atomic, write_manifest, Entry, PRODUCT,
};
use crate::assistant;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskState};
use crate::harness::shell_quote;
use crate::runtime;
use crate::tasks::TASK_ID_ENV;
use crate::tmux;
use crate::worktrees::now_secs;

/// What the user said to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Quit,
}

/// A file apply wrote, and where its old copy went (`None`: it was new).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub id: String,
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
}

/// Apply the staged proposals, one diff and one question per file. Only
/// the user, at a terminal: refused under any agent jeet started, and with
/// no terminal to answer from.
pub fn apply(app: &App) -> Result<()> {
    let task_env = std::env::var(TASK_ID_ENV).ok();
    if let Some(why) = refusal(task_env.as_deref(), std::io::stdin().is_terminal()) {
        bail!(why);
    }
    let color = std::io::stdout().is_terminal();
    let stamp = utc_stamp(now_secs());
    let stdin = std::io::stdin();
    let mut ask = || -> Result<Answer> {
        print!("Apply this change? [y/N/q] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            println!();
            return Ok(Answer::Quit);
        }
        Ok(parse_answer(&line))
    };
    let run = apply_in(&app.home, &stamp, color, &mut ask, &mut std::io::stdout())?;
    if run.applied.is_empty() {
        println!("Nothing applied.");
        return run.error.map_or(Ok(()), Err);
    }
    print!("{}", restore_advice(&app.home, &run.applied));
    let restart = to_restart(app, &run.applied)?;
    if !restart.is_empty() {
        println!(
            "\nThese running agents keep their old settings and instructions until they \
             restart:"
        );
        for slug in &restart {
            println!("  {PRODUCT} task restart {slug}");
        }
    }
    run.error.map_or(Ok(()), Err)
}

/// Why apply may not run here: under an agent jeet started (`task_env`,
/// `$JEET_TASK_ID`, set), or with no terminal on stdin to answer from.
fn refusal(task_env: Option<&str>, stdin_terminal: bool) -> Option<String> {
    if task_env.is_some_and(|v| !v.trim().is_empty()) {
        return Some(format!(
            "`{PRODUCT} setup apply` runs only at your own terminal, not from an agent's \
             session (${TASK_ID_ENV} is set)"
        ));
    }
    if !stdin_terminal {
        return Some(format!(
            "`{PRODUCT} setup apply` asks about each file, so it needs a terminal on stdin"
        ));
    }
    None
}

fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        "q" | "quit" => Answer::Quit,
        _ => Answer::No,
    }
}

/// What a run of [`apply_in`] did: the files it wrote, and the error that
/// stopped it, if one did, after them.
pub struct Run {
    pub applied: Vec<Applied>,
    pub error: Option<anyhow::Error>,
}

/// [`apply`] in `home`, asking with `ask` and printing to `out`. Each file
/// it refuses (not a target, changed since staged, an invalid config) is
/// said and skipped; each one the user takes is backed up to
/// `backups/setup-<stamp>/`, written, logged and unstaged. A file that
/// fails to write stops the run, keeping what was written before it.
pub fn apply_in(
    home: &Path,
    stamp: &str,
    color: bool,
    ask: &mut dyn FnMut() -> Result<Answer>,
    out: &mut dyn Write,
) -> Result<Run> {
    let mut manifest = read_manifest(home)?;
    let mut run = Run {
        applied: Vec::new(),
        error: None,
    };
    if manifest.is_empty() {
        writeln!(
            out,
            "Nothing is staged in {}.",
            proposed_dir(home).display()
        )?;
        return Ok(run);
    }
    let backup_dir = free_backup_dir(home, stamp);
    let ids: Vec<String> = manifest.keys().cloned().collect();
    for id in ids {
        let entry = manifest[&id].clone();
        let (target, bytes) = match ready(home, &id, &entry) {
            Ready::Go { target, bytes } => (target, bytes),
            Ready::Same => {
                writeln!(out, "\n{id}: already as staged; unstaged.")?;
                unstage(home, &mut manifest, &id, out)?;
                continue;
            }
            Ready::Refused(why) => {
                writeln!(out, "\n{id}: refused: {why}")?;
                continue;
            }
        };
        writeln!(out, "\n── {id} → {}", target.display())?;
        if !entry.reason.is_empty() {
            writeln!(out, "   {}", entry.reason)?;
        }
        let old = target.exists().then_some(target.as_path());
        if old.is_none() {
            writeln!(out, "   (new file)")?;
        }
        // What is shown is what was read, and what would be written.
        out.write_all(&diff(old, &bytes, color)?)?;
        match ask()? {
            Answer::Yes => {}
            Answer::No => continue,
            Answer::Quit => break,
        }
        // The prompt may have waited a while.
        if current_hash(&target).ok() != Some(entry.hash.clone()) {
            writeln!(
                out,
                "{id}: refused: the file changed while you looked; ask the setup agent to \
                 stage it again"
            )?;
            continue;
        }
        match write_one(home, &backup_dir, stamp, &id, &entry, &target, &bytes) {
            Ok(applied) => {
                writeln!(out, "{id}: applied.")?;
                run.applied.push(applied);
            }
            Err(e) => {
                run.error = Some(e.context(format!("apply {id}")));
                break;
            }
        }
        unstage(home, &mut manifest, &id, out)?;
    }
    Ok(run)
}

/// `backups/setup-<stamp>`, or with `-2`, `-3`, … when a run in the same
/// second took it.
fn free_backup_dir(home: &Path, stamp: &str) -> PathBuf {
    let root = backups_root(home);
    let mut dir = root.join(format!("setup-{stamp}"));
    let mut n = 2;
    while std::fs::symlink_metadata(&dir).is_ok() {
        dir = root.join(format!("setup-{stamp}-{n}"));
        n += 1;
    }
    dir
}

/// Back `target` up (when it exists), write `bytes` there and log it.
fn write_one(
    home: &Path,
    backup_dir: &Path,
    stamp: &str,
    id: &str,
    entry: &Entry,
    target: &Path,
    bytes: &[u8],
) -> Result<Applied> {
    let backup = match target.exists() {
        true => {
            let to = backup_dir.join(id);
            let dir = to.parent().unwrap_or(backup_dir);
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            std::fs::copy(target, &to)
                .with_context(|| format!("back {} up to {}", target.display(), to.display()))?;
            Some(to)
        }
        false => None,
    };
    write_atomic(target, bytes)?;
    log_applied(home, stamp, id, backup.as_deref(), &entry.reason)?;
    Ok(Applied {
        id: id.to_string(),
        path: target.to_path_buf(),
        backup,
    })
}

/// Drop `id`'s staged file and its manifest entry. What fails is said, not
/// raised: the target is written by then, and a re-run finds it as staged.
fn unstage(
    home: &Path,
    manifest: &mut super::Manifest,
    id: &str,
    out: &mut dyn Write,
) -> Result<()> {
    let staged = proposed_dir(home).join(id);
    if let Err(e) = std::fs::remove_file(&staged) {
        writeln!(out, "{id}: could not remove {}: {e}", staged.display())?;
    }
    manifest.remove(id);
    if let Err(e) = write_manifest(home, manifest) {
        writeln!(out, "{id}: could not update the manifest: {e:#}")?;
    }
    Ok(())
}

/// Whether a staged entry can be shown and applied.
enum Ready {
    Go {
        target: PathBuf,
        bytes: Vec<u8>,
    },
    /// The target already holds what is staged.
    Same,
    Refused(String),
}

fn ready(home: &Path, id: &str, entry: &Entry) -> Ready {
    let target = match target_path(home, id) {
        Ok(target) => target,
        Err(e) => return Ready::Refused(format!("{e:#}")),
    };
    if let Some(written) = &entry.path {
        if !same_path(written, &target) {
            return Ready::Refused(format!(
                "the manifest says {written}, but {id} is {}",
                target.display()
            ));
        }
    }
    if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ready::Refused(format!(
            "{} is a symlink; change the file it points to yourself",
            target.display()
        ));
    }
    let bytes = match read_staged(home, id) {
        Ok(bytes) => bytes,
        Err(e) => return Ready::Refused(format!("{e:#}")),
    };
    let now = match current_hash(&target) {
        Ok(now) => now,
        Err(e) => return Ready::Refused(format!("{e:#}")),
    };
    if now.as_deref() == Some(super::hash(&bytes).as_str()) {
        return Ready::Same;
    }
    if now != entry.hash {
        let why = match (&entry.hash, &now) {
            (None, Some(_)) => "it was staged as a new file, but the file exists now",
            (Some(_), None) => "the file was removed since it was staged",
            _ => "the file changed since it was staged",
        };
        return Ready::Refused(format!("{why}; ask the setup agent to stage it again"));
    }
    if id == "config.toml" {
        let checked = std::str::from_utf8(&bytes)
            .context("not UTF-8")
            .and_then(check_config);
        if let Err(e) = checked {
            return Ready::Refused(format!("the config would not load: {e:#}"));
        }
    }
    Ready::Go { target, bytes }
}

/// What is staged for `id`: a plain file inside `proposed/`, reached
/// through no symlink.
fn read_staged(home: &Path, id: &str) -> Result<Vec<u8>> {
    let staged = proposed_dir(home).join(id);
    let meta = std::fs::symlink_metadata(&staged)
        .with_context(|| format!("nothing is staged at {}", staged.display()))?;
    if !meta.is_file() {
        bail!("{} is not a plain file", staged.display());
    }
    let root = proposed_dir(home)
        .canonicalize()
        .with_context(|| format!("resolve {}", proposed_dir(home).display()))?;
    let real = staged
        .canonicalize()
        .with_context(|| format!("resolve {}", staged.display()))?;
    if real != root.join(id) {
        bail!(
            "{} is reached through a symlink; stage a plain file",
            staged.display()
        );
    }
    std::fs::read(&real).with_context(|| format!("read {}", staged.display()))
}

/// A unified diff of `old` (none: a new file) against `new`, by git, which
/// jeet needs anyway.
fn diff(old: Option<&Path>, new: &[u8], color: bool) -> Result<Vec<u8>> {
    let old = old.unwrap_or(Path::new("/dev/null"));
    let mut child = Command::new("git")
        .args(["--no-pager", "diff", "--no-index", "--no-ext-diff"])
        .arg(if color {
            "--color=always"
        } else {
            "--color=never"
        })
        .arg("--")
        .arg(old)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git diff")?;
    // Written from a thread so a large diff cannot fill stdout first.
    let mut stdin = child.stdin.take().context("git diff's stdin")?;
    let new = new.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&new));
    let output = child.wait_with_output().context("run git diff")?;
    let _ = writer.join();
    // 1 says they differ, which is the point.
    match output.status.code() {
        Some(0 | 1) => Ok(output.stdout),
        _ => bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn log_applied(
    home: &Path,
    stamp: &str,
    id: &str,
    backup: Option<&Path>,
    reason: &str,
) -> Result<()> {
    let log = applied_log(home);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let backup = backup.map_or("new".to_string(), |b| b.display().to_string());
    let reason = reason.replace(['\n', '\t'], " ");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    writeln!(file, "{stamp}\t{id}\t{backup}\t{reason}")
        .with_context(|| format!("write {}", log.display()))
}

/// Where the old files went and the commands that put them back.
fn restore_advice(home: &Path, applied: &[Applied]) -> String {
    let mut text = String::new();
    let backup = applied.iter().find_map(|a| {
        let backup = a.backup.as_ref()?;
        let depth = a.id.split('/').count();
        backup.ancestors().nth(depth).map(Path::to_path_buf)
    });
    if let Some(dir) = backup {
        text.push_str(&format!(
            "\nBackups: {}\nTo restore them: cp -R {} {}\n",
            dir.display(),
            shell_quote(&format!("{}/.", dir.display())),
            shell_quote(&format!("{}/", home.display())),
        ));
    }
    let new: Vec<&Applied> = applied.iter().filter(|a| a.backup.is_none()).collect();
    if !new.is_empty() {
        text.push_str("New files, which a restore removes with:\n");
        for a in new {
            text.push_str(&format!(
                "  rm {}\n",
                shell_quote(&a.path.display().to_string())
            ));
        }
    }
    text
}

/// The running agents that would read what was applied once restarted:
/// every one for the config, whose rules are in each launch's settings;
/// the assistant for its own files. None is restarted here.
fn to_restart(app: &App, applied: &[Applied]) -> Result<Vec<String>> {
    if !tmux::available() {
        return Ok(Vec::new());
    }
    let config = applied.iter().any(|a| a.id == "config.toml");
    let assistant_files = applied.iter().any(|a| a.id.starts_with("assistant/"));
    let mut slugs = Vec::new();
    if (config || assistant_files) && !assistant::running_sessions().is_empty() {
        slugs.push(assistant::SLUG.to_string());
    }
    if config {
        for task in app.db.list_tasks(Some(TaskState::Active))? {
            if task.kind != TaskKind::Assistant && tmux::agent_alive(&runtime::session_name(&task))
            {
                slugs.push(task.slug);
            }
        }
    }
    Ok(slugs)
}

/// List what is staged, each with whether apply would take it and its
/// target's hash now (what the manifest records), and the last applied run.
pub fn status(app: &App) -> Result<()> {
    print!("{}", status_text(&app.home)?);
    Ok(())
}

fn status_text(home: &Path) -> Result<String> {
    let mut text = String::new();
    let manifest = read_manifest(home)?;
    if manifest.is_empty() {
        text.push_str(&format!(
            "Nothing is staged in {}.\n",
            proposed_dir(home).display()
        ));
    } else {
        text.push_str(&format!("Staged in {}:\n", proposed_dir(home).display()));
    }
    for (id, entry) in &manifest {
        let state = match ready(home, id, entry) {
            Ready::Go { .. } => "ready".to_string(),
            Ready::Same => "already applied".to_string(),
            Ready::Refused(why) => format!("refused: {why}"),
        };
        text.push_str(&format!("  {id}  [{state}]\n"));
        if !entry.reason.is_empty() {
            text.push_str(&format!("    {}\n", entry.reason));
        }
        if let Ok(target) = target_path(home, id) {
            let now = match current_hash(&target) {
                Ok(Some(hash)) => hash,
                Ok(None) => "none (no such file)".into(),
                Err(e) => format!("unreadable ({e:#})"),
            };
            text.push_str(&format!("    {} hash now: {now}\n", target.display()));
        }
    }
    let log = std::fs::read_to_string(applied_log(home)).unwrap_or_default();
    let last_stamp = log.lines().last().and_then(|l| l.split('\t').next());
    if let Some(stamp) = last_stamp {
        text.push_str(&format!("Last applied ({stamp}):\n"));
        for line in log.lines().filter(|l| l.split('\t').next() == Some(stamp)) {
            let mut fields = line.split('\t').skip(1);
            let id = fields.next().unwrap_or_default();
            let backup = fields.next().unwrap_or_default();
            text.push_str(&format!("  {id}  (backup: {backup})\n"));
        }
    }
    Ok(text)
}

/// Drop every staged proposal.
pub fn discard(app: &App) -> Result<()> {
    let dir = proposed_dir(&app.home);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => println!("Discarded the proposals in {}.", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("Nothing is staged."),
        Err(e) => return Err(e).with_context(|| format!("remove {}", dir.display())),
    }
    Ok(())
}

/// `secs` since the epoch as a UTC stamp, `20261017T093000Z`.
pub fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::super::{hash, Manifest, MANIFEST};
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Stage `text` for `id`, with the target's hash as it is now.
    fn stage(home: &Path, id: &str, text: &str) {
        write(&proposed_dir(home).join(id), text);
        let mut manifest = read_manifest(home).unwrap();
        let hash = current_hash(&home.join(id)).unwrap();
        manifest.insert(
            id.into(),
            Entry {
                path: None,
                reason: format!("why {id}"),
                hash,
            },
        );
        write_manifest(home, &manifest).unwrap();
    }

    fn run(home: &Path, answers: &[Answer]) -> (Vec<Applied>, String) {
        let mut answers = answers.iter().copied();
        let mut ask = || Ok(answers.next().unwrap_or(Answer::Quit));
        let mut out = Vec::new();
        let run = apply_in(home, "20261017T000000Z", false, &mut ask, &mut out).unwrap();
        assert!(run.error.is_none(), "{:?}", run.error);
        (run.applied, String::from_utf8(out).unwrap())
    }

    #[test]
    fn stamps_are_utc() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(1_792_200_645), "20261017T013045Z");
    }

    #[test]
    fn answers_default_to_no() {
        assert_eq!(parse_answer("y\n"), Answer::Yes);
        assert_eq!(parse_answer(" YES "), Answer::Yes);
        assert_eq!(parse_answer("q"), Answer::Quit);
        assert_eq!(parse_answer("\n"), Answer::No);
        assert_eq!(parse_answer("sure"), Answer::No);
    }

    #[test]
    fn a_yes_backs_up_writes_unstages_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join("assistant/AGENTS.md"), "old\n");
        stage(home, "assistant/AGENTS.md", "new\n");
        stage(home, "templates/worker/SKILL.md", "skill\n");

        let (applied, out) = run(home, &[Answer::Yes, Answer::Yes]);
        assert_eq!(applied.len(), 2, "{out}");
        assert!(out.contains("-old"), "{out}");
        assert!(out.contains("+new"), "{out}");
        assert!(out.contains("(new file)"), "{out}");

        assert_eq!(read(&home.join("assistant/AGENTS.md")), "new\n");
        assert_eq!(read(&home.join("templates/worker/SKILL.md")), "skill\n");
        let backup = home.join("backups/setup-20261017T000000Z/assistant/AGENTS.md");
        assert_eq!(read(&backup), "old\n");
        assert_eq!(applied[0].backup.as_deref(), Some(backup.as_path()));
        assert_eq!(applied[1].backup, None);

        assert!(read_manifest(home).unwrap().is_empty());
        assert!(!proposed_dir(home).join("assistant/AGENTS.md").exists());
        let log = read(&applied_log(home));
        assert_eq!(log.lines().count(), 2, "{log}");
        assert!(log.contains("\tassistant/AGENTS.md\t"), "{log}");
        assert!(log.contains("\ttemplates/worker/SKILL.md\tnew\t"), "{log}");

        let advice = restore_advice(home, &applied);
        assert!(
            advice.contains("cp -R '") && advice.contains("setup-20261017T000000Z/.'"),
            "{advice}"
        );
        assert!(advice.contains("rm "), "{advice}");
    }

    #[test]
    fn a_no_or_a_quit_leaves_everything_staged() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        stage(home, "templates/root/AGENTS.md", "a\n");
        stage(home, "templates/worker/SKILL.md", "b\n");
        let (applied, _) = run(home, &[Answer::No, Answer::Quit]);
        assert!(applied.is_empty());
        assert_eq!(read_manifest(home).unwrap().len(), 2);
        assert!(!home.join("templates").exists());
        assert!(!applied_log(home).exists());
    }

    #[test]
    fn a_stale_target_is_refused_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join("assistant/AGENTS.md"), "old\n");
        stage(home, "assistant/AGENTS.md", "new\n");
        write(&home.join("assistant/AGENTS.md"), "edited meanwhile\n");
        // A new file that appeared after staging, too.
        stage(home, "templates/root/AGENTS.md", "t\n");
        write(&home.join("templates/root/AGENTS.md"), "mine\n");

        let mut asked = 0;
        let mut ask = || {
            asked += 1;
            Ok(Answer::Yes)
        };
        let mut out = Vec::new();
        let applied = apply_in(home, "s", false, &mut ask, &mut out)
            .unwrap()
            .applied;
        let out = String::from_utf8(out).unwrap();
        assert!(applied.is_empty(), "{out}");
        assert_eq!(asked, 0);
        assert!(out.contains("changed since it was staged"), "{out}");
        assert!(out.contains("staged as a new file"), "{out}");
        assert_eq!(
            read(&home.join("assistant/AGENTS.md")),
            "edited meanwhile\n"
        );
    }

    #[test]
    fn an_invalid_config_is_refused_and_a_valid_one_applied() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join("config.toml"), "scan_roots = []\n");
        stage(
            home,
            "config.toml",
            "scan_roots = []\n[assistant]\nallow = [\"nope\"]\n",
        );
        let (applied, out) = run(home, &[Answer::Yes]);
        assert!(applied.is_empty(), "{out}");
        assert!(out.contains("refused: the config would not load"), "{out}");
        assert_eq!(read(&home.join("config.toml")), "scan_roots = []\n");

        stage(home, "config.toml", "scan_roots = [\"~/src\"]\n");
        let (applied, out) = run(home, &[Answer::Yes]);
        assert_eq!(applied.len(), 1, "{out}");
        assert_eq!(
            read(&home.join("config.toml")),
            "scan_roots = [\"~/src\"]\n"
        );
    }

    #[test]
    fn targets_off_the_allowlist_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&proposed_dir(home).join("tasks/x/AGENTS.md"), "evil\n");
        let mut manifest = Manifest::new();
        manifest.insert(
            "tasks/x/AGENTS.md".into(),
            Entry {
                path: None,
                reason: String::new(),
                hash: None,
            },
        );
        // A target id that is allowed, but a path that points elsewhere.
        write(&proposed_dir(home).join("assistant/AGENTS.md"), "x\n");
        manifest.insert(
            "assistant/AGENTS.md".into(),
            Entry {
                path: Some("/etc/AGENTS.md".into()),
                reason: String::new(),
                hash: None,
            },
        );
        write_manifest(home, &manifest).unwrap();
        let (applied, out) = run(home, &[Answer::Yes, Answer::Yes]);
        assert!(applied.is_empty(), "{out}");
        assert!(out.contains("tasks/x/AGENTS.md: refused"), "{out}");
        assert!(out.contains("the manifest says /etc/AGENTS.md"), "{out}");
        assert!(!home.join("tasks").exists());
        assert!(!home.join("assistant").exists());
    }

    #[test]
    fn status_lists_each_proposal_with_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(status_text(home).unwrap().contains("Nothing is staged"));
        write(&home.join("assistant/AGENTS.md"), "old\n");
        stage(home, "assistant/AGENTS.md", "new\n");
        let text = status_text(home).unwrap();
        assert!(text.contains("assistant/AGENTS.md  [ready]"), "{text}");
        assert!(text.contains(&hash(b"old\n")), "{text}");
        run(home, &[Answer::Yes]);
        let text = status_text(home).unwrap();
        assert!(text.contains("Nothing is staged"), "{text}");
        assert!(text.contains("Last applied (20261017T000000Z)"), "{text}");
        assert!(!proposed_dir(home).join(MANIFEST).exists());
    }

    #[test]
    fn refuses_agents_and_no_terminal() {
        assert!(refusal(Some("abc"), true).unwrap().contains("JEET_TASK_ID"));
        assert!(refusal(None, false).unwrap().contains("terminal"));
        assert_eq!(refusal(None, true), None);
        assert_eq!(refusal(Some(" "), true), None);
    }

    #[test]
    fn a_proposal_already_in_place_is_unstaged() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join("assistant/AGENTS.md"), "same\n");
        stage(home, "assistant/AGENTS.md", "same\n");
        let mut ask = || -> Result<Answer> { panic!("asked about an unchanged file") };
        let mut out = Vec::new();
        let run = apply_in(home, "s", false, &mut ask, &mut out).unwrap();
        assert!(run.applied.is_empty());
        assert!(read_manifest(home).unwrap().is_empty());
        assert!(!proposed_dir(home).join("assistant/AGENTS.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_staged_file_through_a_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let outside = dir.path().join("secret");
        write(&outside.join("AGENTS.md"), "secret\n");
        std::fs::create_dir_all(proposed_dir(home)).unwrap();
        std::os::unix::fs::symlink(&outside, proposed_dir(home).join("assistant")).unwrap();
        let mut manifest = Manifest::new();
        manifest.insert(
            "assistant/AGENTS.md".into(),
            Entry {
                path: None,
                reason: String::new(),
                hash: None,
            },
        );
        write_manifest(home, &manifest).unwrap();
        let (applied, out) = run(home, &[Answer::Yes]);
        assert!(applied.is_empty(), "{out}");
        assert!(out.contains("through a symlink"), "{out}");
        assert!(!out.contains("+secret"), "{out}");
        assert_eq!(read(&outside.join("AGENTS.md")), "secret\n");
    }

    #[test]
    fn a_second_run_in_the_same_second_keeps_the_first_backup() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join("assistant/AGENTS.md"), "one\n");
        stage(home, "assistant/AGENTS.md", "two\n");
        run(home, &[Answer::Yes]);
        stage(home, "assistant/AGENTS.md", "three\n");
        let (applied, _) = run(home, &[Answer::Yes]);
        let first = home.join("backups/setup-20261017T000000Z/assistant/AGENTS.md");
        let second = home.join("backups/setup-20261017T000000Z-2/assistant/AGENTS.md");
        assert_eq!(read(&first), "one\n");
        assert_eq!(read(&second), "two\n");
        assert_eq!(applied[0].backup.as_deref(), Some(second.as_path()));
        assert!(restore_advice(home, &applied).contains("setup-20261017T000000Z-2/."));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_keeps_the_old_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let config = home.join("config.toml");
        write(&config, "scan_roots = []\n");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        stage(home, "config.toml", "scan_roots = [\"~/a\"]\n");
        run(home, &[Answer::Yes]);
        let mode = std::fs::metadata(&config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
