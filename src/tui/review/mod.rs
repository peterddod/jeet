//! The review viewer: a pull request's diff, one file to a page, in GitHub's
//! dark colours, with each review thread under the line it is about.
//!
//! Everything slow — the diff, the threads, the pending review, submitting a
//! verdict, every comment, reply and reaction, the syntax colours — runs on a
//! background thread and comes back over a channel, so the viewer is on
//! screen the moment it is asked for and never stops drawing while GitHub
//! thinks. A poll of the
//! PR's fingerprint notices when someone else has changed it.

pub mod actions;
pub mod diff;
mod highlight;
mod render;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use crate::github::{
    self, DiffSide, Fingerprint, PendingReview, PullRequest, Reaction, ReviewThread, ThreadComment,
    Verdict,
};
use actions::{Compose, Confirm, Job, Msg, Picker, Target};
use diff::{FileDiff, LineKind};
use render::Palette;

use super::{BodyKey, Tui};

/// How often the PR is checked for changes made elsewhere.
pub const POLL_EVERY: Duration = Duration::from_secs(30);

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
    viewer.spawn(Job::Load);
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
        viewer.update_highlights();
        if let Some(job) = viewer.tick(Instant::now()) {
            viewer.spawn(job);
        }
        terminal.draw(|frame| render::draw(frame, viewer))?;
        // Wait on input, but wake for an answer on its way or the next poll.
        let wait = match (viewer.jobs, viewer.highlights.working) {
            (0, false) => viewer.until_poll(Instant::now()),
            _ => Duration::from_millis(100),
        };
        if !event::poll(wait)? {
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
    /// A comment, reply or edit being typed.
    pub compose: Option<Compose>,
    pub picker: Option<Picker>,
    pub confirm: Option<Confirm>,
    pub status: String,
    /// Set while a change is with GitHub: one at a time.
    pub busy: Option<String>,
    pub palette: Palette,
    pub submitted: Option<Verdict>,
    pub exit: Option<Exit>,
    /// The PR as it was when the review on screen was fetched.
    fingerprint: Option<Fingerprint>,
    /// GitHub has changes the screen does not show yet.
    pub stale: bool,
    /// A fresh copy of the review is on its way; the old one stays up.
    pub refreshing: bool,
    /// When the PR is next checked for changes; None while there is no
    /// point, such as when it is already known to have some.
    next_poll: Option<Instant>,
    polling: bool,
    /// Bumped by every load, so answers to an older one — or to a poll that
    /// started before it — are recognised and dropped.
    generation: u64,
    tx: Sender<(u64, Msg)>,
    rx: Receiver<(u64, Msg)>,
    /// Background threads yet to answer.
    jobs: usize,
    /// Syntax colours for each file, worked out off the draw path.
    pub highlights: highlight::Cache,
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
            compose: None,
            picker: None,
            confirm: None,
            status: String::new(),
            busy: None,
            palette,
            submitted: None,
            exit: None,
            fingerprint: None,
            stale: false,
            refreshing: false,
            next_poll: None,
            polling: false,
            generation: 0,
            tx,
            rx,
            jobs: 0,
            highlights: highlight::Cache::new(),
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

    /// The row under the cursor.
    pub fn row(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// The comment under the cursor, with its thread.
    pub fn comment(&self) -> Option<(&ReviewThread, &ThreadComment)> {
        let Some(Row::Comment { thread, index, .. }) = self.row() else {
            return None;
        };
        let thread = self.review()?.threads.get(*thread)?;
        Some((thread, thread.comments.get(*index)?))
    }

    /// Hand `job` to a background thread.
    fn spawn(&mut self, job: Job) {
        if job == Job::Load {
            // Whatever was asked before this is about to be out of date.
            self.generation += 1;
            self.polling = false;
        }
        let dir = self.dir.clone();
        let number = self.pr.number;
        let generation = self.generation;
        let tx = self.tx.clone();
        self.jobs += 1;
        std::thread::spawn(move || {
            let msg = actions::run(&dir, number, job);
            let _ = tx.send((generation, msg));
        });
    }

    /// Apply whatever the background threads have finished, starting any
    /// work that follows from it.
    fn take_updates(&mut self) {
        while let Ok((generation, msg)) = self.rx.try_recv() {
            self.jobs = self.jobs.saturating_sub(1);
            let current = generation == self.generation;
            let next = match msg {
                Msg::Loaded { .. } | Msg::Polled(_) if !current => None,
                Msg::Loaded {
                    diff,
                    threads,
                    pending,
                    fingerprint,
                } => {
                    self.loaded(diff, threads, pending, fingerprint);
                    None
                }
                Msg::Polled(print) => {
                    self.polled(print);
                    None
                }
                Msg::Submitted { draft, result } => self.submitted(draft, result),
                Msg::Done { job, result } => self.done(job, result),
            };
            if let Some(job) = next {
                self.spawn(job);
            }
        }
    }

    /// Take the fetched review in. Only the diff is essential: without the
    /// comments or the pending review there is still a diff worth reading,
    /// and the status line says what is missing. A refresh that fails keeps
    /// what was on screen rather than trading it for an error.
    fn loaded(
        &mut self,
        diff: Result<String, String>,
        threads: Result<Vec<ReviewThread>, String>,
        pending: Result<Option<PendingReview>, String>,
        fingerprint: Result<Fingerprint, String>,
    ) {
        let refresh = self.refreshing && self.review().is_some();
        self.refreshing = false;
        self.polling = false;
        self.next_poll = Some(Instant::now() + POLL_EVERY);
        let text = match diff {
            Ok(text) => text,
            Err(why) if refresh => {
                self.status = format!("could not refresh: {why}");
                return;
            }
            Err(why) => {
                self.load = Load::Failed(why);
                self.next_poll = None;
                return;
            }
        };
        // A failed fingerprint leaves none; the next poll takes one.
        self.fingerprint = fingerprint.ok();
        self.stale = false;
        let old = match std::mem::replace(&mut self.load, Load::Loading) {
            Load::Ready(review) => Some(*review),
            _ => None,
        };
        let mut problems = Vec::new();
        let threads = threads.unwrap_or_else(|why| {
            problems.push(format!("could not read the comments: {why}"));
            old.as_ref().map(|r| r.threads.clone()).unwrap_or_default()
        });
        let pending = pending.unwrap_or_else(|why| {
            problems.push(format!("could not check for a pending review: {why}"));
            old.as_ref().and_then(|r| r.pending.clone())
        });
        // Where the cursor was, to find it again in the new rows.
        let place = old.as_ref().and_then(|review| {
            let file = review.files.get(self.page)?;
            Some(actions::place(
                file,
                &review.threads,
                &self.rows,
                self.cursor,
            ))
        });
        let files = diff::parse(&text);
        let old_files = old.as_ref().map(|r| r.files.as_slice()).unwrap_or(&[]);
        self.highlights.renew(old_files, &files);
        self.marks = vec![(0, 0); files.len()];
        self.load = Load::Ready(Box::new(Review {
            files,
            threads,
            pending,
        }));
        // A refresh leaves the status alone unless it has a problem to report:
        // it may be saying what the change that asked for it did.
        if !refresh || !problems.is_empty() {
            self.status = problems.join(" · ");
        }
        match place {
            Some(place) => self.find_place(&place),
            None => self.show_page(0),
        }
    }

    /// Put the cursor back on `place` in the review just loaded: the same
    /// file if it is still in the PR, and the same comment or line in it.
    fn find_place(&mut self, place: &actions::Place) {
        let Some(review) = self.review() else {
            return;
        };
        let last = review.files.len().saturating_sub(1);
        let (page, rows, cursor) = match review.files.iter().position(|f| f.path() == place.path) {
            Some(page) => {
                let file = &review.files[page];
                let rows = build_rows(file, &review.threads);
                let cursor = actions::find(place, file, &review.threads, &rows);
                (page, rows, cursor)
            }
            None => {
                let page = self.page.min(last);
                let rows = match review.files.get(page) {
                    Some(file) => build_rows(file, &review.threads),
                    None => Vec::new(),
                };
                (page, rows, 0)
            }
        };
        self.page = page;
        self.rows = rows;
        self.cursor = cursor;
        // The renderer scrolls only as far as it must to show the cursor.
        self.scroll = self.scroll.min(self.cursor);
    }

    fn polled(&mut self, print: Result<Fingerprint, String>) {
        self.polling = false;
        // A poll that failed says nothing either way; the next one may not.
        let Ok(print) = print else {
            return;
        };
        match &self.fingerprint {
            None => self.fingerprint = Some(print),
            Some(seen) if *seen != print => {
                self.stale = true;
                // Known to be out of date: nothing more to find out until r.
                self.next_poll = None;
            }
            Some(_) => {}
        }
    }

    /// Take in the syntax colours worked out so far, and start on the next
    /// file that wants them: the page on screen first.
    fn update_highlights(&mut self) {
        self.highlights.take();
        if let Load::Ready(review) = &self.load {
            self.highlights.start(&review.files, self.page);
        }
    }

    /// The poll to start, if one is due at `now`.
    pub fn tick(&mut self, now: Instant) -> Option<Job> {
        let due = self.next_poll.is_some_and(|at| now >= at);
        let quiet = self.polling || self.refreshing || self.busy.is_some();
        if !due || quiet || self.review().is_none() {
            return None;
        }
        self.polling = true;
        self.next_poll = Some(now + POLL_EVERY);
        Some(Job::Poll)
    }

    /// How long until the next poll is due.
    fn until_poll(&self, now: Instant) -> Duration {
        match self.next_poll {
            // Never zero: a poll held back by something else must not spin.
            Some(at) => at
                .saturating_duration_since(now)
                .max(Duration::from_millis(100)),
            None => POLL_EVERY,
        }
    }

    /// `r`: fetch the review afresh, keeping the cursor where it is.
    fn refresh(&mut self) -> Option<Job> {
        match &self.load {
            Load::Loading => {
                self.status = "still loading the pull request…".into();
                None
            }
            _ if self.refreshing => {
                self.status = "already refreshing…".into();
                None
            }
            Load::Failed(_) => {
                self.load = Load::Loading;
                self.status.clear();
                Some(Job::Load)
            }
            Load::Ready(_) => {
                self.refreshing = true;
                Some(Job::Load)
            }
        }
    }

    fn submitted(&mut self, draft: Draft, result: Result<(), String>) -> Option<Job> {
        self.busy = None;
        match result {
            Ok(()) => {
                self.status = super::submitted_label(draft.verdict, self.pr.number);
                self.submitted = Some(draft.verdict);
                // The verdict went out with the pending review, if there was one.
                if let Load::Ready(review) = &mut self.load {
                    review.pending = None;
                }
                // Its comments are published now; show them as GitHub has them.
                self.refresh_quietly()
            }
            Err(why) => {
                // Back to the text, so a rejected verdict does not cost it.
                self.status = format!("not submitted: {why}");
                self.draft = Some(draft);
                None
            }
        }
    }

    /// A comment, reply, edit, delete or reaction has been answered.
    fn done(&mut self, job: Job, result: Result<Option<PendingReview>, String>) -> Option<Job> {
        self.busy = None;
        match result {
            Ok(joined) => {
                // An inline comment may have just started the pending review.
                // Know it now, not when the refresh lands — which may be after
                // a verdict is typed, or never if the refresh fails — so the
                // verdict submits it rather than trying to start another.
                if let (Some(pending), Load::Ready(review)) = (joined, &mut self.load) {
                    review.pending = Some(pending);
                }
                self.status = job.done_label();
                self.refresh_quietly()
            }
            Err(why) => {
                self.status = job.failed_label(&why);
                // Back to the text, so a refusal does not cost it.
                if let Job::Send(compose) = job {
                    self.compose = Some(compose);
                }
                None
            }
        }
    }

    /// A refresh after a change of our own, which leaves the status alone.
    /// It goes ahead even with one already on its way: that one may have
    /// left before the change landed, and the new load supersedes it.
    fn refresh_quietly(&mut self) -> Option<Job> {
        match self.load {
            Load::Ready(_) => {
                self.refreshing = true;
                Some(Job::Load)
            }
            Load::Loading | Load::Failed(_) => None,
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

    /// Whether there is a review to act on and nothing else in flight,
    /// saying why not on the status line.
    fn can_act(&mut self) -> bool {
        if self.busy.is_some() {
            self.status = "wait for GitHub to answer first".into();
            return false;
        }
        match &self.load {
            Load::Ready(_) => true,
            Load::Loading => {
                self.status = "still loading the pull request…".into();
                false
            }
            Load::Failed(_) => {
                self.status = "the pull request did not load, so there is nothing to review".into();
                false
            }
        }
    }

    fn start_draft(&mut self, verdict: Verdict) {
        if self.can_act() {
            self.draft = Some(Draft {
                verdict,
                input: String::new(),
            });
        }
    }

    /// Check the draft can go, and hand it over to be submitted if so.
    fn submit(&mut self, draft: Draft) -> Option<Job> {
        let pending = self.review().and_then(|r| r.pending.clone());
        // Without a pending review on screen, a refresh on its way may yet
        // bring one — after a change of ours, say — and a verdict sent
        // without it would try to start a second review GitHub refuses.
        if pending.is_none() && self.refreshing {
            self.status = "refreshing — submit again once it is done".into();
            self.draft = Some(draft);
            return None;
        }
        if super::needs_body(draft.verdict, &draft.input, pending.is_some()) {
            self.status = format!("{} needs a comment to go with it", draft.verdict.label());
            self.draft = Some(draft);
            return None;
        }
        self.start(Job::Submit { draft, pending })
    }

    /// Mark `job` as in flight, and hand it back to be spawned.
    fn start(&mut self, job: Job) -> Option<Job> {
        self.busy = job.busy_label(self.pr.number);
        Some(job)
    }

    /// `c`: a new comment on the line under the cursor.
    fn start_inline(&mut self) {
        if !self.can_act() {
            return;
        }
        let target = match (self.row(), self.file()) {
            (Some(Row::Line(h, l)), Some(file)) => {
                let line = &file.hunks[*h].lines[*l];
                // A removed line is only in the base; anything else is
                // commented on as the head has it, as GitHub does.
                let (side, number) = match line.kind {
                    LineKind::Removed => (DiffSide::Left, line.old),
                    _ => (DiffSide::Right, line.new),
                };
                number.map(|line| Target::Line {
                    path: file.path().to_string(),
                    line,
                    side,
                })
            }
            (Some(Row::Comment { .. }), _) => {
                self.status = "that is a comment: R replies to it".into();
                return;
            }
            _ => None,
        };
        match target {
            Some(target) => {
                self.compose = Some(Compose {
                    target,
                    text: String::new(),
                })
            }
            None => self.status = "move to a line of code to comment on it".into(),
        }
    }

    /// `R`: a reply to the thread under the cursor.
    fn start_reply(&mut self) {
        if !self.can_act() {
            return;
        }
        let Some((thread, _)) = self.comment() else {
            self.status = "move to a comment to reply to it".into();
            return;
        };
        // GitHub takes replies to a thread's first comment only.
        let Some(first) = thread.comments.first() else {
            return;
        };
        let target = Target::Reply {
            to: first.database_id,
            author: first.author_login().to_string(),
        };
        self.compose = Some(Compose {
            target,
            text: String::new(),
        });
    }

    /// `+`: the reaction picker, on the comment under the cursor.
    fn open_picker(&mut self) {
        if !self.can_act() {
            return;
        }
        let Some((_, comment)) = self.comment() else {
            self.status = "move to a comment to react to it".into();
            return;
        };
        let mine = comment
            .reaction_groups
            .iter()
            .filter(|g| g.viewer_has_reacted)
            .filter_map(|g| g.reaction())
            .collect();
        self.picker = Some(Picker {
            comment: comment.database_id,
            selected: 0,
            mine,
        });
    }

    /// `e`: new text for the comment under the cursor, if it is the user's.
    fn start_edit(&mut self) {
        if !self.can_act() {
            return;
        }
        let Some((_, comment)) = self.comment() else {
            self.status = "move to a comment of yours to edit it".into();
            return;
        };
        if !comment.viewer_can_update {
            self.status = "you can only edit your own comments".into();
            return;
        }
        let target = Target::Edit {
            comment: comment.database_id,
            original: comment.body.clone(),
        };
        let text = comment.body.clone();
        self.compose = Some(Compose { target, text });
    }

    /// `d`: ask before deleting the comment under the cursor.
    fn start_delete(&mut self) {
        if !self.can_act() {
            return;
        }
        let Some((_, comment)) = self.comment() else {
            self.status = "move to a comment of yours to delete it".into();
            return;
        };
        if !comment.viewer_can_delete {
            self.status = "you can only delete your own comments".into();
            return;
        }
        self.confirm = Some(Confirm {
            comment: comment.database_id,
            preview: comment.body.lines().next().unwrap_or("").to_string(),
        });
    }

    /// Check the comment being typed can go, and hand it over if so.
    fn send(&mut self, compose: Compose) -> Option<Job> {
        if compose.text.trim().is_empty() {
            self.status = "nothing to send yet".into();
            self.compose = Some(compose);
            return None;
        }
        if let Target::Edit { original, .. } = &compose.target {
            if compose.text.trim() == original.trim() {
                self.status = "the comment is unchanged".into();
                return None;
            }
        }
        self.status.clear();
        self.start(Job::Send(compose))
    }

    fn picker_key(&mut self, mut picker: Picker, key: KeyEvent) -> Option<Job> {
        let count = Reaction::ALL.len();
        match key.code {
            KeyCode::Esc => return None,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                picker.selected = (picker.selected + count - 1) % count
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                picker.selected = (picker.selected + 1) % count
            }
            KeyCode::Char(c @ '1'..='8') => {
                picker.selected = c as usize - '1' as usize;
                return self.react(picker);
            }
            KeyCode::Enter => return self.react(picker),
            _ => {}
        }
        self.picker = Some(picker);
        None
    }

    fn react(&mut self, picker: Picker) -> Option<Job> {
        let reaction = Reaction::ALL[picker.selected];
        if picker.mine.contains(&reaction) {
            self.status = format!("you have already reacted with {}", reaction.emoji());
            return None;
        }
        self.start(Job::React {
            comment: picker.comment,
            reaction,
        })
    }

    fn confirm_key(&mut self, confirm: Confirm, key: KeyEvent) -> Option<Job> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.start(Job::Delete {
                comment: confirm.comment,
            }),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => None,
            // Anything else is not an answer: ask again.
            _ => {
                self.confirm = Some(confirm);
                None
            }
        }
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
        if let Some(mut compose) = self.compose.take() {
            match super::edit_review_body(&mut compose.text, key) {
                BodyKey::Cancel => {}
                BodyKey::Submit => return self.send(compose),
                BodyKey::Typing => self.compose = Some(compose),
            }
            return None;
        }
        if let Some(picker) = self.picker.take() {
            return self.picker_key(picker, key);
        }
        if let Some(confirm) = self.confirm.take() {
            return self.confirm_key(confirm, key);
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
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.busy.is_some() {
                    self.status = "still waiting on GitHub — let it answer first".into();
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
            KeyCode::Char('c') => self.start_inline(),
            KeyCode::Char('R') => self.start_reply(),
            KeyCode::Char('+') => self.open_picker(),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('d') => self.start_delete(),
            KeyCode::Char('r') => return self.refresh(),
            _ => {}
        }
        None
    }

    /// Whether a box is open over the diff, taking the keys.
    pub fn overlay_open(&self) -> bool {
        self.draft.is_some()
            || self.compose.is_some()
            || self.picker.is_some()
            || self.confirm.is_some()
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.overlay_open() {
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
            Ok(print()),
        );
        viewer
    }

    /// The fingerprint fixture, as the PR stood when it was loaded.
    pub(super) fn print() -> Fingerprint {
        Fingerprint {
            head_ref_oid: "5f13c0ffee".into(),
            updated_at: "2026-10-09T08:00:00Z".into(),
            comments: 6,
        }
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
    fn q_backs_out_like_esc_but_types_into_a_box() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Char('C')));
        viewer.handle_key(key(KeyCode::Char('q')));
        assert_eq!(viewer.exit, None);
        assert_eq!(viewer.draft.as_ref().map(|d| d.input.as_str()), Some("q"));
        viewer.handle_key(key(KeyCode::Esc));
        assert!(viewer.draft.is_none());
        viewer.handle_key(key(KeyCode::Char('q')));
        assert_eq!(viewer.exit, Some(Exit::Back));

        // Not while a request is out, as with esc.
        let mut viewer = loaded_viewer(None);
        viewer.busy = Some("posting".into());
        viewer.handle_key(key(KeyCode::Char('q')));
        assert_eq!(viewer.exit, None);
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

        viewer.loaded(
            Err("gh: not found".into()),
            Ok(Vec::new()),
            Ok(None),
            Err("gh: not found".into()),
        );
        assert!(matches!(viewer.load, Load::Failed(_)));
    }

    #[test]
    fn missing_comments_still_show_the_diff() {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Err("HTTP 403".into()),
            Ok(None),
            Ok(print()),
        );
        assert_eq!(viewer.review().unwrap().files.len(), 5);
        assert_eq!(viewer.status, "could not read the comments: HTTP 403");
        assert!(!viewer.rows.iter().any(|r| matches!(r, Row::Comment { .. })));
    }

    /// Put the cursor on the first row matching `row`.
    fn go_to(viewer: &mut Viewer, row: &Row) {
        viewer.cursor = viewer.rows.iter().position(|r| r == row).unwrap();
    }

    fn comment_row(thread: usize, index: usize) -> Row {
        Row::Comment {
            thread,
            index,
            anchored: true,
        }
    }

    fn type_text(viewer: &mut Viewer, text: &str) {
        for c in text.chars() {
            assert_eq!(viewer.handle_key(key(KeyCode::Char(c))), None);
        }
    }

    #[test]
    fn c_on_a_line_comments_on_it() {
        let mut viewer = loaded_viewer(None);
        // The second added line: head line 12.
        go_to(&mut viewer, &Row::Line(0, 5));
        viewer.handle_key(key(KeyCode::Char('c')));
        let target = Target::Line {
            path: "src/lib.rs".into(),
            line: 12,
            side: DiffSide::Right,
        };
        assert_eq!(viewer.compose.as_ref().map(|c| &c.target), Some(&target));
        // Keys are text now: `l` does not turn the page.
        type_text(&mut viewer, "label this");
        assert_eq!(viewer.page, 0);
        let job = viewer.handle_key(key(KeyCode::Enter));
        assert_eq!(
            job,
            Some(Job::Send(Compose {
                target,
                text: "label this".into()
            }))
        );
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.busy.as_deref(), Some("adding your comment…"));

        // A removed line is commented on as the base has it.
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &Row::Line(0, 3));
        viewer.handle_key(key(KeyCode::Char('c')));
        assert_eq!(
            viewer.compose.map(|c| c.target),
            Some(Target::Line {
                path: "src/lib.rs".into(),
                line: 11,
                side: DiffSide::Left,
            })
        );
    }

    #[test]
    fn c_anywhere_else_says_where_to_go() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &Row::Hunk(0));
        viewer.handle_key(key(KeyCode::Char('c')));
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.status, "move to a line of code to comment on it");
        go_to(&mut viewer, &comment_row(0, 0));
        viewer.handle_key(key(KeyCode::Char('c')));
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.status, "that is a comment: R replies to it");
    }

    #[test]
    fn an_empty_comment_is_not_sent() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &Row::Line(0, 5));
        viewer.handle_key(key(KeyCode::Char('c')));
        type_text(&mut viewer, "  ");
        assert_eq!(viewer.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(viewer.status, "nothing to send yet");
        assert!(viewer.compose.is_some(), "still typing");
        assert!(viewer.busy.is_none());
        viewer.handle_key(key(KeyCode::Esc));
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.exit, None, "esc closed the box, not the viewer");
    }

    #[test]
    fn a_reply_goes_to_the_threads_first_comment() {
        let mut viewer = loaded_viewer(None);
        // On the second comment of the thread ada opened.
        go_to(&mut viewer, &comment_row(0, 1));
        viewer.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT));
        let target = Target::Reply {
            to: 1001,
            author: "ada".into(),
        };
        assert_eq!(viewer.compose.as_ref().map(|c| &c.target), Some(&target));
        type_text(&mut viewer, "agreed");
        assert_eq!(
            viewer.handle_key(key(KeyCode::Enter)),
            Some(Job::Send(Compose {
                target,
                text: "agreed".into()
            }))
        );

        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &Row::Line(0, 5));
        viewer.handle_key(key(KeyCode::Char('R')));
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.status, "move to a comment to reply to it");
    }

    #[test]
    fn only_your_own_comments_can_be_edited_or_deleted() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &comment_row(0, 0));
        viewer.handle_key(key(KeyCode::Char('e')));
        assert!(viewer.compose.is_none());
        assert_eq!(viewer.status, "you can only edit your own comments");
        viewer.handle_key(key(KeyCode::Char('d')));
        assert!(viewer.confirm.is_none());
        assert_eq!(viewer.status, "you can only delete your own comments");
    }

    #[test]
    fn editing_starts_from_the_comment_as_it_is() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &comment_row(0, 1));
        viewer.handle_key(key(KeyCode::Char('e')));
        assert_eq!(
            viewer.compose.as_ref().map(|c| c.text.as_str()),
            Some("Good catch, fixed.")
        );
        // Sending it unchanged is a no-op, not a round trip.
        assert_eq!(viewer.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(viewer.status, "the comment is unchanged");
        assert!(viewer.compose.is_none());

        viewer.handle_key(key(KeyCode::Char('e')));
        viewer.handle_key(key(KeyCode::Backspace));
        type_text(&mut viewer, " in 5f13c0f.");
        assert_eq!(
            viewer.handle_key(key(KeyCode::Enter)),
            Some(Job::Send(Compose {
                target: Target::Edit {
                    comment: 1002,
                    original: "Good catch, fixed.".into()
                },
                text: "Good catch, fixed in 5f13c0f.".into()
            }))
        );
        assert_eq!(viewer.busy.as_deref(), Some("saving your edit…"));
    }

    #[test]
    fn a_delete_waits_for_a_yes() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &comment_row(0, 1));
        let cursor = viewer.cursor;
        viewer.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            viewer.confirm,
            Some(Confirm {
                comment: 1002,
                preview: "Good catch, fixed.".into()
            })
        );
        // Not an answer: the question stays.
        assert_eq!(viewer.handle_key(key(KeyCode::Down)), None);
        assert!(viewer.confirm.is_some());
        assert_eq!(viewer.cursor, cursor, "and the key did nothing else");
        assert_eq!(viewer.handle_key(key(KeyCode::Char('n'))), None);
        assert!(viewer.confirm.is_none());

        viewer.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            viewer.handle_key(key(KeyCode::Char('y'))),
            Some(Job::Delete { comment: 1002 })
        );
        assert!(viewer.confirm.is_none());
        assert_eq!(viewer.busy.as_deref(), Some("deleting the comment…"));
        // One change at a time.
        viewer.handle_key(key(KeyCode::Char('d')));
        assert!(viewer.confirm.is_none());
        assert_eq!(viewer.status, "wait for GitHub to answer first");
    }

    #[test]
    fn the_picker_leaves_a_reaction() {
        let mut viewer = loaded_viewer(None);
        // ada's comment, which the user has already given a 👍.
        go_to(&mut viewer, &comment_row(0, 0));
        viewer.handle_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT));
        let picker = viewer.picker.clone().unwrap();
        assert_eq!(picker.comment, 1001);
        assert_eq!(picker.mine, [Reaction::ThumbsUp]);
        assert_eq!(viewer.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(viewer.status, "you have already reacted with 👍");
        assert!(viewer.picker.is_none());

        viewer.handle_key(key(KeyCode::Char('+')));
        viewer.handle_key(key(KeyCode::Left));
        assert_eq!(viewer.picker.as_ref().unwrap().selected, 7, "it wraps");
        viewer.handle_key(key(KeyCode::Right));
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(
            viewer.handle_key(key(KeyCode::Enter)),
            Some(Job::React {
                comment: 1001,
                reaction: Reaction::ThumbsDown
            })
        );
        viewer.busy = None;
        viewer.handle_key(key(KeyCode::Char('+')));
        assert_eq!(
            viewer.handle_key(key(KeyCode::Char('7'))),
            Some(Job::React {
                comment: 1001,
                reaction: Reaction::Rocket
            })
        );
        viewer.busy = None;
        viewer.handle_key(key(KeyCode::Char('+')));
        viewer.handle_key(key(KeyCode::Esc));
        assert!(viewer.picker.is_none());
        assert_eq!(viewer.exit, None);
    }

    #[test]
    fn a_refused_comment_keeps_its_text() {
        let mut viewer = loaded_viewer(None);
        let compose = Compose {
            target: Target::Reply {
                to: 1001,
                author: "ada".into(),
            },
            text: "agreed".into(),
        };
        viewer.busy = Some("replying…".into());
        let next = viewer.done(Job::Send(compose.clone()), Err("HTTP 422".into()));
        assert_eq!(next, None);
        assert_eq!(viewer.compose, Some(compose));
        assert!(viewer.busy.is_none());
        assert_eq!(viewer.status, "could not reply: HTTP 422");
    }

    #[test]
    fn a_change_that_went_through_refetches_the_review() {
        let mut viewer = loaded_viewer(None);
        viewer.busy = Some("deleting…".into());
        let next = viewer.done(Job::Delete { comment: 1002 }, Ok(None));
        assert_eq!(next, Some(Job::Load));
        assert!(viewer.refreshing);
        assert_eq!(viewer.status, "comment deleted");
        // Even over a refresh already on its way, which may predate it.
        assert_eq!(
            viewer.done(
                Job::React {
                    comment: 1001,
                    reaction: Reaction::Eyes
                },
                Ok(None)
            ),
            Some(Job::Load)
        );
        // A verdict publishes the pending comments, so it refetches too.
        let mut viewer = loaded_viewer(None);
        let draft = Draft {
            verdict: Verdict::Comment,
            input: "ok".into(),
        };
        assert_eq!(viewer.submitted(draft, Ok(())), Some(Job::Load));
    }

    /// A comment on the line under the cursor, as `c` would send it.
    fn inline_comment() -> Job {
        Job::Send(Compose {
            target: Target::Line {
                path: "src/lib.rs".into(),
                line: 12,
                side: github::DiffSide::Right,
            },
            text: "nit".into(),
        })
    }

    #[test]
    fn a_verdict_after_an_inline_comment_submits_the_review_it_started() {
        // No pending review yet: the comment starts one.
        let mut viewer = loaded_viewer(None);
        let started = PendingReview {
            id: 8,
            node_id: "PRR_new".into(),
            comments: 1,
        };
        viewer.busy = Some("adding your comment…".into());
        let next = viewer.done(inline_comment(), Ok(Some(started.clone())));
        assert_eq!(next, Some(Job::Load));
        assert!(viewer.refreshing);
        // Approved before the refresh is back: it submits that review,
        // rather than starting a second one GitHub would refuse.
        viewer.handle_key(key(KeyCode::Char('A')));
        let approve = viewer.handle_key(key(KeyCode::Enter));
        assert_eq!(
            approve,
            Some(Job::Submit {
                draft: Draft {
                    verdict: Verdict::Approve,
                    input: String::new()
                },
                pending: Some(started.clone())
            })
        );

        // And a refresh that fails does not lose it.
        let mut viewer = loaded_viewer(None);
        viewer.done(inline_comment(), Ok(Some(started.clone())));
        viewer.loaded(
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
        );
        assert_eq!(viewer.status, "could not refresh: HTTP 502");
        // Comment with nothing to add is fine: the review has a comment.
        viewer.handle_key(key(KeyCode::Char('C')));
        let comment = viewer.handle_key(key(KeyCode::Enter));
        assert_eq!(
            comment,
            Some(Job::Submit {
                draft: Draft {
                    verdict: Verdict::Comment,
                    input: String::new()
                },
                pending: Some(started)
            })
        );
    }

    #[test]
    fn a_verdict_waits_for_a_refresh_that_may_bring_a_pending_review() {
        let mut viewer = loaded_viewer(None);
        // The comment went in, but which review it joined is not known yet.
        viewer.done(inline_comment(), Ok(None));
        assert!(viewer.refreshing);
        viewer.handle_key(key(KeyCode::Char('C')));
        assert_eq!(viewer.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(viewer.status, "refreshing — submit again once it is done");
        assert!(viewer.busy.is_none());
        assert!(viewer.draft.is_some(), "the draft stays to send again");
        // The refresh brings the pending review; the verdict now submits it.
        let pending = PendingReview {
            id: 8,
            node_id: "PRR_new".into(),
            comments: 1,
        };
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Ok(threads()),
            Ok(Some(pending.clone())),
            Ok(print()),
        );
        assert_eq!(
            viewer.handle_key(key(KeyCode::Enter)),
            Some(Job::Submit {
                draft: Draft {
                    verdict: Verdict::Comment,
                    input: String::new()
                },
                pending: Some(pending)
            })
        );
    }

    #[test]
    fn the_pr_is_polled_for_changes_every_thirty_seconds() {
        let mut viewer = loaded_viewer(None);
        let now = Instant::now();
        assert_eq!(viewer.tick(now), None, "not due yet");
        let later = now + POLL_EVERY + Duration::from_secs(1);
        assert_eq!(viewer.tick(later), Some(Job::Poll));
        assert_eq!(viewer.tick(later), None, "one poll at a time");

        // Nothing has changed.
        viewer.polled(Ok(print()));
        assert!(!viewer.stale);
        assert_eq!(viewer.tick(later), None, "the next is thirty seconds off");
        let much_later = later + POLL_EVERY;
        assert_eq!(viewer.tick(much_later), Some(Job::Poll));

        // A failed poll says nothing.
        viewer.polled(Err("timeout".into()));
        assert!(!viewer.stale);
        assert_eq!(viewer.status, "");

        // Someone commented.
        assert_eq!(viewer.tick(much_later + POLL_EVERY), Some(Job::Poll));
        viewer.polled(Ok(Fingerprint {
            comments: 7,
            ..print()
        }));
        assert!(viewer.stale);
        assert_eq!(
            viewer.tick(much_later + POLL_EVERY * 10),
            None,
            "no more polls once there is news"
        );
        assert_eq!(viewer.until_poll(much_later), POLL_EVERY);
    }

    #[test]
    fn no_polls_while_a_change_is_with_github() {
        let mut viewer = loaded_viewer(None);
        let later = Instant::now() + POLL_EVERY * 2;
        viewer.busy = Some("replying…".into());
        assert_eq!(viewer.tick(later), None);
        assert_eq!(
            viewer.until_poll(later),
            Duration::from_millis(100),
            "a held-back poll does not spin"
        );
        viewer.busy = None;
        assert_eq!(viewer.tick(later), Some(Job::Poll));
    }

    #[test]
    fn a_missing_fingerprint_is_taken_by_the_first_poll() {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Ok(threads()),
            Ok(None),
            Err("HTTP 502".into()),
        );
        assert_eq!(viewer.status, "", "a fingerprint is not worth a warning");
        viewer.polled(Ok(print()));
        assert!(!viewer.stale);
        viewer.polled(Ok(Fingerprint {
            head_ref_oid: "new".into(),
            ..print()
        }));
        assert!(viewer.stale);
    }

    #[test]
    fn r_refetches_and_keeps_the_cursor_on_its_line() {
        let mut viewer = loaded_viewer(None);
        viewer.stale = true;
        go_to(&mut viewer, &Row::Line(0, 5));
        assert_eq!(viewer.cursor, 7);
        assert_eq!(viewer.handle_key(key(KeyCode::Char('r'))), Some(Job::Load));
        assert!(viewer.refreshing);
        assert_eq!(viewer.handle_key(key(KeyCode::Char('r'))), None);
        assert_eq!(viewer.status, "already refreshing…");

        // The outdated thread that headed the page has been resolved away,
        // so every row moves up one.
        let mut fewer = threads();
        fewer.remove(2);
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Ok(fewer),
            Ok(None),
            Ok(Fingerprint {
                comments: 5,
                ..print()
            }),
        );
        assert!(!viewer.refreshing && !viewer.stale);
        assert_eq!(viewer.rows[viewer.cursor], Row::Line(0, 5));
        assert_eq!(viewer.cursor, 6);
        assert_eq!(viewer.fingerprint.as_ref().unwrap().comments, 5);
    }

    #[test]
    fn a_refetch_follows_the_file_to_its_new_page() {
        let mut viewer = loaded_viewer(None);
        viewer.handle_key(key(KeyCode::Right));
        assert_eq!(viewer.file().unwrap().path(), "src/old.rs");
        go_to(&mut viewer, &Row::Line(0, 2));
        viewer.handle_key(key(KeyCode::Char('r')));
        // The first file has dropped out of the PR.
        let text = include_str!("../../../tests/fixtures/pr.diff");
        let rest = &text[text[1..].find("diff --git").unwrap() + 1..];
        viewer.loaded(Ok(rest.into()), Ok(threads()), Ok(None), Ok(print()));
        assert_eq!(viewer.page, 0);
        assert_eq!(viewer.file().unwrap().path(), "src/old.rs");
        assert_eq!(viewer.rows[viewer.cursor], Row::Line(0, 2));
    }

    #[test]
    fn a_failed_refetch_keeps_the_review_on_screen() {
        let mut viewer = loaded_viewer(None);
        viewer.stale = true;
        go_to(&mut viewer, &Row::Line(0, 5));
        let cursor = viewer.cursor;
        viewer.handle_key(key(KeyCode::Char('r')));
        viewer.loaded(
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
            Err("HTTP 502".into()),
        );
        assert!(viewer.review().is_some());
        assert_eq!(viewer.cursor, cursor);
        assert_eq!(viewer.status, "could not refresh: HTTP 502");
        assert!(viewer.stale, "still out of date");
        assert!(!viewer.refreshing);
        // And lost comments on a refresh keep the ones there were.
        viewer.handle_key(key(KeyCode::Char('r')));
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Err("HTTP 502".into()),
            Ok(None),
            Ok(print()),
        );
        assert_eq!(viewer.review().unwrap().threads.len(), 3);
        assert_eq!(viewer.status, "could not read the comments: HTTP 502");
    }

    #[test]
    fn r_retries_a_load_that_failed() {
        let mut viewer = Viewer::new(PathBuf::from("/nowhere"), pr(), Palette::truecolor());
        assert_eq!(viewer.handle_key(key(KeyCode::Char('r'))), None);
        assert_eq!(viewer.status, "still loading the pull request…");
        viewer.loaded(
            Err("no gh".into()),
            Ok(Vec::new()),
            Ok(None),
            Err("no gh".into()),
        );
        assert_eq!(viewer.tick(Instant::now() + POLL_EVERY * 2), None);
        assert_eq!(viewer.handle_key(key(KeyCode::Char('r'))), Some(Job::Load));
        assert!(matches!(viewer.load, Load::Loading));
    }

    #[test]
    fn answers_to_an_older_load_are_dropped() {
        let mut viewer = loaded_viewer(None);
        viewer.generation = 2;
        viewer.jobs = 3;
        // A load and a poll both started before the latest load.
        viewer
            .tx
            .send((
                1,
                Msg::Loaded {
                    diff: Err("old".into()),
                    threads: Ok(Vec::new()),
                    pending: Ok(None),
                    fingerprint: Err("old".into()),
                },
            ))
            .unwrap();
        let changed = Fingerprint {
            comments: 9,
            ..print()
        };
        viewer
            .tx
            .send((1, Msg::Polled(Ok(changed.clone()))))
            .unwrap();
        viewer.take_updates();
        assert!(viewer.review().is_some());
        assert!(!viewer.stale);
        assert_eq!(viewer.jobs, 1);
        // A current one is not.
        viewer.tx.send((2, Msg::Polled(Ok(changed)))).unwrap();
        viewer.take_updates();
        assert!(viewer.stale);
        assert_eq!(viewer.jobs, 0);
    }

    /// Run the event loop's highlighting step until `page` has its colours.
    fn wait_for_colours(viewer: &mut Viewer, page: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while viewer.highlights.get(page).is_none() {
            assert!(Instant::now() < deadline, "page {page} never highlighted");
            viewer.update_highlights();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn colours_arrive_from_the_background_and_survive_a_refresh() {
        let mut viewer = loaded_viewer(None);
        assert!(viewer.highlights.get(0).is_none(), "plain to begin with");
        wait_for_colours(&mut viewer, 0);
        // The next page is worked out ahead of being turned to.
        wait_for_colours(&mut viewer, 1);
        viewer.handle_key(key(KeyCode::Right));
        assert!(viewer.highlights.get(viewer.page).is_some());

        // A refresh that brings the same files keeps their colours.
        viewer.handle_key(key(KeyCode::Char('r')));
        viewer.loaded(
            Ok(include_str!("../../../tests/fixtures/pr.diff").to_string()),
            Ok(threads()),
            Ok(None),
            Ok(print()),
        );
        assert!(viewer.highlights.get(0).is_some());
        assert!(viewer.highlights.get(1).is_some());
    }

    #[test]
    fn the_wheel_is_ignored_under_a_box() {
        let mut viewer = loaded_viewer(None);
        go_to(&mut viewer, &comment_row(0, 0));
        let cursor = viewer.cursor;
        viewer.handle_key(key(KeyCode::Char('+')));
        viewer.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(viewer.cursor, cursor);
    }
}
