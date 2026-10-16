//! `jeet worker run <job>` and `jeet worker ls`; see [`crate::workers`].

use anyhow::{bail, Result};

use crate::agent::age_label;
use crate::cli::WorkerCommands;
use crate::config;
use crate::context::App;
use crate::db::workers::WorkerJobRecord;
use crate::manager;
use crate::permissions::{self, Caller};
use crate::workers::{self, JobSpec, Status};

/// How many finished jobs `ls` shows.
const RECENT: usize = 10;

pub fn run(app: &App, command: WorkerCommands) -> Result<()> {
    match command {
        WorkerCommands::Run { job, args } => run_job(app, &job, &args),
        WorkerCommands::Ls => ls(app),
    }
}

fn run_job(app: &App, name: &str, args: &[String]) -> Result<()> {
    let caller = permissions::caller(app)?;
    let requested_by = match &caller {
        Caller::User => None,
        Caller::Root => Some(manager::root(app)?.id),
        Caller::Node(task) => Some(task.id.clone()),
        Caller::Review(review) => bail!("review {} may not run worker jobs", review.label()),
        Caller::Worker(_) => bail!("a worker job only reads; it may not run worker jobs"),
    };
    let job = workers::load_job(&app.home, name)?;
    let mut prompt = job.prompt;
    if !args.is_empty() {
        prompt.push_str(&format!("\n\nInput: {}", args.join(" ")));
    }
    let outcome = workers::run(
        app,
        &JobSpec {
            name: job.name,
            prompt,
            schema: job.schema,
            requested_by,
        },
    )?;
    match (outcome.status, outcome.output) {
        (Status::Done, Some(output)) => {
            println!("{}", shown(&caller, &output.to_string()));
            Ok(())
        }
        (status, _) => {
            let why = outcome.error.unwrap_or_default();
            // The error may quote what the model said, or what it read.
            let message = format!("worker job {} {}: {why}", outcome.id, status.as_str());
            bail!(shown(&caller, &message))
        }
    }
}

/// A job's output as `caller` is shown it: the model wrote it from text
/// others wrote, so an agent gets it fenced as untrusted, unable to close
/// the fence.
fn shown(caller: &Caller, output: &str) -> String {
    match caller {
        Caller::User => output.to_string(),
        Caller::Root | Caller::Node(_) | Caller::Review(_) | Caller::Worker(_) => format!(
            "<worker-data untrusted=\"true\">{}</worker-data>",
            manager::fence::fenced(output)
        ),
    }
}

fn ls(app: &App) -> Result<()> {
    workers::sweep_lost(app);
    let jobs = workers::list_jobs(&app.home);
    println!("jobs in {}:", workers::jobs_dir(&app.home).display());
    if jobs.is_empty() {
        println!("  none; a job is a prompt in <name>.md there");
    }
    let width = jobs.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, about) in &jobs {
        println!("  {name:width$}  {about}");
    }

    let max = app.config.assistant.workers.max();
    match max {
        0 => println!(
            "worker jobs are off ([assistant.workers] max = 0 in {})",
            config::config_path(&app.home).display()
        ),
        max => println!(
            "{} of {max} slots busy",
            workers::busy_slots(&app.home, max)?
        ),
    }

    let running = app.db.running_worker_jobs()?;
    if !running.is_empty() {
        println!("running:");
        for job in &running {
            println!("  {}", line(app, job));
        }
    }
    let finished: Vec<WorkerJobRecord> = app
        .db
        .list_worker_jobs(RECENT + running.len())?
        .into_iter()
        .filter(|job| job.finished_at.is_some())
        .take(RECENT)
        .collect();
    if !finished.is_empty() {
        println!("recent:");
        for job in &finished {
            println!("  {}", line(app, job));
        }
    }
    Ok(())
}

/// One job on one line: id, job, who asked, status, age, cost.
fn line(app: &App, job: &WorkerJobRecord) -> String {
    let by = match &job.requested_by {
        None => "user".to_string(),
        Some(id) => match app.db.get_task(id) {
            Ok(Some(task)) => format!("task {}", task.slug),
            _ => id.clone(),
        },
    };
    let cost = job
        .cost_usd
        .map_or(String::new(), |cost| format!("  ${cost:.4}"));
    format!(
        "{}  {}  by {by}  {}  {}{cost}",
        job.id,
        job.job,
        job.status.as_str(),
        age_label(job.started_at)
    )
}
