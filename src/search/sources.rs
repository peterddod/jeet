//! Listing the documents of one source: which files count, and where they
//! are. It reads directories and file metadata only; the index reads the
//! files that are new or changed.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use walkdir::WalkDir;

use super::scope::Source;
use crate::config;
use crate::context::App;
use crate::git;
use crate::projects;
use crate::tasks;

/// File extensions that hold prose.
const EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "txt", "rst", "adoc"];
/// Extension-less files that hold prose, by upper-cased name. LICENSE is
/// not among them: it is the same text in every repository.
const BARE_NAMES: &[&str] = &["README", "CHANGELOG", "CONTRIBUTING"];
/// Directory names never entered, wherever they are.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".claude",
    ".jeet",
    "node_modules",
    "target",
    "vendor",
    "third_party",
    "dist",
    "build",
    "out",
];
/// A repository path with one of these directories in it is test data.
const TEST_DIRS: &[&str] = &["fixtures", "testdata"];
/// Files over this many bytes are not documents.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// How much of a file is looked at for a NUL byte.
const SNIFF_BYTES: usize = 8 * 1024;
/// Bump when the rules above change, so repositories are listed again.
const RULES_VERSION: &str = "1";
/// The most documents one source holds.
const MAX_DOCS: usize = 5_000;

/// One document, as the file system shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The path below the source's root, with `/` between parts.
    pub rel_path: String,
    pub abs_path: PathBuf,
    /// Last change, in unix seconds.
    pub mtime: i64,
    pub size: u64,
}

/// A mark that changes when the source's documents may have: a repository's
/// HEAD commit. `None` for sources that have none, which are always listed.
pub fn stamp(app: &App, source: &Source) -> Option<String> {
    let Source::Repo(id) = source else {
        return None;
    };
    let repo = app.db.get_repo(id).ok()??;
    let trunk = Path::new(&repo.trunk_path);
    if !trunk.is_dir() {
        return None;
    }
    let head = read_head(&trunk.join(".git")).or_else(|| git::rev_parse(trunk, "HEAD"))?;
    Some(format!("{head} {}", rules_stamp(app)))
}

/// What decides which files count, as a mark: a new [`RULES_VERSION`] or
/// another `[search] exclude` lists a repository again although its HEAD
/// has not moved.
fn rules_stamp(app: &App) -> String {
    let mut rules = RULES_VERSION.to_string();
    for path in app.config.search.excluded_paths() {
        rules.push('\n');
        rules.push_str(&path.to_string_lossy());
    }
    super::index::fnv1a_hex(rules.as_bytes())
}

/// The commit HEAD names in the git directory `git_dir`, read from its files
/// rather than by running git, which costs more than the rest of a search:
/// HEAD, then the loose ref it points at, then `packed-refs`. `None` when any
/// of that is not as expected, such as `.git` being a file.
fn read_head(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref: ") else {
        return is_sha(head).then(|| head.to_string());
    };
    if let Ok(loose) = std::fs::read_to_string(git_dir.join(name)) {
        let sha = loose.trim();
        return is_sha(sha).then(|| sha.to_string());
    }
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, refname) = line.split_once(' ')?;
        (refname == name && is_sha(sha)).then(|| sha.to_string())
    })
}

/// Whether `s` is a full hex object id (SHA-1 or SHA-256).
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The documents of `source`, sorted by path. A source whose folder is gone
/// has none.
pub fn list(app: &App, source: &Source) -> Result<Vec<FileEntry>> {
    let mut files = match source {
        Source::Assistant => {
            let root = config::assistant_dir(&app.home);
            let mut files = walk(&root, &root);
            files.retain(|f| f.rel_path != "CLAUDE.md");
            files
        }
        Source::Task(id) => match app.db.get_task(id)? {
            Some(task) => task_files(&tasks::task_dir(app, &task)),
            None => Vec::new(),
        },
        Source::Project(name) => match projects::dir(app, name) {
            Ok(root) => walk(&root, &root),
            Err(_) => Vec::new(),
        },
        Source::Repo(id) => match app.db.get_repo(id)? {
            Some(repo) => repo_files(Path::new(&repo.trunk_path)),
            None => Vec::new(),
        },
    };
    let excluded = app.config.search.excluded_paths();
    files.retain(|f| !excluded.iter().any(|prefix| f.abs_path.starts_with(prefix)));
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    files.truncate(MAX_DOCS);
    Ok(files)
}

/// A task's own documents: the allowed files at the top of its folder,
/// except the AGENTS.md and CLAUDE.md jeet writes, and everything under
/// `notes/` and `docs/`. Its other folders are worktrees and scratch.
fn task_files(root: &Path) -> Vec<FileEntry> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), "AGENTS.md" | "CLAUDE.md") || !is_doc_name(&name) {
                continue;
            }
            files.extend(entry_of(root, &entry.path()));
        }
    }
    for sub in ["notes", "docs"] {
        let dir = root.join(sub);
        // A link could point anywhere, outside what the caller may read.
        if dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            files.extend(walk(root, &dir));
        }
    }
    files
}

/// A repository's documents: the tracked files at its trunk that are prose.
fn repo_files(trunk: &Path) -> Vec<FileEntry> {
    if !trunk.is_dir() {
        return Vec::new();
    }
    let Ok(tracked) = git::tracked_files(trunk) else {
        return Vec::new();
    };
    tracked
        .iter()
        .filter(|rel| repo_path_allowed(rel))
        .filter_map(|rel| entry_of(trunk, &trunk.join(rel)))
        .collect()
}

/// Whether a tracked path is a document: prose by its name, and in no
/// directory that is skipped or holds test data.
fn repo_path_allowed(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let Some((name, dirs)) = parts.split_last() else {
        return false;
    };
    if dirs
        .iter()
        .any(|d| skip_dir(d) || TEST_DIRS.contains(&d.to_lowercase().as_str()))
    {
        return false;
    }
    is_doc_name(name)
}

/// Every allowed file below `dir`, recursively, with paths relative to
/// `root`.
fn walk(root: &Path, dir: &Path) -> Vec<FileEntry> {
    WalkDir::new(dir)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || !e.file_type().is_dir() || !skip_dir(&e.file_name().to_string_lossy())
        })
        .flatten()
        .filter(|e| e.file_type().is_file() && is_doc_name(&e.file_name().to_string_lossy()))
        .filter_map(|e| entry_of(root, e.path()))
        .collect()
}

/// Directory names never entered.
fn skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.ends_with("-backup")
}

/// Whether a file name is prose by its name alone.
fn is_doc_name(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => EXTENSIONS.contains(&ext.to_lowercase().as_str()),
        _ => BARE_NAMES.contains(&name.to_uppercase().as_str()),
    }
}

/// `path` as a document, if it is a plain file that is small enough and
/// text.
fn entry_of(root: &Path, path: &Path) -> Option<FileEntry> {
    // Not followed: a link in a repository could point at a file the
    // caller may not read, which would then be indexed under that repository.
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .ok()?;
    if head.contains(&0) {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let rel = path.strip_prefix(root).ok()?;
    Some(FileEntry {
        rel_path: rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        abs_path: path.to_path_buf(),
        mtime,
        size: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_not_documents() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("task"), dir.path().join("secret"));
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("s.md"), "# Secret\n").unwrap();
        std::fs::write(root.join("own.md"), "# Own\n").unwrap();
        std::os::unix::fs::symlink(outside.join("s.md"), root.join("link.md")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("notes")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("real/docs")).unwrap();
        let names: Vec<String> = task_files(&root).into_iter().map(|f| f.rel_path).collect();
        assert_eq!(names, ["own.md"]);
        assert!(walk(&root.join("real"), &root.join("real/docs")).is_empty());
    }

    #[test]
    fn head_is_read_from_loose_and_packed_refs() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(read_head(git).as_deref(), Some(sha));
        let loose = "89abcdef0123456789abcdef0123456789abcdef";
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("refs/heads/main"), format!("{loose}\n")).unwrap();
        assert_eq!(read_head(git).as_deref(), Some(loose));
        std::fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(read_head(git).as_deref(), Some(sha));
        std::fs::write(git.join("HEAD"), "garbage\n").unwrap();
        assert_eq!(read_head(git), None);
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn rels(files: &[FileEntry]) -> Vec<&str> {
        let mut rels: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        rels.sort();
        rels
    }

    #[test]
    fn names_are_prose_by_extension_or_by_being_a_readme() {
        for yes in [
            "a.md",
            "A.MD",
            "x.txt",
            "n.rst",
            "p.adoc",
            "README",
            "CHANGELOG",
        ] {
            assert!(is_doc_name(yes), "{yes}");
        }
        for no in ["a.rs", "LICENSE", "Makefile", ".md", "x.png"] {
            assert!(!is_doc_name(no), "{no}");
        }
    }

    #[test]
    fn a_task_lists_its_top_level_notes_and_docs_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "REPORT.md", "r");
        write(root, "AGENTS.md", "generated");
        write(root, "CLAUDE.md", "generated");
        write(root, "notes/a.md", "a");
        write(root, "notes/deep/b.md", "b");
        write(root, "docs/c.md", "c");
        write(root, "api/README.md", "a worktree");
        write(root, "scratch/x.md", "scratch");
        write(root, "notes/node_modules/skip.md", "skip");
        let files = task_files(root);
        assert_eq!(
            rels(&files),
            ["REPORT.md", "docs/c.md", "notes/a.md", "notes/deep/b.md"]
        );
    }

    #[test]
    fn binaries_and_big_files_are_not_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "ok.md", "text");
        std::fs::write(root.join("bin.md"), b"abc\0def").unwrap();
        std::fs::write(root.join("big.md"), vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        write(root, "old-backup/x.md", "x");
        assert_eq!(rels(&walk(root, root)), ["ok.md"]);
    }

    #[test]
    fn repo_paths_skip_vendor_and_test_data() {
        for yes in [
            "README.md",
            "docs/a.md",
            "CHANGELOG",
            "readme.markdown",
            "ReadMe",
            "README.rst",
        ] {
            assert!(repo_path_allowed(yes), "{yes}");
        }
        for no in [
            "src/main.rs",
            "node_modules/x/README.md",
            "tests/fixtures/a.md",
            "pkg/testdata/a.txt",
            "LICENSE",
            "docs/build/a.md",
            "README.json",
            "CHANGELOG.lock",
            "READMEfoo",
        ] {
            assert!(!repo_path_allowed(no), "{no}");
        }
    }
}
