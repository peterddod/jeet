use anyhow::Result;

use crate::context::App;

pub fn run(app: &App, filter: Option<&str>) -> Result<()> {
    let repos = app.db.list_repos(filter)?;
    if repos.is_empty() {
        println!("no repositories found");
        return Ok(());
    }
    println!("{:<45} {:<8} TRUNK", "REPO ID", "MANAGED");
    println!("{}", "-".repeat(100));
    for repo in repos {
        let wt_count = app.db.worktree_count(&repo.id)?;
        let managed = if repo.managed { "yes" } else { "no" };
        println!(
            "{:<45} {:<8} {} ({} worktrees, default: {})",
            repo.id, managed, repo.trunk_path, wt_count, repo.default_branch
        );
    }
    Ok(())
}
