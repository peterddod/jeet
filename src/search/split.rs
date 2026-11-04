//! Cutting a document into passages and naming it. Pure functions of the
//! text, so the index and its tests can use them without touching a disk.

/// A section shorter than this many words is joined to its neighbour: a
/// heading and one line make a poor answer on their own.
const MIN_WORDS: usize = 120;
/// A passage is never longer than this many words, unless one line is.
const MAX_WORDS: usize = 400;
/// The longest title kept.
const MAX_TITLE: usize = 120;

/// One part of a document: what is shown for a hit, and what is searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// The headings above it and its own, `A › B › C`; empty before the
    /// first heading.
    pub heading: String,
    /// The first line, counting from 1.
    pub start_line: usize,
    /// The last line, counting from 1.
    pub end_line: usize,
    /// The lines from `start_line` to `end_line`, trimmed at both ends.
    pub text: String,
}

/// A run of lines under one heading, before merging and splitting.
struct Section<'a> {
    heading: String,
    /// Line number of `lines[0]`, counting from 1.
    start: usize,
    lines: Vec<&'a str>,
}

impl<'a> Section<'a> {
    fn words(&self) -> usize {
        self.lines
            .iter()
            .map(|l| l.split_whitespace().count())
            .sum()
    }

    /// `next` follows this section directly: take its lines in, and keep
    /// the heading of whichever part has more words.
    fn absorb(&mut self, next: Section<'a>) {
        let (mine, theirs) = (self.words(), next.words());
        let Section { heading, lines, .. } = next;
        if theirs > mine {
            self.heading = heading;
        }
        self.lines.extend(lines);
    }
}

/// The document's title: the `title:` of a leading front-matter block, else
/// its first `# ` heading, else the file name without its extension. Trimmed
/// and cut to 120 characters.
pub fn title(rel_path: &str, text: &str) -> String {
    let found = front_matter_title(text)
        .or_else(|| first_h1(text))
        .filter(|t| !t.is_empty());
    let title = match found {
        Some(title) => title,
        None => {
            let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
            match name.rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => stem.to_string(),
                _ => name.to_string(),
            }
        }
    };
    title.trim().chars().take(MAX_TITLE).collect()
}

/// The lines of a leading `---` block, if the text opens with one.
fn front_matter(text: &str) -> Option<Vec<&str>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut block = Vec::new();
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            return Some(block);
        }
        block.push(line);
    }
    None
}

fn front_matter_title(text: &str) -> Option<String> {
    let block = front_matter(text)?;
    let value = block
        .iter()
        .find_map(|line| line.strip_prefix("title:"))?
        .trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    Some(unquoted.trim().to_string())
}

fn first_h1(text: &str) -> Option<String> {
    let skip = front_matter(text).map_or(0, |block| block.len() + 2);
    let mut fence = Fence::default();
    for line in text.lines().skip(skip) {
        if fence.toggles(line) || fence.open() {
            continue;
        }
        if let Some((1, title)) = heading(line) {
            return Some(title);
        }
    }
    None
}

/// Whether the lines being read are inside a fenced code block.
#[derive(Default)]
struct Fence {
    /// The fence character and its length, while one is open.
    open: Option<(char, usize)>,
}

impl Fence {
    /// Feed the next line; true when it opens or closes a fence.
    fn toggles(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let Some(c) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) else {
            return false;
        };
        let run = trimmed.chars().take_while(|&d| d == c).count();
        if run < 3 {
            return false;
        }
        match self.open {
            None => {
                self.open = Some((c, run));
                true
            }
            Some((open_c, open_run)) if open_c == c && run >= open_run => {
                self.open = None;
                true
            }
            Some(_) => false,
        }
    }

    fn open(&self) -> bool {
        self.open.is_some()
    }
}

/// An ATX heading: its level (1 to 6) and text.
fn heading(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let text = after.trim().trim_end_matches('#').trim_end();
    Some((level, text.to_string()))
}

/// Cut `text` into passages: sections at headings of level 1 to 3, small
/// ones joined to a neighbour, large ones split at blank lines.
pub fn passages(text: &str) -> Vec<Passage> {
    let merged = merge(sections(text));
    let mut out = Vec::new();
    for section in merged {
        split(section, &mut out);
    }
    out
}

/// The sections of `text`: one per heading of level 1 to 3 outside code
/// fences, and one for whatever comes before the first.
fn sections(text: &str) -> Vec<Section<'_>> {
    let mut out = vec![Section {
        heading: String::new(),
        start: 1,
        lines: Vec::new(),
    }];
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut fence = Fence::default();
    for (i, line) in text.lines().enumerate() {
        let fence_line = fence.toggles(line);
        if !fence_line && !fence.open() {
            if let Some((level, name)) = heading(line).filter(|(level, _)| *level <= 3) {
                while path.last().is_some_and(|(l, _)| *l >= level) {
                    path.pop();
                }
                path.push((level, name));
                let names: Vec<&str> = path.iter().map(|(_, n)| n.as_str()).collect();
                out.push(Section {
                    heading: names.join(" › "),
                    start: i + 1,
                    lines: Vec::new(),
                });
            }
        }
        if let Some(section) = out.last_mut() {
            section.lines.push(line);
        }
    }
    out
}

/// Join each section under `MIN_WORDS` words to the next, and a short last
/// one to the one before it.
fn merge(sections: Vec<Section<'_>>) -> Vec<Section<'_>> {
    let mut out: Vec<Section> = Vec::new();
    let mut pending: Option<Section> = None;
    for section in sections {
        let current = match pending.take() {
            Some(mut small) => {
                small.absorb(section);
                small
            }
            None => section,
        };
        if current.words() < MIN_WORDS {
            pending = Some(current);
        } else {
            out.push(current);
        }
    }
    if let Some(small) = pending {
        match out.last_mut() {
            Some(last) => last.absorb(small),
            None => out.push(small),
        }
    }
    out
}

/// Cut a section into passages of at most `MAX_WORDS` words and add them to
/// `out`. Paragraphs stay whole when they can; one over the limit is cut by
/// lines.
fn split(section: Section<'_>, out: &mut Vec<Passage>) {
    // The units to pack: each paragraph with the blank lines after it, as
    // (offset of its first line in the section, line count).
    let mut units: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < section.lines.len() {
        let begin = i;
        while i < section.lines.len() && !section.lines[i].trim().is_empty() {
            i += 1;
        }
        while i < section.lines.len() && section.lines[i].trim().is_empty() {
            i += 1;
        }
        let words = |from: usize, to: usize| -> usize {
            section.lines[from..to]
                .iter()
                .map(|l| l.split_whitespace().count())
                .sum()
        };
        if words(begin, i) > MAX_WORDS {
            units.extend((begin..i).map(|n| (n, 1)));
        } else {
            units.push((begin, i - begin));
        }
    }

    let mut piece_start = 0;
    let mut piece_end = 0;
    let mut piece_words = 0;
    for (offset, count) in units {
        let words: usize = section.lines[offset..offset + count]
            .iter()
            .map(|l| l.split_whitespace().count())
            .sum();
        if piece_end > piece_start && piece_words + words > MAX_WORDS {
            push_piece(&section, piece_start, piece_end, out);
            piece_start = offset;
            piece_words = 0;
        }
        piece_end = offset + count;
        piece_words += words;
    }
    push_piece(&section, piece_start, piece_end, out);
}

/// Add lines `from..to` of `section` as a passage, trimmed of blank lines at
/// both ends, unless nothing is left.
fn push_piece(section: &Section<'_>, from: usize, to: usize, out: &mut Vec<Passage>) {
    let lines = &section.lines[from..to];
    let Some(first) = lines.iter().position(|l| !l.trim().is_empty()) else {
        return;
    };
    let last = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(first);
    out.push(Passage {
        heading: section.heading.clone(),
        start_line: section.start + from + first,
        end_line: section.start + from + last,
        text: lines[first..=last].join("\n"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` words on one line, each `w`.
    fn words(n: usize) -> String {
        vec!["w"; n].join(" ")
    }

    #[test]
    fn headings_in_code_fences_are_not_headings() {
        let text = format!(
            "# Top\n{}\n```\n# not a heading\n```\n~~~sh\n## nor this\n~~~\n{}\n",
            words(130),
            words(5)
        );
        let found = passages(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, "Top");
        assert!(found[0].text.contains("# not a heading"));
    }

    #[test]
    fn the_heading_path_follows_levels_one_to_three() {
        let text = format!(
            "# A\n{}\n## B\n{}\n### C\n{}\n#### D stays inside C\n{}\n## E\n{}\n",
            words(130),
            words(130),
            words(130),
            words(130),
            words(130)
        );
        let found = passages(&text);
        let headings: Vec<&str> = found.iter().map(|p| p.heading.as_str()).collect();
        assert_eq!(headings, ["A", "A › B", "A › B › C", "A › E"]);
        assert!(found[2].text.contains("#### D stays inside C"));
    }

    #[test]
    fn text_before_the_first_heading_has_no_heading() {
        let text = format!("{}\n\n# A\n{}\n", words(130), words(130));
        let found = passages(&text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].heading, "");
        assert_eq!((found[0].start_line, found[0].end_line), (1, 1));
        assert_eq!(found[1].heading, "A");
        assert_eq!((found[1].start_line, found[1].end_line), (3, 4));
    }

    #[test]
    fn a_short_section_joins_the_next_and_keeps_the_bigger_heading() {
        let text = format!("# Title\n\nShort intro.\n\n## Body\n{}\n", words(150));
        let found = passages(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, "Title › Body");
        assert_eq!((found[0].start_line, found[0].end_line), (1, 6));
        assert!(found[0].text.starts_with("# Title"));
    }

    #[test]
    fn a_short_last_section_joins_the_one_before() {
        let text = format!("# A\n{}\n## Tail\nbye\n", words(150));
        let found = passages(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, "A");
        assert_eq!((found[0].start_line, found[0].end_line), (1, 4));
    }

    #[test]
    fn a_long_section_is_cut_at_blank_lines() {
        let para = words(150);
        let text = format!("# A\n{para}\n\n{para}\n\n{para}\n\n{para}\n");
        let found = passages(&text);
        assert!(found.len() >= 2, "{found:?}");
        for p in &found {
            assert_eq!(p.heading, "A");
            assert!(p.text.split_whitespace().count() <= MAX_WORDS + 1);
        }
        // Nothing lost, and no line shared.
        assert_eq!(found[0].start_line, 1);
        assert_eq!(found.last().unwrap().end_line, 8);
        for pair in found.windows(2) {
            assert!(pair[0].end_line < pair[1].start_line);
        }
    }

    #[test]
    fn one_huge_paragraph_is_cut_by_lines() {
        let line = words(100);
        let text = [line.as_str(); 10].join("\n");
        let found = passages(&text);
        assert_eq!(found.len(), 3);
        assert_eq!(
            found
                .iter()
                .map(|p| (p.start_line, p.end_line))
                .collect::<Vec<_>>(),
            [(1, 4), (5, 8), (9, 10)]
        );
    }

    #[test]
    fn plain_text_is_cut_by_paragraphs() {
        let para = words(250);
        let text = format!("{para}\n\n{para}\n");
        let found = passages(&text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].heading, "");
        assert_eq!((found[1].start_line, found[1].end_line), (3, 3));
    }

    #[test]
    fn empty_text_has_no_passages() {
        assert!(passages("").is_empty());
        assert!(passages("\n\n  \n").is_empty());
    }

    #[test]
    fn title_prefers_front_matter_then_h1_then_the_file_name() {
        assert_eq!(
            title(
                "a/b.md",
                "---\nauthor: x\ntitle: \"Quoted one\"\n---\n# Other\n"
            ),
            "Quoted one"
        );
        assert_eq!(title("a/b.md", "intro\n# The H1\n## Not\n"), "The H1");
        assert_eq!(title("notes/plan-v2.md", "no heading here"), "plan-v2");
        assert_eq!(title("README", "text"), "README");
        // An h1 inside a fence is code.
        assert_eq!(title("x.md", "```\n# nope\n```\n"), "x");
        let long = format!("# {}", "t".repeat(300));
        assert_eq!(title("x.md", &long).chars().count(), MAX_TITLE);
    }
}
