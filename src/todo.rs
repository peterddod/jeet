//! The assistant's todo list: `$JEET_HOME/assistant/todo.md`, the one place
//! it is kept. The assistant edits it, `jeet todo` scripts it, the tasks
//! list shows it.
//!
//! It is plain markdown. Every line that is a checkbox is an item, in
//! priority order, most important first:
//!
//! ```text
//! - [ ] !p1 Fix the login cookie @fix-login
//! - [x] Write the release notes
//! ```
//!
//! `!p1`, `!p2`… mark an item's priority, and `@slug` links it to a task.
//! Everything else in the file — headings, notes, blank lines — is kept
//! exactly as written, and so is every item jeet does not change.
//!
//! Changes are made under a lock and written to a temporary file that is
//! renamed over the list, so two writers never lose each other's items and
//! a reader never sees half a list.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// The list in the assistant's home.
pub fn path(assistant_dir: &Path) -> PathBuf {
    assistant_dir.join("todo.md")
}

/// One checkbox line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Everything up to and including the `[`: indentation and bullet.
    before: String,
    /// What is in the box: a space, or `x`.
    mark: char,
    /// Everything after the mark, from the `]` on.
    after: String,
}

impl Item {
    fn new(text: &str) -> Self {
        Self {
            before: "- [".to_string(),
            mark: ' ',
            after: format!("] {text}"),
        }
    }

    pub fn done(&self) -> bool {
        self.mark != ' '
    }

    /// The item as written after its box.
    pub fn text(&self) -> &str {
        self.after[1..].trim()
    }

    /// `!pN`, if the item has one.
    pub fn priority(&self) -> Option<u8> {
        self.text().split_whitespace().find_map(priority_of)
    }

    /// The tasks it links to with `@slug`, in order.
    pub fn tasks(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for slug in self.text().split_whitespace().filter_map(task_of) {
            if !out.contains(&slug) {
                out.push(slug);
            }
        }
        out
    }

    /// The text without its priority and task links.
    pub fn title(&self) -> String {
        self.text()
            .split_whitespace()
            .filter(|w| priority_of(w).is_none() && task_of(w).is_none())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn render(&self) -> String {
        format!("{}{}{}", self.before, self.mark, self.after)
    }
}

/// `!p1` → 1.
fn priority_of(word: &str) -> Option<u8> {
    let digits = word
        .strip_prefix("!p")
        .or_else(|| word.strip_prefix("!P"))?;
    match digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        true => None,
        false => digits.parse().ok(),
    }
}

/// `@fix-login` → `fix-login`; punctuation after it, as in `@fix-login,`,
/// is not part of it.
fn task_of(word: &str) -> Option<String> {
    let rest = word.strip_prefix('@')?;
    let slug = rest
        .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    let ok = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(slug)
}

/// `line` as an item, if it is a checkbox: `- [ ] …`, `* [x] …`, indented
/// or not.
fn parse_item(line: &str) -> Option<Item> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let mut chars = body.chars();
    let bullet = chars.next()?;
    if !matches!(bullet, '-' | '*' | '+') || chars.next()? != ' ' || chars.next()? != '[' {
        return None;
    }
    let mark = chars.next()?;
    let mark = match mark {
        ' ' => ' ',
        'x' | 'X' => mark,
        _ => return None,
    };
    let after = chars.as_str();
    if !(after.starts_with("] ") || after == "]") {
        return None;
    }
    Some(Item {
        before: format!("{indent}{bullet} ["),
        mark,
        after: after.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Item(Item),
    Text(String),
}

/// A parsed todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoList {
    lines: Vec<Line>,
    /// Whether the file ended with a line break.
    newline: bool,
}

/// An item as `jeet todo --json` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemView {
    /// Its number in the list, from 1, as `jeet todo done` takes it.
    pub id: usize,
    pub done: bool,
    pub text: String,
    pub title: String,
    pub priority: Option<u8>,
    pub tasks: Vec<String>,
}

pub fn parse(text: &str) -> TodoList {
    let lines = text
        .lines()
        .map(|line| match parse_item(line) {
            Some(item) => Line::Item(item),
            None => Line::Text(line.to_string()),
        })
        .collect();
    TodoList {
        lines,
        newline: text.is_empty() || text.ends_with('\n'),
    }
}

impl TodoList {
    pub fn render(&self) -> String {
        let mut out = self
            .lines
            .iter()
            .map(|line| match line {
                Line::Item(item) => item.render(),
                Line::Text(text) => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        if self.newline && !out.is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn items(&self) -> Vec<&Item> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Item(item) => Some(item),
                Line::Text(_) => None,
            })
            .collect()
    }

    /// Every item, numbered.
    pub fn views(&self) -> Vec<ItemView> {
        self.items()
            .into_iter()
            .enumerate()
            .map(|(i, item)| view(i + 1, item))
            .collect()
    }

    /// The line of item `id` (from 1), or why there is none.
    fn line_of(&self, id: usize) -> Result<usize> {
        let mut n = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if let Line::Item(_) = line {
                n += 1;
                if n == id {
                    return Ok(i);
                }
            }
        }
        match n {
            0 => bail!("there is no item {id}: the list is empty"),
            n => bail!("there is no item {id}: the list has {n}"),
        }
    }

    /// Add an item, in priority order: before the first open item of lower
    /// priority, or after the last item. Returns it.
    pub fn add(&mut self, text: &str) -> ItemView {
        let item = Item::new(text);
        let priority = item.priority();
        let before = priority.and_then(|p| {
            self.lines.iter().position(|line| match line {
                Line::Item(other) => !other.done() && other.priority().is_none_or(|q| q > p),
                Line::Text(_) => false,
            })
        });
        let last_item = self
            .lines
            .iter()
            .rposition(|line| matches!(line, Line::Item(_)));
        let at = match (before, last_item) {
            (Some(i), _) => i,
            (None, Some(i)) => i + 1,
            (None, None) => self.lines.len(),
        };
        self.lines.insert(at, Line::Item(item));
        let id = self.lines[..=at]
            .iter()
            .filter(|line| matches!(line, Line::Item(_)))
            .count();
        let Line::Item(item) = &self.lines[at] else {
            unreachable!("just inserted an item there")
        };
        view(id, item)
    }

    /// Tick items off, or with `done` false put them back.
    pub fn set_done(&mut self, ids: &[usize], done: bool) -> Result<Vec<ItemView>> {
        let at: Vec<usize> = ids
            .iter()
            .map(|&id| self.line_of(id))
            .collect::<Result<_>>()?;
        let mut out = Vec::new();
        for (&id, i) in ids.iter().zip(at) {
            if let Line::Item(item) = &mut self.lines[i] {
                item.mark = match (done, item.done()) {
                    (true, true) => item.mark,
                    (true, false) => 'x',
                    (false, _) => ' ',
                };
                out.push(view(id, item));
            }
        }
        Ok(out)
    }

    /// Remove items, numbered as they are before any goes.
    pub fn remove(&mut self, ids: &[usize]) -> Result<Vec<ItemView>> {
        let mut at: Vec<(usize, usize)> = ids
            .iter()
            .map(|&id| self.line_of(id).map(|i| (i, id)))
            .collect::<Result<_>>()?;
        at.sort_unstable();
        at.dedup();
        let mut out = Vec::new();
        for &(i, id) in at.iter().rev() {
            if let Line::Item(item) = self.lines.remove(i) {
                out.push(view(id, &item));
            }
        }
        out.reverse();
        Ok(out)
    }
}

fn view(id: usize, item: &Item) -> ItemView {
    ItemView {
        id,
        done: item.done(),
        text: item.text().to_string(),
        title: item.title(),
        priority: item.priority(),
        tasks: item.tasks(),
    }
}

/// `text` as an item's text: on one line, with no control characters, and
/// not blank.
pub fn item_text(text: &str) -> Result<String> {
    let flat: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c => c,
        })
        .filter(|c| !c.is_control())
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        bail!("a todo item needs some text");
    }
    Ok(flat)
}

/// Read the list in `dir`, let `change` change it, and write it back if it
/// did — all under the list's lock. A missing list starts from `template`.
pub fn update<T>(
    dir: &Path,
    template: &str,
    change: impl FnOnce(&mut TodoList) -> Result<T>,
) -> Result<T> {
    let _lock = lock(dir)?;
    let file = path(dir);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => template.to_string(),
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let mut list = parse(&text);
    let out = change(&mut list)?;
    let rendered = list.render();
    if rendered != text || !file.exists() {
        write_atomically(&file, &rendered)?;
    }
    Ok(out)
}

/// The list in `dir`, as it is now; `template`'s when there is none yet.
pub fn read(dir: &Path, template: &str) -> Result<TodoList> {
    let file = path(dir);
    match std::fs::read_to_string(&file) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(parse(template)),
        Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
    }
}

/// Held while the list is read, changed and written.
fn lock(dir: &Path) -> Result<std::fs::File> {
    let marker = dir.join(crate::tasks::MARKER_DIR);
    std::fs::create_dir_all(&marker).with_context(|| format!("create {}", marker.display()))?;
    let path = marker.join("todo.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

/// Replace `path` with `text` whole: written beside it, flushed, renamed.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.unwrap_or_default(),
        std::process::id()
    ));
    let written = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_all()
        });
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Todo\n\nSome notes the assistant keeps.\n\n\
                          - [ ] !p1 Fix the login cookie @fix-login\n\
                          - [x] Write the release notes\n  \
                          * [X] nested, done\n\
                          - [ ]\n\
                          - [ ] review @api-7, then @fix-login.\n\
                          - not a checkbox\n\
                          - [~] nor this\n";

    #[test]
    fn a_list_round_trips_exactly() {
        assert_eq!(parse(SAMPLE).render(), SAMPLE);
        let bare = "- [ ] one\n- [x] two";
        assert_eq!(parse(bare).render(), bare);
        assert_eq!(parse("").render(), "");
        let crlf_free = "# Todo\n\n";
        assert_eq!(parse(crlf_free).render(), crlf_free);
    }

    #[test]
    fn items_carry_priority_and_task_links() {
        let list = parse(SAMPLE);
        let views = list.views();
        assert_eq!(views.len(), 5);
        assert_eq!(
            views[0],
            ItemView {
                id: 1,
                done: false,
                text: "!p1 Fix the login cookie @fix-login".into(),
                title: "Fix the login cookie".into(),
                priority: Some(1),
                tasks: vec!["fix-login".into()],
            }
        );
        assert!(views[1].done && views[2].done);
        assert_eq!(views[3].text, "");
        assert_eq!(views[4].tasks, ["api-7", "fix-login"]);
        assert_eq!(views[4].priority, None);
        assert_eq!(priority_of("!p12"), Some(12));
        assert_eq!(priority_of("!p"), None);
        assert_eq!(priority_of("!px"), None);
        assert_eq!(task_of("@Upper"), None);
        assert_eq!(task_of("@"), None);
    }

    #[test]
    fn items_are_added_in_priority_order() {
        let mut list =
            parse("# Todo\n\n- [ ] !p1 first\n- [ ] !p3 third\n- [ ] whenever\n\nnotes\n");
        let added = list.add("!p2 second");
        assert_eq!(added.id, 2);
        let added = list.add("later");
        assert_eq!(added.id, 5);
        let added = list.add("!p1 also first");
        assert_eq!(added.id, 2);
        assert_eq!(
            list.render(),
            "# Todo\n\n- [ ] !p1 first\n- [ ] !p1 also first\n- [ ] !p2 second\n\
             - [ ] !p3 third\n- [ ] whenever\n- [ ] later\n\nnotes\n"
        );
        // Into an empty list, after whatever is there.
        let mut empty = parse("# Todo\n");
        assert_eq!(empty.add("!p1 x").id, 1);
        assert_eq!(empty.render(), "# Todo\n- [ ] !p1 x\n");
        let mut nothing = parse("");
        nothing.add("x");
        assert_eq!(nothing.render(), "- [ ] x\n");
    }

    #[test]
    fn items_are_ticked_off_and_removed_by_number() {
        let mut list = parse(SAMPLE);
        let done = list.set_done(&[1, 2], true).unwrap();
        assert!(done.iter().all(|v| v.done));
        assert!(list.render().contains("- [x] !p1 Fix the login cookie"));
        // Already done stays as it was written.
        assert!(list.render().contains("  * [X] nested, done"));
        list.set_done(&[3], false).unwrap();
        assert!(list.render().contains("  * [ ] nested, done"));

        let removed = list.remove(&[4, 1, 4]).unwrap();
        assert_eq!(
            removed.iter().map(|v| v.id).collect::<Vec<_>>(),
            [1, 4],
            "numbered as before any went"
        );
        assert_eq!(list.items().len(), 3);
        assert!(list.render().contains("Some notes the assistant keeps."));

        let err = list.set_done(&[9], true).unwrap_err().to_string();
        assert!(err.contains("has 3"), "{err}");
        // Nothing changes when one number is wrong.
        let before = list.render();
        assert!(list.remove(&[1, 9]).is_err());
        assert_eq!(list.render(), before);
        assert!(parse("")
            .remove(&[1])
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }

    #[test]
    fn item_text_is_one_clean_line() {
        assert_eq!(
            item_text("  fix\nthe\tthing\u{1b}[2J ").unwrap(),
            "fix the thing[2J"
        );
        assert!(item_text(" \n ").is_err());
    }

    #[test]
    fn updates_write_the_whole_list_and_only_when_it_changed() {
        let dir = tempfile::tempdir().unwrap();
        // Missing: starts from the template, and is written even unchanged.
        let n = update(dir.path(), "# Todo\n", |list| Ok(list.items().len())).unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            std::fs::read_to_string(path(dir.path())).unwrap(),
            "# Todo\n"
        );

        update(dir.path(), "", |list| Ok(list.add("!p1 ship it"))).unwrap();
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert_eq!(text, "# Todo\n- [ ] !p1 ship it\n");
        // A failed change writes nothing.
        assert!(update(dir.path(), "", |list| list.remove(&[5])).is_err());
        assert_eq!(std::fs::read_to_string(path(dir.path())).unwrap(), text);
        // Nothing left beside it.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        assert_eq!(read(dir.path(), "").unwrap().items().len(), 1);
    }

    #[test]
    fn concurrent_updates_keep_every_item() {
        let dir = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    update(&dir, "", |list| Ok(list.add(&format!("item {i}")))).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(read(dir.path(), "").unwrap().items().len(), 8);
    }
}
