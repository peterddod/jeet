//! Syntax highlighting for the review viewer, in GitHub's dark colours.
//!
//! Highlighting a file takes syntect a while — loading its syntaxes alone
//! does — so it never happens while drawing. [`Cache`] works out one file at
//! a time on a background thread, the page on screen first, and the
//! renderer draws a page plain until its highlighting arrives.
//!
//! Only the colour of the text comes from here: the added and removed
//! backgrounds, and the changed-word emphasis, stay the diff's.

use std::ops::Range;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::diff::{FileDiff, LineKind};

pub type Rgb = (u8, u8, u8);

/// How a run of code is drawn. No colour means the text's usual one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tint {
    pub fg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
}

/// A run of a line's text in one tint, by byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub range: Range<usize>,
    pub tint: Tint,
}

/// One file's highlighting: the runs of every line, by hunk and line. Empty
/// for a file that is not highlighted — a language syntect does not know,
/// or a file too big to be worth it — which is drawn plain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileHighlight {
    lines: Vec<Vec<Vec<Run>>>,
}

impl FileHighlight {
    pub fn line(&self, hunk: usize, line: usize) -> Option<&[Run]> {
        Some(self.lines.get(hunk)?.get(line)?.as_slice())
    }
}

/// Past this, a line is likely minified or generated, and highlighting it
/// would take long for nothing anyone reads.
const MAX_LINE: usize = 2_000;
/// Lines in a file's diff past which it is not highlighted.
const MAX_LINES: usize = 20_000;

/// Highlight `file`'s diff. The two sides are separate texts, so each is
/// read as its own stream — context and added lines as the head has them,
/// context and removed lines as the base does — and a context line takes
/// its colours from the head.
pub fn highlight(file: &FileDiff) -> FileHighlight {
    let lines = file.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
    let too_long = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .any(|l| l.text.len() > MAX_LINE);
    if file.binary || lines == 0 || lines > MAX_LINES || too_long {
        return FileHighlight::default();
    }
    let syntaxes = syntaxes();
    let Some(syntax) = syntax_for(syntaxes, file) else {
        return FileHighlight::default();
    };
    let theme = theme();
    let mut head = HighlightLines::new(syntax, theme);
    let mut base = HighlightLines::new(syntax, theme);
    let mut out = Vec::with_capacity(file.hunks.len());
    for hunk in &file.hunks {
        let mut runs = Vec::with_capacity(hunk.lines.len());
        for line in &hunk.lines {
            let found = match line.kind {
                LineKind::Added => runs_of(&mut head, syntaxes, &line.text),
                LineKind::Removed => runs_of(&mut base, syntaxes, &line.text),
                LineKind::Context => {
                    // Read by the base too, to keep its state in step.
                    let _ = runs_of(&mut base, syntaxes, &line.text);
                    runs_of(&mut head, syntaxes, &line.text)
                }
            };
            // syntect gave up on the file: better plain than half-done.
            let Some(found) = found else {
                return FileHighlight::default();
            };
            runs.push(found);
        }
        out.push(runs);
    }
    FileHighlight { lines: out }
}

/// One line's runs, with neighbours of the same tint joined up.
fn runs_of(lines: &mut HighlightLines, syntaxes: &SyntaxSet, text: &str) -> Option<Vec<Run>> {
    // The syntaxes expect each line to end in a newline, as in a file.
    let with_newline = format!("{text}\n");
    let pieces = lines.highlight_line(&with_newline, syntaxes).ok()?;
    let default = theme().settings.foreground;
    let mut runs: Vec<Run> = Vec::new();
    let mut at = 0;
    for (style, piece) in pieces {
        let start = at;
        at += piece.len();
        let end = at.min(text.len());
        if start >= end {
            continue;
        }
        let fg = (Some(style.foreground) != default).then_some(rgb(style.foreground));
        let tint = Tint {
            fg,
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
        };
        match runs.last_mut() {
            Some(last) if last.tint == tint => last.range.end = end,
            _ => runs.push(Run {
                range: start..end,
                tint,
            }),
        }
    }
    Some(runs)
}

fn rgb(color: SynColor) -> Rgb {
    (color.r, color.g, color.b)
}

/// The pieces a line is drawn in: wherever a syntax run or the changed-word
/// emphasis starts or stops, a new piece begins, with its tint and whether
/// it is emphasised. Runs that do not fit the text — cut mid-character, or
/// past its end — are ignored, and the line comes out plain.
pub fn pieces(
    text: &str,
    runs: &[Run],
    emphasis: Option<&Range<usize>>,
) -> Vec<(Range<usize>, Tint, bool)> {
    let fits = |at: usize| at <= text.len() && text.is_char_boundary(at);
    let runs = match runs
        .iter()
        .all(|r| fits(r.range.start) && fits(r.range.end))
    {
        true => runs,
        false => &[],
    };
    let emphasis = emphasis.filter(|e| fits(e.start) && fits(e.end));
    let mut cuts = vec![0, text.len()];
    for run in runs {
        cuts.extend([run.range.start, run.range.end]);
    }
    if let Some(e) = emphasis {
        cuts.extend([e.start, e.end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter(|w| w[0] < w[1])
        .map(|w| {
            let (start, end) = (w[0], w[1]);
            let tint = runs
                .iter()
                .find(|r| r.range.start <= start && start < r.range.end)
                .map(|r| r.tint)
                .unwrap_or_default();
            let emphasised = emphasis.is_some_and(|e| e.start <= start && start < e.end);
            (start..end, tint, emphasised)
        })
        .collect()
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Languages syntect's defaults lack, read as the nearest one they have.
const ALIASES: [(&str, &str); 7] = [
    ("ts", "js"),
    ("tsx", "js"),
    ("mts", "js"),
    ("cts", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
];

/// The language of `file`: by its whole name (`Makefile`), by extension, or
/// by a first line such as `#!/bin/sh` when the diff starts at the top.
fn syntax_for<'a>(syntaxes: &'a SyntaxSet, file: &FileDiff) -> Option<&'a SyntaxReference> {
    let path = file.path();
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    let by_name = syntaxes.find_syntax_by_extension(name);
    let by_extension = extension.and_then(|ext| {
        let ext = ALIASES
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(ext))
            .map(|(_, to)| *to)
            .unwrap_or(ext);
        syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| syntaxes.find_syntax_by_extension(&ext.to_ascii_lowercase()))
    });
    let by_first_line = || {
        let first = file.hunks.first()?.lines.first()?;
        let at_top = first.new == Some(1) || (first.new.is_none() && first.old == Some(1));
        at_top
            .then(|| syntaxes.find_syntax_by_first_line(&first.text))
            .flatten()
    };
    let plain = syntaxes.find_syntax_plain_text().name.as_str();
    by_name
        .or(by_extension)
        .or_else(by_first_line)
        .filter(|s| s.name != plain)
}

/// GitHub's dark syntax colours, after Primer's: what each kind of token is
/// drawn in.
const GITHUB_DARK: [(&str, Rgb, Option<FontStyle>); 30] = [
    (
        "comment, punctuation.definition.comment, string.comment",
        (0x8b, 0x94, 0x9e),
        None,
    ),
    (
        "constant.other.placeholder, constant.character",
        (0xff, 0x7b, 0x72),
        None,
    ),
    (
        "constant, entity.name.constant, variable.other.constant, \
         variable.other.enummember, variable.language, entity",
        (0x79, 0xc0, 0xff),
        None,
    ),
    (
        "entity.name, meta.export.default, meta.definition.variable",
        (0xff, 0xa6, 0x57),
        None,
    ),
    (
        "variable.parameter.function, meta.jsx.children, meta.block, \
         meta.tag.attributes, meta.object.member, meta.embedded.expression",
        FG,
        None,
    ),
    ("entity.name.function", (0xd2, 0xa8, 0xff), None),
    (
        "entity.name.tag, support.class.component",
        (0x7e, 0xe7, 0x87),
        None,
    ),
    ("keyword", (0xff, 0x7b, 0x72), None),
    ("storage, storage.type", (0xff, 0x7b, 0x72), None),
    (
        "storage.modifier.package, storage.modifier.import, storage.type.java",
        FG,
        None,
    ),
    (
        "string, string punctuation.section.embedded source",
        (0xa5, 0xd6, 0xff),
        None,
    ),
    ("support", (0x79, 0xc0, 0xff), None),
    ("meta.property-name", (0x79, 0xc0, 0xff), None),
    ("variable", (0xff, 0xa6, 0x57), None),
    ("variable.other", FG, None),
    (
        "invalid.broken, invalid.deprecated, invalid.illegal, invalid.unimplemented",
        (0xff, 0xa1, 0x98),
        Some(FontStyle::ITALIC),
    ),
    ("string source", FG, None),
    ("string variable", (0x79, 0xc0, 0xff), None),
    ("source.regexp, string.regexp", (0xa5, 0xd6, 0xff), None),
    (
        "string.regexp constant.character.escape",
        (0x7e, 0xe7, 0x87),
        Some(FontStyle::BOLD),
    ),
    (
        "support.constant, support.variable",
        (0x79, 0xc0, 0xff),
        None,
    ),
    ("meta.module-reference", (0x79, 0xc0, 0xff), None),
    (
        "punctuation.definition.list.begin.markdown",
        (0xff, 0xa6, 0x57),
        None,
    ),
    (
        "markup.heading, markup.heading entity.name",
        (0x79, 0xc0, 0xff),
        Some(FontStyle::BOLD),
    ),
    ("markup.quote", (0x7e, 0xe7, 0x87), None),
    ("markup.italic", FG, Some(FontStyle::ITALIC)),
    ("markup.bold", FG, Some(FontStyle::BOLD)),
    ("markup.raw", (0x79, 0xc0, 0xff), None),
    (
        "markup.inserted, meta.diff.header.to-file, punctuation.definition.inserted",
        (0xaf, 0xf5, 0xb4),
        None,
    ),
    (
        "markup.deleted, meta.diff.header.from-file, punctuation.definition.deleted",
        (0xff, 0xdc, 0xd7),
        None,
    ),
];

/// The text's usual colour, which a token with nothing to say keeps.
const FG: Rgb = (0xe6, 0xed, 0xf3);

/// [`GITHUB_DARK`] as a syntect theme.
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let color = |(r, g, b): Rgb| SynColor { r, g, b, a: 0xff };
        let scopes = GITHUB_DARK
            .iter()
            .filter_map(|(selectors, fg, font)| {
                Some(ThemeItem {
                    scope: ScopeSelectors::from_str(selectors).ok()?,
                    style: StyleModifier {
                        foreground: Some(color(*fg)),
                        background: None,
                        font_style: *font,
                    },
                })
            })
            .collect();
        Theme {
            name: Some("GitHub Dark".into()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(color(FG)),
                background: Some(color((0x0d, 0x11, 0x17))),
                ..ThemeSettings::default()
            },
            scopes,
        }
    })
}

/// Each file's highlighting for the review on screen, by page, worked out
/// on a background thread one file at a time.
pub struct Cache {
    done: Vec<Option<Arc<FileHighlight>>>,
    /// Pages already handed to a thread, for the current files.
    asked: Vec<bool>,
    /// Bumped whenever the files are replaced, so highlighting of the old
    /// ones is recognised and dropped.
    version: u64,
    /// A thread is at work.
    pub working: bool,
    tx: Sender<(u64, usize, FileHighlight)>,
    rx: Receiver<(u64, usize, FileHighlight)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            done: Vec::new(),
            asked: Vec::new(),
            version: 0,
            working: false,
            tx,
            rx,
        }
    }

    pub fn get(&self, page: usize) -> Option<&FileHighlight> {
        self.done.get(page)?.as_deref()
    }

    /// The files have been fetched afresh: forget what was worked out for
    /// the old ones, except where a file is just as it was, so a refresh
    /// does not flash the page back to plain.
    pub fn renew(&mut self, old: &[FileDiff], new: &[FileDiff]) {
        self.version += 1;
        self.done = new
            .iter()
            .map(|file| {
                let at = old.iter().position(|o| o == file)?;
                self.done.get(at)?.clone()
            })
            .collect();
        self.asked = self.done.iter().map(Option::is_some).collect();
    }

    /// Store what the threads have finished. True if a page got colours.
    pub fn take(&mut self) -> bool {
        let mut changed = false;
        while let Ok((version, page, highlight)) = self.rx.try_recv() {
            self.working = false;
            if version != self.version {
                continue;
            }
            if let Some(slot) = self.done.get_mut(page) {
                *slot = Some(Arc::new(highlight));
                changed = true;
            }
        }
        changed
    }

    /// The page to highlight next, if any: the one on screen, then the
    /// ones either side of it, so turning a page usually finds it ready.
    pub fn next(&self, page: usize, files: usize) -> Option<usize> {
        if self.working {
            return None;
        }
        [Some(page), page.checked_add(1), page.checked_sub(1)]
            .into_iter()
            .flatten()
            .filter(|&p| p < files)
            .find(|&p| !self.asked.get(p).copied().unwrap_or(true))
    }

    /// Highlight the next page that wants it, on a background thread.
    pub fn start(&mut self, files: &[FileDiff], page: usize) {
        let Some(next) = self.next(page, files.len()) else {
            return;
        };
        self.asked[next] = true;
        self.working = true;
        let file = files[next].clone();
        let version = self.version;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((version, next, highlight(&file)));
        });
    }

    /// Put `highlight` straight in, as a thread would.
    #[cfg(test)]
    pub fn insert(&mut self, page: usize, highlight: FileHighlight) {
        if let Some(slot) = self.done.get_mut(page) {
            *slot = Some(Arc::new(highlight));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::review::diff::parse;

    fn fixture() -> Vec<FileDiff> {
        parse(include_str!("../../../tests/fixtures/pr.diff"))
    }

    /// The text of `line` in each colour, as (text, colour) pairs.
    fn colours<'a>(text: &'a str, runs: &[Run]) -> Vec<(&'a str, Option<Rgb>)> {
        runs.iter()
            .map(|r| (&text[r.range.clone()], r.tint.fg))
            .collect()
    }

    const KEYWORD: Rgb = (0xff, 0x7b, 0x72);
    const TYPE: Rgb = (0xff, 0xa6, 0x57);

    #[test]
    fn colours_rust_as_github_does() {
        let files = fixture();
        let highlight = highlight(&files[0]);
        // `    size: u64,` on the head side: a primitive type is red, and
        // the rest keeps the usual colour.
        let line = &files[0].hunks[0].lines[4];
        let runs = highlight.line(0, 4).unwrap();
        let found = colours(&line.text, runs);
        assert_eq!(
            found,
            [("    size: ", None), ("u64", Some(KEYWORD)), (",", None)]
        );
        // The runs cover the line, end to end, in order.
        assert_eq!(runs.first().unwrap().range.start, 0);
        assert_eq!(runs.last().unwrap().range.end, line.text.len());
        assert!(runs.windows(2).all(|w| w[0].range.end == w[1].range.start));

        // `impl Store {` — a context line — has its keyword in red.
        let at = files[0].hunks[0]
            .lines
            .iter()
            .position(|l| l.text.starts_with("impl"))
            .unwrap();
        let line = &files[0].hunks[0].lines[at];
        let found = colours(&line.text, highlight.line(0, at).unwrap());
        assert!(found.contains(&("impl", Some(KEYWORD))), "{found:?}");
        assert!(found.contains(&("Store", Some(TYPE))), "{found:?}");
    }

    #[test]
    fn removed_lines_are_coloured_as_the_base_has_them() {
        let files = fixture();
        let old = &files[1];
        assert_eq!(old.status, crate::tui::review::diff::FileStatus::Deleted);
        let highlight = highlight(old);
        let line = &old.hunks[0].lines[0];
        assert_eq!(line.text, "fn old() {");
        let found = colours(&line.text, highlight.line(0, 0).unwrap());
        assert!(found.contains(&("fn", Some(KEYWORD))), "{found:?}");
    }

    #[test]
    fn unknown_and_unhighlightable_files_stay_plain() {
        let files = fixture();
        // The binary logo, and the pure rename with no lines.
        assert_eq!(highlight(&files[3]), FileHighlight::default());
        assert_eq!(highlight(&files[4]), FileHighlight::default());
        let unknown = parse("--- a/x.nosuchlang\n+++ b/x.nosuchlang\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(highlight(&unknown[0]), FileHighlight::default());
        // A minified line is not worth the time.
        let long = format!(
            "--- a/x.js\n+++ b/x.js\n@@ -0,0 +1 @@\n+{}\n",
            "var a=1;".repeat(400)
        );
        assert_eq!(highlight(&parse(&long)[0]), FileHighlight::default());
    }

    #[test]
    fn finds_the_language_by_name_alias_or_first_line() {
        let syntaxes = syntaxes();
        let name = |diff: &str| {
            let files = parse(diff);
            syntax_for(syntaxes, &files[0]).map(|s| s.name.clone())
        };
        assert_eq!(
            name("--- a/Makefile\n+++ b/Makefile\n@@ -1 +1 @@\n-a\n+b\n").as_deref(),
            Some("Makefile")
        );
        assert_eq!(
            name("--- a/app.tsx\n+++ b/app.tsx\n@@ -1 +1 @@\n-a\n+b\n").as_deref(),
            Some("JavaScript")
        );
        assert_eq!(
            name("--- a/lib.RS\n+++ b/lib.RS\n@@ -1 +1 @@\n-a\n+b\n").as_deref(),
            Some("Rust")
        );
        // A script with no extension, known by its shebang.
        let script = name("--- /dev/null\n+++ b/run\n@@ -0,0 +1,2 @@\n+#!/bin/bash\n+echo hi\n");
        assert!(
            script.as_deref().is_some_and(|s| s.contains("bash")),
            "{script:?}"
        );
        // But not when the diff does not start at its top.
        assert_eq!(
            name("--- a/run\n+++ b/run\n@@ -5 +5 @@\n-#!/bin/bash\n+#!/bin/bash\n"),
            None
        );
        assert_eq!(
            name("--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-a\n+b\n"),
            None,
            "plain text is no highlighting at all"
        );
    }

    #[test]
    fn pieces_layer_the_emphasis_over_the_syntax() {
        let red = Tint {
            fg: Some(KEYWORD),
            ..Tint::default()
        };
        let runs = [
            Run {
                range: 0..3,
                tint: red,
            },
            Run {
                range: 3..9,
                tint: Tint::default(),
            },
        ];
        // `let x = 1`, with `x = 1` changed.
        let got = pieces("let x = 1", &runs, Some(&(4..9)));
        assert_eq!(
            got,
            [
                (0..3, red, false),
                (3..4, Tint::default(), false),
                (4..9, Tint::default(), true),
            ]
        );
        // No runs and no emphasis: the line in one piece.
        assert_eq!(pieces("abc", &[], None), [(0..3, Tint::default(), false)]);
        assert!(pieces("", &runs, None).is_empty());
    }

    #[test]
    fn runs_that_do_not_fit_the_text_are_ignored() {
        let tint = Tint {
            fg: Some(KEYWORD),
            ..Tint::default()
        };
        // Mid-way through the `é`, and past the end.
        for range in [0..1, 0..40] {
            let runs = [Run {
                range: range.clone(),
                tint,
            }];
            assert_eq!(
                pieces("é!", &runs, None),
                [(0..3, Tint::default(), false)],
                "{range:?}"
            );
        }
    }

    #[test]
    fn the_cache_highlights_the_page_on_screen_first() {
        let files = fixture();
        let mut cache = Cache::new();
        cache.renew(&[], &files);
        assert_eq!(cache.next(2, files.len()), Some(2));
        cache.asked[2] = true;
        assert_eq!(cache.next(2, files.len()), Some(3), "then the next");
        cache.asked[3] = true;
        assert_eq!(cache.next(2, files.len()), Some(1), "then the one before");
        cache.asked[1] = true;
        assert_eq!(cache.next(2, files.len()), None, "no further afield");
        assert_eq!(cache.next(0, files.len()), Some(0));
        assert_eq!(cache.next(4, files.len()), Some(4));
        // And nothing at all past the last file.
        assert_eq!(cache.next(9, files.len()), None);
    }

    #[test]
    fn the_cache_works_on_a_background_thread() {
        let files = fixture();
        let mut cache = Cache::new();
        cache.renew(&[], &files);
        cache.start(&files, 0);
        assert!(cache.working);
        // One at a time.
        assert_eq!(cache.next(0, files.len()), None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !cache.take() {
            assert!(std::time::Instant::now() < deadline, "never highlighted");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!cache.working);
        assert_eq!(cache.get(0), Some(&highlight(&files[0])));
        assert_eq!(cache.get(1), None);
    }

    #[test]
    fn a_refresh_keeps_colours_for_files_that_did_not_change() {
        let files = fixture();
        let mut cache = Cache::new();
        cache.renew(&[], &files);
        cache.insert(0, highlight(&files[0]));
        cache.insert(1, highlight(&files[1]));
        // The first file changed; the second moved up a page.
        let mut new = files[1..].to_vec();
        let mut changed = files[0].clone();
        changed.hunks[0].lines[4].text = "    size: u128,".into();
        new.push(changed);
        cache.renew(&files, &new);
        assert_eq!(cache.get(0), Some(&highlight(&files[1])));
        assert_eq!(cache.get(4), None, "a changed file is worked out again");
        assert_eq!(cache.next(4, new.len()), Some(4));
        assert_eq!(cache.next(0, new.len()), Some(1), "page 0 is still known");
    }

    #[test]
    fn answers_for_old_files_are_dropped() {
        let files = fixture();
        let mut cache = Cache::new();
        cache.renew(&[], &files);
        cache.working = true;
        cache.tx.send((0, 0, highlight(&files[0]))).unwrap();
        assert!(!cache.take());
        assert!(!cache.working, "the thread is free all the same");
        assert_eq!(cache.get(0), None);
    }
}
