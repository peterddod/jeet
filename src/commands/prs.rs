//! `jeet prs [repo]`: the open pull requests, and which of them you have.

use std::path::Path;

use anyhow::Result;
use unicode_width::UnicodeWidthStr;

use crate::context::App;
use crate::db::RepoRecord;
use crate::github::{self, ListedPr};
use crate::review::{self, Job};
use crate::tui::ui::truncate;
use crate::worktrees;

/// Plenty for a review queue; a repository with more open than this has a
/// problem no listing will solve.
const LIMIT: usize = 100;

const TITLE_WIDTH: usize = 50;

/// An open PR and what is true of it for you — shared by `jeet prs` and the
/// explorer's PR list, so the two never disagree.
#[derive(Debug, Clone)]
pub struct PrRow {
    pub pr: ListedPr,
    pub requested: bool,
    pub checked_out: bool,
    pub job: Option<Job>,
}

impl PrRow {
    /// Asked to review it, checked out, reviewed — whichever apply.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.requested {
            notes.push("your review requested".to_string());
        }
        if self.checked_out {
            notes.push("checked out".to_string());
        }
        if let Some(job) = &self.job {
            notes.push(format!("review {}", job.describe()));
        }
        notes
    }
}

/// The open PRs of `repo`, newest first.
pub fn rows(app: &App, repo: &RepoRecord) -> Result<Vec<PrRow>> {
    let trunk = Path::new(&repo.trunk_path);
    let prs = github::open_prs(trunk, LIMIT)?;
    if prs.is_empty() {
        return Ok(Vec::new());
    }
    // Only to flag the PRs waiting on you; without it the list still stands.
    let me = github::current_user(trunk).unwrap_or_default();
    let local: Vec<String> = worktrees::list(app, repo)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !e.missing)
        .filter_map(|e| e.branch)
        .collect();
    Ok(prs
        .into_iter()
        .map(|pr| PrRow {
            requested: pr.requests(&me),
            checked_out: local.contains(&pr.local_branch()),
            job: review::load(app, repo, pr.number),
            pr,
        })
        .collect())
}

pub fn run(app: &App, filter: Option<&str>) -> Result<()> {
    let repo = match filter {
        Some(filter) => crate::resolve::resolve_repo_filter(&app.db, filter)?,
        None => {
            let cwd = std::env::current_dir()?;
            match crate::resolve::resolve_context(app, &cwd) {
                Ok(ctx) => ctx.repo,
                Err(e) => anyhow::bail!(
                    "{e}\nrun from inside a repository or name one: `jeet prs <repo>`"
                ),
            }
        }
    };
    let rows = rows(app, &repo)?;
    if rows.is_empty() {
        println!("{}: no open pull requests", repo.id);
        return Ok(());
    }

    let count = rows.len();
    println!(
        "{}: {count} open pull request{}",
        repo.id,
        if count == 1 { "" } else { "s" }
    );
    let number_width = rows
        .iter()
        .map(|row| row.pr.number.to_string().len() + 1)
        .max()
        .unwrap_or(0);
    let author_width = rows
        .iter()
        .map(|row| row.pr.author.login.width())
        .max()
        .unwrap_or(0)
        .min(20);
    for row in &rows {
        let pr = &row.pr;
        let line = format!(
            "  {:>nw$}  {}  {}  {:>13}  {:<17}  {}",
            format!("#{}", pr.number),
            pad(&truncate(&pr.title, TITLE_WIDTH), TITLE_WIDTH),
            pad(&truncate(&pr.author.login, author_width), author_width),
            format!("+{} -{}", pr.additions, pr.deletions),
            pr.status(),
            row.notes().join(" · "),
            nw = number_width,
        );
        println!("{}", line.trim_end());
    }
    let hint = match filter {
        Some(f) => format!("jeet review <number> --repo {f}"),
        None => "jeet review <number>".to_string(),
    };
    eprintln!("jeet: `{hint}` checks one out to review");
    Ok(())
}

/// Left-align in display columns: `{:<}` counts characters, and a title with
/// CJK or emoji in it would push every column after it out of line.
pub fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}
