//! What the tasks list shows of the assistant: its sessions and what each is
//! doing, the messages queued for it, its todo list, and an activity feed of
//! the GitHub events jeet watched for and the actions the assistant took.
//!
//! All of it lives in files — status files, `inbox.jsonl`, `todo.md`,
//! `watch-events.jsonl`, `actions.log` — read by [`gather`] on a background
//! thread, never in a draw. The screen reads only the index, for the
//! [`Probe`] that tells the thread which sessions there are.
//!
//! Event titles, messages and the todo list are written by other people or
//! other agents: what is drawn from them goes through
//! [`crate::commands::task::printable`], and a link is only ever opened when
//! it is a web address, handed to the opener as one argument.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::assistant::{self, Session};
use crate::context::App;
use crate::db::tasks::TaskSessionRecord;
use crate::messages::{self, Queued};
use crate::notify;
use crate::permissions::{self, ActionRecord, Outcome};
use crate::tasks;
use crate::tmux;
use crate::todo::{self, ItemView};
use crate::watch::{self, Logged};

/// The most of the end of each log the feed reads: the logs only grow, and
/// the feed shows the latest of them.
const TAIL_BYTES: u64 = 64 * 1024;

/// The most entries the feed keeps.
pub const FEED_MAX: usize = 200;

/// What the thread needs to know to [`gather`]: no index, so it can run on a
/// thread of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `$JEET_HOME`, where the events log is.
    pub home: PathBuf,
    /// The assistant's home.
    pub dir: PathBuf,
    /// Its sessions, as the index has them; none before its first.
    pub records: Vec<TaskSessionRecord>,
    pub primary: Option<String>,
    /// Its task id; none before its first session.
    pub task_id: Option<String>,
}

/// The assistant's [`Probe`], from the index. Quick: no tmux, no files.
pub fn probe(app: &App) -> Result<Probe> {
    let (dir, records, primary, task_id) = match app.db.get_assistant_task()? {
        Some(task) => (
            tasks::task_dir(app, &task),
            app.db.list_task_sessions(&task.id)?,
            app.db.primary_session(&task.id)?,
            Some(task.id),
        ),
        None => (assistant::home_dir(&app.home), Vec::new(), None, None),
    };
    Ok(Probe {
        home: app.home.clone(),
        dir,
        records,
        primary,
        task_id,
    })
}

/// Everything the tasks list shows of the assistant, as of one look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    /// Most recently active first.
    pub sessions: Vec<Session>,
    /// Messages and events queued for it, not yet handed to a session.
    pub inbox: usize,
    /// `todo.md`, item by item.
    pub todo: Vec<ItemView>,
    /// Why `todo.md` could not be read, if it could not.
    pub todo_error: Option<String>,
    /// Newest first.
    pub feed: Vec<FeedEntry>,
    /// How many of its running windows jeet has no record of starting
    /// ([`assistant::unanchored_windows`]).
    pub unanchored: usize,
}

impl Info {
    pub fn open_todo(&self) -> usize {
        self.todo.iter().filter(|i| !i.done).count()
    }
}

/// Where a feed entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    /// A GitHub event `jeet watch` saw.
    Event,
    /// Something the assistant did, or was refused.
    Action,
    /// A message or event waiting for one of its sessions.
    Queued,
}

/// One line of the inbox/activity panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// Unix seconds.
    pub at: i64,
    pub kind: FeedKind,
    /// What happened, in a word or two: `review requested`, `done`, `queued`.
    pub what: String,
    /// The event's title, the action, or the message's first line.
    pub text: String,
    /// Where it was or went: `acme/api #42 → the assistant`, a capability.
    pub place: String,
    /// The page it is about, when it is one that may be opened.
    pub url: Option<String>,
}

/// Look at everything [`Info`] holds. `windows` are the assistant's tmux
/// windows, which the caller lists, so that this runs tmux not at all.
pub fn gather(probe: &Probe, windows: &[tmux::Window]) -> Info {
    let sessions = assistant::sessions_at(
        &probe.dir,
        probe.records.clone(),
        probe.primary.as_deref(),
        windows,
    );
    let queued = queued(&probe.dir);
    let (todo, todo_error) = match read_todo(&probe.dir) {
        Ok(todo) => (todo, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let mut feed: Vec<FeedEntry> = queued.iter().map(queued_entry).collect();
    feed.extend(
        watch::tail_lines(&watch::events_path(&probe.home), TAIL_BYTES)
            .iter()
            .filter_map(|line| serde_json::from_str::<Logged>(line).ok())
            .map(event_entry),
    );
    feed.extend(
        watch::tail_lines(&permissions::actions_log(&probe.home), TAIL_BYTES)
            .iter()
            .filter_map(|line| serde_json::from_str::<ActionRecord>(line).ok())
            .map(action_entry),
    );
    // Stable, so what was logged later stays above what was logged in the
    // same second before it.
    feed.reverse();
    feed.sort_by_key(|e| std::cmp::Reverse(e.at));
    feed.truncate(FEED_MAX);
    Info {
        sessions,
        inbox: queued.len(),
        todo,
        todo_error,
        feed,
        unanchored: 0,
    }
}

/// The todo list in `dir`; empty when there is none yet.
fn read_todo(dir: &Path) -> Result<Vec<ItemView>> {
    Ok(todo::read(dir, "")?.views())
}

/// What is queued in the inbox in `dir`. A home with no inbox is left
/// without one: the list only looks.
fn queued(dir: &Path) -> Vec<Queued> {
    if !messages::inbox_path(dir).exists() {
        return Vec::new();
    }
    messages::Inbox::lock(dir)
        .and_then(|inbox| inbox.read())
        .unwrap_or_default()
}

fn event_entry(logged: Logged) -> FeedEntry {
    let event = logged.event;
    let place = match event.place() {
        place if place.is_empty() => format!("→ {}", logged.to),
        place => format!("{place} → {}", logged.to),
    };
    FeedEntry {
        at: event.at,
        kind: FeedKind::Event,
        what: event.kind.label().to_string(),
        text: one_line(&event.title),
        place: one_line(&place),
        url: Some(event.url).filter(|u| notify::openable_url(u)),
    }
}

fn action_entry(record: ActionRecord) -> FeedEntry {
    let what = match record.outcome {
        Outcome::Done => "done",
        Outcome::Failed => "failed",
        Outcome::Denied => "refused",
        Outcome::Skipped => "skipped",
    };
    let text = match &record.error {
        Some(error) => format!("{}: {error}", record.action),
        None => record.action.clone(),
    };
    FeedEntry {
        at: record.at,
        kind: FeedKind::Action,
        what: what.to_string(),
        text: one_line(&text),
        place: one_line(&record.capability),
        url: None,
    }
}

fn queued_entry(queued: &Queued) -> FeedEntry {
    FeedEntry {
        at: queued.at,
        kind: FeedKind::Queued,
        what: "queued".to_string(),
        text: first_line(&queued.message),
        place: match &queued.to {
            Some(_) => "for a busy session".to_string(),
            None => "for the next session".to_string(),
        },
        url: None,
    }
}

/// The first line of a queued message that says something, past the fence
/// tags it came in.
fn first_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| !(l.starts_with('<') && l.ends_with('>')))
        .map(one_line)
        .unwrap_or_default()
}

/// `text` on one line, with nothing in it the terminal would act on.
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c => c,
        })
        .filter(|c| !c.is_control())
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Todo
// ---------------------------------------------------------------------------

/// Tick item `id` off, or with `done` false put it back. `text` is the item
/// as the panel showed it: the list may have changed since, so the item is
/// found by it, and nothing is changed when it has gone.
pub fn set_todo_done(dir: &Path, id: usize, text: &str, done: bool) -> Result<ItemView> {
    assistant::ensure_files(dir)?;
    todo::update(dir, assistant::TODO, |list| {
        let views = list.views();
        let id = match views.get(id.wrapping_sub(1)) {
            Some(item) if item.text == text => id,
            _ => views
                .iter()
                .find(|item| item.text == text)
                .map(|item| item.id)
                .ok_or_else(|| anyhow!("that item has changed since it was shown; look again"))?,
        };
        let mut changed = list.set_done(&[id], done)?;
        changed
            .pop()
            .ok_or_else(|| anyhow!("there is no item {id}"))
    })
}

/// Add an item to the todo list, in priority order.
pub fn add_todo(dir: &Path, text: &str) -> Result<ItemView> {
    let text = todo::item_text(text)?;
    assistant::ensure_files(dir)?;
    todo::update(dir, assistant::TODO, |list| Ok(list.add(&text)))
}

/// Where each task the open todo items link to ranks: by the item's
/// priority, then by where it is in the list. A task linked more than once
/// ranks by its best item.
pub fn todo_ranks(items: &[ItemView]) -> HashMap<String, (u16, usize)> {
    let mut ranks: HashMap<String, (u16, usize)> = HashMap::new();
    for (at, item) in items.iter().enumerate().filter(|(_, i)| !i.done) {
        let rank = (item.priority.map_or(u16::MAX, u16::from), at);
        for slug in &item.tasks {
            ranks
                .entry(slug.clone())
                .and_modify(|r| *r = (*r).min(rank))
                .or_insert(rank);
        }
    }
    ranks
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// The command that opens `url` in the browser; none unless it is a web
/// address. It is one argument, after the opener, never through a shell.
pub fn opener_argv(url: &str, macos: bool) -> Option<Vec<String>> {
    if !notify::openable_url(url) {
        return None;
    }
    let opener = match macos {
        true => "open",
        false => "xdg-open",
    };
    Some(vec![opener.to_string(), url.to_string()])
}

/// Open `url` in the browser.
pub fn open_url(url: &str) -> Result<()> {
    let Some(argv) = opener_argv(url, cfg!(target_os = "macos")) else {
        anyhow::bail!("only web addresses are opened");
    };
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow!("run {}: {e}", argv[0]))?;
    match status.success() {
        true => Ok(()),
        false => anyhow::bail!("{} failed ({status})", argv[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EventKind;
    use crate::status::Activity;
    use crate::watch::Event;

    fn record(id: &str, last: i64) -> TaskSessionRecord {
        TaskSessionRecord {
            session_id: id.into(),
            task_id: "t".into(),
            parent_id: None,
            name: format!("{id} name"),
            created_at: last,
            last_activity: last,
        }
    }

    fn append(path: &Path, line: &str) {
        use std::io::Write;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn logged(at: i64, title: &str, url: &str) -> String {
        serde_json::to_string(&Logged {
            event: Event {
                kind: EventKind::ReviewRequested,
                repo: "acme/api".into(),
                number: Some(42),
                title: title.into(),
                url: url.into(),
                actor: None,
                reason: "review_requested".into(),
                at,
            },
            to: "the assistant".into(),
        })
        .unwrap()
    }

    fn action(at: i64, outcome: Outcome, error: Option<&str>) -> String {
        serde_json::to_string(&ActionRecord {
            at,
            capability: "task.create".into(),
            action: "created task fix-login (api)".into(),
            outcome,
            error: error.map(str::to_string),
        })
        .unwrap()
    }

    #[test]
    fn an_assistant_never_set_up_has_nothing_and_gets_nothing_made() {
        let home = tempfile::tempdir().unwrap();
        let probe = Probe {
            home: home.path().to_path_buf(),
            dir: home.path().join("assistant"),
            records: Vec::new(),
            primary: None,
            task_id: None,
        };
        assert_eq!(gather(&probe, &[]), Info::default());
        assert!(!probe.dir.exists(), "looking makes nothing");
    }

    #[test]
    fn gathering_reads_sessions_inbox_todo_and_the_feed() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("assistant");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("todo.md"),
            "# Todo\n- [ ] !p1 ship @fix-login\n- [x] done one\n- [ ] later\n",
        )
        .unwrap();
        {
            let inbox = messages::Inbox::lock(&dir).unwrap();
            inbox
                .push_to(
                    &assistant::fence_message("task fix-login", "please \x1b[2Jlook"),
                    None,
                )
                .unwrap();
        }
        let events = watch::events_path(home.path());
        append(
            &events,
            &logged(
                100,
                "Fix\x1b]8;;evil\x07 it",
                "https://github.com/acme/api/pull/42",
            ),
        );
        append(&events, &logged(300, "Second", "file:///etc/passwd"));
        append(&events, "not json");
        let log = permissions::actions_log(home.path());
        append(&log, &action(200, Outcome::Done, None));
        append(&log, &action(400, Outcome::Denied, Some("not allowed")));

        let probe = Probe {
            home: home.path().to_path_buf(),
            dir: dir.clone(),
            records: vec![record("a", 10), record("b", 20)],
            primary: Some("a".into()),
            task_id: None,
        };
        let windows = [tmux::Window {
            id: "@1".into(),
            alive: true,
            key: Some("a".into()),
            launch: Some("a".into()),
        }];
        let info = gather(&probe, &windows);

        assert_eq!(info.sessions.len(), 2);
        assert_eq!(info.sessions[0].id, "b", "most recently active first");
        assert_eq!(info.sessions[0].activity, Activity::NotStarted);
        assert!(info.sessions[1].primary && info.sessions[1].running());
        assert_eq!(info.inbox, 1);
        assert_eq!(info.todo.len(), 3);
        assert_eq!(info.open_todo(), 2);
        assert_eq!(info.todo_error, None);

        // Queued just now, so newest of all.
        let queued = &info.feed[0];
        assert_eq!(queued.kind, FeedKind::Queued);
        assert_eq!(queued.text, "please [2Jlook");
        let whats: Vec<(i64, &str)> = info.feed[1..]
            .iter()
            .map(|e| (e.at, e.what.as_str()))
            .collect();
        assert_eq!(
            whats,
            [
                (400, "refused"),
                (300, "review requested"),
                (200, "done"),
                (100, "review requested")
            ]
        );
        assert_eq!(
            info.feed[1].text,
            "created task fix-login (api): not allowed"
        );
        assert_eq!(info.feed[1].place, "task.create");
        // Someone else's title: nothing in it reaches the terminal as a
        // control, and only a web address is kept to open.
        let first = &info.feed[4];
        assert_eq!(first.text, "Fix]8;;evil it");
        assert_eq!(first.place, "acme/api #42 → the assistant");
        assert_eq!(
            first.url.as_deref(),
            Some("https://github.com/acme/api/pull/42")
        );
        assert_eq!(info.feed[2].url, None);
        assert_eq!(queued.place, "for the next session");
    }

    #[test]
    fn the_probe_follows_the_index_and_the_screen_changes_the_list_jeet_todo_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: crate::config::load_or_create(&home).unwrap(),
            db: crate::db::Database::open(&crate::config::index_path(&home)).unwrap(),
            home: home.clone(),
        };
        // No assistant yet: its home, with nothing in it.
        let before = probe(&app).unwrap();
        assert_eq!(before.dir, home.join("assistant"));
        assert!(before.records.is_empty() && before.primary.is_none());

        let task = assistant::ensure(&app).unwrap();
        app.db
            .upsert_task_session(&TaskSessionRecord {
                task_id: task.id.clone(),
                ..record("s1", 5)
            })
            .unwrap();
        app.db.set_primary_session(&task.id, Some("s1")).unwrap();
        let after = probe(&app).unwrap();
        assert_eq!(after.dir, before.dir);
        assert_eq!(after.records.len(), 1);
        assert_eq!(after.primary.as_deref(), Some("s1"));

        let info = gather(&after, &[]);
        assert_eq!(info.sessions.len(), 1);
        assert!(info.sessions[0].primary && !info.sessions[0].running());
        assert!(info.todo.is_empty(), "the template is an empty list");

        add_todo(&after.dir, "!p1 ship it @fix-login").unwrap();
        add_todo(&after.dir, "tidy up").unwrap();
        set_todo_done(&after.dir, 2, "tidy up", true).unwrap();
        let info = gather(&after, &[]);
        assert_eq!(info.open_todo(), 1);
        assert_eq!(info.todo[0].tasks, ["fix-login"]);
        let text = std::fs::read_to_string(todo::path(&after.dir)).unwrap();
        assert!(
            text.contains("- [ ] !p1 ship it @fix-login\n- [x] tidy up\n"),
            "{text}"
        );
    }

    #[test]
    fn the_feed_keeps_the_newest() {
        let home = tempfile::tempdir().unwrap();
        let log = permissions::actions_log(home.path());
        for at in 0..(FEED_MAX as i64 + 50) {
            append(&log, &action(at, Outcome::Done, None));
        }
        let probe = Probe {
            home: home.path().to_path_buf(),
            dir: home.path().join("assistant"),
            records: Vec::new(),
            primary: None,
            task_id: None,
        };
        let feed = gather(&probe, &[]).feed;
        assert_eq!(feed.len(), FEED_MAX);
        assert_eq!(feed[0].at, FEED_MAX as i64 + 49);
    }

    #[test]
    fn todo_items_are_ticked_off_by_what_they_say() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("todo.md"),
            "- [ ] one\n- [ ] two\n- [ ] three\n",
        )
        .unwrap();
        let done = set_todo_done(dir.path(), 2, "two", true).unwrap();
        assert!(done.done);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("todo.md")).unwrap(),
            "- [ ] one\n- [x] two\n- [ ] three\n"
        );
        // Someone added an item above it since the panel was drawn: still
        // the same item.
        std::fs::write(
            dir.path().join("todo.md"),
            "- [ ] zero\n- [ ] one\n- [x] two\n- [ ] three\n",
        )
        .unwrap();
        let undone = set_todo_done(dir.path(), 2, "two", false).unwrap();
        assert_eq!(undone.id, 3);
        assert!(std::fs::read_to_string(dir.path().join("todo.md"))
            .unwrap()
            .contains("- [ ] two\n"));
        // Gone altogether: nothing changes.
        let before = std::fs::read_to_string(dir.path().join("todo.md")).unwrap();
        let err = set_todo_done(dir.path(), 1, "vanished", true).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("todo.md")).unwrap(),
            before
        );
    }

    #[test]
    fn added_items_go_in_by_priority_and_must_say_something() {
        let dir = tempfile::tempdir().unwrap();
        add_todo(dir.path(), "later").unwrap();
        let added = add_todo(dir.path(), " !p1 first\tthing ").unwrap();
        assert_eq!(added.id, 1);
        assert_eq!(added.title, "first thing");
        assert!(add_todo(dir.path(), "  ").is_err());
        let text = std::fs::read_to_string(dir.path().join("todo.md")).unwrap();
        assert!(
            text.ends_with("- [ ] !p1 first thing\n- [ ] later\n"),
            "{text}"
        );
    }

    #[test]
    fn tasks_rank_by_their_best_open_item() {
        let items = todo::parse(
            "- [ ] whenever @c\n\
             - [ ] !p2 soon @b @a\n\
             - [x] !p1 finished @c\n\
             - [ ] !p1 now @a\n",
        )
        .views();
        let ranks = todo_ranks(&items);
        assert_eq!(ranks["a"], (1, 3));
        assert_eq!(ranks["b"], (2, 1));
        assert_eq!(ranks["c"], (u16::MAX, 0), "a done item ranks nothing");
        assert!(!ranks.contains_key("d"));
    }

    #[test]
    fn only_web_addresses_are_opened() {
        assert_eq!(
            opener_argv("https://github.com/acme/api/pull/1", true).unwrap(),
            ["open", "https://github.com/acme/api/pull/1"]
        );
        assert_eq!(
            opener_argv("https://x.test/", false).unwrap()[0],
            "xdg-open"
        );
        for bad in [
            "-a Calculator",
            "file:///etc/passwd",
            "/Applications/Calculator.app",
            "https://x.test/ -a Terminal",
            "",
        ] {
            assert_eq!(opener_argv(bad, true), None, "{bad}");
        }
    }

    #[test]
    fn a_queued_message_shows_what_it_says_not_its_fence() {
        assert_eq!(
            first_line(
                "<jeet-message from=\"x\" untrusted=\"true\">\n\nhello\nmore\n</jeet-message>"
            ),
            "hello"
        );
        assert_eq!(first_line("plain"), "plain");
        assert_eq!(first_line(""), "");
    }
}
