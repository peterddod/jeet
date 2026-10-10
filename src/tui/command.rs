//! Commands typed into the explorer's filter: `:mv`, `:cp`, `:mkdir`, `:rm`
//! and `:touch`, and nothing else.
//!
//! Never a shell. The line is split with [`crate::config::split_command`],
//! every operand is a path resolved against the directory being listed, and
//! the work is done with `std::fs`. A path that lands outside the worktree
//! root — by `..`, by being absolute, or through a symlink — is refused before
//! anything touches the disk.
//!
//! A git checkout below the root — a task's worktree on its canvas, a
//! submodule, a nested clone — is git's to remove, move or copy, so `rm`,
//! `mv` and `cp` refuse one, and anything holding one. Deleting a worktree's
//! folder by hand would take its uncommitted work with it and leave git a
//! dangling record; moving it would break the link to its trunk.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

/// Everything the filter accepts after a `:`, in the order ⇥ offers them.
pub const VERBS: [&str; 5] = ["cp", "mkdir", "mv", "rm", "touch"];

/// Said in the status line when the filter turns into a command line.
pub const USAGE: &str = "mv a b · cp [-r] a b · mkdir [-p] d · rm [-r] p · touch f";

/// One parsed command. Straight out of [`parse`] its paths are the words as
/// typed; after [`Command::resolve`] they are absolute, normalised and known
/// to sit inside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Move {
        from: PathBuf,
        to: PathBuf,
    },
    Copy {
        from: PathBuf,
        to: PathBuf,
        recursive: bool,
    },
    Mkdir {
        dir: PathBuf,
        parents: bool,
    },
    Remove {
        path: PathBuf,
        recursive: bool,
    },
    Touch {
        file: PathBuf,
    },
}

/// Split a command line (the filter without its `:`) into a [`Command`].
///
/// Flags are checked per verb, and only the ones the explorer documents are
/// accepted: `rm -f` is not "close enough to `rm`", it is a habit carried over
/// from a shell that will not be the one running it. `--` ends the flags, so
/// a file whose name starts with a dash is still reachable.
pub fn parse(line: &str) -> Result<Command> {
    let words = crate::config::split_command(line);
    let Some((verb, rest)) = words.split_first() else {
        bail!("type a command: {USAGE}");
    };
    let allowed: &[char] = match verb.as_str() {
        "cp" | "rm" => &['r', 'R'],
        "mkdir" => &['p'],
        "mv" | "touch" => &[],
        other => bail!("unknown command {other:?} — only {}", VERBS.join(", ")),
    };

    let mut flagged = false;
    let mut operands = Vec::new();
    let mut flags_done = false;
    for word in rest {
        if !flags_done && word == "--" {
            flags_done = true;
            continue;
        }
        if !flags_done && word.len() > 1 && word.starts_with('-') {
            for flag in word.chars().skip(1) {
                if !allowed.contains(&flag) {
                    bail!("{verb}: unknown flag -{flag} — usage: {}", usage(verb));
                }
            }
            flagged = true;
            continue;
        }
        operands.push(PathBuf::from(word));
    }

    let command = match (verb.as_str(), operands.as_mut_slice()) {
        ("mv", [from, to]) => Command::Move {
            from: std::mem::take(from),
            to: std::mem::take(to),
        },
        ("cp", [from, to]) => Command::Copy {
            from: std::mem::take(from),
            to: std::mem::take(to),
            recursive: flagged,
        },
        ("mkdir", [dir]) => Command::Mkdir {
            dir: std::mem::take(dir),
            parents: flagged,
        },
        ("rm", [path]) => Command::Remove {
            path: std::mem::take(path),
            recursive: flagged,
        },
        ("touch", [file]) => Command::Touch {
            file: std::mem::take(file),
        },
        _ => bail!("usage: {}", usage(verb)),
    };
    Ok(command)
}

fn usage(verb: &str) -> &'static str {
    match verb {
        "mv" => "mv <from> <to>",
        "cp" => "cp [-r] <from> <to>",
        "mkdir" => "mkdir [-p] <dir>",
        "rm" => "rm [-r] <path>",
        _ => "touch <file>",
    }
}

impl Command {
    /// Anchor every operand at `cwd` and refuse anything outside `root`.
    ///
    /// Two checks, because either alone has a hole. The lexical one catches
    /// `..` and absolute paths and is what the refusal message describes. The
    /// physical one canonicalises whatever part of the path already exists,
    /// so `link/x` with `link -> /etc` is caught too. A path's last component
    /// is followed only where the operation would follow it: `rm` and the
    /// source of `mv` act on a symlink itself, while `touch`, `cp` and a
    /// destination folder write through it.
    ///
    /// Copying or moving into an existing folder lands inside it, as in a
    /// shell, and that is settled here so the confirmation and the status
    /// line name the path that actually changes. Anything already there is
    /// refused rather than overwritten.
    pub fn resolve(self, cwd: &Path, root: &Path) -> Result<Command> {
        let root = normalize(root);
        let real_root = fs::canonicalize(&root)
            .with_context(|| format!("cannot resolve the worktree root {}", root.display()))?;
        let lexical = |word: &Path| operand(word, cwd, &root);
        let inside = |path: &Path, follow: bool| contained(path, follow, &real_root);

        match self {
            Command::Move { from, to } => {
                let from = lexical(&from)?;
                not_the_root(&from, &root)?;
                inside(&from, false)?;
                exists(&from)?;
                not_a_checkout(&from)?;
                let to = destination(&from, lexical(&to)?, &real_root)?;
                if to.starts_with(&from) {
                    bail!("cannot move a folder into itself");
                }
                Ok(Command::Move { from, to })
            }
            Command::Copy {
                from,
                to,
                recursive,
            } => {
                let from = lexical(&from)?;
                inside(&from, true)?;
                exists(&from)?;
                // `cp` follows the source at the top, so check where it leads.
                not_a_checkout(&physical(&from)?)?;
                if from.is_dir() && !recursive {
                    bail!("{} is a folder — use cp -r", name(&from));
                }
                let to = destination(&from, lexical(&to)?, &real_root)?;
                // Physically, not lexically: `cp -r src link/x` with `link`
                // pointing back into `src` would copy forever.
                let real_from = physical(&from)?;
                if physical(&to)?.starts_with(&real_from) {
                    bail!("cannot copy a folder into itself");
                }
                Ok(Command::Copy {
                    from,
                    to,
                    recursive,
                })
            }
            Command::Mkdir { dir, parents } => {
                let dir = lexical(&dir)?;
                inside(&dir, false)?;
                Ok(Command::Mkdir { dir, parents })
            }
            Command::Remove { path, recursive } => {
                let path = lexical(&path)?;
                not_the_root(&path, &root)?;
                inside(&path, false)?;
                exists(&path)?;
                not_a_checkout(&path)?;
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.is_dir() && !recursive {
                    bail!("{} is a folder — use rm -r", name(&path));
                }
                Ok(Command::Remove { path, recursive })
            }
            Command::Touch { file } => {
                let file = lexical(&file)?;
                inside(&file, true)?;
                Ok(Command::Touch { file })
            }
        }
    }

    /// Do it. Only ever called on a resolved command.
    ///
    /// A folder about to be moved, copied or removed is first searched for a
    /// git checkout further down. [`Command::resolve`] only looks at the
    /// folder itself, since it runs on the draw path and a walk may not be
    /// quick; this runs under the spinner.
    pub fn run(&self) -> Result<()> {
        match self {
            Command::Move { from: path, .. } | Command::Remove { path, .. } => {
                no_checkout_inside(path)?
            }
            Command::Copy { from, .. } => no_checkout_inside(&fs::canonicalize(from)?)?,
            Command::Mkdir { .. } | Command::Touch { .. } => {}
        }
        match self {
            Command::Move { from, to } => fs::rename(from, to)
                .with_context(|| format!("mv {} {}", from.display(), to.display())),
            Command::Copy { from, to, .. } => {
                // The source is followed at the top, as `cp` does; inside a
                // tree, links are copied as links.
                let real = fs::canonicalize(from)?;
                copy_tree(&real, to)
                    .with_context(|| format!("cp {} {}", from.display(), to.display()))
            }
            Command::Mkdir { dir, parents: true } => {
                fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))
            }
            Command::Mkdir {
                dir,
                parents: false,
            } => fs::create_dir(dir).with_context(|| format!("mkdir {}", dir.display())),
            Command::Remove { path, .. } => {
                // `symlink_metadata`, so a link to a folder is unlinked rather
                // than emptied.
                let metadata = fs::symlink_metadata(path)?;
                let removed = if metadata.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
                removed.with_context(|| format!("rm {}", path.display()))
            }
            Command::Touch { file } => {
                touch(file).with_context(|| format!("touch {}", file.display()))
            }
        }
    }

    /// What it did, for the status line, with paths relative to `root`.
    pub fn summary(&self, root: &Path) -> String {
        let rel = |path: &Path| relative(path, root);
        match self {
            Command::Move { from, to } => format!("moved {} → {}", rel(from), rel(to)),
            Command::Copy { from, to, .. } => format!("copied {} → {}", rel(from), rel(to)),
            Command::Mkdir { dir, .. } => format!("created {}/", rel(dir)),
            Command::Remove { path, .. } => format!("removed {}", rel(path)),
            Command::Touch { file } => format!("touched {}", rel(file)),
        }
    }

    /// The path to leave the cursor on afterwards, if the command made one.
    pub fn landed(&self) -> Option<&Path> {
        match self {
            Command::Move { to, .. } | Command::Copy { to, .. } => Some(to),
            Command::Mkdir { dir, .. } => Some(dir),
            Command::Touch { file } => Some(file),
            Command::Remove { .. } => None,
        }
    }
}

/// `path` as shown to the user: relative to the root, or whole when it is not
/// under it.
pub fn relative(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rest) if rest.as_os_str().is_empty() => "/".to_string(),
        Ok(rest) => rest.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// One operand, joined onto `cwd` and normalised, refused when it lands
/// outside `root` or inside `.git` — which is inside the root, but is git's
/// and not ours to edit by hand.
fn operand(word: &Path, cwd: &Path, root: &Path) -> Result<PathBuf> {
    if word.as_os_str().is_empty() {
        bail!("empty path");
    }
    let path = normalize(&cwd.join(word));
    let Ok(rest) = path.strip_prefix(root) else {
        bail!("refused: {} is outside the worktree root", word.display());
    };
    if in_git(rest) {
        bail!("refused: {} is inside .git", word.display());
    }
    Ok(path)
}

/// Whether a path below the root runs through `.git`, in any letter case:
/// macOS and Windows file systems are case-insensitive by default, so
/// `.GIT` opens the same directory.
fn in_git(rest: &Path) -> bool {
    rest.components().any(|c| {
        c.as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b".git")
    })
}

fn not_the_root(path: &Path, root: &Path) -> Result<()> {
    if path == root {
        bail!("refused: that is the worktree root");
    }
    Ok(())
}

/// Where a git checkout goes instead, said with every refusal of one.
const LEAVE_IT_TO_GIT: &str =
    "leave it to git (a task's worktree: w in the tasks list, or jeet worktree rm)";

/// Whether `path` is a folder with a `.git` of its own — a worktree, a clone
/// or a submodule. A symlink is not: removing or moving it leaves its target
/// alone.
fn is_checkout(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(path.join(".git")).is_ok()
}

fn not_a_checkout(path: &Path) -> Result<()> {
    if is_checkout(path) {
        bail!(
            "refused: {} is a git checkout — {LEAVE_IT_TO_GIT}",
            name(path)
        );
    }
    Ok(())
}

/// Refused when the folder `path`, or any folder under it, is a git
/// checkout. Symlinks are not followed: what they point to is not touched.
fn no_checkout_inside(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    let found = walkdir::WalkDir::new(path)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .find(|entry| in_git(Path::new(entry.file_name())));
    let Some(checkout) = found.as_ref().and_then(|entry| entry.path().parent()) else {
        return Ok(());
    };
    let shown = checkout
        .strip_prefix(path.parent().unwrap_or(path))
        .unwrap_or(checkout);
    if checkout == path {
        bail!(
            "refused: {} is a git checkout — {LEAVE_IT_TO_GIT}",
            name(path)
        );
    }
    bail!(
        "refused: {} holds a git checkout, {} — {LEAVE_IT_TO_GIT}",
        name(path),
        shown.display()
    );
}

fn exists(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_err() {
        bail!("{} does not exist", name(path));
    }
    Ok(())
}

/// Whether `path` really sits inside `real_root` once symlinks are resolved:
/// all of it when `follow`, or only the folder it is in when the operation
/// acts on the last component itself.
fn contained(path: &Path, follow: bool, real_root: &Path) -> Result<()> {
    let checked = match (follow, path.parent()) {
        (false, Some(parent)) => parent,
        _ => path,
    };
    let real = physical(checked)?;
    let Ok(rest) = real.strip_prefix(real_root) else {
        bail!("refused: {} leads outside the worktree root", name(path));
    };
    // A symlink to `.git` passes the lexical check; its target does not.
    if in_git(rest) {
        bail!("refused: {} leads into .git", name(path));
    }
    Ok(())
}

/// Where a copy or move to `to` lands: inside it when it is a folder, as in a
/// shell. Refused when that is outside the root, or already taken.
fn destination(from: &Path, to: PathBuf, real_root: &Path) -> Result<PathBuf> {
    contained(&to, true, real_root)?;
    let to = if to.is_dir() {
        match from.file_name() {
            Some(file_name) => to.join(file_name),
            None => bail!("cannot tell what to call {}", from.display()),
        }
    } else {
        to
    };
    // The folder it lands in has been checked; the name itself must be free.
    if fs::symlink_metadata(&to).is_ok() {
        bail!("{} already exists", name(&to));
    }
    contained(&to, false, real_root)?;
    Ok(to)
}

/// The real location of `path`: its deepest part that exists, canonicalised,
/// with whatever does not exist yet put back on the end.
///
/// A dangling symlink on the way fails to canonicalise and so fails the whole
/// thing — there is no telling where writing through it would land.
fn physical(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        let Some(parent) = existing.parent() else {
            break;
        };
        if let Some(file_name) = existing.file_name() {
            missing.push(file_name.to_os_string());
        }
        existing = parent;
    }
    let mut real = fs::canonicalize(existing)
        .with_context(|| format!("cannot resolve {}", existing.display()))?;
    for part in missing.into_iter().rev() {
        real.push(part);
    }
    Ok(real)
}

/// `.` and `..` folded away without consulting the disk, so the check sees
/// the same path the operation will be handed.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn touch(file: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(file).is_ok() {
        return fs::File::open(file)?.set_modified(SystemTime::now());
    }
    // `create_new` refuses to go through a symlink, so nothing can be made
    // anywhere but where the checks said.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .map(drop)
}

/// Copy a file or a whole tree, symlinks below the top copied as symlinks.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return copy_link(from, to);
    }
    if !metadata.is_dir() {
        return fs::copy(from, to).map(drop);
    }
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::copy(from, to).map(drop)
}

/// The word being typed at the end of a command line, for ⇥ and for
/// narrowing the listing while it is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Byte offset in the line where the word starts, quote and all.
    pub start: usize,
    /// The word with its quotes taken out, as `split_command` would give it.
    pub text: String,
    /// Which word it is: 0 is the verb.
    pub index: usize,
}

/// Find the last word, by the same quoting rules as `split_command`. A line
/// that ends in whitespace is starting a fresh, empty word.
pub fn tail(line: &str) -> Tail {
    let mut index = 0;
    let mut start = None;
    let mut text = String::new();
    let mut quote: Option<char> = None;
    for (at, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => text.push(c),
            None if c.is_whitespace() => {
                if start.take().is_some() {
                    index += 1;
                    text.clear();
                }
            }
            None => {
                start.get_or_insert(at);
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else {
                    text.push(c);
                }
            }
        }
    }
    Tail {
        start: start.unwrap_or(line.len()),
        text,
        index,
    }
}

/// What the listing narrows to while a command is typed: the operand under
/// the cursor when it is a bare name in this folder, and nothing otherwise —
/// the verb, a flag and a path into another folder all have nothing here to
/// match against.
pub fn needle(line: &str) -> String {
    let tail = tail(line);
    if tail.index == 0 || tail.text.starts_with('-') || tail.text.contains('/') {
        return String::new();
    }
    tail.text
}

/// ⇥ on the verb: as far as the verbs agree, and a space once it is settled.
pub fn complete_verb(typed: &str) -> Option<String> {
    let matching: Vec<&str> = VERBS
        .iter()
        .copied()
        .filter(|v| v.starts_with(typed))
        .collect();
    match matching.as_slice() {
        [] => None,
        [only] => Some(format!("{only} ")),
        [first, rest @ ..] => {
            let shared = rest.iter().fold(first.len(), |len, verb| {
                first
                    .bytes()
                    .zip(verb.bytes())
                    .take(len)
                    .take_while(|(a, b)| a == b)
                    .count()
            });
            (shared > typed.len()).then(|| first[..shared].to_string())
        }
    }
}

/// A name as it has to be typed for `split_command` to give it back whole.
///
/// There is no escaping, only quoting, so a name holding both kinds of quote
/// cannot be written at all; it is left as it is.
pub fn quoted(word: &str) -> String {
    let needs = word.is_empty()
        || word
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\'');
    if !needs {
        return word.to_string();
    }
    if !word.contains('"') {
        format!("\"{word}\"")
    } else if !word.contains('\'') {
        format!("'{word}'")
    } else {
        word.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parses_the_five_commands() {
        assert_eq!(
            parse("mv a b").unwrap(),
            Command::Move {
                from: p("a"),
                to: p("b")
            }
        );
        assert_eq!(
            parse("cp -r src dst").unwrap(),
            Command::Copy {
                from: p("src"),
                to: p("dst"),
                recursive: true
            }
        );
        assert_eq!(
            parse("cp a b").unwrap(),
            Command::Copy {
                from: p("a"),
                to: p("b"),
                recursive: false
            }
        );
        assert_eq!(
            parse("mkdir -p a/b/c").unwrap(),
            Command::Mkdir {
                dir: p("a/b/c"),
                parents: true
            }
        );
        assert_eq!(
            parse("rm -R old").unwrap(),
            Command::Remove {
                path: p("old"),
                recursive: true
            }
        );
        assert_eq!(
            parse("touch notes.md").unwrap(),
            Command::Touch {
                file: p("notes.md")
            }
        );
    }

    #[test]
    fn quoted_operands_keep_their_spaces() {
        assert_eq!(
            parse("mv \"my file.txt\" 'your file.txt'").unwrap(),
            Command::Move {
                from: p("my file.txt"),
                to: p("your file.txt")
            }
        );
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        assert_eq!(
            parse("rm -- -r").unwrap(),
            Command::Remove {
                path: p("-r"),
                recursive: false
            }
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for line in [
            "",
            "   ",
            "ls",
            "chmod 777 x",
            "rm -f x",
            "rm -rf x",
            "mv -f a b",
            "mkdir -r d",
            "mv a",
            "mv a b c",
            "rm",
            "rm a b",
            "touch",
            "cp -r a",
        ] {
            assert!(parse(line).is_err(), "{line:?} should not parse");
        }
    }

    /// The verb is never handed to a shell, so shell syntax is just more
    /// operands — and too many of them to be a valid command.
    #[test]
    fn shell_syntax_is_not_interpreted() {
        assert!(parse("touch a; rm -r b").is_err());
        assert_eq!(
            parse("touch $(whoami)").unwrap(),
            Command::Touch {
                file: p("$(whoami)")
            }
        );
    }

    struct Tree {
        _dir: TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    /// A worktree root with something in it, beside a folder that is not.
    fn tree() -> Tree {
        let dir = TempDir::new().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("wt");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README.md"), "hi").unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), "s").unwrap();
        Tree {
            _dir: dir,
            root,
            outside,
        }
    }

    fn resolve(line: &str, cwd: &Path, root: &Path) -> Result<Command> {
        parse(line)?.resolve(cwd, root)
    }

    fn refused(line: &str, cwd: &Path, root: &Path) -> String {
        match resolve(line, cwd, root) {
            Ok(command) => panic!("{line:?} resolved to {command:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn operands_resolve_against_the_listed_folder() {
        let t = tree();
        let src = t.root.join("src");
        assert_eq!(
            resolve("mv main.rs ../lib.rs", &src, &t.root).unwrap(),
            Command::Move {
                from: src.join("main.rs"),
                to: t.root.join("lib.rs")
            }
        );
        assert_eq!(
            resolve("touch ./x/../y", &src, &t.root).unwrap(),
            Command::Touch {
                file: src.join("y")
            }
        );
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let t = tree();
        let src = t.root.join("src");
        let secret = t.outside.join("secret");
        let lines = [
            "mv main.rs ../../escaped".to_string(),
            format!("cp {} stolen", secret.display()),
            format!("cp main.rs {}", t.outside.join("copy").display()),
            "mkdir -p ../../made".to_string(),
            "rm -r ../..".to_string(),
            format!("rm {}", secret.display()),
            "touch /etc/jeet-should-never-exist".to_string(),
        ];
        for line in &lines {
            let why = refused(line, &src, &t.root);
            assert!(why.contains("refused"), "{line:?}: {why}");
        }
        assert!(secret.exists());
        assert!(!t.outside.join("copy").exists());
        assert!(!Path::new("/etc/jeet-should-never-exist").exists());
    }

    #[test]
    fn the_root_and_git_are_off_limits() {
        let t = tree();
        assert!(refused("rm -r .", &t.root, &t.root).contains("worktree root"));
        assert!(refused("mv . ../elsewhere", &t.root, &t.root).contains("refused"));
        assert!(refused("rm -r .git", &t.root, &t.root).contains(".git"));
        assert!(refused("touch .git/HEAD", &t.root, &t.root).contains(".git"));
    }

    #[test]
    fn git_is_off_limits_in_any_letter_case() {
        let t = tree();
        let src = t.root.join("src");
        for line in [
            "rm -r .GIT",
            "rm -r .Git/objects",
            "mv .GIT elsewhere",
            "mv README.md .gIt/README.md",
            "cp README.md .Git/config",
            "touch .GIT/HEAD",
            "mkdir -p .GiT/hooks",
        ] {
            let why = refused(line, &t.root, &t.root);
            assert!(why.contains(".git"), "{line:?}: {why}");
        }
        assert!(refused("rm -r ../.GIT", &src, &t.root).contains(".git"));
        assert!(t.root.join(".git").is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_cannot_smuggle_a_path_into_git() {
        let t = tree();
        std::os::unix::fs::symlink(t.root.join(".git"), t.root.join("g")).unwrap();
        for line in [
            "touch g/HEAD",
            "rm -r g/objects",
            "mkdir -p g/hooks",
            "cp README.md g/config",
        ] {
            let why = refused(line, &t.root, &t.root);
            assert!(why.contains(".git"), "{line:?}: {why}");
        }
        // The link itself is ours to remove.
        assert!(resolve("rm g", &t.root, &t.root).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_cannot_smuggle_a_path_out() {
        let t = tree();
        std::os::unix::fs::symlink(&t.outside, t.root.join("door")).unwrap();
        std::os::unix::fs::symlink(t.outside.join("secret"), t.root.join("key")).unwrap();
        std::os::unix::fs::symlink(t.outside.join("nowhere"), t.root.join("dangling")).unwrap();

        for line in [
            "touch door/planted",
            "mkdir -p door/a/b",
            "rm door/secret",
            "mv README.md door",
            "cp README.md door/x",
            "cp key stolen",
            "cp -r door stolen",
            "touch key",
            "touch dangling",
        ] {
            let why = refused(line, &t.root, &t.root);
            assert!(
                why.contains("refused") || why.contains("cannot resolve"),
                "{line:?}: {why}"
            );
        }

        // The link itself is ours, though: removing or renaming it touches
        // nothing on the far side.
        let rm = resolve("rm door", &t.root, &t.root).unwrap();
        rm.run().unwrap();
        assert!(t.outside.join("secret").exists());
        assert!(fs::symlink_metadata(t.root.join("door")).is_err());
    }

    #[test]
    fn folders_need_the_recursive_flag() {
        let t = tree();
        assert!(refused("rm src", &t.root, &t.root).contains("rm -r"));
        assert!(refused("cp src copy", &t.root, &t.root).contains("cp -r"));
    }

    #[test]
    fn nothing_is_overwritten() {
        let t = tree();
        fs::write(t.root.join("src/README.md"), "other").unwrap();
        assert!(refused("mv README.md src/main.rs", &t.root, &t.root).contains("exists"));
        assert!(refused("cp README.md src", &t.root, &t.root).contains("exists"));
        assert!(refused("mv missing x", &t.root, &t.root).contains("does not exist"));
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let t = tree();
        assert!(refused("cp -r src src/inner", &t.root, &t.root).contains("itself"));
        assert!(refused("mv src src/inner", &t.root, &t.root).contains("itself"));
    }

    #[test]
    fn commands_do_what_they_say() {
        let t = tree();
        let run = |line: &str| {
            let command = resolve(line, &t.root, &t.root).unwrap();
            command.run().unwrap();
            command
        };

        let made = run("mkdir -p docs/notes");
        assert!(t.root.join("docs/notes").is_dir());
        assert_eq!(made.summary(&t.root), "created docs/notes/");

        run("touch docs/notes/a.md");
        assert!(t.root.join("docs/notes/a.md").is_file());

        // Into an existing folder, the way a shell does it.
        let moved = run("mv README.md docs");
        assert!(t.root.join("docs/README.md").is_file());
        assert!(!t.root.join("README.md").exists());
        assert_eq!(moved.summary(&t.root), "moved README.md → docs/README.md");
        assert_eq!(
            moved.landed(),
            Some(t.root.join("docs/README.md").as_path())
        );

        run("cp -r docs backup");
        assert!(t.root.join("backup/notes/a.md").is_file());
        assert!(t.root.join("backup/README.md").is_file());

        run("cp src/main.rs main.rs");
        assert_eq!(
            fs::read_to_string(t.root.join("main.rs")).unwrap(),
            "fn main() {}"
        );

        run("rm -r backup");
        assert!(!t.root.join("backup").exists());
        run("rm main.rs");
        assert!(!t.root.join("main.rs").exists());
    }

    /// A task's canvas: no `.git` at the root, a worktree in `api` (its
    /// `.git` a file pointing into the trunk), and a nested clone further
    /// down in `vendor`.
    fn canvas() -> Tree {
        let dir = TempDir::new().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("task");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("api/src")).unwrap();
        fs::write(
            root.join("api/.git"),
            "gitdir: /store/api/.git/worktrees/api\n",
        )
        .unwrap();
        fs::write(root.join("api/src/lib.rs"), "uncommitted").unwrap();
        fs::create_dir_all(root.join("vendor/lib/.git")).unwrap();
        fs::write(root.join("vendor/lib/x.c"), "x").unwrap();
        fs::write(root.join("notes.md"), "n").unwrap();
        fs::create_dir(&outside).unwrap();
        Tree {
            _dir: dir,
            root,
            outside,
        }
    }

    #[test]
    fn a_worktree_on_the_canvas_is_left_to_git() {
        let t = canvas();
        let api = t.root.join("api");
        for (line, cwd) in [
            ("rm -r api", &t.root),
            ("rm api", &t.root),
            ("mv api other", &t.root),
            ("mv api vendor", &t.root),
            ("cp -r api copy", &t.root),
            ("rm -r ./api/src/..", &t.root),
            ("rm -r .", &api),
            ("mv ../../api ../../x", &api.join("src")),
        ] {
            let why = refused(line, cwd, &t.root);
            assert!(why.contains("git checkout"), "{line:?}: {why}");
            assert!(why.contains("jeet worktree rm"), "{line:?}: {why}");
        }
        assert_eq!(
            fs::read_to_string(api.join("src/lib.rs")).unwrap(),
            "uncommitted"
        );

        // A link to it is only a link: removing it leaves the worktree be,
        // but copying follows it, so that is refused too.
        std::os::unix::fs::symlink(&api, t.root.join("link")).unwrap();
        assert!(refused("cp -r link copy", &t.root, &t.root).contains("git checkout"));
        resolve("rm link", &t.root, &t.root).unwrap().run().unwrap();
        assert!(api.join(".git").is_file());

        // Inside the worktree, files are anyone's.
        for line in ["mv api/src/lib.rs api/src/main.rs", "rm -r api/src"] {
            resolve(line, &t.root, &t.root).unwrap().run().unwrap();
        }
        assert!(!api.join("src").exists());
        assert!(api.join(".git").is_file());
    }

    #[test]
    fn a_folder_holding_a_checkout_is_refused_before_anything_is_touched() {
        let t = canvas();
        for line in ["rm -r vendor", "mv vendor elsewhere", "cp -r vendor copy"] {
            // Resolving looks at the folder alone, so it lets these by; the
            // walk under the spinner is what stops them.
            let command = resolve(line, &t.root, &t.root).unwrap();
            let why = command.run().unwrap_err().to_string();
            assert!(
                why.contains("holds a git checkout, vendor/lib"),
                "{line:?}: {why}"
            );
        }
        assert!(t.root.join("vendor/lib/x.c").is_file());
        assert!(!t.root.join("elsewhere").exists());
        assert!(!t.root.join("copy").exists());
        assert!(t.outside.is_dir());

        // Once the checkout itself is gone, the folder is just a folder.
        fs::remove_dir_all(t.root.join("vendor/lib/.git")).unwrap();
        resolve("rm -r vendor", &t.root, &t.root)
            .unwrap()
            .run()
            .unwrap();
        assert!(!t.root.join("vendor").exists());
    }

    #[test]
    fn touch_updates_an_existing_file_without_emptying_it() {
        let t = tree();
        let file = t.root.join("README.md");
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        resolve("touch README.md", &t.root, &t.root)
            .unwrap()
            .run()
            .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hi");
        assert!(fs::metadata(&file).unwrap().modified().unwrap() > old);
    }

    #[test]
    fn the_tail_is_the_word_being_typed() {
        assert_eq!(
            tail("mv sr"),
            Tail {
                start: 3,
                text: "sr".into(),
                index: 1
            }
        );
        assert_eq!(
            tail("mv src "),
            Tail {
                start: 7,
                text: String::new(),
                index: 2
            }
        );
        assert_eq!(
            tail("m"),
            Tail {
                start: 0,
                text: "m".into(),
                index: 0
            }
        );
        assert_eq!(tail("").index, 0);
        // An open quote holds the space inside the word.
        assert_eq!(
            tail("mv \"my fi"),
            Tail {
                start: 3,
                text: "my fi".into(),
                index: 1
            }
        );
    }

    #[test]
    fn the_listing_narrows_to_bare_operands_only() {
        assert_eq!(needle("mv"), "");
        assert_eq!(needle("mv RE"), "RE");
        assert_eq!(needle("rm -r"), "");
        assert_eq!(needle("mv src/ma"), "");
        assert_eq!(needle("mv a "), "");
    }

    #[test]
    fn verbs_complete_as_far_as_they_agree() {
        assert_eq!(complete_verb("m"), None);
        assert_eq!(complete_verb("mk"), Some("mkdir ".into()));
        assert_eq!(complete_verb("t"), Some("touch ".into()));
        assert_eq!(complete_verb("mv"), Some("mv ".into()));
        assert_eq!(complete_verb("x"), None);
        assert_eq!(complete_verb(""), None);
    }

    #[test]
    fn names_are_quoted_only_when_they_must_be() {
        assert_eq!(quoted("src"), "src");
        assert_eq!(quoted("my file"), "\"my file\"");
        assert_eq!(quoted("say \"hi\""), "'say \"hi\"'");
        // And what comes out splits back into the same name.
        for name in ["my file", "it's here", "say \"hi\""] {
            assert_eq!(crate::config::split_command(&quoted(name)), vec![name]);
        }
    }
}
