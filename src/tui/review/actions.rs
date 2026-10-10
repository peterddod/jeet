//! What the viewer does besides read: comment on a line, reply, react, edit
//! and delete, keep an eye on the PR for changes, and find its place again
//! after fetching the review afresh.
//!
//! The work itself is [`run`], which blocks on `gh` and so only ever runs on
//! a background thread; everything else here is pure.

use std::path::Path;

use crate::github::{self, DiffSide, Fingerprint, PendingReview, Reaction, ReviewThread};

use super::diff::{DiffLine, FileDiff};
use super::{Draft, Row};

/// What a comment being typed is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A new thread on a line of the diff, joining the pending review.
    Line {
        path: String,
        line: u32,
        side: DiffSide,
    },
    /// A reply to the thread opened by comment `to`.
    Reply { to: u64, author: String },
    /// New text for comment `comment`, which said `original`.
    Edit { comment: u64, original: String },
}

impl Target {
    /// The text box's title. Paths and logins come from the PR, so the
    /// renderer cleans this before it is drawn.
    pub fn title(&self) -> String {
        match self {
            Target::Line { path, line, side } => match side {
                DiffSide::Right => format!("comment on {path}:{line}"),
                DiffSide::Left => format!("comment on {path}:{line} (removed)"),
            },
            Target::Reply { author, .. } => format!("reply to {author}"),
            Target::Edit { .. } => "edit your comment".into(),
        }
    }

    /// What ⏎ does, for the text box's hint line.
    pub fn send_label(&self) -> &'static str {
        match self {
            Target::Line { .. } => "add to your pending review",
            Target::Reply { .. } => "reply",
            Target::Edit { .. } => "save",
        }
    }

    /// The banner while GitHub is asked.
    fn busy_label(&self) -> &'static str {
        match self {
            Target::Line { .. } => "adding your comment…",
            Target::Reply { .. } => "replying…",
            Target::Edit { .. } => "saving your edit…",
        }
    }
}

/// A comment being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compose {
    pub target: Target,
    pub text: String,
}

/// The reaction picker, open on one comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// REST id of the comment being reacted to.
    pub comment: u64,
    /// Index into [`Reaction::ALL`].
    pub selected: usize,
    /// The reactions the user has already left on it.
    pub mine: Vec<Reaction>,
}

/// A delete waiting for a yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub comment: u64,
    /// The comment's first line, to say which one is going.
    pub preview: String,
}

/// Work for a background thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// The diff, threads, pending review and fingerprint, all afresh.
    Load,
    /// Just the fingerprint, to see whether the PR has moved on.
    Poll,
    Submit {
        draft: Draft,
        pending: Option<PendingReview>,
    },
    Send(Compose),
    Delete {
        comment: u64,
    },
    React {
        comment: u64,
        reaction: Reaction,
    },
}

impl Job {
    /// The banner while a change is with GitHub; None for the jobs that
    /// only read and so keep out of the user's way.
    pub fn busy_label(&self, number: u64) -> Option<String> {
        match self {
            Job::Load | Job::Poll => None,
            Job::Submit { draft, .. } => {
                Some(format!("submitting: {} #{number}", draft.verdict.label()))
            }
            Job::Send(compose) => Some(compose.target.busy_label().into()),
            Job::Delete { .. } => Some("deleting the comment…".into()),
            Job::React { .. } => Some("reacting…".into()),
        }
    }

    /// The status line once a change has gone through.
    pub fn done_label(&self) -> String {
        match self {
            Job::Send(Compose { target, .. }) => match target {
                Target::Line { .. } => "added to your pending review — A, C or X submits it".into(),
                Target::Reply { author, .. } => format!("replied to {author}"),
                Target::Edit { .. } => "comment updated".into(),
            },
            Job::Delete { .. } => "comment deleted".into(),
            Job::React { reaction, .. } => format!("reacted with {}", reaction.emoji()),
            Job::Load | Job::Poll | Job::Submit { .. } => String::new(),
        }
    }

    /// The status line when GitHub said no.
    pub fn failed_label(&self, why: &str) -> String {
        let what = match self {
            Job::Send(Compose { target, .. }) => match target {
                Target::Line { .. } => "comment",
                Target::Reply { .. } => "reply",
                Target::Edit { .. } => "save the edit",
            },
            Job::Delete { .. } => "delete the comment",
            Job::React { .. } => "react",
            Job::Load | Job::Poll | Job::Submit { .. } => "do that",
        };
        format!("could not {what}: {why}")
    }
}

/// A background thread's answer.
#[derive(Debug)]
pub enum Msg {
    Loaded {
        diff: Result<String, String>,
        threads: Result<Vec<ReviewThread>, String>,
        pending: Result<Option<PendingReview>, String>,
        fingerprint: Result<Fingerprint, String>,
    },
    Polled(Result<Fingerprint, String>),
    Submitted {
        draft: Draft,
        result: Result<(), String>,
    },
    /// A comment sent, edited, deleted or reacted to.
    Done {
        job: Job,
        result: Result<(), String>,
    },
}

/// A background lookup's answer, with its error put into words.
fn joined<T>(answer: std::thread::Result<anyhow::Result<T>>) -> Result<T, String> {
    match answer {
        Ok(inner) => inner.map_err(|e| format!("{e:#}")),
        Err(_) => Err("the lookup stopped".to_string()),
    }
}

fn worded<T>(result: anyhow::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("{e:#}"))
}

/// Do `job` for PR `number`, with `gh` run in `dir`. Blocks for as long as
/// GitHub takes, so it belongs on a background thread.
///
/// Text the user typed goes to `gh` as a field value, an argument of its own:
/// it is never part of a command line a shell reads.
pub fn run(dir: &Path, number: u64, job: Job) -> Msg {
    match job {
        Job::Load => {
            let (diff, threads, pending, fingerprint) = std::thread::scope(|scope| {
                let threads = scope.spawn(|| github::review_threads(dir, number));
                let pending = scope.spawn(|| github::pending_review(dir, number));
                let fingerprint = scope.spawn(|| github::pr_fingerprint(dir, number));
                let diff = github::pr_diff(dir, number);
                (
                    worded(diff),
                    joined(threads.join()),
                    joined(pending.join()),
                    joined(fingerprint.join()),
                )
            });
            Msg::Loaded {
                diff,
                threads,
                pending,
                fingerprint,
            }
        }
        Job::Poll => Msg::Polled(worded(github::pr_fingerprint(dir, number))),
        Job::Submit { draft, pending } => {
            let body = draft.input.trim().to_string();
            let result = worded(github::submit_review(
                dir,
                number,
                draft.verdict,
                &body,
                pending.as_ref(),
            ));
            Msg::Submitted { draft, result }
        }
        Job::Send(ref compose) => {
            let body = compose.text.trim();
            let result = match &compose.target {
                Target::Line { path, line, side } => {
                    github::add_inline_comment(dir, number, path, *line, *side, body)
                }
                Target::Reply { to, .. } => github::reply(dir, number, *to, body),
                Target::Edit { comment, .. } => github::edit_comment(dir, *comment, body),
            };
            Msg::Done {
                result: worded(result),
                job,
            }
        }
        Job::Delete { comment } => Msg::Done {
            result: worded(github::delete_comment(dir, comment)),
            job,
        },
        Job::React { comment, reaction } => Msg::Done {
            result: worded(github::react(dir, comment, reaction)),
            job,
        },
    }
}

/// Where the cursor is, in terms that outlive the rows: the file, the
/// comment if it is on one, and the line it is on or under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub path: String,
    /// GraphQL id of the comment under the cursor.
    pub comment: Option<String>,
    /// The head line number of the line the cursor is on, or that the
    /// comment under it hangs from.
    pub new: Option<u32>,
    /// The base line number, which is all a removed line has.
    pub old: Option<u32>,
    /// The row index, for when nothing better is left to go on.
    pub index: usize,
}

fn line_of<'a>(file: &'a FileDiff, row: &Row) -> Option<&'a DiffLine> {
    match row {
        Row::Line(h, l) => file.hunks.get(*h)?.lines.get(*l),
        _ => None,
    }
}

fn comment_id<'a>(threads: &'a [ReviewThread], row: &Row) -> Option<&'a str> {
    match row {
        Row::Comment { thread, index, .. } => {
            Some(threads.get(*thread)?.comments.get(*index)?.id.as_str())
        }
        _ => None,
    }
}

/// The [`Place`] of row `cursor` of `file`'s page.
pub fn place(file: &FileDiff, threads: &[ReviewThread], rows: &[Row], cursor: usize) -> Place {
    let line = match rows.get(cursor) {
        Some(Row::Line(..)) => line_of(file, &rows[cursor]),
        // A comment's line is the last one above it.
        Some(Row::Comment { .. }) => rows[..cursor]
            .iter()
            .rev()
            .find_map(|row| line_of(file, row)),
        _ => None,
    };
    Place {
        path: file.path().to_string(),
        comment: rows
            .get(cursor)
            .and_then(|row| comment_id(threads, row))
            .map(str::to_string),
        new: line.and_then(|l| l.new),
        old: line.and_then(|l| l.old),
        index: cursor,
    }
}

/// The row of `rows` (the page of `file`, which must be `place.path`) that
/// is nearest to `place`: the same comment, else the same line, else the
/// first line after it, since the code the cursor was on may have gone.
pub fn find(place: &Place, file: &FileDiff, threads: &[ReviewThread], rows: &[Row]) -> usize {
    let last = rows.len().saturating_sub(1);
    if let Some(id) = &place.comment {
        if let Some(i) = rows
            .iter()
            .position(|row| comment_id(threads, row) == Some(id.as_str()))
        {
            return i;
        }
    }
    let first = |test: &dyn Fn(&DiffLine) -> bool| {
        rows.iter()
            .position(|row| line_of(file, row).is_some_and(test))
    };
    let found = match (place.new, place.old) {
        (Some(n), _) => {
            first(&|l| l.new == Some(n)).or_else(|| first(&|l| l.new.is_some_and(|m| m >= n)))
        }
        // A removed line has no head number, and is known by its base one.
        (None, Some(o)) => first(&|l| l.new.is_none() && l.old == Some(o))
            .or_else(|| first(&|l| l.old.is_some_and(|m| m >= o))),
        (None, None) => None,
    };
    found.unwrap_or(place.index.min(last))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{loaded_viewer, threads};
    use super::*;

    fn fixture() -> Vec<FileDiff> {
        super::super::diff::parse(include_str!("../../../tests/fixtures/pr.diff"))
    }

    fn rows_of(file: &FileDiff) -> Vec<Row> {
        super::super::build_rows(file, &threads())
    }

    #[test]
    fn a_place_survives_a_rebuild_of_the_same_page() {
        let files = fixture();
        let rows = rows_of(&files[0]);
        let threads = threads();
        for cursor in 0..rows.len() {
            let at = place(&files[0], &threads, &rows, cursor);
            assert_eq!(find(&at, &files[0], &threads, &rows), cursor, "{at:?}");
        }
    }

    #[test]
    fn a_comment_is_placed_by_its_id_and_its_line() {
        let files = fixture();
        let rows = rows_of(&files[0]);
        let threads = threads();
        let reply = rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    Row::Comment {
                        thread: 0,
                        index: 1,
                        ..
                    }
                )
            })
            .unwrap();
        let at = place(&files[0], &threads, &rows, reply);
        assert_eq!(at.comment.as_deref(), Some("PRRC_kwDOAAABc84AAAAD"));
        assert_eq!(at.new, Some(12), "the line its thread is on");

        // Once the comment is gone, the cursor lands on its line instead.
        let mut fewer = threads.clone();
        fewer[0].comments.pop();
        let rows = super::super::build_rows(&files[0], &fewer);
        let i = find(&at, &files[0], &fewer, &rows);
        assert_eq!(line_of(&files[0], &rows[i]).unwrap().new, Some(12));
    }

    #[test]
    fn a_line_that_has_gone_finds_the_next_one() {
        let files = fixture();
        let file = &files[0];
        let rows = rows_of(file);
        let at = Place {
            path: file.path().into(),
            comment: None,
            new: Some(20),
            old: None,
            index: 0,
        };
        // Head line 20 is in neither hunk; 31 is the next one there is.
        let i = find(&at, file, &[], &rows);
        assert_eq!(line_of(file, &rows[i]).unwrap().new, Some(31));

        // A removed line is known by its base number.
        let removed = Place {
            new: None,
            old: Some(11),
            ..at.clone()
        };
        let i = find(&removed, file, &[], &rows);
        let line = line_of(file, &rows[i]).unwrap();
        assert_eq!((line.old, line.new), (Some(11), None));

        // Past the end of every hunk: the row it was on, kept on the page.
        let beyond = Place {
            new: Some(999),
            index: 999,
            ..at
        };
        assert_eq!(find(&beyond, file, &[], &rows), rows.len() - 1);
    }

    #[test]
    fn labels_say_what_happened() {
        let compose = |target| {
            Job::Send(Compose {
                target,
                text: "hi".into(),
            })
        };
        let reply = compose(Target::Reply {
            to: 1,
            author: "ada".into(),
        });
        assert_eq!(reply.done_label(), "replied to ada");
        assert_eq!(reply.failed_label("HTTP 422"), "could not reply: HTTP 422");
        assert_eq!(reply.busy_label(42).as_deref(), Some("replying…"));
        assert_eq!(Job::Poll.busy_label(42), None);
        let react = Job::React {
            comment: 1,
            reaction: Reaction::Rocket,
        };
        assert_eq!(react.done_label(), "reacted with 🚀");
        let line = Target::Line {
            path: "a.rs".into(),
            line: 3,
            side: DiffSide::Left,
        };
        assert_eq!(line.title(), "comment on a.rs:3 (removed)");
    }

    /// Jobs run end to end against a stand-in `gh`.
    #[cfg(unix)]
    mod stub {
        use super::*;
        use crate::github::GH_PROGRAM;

        /// A stand-in `gh` for this thread that records each call's arguments,
        /// one file per call, and answers the lookups the jobs make.
        struct StubGh {
            dir: tempfile::TempDir,
        }

        impl StubGh {
            fn new(pending: bool) -> Self {
                let dir = tempfile::TempDir::new().unwrap();
                let script = dir.path().join("gh");
                std::fs::write(
                    &script,
                    r#"#!/bin/sh
    here=$(dirname "$0")
    mkdir -p "$here/calls"
    n=$(ls "$here/calls" | wc -l | tr -d ' ')
    printf '%s\0' "$@" > "$here/calls/$(printf '%03d' "$n")"
    [ "$GH_PROMPT_DISABLED" = 1 ] || { echo "gh run without GH_PROMPT_DISABLED" >&2; exit 3; }
    [ -f "$here/fail" ] && { cat "$here/fail" >&2; exit 1; }
    for arg in "$@"; do
      case "$arg" in
        */reviews/7/comments\?per_page=100) echo '[[{},{}]]'; exit 0 ;;
        */reviews\?per_page=100) cat "$here/reviews.json"; exit 0 ;;
        query=query*) cat "$here/fingerprint.json"; exit 0 ;;
      esac
    done
    echo '{}'
    "#,
                )
                .unwrap();
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                let reviews = if pending {
                    r#"[[{"id":6,"node_id":"PRR_old","state":"COMMENTED"},{"id":7,"node_id":"PRR_x","state":"PENDING"}]]"#
                } else {
                    "[[]]"
                };
                std::fs::write(dir.path().join("reviews.json"), reviews).unwrap();
                std::fs::write(
                    dir.path().join("fingerprint.json"),
                    include_str!("../../../tests/fixtures/pr_fingerprint.json"),
                )
                .unwrap();
                GH_PROGRAM.with(|p| *p.borrow_mut() = Some(script));
                Self { dir }
            }

            fn fail_with(&self, stderr: &str) {
                std::fs::write(self.dir.path().join("fail"), stderr).unwrap();
            }

            /// Every call so far, each as its arguments.
            fn calls(&self) -> Vec<Vec<String>> {
                let dir = self.dir.path().join("calls");
                let mut names: Vec<_> = std::fs::read_dir(&dir)
                    .map(|d| d.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                names.sort();
                names
                    .iter()
                    .map(|path| {
                        let text = std::fs::read_to_string(path).unwrap();
                        text.split_terminator('\0').map(str::to_string).collect()
                    })
                    .collect()
            }

            fn run(&self, job: Job) -> Msg {
                run(self.dir.path(), 42, job)
            }
        }

        impl Drop for StubGh {
            fn drop(&mut self) {
                GH_PROGRAM.with(|p| *p.borrow_mut() = None);
            }
        }

        fn done(msg: Msg) -> Result<(), String> {
            match msg {
                Msg::Done { result, .. } => result,
                other => panic!("not a change: {other:?}"),
            }
        }

        fn has(call: &[String], arg: &str) -> bool {
            call.iter().any(|a| a == arg)
        }

        #[test]
        fn an_inline_comment_joins_the_pending_review() {
            let gh = StubGh::new(true);
            let body = "Use `u64`; $(rm -rf /) is just text.\nSecond line.";
            let job = Job::Send(Compose {
                target: Target::Line {
                    path: "src/lib.rs".into(),
                    line: 12,
                    side: DiffSide::Right,
                },
                text: format!("  {body}\n"),
            });
            assert_eq!(done(gh.run(job)), Ok(()));
            let calls = gh.calls();
            let add = calls.last().unwrap();
            assert_eq!(&add[..2], ["api", "graphql"]);
            assert!(has(add, "review=PRR_x"), "{add:?}");
            assert!(has(add, "path=src/lib.rs"), "{add:?}");
            assert!(has(add, "line=12"), "{add:?}");
            assert!(has(add, "side=RIGHT"), "{add:?}");
            // The text arrives whole and trimmed, as one argument.
            assert!(has(add, &format!("body={body}")), "{add:?}");
        }

        #[test]
        fn an_inline_comment_starts_a_pending_review_when_there_is_none() {
            let gh = StubGh::new(false);
            let job = Job::Send(Compose {
                target: Target::Line {
                    path: "src/old.rs".into(),
                    line: 2,
                    side: DiffSide::Left,
                },
                text: "why?".into(),
            });
            assert_eq!(done(gh.run(job)), Ok(()));
            let calls = gh.calls();
            let create = calls.last().unwrap();
            assert!(has(create, "POST"), "{create:?}");
            assert!(has(create, "repos/{owner}/{repo}/pulls/42/reviews"));
            assert!(has(create, "comments[][side]=LEFT"), "{create:?}");
            assert!(has(create, "comments[][line]=2"), "{create:?}");
            assert!(
                !create.iter().any(|a| a.starts_with("event=")),
                "no event, so the review stays pending: {create:?}"
            );
        }

        #[test]
        fn replies_edits_deletes_and_reactions_reach_the_comment() {
            let gh = StubGh::new(false);
            let reply = Job::Send(Compose {
                target: Target::Reply {
                    to: 1001,
                    author: "ada".into(),
                },
                text: "Fixed.".into(),
            });
            assert_eq!(done(gh.run(reply)), Ok(()));
            let edit = Job::Send(Compose {
                target: Target::Edit {
                    comment: 1002,
                    original: "Good catch, fixed.".into(),
                },
                text: "Good catch — fixed in 5f13c0f.".into(),
            });
            assert_eq!(done(gh.run(edit)), Ok(()));
            assert_eq!(done(gh.run(Job::Delete { comment: 1002 })), Ok(()));
            let react = Job::React {
                comment: 1001,
                reaction: Reaction::ThumbsUp,
            };
            assert_eq!(done(gh.run(react)), Ok(()));

            let calls = gh.calls();
            assert_eq!(calls.len(), 4);
            assert!(has(
                &calls[0],
                "repos/{owner}/{repo}/pulls/42/comments/1001/replies"
            ));
            assert!(has(&calls[0], "body=Fixed."));
            assert!(has(&calls[1], "PATCH"));
            assert!(has(&calls[1], "repos/{owner}/{repo}/pulls/comments/1002"));
            assert!(has(&calls[1], "body=Good catch — fixed in 5f13c0f."));
            assert!(has(&calls[2], "DELETE"));
            assert!(has(&calls[2], "repos/{owner}/{repo}/pulls/comments/1002"));
            assert!(has(
                &calls[3],
                "repos/{owner}/{repo}/pulls/comments/1001/reactions"
            ));
            assert!(has(&calls[3], "content=+1"));
        }

        #[test]
        fn a_refusal_comes_back_in_words() {
            let gh = StubGh::new(false);
            gh.fail_with("HTTP 403: Resource not accessible by integration\n");
            let result = done(gh.run(Job::Delete { comment: 1 }));
            assert_eq!(
                result,
                Err("HTTP 403: Resource not accessible by integration".into())
            );
        }

        #[test]
        fn a_poll_reads_the_fingerprint() {
            let gh = StubGh::new(false);
            match gh.run(Job::Poll) {
                Msg::Polled(Ok(print)) => assert_eq!(print.head_ref_oid, "5f13c0ffee"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn a_finished_change_shows_up_after_a_reload() {
            // The whole round: a reply is typed, sent, and its success asks for
            // the review afresh — which keeps the cursor where it was.
            let gh = StubGh::new(false);
            let mut viewer = loaded_viewer(None);
            let row = viewer
                .rows
                .iter()
                .position(|r| {
                    matches!(
                        r,
                        Row::Comment {
                            thread: 0,
                            index: 1,
                            ..
                        }
                    )
                })
                .unwrap();
            viewer.cursor = row;
            let key = |c| {
                ratatui::crossterm::event::KeyEvent::new(
                    c,
                    ratatui::crossterm::event::KeyModifiers::NONE,
                )
            };
            use ratatui::crossterm::event::KeyCode;
            viewer.handle_key(key(KeyCode::Char('R')));
            for c in "thanks".chars() {
                viewer.handle_key(key(KeyCode::Char(c)));
            }
            let job = viewer.handle_key(key(KeyCode::Enter)).unwrap();
            let Msg::Done { job, result } = gh.run(job) else {
                panic!("not a change");
            };
            let calls = gh.calls();
            assert!(
                has(
                    &calls[0],
                    "repos/{owner}/{repo}/pulls/42/comments/1001/replies"
                ),
                "a reply goes to the thread's first comment: {:?}",
                calls[0]
            );
            assert_eq!(viewer.done(job, result), Some(Job::Load));
            assert_eq!(viewer.status, "replied to ada");
            assert!(viewer.refreshing);
        }
    }
}
