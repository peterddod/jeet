//! Background review jobs: the command `jeet review` starts in a PR's worktree.
//!
//! A job is a JSON state file plus a log under `$JEET_HOME/reviews`, keyed by
//! repository and PR number. `jeet review` writes the state file and launches
//! the hidden `jeet review-job` runner detached from the terminal; the runner
//! runs the configured command with its output going to the log, records how
//! it ended, and raises a desktop notification. Anything that wants to know
//! how a review is going — the explorer, a second `jeet review` — reads the
//! state file back.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ReviewConfig;
use crate::context::App;
use crate::db::anchors::{Anchor, AnchorKind};
use crate::db::tasks::TaskKind;
use crate::db::{Database, RepoRecord};
use crate::github::PullRequest;
use crate::identity;
use crate::remote;
use crate::tmux;
use crate::worktrees::now_secs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub pr: u64,
    pub title: String,
    /// The PR head commit the review was started against, so a second
    /// `jeet review` can tell a finished review from a stale one.
    pub head: String,
    pub worktree: PathBuf,
    /// The command as run, placeholders already substituted.
    pub command: String,
    pub env: Vec<(String, String)>,
    pub notify: bool,
    pub log: PathBuf,
    pub started_at: i64,
    #[serde(default)]
    pub pid: Option<u32>,
    /// When the runner started, as [`identity`] reads it: with `pid`, the
    /// one process that runs the job, which a reused pid is not.
    #[serde(default)]
    pub runner_start: Option<i64>,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Unfinished, and its process is gone. Worked out once when the job is
    /// loaded — the explorer asks for the state every frame, and a process
    /// spawned per frame to check is not a price worth paying.
    #[serde(skip)]
    lost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Finished(i32),
    /// Never recorded an ending and its process is gone: killed, or the
    /// machine went down under it.
    Lost,
}

impl Job {
    pub fn state(&self) -> JobState {
        match (self.exit_code, self.lost) {
            (Some(code), _) => JobState::Finished(code),
            (None, true) => JobState::Lost,
            (None, false) => JobState::Running,
        }
    }

    /// One line on where the job stands, for the explorer and the CLI.
    pub fn describe(&self) -> String {
        let ago = |at: i64| crate::agent::age_label(at);
        match self.state() {
            JobState::Running => format!("running (started {})", ago(self.started_at)),
            JobState::Finished(0) => {
                format!(
                    "finished {}",
                    ago(self.finished_at.unwrap_or(self.started_at))
                )
            }
            JobState::Finished(code) => format!(
                "failed with status {code} {}",
                ago(self.finished_at.unwrap_or(self.started_at))
            ),
            JobState::Lost => "stopped without finishing".to_string(),
        }
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    true
}

fn jobs_dir(app: &App, repo: &RepoRecord) -> Result<PathBuf> {
    let id = remote::identity_from_id(&repo.id)?;
    Ok(app
        .home
        .join("reviews")
        .join(id.host)
        .join(id.owner)
        .join(id.repo))
}

fn state_file(app: &App, repo: &RepoRecord, pr: u64) -> Result<PathBuf> {
    Ok(jobs_dir(app, repo)?.join(format!("{pr}.json")))
}

/// The job recorded for `pr`, if one was ever started.
pub fn load(app: &App, repo: &RepoRecord, pr: u64) -> Option<Job> {
    let mut job = read(&state_file(app, repo, pr).ok()?).ok()?;
    job.lost = job.exit_code.is_none() && job.pid.is_some_and(|pid| !process_alive(pid));
    Some(job)
}

/// The worktree of every review job ever recorded, under any repository.
pub fn recorded_worktrees(app: &App) -> Vec<PathBuf> {
    recorded_jobs(app).map(|job| job.worktree).collect()
}

/// Every review job ever recorded, under any repository.
fn recorded_jobs(app: &App) -> impl Iterator<Item = Job> {
    walkdir::WalkDir::new(app.home.join("reviews"))
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read(e.path()).ok())
}

fn read(path: &Path) -> Result<Job> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read review state {}", path.display()))?;
    serde_json::from_str(&text).context("parse review state")
}

fn write(path: &Path, job: &Job) -> Result<()> {
    let text = serde_json::to_string_pretty(job)?;
    // Through a temporary file so a reader never sees half a job.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// Why a new job should not start, given the one already recorded — or
/// nothing, when it should.
pub fn reason_to_skip(existing: Option<&Job>, head: &str) -> Option<String> {
    let job = existing?;
    match job.state() {
        JobState::Running => Some(format!("a review of #{} is already running", job.pr)),
        JobState::Finished(0) if job.head == head => Some(format!(
            "#{} was already reviewed at this commit ({}); pass --rerun to review it again",
            job.pr,
            job.describe()
        )),
        _ => None,
    }
}

/// The configured command with `{pr}` and `{url}` filled in.
///
/// Only those two: both are GitHub's own, digits and a URL built from the
/// repository's name. A branch or title comes from whoever opened the PR, and
/// pasted into a shell command it runs as code — so those go in the
/// environment, where the command can quote them itself.
pub fn expand(template: &str, pr: &PullRequest) -> String {
    template
        .replace("{pr}", &pr.number.to_string())
        .replace("{url}", &pr.url)
}

fn job_env(pr: &PullRequest, log: &Path) -> Vec<(String, String)> {
    vec![
        ("JEET_PR".into(), pr.number.to_string()),
        ("JEET_PR_URL".into(), pr.url.clone()),
        ("JEET_PR_TITLE".into(), pr.title.clone()),
        ("JEET_PR_BRANCH".into(), pr.head_ref_name.clone()),
        ("JEET_PR_BASE".into(), pr.base_ref_name.clone()),
        ("JEET_REVIEW_LOG".into(), log.to_string_lossy().to_string()),
    ]
}

/// Start the review command for `pr` in `worktree`, detached from this
/// terminal so it outlives both jeet and the shell it was started from.
///
/// `as_task` is the review task when it has a parent: the command then runs
/// as that task's agent, a Node that reports to it, not as whoever started
/// the review. `runner_for` is the task the runner is recorded as: that
/// one, else the assistant that started it, else the top-level review task
/// itself, which runs as the user.
#[allow(clippy::too_many_arguments)]
pub fn start(
    app: &App,
    repo: &RepoRecord,
    pr: &PullRequest,
    worktree: &Path,
    config: &ReviewConfig,
    template: &str,
    as_task: Option<&str>,
    runner_for: &str,
) -> Result<Job> {
    let dir = jobs_dir(app, repo)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.json", pr.number));
    let log = dir.join(format!("{}.log", pr.number));

    let mut env = job_env(pr, &log);
    if let Some(id) = as_task {
        env.push((crate::tasks::TASK_ID_ENV.into(), id.into()));
    }
    let mut job = Job {
        pr: pr.number,
        title: pr.title.clone(),
        head: pr.head_ref_oid.clone(),
        worktree: worktree.to_path_buf(),
        command: expand(template, pr),
        env,
        notify: config.notify(),
        log: log.clone(),
        started_at: now_secs(),
        pid: None,
        runner_start: None,
        finished_at: None,
        exit_code: None,
        lost: false,
    };
    write(&path, &job)?;

    // Once the runner has the go-ahead it is the only writer of the state
    // file: two writers is how a quick-failing command's exit status got
    // papered over with a stale copy. So launching records only the runner,
    // before the go-ahead — and that launching failed, which would otherwise
    // leave a job that reads as running forever, refusing every later
    // `jeet review`, `--rerun` included.
    if let Err(e) = launch(app, &path, &log, worktree, runner_for) {
        job.finished_at = Some(now_secs());
        job.exit_code = Some(127);
        let _ = write(&path, &job);
        return Err(e);
    }
    Ok(job)
}

/// Launch the runner for the job in `state`, recorded as task
/// `runner_for`'s, so what its command runs is known for the task's, and
/// archiving or deleting the task finds it to end.
fn launch(app: &App, state: &Path, log: &Path, worktree: &Path, runner_for: &str) -> Result<()> {
    let out = File::create(log).with_context(|| format!("create {}", log.display()))?;
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    // `nohup` so closing the terminal does not take the review with it — the
    // ignored SIGHUP survives into everything the command runs — and a process
    // group of its own so a ctrl-c meant for the explorer does not either.
    let mut cmd = Command::new("nohup");
    cmd.arg(exe)
        .arg("review-job")
        .arg(state)
        .current_dir(worktree)
        // For the go-ahead: see [`await_go`].
        .stdin(Stdio::piped())
        .stdout(out.try_clone()?)
        .stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Recorded as a top-level task, the runner is the user's: a hint it
    // inherited (the shell of another task's agent, say) would be taken for
    // a claim to be that task, and what its command runs refused.
    let task = app.db.get_task(runner_for)?;
    let users = task.is_some_and(|t| t.parent_id.is_none() && t.kind != TaskKind::Assistant);
    if users {
        cmd.env_remove(crate::tasks::TASK_ID_ENV)
            .env_remove(crate::permissions::ASSISTANT_ENV);
    }
    let mut child = cmd.spawn().context("start the review command")?;
    hand_over(app, state, runner_for, users, &mut child)
}

/// Record `child`, the runner [`launch`] just started for the job in
/// `state`, as task `runner_for`'s, name it in the job's file, then give it
/// the go-ahead. When it cannot be recorded it is killed, unless it is the
/// user's (`users`): the user needs no anchor to be let through, so its
/// job runs on, with a warning that archive and delete cannot find it.
fn hand_over(
    app: &App,
    state: &Path,
    runner_for: &str,
    users: bool,
    child: &mut std::process::Child,
) -> Result<()> {
    // `nohup` execs the runner, so this is its pid. Its pid and start go in
    // the job's file before it has the go-ahead, so a job is matched to its
    // runner from the first: archiving the task in the moments before the
    // runner gets going ends it all the same.
    let pid = child.id();
    let recorded = match identity::record(&app.db, runner_for, AnchorKind::Job, None, pid)
        .context("record the review command as its task's")
    {
        Err(e) if users => {
            eprintln!(
                "jeet: warning: {e:#}; archiving or deleting the task will not end this review"
            );
            Ok(())
        }
        recorded => recorded,
    };
    let recorded = recorded.and_then(|()| {
        let mut job = read(state)?;
        job.pid = Some(pid);
        job.runner_start = identity::start_of(pid);
        write(state, &job)
    });
    if let Err(e) = recorded {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(GO).context("start the review command")?;
    }
    Ok(())
}

/// The runners jeet recorded for task `task_id`'s review jobs.
fn task_jobs(db: &Database, task_id: &str) -> Result<Vec<Anchor>> {
    db.task_anchors(task_id, AnchorKind::Job)
}

/// The runners of the review jobs still running in a worktree under `dir`,
/// whoever they run as: one the assistant started for a top-level review
/// task is recorded as the assistant's, not the task's, so [`task_jobs`]
/// does not find it. Only the runner the job recorded as its own, by pid
/// and start (a pid reused by another job's runner is not it), while it is
/// still that process.
fn jobs_in(app: &App, dir: &Path) -> Result<Vec<Anchor>> {
    let jobs: Vec<Job> = recorded_jobs(app)
        .filter(|job| job.exit_code.is_none() && job.worktree.starts_with(dir))
        .collect();
    let pids: Vec<u32> = jobs.iter().filter_map(|job| job.pid).collect();
    let mut runners = app.db.anchors_for(&pids)?;
    runners.retain(|anchor| {
        anchor.kind == AnchorKind::Job
            && jobs
                .iter()
                .any(|job| job.pid == Some(anchor.pid) && job.runner_start == Some(anchor.start))
            && identity::live(anchor)
    });
    Ok(runners)
}

/// Fail when this jeet runs under one of the review jobs [`stop_jobs`] or
/// [`stop_jobs_in`] would end for task `task_id`, in `dir`: ending the job
/// to `verb` the task would end this jeet with it, part-way through.
pub fn refuse_inside_job(app: &App, task_id: &str, dir: &Path, verb: &str) -> Result<()> {
    let mut runners = task_jobs(&app.db, task_id)?;
    runners.extend(jobs_in(app, dir)?);
    if runners.iter().any(identity::runs_under) {
        bail!(
            "this jeet runs under a review job in the task, which {verb} would end, and this \
             jeet with it; {verb} the task once the review is done, or from outside it"
        );
    }
    Ok(())
}

/// End the review jobs running for task `task_id`: each runner jeet
/// recorded for it, while it is still that process, with its process group
/// ([`launch`] gives it one), so the command it runs goes too. Returns
/// once they have exited, with how many were still running.
pub fn stop_jobs(db: &Database, task_id: &str) -> Result<usize> {
    let mut ended = 0;
    for anchor in task_jobs(db, task_id)? {
        ended += usize::from(end_group(&anchor)?);
    }
    Ok(ended)
}

/// End the review jobs still running in a worktree under `dir` (see
/// [`jobs_in`]) as [`stop_jobs`] does; their records go with them.
pub fn stop_jobs_in(app: &App, dir: &Path) -> Result<usize> {
    let mut ended = 0;
    for anchor in jobs_in(app, dir)? {
        ended += usize::from(end_group(&anchor)?);
        app.db.delete_anchor(anchor.pid, anchor.start)?;
    }
    Ok(ended)
}

/// End `anchor`'s process group, while it is still that process, and
/// return once the runner and what it started have exited: what they write
/// after this would miss an archive's snapshot. Whatever has not exited
/// within [`tmux::EXIT_GRACE`] is killed outright. Whether it was running.
fn end_group(anchor: &Anchor) -> Result<bool> {
    if anchor.pid <= 1 || !identity::live(anchor) {
        return Ok(false);
    }
    // Taken before the signal: once the runner is gone, its children are
    // no longer known as its.
    let tree = tmux::process_tree(&[anchor.pid]);
    let _ = Command::new("kill")
        .args(["-s", "TERM", "--", &format!("-{}", anchor.pid)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    tmux::end_processes(&tree, tmux::EXIT_GRACE).context("end the review command")?;
    Ok(true)
}

/// What [`launch`] writes to the runner once it is recorded.
const GO: &[u8] = b"g";

/// Wait for [`launch`]'s go-ahead, before the runner asks who it is: until
/// it has one, it is not yet recorded as its task's. Anything else on
/// stdin, a terminal or nothing at all, means it was not started by
/// `jeet review`.
pub fn await_go() -> Result<()> {
    let mut stdin = std::io::stdin();
    let refused = "review-job is started only by `jeet review`";
    if stdin.is_terminal() {
        bail!(refused);
    }
    let mut byte = [0u8; 1];
    match stdin.read(&mut byte) {
        Ok(1) if byte == GO => Ok(()),
        _ => bail!(refused),
    }
}

/// The runner for the job in `state`, refused before it ran the command,
/// or never given the go-ahead (its `jeet review` died first): record that
/// it ended, or the job reads as running forever, refusing
/// every later `jeet review`. Only a state file jeet wrote, for a job not
/// yet ended and no other runner's.
pub fn refused(state: &Path) {
    let Ok(app) = App::open() else {
        return;
    };
    if crate::permissions::check_review_job(&app.home, state).is_err() {
        return;
    }
    let Ok(mut job) = read(state) else {
        return;
    };
    // `jeet review` names the runner in the file before the go-ahead.
    if job.exit_code.is_some() || job.pid.is_some_and(|pid| pid != std::process::id()) {
        return;
    }
    job.finished_at = Some(now_secs());
    job.exit_code = Some(REFUSED);
    let _ = write(state, &job);
}

/// The status a refused runner records: its command never ran.
const REFUSED: i32 = 126;

/// `jeet review-job <state>`: run a job's command and record how it ended.
///
/// Its stdout and stderr are already the job's log.
pub fn run_job(path: &Path) -> Result<()> {
    let mut job = read(path)?;
    // Our own pid, which is the one `jeet review` launched: `nohup` execs us.
    job.pid = Some(std::process::id());
    job.runner_start = identity::start_of(std::process::id());
    write(path, &job)?;
    println!("$ {}", job.command);
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&job.command).current_dir(&job.worktree);
    // Run as a task, the command is that task's agent, never the assistant
    // that started it: the runner itself stays the assistant's, so the
    // check on its state file still applies.
    if job.env.iter().any(|(k, _)| k == crate::tasks::TASK_ID_ENV) {
        cmd.env_remove(crate::permissions::ASSISTANT_ENV);
    }
    let code = match cmd
        .envs(job.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("jeet: could not run the review command: {e}");
            127
        }
    };

    job.finished_at = Some(now_secs());
    job.exit_code = Some(code);
    write(path, &job)?;

    // Run headless, the command gets no hooks to say it ended: as its
    // task's Node, the runner says so for it, unless it reported done.
    if let Some((_, id)) = job.env.iter().find(|(k, _)| k == crate::tasks::TASK_ID_ENV) {
        if let Err(e) = App::open().and_then(|app| report_end(&app, id, code, job.started_at)) {
            eprintln!("jeet: could not report the review's end: {e:#}");
        }
    }

    if job.notify {
        let message = match code {
            0 => format!("Review of #{} is ready", job.pr),
            _ => format!("Review of #{} failed (status {code})", job.pr),
        };
        let url = job
            .env
            .iter()
            .find(|(key, _)| key == "JEET_PR_URL")
            .map(|(_, url)| url.as_str());
        crate::notify::notify(&crate::notify::Notification {
            title: "jeet",
            subtitle: Some(&job.title),
            body: &message,
            url,
        });
    }
    Ok(())
}

/// Tell review task `id`'s parent its command exited with `code`, once
/// for the run that started at `since`, unless that run reported done: the
/// task is reused for every review of its PR, so an earlier run's done
/// does not count.
fn report_end(app: &App, id: &str, code: i32, since: i64) -> Result<()> {
    let Some(task) = app.db.get_task(id)? else {
        return Ok(());
    };
    let body =
        format!("ended without reporting done; the review command exited with status {code}");
    let ended = crate::reports::StatusEvent::Ended;
    crate::reports::auto_status(app, &task, ended, &body, since, since)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PullRequest {
        PullRequest {
            number: 7,
            title: "$(rm -rf ~)".into(),
            url: "https://github.com/acme/widget/pull/7".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            head_ref_name: "x;touch pwned".into(),
            head_ref_oid: "abc".into(),
            base_ref_name: "main".into(),
            is_cross_repository: true,
        }
    }

    fn job(exit_code: Option<i32>, head: &str) -> Job {
        Job {
            pr: 7,
            title: "t".into(),
            head: head.into(),
            worktree: PathBuf::from("/tmp"),
            command: "true".into(),
            env: Vec::new(),
            notify: false,
            log: PathBuf::from("/tmp/7.log"),
            started_at: 0,
            pid: None,
            runner_start: None,
            finished_at: exit_code.map(|_| 0),
            exit_code,
            lost: false,
        }
    }

    #[test]
    fn only_github_values_reach_the_shell() {
        let cmd = expand(
            "claude -p '/review-pr {pr}' # {url} {branch} {title}",
            &pr(),
        );
        assert_eq!(
            cmd,
            "claude -p '/review-pr 7' # https://github.com/acme/widget/pull/7 {branch} {title}"
        );
        let env = job_env(&pr(), Path::new("/l"));
        assert!(env.contains(&("JEET_PR_BRANCH".into(), "x;touch pwned".into())));
    }

    #[test]
    fn a_finished_review_of_the_same_commit_is_not_repeated() {
        assert!(reason_to_skip(None, "abc").is_none());
        assert!(reason_to_skip(Some(&job(Some(0), "abc")), "abc").is_some());
        // New commits, or a run that failed, are worth another go.
        assert!(reason_to_skip(Some(&job(Some(0), "abc")), "def").is_none());
        assert!(reason_to_skip(Some(&job(Some(1), "abc")), "abc").is_none());
        // Still running (no pid recorded yet counts as running).
        assert!(reason_to_skip(Some(&job(None, "abc")), "def").is_some());
    }

    #[test]
    fn the_runner_records_how_the_command_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("7.json");
        let mut j = job(None, "abc");
        j.worktree = dir.path().to_path_buf();
        j.command = "test \"$JEET_PR\" = 7 && exit 3".into();
        j.env = vec![("JEET_PR".into(), "7".into())];
        write(&path, &j).unwrap();
        run_job(&path).unwrap();
        let done = read(&path).unwrap();
        assert_eq!(done.state(), JobState::Finished(3));
        assert_eq!(
            done.pid,
            Some(std::process::id()),
            "the runner records itself"
        );
        assert_eq!(done.runner_start, identity::start_of(std::process::id()));
        assert!(done.finished_at.is_some());
    }

    /// A job row whose pid is now another process's: its group is spared;
    /// the job's own is ended.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_spares_a_group_whose_pid_was_reused() {
        use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskState};
        use crate::placement::Placement;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("index.db")).unwrap();
        db.insert_task(&TaskRecord {
            id: "t".into(),
            slug: "t".into(),
            name: "t".into(),
            project: None,
            kind: TaskKind::Review,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: None,
            placement: Placement::Local,
            run_state: RunState::Run,
            run_state_by: None,
            max_children: None,
        })
        .unwrap();
        // An unrelated process group, as a job's would be.
        let mut other = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&db, "t", AnchorKind::Job, None, other.id()).unwrap();
        let mut row = db.task_anchors("t", AnchorKind::Job).unwrap().remove(0);
        db.delete_anchor(row.pid, row.start).unwrap();
        row.start += 1;
        db.insert_anchor(&row, None).unwrap();

        stop_jobs(&db, "t").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "an unrelated group was ended"
        );

        db.delete_anchor(row.pid, row.start).unwrap();
        identity::record(&db, "t", AnchorKind::Job, None, other.id()).unwrap();
        stop_jobs(&db, "t").unwrap();
        let status = other.wait().unwrap();
        assert!(!status.success(), "the job's group was not ended");
    }

    /// Archiving or deleting one task's dir ends the job running there and
    /// only it: not another task's, and not a runner whose pid a stale job
    /// file in the dir still names.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_in_ends_only_the_dirs_own_runner() {
        use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskState};
        use crate::placement::Placement;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: Database::open(&crate::config::index_path(&home)).unwrap(),
            home: home.clone(),
        };
        app.db
            .insert_task(&TaskRecord {
                id: "a".into(),
                slug: "a".into(),
                name: "a".into(),
                project: None,
                kind: TaskKind::Assistant,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: None,
                placement: Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
        let group = || {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let reviews = home.join("reviews/widget");
        std::fs::create_dir_all(&reviews).unwrap();
        let x = home.join("tasks/x");
        let y = home.join("tasks/y");
        // The assistant's runner for a job in `x`, and one for a job in `y`.
        let mut mine = group();
        let mut other = group();
        let state = |name: &str, worktree: &Path, pid: u32, start: Option<i64>| {
            let mut j = job(None, "abc");
            j.worktree = worktree.join("widget");
            j.pid = Some(pid);
            j.runner_start = start;
            write(&reviews.join(name), &j).unwrap();
        };
        let start = |child: &std::process::Child| identity::start_of(child.id()).unwrap();
        state("1.json", &x, mine.id(), Some(start(&mine)));
        state("2.json", &y, other.id(), Some(start(&other)));
        // A job in `x` its runner never finished, whose pid is now `other`'s,
        // and one from before runners recorded their start.
        state("3.json", &x, other.id(), Some(start(&other) + 1));
        state("4.json", &x, other.id(), None);
        for child in [&mine, &other] {
            identity::record(&app.db, "a", AnchorKind::Job, None, child.id()).unwrap();
        }

        stop_jobs_in(&app, &x).unwrap();
        let status = mine.wait().unwrap();
        assert!(!status.success(), "the dir's own job was not ended");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "another task's job was ended"
        );
        let left = app.db.task_anchors("a", AnchorKind::Job).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pid, other.id());
        let _ = other.kill();
        let _ = other.wait();
    }
    /// A test index holding the assistant and one top-level review task `t`.
    #[cfg(unix)]
    fn test_app(home: &Path) -> App {
        use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskState};
        use crate::placement::Placement;
        let app = App {
            config: crate::config::load_or_create(home).unwrap(),
            db: Database::open(&crate::config::index_path(home)).unwrap(),
            home: home.to_path_buf(),
        };
        for (id, kind) in [("a", TaskKind::Assistant), ("t", TaskKind::Review)] {
            app.db
                .insert_task(&TaskRecord {
                    id: id.into(),
                    slug: id.into(),
                    name: id.into(),
                    project: None,
                    kind,
                    state: TaskState::Active,
                    created_at: 0,
                    last_activity: 0,
                    parent_id: None,
                    placement: Placement::Local,
                    run_state: RunState::Run,
                    run_state_by: None,
                    max_children: None,
                })
                .unwrap();
        }
        app
    }

    /// A review command that takes a moment to finish once told to end,
    /// writing as it goes: stopping the job returns only once it is done, so
    /// what it wrote is there before an archive's snapshot.
    #[cfg(unix)]
    #[test]
    fn stop_jobs_waits_for_the_command_to_exit() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let done = home.join("done");
        let mut job = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "trap 'sleep 1; touch {}; exit 0' TERM; while :; do sleep 0.1; done",
                done.display()
            ))
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&app.db, "t", AnchorKind::Job, None, job.id()).unwrap();

        stop_jobs(&app.db, "t").unwrap();
        assert!(done.exists(), "returned before the command had exited");
        let _ = job.wait();
    }

    /// Archive or delete run under the task's review job would end itself
    /// part-way, so it is refused there; run elsewhere, it is not.
    #[cfg(unix)]
    #[test]
    fn refuse_inside_job_refuses_only_under_the_tasks_job() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let task_dir = home.join("tasks/t");
        let mut other = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        identity::record(&app.db, "t", AnchorKind::Job, None, other.id()).unwrap();
        refuse_inside_job(&app, "t", &task_dir, "archive").unwrap();

        // This process as the task's runner.
        let me = Anchor {
            pid: std::process::id(),
            start: identity::start_of(std::process::id()).unwrap(),
            task_id: "t".into(),
            kind: AnchorKind::Job,
            created_at: 0,
        };
        app.db.insert_anchor(&me, None).unwrap();
        let err = refuse_inside_job(&app, "t", &task_dir, "archive").unwrap_err();
        assert!(err.to_string().contains("runs under a review job"), "{err}");
        app.db.delete_anchor(me.pid, me.start).unwrap();

        // The assistant's runner for a job in the task's worktree.
        let reviews = home.join("reviews/widget");
        std::fs::create_dir_all(&reviews).unwrap();
        let mut j = job(None, "abc");
        j.worktree = task_dir.join("widget");
        j.pid = Some(me.pid);
        j.runner_start = Some(me.start);
        write(&reviews.join("1.json"), &j).unwrap();
        app.db
            .insert_anchor(
                &Anchor {
                    task_id: "a".into(),
                    ..me.clone()
                },
                None,
            )
            .unwrap();
        let err = refuse_inside_job(&app, "t", &task_dir, "delete").unwrap_err();
        assert!(err.to_string().contains("delete would end"), "{err}");

        let _ = other.kill();
        let _ = other.wait();
    }

    /// The runner is named in the job's file, and recorded, before it has
    /// the go-ahead: archiving or deleting the task in the moments before
    /// the runner writes itself ends the job all the same.
    #[cfg(unix)]
    #[test]
    fn the_runner_is_named_before_the_go_ahead() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let reviews = home.join("reviews/widget");
        std::fs::create_dir_all(&reviews).unwrap();
        let state = reviews.join("7.json");
        let mut j = job(None, "abc");
        j.worktree = home.join("tasks/t/widget");
        write(&state, &j).unwrap();
        // A runner that, given the go-ahead, copies the job's file as it
        // then reads, and keeps running as a job would.
        let seen = home.join("seen.json");
        let mut runner = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "head -c1 >/dev/null; cp {} {}; exec sleep 30",
                state.display(),
                seen.display()
            ))
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();

        hand_over(&app, &state, "a", false, &mut runner).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !seen.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let at_go = read(&seen).unwrap();
        assert_eq!(at_go.pid, Some(runner.id()));
        assert!(at_go.runner_start.is_some());
        assert_eq!(at_go.runner_start, identity::start_of(runner.id()));
        let found = jobs_in(&app, &home.join("tasks/t")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, runner.id());
        assert_eq!(found[0].task_id, "a");

        stop_jobs_in(&app, &home.join("tasks/t")).unwrap();
        assert!(!runner.wait().unwrap().success());
    }

    /// A runner that cannot be recorded is killed, and its job left for
    /// [`start`] to end, unless it is the user's: the user needs no anchor,
    /// so that job runs on.
    #[cfg(unix)]
    #[test]
    fn only_the_users_runner_runs_on_unrecorded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = test_app(&home);
        let state = home.join("7.json");
        // A runner gone before it is recorded: its process cannot be read.
        let gone = || {
            let mut runner = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
            drop(runner.stdin.take());
            runner.wait().unwrap();
            runner
        };

        write(&state, &job(None, "abc")).unwrap();
        let mut runner = gone();
        let err = hand_over(&app, &state, "a", false, &mut runner).unwrap_err();
        assert!(format!("{err:#}").contains("cannot be read"), "{err:#}");
        assert_eq!(read(&state).unwrap().pid, None);

        let mut runner = gone();
        hand_over(&app, &state, "t", true, &mut runner).unwrap();
        assert_eq!(read(&state).unwrap().pid, Some(runner.id()));
        assert!(app
            .db
            .task_anchors("t", AnchorKind::Job)
            .unwrap()
            .is_empty());
    }
}
