use clap_complete::CompletionCandidate;

use crate::context::App;

pub fn repo_filter_candidates() -> Vec<CompletionCandidate> {
    repo_filters()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

pub fn all_branch_candidates() -> Vec<CompletionCandidate> {
    all_branch_names()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

pub fn task_candidates() -> Vec<CompletionCandidate> {
    let Ok(app) = App::open() else {
        return Vec::new();
    };
    let Ok(tasks) = app.db.list_tasks(None) else {
        return Vec::new();
    };
    tasks
        .into_iter()
        .map(|t| CompletionCandidate::new(t.slug))
        .collect()
}

pub fn project_candidates() -> Vec<CompletionCandidate> {
    let Ok(app) = App::open() else {
        return Vec::new();
    };
    crate::projects::list(&app)
        .unwrap_or_default()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// Worktree names of the task the completing shell is in.
pub fn task_worktree_candidates() -> Vec<CompletionCandidate> {
    let Ok(app) = App::open() else {
        return Vec::new();
    };
    let Ok(task) = crate::tasks::current(&app, None) else {
        return Vec::new();
    };
    let Ok(rows) = app.db.list_task_worktrees(&task.id) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|r| CompletionCandidate::new(r.dir_name))
        .collect()
}

pub fn repo_filters() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(app) = App::open() else {
        return out;
    };
    let Ok(repos) = app.db.list_repos(None) else {
        return out;
    };

    for repo in repos {
        out.push(repo.id.clone());
        let parts: Vec<&str> = repo.id.split('/').collect();
        if parts.len() >= 2 {
            let short = format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]);
            out.push(short);
            out.push(parts[parts.len() - 1].to_string());
        }
    }

    out.sort();
    out.dedup();
    out
}

pub fn all_branch_names() -> Vec<String> {
    branch_names(None)
}

pub fn branch_names(filter: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(app) = App::open() else {
        return out;
    };

    let repos = if let Some(filter) = filter {
        match crate::resolve::resolve_repo_filter(&app.db, filter) {
            Ok(repo) => vec![repo],
            Err(_) => return out,
        }
    } else if let Ok(repos) = app.db.list_repos(None) {
        repos
    } else {
        return out;
    };

    // The default branch, and every branch a task has a worktree on: read
    // from the index, so a tab press never waits on git.
    for repo in repos {
        out.push(repo.default_branch.clone());
        if let Ok(rows) = app.db.task_worktrees_of_repo(&repo.id) {
            out.extend(rows.into_iter().filter_map(|row| row.branch));
        }
    }

    out.sort();
    out.dedup();
    out
}
