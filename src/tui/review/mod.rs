//! The review viewer: a pull request's diff, one file to a page, in GitHub's
//! dark colours, with each review thread under the line it is about.
//!
//! Everything slow — the diff, the threads, the pending review, submitting a
//! verdict — runs on a background thread and comes back over a channel, so
//! the viewer is on screen the moment it is asked for and never stops
//! drawing while GitHub thinks.

pub mod diff;
mod render;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use crate::github::{self, PendingReview, PullRequest, ReviewThread, Verdict};
use diff::{FileDiff, LineKind};
use render::Palette;

use super::{BodyKey, Tui};

/// How the viewer was left, which decides what its caller does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Esc: back to wherever it was opened from.
    Back,
    /// ctrl-c or ctrl-q: the user is done with jeet altogether.
    Quit,
}

/// What came of a visit to the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub exit: Exit,
    /// The verdict submitted from it, if one was.
    pub submitted: Option<Verdict>,
}

/// Show the review of `pr` in the explorer's terminal, returning when the
/// user leaves it. `dir` is a checkout of the PR's repository, for `gh`.
pub fn view(terminal: &mut Tui, dir: &Path, pr: &PullRequest) -> Result<Outcome> {
    let mut viewer = Viewer::new(dir.to_path_buf(), pr.clone(), Palette::detect());
    viewer.start_load();
    event_loop(terminal, &mut viewer)?;
    Ok(Outcome {
        exit: viewer.exit.unwrap_or(Exit::Back),
        submitted: viewer.submitted,
    })
}

/// The viewer on a terminal of its own, for `jeet review`.
pub fn run(dir: &Path, pr: &PullRequest) -> Result<Outcome> {
    let mut terminal = super::init_terminal()?;
    let outcome = view(&mut terminal, dir, pr);
    super::restore_terminal(&mut terminal)?;
    outcome
}

fn event_loop(terminal: &mut Tui, viewer: &mut Viewer) -> Result<()> {
    while viewer.exit.is_none() {
        viewer.take_updates();
        terminal.draw(|frame| render::draw(frame, viewer))?;
        // Block on input unless an answer is on its way.
        if viewer.jobs > 0 && !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(job) = viewer.handle_key(key) {
                    viewer.spawn(job);
                }
            }
            Event::Mouse(mouse) => viewer.handle_mouse(mouse),
            _ => {}
        }
    }
    Ok(())
}

/// One row the cursor can rest on. Rows are per page, and render to one
/// screen line each — except comments, which take as many as they need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Something to say in place of lines: a binary file, a pure rename.
    Note(String),
    /// A hunk's `@@` header, by index into the file's hunks.
    Hunk(usize),
    /// A diff line, by hunk and line index.
    Line(usize, usize),
    /// One comment of a thread, by index into the review's threads.
    Comment {
        thread: usize,
        index: usize,
        /// False for a thread whose line is not in the diff any more; those
        /// gather at the top of the page.
        anchored: bool,
    },
}

/// The rows of one file's page: its hunks and lines, with every comment
/// placed straight after the line its thread is on.
pub fn build_rows(file: &FileDiff, threads: &[ReviewThread]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut anchored: Vec<((usize, usize), usize)> = Vec::new();
    let comments = |thread: usize, anchored: bool| {
        (0..threads[thread].comments.len()).map(move |index| Row::Comment {
            thread,
            index,
            anchored,
        })
    };
    for (i, thread) in threads.iter().enumerate() {
        if thread.path != file.path() || thread.comments.is_empty() {
            continue;
        }
        match anchor(file, thread) {
            Some(at) => anchored.push((at, i)),
            None => rows.extend(comments(i, false)),
        }
    }
    if file.binary {
        rows.push(Row::Note("binary file not shown".into()));
    } else if file.hunks.is_empty() {
        rows.push(Row::Note(match file.status {
            diff::FileStatus::Renamed => "renamed without changes".into(),
            _ => "no changes to show".into(),
        }));
    }
    for (h, hunk) in file.hunks.iter().enumerate() {
        rows.push(Row::Hunk(h));
        for l in 0..hunk.lines.len() {
            rows.push(Row::Line(h, l));
            for &(_, thread) in anchored.iter().filter(|(at, _)| *at == (h, l)) {
                rows.extend(comments(thread, true));
            }
        }
    }
    rows
}

/// The diff line a thread is on: by head line number on the right, base
/// line number on the left. None when the thread is outdated or its line is
/// outside every hunk.
fn anchor(file: &FileDiff, thread: &ReviewThread) -> Option<(usize, usize)> {
    let line = thread.line?;
    file.hunks.iter().enumerate().find_map(|(h, hunk)| {
        let l = hunk.lines.iter().position(|l| match thread.diff_side {
            github::DiffSide::Right => l.new == Some(line) && l.kind != LineKind::Removed,
            github::DiffSide::Left => l.old == Some(line) && l.kind != LineKind::Added,
        })?;
        Some((h, l))
    })
}

/// Everything fetched for the review.
#[derive(Debug, Clone)]
pub struct Review {
    pub files: Vec<FileDiff>,
    pub threads: Vec<ReviewThread>,
    /// The user's unsubmitted review, which a verdict submits.
    pub pending: Option<PendingReview>,
}

#[derive(Debug, Clone)]
pub enum Load {
    Loading,
    Failed(String),
    Ready(Box<Review>),
}

/// The summary that goes with a verdict, being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub verdict: Verdict,
    pub input: String,
}

/// A background lookup's answer, with its error put into words.
fn joined<T>(answer: std::thread::Result<Result<T>>) -> Result<T, String> {
    match answer {
        Ok(inner) => inner.map_err(|e| format!("{e:#}")),
        Err(_) => Err("the lookup stopped".to_string()),
    }
}

/// Work for a background thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Submit {
        draft: Draft,
        pending: Option<PendingReview>,
    },
}

/// A background thread's answer.
enum Msg {
    Loaded {
        diff: Result<String, String>,
        threads: Result<Vec<ReviewThread>, String>,
        pending: Result<Option<PendingReview>, String>,
    },
    Submitted {
        draft: Draft,
        result: Result<(), String>,
    },
}

pub struct Viewer {
    pub pr: PullRequest,
    /// Where `gh` runs: a checkout of the PR's repository.
    pub dir: PathBuf,
    pub load: Load,
    /// Index of the file on screen.
    pub page: usize,
    pub rows: Vec<Row>,
    /// Index into `rows`.
    pub cursor: usize,
    /// First row on screen, kept here so the view only scrolls when the
    /// cursor would leave it. Set by the renderer.
    pub scroll: usize,
    /// Cursor and scroll left behind on each page, so coming back to a file
    /// finds the place you were reading.
    marks: Vec<(usize, usize)>,
    /// How many rows the last frame had room for, for PgUp/PgDn.
    pub page_rows: usize,
    pub draft: Option<Draft>,
    pub status: String,
    /// Set while a verdict is being submitted.
    pub busy: Option<String>,
    pub palette: Palette,
    pub submitted: Option<Verdict>,
    pub exit: Option<Exit>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Background threads yet to answer.
    jobs: usize,
}

impl Viewer {
    pub fn new(dir: PathBuf, pr: PullRequest, palette: Palette) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            pr,
            dir,
            load: Load::Loading,
            page: 0,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            marks: Vec::new(),
            page_rows: 20,
            draft: None,
            status: String::new(),
            busy: None,
            palette,
            submitted: None,
            exit: None,
            tx,
            rx,
            jobs: 0,
        }
    }

    pub fn review(&self) -> Option<&Review> {
        match &self.load {
            Load::Ready(review) => Some(review),
            _ => None,
        }
    }

    pub fn file(&self) -> Option<&FileDiff> {
        self.review()?.files.get(self.page)
    }

    /// Fetch the diff, the threads and the pending review, side by side.
    fn start_load(&mut self) {
        let dir = self.dir.clone();
        let number = self.pr.number;
        let tx = self.tx.clone();
        self.jobs += 1;
        std::thread::spawn(move || {
            let (diff, threads, pending) = std::thread::scope(|scope| {
                let threads = scope.spawn(|| github::review_threads(&dir, number));
                let pending = scope.spawn(|| github::pending_review(&dir, number));
                let diff = github::pr_diff(&dir, number);
                (
                    diff.map_err(|e| format!("{e:#}")),
                    joined(threads.join()),
                    joined(pending.join()),
                )
            });
            let _ = tx.send(Msg::Loaded {
                diff,
                threads,
                pending,
            });
        });
    }

    fn spawn(&mut self, job: Job) {
        let dir = self.dir.clone();
        let number = self.pr.number;
        let tx = self.tx.clone();
        self.jobs += 1;
        match job {
            Job::Submit { draft, pending } => {
                std::thread::spawn(move || {
                    let body = draft.input.trim().to_string();
                    let result =
                        github::submit_review(&dir, number, draft.verdict, &body, pending.as_ref())
                            .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Submitted { draft, result });
                });
            }
        }
    }

    /// Apply whatever the background threads have finished.
    fn take_updates(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.jobs = self.jobs.saturating_sub(1);
            match msg {
                Msg::Loaded {
                    diff,
                    threads,
                    pending,
                } => self.loaded(diff, threads, pending),
                Msg::Submitted { draft, result } => self.submitted(draft, result),
            }
        }
    }

    /// Take the fetched review in. Only the diff is essential: without the
    /// comments or the pending review there is still a diff worth reading,
    /// and the status line says what is missing.
    fn loaded(
        &mut self,
        diff: Result<String, String>,
        threads: Result<Vec<ReviewThread>, String>,
        pending: Result<Option<PendingReview>, String>,
    ) {
        let text = match diff {
            Ok(text) => text,
            Err(why) => {
                self.load = Load::Failed(why);
                return;
            }
        };
        let mut problems = Vec::new();
        let threads = threads.unwrap_or_else(|why| {
            problems.push(format!("could not read the comments: {why}"));
            Vec::new()
        });
        let pending = pending.unwrap_or_else(|why| {
            problems.push(format!("could not check for a pending review: {why}"));
            None
        });
        let files = diff::parse(&text);
        self.marks = vec![(0, 0); files.len()];
        self.load = Load::Ready(Box::new(Review {
            files,
            threads,
            pending,
        }));
        self.status = problems.join(" · ");
        self.show_page(0);
    }

    fn submitted(&mut self, draft: Draft, result: Result<(), String>) {
        self.busy = None;
        match result {
            Ok(()) => {
                self.status = super::submitted_label(draft.verdict, self.pr.number);
                self.submitted = Some(draft.verdict);
                // The verdict went out with the pending review, if there was one.
                if let Load::Ready(review) = &mut self.load {
                    review.pending = None;
                }
            }
            Err(why) => {
                // Back to the text, so a rejected verdict does not cost it.
                self.status = format!("not submitted: {why}");
                self.draft = Some(draft);
            }
        }
    }

    /// Turn to file `page`, leaving a mark on the one being left.
    fn show_page(&mut self, page: usize) {
        if let Some(mark) = self.marks.get_mut(self.page) {
            *mark = (self.cursor, self.scroll);
        }
        let rows = match self.review() {
            Some(review) => match review.files.get(page) {
                Some(file) => build_rows(file, &review.threads),
                None => Vec::new(),
            },
            None => return,
        };
        self.page = page;
        self.rows = rows;
        let (cursor, scroll) = self.marks.get(page).copied().unwrap_or((0, 0));
        self.cursor = cursor.min(self.rows.len().saturating_sub(1));
        self.scroll = scroll.min(self.cursor);
    }

    fn turn(&mut self, delta: isize) {
        let Some(files) = self.review().map(|r| r.files.len()) else {
            return;
        };
        let target = self.page as isize + delta;
        if target < 0 {
            self.status = "this is the first file".into();
        } else if target as usize >= files {
            self.status = "this is the last file".into();
        } else {
            self.status.clear();
            self.show_page(target as usize);
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last.max(0)) as usize;
    }

    fn start_draft(&mut self, verdict: Verdict) {
        if self.busy.is_some() {
            self.status = "still submitting the last verdict…".into();
            return;
        }
        match &self.load {
            Load::Ready(_) => {
                self.draft = Some(Draft {
                    verdict,
                    input: String::new(),
                })
            }
            Load::Loading => self.status = "still loading the pull request…".into(),
            Load::Failed(_) => {
                self.status = "the pull request did not load, so there is nothing to review".into()
            }
        }
    }

    /// Check the draft can go, and hand it over to be submitted if so.
    fn submit(&mut self, draft: Draft) -> Option<Job> {
        let pending = self.review().and_then(|r| r.pending.clone());
        if super::needs_body(draft.verdict, &draft.input, pending.is_some()) {
            self.status = format!("{} needs a comment to go with it", draft.verdict.label());
            self.draft = Some(draft);
            return None;
        }
        self.busy = Some(format!(
            "submitting: {} #{}",
            draft.verdict.label(),
            self.pr.number
        ));
        Some(Job::Submit { draft, pending })
    }

    /// Act on a key, returning work for a background thread if it made some.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Job> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.exit = Some(Exit::Quit);
            return None;
        }
        if let Some(mut draft) = self.draft.take() {
            match super::edit_review_body(&mut draft.input, key) {
                BodyKey::Cancel => {}
                BodyKey::Submit => return self.submit(draft),
                BodyKey::Typing => self.draft = Some(draft),
            }
            return None;
        }
        let page = self.page_rows.max(1) as isize;
        // Some terminals report a capital as its lowercase letter with shift.
        let code = match key.code {
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                KeyCode::Char(c.to_ascii_uppercase())
            }
            code => code,
        };
        match code {
            KeyCode::Esc => {
                if self.busy.is_some() {
                    self.status = "still submitting — wait for GitHub to answer".into();
                } else {
                    self.exit = Some(Exit::Back);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.move_cursor(page),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.move_cursor(isize::MAX / 2),
            KeyCode::Left | KeyCode::Char('h') => self.turn(-1),
            KeyCode::Right | KeyCode::Char('l') => self.turn(1),
            KeyCode::Char('A') => self.start_draft(Verdict::Approve),
            KeyCode::Char('C') => self.start_draft(Verdict::Comment),
            KeyCode::Char('X') => self.start_draft(Verdict::RequestChanges),
            _ => {}
        }
        None
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.draft.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::ScrollDown => self.move_cursor(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn pr() -> PullRequest {
        PullRequest {
            number: 42,
            title: "Widen the store size".into(),
            url: "https://github.com/acme/widget/pull/42".into(),
            state: "OPEN".into(),
            is_draft: false,
            review_decision: String::new(),
            head_ref_name: "wide".into(),
            head_ref_oid: "abc".into(),
            base_ref_name: "main".into(),
            is_cross_repository: false,
        }
    }

    pub(super) fn threads() -> Vec<ReviewThread> {
        // The fixture's own pages, as `gh api --slurp` gives them.
        let text = include_str!("../../../tests/fixtures/review_threads.json");
        let pages: Vec<serde_json::Value> = serde_json::from_str(text).unwrap();
        pages
            .iter()
            .flat_map(|p| {
                p["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .map(|t| serde_json::from_value(t).unwrap())
            .collect()
    }

    /// A viewer with the fixture PR loaded, as if `gh` had answered.
    pub(super) fn loaded_viewer(pending: Option<PendingReview>) -> Viewer {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Ok(threads()),
            Ok(pending),
        );
        viewer
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn comments_follow_the_line_their_thread_is_on() {
        let viewer = loaded_viewer(None);
        let rows = &viewer.rows;
        // The outdated thread has no line, so it heads the page.
        assert_eq!(
            rows[0],
            Row::Comment {
                thread: 2,
                index: 0,
                anchored: false
            }
        );
        assert_eq!(rows[1], Row::Hunk(0));
        // Thread 0 is on head line 12: the second added line.
        let at = rows.iter().position(|r| *r == Row::Line(0, 5)).unwrap();
        assert_eq!(
            &rows[at + 1..at + 3],
            &[
                Row::Comment {
                    thread: 0,
                    index: 0,
                    anchored: true
                },
                Row::Comment {
                    thread: 0,
                    index: 1,
                    anchored: true
                },
            ]
        );
        assert_eq!(rows[at + 3], Row::Line(0, 6));
        let comments = rows
            .iter()
            .filter(|r| matches!(r, Row::Comment { .. }))
            .count();
        assert_eq!(comments, 3, "the other file's thread is not on this page");
    }

    #[test]
    fn a_left_side_thread_sits_under_the_removed_line() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(viewer.page, 1);
        let at = viewer
            .rows
            .iter()
            .position(|r| *r == Row::Line(0, 2))
            .unwrap();
        assert!(matches!(
            viewer.rows[at + 1],
            Row::Comment {
                thread: 1,
                anchored: true,
                ..
            }
        ));
    }

    #[test]
    fn files_without_lines_say_why() {
        let mut viewer = loaded_viewer(None);
        for _ in 0..3 {
            viewer.handle_key(key(KeyCode::Right));
        }
        assert_eq!(viewer.rows, [Row::Note("binary file not shown".into())]);
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(viewer.rows, [Row::Note("renamed without changes".into())]);
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(viewer.page, 4, "there is no sixth file");
        assert_eq!(viewer.status, "this is the last file");
    }

    #[test]
    fn paging_back_returns_to_where_you_were() {
        let mut viewer = loaded_viewer(None);
        for _ in 0..4 {
            viewer.handle_key(key(KeyCode::Down));
        }
        assert_eq!(viewer.cursor, 4);
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(viewer.cursor, 0, "a new page starts at the top");
        viewer.handle_key(key(KeyCode::Left));
        assert_eq!(viewer.page, 0);
        assert_eq!(viewer.cursor, 4);
        viewer.handle_key(key(KeyCode::Left));
        assert_eq!(viewer.status, "this is the first file");
    }

    #[test]
    fn the_cursor_stays_on_the_page() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Up));
        assert_eq!(viewer.cursor, 0);
        viewer.handle_key(key(KeyCode::End));
        assert_eq!(viewer.cursor, viewer.rows.len() - 1);
        viewer.handle_key(key(KeyCode::Down));
        assert_eq!(viewer.cursor, viewer.rows.len() - 1);
        viewer.handle_key(key(KeyCode::Home));
        assert_eq!(viewer.cursor, 0);
        viewer.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(viewer.cursor, 1, "the wheel moves the cursor");
    }

    #[test]
    fn a_verdict_is_typed_then_submitted() {
        let mut viewer = loaded_viewer(None);
        assert_eq!(viewer.handle_key(key(KeyCode::Char('X'))), None);
        assert_eq!(
            viewer.draft.as_ref().map(|d| d.verdict),
            Some(Verdict::RequestChanges)
        );
        // Letters are text now, not commands.
        viewer.handle_key(key(KeyCode::Char('l')));
        assert_eq!(viewer.page, 0);
        // Nothing to say is refused before the round trip.
        viewer.handle_key(key(KeyCode::Backspace));
        assert_eq!(viewer.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(
            viewer.status,
            "request changes needs a comment to go with it"
        );
        assert!(viewer.draft.is_some());

        for c in "use u64".chars() {
            viewer.handle_key(key(KeyCode::Char(c)));
        }
        let job = viewer.handle_key(key(KeyCode::Enter));
        assert_eq!(
            job,
            Some(Job::Submit {
                draft: Draft {
                    verdict: Verdict::RequestChanges,
                    input: "use u64".into()
                },
                pending: None
            })
        );
        assert!(viewer.busy.is_some());
        assert_eq!(viewer.handle_key(key(KeyCode::Char('A'))), None);
        assert!(viewer.draft.is_none(), "one verdict at a time");
    }

    #[test]
    fn a_shifted_lowercase_letter_is_its_capital() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT));
        assert_eq!(viewer.draft.map(|d| d.verdict), Some(Verdict::Approve));
    }

    #[test]
    fn approving_with_a_pending_review_submits_it() {
        let pending = PendingReview {
            id: 7,
            node_id: "PRR_x".into(),
            comments: 3,
        };
        let mut viewer = loaded_viewer(Some(pending.clone()));
        viewer.handle_key(key(KeyCode::Char('A')));
        let job = viewer.handle_key(key(KeyCode::Enter));
        assert_eq!(
            job,
            Some(Job::Submit {
                draft: Draft {
                    verdict: Verdict::Approve,
                    input: String::new()
                },
                pending: Some(pending)
            })
        );
        viewer.submitted(
            Draft {
                verdict: Verdict::Approve,
                input: String::new(),
            },
            Ok(()),
        );
        assert_eq!(viewer.submitted, Some(Verdict::Approve));
        assert_eq!(viewer.status, "approved #42");
        assert!(viewer.review().unwrap().pending.is_none());
    }

    #[test]
    fn a_rejected_verdict_keeps_its_text() {
        let mut viewer = loaded_viewer(None);
        let draft = Draft {
            verdict: Verdict::Comment,
            input: "nice".into(),
        };
        viewer.busy = Some("submitting".into());
        viewer.submitted(
            draft.clone(),
            Err("Can not approve your own pull request".into()),
        );
        assert_eq!(viewer.draft, Some(draft));
        assert!(viewer.busy.is_none());
        assert!(viewer.status.starts_with("not submitted: "));
        assert_eq!(viewer.submitted, None);
    }

    #[test]
    fn esc_closes_the_draft_before_the_viewer() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Char('C')));
        viewer.handle_key(key(KeyCode::Esc));
        assert!(viewer.draft.is_none());
        assert_eq!(viewer.exit, None);
        viewer.handle_key(key(KeyCode::Esc));
        assert_eq!(viewer.exit, Some(Exit::Back));

        let mut viewer = loaded_viewer(None);
        viewer.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(viewer.exit, Some(Exit::Quit));
    }

    #[test]
    fn nothing_to_review_until_it_loads() {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        viewer.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT));
        assert!(viewer.draft.is_none());
        assert_eq!(viewer.status, "still loading the pull request…");
        viewer.handle_key(key(KeyCode::Right));
        viewer.handle_key(key(KeyCode::Down));
        assert_eq!((viewer.page, viewer.cursor), (0, 0));

        viewer.loaded(Err("gh: not found".into()), Ok(Vec::new()), Ok(None));
        assert!(matches!(viewer.load, Load::Failed(_)));
    }

    #[test]
    fn missing_comments_still_show_the_diff() {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Err("HTTP 403".into()),
            Ok(None),
        );
        assert_eq!(viewer.review().unwrap().files.len(), 5);
        assert_eq!(viewer.status, "could not read the comments: HTTP 403");
        assert!(!viewer.rows.iter().any(|r| matches!(r, Row::Comment { .. })));
    }
}
