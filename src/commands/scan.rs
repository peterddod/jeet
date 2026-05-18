use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use walkdir::WalkDir;

use crate::commands::adopt;
use crate::context::App;
use crate::git;

pub fn run(app: &App) -> Result<()> {
    let mut count = 0;
    let mut seen_ids = HashSet::new();
    let worktrees_root = app.worktrees_root();

    for root in &app.config.scan_roots {
        let root = crate::config::expand_path(root);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_name() != ".git" {
                continue;
            }
            let repo_dir = entry
                .path()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.clone());

            if repo_dir.starts_with(&worktrees_root) {
                continue;
            }

            if !git::is_git_repo(&repo_dir) {
                continue;
            }

            let path_str = repo_dir.to_string_lossy();
            if let Err(e) = adopt::run(app, &path_str) {
                eprintln!("scan: skip {}: {e}", repo_dir.display());
                continue;
            }

            if let Ok(remote) = git::origin_url(&repo_dir) {
                if let Ok(id) = crate::remote::parse_remote_url_anyhow(&remote) {
                    if !seen_ids.insert(id.id()) {
                        continue;
                    }
                }
            }
            count += 1;
        }
    }
    println!("scan complete: {count} repositories indexed");
    Ok(())
}
