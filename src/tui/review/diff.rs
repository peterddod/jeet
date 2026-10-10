//! A unified diff, as `gh pr diff` prints it, parsed into files, hunks and
//! numbered lines.
//!
//! Everything here is pure: the text comes in, the model goes out, and the
//! renderer draws only what this describes.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// None for a file the PR adds.
    pub old_path: Option<String>,
    /// None for a file the PR deletes.
    pub new_path: Option<String>,
    pub status: FileStatus,
    /// Changed, but with no lines to show.
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn new() -> Self {
        Self {
            old_path: None,
            new_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: Vec::new(),
        }
    }

    /// The path review comments use: the file's name after the PR, or before
    /// it for a file the PR deletes.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// The path for display, with where a renamed file came from.
    pub fn label(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            _ => self.path().to_string(),
        }
    }

    /// Lines added and removed.
    pub fn counts(&self) -> (usize, usize) {
        let lines = self.hunks.iter().flat_map(|h| &h.lines);
        lines.fold((0, 0), |(added, removed), line| match line.kind {
            LineKind::Added => (added + 1, removed),
            LineKind::Removed => (added, removed + 1),
            LineKind::Context => (added, removed),
        })
    }

    /// The widest line number in the file, in digits, for the gutter.
    pub fn number_width(&self) -> usize {
        let widest = self
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .flat_map(|l| [l.old, l.new])
            .flatten()
            .max()
            .unwrap_or(0);
        widest.to_string().len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ context` line, as written.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// The line's number in the base, for context and removed lines.
    pub old: Option<u32>,
    /// The line's number in the head, for context and added lines.
    pub new: Option<u32>,
    /// The line without its `+`/`-`/` ` marker.
    pub text: String,
    /// The bytes of `text` that differ from the line it replaced, when it
    /// replaced one and only part of it changed.
    pub emphasis: Option<Range<usize>>,
}

/// Parse a unified diff. Anything that is not a diff is skipped rather than
/// refused: a viewer showing most of a PR beats one showing an error.
pub fn parse(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut file: Option<FileDiff> = None;
    // Lines still to come in the hunk being read, on each side. A hunk is
    // read by count, not by looking at what each line starts with: a removed
    // line reading `-- x` arrives as `--- x`, which is also a file header.
    let mut old_left = 0u32;
    let mut new_left = 0u32;
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    for line in text.lines() {
        if old_left > 0 || new_left > 0 {
            let current = file.as_mut().and_then(|f| f.hunks.last_mut());
            if let Some(hunk) = current {
                let (kind, body) = match line.as_bytes().first() {
                    Some(b'+') => (Some(LineKind::Added), &line[1..]),
                    Some(b'-') => (Some(LineKind::Removed), &line[1..]),
                    Some(b' ') => (Some(LineKind::Context), &line[1..]),
                    // Some tools strip the space off a blank context line.
                    None => (Some(LineKind::Context), ""),
                    // `\ No newline at end of file`: a note, not a line.
                    Some(b'\\') => continue,
                    _ => (None, line),
                };
                match kind {
                    Some(LineKind::Added) if new_left > 0 => {
                        hunk.lines
                            .push(diff_line(LineKind::Added, None, Some(new_no), body));
                        new_no += 1;
                        new_left -= 1;
                        continue;
                    }
                    Some(LineKind::Removed) if old_left > 0 => {
                        hunk.lines
                            .push(diff_line(LineKind::Removed, Some(old_no), None, body));
                        old_no += 1;
                        old_left -= 1;
                        continue;
                    }
                    Some(LineKind::Context) if old_left > 0 && new_left > 0 => {
                        hunk.lines.push(diff_line(
                            LineKind::Context,
                            Some(old_no),
                            Some(new_no),
                            body,
                        ));
                        old_no += 1;
                        new_no += 1;
                        old_left -= 1;
                        new_left -= 1;
                        continue;
                    }
                    // A line the counts did not promise: the hunk was cut
                    // short, so read this one as a header instead.
                    _ => {
                        old_left = 0;
                        new_left = 0;
                    }
                }
            } else {
                old_left = 0;
                new_left = 0;
            }
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.extend(file.take());
            let mut next = FileDiff::new();
            // Only a fallback: `---`/`+++` and `rename` lines say it better,
            // but a binary or mode-only change has nothing else.
            if let Some((old, new)) = git_header_paths(rest) {
                next.old_path = Some(old);
                next.new_path = Some(new);
            }
            file = Some(next);
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let Some(current) = file.as_mut() else {
                continue;
            };
            let Some((old_start, old_count, new_start, new_count)) = hunk_range(rest) else {
                continue;
            };
            current.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            old_no = old_start;
            new_no = new_start;
            old_left = old_count;
            new_left = new_count;
            continue;
        }
        // A plain `diff -u` has no `diff --git` line: its `---` starts a file.
        if line.starts_with("--- ") && file.as_ref().is_none_or(|f| !f.hunks.is_empty()) {
            files.extend(file.take());
            file = Some(FileDiff::new());
        }
        let Some(current) = file.as_mut() else {
            continue;
        };
        if line.starts_with("new file mode") {
            current.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            current.status = FileStatus::Deleted;
        } else if let Some(path) = line
            .strip_prefix("rename from ")
            .or(line.strip_prefix("copy from "))
        {
            current.old_path = Some(unquote(path));
            current.status = FileStatus::Renamed;
        } else if let Some(path) = line
            .strip_prefix("rename to ")
            .or(line.strip_prefix("copy to "))
        {
            current.new_path = Some(unquote(path));
            current.status = FileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("--- ") {
            current.old_path = header_path(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            current.new_path = header_path(path, "b/");
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            current.binary = true;
        }
    }
    files.extend(file);

    for file in &mut files {
        match (&file.old_path, &file.new_path) {
            (None, Some(_)) => file.status = FileStatus::Added,
            (Some(_), None) => file.status = FileStatus::Deleted,
            _ => {}
        }
        for hunk in &mut file.hunks {
            mark_emphasis(&mut hunk.lines);
        }
    }
    files
}

fn diff_line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
    DiffLine {
        kind,
        old,
        new,
        text: text.to_string(),
        emphasis: None,
    }
}

/// `-a,b +c,d @@ …` into its four numbers; a missing count means one.
fn hunk_range(rest: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = rest.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let range = |text: &str| -> Option<(u32, u32)> {
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

/// The path on a `---`/`+++` line, without its `a/`/`b/`; None for
/// `/dev/null`, which is how a diff says the file is not on that side.
fn header_path(text: &str, prefix: &str) -> Option<String> {
    // git puts a tab after a path with a trailing space in it.
    let text = text.strip_suffix('\t').unwrap_or(text);
    if text == "/dev/null" {
        return None;
    }
    let path = unquote(text);
    Some(
        path.strip_prefix(prefix)
            .map(str::to_string)
            .unwrap_or(path),
    )
}

/// The two paths on a `diff --git a/x b/y` line. Unquoted paths may hold
/// spaces, so this only trusts a line it can split unambiguously: quoted
/// paths, or the same path on both sides.
fn git_header_paths(rest: &str) -> Option<(String, String)> {
    if rest.starts_with('"') {
        let close = closing_quote(rest)?;
        let (old, new) = rest.split_at(close + 1);
        let old = unquote(old);
        let new = unquote(new.trim_start());
        return Some((strip(&old, "a/"), strip(&new, "b/")));
    }
    // `a/<p> b/<p>`: the halves are equal, so the split is in the middle.
    if rest.len() % 2 == 1 {
        let half = rest.len() / 2;
        if rest.is_char_boundary(half) && rest.as_bytes()[half] == b' ' {
            let (old, new) = (&rest[..half], &rest[half + 1..]);
            if old.strip_prefix("a/").is_some() && old.strip_prefix("a/") == new.strip_prefix("b/")
            {
                return Some((strip(old, "a/"), strip(new, "b/")));
            }
        }
    }
    let at = rest.rfind(" b/")?;
    Some((strip(&rest[..at], "a/"), strip(&rest[at + 1..], "b/")))
}

fn strip(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

/// Where the quoted string at the start of `text` ends.
fn closing_quote(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// git's C-style quoting, which it uses for paths with unusual characters:
/// `"caf\303\251.txt"` is `café.txt`. Unquoted text comes back as it is.
fn unquote(text: &str) -> String {
    let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return text.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(0x07),
            Some('b') => bytes.push(0x08),
            Some('f') => bytes.push(0x0c),
            Some('v') => bytes.push(0x0b),
            Some(d @ '0'..='7') => {
                let mut value = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            value = value * 8 + digit;
                            chars.next();
                        }
                        None => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// Pair each run of removed lines with the added run straight after it, line
/// for line, and mark the part of each pair that actually changed.
fn mark_emphasis(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Added {
            i += 1;
        }
        let pairs = (added_start - removed_start).min(i - added_start);
        for k in 0..pairs {
            let (old, new) = (removed_start + k, added_start + k);
            if let Some((old_range, new_range)) = changed_ranges(&lines[old].text, &lines[new].text)
            {
                lines[old].emphasis = Some(old_range);
                lines[new].emphasis = Some(new_range);
            }
        }
    }
}

/// The differing middle of two lines — what is left once the common start
/// and end are taken off, widened to whole words. None when there is nothing
/// worth singling out: the lines are the same, or nothing of them is.
pub fn changed_ranges(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .find(|((_, a), b)| a != b)
        .map(|((i, _), _)| i)
        .unwrap_or(old.len().min(new.len()));
    // Never back into a char boundary that is not one in both strings.
    let prefix = (0..=prefix)
        .rev()
        .find(|&p| old.is_char_boundary(p) && new.is_char_boundary(p))
        .unwrap_or(0);
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .scan(0, |sum, len| {
            *sum += len;
            Some(*sum)
        })
        .take_while(|&sum| sum <= max_suffix)
        .last()
        .unwrap_or(0);

    let start = word_start(old, prefix).min(word_start(new, prefix));
    let old_end = word_end(old, old.len() - suffix);
    let new_end = word_end(new, new.len() - suffix);
    // Widening may have walked one side further than the other into the
    // common text; take the furthest so both cover the same words.
    let tail = (old.len() - old_end).min(new.len() - new_end);
    let (old_range, new_range) = (start..old.len() - tail, start..new.len() - tail);
    if old_range.is_empty() && new_range.is_empty() {
        return None;
    }
    // A wholly different line is already said by its background.
    if old_range.len() == old.len() && new_range.len() == new.len() {
        return None;
    }
    Some((old_range, new_range))
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Back from `at` to the start of the word it splits, if it splits one.
fn word_start(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    if !text[at..].chars().next().is_some_and(is_word) {
        return at;
    }
    while let Some(c) = text[..at].chars().next_back() {
        if !is_word(c) {
            break;
        }
        at -= c.len_utf8();
    }
    at
}

/// On from `at` to the end of the word it splits, if it splits one.
fn word_end(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    if !text[..at].chars().next_back().is_some_and(is_word) {
        return at;
    }
    while let Some(c) = text[at..].chars().next() {
        if !is_word(c) {
            break;
        }
        at += c.len_utf8();
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<FileDiff> {
        parse(include_str!("../../../tests/fixtures/pr.diff"))
    }

    #[test]
    fn splits_a_pr_into_files() {
        let files = fixture();
        let paths: Vec<String> = files.iter().map(FileDiff::label).collect();
        assert_eq!(
            paths,
            [
                "src/lib.rs",
                "src/old.rs",
                "docs/new file.md",
                "assets/logo.png",
                "src/a.rs → src/b.rs",
            ]
        );
        let statuses: Vec<FileStatus> = files.iter().map(|f| f.status).collect();
        assert_eq!(
            statuses,
            [
                FileStatus::Modified,
                FileStatus::Deleted,
                FileStatus::Added,
                FileStatus::Modified,
                FileStatus::Renamed,
            ]
        );
        assert!(files[3].binary && files[3].hunks.is_empty());
        assert!(files[4].hunks.is_empty(), "a pure rename has no lines");
        assert_eq!(
            files[1].path(),
            "src/old.rs",
            "a deleted file keeps its name"
        );
    }

    #[test]
    fn numbers_every_line_on_both_sides() {
        let files = fixture();
        let lib = &files[0];
        assert_eq!(lib.hunks.len(), 2);
        let first = &lib.hunks[0];
        assert!(first.header.starts_with("@@ -8,7 +8,8 @@"));
        let numbered: Vec<(LineKind, Option<u32>, Option<u32>)> =
            first.lines.iter().map(|l| (l.kind, l.old, l.new)).collect();
        use LineKind::*;
        assert_eq!(
            numbered,
            [
                (Context, Some(8), Some(8)),
                (Context, Some(9), Some(9)),
                (Context, Some(10), Some(10)),
                (Removed, Some(11), None),
                (Added, None, Some(11)),
                (Added, None, Some(12)),
                (Context, Some(12), Some(13)),
                (Context, Some(13), Some(14)),
                (Context, Some(14), Some(15)),
            ]
        );
        assert_eq!(first.lines[2].text, "", "a blank context line");
        assert_eq!(lib.counts(), (3, 3));
        assert_eq!(lib.number_width(), 2);
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_stays_a_line() {
        let files = fixture();
        let second = &files[0].hunks[1];
        let texts: Vec<&str> = second.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "    fn drop_all(&mut self) {",
                "        // nothing",
                "-- not a header, a removed line",
                "        self.files = 0;",
            ]
        );
        assert_eq!(files[0].old_path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn the_no_newline_marker_is_not_a_line() {
        let files = fixture();
        let old = &files[1];
        assert_eq!(old.hunks[0].lines.len(), 3);
        assert_eq!(old.counts(), (0, 3));
        assert_eq!(old.new_path, None);
    }

    #[test]
    fn marks_the_words_that_changed() {
        let files = fixture();
        let lines = &files[0].hunks[0].lines;
        let removed = &lines[3];
        let added = &lines[4];
        let emph = |l: &DiffLine| l.emphasis.clone().map(|r| l.text[r].to_string());
        assert_eq!(emph(removed).as_deref(), Some("u32"));
        assert_eq!(emph(added).as_deref(), Some("u64"));
        assert_eq!(lines[5].emphasis, None, "an added line with no partner");
    }

    #[test]
    fn changed_ranges_cover_whole_words() {
        let pick = |a: &str, b: &str| {
            changed_ranges(a, b).map(|(x, y)| (a[x].to_string(), b[y].to_string()))
        };
        assert_eq!(
            pick("let total = count;", "let total = counter;"),
            Some(("count".into(), "counter".into()))
        );
        assert_eq!(
            pick("foo(a, b)", "foo(a, b, c)"),
            Some(("".into(), ", c".into()))
        );
        assert_eq!(pick("same", "same"), None);
        assert_eq!(pick("abc", "xyz"), None, "nothing in common");
        assert_eq!(
            pick("café = 1", "café = 2"),
            Some(("1".into(), "2".into())),
            "multibyte text"
        );
        assert_eq!(pick("é", "è"), None);
    }

    #[test]
    fn reads_quoted_paths() {
        assert_eq!(unquote(r#""caf\303\251.txt""#), "café.txt");
        assert_eq!(unquote(r#""tab\there""#), "tab\there");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(
            git_header_paths(r#""a/caf\303\251" "b/caf\303\251""#),
            Some(("café".into(), "café".into()))
        );
        assert_eq!(
            git_header_paths("a/x y.md b/x y.md"),
            Some(("x y.md".into(), "x y.md".into()))
        );
        assert_eq!(
            git_header_paths("a/old b/new"),
            Some(("old".into(), "new".into()))
        );
    }

    #[test]
    fn reads_a_plain_unified_diff_too() {
        let text =
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-one\n+two\n--- a/y\n+++ b/y\n@@ -0,0 +1 @@\n+new\n";
        let files = parse(text);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "x");
        assert_eq!(files[0].hunks[0].lines[1].new, Some(1));
        assert_eq!(files[1].path(), "y");
    }

    #[test]
    fn garbage_is_skipped_not_fatal() {
        assert!(parse("").is_empty());
        assert!(parse("not a diff at all\n").is_empty());
        // A hunk cut short ends where the next file begins.
        let files = parse("diff --git a/x b/x\n@@ -1,5 +1,5 @@\n-a\ndiff --git a/y b/y\n");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].hunks[0].lines.len(), 1);
    }
}
