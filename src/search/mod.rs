//! `plete search`: find the documents (notes, briefs, reports, plans, READMEs)
//! that answer a question put in plain language. Documents are listed from
//! their sources, indexed into one SQLite file when a search runs, and ranked
//! by BM25 over their passages. Code is not searched.

pub mod index;
mod scope;
mod sources;
mod split;

use anyhow::{bail, Result};

use crate::context::App;
pub use index::{DocHit, Status};
pub use scope::Scope;

/// The documents in `scope` that best answer `query`, best first, at most
/// `limit`. The sources are brought up to date first, so a search always
/// sees the files as they are now.
pub fn search(app: &App, scope: &Scope, query: &str, limit: usize) -> Result<Vec<DocHit>> {
    let conn = index::open(app)?;
    index::refresh(&conn, app, &scope.sources)?;
    index::query(&conn, &scope.keys(), query, limit)
}

/// What the index holds for the sources in `scope`, after bringing them up
/// to date.
pub fn status(app: &App, scope: &Scope) -> Result<Status> {
    let conn = index::open(app)?;
    index::refresh(&conn, app, &scope.sources)?;
    index::status(app, &conn, &scope.keys())
}

/// Delete the index; the next search builds it again. Only the user and the
/// assistant may: it is one file shared by every caller.
pub fn reset(app: &App) -> Result<()> {
    if !scope::is_user_or_assistant(app)? {
        bail!("only the user and the assistant can reset the search index");
    }
    // Emptied rather than deleted: another search may have the file open,
    // and would go on writing to a file nobody can see.
    let conn = index::open(app)?;
    index::clear(&conn)
}
