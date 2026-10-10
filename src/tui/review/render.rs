//! Drawing the review viewer, in GitHub's dark theme.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::actions::{Compose, Confirm, Picker};
use super::diff::{DiffLine, FileDiff, FileStatus, LineKind};
use super::highlight::{self, FileHighlight, Rgb, Run};
use super::{Load, Row, Viewer};
use crate::github::{Reaction, ReviewThread};

/// The colours, as GitHub's dark theme has them — or the nearest the
/// 256-colour palette has, for a terminal that cannot show the real thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    /// Secondary text: timestamps, counts, hints.
    pub muted: Color,
    pub gutter: Color,
    pub added: Color,
    pub added_word: Color,
    pub added_sign: Color,
    pub removed: Color,
    pub removed_word: Color,
    pub removed_sign: Color,
    pub hunk: Color,
    pub hunk_fg: Color,
    pub comment: Color,
    pub border: Color,
    pub accent: Color,
    pub warn: Color,
    /// Whether code is coloured in the theme's own RGB, or the nearest the
    /// 256-colour palette has.
    pub truecolor: bool,
}

impl Palette {
    /// Truecolor where the terminal says it has it, through `COLORTERM`.
    pub fn detect() -> Self {
        Self::for_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }

    pub fn for_colorterm(colorterm: Option<&str>) -> Self {
        match colorterm.map(str::to_ascii_lowercase).as_deref() {
            Some("truecolor") | Some("24bit") => Self::truecolor(),
            _ => Self::indexed(),
        }
    }

    pub fn truecolor() -> Self {
        Self {
            bg: Color::Rgb(0x0d, 0x11, 0x17),
            fg: Color::Rgb(0xe6, 0xed, 0xf3),
            muted: Color::Rgb(0x8b, 0x94, 0x9e),
            gutter: Color::Rgb(0x6e, 0x76, 0x81),
            added: Color::Rgb(0x12, 0x26, 0x1e),
            added_word: Color::Rgb(0x1f, 0x6f, 0x3a),
            added_sign: Color::Rgb(0x3f, 0xb9, 0x50),
            removed: Color::Rgb(0x25, 0x17, 0x1c),
            removed_word: Color::Rgb(0x6e, 0x1e, 0x23),
            removed_sign: Color::Rgb(0xf8, 0x51, 0x49),
            hunk: Color::Rgb(0x12, 0x1d, 0x2f),
            hunk_fg: Color::Rgb(0x8b, 0x94, 0x9e),
            comment: Color::Rgb(0x16, 0x1b, 0x22),
            border: Color::Rgb(0x30, 0x36, 0x3d),
            accent: Color::Rgb(0x2f, 0x81, 0xf7),
            warn: Color::Rgb(0xd2, 0x99, 0x22),
            truecolor: true,
        }
    }

    /// The nearest entries of the xterm 256-colour palette.
    pub fn indexed() -> Self {
        Self {
            bg: Color::Indexed(233),
            fg: Color::Indexed(255),
            muted: Color::Indexed(246),
            gutter: Color::Indexed(243),
            added: Color::Indexed(22),
            added_word: Color::Indexed(28),
            added_sign: Color::Indexed(71),
            removed: Color::Indexed(52),
            removed_word: Color::Indexed(88),
            removed_sign: Color::Indexed(203),
            hunk: Color::Indexed(17),
            hunk_fg: Color::Indexed(246),
            comment: Color::Indexed(234),
            border: Color::Indexed(237),
            accent: Color::Indexed(33),
            warn: Color::Indexed(178),
            truecolor: false,
        }
    }

    /// A colour from the syntax theme, as this terminal can show it.
    pub fn code(&self, (r, g, b): Rgb) -> Color {
        match self.truecolor {
            true => Color::Rgb(r, g, b),
            false => Color::Indexed(nearest_indexed((r, g, b))),
        }
    }
}

/// The xterm 256-colour entry nearest `rgb`: the closest of the 6×6×6 cube
/// and the grey ramp.
pub fn nearest_indexed((r, g, b): Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |c: u8| {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] as i32 - c as i32).abs())
            .unwrap_or(0)
    };
    let distance = |(x, y, z): Rgb| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(x, r) + d(y, g) + d(z, b)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    // The greys run 8, 18, … 238.
    let average = (r as usize + g as usize + b as usize) / 3;
    let step = (average.saturating_sub(3) / 10).min(23);
    let grey = 8 + 10 * step as u8;
    match distance((grey, grey, grey)) < distance(cube_rgb) {
        true => 232 + step as u8,
        false => cube as u8,
    }
}

/// The keys that work where the cursor is, as the bottom bar spells them:
/// what can be done to a line of code, or to a comment — editing and
/// deleting only on a comment GitHub says the user may change.
fn keys(viewer: &Viewer) -> Vec<(&'static str, &'static str)> {
    let mut keys = vec![
        ("←→", "file"),
        ("↑↓", "move"),
        ("A", "approve"),
        ("C", "comment"),
        ("X", "request changes"),
    ];
    match viewer.row() {
        Some(Row::Line(..)) => keys.push(("c", "inline comment")),
        Some(Row::Comment { .. }) => {
            keys.extend([("R", "reply"), ("+", "react")]);
            if let Some((_, comment)) = viewer.comment() {
                if comment.viewer_can_update {
                    keys.push(("e", "edit"));
                }
                if comment.viewer_can_delete {
                    keys.push(("d", "delete"));
                }
            }
        }
        _ => {}
    }
    keys.extend([("r", "refresh"), ("Esc", "back")]);
    keys
}

/// Which keys give up their place first when the bar is too narrow.
const DROP_FIRST: [&str; 3] = ["←→", "↑↓", "r"];

pub fn draw(frame: &mut Frame, viewer: &mut Viewer) {
    let p = viewer.palette;
    let area = frame.area();
    frame.render_widget(Block::default().style(base(&p)), area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], viewer);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(chunks[1].width as usize),
            Style::default().fg(p.border),
        ))),
        chunks[1],
    );
    draw_body(frame, chunks[2], viewer);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(&viewer.status, chunks[3].width as usize),
            Style::default().fg(p.warn),
        ))),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(key_bar(&p, keys(viewer), chunks[4].width)),
        chunks[4],
    );

    if let Some(draft) = &viewer.draft {
        let pending = viewer.review().and_then(|r| r.pending.as_ref());
        super::super::ui::draw_review_body(
            frame,
            viewer.pr.number,
            draft.verdict,
            &draft.input,
            pending,
        );
    }
    if let Some(compose) = &viewer.compose {
        draw_compose(frame, &p, compose);
    }
    if let Some(picker) = &viewer.picker {
        draw_picker(frame, &p, picker);
    }
    if let Some(confirm) = &viewer.confirm {
        draw_confirm(frame, &p, confirm);
    }
    if let Some(busy) = &viewer.busy {
        super::super::ui::draw_working(frame, busy);
    }
}

fn base(p: &Palette) -> Style {
    Style::default().bg(p.bg).fg(p.fg)
}

fn draw_header(frame: &mut Frame, area: Rect, viewer: &Viewer) {
    let p = &viewer.palette;
    let width = area.width as usize;
    let position = match viewer.review() {
        Some(review) if !review.files.is_empty() => {
            format!("file {}/{} ", viewer.page + 1, review.files.len())
        }
        _ => String::new(),
    };
    // Changes on GitHub the screen does not show yet, or a fetch of them.
    let (news, news_style) = if viewer.refreshing {
        ("refreshing…  ", Style::default().fg(p.muted))
    } else if viewer.stale {
        (
            "⟳ updates – r  ",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        )
    } else {
        ("", Style::default())
    };
    let number = format!(" #{} ", viewer.pr.number);
    let room = width.saturating_sub(number.width() + news.width() + position.width() + 1);
    let title = Line::from(vec![
        Span::styled(number, Style::default().fg(p.accent)),
        Span::styled(
            fit(&clean(&viewer.pr.title), room),
            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
        ),
    ]);
    // Through a layout rather than by hand, so a terminal too short for
    // both lines gets an empty rect, not one off the edge of the buffer.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    let top = rows[0];
    frame.render_widget(Paragraph::new(title), top);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(news, news_style),
            Span::styled(position, Style::default().fg(p.muted)),
        ]))
        .alignment(Alignment::Right),
        top,
    );

    let second = rows[1];
    let line = match (&viewer.load, viewer.file()) {
        (Load::Loading, _) => Line::from(Span::styled(
            " loading the diff and its comments…",
            Style::default().fg(p.muted),
        )),
        (Load::Failed(_), _) => Line::from(Span::styled(
            " the pull request could not be loaded",
            Style::default().fg(p.removed_sign),
        )),
        (Load::Ready(_), None) => Line::from(Span::styled(
            " no files changed",
            Style::default().fg(p.muted),
        )),
        (Load::Ready(review), Some(file)) => {
            file_line(p, file, &review.threads, review.pending.as_ref(), width)
        }
    };
    frame.render_widget(Paragraph::new(line), second);
}

/// The second header line: which file, how big a change, what is said on it.
fn file_line(
    p: &Palette,
    file: &FileDiff,
    threads: &[ReviewThread],
    pending: Option<&crate::github::PendingReview>,
    width: usize,
) -> Line<'static> {
    let (added, removed) = file.counts();
    let (color, kind) = match file.status {
        FileStatus::Added => (p.added_sign, Some("new file")),
        FileStatus::Deleted => (p.removed_sign, Some("deleted")),
        FileStatus::Renamed => (p.fg, Some("renamed")),
        FileStatus::Modified => (p.fg, None),
    };
    let mut notes: Vec<String> = Vec::new();
    notes.extend(kind.map(str::to_string));
    if file.binary {
        notes.push("binary".into());
    }
    let on_file = threads.iter().filter(|t| t.path == file.path()).count();
    if on_file > 0 {
        notes.push(format!(
            "{on_file} thread{}",
            if on_file == 1 { "" } else { "s" }
        ));
    }
    if let Some(review) = pending {
        notes.push(format!(
            "your pending review: {} comment{}",
            review.comments,
            if review.comments == 1 { "" } else { "s" }
        ));
    }
    let counts = format!("  +{added} −{removed}");
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!("  · {}", notes.join(" · "))
    };
    let room = width.saturating_sub(1 + counts.width() + notes.width().min(width / 2));
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            fit_start(&clean(&file.label()), room.max(8)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  +{added}"), Style::default().fg(p.added_sign)),
        Span::styled(format!(" −{removed}"), Style::default().fg(p.removed_sign)),
        Span::styled(notes, Style::default().fg(p.muted)),
    ])
}

fn key_bar(p: &Palette, mut keys: Vec<(&str, &str)>, width: u16) -> Line<'static> {
    let len = |keys: &[(&str, &str)]| -> usize {
        1 + keys
            .iter()
            .map(|(key, what)| key.width() + 1 + what.width())
            .sum::<usize>()
            + 3 * keys.len().saturating_sub(1)
    };
    for key in DROP_FIRST {
        if len(&keys) <= width as usize {
            break;
        }
        keys.retain(|(k, _)| *k != key);
    }
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, (key, what)) in keys.iter().enumerate() {
        let sep = if i == 0 { "" } else { " · " };
        let len = sep.width() + key.width() + 1 + what.width();
        if used + len > width as usize {
            break;
        }
        used += len;
        spans.push(Span::styled(sep, Style::default().fg(p.border)));
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {what}"),
            Style::default().fg(p.muted),
        ));
    }
    Line::from(spans)
}

fn draw_body(frame: &mut Frame, area: Rect, viewer: &mut Viewer) {
    let p = viewer.palette;
    let height = area.height as usize;
    viewer.page_rows = height.saturating_sub(1).max(1);
    let message = |lines: Vec<String>| -> Vec<Line<'static>> {
        lines
            .into_iter()
            .map(|l| Line::from(Span::styled(format!("  {l}"), Style::default().fg(p.muted))))
            .collect()
    };
    let lines = match &viewer.load {
        Load::Loading => message(vec![
            String::new(),
            format!("loading #{} from GitHub…", viewer.pr.number),
        ]),
        Load::Failed(why) => {
            let mut lines = vec![String::new(), "could not load the pull request:".into()];
            lines.extend(super::super::ui::wrap_words(
                &clean(why),
                (area.width as usize).saturating_sub(4).max(10),
            ));
            lines.extend([String::new(), "esc to go back".into()]);
            message(lines)
        }
        Load::Ready(review) => match review.files.get(viewer.page) {
            None => message(vec![
                String::new(),
                "this pull request changes no files".into(),
            ]),
            Some(file) => {
                let ctx = Ctx {
                    p: &p,
                    file,
                    highlight: viewer.highlights.get(viewer.page),
                    threads: &review.threads,
                    numbers: file.number_width().max(3),
                    width: area.width as usize,
                };
                let heights = |i: usize| ctx.height(&viewer.rows[i]);
                let scroll = fit_scroll(viewer.scroll, viewer.cursor, height, heights);
                let mut lines = Vec::new();
                for (i, row) in viewer.rows.iter().enumerate().skip(scroll) {
                    if lines.len() >= height {
                        break;
                    }
                    lines.extend(ctx.lines(row, i == viewer.cursor));
                }
                viewer.scroll = scroll;
                lines
            }
        },
    };
    frame.render_widget(Paragraph::new(lines).style(base(&p)), area);
}

/// A box over the diff in the viewer's colours, `width` by `height` cells
/// counting its border, centred and cut down to fit the frame. Returns the
/// space inside the border.
fn draw_box(
    frame: &mut Frame,
    p: &Palette,
    title: &str,
    title_color: Color,
    width: usize,
    height: usize,
) -> Rect {
    let area = frame.area();
    let w = (width as u16).min(area.width);
    let h = (height as u16).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let title = fit(
        &format!(" {} ", clean(title)),
        (w as usize).saturating_sub(2),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.border))
        .title(Span::styled(title, Style::default().fg(title_color)))
        .style(Style::default().bg(p.comment).fg(p.fg));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    inner
}

/// The width of a box that takes most of the frame, but not absurdly much.
fn box_width(frame: &Frame) -> usize {
    let width = frame.area().width as usize;
    (width * 7 / 10).clamp(40.min(width), 100)
}

/// The text box for an inline comment, a reply or an edit.
fn draw_compose(frame: &mut Frame, p: &Palette, compose: &Compose) {
    let width = box_width(frame);
    let inner = width.saturating_sub(2).max(1);
    let hint = format!(
        "⏎ {} · ctrl-j new line · ctrl-u clear · esc cancel",
        compose.target.send_label()
    );
    let mut text: Vec<Line<'static>> = Vec::new();
    for line in compose.text.split('\n') {
        for wrapped in super::super::ui::wrap_words(&clean(line), inner) {
            text.push(Line::from(Span::styled(wrapped, Style::default().fg(p.fg))));
        }
    }
    // The cursor, after the last character typed.
    if let Some(last) = text.last_mut() {
        last.spans
            .push(Span::styled("█", Style::default().fg(p.accent)));
    }
    if compose.text.is_empty() {
        text = vec![Line::from(vec![
            Span::styled("█", Style::default().fg(p.accent)),
            Span::styled("leave a comment", Style::default().fg(p.muted)),
        ])];
    }
    // Room for the hint under a blank line; a long text shows its end, where
    // the typing is.
    let room = (frame.area().height as usize).saturating_sub(4).max(1);
    let shown = text.len().min(room.saturating_sub(2).max(1));
    let mut lines: Vec<Line<'static>> = text.split_off(text.len() - shown);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        fit(&hint, inner),
        Style::default().fg(p.muted),
    )));
    let area = draw_box(
        frame,
        p,
        &compose.target.title(),
        p.accent,
        width,
        lines.len() + 2,
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// The row of reactions to pick from.
fn draw_picker(frame: &mut Frame, p: &Palette, picker: &Picker) {
    let mut row = vec![Span::raw(" ")];
    for (i, reaction) in Reaction::ALL.iter().enumerate() {
        let mut style = Style::default().fg(p.fg);
        if picker.mine.contains(reaction) {
            style = style.fg(p.accent).add_modifier(Modifier::UNDERLINED);
        }
        if i == picker.selected {
            style = style.bg(p.accent);
        }
        row.push(Span::styled(format!(" {} ", reaction.emoji()), style));
    }
    let digits: String = (1..=Reaction::ALL.len())
        .map(|n| format!("  {n} "))
        .collect();
    let lines = vec![
        Line::from(row),
        Line::from(Span::styled(
            format!(" {digits}"),
            Style::default().fg(p.muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " ←→ choose · ⏎ react · esc cancel",
            Style::default().fg(p.muted),
        )),
    ];
    let area = draw_box(frame, p, "react", p.accent, 38, lines.len() + 2);
    frame.render_widget(Paragraph::new(lines), area);
}

/// The question before a comment is deleted.
fn draw_confirm(frame: &mut Frame, p: &Palette, confirm: &Confirm) {
    let width = box_width(frame).min(64);
    let inner = width.saturating_sub(2).max(1);
    let lines = vec![
        Line::from(Span::styled(
            fit(&format!("“{}”", clean(&confirm.preview)), inner),
            Style::default().fg(p.fg).add_modifier(Modifier::ITALIC),
        )),
        Line::from(Span::styled(
            "Delete this comment? GitHub cannot bring it back.",
            Style::default().fg(p.muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "y",
                Style::default()
                    .fg(p.removed_sign)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" delete · ", Style::default().fg(p.muted)),
            Span::styled("n", Style::default().fg(p.fg).add_modifier(Modifier::BOLD)),
            Span::styled(" keep it", Style::default().fg(p.muted)),
        ]),
    ];
    let area = draw_box(
        frame,
        p,
        "delete comment",
        p.removed_sign,
        width,
        lines.len() + 2,
    );
    frame.render_widget(Paragraph::new(lines), area);
}

/// The first row to draw so the cursor's row is wholly on screen, moving the
/// view no further than it has to. `height` is in screen lines, and `row_height`
/// says how many lines a row takes.
pub fn fit_scroll(
    scroll: usize,
    cursor: usize,
    height: usize,
    row_height: impl Fn(usize) -> usize,
) -> usize {
    if cursor <= scroll {
        return cursor;
    }
    let mut total: usize = (scroll..=cursor).map(&row_height).sum();
    let mut first = scroll;
    // A row taller than the screen is shown from its top.
    while total > height && first < cursor {
        total -= row_height(first);
        first += 1;
    }
    first
}

/// What drawing one page's rows needs to know.
struct Ctx<'a> {
    p: &'a Palette,
    file: &'a FileDiff,
    /// The file's syntax colours, once worked out; plain until then.
    highlight: Option<&'a FileHighlight>,
    threads: &'a [ReviewThread],
    /// Digits in the widest line number.
    numbers: usize,
    width: usize,
}

impl Ctx<'_> {
    fn height(&self, row: &Row) -> usize {
        match row {
            Row::Comment { .. } => self.lines(row, false).len(),
            _ => 1,
        }
    }

    fn lines(&self, row: &Row, selected: bool) -> Vec<Line<'static>> {
        match row {
            Row::Note(text) => vec![self.note(text, selected)],
            Row::Hunk(h) => vec![self.hunk(&self.file.hunks[*h].header, selected)],
            Row::Line(h, l) => {
                let runs = self.highlight.and_then(|hl| hl.line(*h, *l));
                vec![self.diff_line(&self.file.hunks[*h].lines[*l], runs, selected)]
            }
            Row::Comment {
                thread,
                index,
                anchored,
            } => self.comment(&self.threads[*thread], *index, *anchored, selected),
        }
    }

    fn marker(&self, selected: bool, bg: Color) -> Span<'static> {
        if selected {
            Span::styled("▌", Style::default().fg(self.p.accent).bg(bg))
        } else {
            Span::styled(" ", Style::default().bg(bg))
        }
    }

    fn note(&self, text: &str, selected: bool) -> Line<'static> {
        let mut line = Builder::new(self.width);
        line.push_span(self.marker(selected, self.p.bg));
        line.push(
            &format!(" {text}"),
            Style::default()
                .fg(self.p.muted)
                .bg(self.p.bg)
                .add_modifier(Modifier::ITALIC),
        );
        line.finish(Style::default().bg(self.p.bg))
    }

    fn hunk(&self, header: &str, selected: bool) -> Line<'static> {
        let style = Style::default().fg(self.p.hunk_fg).bg(self.p.hunk);
        let mut line = Builder::new(self.width);
        line.push_span(self.marker(selected, self.p.hunk));
        line.push(&" ".repeat(self.numbers * 2 + 3), style);
        line.push_code(header, style);
        line.finish(style)
    }

    fn diff_line(&self, diff: &DiffLine, runs: Option<&[Run]>, selected: bool) -> Line<'static> {
        let p = self.p;
        let (bg, word, sign, sign_color) = match diff.kind {
            LineKind::Added => (p.added, p.added_word, "+", p.added_sign),
            LineKind::Removed => (p.removed, p.removed_word, "-", p.removed_sign),
            LineKind::Context => (p.bg, p.bg, " ", p.fg),
        };
        let number = |n: Option<u32>| match n {
            Some(n) => format!("{n:>w$}", w = self.numbers),
            None => " ".repeat(self.numbers),
        };
        let gutter = if selected {
            Style::default()
                .fg(p.fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.gutter).bg(bg)
        };
        let code = Style::default().fg(p.fg).bg(bg);
        let mut line = Builder::new(self.width);
        line.push_span(self.marker(selected, bg));
        line.push(
            &format!("{} {} ", number(diff.old), number(diff.new)),
            gutter,
        );
        line.push(sign, Style::default().fg(sign_color).bg(bg));
        line.push(" ", code);
        // The syntax colours the text; the diff keeps the backgrounds.
        let pieces = highlight::pieces(&diff.text, runs.unwrap_or(&[]), diff.emphasis.as_ref());
        for (range, tint, emphasised) in pieces {
            let mut style = code;
            if let Some(fg) = tint.fg {
                style = style.fg(p.code(fg));
            }
            if tint.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if tint.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if emphasised {
                style = style.bg(word);
            }
            line.push_code(&diff.text[range], style);
        }
        line.finish(code)
    }

    /// One comment of a thread, boxed; the thread's first comment opens the
    /// box and its last closes it.
    fn comment(
        &self,
        thread: &ReviewThread,
        index: usize,
        anchored: bool,
        selected: bool,
    ) -> Vec<Line<'static>> {
        let p = self.p;
        let Some(comment) = thread.comments.get(index) else {
            return Vec::new();
        };
        // In line with the code, when there is room for a box beside it.
        let code_column = self.numbers * 2 + 5;
        let indent = if self.width >= code_column + 24 {
            code_column
        } else {
            1
        };
        let inner = self.width.saturating_sub(indent + 1 + 4).max(8);
        let border = Style::default()
            .fg(if selected { p.accent } else { p.border })
            .bg(p.comment);
        let text = Style::default().fg(p.fg).bg(p.comment);
        let dim = Style::default().fg(p.muted).bg(p.comment);
        let lead = |selected: bool| {
            vec![
                self.marker(selected, p.bg),
                Span::styled(" ".repeat(indent - 1), Style::default().bg(p.bg)),
            ]
        };
        // `│ content │`, with the content padded out to the box.
        let boxed = |content: Vec<Span<'static>>| -> Line<'static> {
            let used: usize = content.iter().map(|s| s.content.width()).sum();
            let mut spans = lead(selected);
            spans.push(Span::styled("│ ", border));
            spans.extend(content);
            spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), text));
            spans.push(Span::styled(" │", border));
            Line::from(spans)
        };
        let rule = |left: &str, label: Option<String>, right: &str| -> Line<'static> {
            let mut spans = lead(selected);
            spans.push(Span::styled(format!("{left}─"), border));
            let mut used = 1;
            if let Some(label) = label {
                let label = fit(&format!(" {label} "), inner.saturating_sub(2));
                used += label.width();
                spans.push(Span::styled(label, dim));
            }
            spans.push(Span::styled(
                format!("{}{right}", "─".repeat((inner + 2).saturating_sub(used))),
                border,
            ));
            Line::from(spans)
        };

        let mut lines = Vec::new();
        if index == 0 {
            let mut label = Vec::new();
            if !anchored {
                label.push(match thread.original_line {
                    Some(line) => format!("on line {line} of an earlier version"),
                    None => "not on a line of this diff".into(),
                });
            }
            if thread.is_outdated {
                label.push("outdated".into());
            }
            if thread.is_resolved {
                label.push("resolved".into());
            }
            let label = (!label.is_empty()).then(|| label.join(" · "));
            lines.push(rule("╭", label, "╮"));
        }

        let when = comment
            .created_secs()
            .map(crate::agent::age_label)
            .unwrap_or_default();
        let author = fit(
            &clean(comment.author_login()),
            inner.saturating_sub(when.width() + 3),
        );
        let mut heading = vec![Span::styled(author, text.add_modifier(Modifier::BOLD))];
        if !when.is_empty() {
            heading.push(Span::styled(format!(" · {when}"), dim));
        }
        lines.push(boxed(heading));

        for paragraph in comment.body.lines() {
            for wrapped in super::super::ui::wrap_words(&clean(paragraph), inner) {
                lines.push(boxed(vec![Span::styled(fit(&wrapped, inner), text)]));
            }
        }

        let reactions: Vec<Span<'static>> = comment
            .reaction_groups
            .iter()
            .filter(|g| g.count() > 0)
            .filter_map(|g| Some((g.reaction()?, g.count(), g.viewer_has_reacted)))
            .map(|(reaction, count, mine)| {
                Span::styled(
                    format!("{} {count}  ", reaction.emoji()),
                    if mine { text.fg(p.accent) } else { dim },
                )
            })
            .collect();
        if !reactions.is_empty() {
            lines.push(boxed(reactions));
        }

        if index + 1 == thread.comments.len() {
            lines.push(rule("╰", None, "╯"));
        } else {
            lines.push(rule("├", None, "┤"));
        }
        lines
    }
}

/// One screen line, built left to right and cut off at the edge.
struct Builder {
    spans: Vec<Span<'static>>,
    used: usize,
    width: usize,
    /// Column within the code, for tab stops.
    column: usize,
    full: bool,
}

/// Tab stops in code, every this many columns.
const TAB: usize = 4;

impl Builder {
    fn new(width: usize) -> Self {
        Self {
            spans: Vec::new(),
            used: 0,
            width,
            column: 0,
            full: false,
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        let text = span.content.to_string();
        self.push(&text, span.style);
    }

    /// Text that is already safe to print, cut at the edge with a `›`.
    fn push(&mut self, text: &str, style: Style) {
        if self.full {
            return;
        }
        let mut taken = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if self.used + w > self.width {
                // Make room for the mark that says there is more.
                while self.used + 1 > self.width {
                    match taken.pop() {
                        Some(gone) => self.used -= gone.width().unwrap_or(0),
                        None => break,
                    }
                }
                if !taken.is_empty() {
                    self.spans
                        .push(Span::styled(std::mem::take(&mut taken), style));
                }
                if self.used < self.width {
                    self.spans.push(Span::styled("›", style));
                    self.used += 1;
                }
                self.full = true;
                return;
            }
            taken.push(c);
            self.used += w;
        }
        if !taken.is_empty() {
            self.spans.push(Span::styled(taken, style));
        }
    }

    /// Code from the diff: tabs expanded to the next stop, and anything that
    /// would act on the terminal rather than print made visible instead.
    fn push_code(&mut self, text: &str, style: Style) {
        let mut out = String::new();
        for c in text.chars() {
            match c {
                '\t' => {
                    let pad = TAB - self.column % TAB;
                    out.push_str(&" ".repeat(pad));
                    self.column += pad;
                }
                c if unsafe_char(c) => {
                    out.push('�');
                    self.column += 1;
                }
                c => {
                    out.push(c);
                    self.column += c.width().unwrap_or(0);
                }
            }
        }
        self.push(&out, style);
    }

    /// The line, padded out to the full width in `fill`.
    fn finish(mut self, fill: Style) -> Line<'static> {
        if self.used < self.width {
            self.spans
                .push(Span::styled(" ".repeat(self.width - self.used), fill));
        }
        Line::from(self.spans)
    }
}

/// Control characters would drive the terminal, and bidirectional overrides
/// would make code read differently from how it runs. Neither gets printed
/// as it is: what a PR's author wrote must not be able to disguise itself.
fn unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Text from GitHub, made safe to print: tabs become spaces and anything in
/// [`unsafe_char`] becomes `�`.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if unsafe_char(c) => '�',
            c => c,
        })
        .collect()
}

fn fit(text: &str, width: usize) -> String {
    super::super::ui::truncate(text, width)
}

fn fit_start(text: &str, width: usize) -> String {
    super::super::ui::truncate_start(text, width)
}

#[cfg(test)]
mod tests {
    use super::super::tests::loaded_viewer;
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;

    fn render(viewer: &mut Viewer, width: u16, height: u16) -> (String, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, viewer)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buffer)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn falls_back_to_256_colours_without_truecolor() {
        assert_eq!(
            Palette::for_colorterm(Some("truecolor")),
            Palette::truecolor()
        );
        assert_eq!(Palette::for_colorterm(Some("24bit")), Palette::truecolor());
        assert_eq!(
            Palette::for_colorterm(Some("TrueColor")),
            Palette::truecolor()
        );
        assert_eq!(Palette::for_colorterm(None), Palette::indexed());
        assert_eq!(Palette::for_colorterm(Some("")), Palette::indexed());
        assert_eq!(Palette::truecolor().bg, Color::Rgb(0x0d, 0x11, 0x17));
        let indexed = Palette::indexed();
        for color in [indexed.bg, indexed.added, indexed.removed, indexed.accent] {
            assert!(matches!(color, Color::Indexed(_)), "{color:?}");
        }
    }

    #[test]
    fn draws_a_file_page_with_its_comments() {
        let mut viewer = loaded_viewer(None);
        let (screen, buffer) = render(&mut viewer, 100, 40);
        assert!(screen.contains("#42 Widen the store size"), "{screen}");
        assert!(screen.contains("file 1/5"), "{screen}");
        assert!(screen.contains("src/lib.rs  +3 −3"), "{screen}");
        assert!(
            screen.contains("@@ -8,7 +8,8 @@ pub struct Store {"),
            "{screen}"
        );
        assert!(screen.contains("  11     -     size: u32,"), "{screen}");
        assert!(screen.contains("      12 +     files: usize,"), "{screen}");
        assert!(screen.contains("ada"), "{screen}");
        assert!(screen.contains("Should this be `u64`?"), "{screen}");
        // A wide glyph takes two cells; the second reads back as a space.
        assert!(screen.contains("👍  2"), "{screen}");
        assert!(
            screen.contains("on line 40 of an earlier version · outdated"),
            "{screen}"
        );
        assert!(
            screen.contains("A approve · C comment · X request changes"),
            "{screen}"
        );
        assert!(screen.contains("Esc back"), "{screen}");

        // GitHub's colours: the page background, a comment box, an added line.
        assert_eq!(buffer[(50, 0)].bg, Color::Rgb(0x0d, 0x11, 0x17));
        assert_eq!(buffer[(50, 3)].bg, Color::Rgb(0x16, 0x1b, 0x22));
        let added_row = screen
            .lines()
            .position(|l| l.contains("files: usize"))
            .unwrap() as u16;
        assert_eq!(buffer[(60, added_row)].bg, Color::Rgb(0x12, 0x26, 0x1e));
        // And the word that changed stands out from the rest of its line.
        let removed_row = screen
            .lines()
            .position(|l| l.contains("size: u32"))
            .unwrap() as u16;
        let line = screen.lines().nth(removed_row as usize).unwrap();
        let at = line[..line.find("u32").unwrap()].chars().count() as u16;
        assert_eq!(buffer[(at, removed_row)].bg, Color::Rgb(0x6e, 0x1e, 0x23));
        assert_eq!(
            buffer[(at - 3, removed_row)].bg,
            Color::Rgb(0x25, 0x17, 0x1c)
        );
    }

    /// Where `needle` starts on the screen's first line containing it.
    fn find(screen: &str, needle: &str) -> (u16, u16) {
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not on screen: {screen}"));
        let x = line[..line.find(needle).unwrap()].chars().count();
        (x as u16, y as u16)
    }

    /// The page on screen highlighted, as the background thread would.
    fn highlight_page(viewer: &mut Viewer) {
        let file = viewer.file().unwrap().clone();
        viewer
            .highlights
            .insert(viewer.page, super::super::highlight::highlight(&file));
    }

    #[test]
    fn code_is_plain_until_its_colours_are_ready() {
        let mut viewer = loaded_viewer(None);
        let (screen, buffer) = render(&mut viewer, 100, 40);
        let (x, y) = find(&screen, "impl Store");
        assert_eq!(buffer[(x, y)].fg, Color::Rgb(0xe6, 0xed, 0xf3));

        highlight_page(&mut viewer);
        let (screen, buffer) = render(&mut viewer, 100, 40);
        let (x, y) = find(&screen, "impl Store");
        // The keyword in GitHub's red, the type name in its orange.
        assert_eq!(buffer[(x, y)].fg, Color::Rgb(0xff, 0x7b, 0x72));
        assert_eq!(buffer[(x + 5, y)].fg, Color::Rgb(0xff, 0xa6, 0x57));
        assert_eq!(buffer[(x, y)].bg, Color::Rgb(0x0d, 0x11, 0x17));
    }

    #[test]
    fn colours_sit_on_the_added_and_removed_backgrounds() {
        let mut viewer = loaded_viewer(None);
        highlight_page(&mut viewer);
        let (screen, buffer) = render(&mut viewer, 100, 40);
        // `usize` on an added line: syntax colour, added background.
        let (x, y) = find(&screen, "usize");
        assert_eq!(buffer[(x, y)].fg, Color::Rgb(0xff, 0x7b, 0x72));
        assert_eq!(buffer[(x, y)].bg, Color::Rgb(0x12, 0x26, 0x1e));
        // `u32`, the word that changed on a removed line, keeps its
        // emphasis under the colour.
        let (x, y) = find(&screen, "u32");
        assert_eq!(buffer[(x, y)].bg, Color::Rgb(0x6e, 0x1e, 0x23));
        assert_eq!(buffer[(x, y)].fg, Color::Rgb(0xff, 0x7b, 0x72));
        assert_eq!(buffer[(x - 3, y)].bg, Color::Rgb(0x25, 0x17, 0x1c));
        // The line under the cursor is still marked.
        assert!(screen.contains("▌"), "{screen}");
    }

    #[test]
    fn colours_fall_back_to_the_256_colour_palette() {
        let mut viewer = loaded_viewer(None);
        viewer.palette = Palette::indexed();
        highlight_page(&mut viewer);
        let (screen, buffer) = render(&mut viewer, 100, 40);
        let (x, y) = find(&screen, "impl Store");
        assert_eq!(
            buffer[(x, y)].fg,
            Color::Indexed(nearest_indexed((0xff, 0x7b, 0x72)))
        );
        // `{` has nothing to say, and keeps the text's usual colour.
        assert_eq!(buffer[(x + 11, y)].fg, Color::Indexed(255));
    }

    #[test]
    fn nearest_indexed_picks_the_closest_entry() {
        assert_eq!(nearest_indexed((0, 0, 0)), 16);
        assert_eq!(nearest_indexed((255, 255, 255)), 231);
        assert_eq!(nearest_indexed((255, 0, 0)), 196);
        assert_eq!(nearest_indexed((0x80, 0x80, 0x80)), 244, "a grey");
        assert_eq!(nearest_indexed((0xff, 0x7b, 0x72)), 209);
        assert_eq!(nearest_indexed((0xe6, 0xed, 0xf3)), 255);
    }

    #[test]
    fn any_terminal_size_draws_without_panicking() {
        let mut viewer = loaded_viewer(None);
        for (width, height) in [(1, 1), (2, 5), (10, 3), (20, 8), (30, 40), (240, 6)] {
            viewer.cursor = 0;
            render(&mut viewer, width, height);
            viewer.handle_key(key(KeyCode::End));
            render(&mut viewer, width, height);
            viewer.handle_key(key(KeyCode::Char('C')));
            render(&mut viewer, width, height);
            viewer.handle_key(key(KeyCode::Esc));
        }
    }

    #[test]
    fn the_header_counts_pages_as_you_turn_them() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Right));
        viewer.handle_key(key(KeyCode::Right));
        let (screen, _) = render(&mut viewer, 80, 20);
        assert!(screen.contains("file 3/5"), "{screen}");
        assert!(screen.contains("docs/new file.md"), "{screen}");
        assert!(screen.contains("new file"), "{screen}");
    }

    #[test]
    fn the_cursor_is_always_on_screen() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::End));
        let (screen, _) = render(&mut viewer, 80, 12);
        assert!(screen.contains("self.files = 0;"), "{screen}");
        assert!(viewer.scroll > 0);
        viewer.handle_key(key(KeyCode::Home));
        let (screen, _) = render(&mut viewer, 80, 12);
        assert_eq!(viewer.scroll, 0);
        assert!(screen.contains("outdated"), "{screen}");
    }

    #[test]
    fn fit_scroll_moves_only_as_far_as_it_must() {
        let ones = |_: usize| 1;
        assert_eq!(fit_scroll(0, 3, 10, ones), 0);
        assert_eq!(fit_scroll(0, 12, 10, ones), 3);
        assert_eq!(fit_scroll(5, 2, 10, ones), 2);
        // A tall row on the way pushes the view further.
        let tall = |i: usize| if i == 1 { 6 } else { 1 };
        assert_eq!(fit_scroll(0, 5, 8, tall), 2);
        // A row taller than the screen shows from its top.
        assert_eq!(fit_scroll(0, 1, 4, tall), 1);
    }

    #[test]
    fn nothing_from_the_pr_reaches_the_terminal_raw() {
        assert_eq!(clean("a\u{1b}[31mb"), "a�[31mb");
        assert_eq!(clean("x\u{202e}y"), "x�y");
        let mut line = Builder::new(20);
        line.push_code("\tx\u{7}", Style::default());
        let text: String = line
            .finish(Style::default())
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text, "    x�              ");
    }

    #[test]
    fn long_lines_are_cut_at_the_edge() {
        let mut line = Builder::new(6);
        line.push("abcdefghij", Style::default());
        line.push("more", Style::default());
        let text: String = line
            .finish(Style::default())
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text, "abcde›");
        assert_eq!(text.width(), 6);
    }

    #[test]
    fn a_draft_shows_the_review_body_panel() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Char('A')));
        let (screen, _) = render(&mut viewer, 100, 30);
        assert!(screen.contains("approve #42"), "{screen}");
        assert!(screen.contains("posts a new review"), "{screen}");
    }

    #[test]
    fn loading_and_failure_are_said_in_the_body() {
        let mut viewer = Viewer::new("/x".into(), super::super::tests::pr(), Palette::indexed());
        let (screen, _) = render(&mut viewer, 80, 12);
        assert!(screen.contains("loading #42 from GitHub"), "{screen}");
        viewer.loaded(
            Err("gh: auth required".into()),
            Ok(Vec::new()),
            Ok(None),
            Err("gh: auth required".into()),
        );
        let (screen, _) = render(&mut viewer, 80, 12);
        assert!(
            screen.contains("could not load the pull request:"),
            "{screen}"
        );
        assert!(screen.contains("gh: auth required"), "{screen}");
    }

    fn go_to(viewer: &mut Viewer, row: Row) {
        viewer.cursor = viewer.rows.iter().position(|r| *r == row).unwrap();
    }

    fn own_reply() -> Row {
        Row::Comment {
            thread: 0,
            index: 1,
            anchored: true,
        }
    }

    #[test]
    fn the_key_bar_offers_what_works_where_the_cursor_is() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, Row::Line(0, 5));
        let (screen, _) = render(&mut viewer, 120, 30);
        let bar = screen.lines().last().unwrap();
        assert!(
            bar.contains("A approve · C comment · X request changes · c inline comment"),
            "{bar}"
        );
        assert!(bar.contains("r refresh · Esc back"), "{bar}");
        assert!(!bar.contains("reply"), "{bar}");

        // Someone else's comment: reply and react, but no edit or delete.
        go_to(
            &mut viewer,
            Row::Comment {
                thread: 0,
                index: 0,
                anchored: true,
            },
        );
        let (screen, _) = render(&mut viewer, 120, 30);
        let bar = screen.lines().last().unwrap();
        assert!(bar.contains("R reply · + react · r refresh"), "{bar}");
        assert!(!bar.contains("edit") && !bar.contains("delete"), "{bar}");
        assert!(!bar.contains("inline comment"), "{bar}");

        // The user's own.
        go_to(&mut viewer, own_reply());
        let (screen, _) = render(&mut viewer, 120, 30);
        let bar = screen.lines().last().unwrap();
        assert!(
            bar.contains("R reply · + react · e edit · d delete"),
            "{bar}"
        );
    }

    #[test]
    fn a_narrow_key_bar_drops_the_obvious_keys_first() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, own_reply());
        let (screen, _) = render(&mut viewer, 96, 30);
        let bar = screen.lines().last().unwrap();
        assert!(!bar.contains("←→"), "{bar}");
        assert!(bar.contains("e edit · d delete"), "{bar}");
        assert!(bar.contains("Esc back"), "{bar}");
    }

    #[test]
    fn news_from_github_shows_in_the_header() {
        let mut viewer = loaded_viewer(None);
        let (screen, _) = render(&mut viewer, 100, 20);
        assert!(!screen.contains("⟳"), "{screen}");
        viewer.stale = true;
        let (screen, buffer) = render(&mut viewer, 100, 20);
        let top = screen.lines().next().unwrap();
        assert!(top.contains("⟳ updates – r  file 1/5"), "{top}");
        let at = top.chars().position(|c| c == '⟳').unwrap() as u16;
        assert_eq!(buffer[(at, 0)].fg, Color::Rgb(0x2f, 0x81, 0xf7));
        viewer.refreshing = true;
        let (screen, _) = render(&mut viewer, 100, 20);
        assert!(screen.lines().next().unwrap().contains("refreshing…"));
    }

    #[test]
    fn the_comment_box_says_what_it_is_for() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, Row::Line(0, 5));
        viewer.handle_key(key(KeyCode::Char('c')));
        let (screen, _) = render(&mut viewer, 100, 30);
        assert!(screen.contains("comment on src/lib.rs:12"), "{screen}");
        assert!(screen.contains("leave a comment"), "{screen}");
        assert!(screen.contains("⏎ add to your pending review"), "{screen}");
        for c in "first".chars() {
            viewer.handle_key(key(KeyCode::Char(c)));
        }
        viewer.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        for c in "second".chars() {
            viewer.handle_key(key(KeyCode::Char(c)));
        }
        let (screen, _) = render(&mut viewer, 100, 30);
        assert!(screen.contains("first"), "{screen}");
        assert!(screen.contains("second█"), "{screen}");

        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, own_reply());
        viewer.handle_key(key(KeyCode::Char('R')));
        let (screen, _) = render(&mut viewer, 100, 30);
        assert!(screen.contains("reply to ada"), "{screen}");
    }

    #[test]
    fn a_long_comment_box_shows_where_the_typing_is() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, Row::Line(0, 5));
        viewer.handle_key(key(KeyCode::Char('c')));
        for i in 0..40 {
            for c in format!("line {i}").chars() {
                viewer.handle_key(key(KeyCode::Char(c)));
            }
            viewer.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        viewer.handle_key(key(KeyCode::Char('z')));
        let (screen, _) = render(&mut viewer, 80, 20);
        assert!(screen.contains("z█"), "{screen}");
        assert!(screen.contains("⏎ add to your pending review"), "{screen}");
        assert!(
            !screen.contains("line 0 "),
            "the start scrolled away: {screen}"
        );
    }

    #[test]
    fn the_picker_shows_every_reaction() {
        let mut viewer = loaded_viewer(None);
        go_to(
            &mut viewer,
            Row::Comment {
                thread: 0,
                index: 0,
                anchored: true,
            },
        );
        viewer.handle_key(key(KeyCode::Char('+')));
        viewer.handle_key(key(KeyCode::Right));
        let (screen, buffer) = render(&mut viewer, 100, 30);
        assert!(screen.contains("react"), "{screen}");
        for emoji in ["👍", "👎", "😄", "🎉", "😕", "🚀", "👀"] {
            assert!(screen.contains(emoji), "{emoji} missing: {screen}");
        }
        assert!(screen.contains("⏎ react"), "{screen}");
        // The chosen one stands out.
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("👎"))
            .unwrap();
        let x = line[..line.find("👎").unwrap()].chars().count() as u16;
        assert_eq!(buffer[(x, y as u16)].bg, Color::Rgb(0x2f, 0x81, 0xf7));
    }

    #[test]
    fn a_delete_asks_first() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, own_reply());
        viewer.handle_key(key(KeyCode::Char('d')));
        let (screen, _) = render(&mut viewer, 100, 30);
        assert!(screen.contains("delete comment"), "{screen}");
        assert!(screen.contains("Good catch, fixed."), "{screen}");
        assert!(screen.contains("y delete · n keep it"), "{screen}");
    }

    #[test]
    fn every_box_draws_at_any_size() {
        for (width, height) in [(1, 1), (2, 5), (10, 3), (20, 8), (30, 40), (240, 6)] {
            for open in ['c', 'R', '+', 'd', 'e'] {
                let mut viewer = loaded_viewer(None);
                let row = match open {
                    'c' => Row::Line(0, 5),
                    _ => own_reply(),
                };
                go_to(&mut viewer, row);
                viewer.handle_key(key(KeyCode::Char(open)));
                assert!(viewer.overlay_open(), "{open}");
                render(&mut viewer, width, height);
            }
        }
    }
}
