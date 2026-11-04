//! `jeet search`: the documents that answer a question, with their best
//! passages. It reads, and asks no model.

use anyhow::{bail, Result};

use crate::cli::SearchKind;
use crate::context::App;
use crate::manager;
use crate::search::{self, DocHit, Scope, Status};

/// The most documents `--limit` allows.
const MAX_LIMIT: usize = 20;

pub fn run(
    app: &App,
    query: &[String],
    kinds: &[SearchKind],
    limit: usize,
    json: bool,
    status: bool,
    reset: bool,
) -> Result<()> {
    if reset {
        search::reset(app)?;
        println!("Deleted the search index; the next search builds it again.");
        return Ok(());
    }
    let scope = Scope::of_caller(app, kinds)?;
    if status {
        let status = search::status(app, &scope)?;
        match json {
            true => println!("{}", serde_json::to_string_pretty(&status)?),
            false => print!("{}", status_text(&status)),
        }
        return Ok(());
    }
    let query = query.join(" ");
    if query.trim().is_empty() {
        bail!("say what to search for, e.g. `jeet search what did we decide about retries`");
    }
    let hits = search::search(app, &scope, &query, limit.clamp(1, MAX_LIMIT))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("No documents match.");
        if scope.sources.is_empty() {
            println!("There is nothing you may search: no source of documents is open to you.");
        }
        return Ok(());
    }
    print!("{}", hits_text(&hits));
    Ok(())
}

/// The hits as numbered entries, each with its passages.
fn hits_text(hits: &[DocHit]) -> String {
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}  ({:.2})  {}  · {}\n",
            i + 1,
            hit.title,
            hit.score,
            shown_path(&hit.path),
            manager::date(hit.modified)
        ));
        for passage in &hit.passages {
            let heading = if passage.heading.is_empty() {
                "(top)"
            } else {
                passage.heading.as_str()
            };
            out.push_str(&format!(
                "   {heading}   L{}-{}\n",
                passage.start_line, passage.end_line
            ));
            for line in &passage.excerpt {
                out.push_str(&format!("     {line}\n"));
            }
        }
    }
    out
}

/// `path` with `~` for the home directory when it is under it.
fn shown_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = std::path::Path::new(path).strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.to_string()
}

/// What is indexed, one source to a line, then the totals.
fn status_text(status: &Status) -> String {
    let mut out = String::new();
    if status.sources.is_empty() {
        out.push_str("No source of documents is open to you.\n");
    }
    for s in &status.sources {
        out.push_str(&format!(
            "{:<50} {:>5} docs {:>6} passages  {}\n",
            s.source,
            s.docs,
            s.passages,
            s.stamp.as_deref().unwrap_or("-")
        ));
    }
    out.push_str(&format!(
        "{} docs, {} passages; index {} ({} bytes)\n",
        status.docs, status.passages, status.path, status.bytes
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::index::PassageHit;

    #[test]
    fn a_hit_is_shown_with_its_passages() {
        let hit = DocHit {
            path: "/srv/a.md".into(),
            source: "assistant".into(),
            title: "Plan".into(),
            modified: 86_400 * 365,
            score: 1.0,
            passages: vec![
                PassageHit {
                    heading: String::new(),
                    start_line: 1,
                    end_line: 4,
                    excerpt: vec!["one".into()],
                },
                PassageHit {
                    heading: "A › B".into(),
                    start_line: 5,
                    end_line: 9,
                    excerpt: vec!["two".into(), "three".into()],
                },
            ],
        };
        assert_eq!(
            hits_text(&[hit]),
            "1. Plan  (1.00)  /srv/a.md  · 1971-01-01\n   (top)   L1-4\n     one\n   A › B   L5-9\n     two\n     three\n"
        );
    }
}
