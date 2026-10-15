//! `jeet review <pr>`: find or make the PR's review, check the PR out in
//! it, have its agent review it, and open the review viewer. Also what
//! `jeet task open|archive|unarchive|delete` do with the name of a review,
//! which is no task.

use std::io::{self, IsTerminal};

use anyhow::{bail, Context, Result};

use crate::agent::AgentSpec;
use crate::cd;
use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{TaskRecord, TaskState};
use crate::github;
use crate::manager::run;
use crate::messages::{self, Delivered};
use crate::permissions::{self, Caller};
use crate::resolve::repo_from_filter_or_cwd;
use crate::review;
use crate::runtime::{self, Admission, Started};
use crate::status;
use crate::tmux;
use crate::worktrees::now_secs;

use super::task::{confirm, from_sender, standing_in, warn_all};

pub fn run(
    app: &App,
    pr: &str,
    repo_filter: Option<&str>,
    start_review: bool,
    rerun: bool,
) -> Result<()> {
    let number = github::parse_pr_number(pr)?;
    let repo = repo_from_filter_or_cwd(app, repo_filter)?;
    let trunk = std::path::Path::new(&repo.trunk_path);

    eprintln!("jeet: looking up #{number}");
    let found = github::pr_history(trunk, number)?;
    let pr = found.pr.clone();
    // The title is whoever opened the PR's: an agent gets it fenced.
    let caller = permissions::caller(app)?;
    eprintln!(
        "jeet: #{} {} ({})",
        pr.number,
        super::task::shown_github(&caller, &pr.title),
        pr.state_label()
    );

    // What the assistant asks for reports to it: the review agent's done
    // and blocked go to the task that asked. A task with a parent never
    // gets here: `jeet review` is refused to it (`permissions::node_scope`
    // has no rule for it), nor does a review's agent (`review_gate`).
    let requested_by = match caller {
        Caller::Root => Some(super::task::assistant_parent(app)?.id),
        Caller::User => None,
        Caller::Node(node) => bail!("task {} may not run `jeet review`", node.slug),
        Caller::Review(review) => bail!("review {} may not run `jeet review`", review.label()),
    };
    let checkout = review::review_for(app, &repo, &pr, requested_by.as_deref())?;
    let review = &checkout.review;
    // Only now: a sync started before the checkout would be fetching this
    // repo just as `gh pr checkout` does. The checkout waits out one already
    // running, but there is no need to start one in its way.
    crate::store::maybe_sync_background(app);
    for warning in &checkout.warnings {
        eprintln!("jeet: {warning}");
    }
    let verb = if checkout.created {
        "new review"
    } else {
        "review"
    };
    eprintln!(
        "jeet: {verb} {} -> {}",
        review.slug,
        checkout.path.display()
    );

    if start_review {
        // Once its agent has been asked, a review stays with it: a job
        // beside it would leave a second session.
        let has_agent = review::asked(&review::folded(app, review)).is_some();
        if app.config.review.command().is_some() && has_agent {
            eprintln!(
                "jeet: warning: [review] command is deprecated, and not run for a review its \
                 agent has started; the agent is asked instead"
            );
        }
        if app.config.review.command().is_some() && !has_agent {
            eprintln!(
                "jeet: warning: [review] command is deprecated: it runs headless, outside the \
                 review's agent; remove it to have the agent review in its tmux pane, \
                 asked with [review] prompt"
            );
            review::remember_requester(app, &review.id, requested_by.as_deref())?;
            legacy_review(app, &repo, &pr, &checkout, requested_by.as_deref(), rerun)?;
        } else {
            let asked = review::start_agent(
                app,
                &repo,
                review,
                &pr,
                &checkout.path,
                rerun,
                requested_by.as_deref(),
            );
            match asked {
                Ok(line) => eprintln!("jeet: {line}"),
                Err(e) => eprintln!("jeet: warning: the review agent did not start: {e:#}"),
            }
        }
    }

    if io::stdout().is_terminal() {
        // The review itself, then the shell lands in the worktree to work on it.
        crate::tui::review::run(&checkout.path, &pr)?;
        return crate::commands::explore::hand_off(&checkout.path);
    }
    // Scripted: behave like `jeet worktree`, printing the path to consume.
    println!("{}", checkout.path.display());
    cd::request(&checkout.path)
}

/// The deprecated `[review] command`, run as a headless background job.
fn legacy_review(
    app: &App,
    repo: &crate::db::RepoRecord,
    pr: &github::PullRequest,
    checkout: &review::ReviewCheckout,
    requested_by: Option<&str>,
    rerun: bool,
) -> Result<()> {
    let review = &checkout.review;
    // Asked for by a task, the review command is the review's agent and
    // reports to it; otherwise it runs as the user. Either way the runner
    // is recorded as the review's, so archiving or deleting the review
    // finds the job and ends it.
    let as_task = requested_by.map(|_| review.id.as_str());
    let runner_for = review.id.as_str();
    let worktree = checkout.path.as_path();
    let config = &app.config.review;
    let Some(template) = config.command() else {
        return Ok(());
    };
    // Taken before the job is recorded and held until it is, so a stop
    // landing meanwhile waits for it and ends it.
    let _guard = run::start_lock(app)?;
    let existing = review::load(app, repo, pr.number);
    if let Some(job) = &existing {
        if rerun && job.state() == review::JobState::Running {
            eprintln!(
                "jeet: a review of #{} is still running; not starting another",
                pr.number
            );
            return Ok(());
        }
    }
    if !rerun {
        if let Some(why) = review::reason_to_skip(existing.as_ref(), &pr.head_ref_oid) {
            eprintln!("jeet: {why}");
            return Ok(());
        }
    }
    let job = review::start(
        app, repo, pr, worktree, config, template, as_task, runner_for,
    )?;
    eprintln!("jeet: reviewing in the background: {}", job.command);
    eprintln!("jeet: output -> {}", job.log.display());
    if as_task.is_some() {
        // No harness launches it, so no deny rules reach it.
        eprintln!(
            "jeet: warning: the review command runs outside a harness launch, so it has no \
             deny rules and is not stopped from force-pushing or merging; only jeet's own \
             commands are gated"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `jeet task open|archive|unarchive|delete <review>`
// ---------------------------------------------------------------------------

/// What a name given to `jeet task open|archive|unarchive|delete` is: a task,
/// or a review, which those verbs reach by its slug so the paths they have
/// always documented keep working.
pub enum Named {
    Task(TaskRecord),
    Review(Box<ReviewRecord>),
}

/// The review `name` names exactly: its slug or its id.
fn review_exact(app: &App, name: &str) -> Result<Option<ReviewRecord>> {
    match app.db.find_review_by_slug(name)? {
        Some(review) => Ok(Some(review)),
        None => app.db.get_review(name),
    }
}

/// The one review whose slug starts with `name`, if exactly one does.
fn review_by_prefix(app: &App, name: &str) -> Result<Option<ReviewRecord>> {
    let mut found = app
        .db
        .list_reviews(None)?
        .into_iter()
        .filter(|r| r.slug.starts_with(name));
    Ok(match (found.next(), found.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    })
}

/// The task or review `name` names for `verb`: a review by its slug or id,
/// else the task [`permissions::scoped_target`] finds, else (unless
/// `exact`) the one review whose slug starts with `name`. Only the user and
/// the assistant reach a review; a task with a parent reaches only the
/// tasks below it.
pub fn named(app: &App, verb: permissions::Verb, name: &str, exact: bool) -> Result<Named> {
    let target = permissions::Target::Key {
        key: name.to_string(),
        exact,
    };
    let review = match review_exact(app, name)? {
        Some(review) => review,
        None => match permissions::scoped_target(app, verb, target) {
            Ok(task) => return Ok(Named::Task(task)),
            Err(e) => {
                let by_prefix = match exact {
                    true => None,
                    false => review_by_prefix(app, name)?,
                };
                match by_prefix {
                    Some(review) => review,
                    None => return Err(e),
                }
            }
        },
    };
    if let Caller::Node(node) = permissions::caller(app)? {
        bail!(
            "task {} may not run `jeet {}` on review {}: it may run it only on the tasks \
             below it; ask with `jeet report blocked|question \"…\"`",
            node.slug,
            verb.name(),
            review.label()
        );
    }
    Ok(Named::Review(Box::new(review)))
}

/// [`named`] for a verb that reaches a review's agent, which only the user
/// and the task that asked for the review may do.
pub fn named_agent(app: &App, verb: permissions::Verb, name: &str) -> Result<Named> {
    let named = named(app, verb, name, false)?;
    if let Named::Review(review) = &named {
        let asked = match permissions::caller(app)? {
            Caller::User => true,
            Caller::Root => {
                let assistant = app.db.get_assistant_task()?.map(|task| task.id);
                review.requested_by.is_some() && review.requested_by == assistant
            }
            Caller::Node(_) | Caller::Review(_) => false,
        };
        if !asked {
            bail!(
                "`jeet {}` reaches review {}'s agent only for the user, or for the task that \
                 asked for the review",
                verb.name(),
                review.label()
            );
        }
    }
    Ok(named)
}

/// Get a message to the review's agent, however it is doing; see
/// [`messages`].
pub fn send(app: &App, review: &ReviewRecord, message: &str) -> Result<()> {
    let called = review.slug.as_str();
    if review.state == TaskState::Archived {
        bail!(
            "review {0} is archived; `jeet task unarchive {0}` first",
            review.slug
        );
    }
    let (message, from) = from_sender(app, &permissions::caller(app)?, message)?;
    let agent = review::ReviewAgent::new(app, review);
    let delivered = messages::deliver(&review::dir(app, review), &agent, &message, from)?;
    let _ = app.db.touch_review(&review.id, now_secs());
    match delivered {
        Delivered::Typed => eprintln!("jeet: sent to {called}'s agent"),
        Delivered::Queued => eprintln!(
            "jeet: {called}'s agent is busy, waiting on you, or has text you have not sent in \
             its prompt, so the message is queued; it gets it when it finishes or sits idle"
        ),
        Delivered::Started => eprintln!(
            "jeet: started {called}'s agent with the message in tmux session {}",
            review::session_name(review)
        ),
        Delivered::Held => eprintln!(
            "jeet: {called}'s agent is not running and cannot be started from here, so the \
             message is queued; it gets it when it next runs"
        ),
        Delivered::Paused => eprintln!(
            "jeet: review {called} is paused at its [review] caps, so the message is queued; \
             it gets it once the review is asked for again"
        ),
        Delivered::StartQueued { live, max } => {
            let (cap, frees) = run::REVIEW_MAX_LIVE;
            eprintln!(
                "jeet: review {called}'s start waits on {cap} ({live}/{max} live), so the \
                 message is queued; it starts with it when {frees}"
            )
        }
        Delivered::Wake => {
            eprintln!("jeet: sent to {called}'s agent; it gets it now, through its wake channel")
        }
    }
    Ok(())
}

/// End the review's agent and resume its session in a new one in the same
/// tmux session, as `jeet task restart` does a task's: refused for an agent
/// mid-turn, waiting on someone, starting, or holding text that was not
/// sent, unless `force`.
pub fn restart(app: &App, review: &ReviewRecord, force: bool) -> Result<()> {
    let called = review.slug.as_str();
    if review.state == TaskState::Archived {
        bail!(
            "review {0} is archived; `jeet task unarchive {0}` first",
            review.slug
        );
    }
    runtime::refuse_inside_agent_session(
        &review.id,
        &review.slug,
        &review::subject(review),
        "restart",
    )?;
    let spec = AgentSpec::from_config(&app.config)?;
    if !spec.supports_sessions() {
        bail!(
            "{} keeps no sessions jeet can resume, so a restart would lose review {called}'s \
             conversation; stop it and `jeet task open {called}` if you want a fresh one",
            spec.display(),
        );
    }
    let tmux_session = review::session_name(review);
    if !(tmux::available() && tmux::agent_alive(&tmux_session)) {
        bail!("review {called}'s agent is not running; `jeet task open {called}` starts it");
    }
    // Ended now, it would not start again until it is asked for again.
    if review.paused {
        bail!(
            "review {called} is paused at its [review] caps; ask for the review again before \
             restarting its agent"
        );
    }
    if !force {
        let activity = review::activity(app, review, true);
        let draft = spec
            .harness
            .has_draft(&tmux::capture_styled(&tmux_session).unwrap_or_default());
        if let Some(why) = runtime::busy_reason(&activity, draft, true) {
            bail!(
                "review {called}'s agent {why}, which a restart would cut off; wait for it, or \
                 pass --force"
            );
        }
    }
    review::adopt_sessions(app, review);
    let review = &app
        .db
        .get_review(&review.id)?
        .unwrap_or_else(|| review.clone());
    let Some(session) = review.session_id.clone() else {
        bail!(
            "review {called} has no session jeet knows of to resume; stop it and `jeet task \
             open {called}`"
        );
    };
    // Its busy time so far, before its session ends, as a stop charges it.
    if let Err(e) = status::fold_busy_review(app, review, true, now_secs()) {
        eprintln!("jeet: could not charge review {called}'s limits: {e:#}");
    }
    runtime::stop_session(&review.slug)?;
    app.db.delete_task_anchors(&review.id, AnchorKind::Pane)?;
    let admission = match permissions::caller(app)? {
        Caller::User => Admission::Bypass,
        Caller::Root | Caller::Node(_) | Caller::Review(_) => Admission::Gate,
    };
    let running = review::ensure_running(app, review, Some(&session), None, admission)
        .with_context(|| {
            format!(
                "ended review {called}'s agent, but could not start it again; `jeet task open \
                 {called} --session {session}` resumes it"
            )
        })?;
    warn_all(&running.warnings);
    match running.started {
        Started::Now => eprintln!(
            "jeet: restarted review {called}'s agent in tmux session {}, resuming session \
             {session}",
            running.tmux_session
        ),
        Started::AlreadyRunning => eprintln!(
            "jeet: ended review {called}'s agent, but another was started meanwhile in tmux \
             session {}",
            running.tmux_session
        ),
        Started::Queued { live, max, .. } => eprintln!(
            "jeet: ended review {called}'s agent; it waits on {} ({live}/{max} live) to start \
             again, resuming its session",
            run::REVIEW_MAX_LIVE.0
        ),
    }
    Ok(())
}

/// Attach to the review's agent in its tmux session, starting it first if
/// it is not running, as `jeet task open` does a task's.
pub fn open(app: &App, review: &ReviewRecord, session: Option<&str>) -> Result<()> {
    if review.state == TaskState::Archived {
        bail!(
            "review {0} is archived; `jeet task unarchive {0}` first",
            review.slug
        );
    }
    let dir = review::dir(app, review);
    // The user at a terminal asked for it: over `max_live` it starts anyway.
    // Anyone else waits for a slot like any start.
    let admission = match permissions::caller(app)? {
        Caller::User => Admission::Bypass,
        _ => Admission::Gate,
    };
    let name = review::session_name(review);
    if !(tmux::has_session(&name) && tmux::pane_alive(&name)) {
        if let Some(job) = review::running_job(app, review) {
            bail!(review::running_refusal(review, &job));
        }
    }
    let running = review::ensure_running(app, review, session, None, admission)?;
    warn_all(&running.warnings);
    let tmux_session = &running.tmux_session;
    match running.started {
        Started::Now => eprintln!(
            "jeet: started {} in tmux session {tmux_session}",
            AgentSpec::from_config(&app.config)?.display()
        ),
        Started::AlreadyRunning => {}
        Started::Queued { live, max, .. } => {
            let (cap, frees) = run::REVIEW_MAX_LIVE;
            eprintln!("jeet: queued ({live}/{max} live, {cap}); its agent starts when {frees}");
            return Ok(());
        }
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        eprintln!("jeet: `tmux attach -t {tmux_session}` to talk to the agent");
        println!("{}", dir.display());
        return Ok(());
    }
    let code = tmux::attach(tmux_session)?;
    let _ = app.db.touch_review(&review.id, now_secs());
    cd::request(&dir)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Archive the review once the user has seen which ignored files would go
/// with its worktree and agreed — or passed --force.
pub fn archive(app: &App, review: &ReviewRecord, force: bool) -> Result<()> {
    if review.state == TaskState::Archived {
        eprintln!("jeet: review {} is archived already", review.slug);
        return Ok(());
    }
    crate::runtime::refuse_inside_agent_session(
        &review.id,
        &review.slug,
        &review::subject(review),
        "archive",
    )?;
    if !force {
        let unkept = review::unkept(app, review);
        if !unkept.is_empty() {
            let mut report = format!(
                "archiving review {} keeps its uncommitted work, but not the files git \
                 ignores, which are deleted with its worktree:",
                review.slug
            );
            for line in &unkept {
                report.push_str(&format!("\n  {line}"));
            }
            confirm(
                &report,
                "archive it?",
                &format!("kept review {} as it is", review.slug),
            )?;
        }
    }
    let warnings = review::archive(app, review)?;
    warn_all(&warnings);
    eprintln!("jeet: archived review {}", review.slug);
    Ok(())
}

/// Bring an archived review back.
pub fn unarchive(app: &App, review: &ReviewRecord) -> Result<()> {
    if review.state != TaskState::Archived {
        eprintln!("jeet: review {} is not archived", review.slug);
        return Ok(());
    }
    let warnings = review::unarchive(app, review)?;
    warn_all(&warnings);
    eprintln!(
        "jeet: review {} is back at {}",
        review.slug,
        review::dir(app, review).display()
    );
    Ok(())
}

/// Delete the review once the user has seen what would be lost and agreed
/// — or passed --force.
pub fn delete(app: &App, review: &ReviewRecord, force: bool) -> Result<()> {
    let dir = review::dir(app, review);
    if standing_in(&dir) {
        bail!(
            "you are inside {}; cd out of it before deleting the review",
            dir.display()
        );
    }
    crate::runtime::refuse_inside_agent_session(
        &review.id,
        &review.slug,
        &review::subject(review),
        "delete",
    )?;
    if !force {
        let losses = review::losses(app, review)?;
        let mut report = format!(
            "deleting review {} removes {} and its worktree",
            review.slug,
            dir.display()
        );
        if !losses.is_empty() {
            report.push_str(", losing:");
        }
        for loss in &losses {
            report.push_str(&format!("\n  {loss}"));
        }
        confirm(
            &report,
            "delete it?",
            &format!("kept review {}", review.slug),
        )?;
    }
    let warnings = review::delete(app, review)?;
    warn_all(&warnings);
    eprintln!("jeet: deleted review {}", review.slug);
    Ok(())
}
