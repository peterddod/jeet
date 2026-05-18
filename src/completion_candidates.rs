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

    for repo in repos {
        out.push(repo.default_branch.clone());
        if let Ok(wts) = app.db.list_worktrees(&repo.id) {
            for wt in wts {
                out.push(wt.branch);
            }
        }
    }

    out.sort();
    out.dedup();
    out
}
