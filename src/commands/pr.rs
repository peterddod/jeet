//! `jeet pr merge <pr>`: merge a pull request, but only one GitHub would let
//! through its merge button without anyone's override: open, not a draft,
//! free of conflicts, every check passed, and not blocked by branch
//! protection. Nothing here bypasses a rule; a PR that is not ready is
//! refused, saying why.

use std::path::Path;

use anyhow::{bail, Result};

use crate::commands::task::shown_github;
use crate::context::App;
use crate::db::RepoRecord;
use crate::github::{self, CheckState, MergeMethod, MergeSettings, MergeStatus};
use crate::permissions;
use crate::pr_history;
use crate::resolve::repo_from_filter_or_cwd;

pub fn merge(
    app: &App,
    pr: &str,
    repo_filter: Option<&str>,
    method: Option<MergeMethod>,
    keep_branch: bool,
) -> Result<()> {
    let number = github::parse_pr_number(pr)?;
    let filter = repo_filter.map(str::to_string).or_else(|| repo_of_url(pr));
    let repo = repo_from_filter_or_cwd(app, filter.as_deref())?;
    let trunk = Path::new(&repo.trunk_path);

    // A title, check names and a branch are written by whoever opened the
    // PR, a fork's author included: an agent gets them fenced.
    let caller = permissions::caller(app)?;
    eprintln!("jeet: looking up #{number}");
    let status = github::merge_status(trunk, number)?;
    if let Some(why) = refusal(&status) {
        bail!("not merging: {}", shown_github(&caller, &why));
    }
    let settings = github::merge_settings(trunk)?;
    let configured = app.config.merge.method;
    let method = choose_method(method, configured, &settings)?;
    if let Some(configured) = configured.filter(|c| *c != method) {
        eprintln!(
            "jeet: the repository does not allow {} merges ([merge] method); using {}",
            configured.as_str(),
            method.as_str()
        );
    }

    let pr = &status.history.pr;
    github::merge_pr(trunk, number, method, &pr.head_ref_oid)?;
    eprintln!(
        "jeet: merged #{number} {} ({})",
        shown_github(&caller, &pr.title),
        method.as_str()
    );

    // As `gh pr merge --delete-branch` would, but only on GitHub: a task
    // may still have the branch checked out. A fork's branch is not ours to
    // delete, and a repository that deletes merged branches already has.
    if !keep_branch && !pr.is_cross_repository && !settings.delete_branch_on_merge {
        let branch = shown_github(&caller, &pr.head_ref_name);
        match github::delete_remote_branch(trunk, &pr.head_ref_name) {
            Ok(()) => eprintln!("jeet: deleted branch {branch} on GitHub"),
            Err(e) => eprintln!("jeet: warning: could not delete branch {branch}: {e:#}"),
        }
    }

    if let Err(e) = record(app, &repo, &status) {
        eprintln!("jeet: warning: could not record the merge in task history: {e:#}");
    }
    Ok(())
}

/// The repository a PR URL names, as the store's ids spell it:
/// `https://github.com/acme/api/pull/7` is `github.com/acme/api`.
fn repo_of_url(text: &str) -> Option<String> {
    let (repo, _) = text.trim().split_once("/pull/")?;
    let repo = repo.split_once("://").map_or(repo, |(_, rest)| rest);
    (repo.matches('/').count() == 2).then(|| repo.to_string())
}

/// Why `status` must not be merged, or nothing when it may.
pub fn refusal(status: &MergeStatus) -> Option<String> {
    let pr = &status.history.pr;
    let n = pr.number;
    match pr.state.as_str() {
        "OPEN" => {}
        "MERGED" => return Some(format!("#{n} is already merged")),
        _ => return Some(format!("#{n} is closed")),
    }
    if pr.is_draft {
        return Some(format!("#{n} is a draft"));
    }
    if pr.head_ref_oid.is_empty() {
        return Some(format!("GitHub did not say which commit #{n} is at"));
    }
    match status.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => return Some(format!("#{n} has conflicts with its base branch")),
        _ => {
            return Some(format!(
                "GitHub has not worked out yet whether #{n} can be merged; try again shortly"
            ))
        }
    }
    let named = |state: CheckState| -> Vec<&str> {
        status
            .status_check_rollup
            .iter()
            .filter(|c| c.state() == state)
            .map(|c| c.name())
            .collect()
    };
    let failed = named(CheckState::Failed);
    if !failed.is_empty() {
        return Some(format!("checks failed on #{n}: {}", failed.join(", ")));
    }
    let pending = named(CheckState::Pending);
    if !pending.is_empty() {
        return Some(format!(
            "checks have not finished on #{n}: {}",
            pending.join(", ")
        ));
    }
    match pr.review_decision.as_str() {
        "CHANGES_REQUESTED" => return Some(format!("changes are requested on #{n}")),
        "REVIEW_REQUIRED" => return Some(format!("#{n} still needs an approving review")),
        _ => {}
    }
    match status.merge_state_status.as_str() {
        "CLEAN" | "HAS_HOOKS" => None,
        "BLOCKED" => Some(format!(
            "GitHub blocks merging #{n}: branch protection wants something it does not have yet \
             (a review or a required check)"
        )),
        "BEHIND" => Some(format!(
            "#{n} is behind its base branch, which must be brought up to date first"
        )),
        "DIRTY" => Some(format!("#{n} has conflicts with its base branch")),
        "DRAFT" => Some(format!("#{n} is a draft")),
        "UNSTABLE" => Some(format!(
            "GitHub reports checks on #{n} that have not passed"
        )),
        "UNKNOWN" | "" => Some(format!(
            "GitHub has not worked out yet whether #{n} can be merged; try again shortly"
        )),
        other => Some(format!("GitHub says #{n} cannot be merged ({other})")),
    }
}

/// The merge method: `flag` if given, which the repository must allow; else
/// `configured` if it allows that; else its own default; else the first it
/// allows.
pub fn choose_method(
    flag: Option<MergeMethod>,
    configured: Option<MergeMethod>,
    settings: &MergeSettings,
) -> Result<MergeMethod> {
    let allowed = settings.allowed();
    let allowed_list = || {
        allowed
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(method) = flag {
        if !settings.allows(method) {
            bail!(
                "the repository does not allow {} merges (it allows: {})",
                method.as_str(),
                allowed_list()
            );
        }
        return Ok(method);
    }
    if let Some(method) = configured.filter(|m| settings.allows(*m)) {
        return Ok(method);
    }
    match settings.default_method().or(allowed.first().copied()) {
        Some(method) => Ok(method),
        None => bail!("the repository allows no merge method jeet knows"),
    }
}

/// Write the merge into the history of every task that has the PR: the ones
/// whose history records it, or with a worktree on its branch or checked out
/// for it, and into the PR's review.
fn record(app: &App, repo: &RepoRecord, status: &MergeStatus) -> Result<()> {
    let pr = &status.history.pr;
    let mut tasks = app.db.tasks_with_pr(&repo.id, pr.number)?;
    for wt in app.db.task_worktrees_of_repo(&repo.id)? {
        let on_branch = wt.branch.as_deref() == Some(pr.local_branch().as_str());
        if on_branch || wt.pr_number == Some(pr.number) {
            tasks.push(wt.task_id);
        }
    }
    tasks.sort();
    tasks.dedup();
    let review = app.db.find_review(&repo.id, pr.number)?;
    if tasks.is_empty() && review.is_none() {
        return Ok(());
    }
    // What GitHub says now, merged; the merge succeeded whatever it says.
    let trunk = Path::new(&repo.trunk_path);
    let mut merged =
        github::pr_history(trunk, pr.number).unwrap_or_else(|_| status.history.clone());
    merged.pr.state = "MERGED".into();
    for task in tasks {
        app.db
            .upsert_task_pr(&pr_history::to_record(&task, &repo.id, merged.clone()))?;
    }
    if let Some(review) = review {
        app.db
            .set_review_pr(&review.id, &merged.pr.title, "MERGED", Some(&merged.pr.url))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{HistoryPr, PullRequest, StatusCheck};

    fn ready() -> MergeStatus {
        MergeStatus {
            history: HistoryPr {
                pr: PullRequest {
                    number: 42,
                    title: "Fix it".into(),
                    url: "https://github.com/acme/api/pull/42".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    review_decision: "APPROVED".into(),
                    head_ref_name: "fix".into(),
                    head_ref_oid: "abc".into(),
                    base_ref_name: "main".into(),
                    is_cross_repository: false,
                },
                additions: 1,
                deletions: 1,
                updated_at: String::new(),
            },
            mergeable: "MERGEABLE".into(),
            merge_state_status: "CLEAN".into(),
            status_check_rollup: vec![
                run("build", "COMPLETED", "SUCCESS"),
                run("lint", "COMPLETED", "SKIPPED"),
                status("ci/legacy", "SUCCESS"),
            ],
        }
    }

    fn run(name: &str, state: &str, conclusion: &str) -> StatusCheck {
        StatusCheck::CheckRun {
            name: name.into(),
            status: state.into(),
            conclusion: conclusion.into(),
        }
    }

    fn status(context: &str, state: &str) -> StatusCheck {
        StatusCheck::StatusContext {
            context: context.into(),
            state: state.into(),
        }
    }

    fn refused(change: impl FnOnce(&mut MergeStatus)) -> String {
        let mut status = ready();
        change(&mut status);
        refusal(&status).expect("refused")
    }

    #[test]
    fn a_ready_pr_may_be_merged() {
        assert_eq!(refusal(&ready()), None);
        let mut hooks = ready();
        hooks.merge_state_status = "HAS_HOOKS".into();
        hooks.status_check_rollup.clear();
        hooks.history.pr.review_decision = String::new();
        assert_eq!(refusal(&hooks), None, "no checks and no review rule");
    }

    #[test]
    fn a_pr_that_is_not_ready_is_refused_saying_why() {
        let cases: Vec<(String, &str)> = vec![
            (
                refused(|s| s.history.pr.state = "MERGED".into()),
                "already merged",
            ),
            (
                refused(|s| s.history.pr.state = "CLOSED".into()),
                "is closed",
            ),
            (refused(|s| s.history.pr.is_draft = true), "is a draft"),
            (refused(|s| s.mergeable = "CONFLICTING".into()), "conflicts"),
            (refused(|s| s.mergeable = "UNKNOWN".into()), "try again"),
            (
                refused(|s| {
                    s.status_check_rollup
                        .push(run("test", "COMPLETED", "FAILURE"))
                }),
                "checks failed on #42: test",
            ),
            (
                refused(|s| s.status_check_rollup.push(status("deploy", "ERROR"))),
                "checks failed on #42: deploy",
            ),
            (
                refused(|s| s.status_check_rollup.push(run("test", "IN_PROGRESS", ""))),
                "not finished on #42: test",
            ),
            (
                refused(|s| s.status_check_rollup.push(status("deploy", "PENDING"))),
                "not finished on #42: deploy",
            ),
            (
                refused(|s| s.history.pr.review_decision = "CHANGES_REQUESTED".into()),
                "changes are requested",
            ),
            (
                refused(|s| s.history.pr.review_decision = "REVIEW_REQUIRED".into()),
                "approving review",
            ),
            (
                refused(|s| s.merge_state_status = "BLOCKED".into()),
                "blocks merging",
            ),
            (
                refused(|s| s.merge_state_status = "BEHIND".into()),
                "behind its base",
            ),
            (
                refused(|s| s.merge_state_status = "DIRTY".into()),
                "conflicts",
            ),
            (
                refused(|s| s.merge_state_status = "UNSTABLE".into()),
                "not passed",
            ),
            (
                refused(|s| s.merge_state_status = "UNKNOWN".into()),
                "try again",
            ),
            (
                refused(|s| s.history.pr.head_ref_oid.clear()),
                "which commit",
            ),
        ];
        for (message, expected) in cases {
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
    }

    #[test]
    fn gh_check_rollups_parse() {
        let json = r#"{"number":5,"title":"T","url":"u","state":"OPEN","isDraft":false,"reviewDecision":"","headRefName":"f","headRefOid":"abc","baseRefName":"main","isCrossRepository":false,"additions":1,"deletions":2,"updatedAt":"2026-10-01T00:00:00Z","mergeable":"MERGEABLE","mergeStateStatus":"UNSTABLE","statusCheckRollup":[{"__typename":"CheckRun","name":"build","status":"IN_PROGRESS","conclusion":""},{"__typename":"StatusContext","context":"CodeRabbit","state":"SUCCESS"},{"__typename":"SomethingNew"}]}"#;
        let status: MergeStatus = serde_json::from_str(json).unwrap();
        let states: Vec<CheckState> = status
            .status_check_rollup
            .iter()
            .map(StatusCheck::state)
            .collect();
        assert_eq!(
            states,
            [CheckState::Pending, CheckState::Passed, CheckState::Pending]
        );
        assert_eq!(status.history.additions, 1);
        let json = json.replace(
            r#""statusCheckRollup":[{"__typename":"CheckRun","name":"build","status":"IN_PROGRESS","conclusion":""},{"__typename":"StatusContext","context":"CodeRabbit","state":"SUCCESS"},{"__typename":"SomethingNew"}]"#,
            r#""statusCheckRollup":null"#,
        );
        let status: MergeStatus = serde_json::from_str(&json).unwrap();
        assert!(status.status_check_rollup.is_empty());
    }

    fn settings(merge: bool, squash: bool, rebase: bool, default: &str) -> MergeSettings {
        MergeSettings {
            merge_commit_allowed: merge,
            squash_merge_allowed: squash,
            rebase_merge_allowed: rebase,
            delete_branch_on_merge: false,
            viewer_default_merge_method: default.into(),
        }
    }

    #[test]
    fn the_method_is_the_flag_then_config_then_the_repositorys() {
        use MergeMethod::*;
        let all = settings(true, true, true, "REBASE");
        assert_eq!(
            choose_method(Some(Squash), Some(Merge), &all).unwrap(),
            Squash
        );
        assert_eq!(choose_method(None, Some(Merge), &all).unwrap(), Merge);
        assert_eq!(choose_method(None, None, &all).unwrap(), Rebase);

        let squash_only = settings(false, true, false, "MERGE");
        // A configured method the repository forbids gives way to its own.
        assert_eq!(
            choose_method(None, Some(Merge), &squash_only).unwrap(),
            Squash
        );
        // Asked for outright, it is refused.
        let e = choose_method(Some(Rebase), None, &squash_only).unwrap_err();
        assert!(e.to_string().contains("does not allow rebase"), "{e}");
        assert!(e.to_string().contains("allows: squash"), "{e}");

        assert!(choose_method(None, None, &settings(false, false, false, "")).is_err());
    }

    #[test]
    fn a_pr_url_names_its_repository() {
        assert_eq!(
            repo_of_url("https://github.com/acme/api/pull/7").as_deref(),
            Some("github.com/acme/api")
        );
        assert_eq!(repo_of_url("7"), None);
        assert_eq!(repo_of_url("#7"), None);
    }
}
