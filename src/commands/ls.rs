use std::path::Path;

use anyhow::Result;

use crate::context::App;

pub fn run(app: &App, filter: Option<&str>) -> Result<()> {
    let repos = app.db.list_repos(filter)?;
    if repos.is_empty() {
        println!("no repositories found");
        return Ok(());
    }
    let store = app.store_root();
    let mut outside = 0;
    println!("{:<45} TRUNK", "REPO ID");
    println!("{}", "-".repeat(100));
    for repo in repos {
        let in_tasks = app.db.task_worktrees_of_repo(&repo.id)?.len()
            + app.db.review_worktrees_of_repo(&repo.id)?.len();
        let mut notes = vec![
            format!("default: {}", repo.default_branch),
            format!("{in_tasks} in tasks"),
        ];
        if !Path::new(&repo.trunk_path).starts_with(&store) {
            notes.push("outside the store".to_string());
            outside += 1;
        }
        println!("{:<45} {} ({})", repo.id, repo.trunk_path, notes.join(", "));
    }
    if outside > 0 {
        eprintln!("jeet: `jeet import` moves repositories outside the store into it");
    }
    Ok(())
}
