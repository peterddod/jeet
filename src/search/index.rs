//! The search index: a SQLite file holding every document's passages in an
//! FTS5 table. It is a cache of files that live elsewhere, so it can be
//! deleted at any time and is brought up to date when a search runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, Transaction, TransactionBehavior};
use serde::Serialize;

use super::scope::Source;
use super::sources::{self, FileEntry};
use super::split;
use crate::context::App;

/// The layout of the tables below. A file with another number is dropped and
/// rebuilt.
const SCHEMA_VERSION: &str = "1";
/// How long to wait for another jeet writing the index.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// How many passages a query ranks before they are grouped by document.
const CANDIDATES: usize = 200;
/// SQLite refuses more bound values than this in one statement (the old
/// limit is 999; one is the MATCH string).
const MAX_SOURCES_PER_QUERY: usize = 900;
/// The RRF constant: how quickly a rank's weight falls off.
const RRF_K: f64 = 60.0;
/// The most passages shown for a document.
const PASSAGES_PER_DOC: usize = 3;
/// The longest excerpt line, in characters.
const MAX_EXCERPT_LINE: usize = 160;
/// The most lines in one excerpt.
const EXCERPT_LINES: usize = 3;

/// Words that say what is asked and not what it is about. They are dropped
/// from a question before it is searched.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be",
    "been", "before", "but", "by", "can", "could", "did", "do", "does", "doing", "done", "for",
    "from", "find", "get", "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its",
    "just", "let", "me", "more", "most", "my", "need", "no", "not", "of", "on", "one", "or",
    "other", "our", "out", "over", "please", "should", "show", "so", "some", "such", "tell",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "to", "up", "us", "use", "want", "was", "we", "were", "what", "when", "where", "which", "who",
    "whom", "why", "will", "with", "would", "yes", "you", "your",
];

/// One document in the results.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocHit {
    /// Where the file is.
    pub path: String,
    /// Which source it is from, such as `assistant` or `repo:github.com/a/b`.
    pub source: String,
    pub title: String,
    /// When the file last changed, in unix seconds.
    pub modified: i64,
    /// How well it matches next to the best hit, which scores 1.
    pub score: f64,
    /// Its best passages, best first.
    pub passages: Vec<PassageHit>,
}

/// One passage of a document in the results.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PassageHit {
    /// The headings it sits under, empty before the first.
    pub heading: String,
    pub start_line: usize,
    pub end_line: usize,
    /// The few lines of it that say most about the question.
    pub excerpt: Vec<String>,
}

/// What the index holds for one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceStatus {
    pub source: String,
    pub docs: usize,
    pub passages: usize,
    /// The mark the source was last indexed at, if it has one.
    pub stamp: Option<String>,
}

/// What the index holds for the sources asked about, and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub sources: Vec<SourceStatus>,
    pub docs: usize,
    pub passages: usize,
    pub path: String,
    /// The index file's size in bytes; 0 if there is none yet.
    pub bytes: u64,
}

/// The index file.
pub fn index_path(app: &App) -> PathBuf {
    app.home.join("search").join("index.db")
}

/// Open the index, making it (and its directory) when missing.
pub fn open(app: &App) -> Result<Connection> {
    let path = index_path(app);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    open_at(&path)
}

/// Open the index file at `path`, with its tables made, or made again when
/// they are of another version.
pub fn open_at(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("open search index {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL lets a search read while another one writes.
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )?;
    let version: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .ok();
    if version.as_deref().is_some_and(|v| v != SCHEMA_VERSION) {
        conn.execute_batch(
            "DROP TABLE IF EXISTS passages_fts;
             DROP TABLE IF EXISTS passages;
             DROP TABLE IF EXISTS docs;
             DROP TABLE IF EXISTS sources;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources(source TEXT PRIMARY KEY, stamp TEXT);
         CREATE TABLE IF NOT EXISTS docs(id INTEGER PRIMARY KEY, source TEXT NOT NULL,
             path TEXT NOT NULL, rel_path TEXT NOT NULL, title TEXT NOT NULL,
             mtime INTEGER NOT NULL, size INTEGER NOT NULL, hash TEXT NOT NULL,
             UNIQUE(source, path));
         CREATE TABLE IF NOT EXISTS passages(id INTEGER PRIMARY KEY,
             doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
             start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
             heading TEXT NOT NULL, body TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS passages_doc ON passages(doc_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS passages_fts
             USING fts5(title, heading, body, tokenize='porter unicode61');",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('schema', ?1)",
        params![SCHEMA_VERSION],
    )?;
    Ok(conn)
}

/// Bring the index up to date for `sources`.
pub fn refresh(conn: &Connection, app: &App, sources: &[Source]) -> Result<()> {
    for source in sources {
        let key = source.key();
        let stamp = sources::stamp(app, source);
        // Same commit as last time: nothing to list.
        if stamp.is_some() && stamp == stored_stamp(conn, &key)? {
            continue;
        }
        let files = sources::list(app, source)?;
        refresh_source(conn, &key, stamp.as_deref(), files)?;
    }
    Ok(())
}

fn stored_stamp(conn: &Connection, key: &str) -> Result<Option<String>> {
    let stamp = conn
        .query_row("SELECT stamp FROM sources WHERE source = ?1", [key], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten();
    Ok(stamp)
}

/// Make the index hold exactly `files` for the source `key`, reading only
/// those that are new or changed. All of it is one transaction, taken with
/// the write lock up front: a second search waits for it, then finds the
/// source up to date.
pub fn refresh_source(
    conn: &Connection,
    key: &str,
    stamp: Option<&str>,
    files: Vec<FileEntry>,
) -> Result<()> {
    // Most searches change nothing: look before taking the write lock.
    if up_to_date(conn, key, stamp, &files)? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut known: HashMap<String, (i64, i64, u64, String)> = HashMap::new();
    {
        let mut stmt =
            tx.prepare("SELECT path, id, mtime, size, hash FROM docs WHERE source = ?1")?;
        let rows = stmt.query_map([key], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64, r.get(4)?),
            ))
        })?;
        for row in rows {
            let (path, value) = row?;
            known.insert(path, value);
        }
    }

    for file in &files {
        let path = file.abs_path.to_string_lossy().to_string();
        let old = known.remove(&path);
        if let Some((_, mtime, size, _)) = &old {
            if *mtime == file.mtime && *size == file.size {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&file.abs_path) else {
            // Gone since it was listed: the next refresh drops it.
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let hash = fnv1a_hex(&bytes);
        match old {
            Some((id, _, _, old_hash)) if old_hash == hash => {
                tx.execute(
                    "UPDATE docs SET mtime = ?1, size = ?2 WHERE id = ?3",
                    params![file.mtime, file.size as i64, id],
                )?;
            }
            Some((id, ..)) => {
                delete_passages(&tx, id)?;
                let title = split::title(&file.rel_path, &text);
                tx.execute(
                    "UPDATE docs SET rel_path = ?1, title = ?2, mtime = ?3, size = ?4, hash = ?5
                     WHERE id = ?6",
                    params![file.rel_path, title, file.mtime, file.size as i64, hash, id],
                )?;
                insert_passages(&tx, id, &title, &text)?;
            }
            None => {
                let title = split::title(&file.rel_path, &text);
                tx.execute(
                    "INSERT INTO docs(source, path, rel_path, title, mtime, size, hash)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        key,
                        path,
                        file.rel_path,
                        title,
                        file.mtime,
                        file.size as i64,
                        hash
                    ],
                )?;
                insert_passages(&tx, tx.last_insert_rowid(), &title, &text)?;
            }
        }
    }

    // What is left of the old rows is no longer there.
    for (id, ..) in known.into_values() {
        delete_passages(&tx, id)?;
        tx.execute("DELETE FROM docs WHERE id = ?1", [id])?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO sources(source, stamp) VALUES(?1, ?2)",
        params![key, stamp],
    )?;
    tx.commit()?;
    Ok(())
}

/// Whether the index already holds `files` for the source `key`, as they
/// are now, at `stamp`.
fn up_to_date(
    conn: &Connection,
    key: &str,
    stamp: Option<&str>,
    files: &[FileEntry],
) -> Result<bool> {
    let stored: Option<Option<String>> = conn
        .query_row("SELECT stamp FROM sources WHERE source = ?1", [key], |r| {
            r.get(0)
        })
        .ok();
    if stored.as_ref().map(Option::as_deref) != Some(stamp) {
        return Ok(false);
    }
    let mut known: HashMap<String, (i64, i64)> = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, mtime, size FROM docs WHERE source = ?1")?;
    let rows = stmt.query_map([key], |r| {
        Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
    })?;
    for row in rows {
        let (path, value) = row?;
        known.insert(path, value);
    }
    Ok(known.len() == files.len()
        && files.iter().all(|f| {
            known.get(f.abs_path.to_string_lossy().as_ref()) == Some(&(f.mtime, f.size as i64))
        }))
}

/// Remove a document's passages, from the FTS table too.
fn delete_passages(conn: &Connection, doc_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM passages_fts WHERE rowid IN (SELECT id FROM passages WHERE doc_id = ?1)",
        [doc_id],
    )?;
    conn.execute("DELETE FROM passages WHERE doc_id = ?1", [doc_id])?;
    Ok(())
}

/// Cut `text` into passages and add them under the document `doc_id`.
fn insert_passages(conn: &Connection, doc_id: i64, title: &str, text: &str) -> Result<()> {
    for p in split::passages(text) {
        conn.execute(
            "INSERT INTO passages(doc_id, start_line, end_line, heading, body)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                doc_id,
                p.start_line as i64,
                p.end_line as i64,
                p.heading,
                p.text
            ],
        )?;
        conn.execute(
            "INSERT INTO passages_fts(rowid, title, heading, body) VALUES(?1, ?2, ?3, ?4)",
            params![conn.last_insert_rowid(), title, p.heading, p.text],
        )?;
    }
    Ok(())
}

/// FNV-1a over `bytes`, 64 bits, as hex. It is written out here because
/// the hash is stored: the standard library's hashers may change between
/// Rust versions.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The words of `query` worth searching for: lower case, letters and digits
/// only, longer than one character, and not a stopword. A query of nothing
/// else searches for all its words.
fn query_tokens(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let all: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 1)
        .collect();
    let mut kept: Vec<&str> = all
        .iter()
        .copied()
        .filter(|t| !STOPWORDS.contains(t))
        .collect();
    if kept.is_empty() {
        kept = all;
    }
    let mut out: Vec<String> = Vec::new();
    for token in kept {
        if !out.iter().any(|t| t == token) {
            out.push(token.to_string());
        }
    }
    out
}

/// The FTS5 MATCH string for `tokens`: each one quoted, any of them enough.
fn match_string(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// A document's score from its passages' scores, best first: the best, half
/// the second and a quarter of the third.
fn doc_score(passage_scores: &[f64]) -> f64 {
    passage_scores
        .iter()
        .zip([1.0, 0.5, 0.25])
        .map(|(s, weight)| s * weight)
        .sum()
}

/// The weight of the passage at 1-based `rank` in a ranked list.
fn rrf(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64)
}

/// The best documents for `query` among the sources named by `keys`, at
/// most `limit`.
pub fn query(conn: &Connection, keys: &[String], query: &str, limit: usize) -> Result<Vec<DocHit>> {
    let tokens = query_tokens(query);
    if tokens.is_empty() || keys.is_empty() {
        return Ok(Vec::new());
    }
    let matching = match_string(&tokens);

    // (passage id, doc id, bm25), best first. bm25 is lower for a better
    // match.
    let mut ranked: Vec<(i64, i64, f64)> = Vec::new();
    for chunk in keys.chunks(MAX_SOURCES_PER_QUERY) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT p.id, p.doc_id, bm25(passages_fts, 4.0, 2.0, 1.0) AS s
             FROM passages_fts
             JOIN passages p ON p.id = passages_fts.rowid
             JOIN docs d ON d.id = p.doc_id
             WHERE passages_fts MATCH ?1 AND d.source IN ({marks})
             ORDER BY s LIMIT {CANDIDATES}"
        ))?;
        let args = std::iter::once(&matching).chain(chunk.iter());
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        for row in rows {
            ranked.push(row?);
        }
    }
    ranked.sort_by(|a, b| a.2.total_cmp(&b.2));
    ranked.truncate(CANDIDATES);

    // Each document's passages, with their scores, in rank order.
    let mut by_doc: Vec<(i64, Vec<(i64, f64)>)> = Vec::new();
    for (i, (passage, doc, _)) in ranked.iter().enumerate() {
        let score = rrf(i + 1);
        match by_doc.iter_mut().find(|(d, _)| d == doc) {
            Some((_, list)) => list.push((*passage, score)),
            None => by_doc.push((*doc, vec![(*passage, score)])),
        }
    }

    let mut docs: Vec<(f64, DocHit, Vec<i64>)> = Vec::new();
    // Archives copy task folders whole: show each text once, at its best.
    let mut seen_hashes: Vec<String> = Vec::new();
    for (doc_id, list) in by_doc {
        let scores: Vec<f64> = list.iter().map(|(_, s)| *s).collect();
        let (hit, hash) = conn.query_row(
            "SELECT path, source, title, mtime, hash FROM docs WHERE id = ?1",
            [doc_id],
            |r| {
                let hit = DocHit {
                    path: r.get(0)?,
                    source: r.get(1)?,
                    title: r.get(2)?,
                    modified: r.get(3)?,
                    score: 0.0,
                    passages: Vec::new(),
                };
                Ok((hit, r.get::<_, String>(4)?))
            },
        )?;
        if seen_hashes.contains(&hash) {
            continue;
        }
        seen_hashes.push(hash);
        let ids = list
            .iter()
            .take(PASSAGES_PER_DOC)
            .map(|(p, _)| *p)
            .collect();
        docs.push((doc_score(&scores), hit, ids));
    }
    docs.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.modified.cmp(&a.1.modified))
            .then(a.1.path.cmp(&b.1.path))
    });
    docs.truncate(limit);

    let top = docs.first().map_or(1.0, |d| d.0);
    let mut out = Vec::new();
    for (score, mut hit, passage_ids) in docs {
        hit.score = (score / top * 100.0).round() / 100.0;
        for id in passage_ids {
            let (heading, start, end, body): (String, i64, i64, String) = conn.query_row(
                "SELECT heading, start_line, end_line, body FROM passages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            hit.passages.push(PassageHit {
                heading: below_title(&heading, &hit.title),
                start_line: start as usize,
                end_line: end as usize,
                excerpt: excerpt(&body, &tokens),
            });
        }
        out.push(hit);
    }
    Ok(out)
}

/// `heading` without the document's title in front: the title is shown
/// beside it already.
fn below_title(heading: &str, title: &str) -> String {
    if heading == title {
        return String::new();
    }
    match heading
        .strip_prefix(title)
        .and_then(|rest| rest.strip_prefix(" › "))
    {
        Some(rest) => rest.to_string(),
        None => heading.to_string(),
    }
}

/// The lines of `body` to show: the (up to) three consecutive non-blank
/// lines that hold the most of `tokens`, or the first lines when none does.
/// Each is trimmed and cut to 160 characters.
fn excerpt(body: &str, tokens: &[String]) -> Vec<String> {
    let mut lines: Vec<&str> = body.lines().collect();
    // The section's own heading line says nothing the heading does not.
    if lines
        .first()
        .is_some_and(|l| l.trim_start().starts_with('#'))
        && lines.len() > 1
    {
        lines.remove(0);
    }
    // Runs of non-blank lines.
    let mut blocks: Vec<Vec<&str>> = vec![Vec::new()];
    for line in lines {
        if line.trim().is_empty() {
            if blocks.last().is_some_and(|b| !b.is_empty()) {
                blocks.push(Vec::new());
            }
        } else if let Some(block) = blocks.last_mut() {
            block.push(line.trim());
        }
    }
    let hits = |line: &str| {
        let lower = line.to_lowercase();
        tokens.iter().filter(|t| lower.contains(t.as_str())).count()
    };
    let mut best: Option<(usize, &[&str])> = None;
    for block in blocks.iter().filter(|b| !b.is_empty()) {
        for start in 0..block.len() {
            let window = &block[start..(start + EXCERPT_LINES).min(block.len())];
            let score: usize = window.iter().map(|l| hits(l)).sum();
            if score > 0 && best.is_none_or(|(s, _)| score > s) {
                best = Some((score, window));
            }
        }
    }
    let chosen: &[&str] = match best {
        Some((_, window)) => window,
        None => blocks
            .iter()
            .find(|b| !b.is_empty())
            .map_or(&[][..], |b| &b[..b.len().min(EXCERPT_LINES)]),
    };
    chosen.iter().map(|l| cap(l)).collect()
}

/// `line` cut to 160 characters, ending in `…` when it was cut.
fn cap(line: &str) -> String {
    if line.chars().count() <= MAX_EXCERPT_LINE {
        return line.to_string();
    }
    let mut out: String = line.chars().take(MAX_EXCERPT_LINE - 1).collect();
    out.push('…');
    out
}

/// What the index holds for the sources named by `keys`.
pub fn status(app: &App, conn: &Connection, keys: &[String]) -> Result<Status> {
    let mut sources = Vec::new();
    for key in keys {
        let (docs, passages): (i64, i64) = conn.query_row(
            "SELECT COUNT(DISTINCT d.id), COUNT(p.id)
             FROM docs d LEFT JOIN passages p ON p.doc_id = d.id WHERE d.source = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        sources.push(SourceStatus {
            source: key.clone(),
            docs: docs as usize,
            passages: passages as usize,
            stamp: stored_stamp(conn, key)?,
        });
    }
    let path = index_path(app);
    Ok(Status {
        docs: sources.iter().map(|s| s.docs).sum(),
        passages: sources.iter().map(|s| s.passages).sum(),
        sources,
        bytes: std::fs::metadata(&path).map_or(0, |m| m.len()),
        path: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rel` written under `root`, listed as the source's file.
    fn put(root: &Path, rel: &str, text: &str) -> FileEntry {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        entry(root, rel)
    }

    fn entry(root: &Path, rel: &str) -> FileEntry {
        let path = root.join(rel);
        let meta = std::fs::metadata(&path).unwrap();
        FileEntry {
            rel_path: rel.to_string(),
            abs_path: path,
            mtime: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            size: meta.len(),
        }
    }

    fn filler(word: &str, n: usize) -> String {
        vec![word; n].join(" ")
    }

    fn hashes(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT rel_path, hash FROM docs ORDER BY rel_path")
            .unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn tokens_drop_question_words_and_keep_the_rest() {
        assert_eq!(
            query_tokens("What did we decide about MCP passthrough for agents?"),
            ["decide", "mcp", "passthrough", "agents"]
        );
        assert_eq!(
            match_string(&query_tokens("how should a task report it is blocked")),
            "\"task\" OR \"report\" OR \"blocked\""
        );
        // Only stopwords: search them after all. One character: nothing.
        assert_eq!(query_tokens("what is the"), ["what", "is", "the"]);
        assert!(query_tokens("a ? x").is_empty());
        // Repeats count once.
        assert_eq!(query_tokens("index Index INDEX"), ["index"]);
    }

    #[test]
    fn a_documents_score_weighs_its_best_three_passages() {
        let (a, b, c) = (rrf(1), rrf(2), rrf(3));
        assert!((doc_score(&[a, b, c, rrf(4)]) - (a + 0.5 * b + 0.25 * c)).abs() < 1e-12);
        assert!((doc_score(&[a]) - a).abs() < 1e-12);
        assert!(rrf(1) > rrf(2));
    }

    #[test]
    fn the_hash_is_fnv1a_64() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn the_excerpt_is_the_lines_with_the_most_hits() {
        let long = "x".repeat(200);
        let body = format!(
            "## Heading\nfirst line\nsecond line\n\nthe rollout plan\nuses canary {long}\n"
        );
        let tokens = vec!["canary".to_string(), "rollout".to_string()];
        let got = excerpt(&body, &tokens);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "the rollout plan");
        assert!(got[1].ends_with('…'));
        assert_eq!(got[1].chars().count(), MAX_EXCERPT_LINE);
        // Nothing matches: the first lines.
        let got = excerpt(&body, &["zzz".to_string()]);
        assert_eq!(got, ["first line", "second line"]);
    }

    #[test]
    fn search_finds_the_document_and_follows_edits_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let conn = open_at(&dir.path().join("index.db")).unwrap();

        let mut files = vec![
            put(
                &src,
                "deploy.md",
                &format!(
                    "# Deploys\n{}\n## Rollback\nTo roll back a release, redeploy the previous tag. {}\n",
                    filler("shipping", 130),
                    filler("steps", 130)
                ),
            ),
            put(&src, "cooking.md", &format!("# Soup\n{}\n", filler("onion", 150))),
            put(&src, "plan.txt", &filler("quarterly roadmap", 150)),
        ];
        refresh_source(&conn, "assistant", None, files.clone()).unwrap();
        let keys = vec!["assistant".to_string()];

        let hits = query(&conn, &keys, "how do we roll back a release?", 5).unwrap();
        assert_eq!(hits[0].title, "Deploys");
        assert_eq!(hits[0].score, 1.0);
        // The title is shown already, so the heading leaves it out.
        assert_eq!(hits[0].passages[0].heading, "Rollback");
        assert!(hits[0].passages[0].excerpt[0].contains("roll back"));
        assert!(query(&conn, &keys, "zebra", 5).unwrap().is_empty());
        // Another source's documents are out of reach.
        assert!(query(&conn, &["task:x".to_string()], "release", 5)
            .unwrap()
            .is_empty());

        // Edit one file: only its hash moves, and the new words are found.
        let before = hashes(&conn);
        std::fs::write(
            src.join("cooking.md"),
            format!("# Soup\n{}\n", filler("pumpernickel", 150)),
        )
        .unwrap();
        files[1] = entry(&src, "cooking.md");
        files[1].mtime += 5;
        refresh_source(&conn, "assistant", None, files.clone()).unwrap();
        let after = hashes(&conn);
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(b.0, a.0);
            assert_eq!(b.1 == a.1, a.0 != "cooking.md", "{}", a.0);
        }
        assert_eq!(
            query(&conn, &keys, "pumpernickel", 5).unwrap()[0].title,
            "Soup"
        );
        assert!(query(&conn, &keys, "onion", 5).unwrap().is_empty());

        // Touched but not changed: same hash, mtime moves, still found.
        files[2].mtime += 9;
        refresh_source(&conn, "assistant", None, files.clone()).unwrap();
        assert_eq!(hashes(&conn), after);
        assert_eq!(query(&conn, &keys, "roadmap", 5).unwrap()[0].title, "plan");

        // Delete one: gone from the results and the tables.
        files.remove(0);
        refresh_source(&conn, "assistant", Some("abc"), files).unwrap();
        assert!(query(&conn, &keys, "release", 5).unwrap().is_empty());
        let passages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM passages_fts WHERE passages_fts MATCH 'shipping'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(passages, 0);
        assert_eq!(
            stored_stamp(&conn, "assistant").unwrap().as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn a_text_found_in_two_places_is_shown_once() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_at(&dir.path().join("index.db")).unwrap();
        let text = "# Report\nWe chose the canary rollout.\n";
        let task = put(&dir.path().join("task"), "REPORT.md", text);
        let archive = put(&dir.path().join("archive"), "REPORT.md", text);
        refresh_source(&conn, "task:x", None, vec![task]).unwrap();
        refresh_source(&conn, "assistant", None, vec![archive]).unwrap();
        let keys = vec!["assistant".to_string(), "task:x".to_string()];
        assert_eq!(query(&conn, &keys, "canary rollout", 5).unwrap().len(), 1);
    }

    #[test]
    fn a_heading_leaves_out_the_title_in_front() {
        assert_eq!(below_title("Deploys › Rollback", "Deploys"), "Rollback");
        assert_eq!(below_title("Deploys", "Deploys"), "");
        assert_eq!(
            below_title("Other › Rollback", "Deploys"),
            "Other › Rollback"
        );
        assert_eq!(below_title("Deploys2", "Deploys"), "Deploys2");
    }

    #[test]
    fn an_index_of_another_version_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = open_at(&path).unwrap();
        let file = put(dir.path(), "a.md", "# A\nhello world\n");
        refresh_source(&conn, "assistant", None, vec![file]).unwrap();
        conn.execute("UPDATE meta SET value = '0' WHERE key = 'schema'", [])
            .unwrap();
        drop(conn);
        let conn = open_at(&path).unwrap();
        assert!(hashes(&conn).is_empty());
    }
}
