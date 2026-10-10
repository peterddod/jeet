//! `jeet sync [--repo R]`: fetch the store now, in the foreground.

use anyhow::{bail, Result};

use crate::context::App;
use crate::resolve;
use crate::store;

pub fn run(app: &App, repo: Option<&str>) -> Result<()> {
    // One repo skips the store lock: it is a single fetch, and git's own ref
    // locks already keep it from tangling with a background sync of the same.
    if let Some(filter) = repo {
        let repo = resolve::resolve_repo_filter(&app.db, filter)?;
        let result = store::sync_repo(app, &repo);
        store::report(&repo, &result);
        return match result {
            Ok(_) => Ok(()),
            Err(_) => bail!("could not sync {}", repo.id),
        };
    }
    let (failed, total) = store::sync_now(app)?;
    if total == 0 {
        eprintln!("jeet: no repositories to sync; `jeet clone <url>` adds one");
    }
    if failed > 0 {
        bail!("{failed} of {total} repositories failed to sync");
    }
    Ok(())
}
