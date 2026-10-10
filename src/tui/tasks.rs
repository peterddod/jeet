//! The tasks list: `jeet tasks`, and `jeet` with no arguments.
//!
//! One row per task — what its agent is doing, its name, the worktrees it
//! holds, how much disk it takes and when anything last happened in it —
//! newest activity first, with keys for everything a task goes through:
//! open its agent, browse it, manage its worktrees, archive, delete, and the
//! projects new tasks start from.
//!
//! Nothing slow happens in a draw. Agent statuses are polled, task sizes
//! walked and PRs looked up on background threads that report over a
//! channel, tagged by task id; everything git or the network does on a key
//! press runs under [`term::with_progress`], so the screen keeps drawing.
//!
//! Unlike the explorer, commands here are bare letters, the way the task
//! keys are learned; `/` opens the filter, which then takes typed characters
//! and narrows the list exactly as the explorer's does.
//!
//! The assistant is pinned above the list: its sessions and what each is
//! doing, what is queued for it and its open todo items, read on a
//! background thread of its own ([`assistant_info`]). `c` talks to it, `C`
//! picks one of its sessions, `t` opens its todo list, `i` its inbox and
//! activity, and `o` sorts the tasks by the todo items that link them.
//!
//! [`TasksUi`] holds the state and turns keys into [`Action`]s without
//! touching the terminal, the index or git, so the whole key map is unit
//! tested; [`run`] carries the actions out.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use super::assistant_info::{self, Info as AssistantInfo};
use super::chat::{self as session_picker, Pick, Step};
use super::state::{rank_matches, Exit};
use super::tasks_ui;
use super::term::{self, Tui};
use crate::agent::{self, AgentSpec};
use crate::assistant::{self, Open};
use crate::commands::task::printable;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskPrRecord, TaskRecord, TaskState, TaskWorktreeRecord};
use crate::git;
use crate::github;
use crate::pr_history;
use crate::projects;
use crate::runtime;
use crate::status::{self, Activity};
use crate::tasks::{self, NewTask};
use crate::tmux;
use crate::todo::ItemView;
use crate::watch;
use crate::worktrees::{self, now_secs, WorktreeEntry, WorktreeKind, WorktreeStatus};

/// How often agent statuses are read again.
pub const POLL_EVERY: Duration = Duration::from_secs(2);

/// How often task sizes are walked again, for worktrees an agent added or
/// build output that grew while the list was open.
pub const RESIZE_EVERY: Duration = Duration::from_secs(120);

/// How often the rows are read from the index again, for worktrees and
/// tasks that came and went behind the list's back.
pub const REREAD_EVERY: Duration = Duration::from_secs(5);

/// How often the list sees whether a GitHub poll (`jeet watch --once`) is
/// due, while it is open; `[watch] interval_secs` says how often one is.
const WATCH_EVERY: Duration = Duration::from_secs(30);

/// What the screen esc in the explorer comes back to.
const BACK_LABEL: &str = "tasks";

/// Said when a key needs a task and the list has none under the cursor.
const NO_TASK: &str = "no task selected — n makes one";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One task, with what the background threads have found out about it.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: TaskRecord,
    pub dir: PathBuf,
    /// As of the latest reconcile: the agent adds worktrees as it goes.
    pub worktrees: Vec<TaskWorktreeRecord>,
    /// None until the poller first reports, and for archived tasks.
    pub activity: Option<Activity>,
    /// Unix seconds; the poller raises it from status files and transcripts.
    pub last_activity: i64,
    /// Bytes on disk, once the size walker has been through the task.
    pub size: Option<u64>,
}

impl TaskRow {
    pub fn summary(&self) -> String {
        tasks::worktree_summary(&self.worktrees)
    }

    /// Whether the agent is running in the task's tmux session, as far as
    /// the last poll knows.
    pub fn agent_running(&self) -> bool {
        matches!(
            self.activity,
            Some(
                Activity::Live
                    | Activity::Idle { .. }
                    | Activity::Running { .. }
                    | Activity::Waiting { .. }
            )
        )
    }
}

/// Which tasks the list shows; ⇥ switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Active,
    Archived,
}

impl View {
    pub fn state(self) -> TaskState {
        match self {
            View::Active => TaskState::Active,
            View::Archived => TaskState::Archived,
        }
    }

    fn other(self) -> View {
        match self {
            View::Active => View::Archived,
            View::Archived => View::Active,
        }
    }
}

/// What a background thread found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Status {
        id: String,
        activity: Option<Activity>,
        last_activity: i64,
    },
    Size {
        id: String,
        bytes: u64,
    },
    /// A task's PRs, as GitHub has them now.
    Prs(pr_history::Found),
    /// The assistant, as its files have it now.
    Assistant(Box<AssistantInfo>),
}

/// A list to choose from, narrowed by typing — one repository for a new
/// worktree, or any number of them for a new task.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub items: Vec<String>,
    pub marked: Vec<bool>,
    pub filter: String,
    /// Indices into `items` the filter keeps, in display order.
    pub visible: Vec<usize>,
    /// Cursor within `visible`.
    pub selected: usize,
}

impl Picker {
    pub fn new(items: Vec<String>) -> Self {
        let mut picker = Picker {
            marked: vec![false; items.len()],
            items,
            ..Picker::default()
        };
        picker.refilter();
        picker
    }

    fn refilter(&mut self) {
        let keep = self.current_index();
        self.visible = rank_matches(self.items.iter().map(String::as_str), &self.filter);
        // A narrower filter puts the best match first, so the cursor goes
        // there rather than staying on a row that has dropped down the list.
        self.selected = match keep {
            Some(index) if self.filter.is_empty() => {
                self.visible.iter().position(|&i| i == index).unwrap_or(0)
            }
            _ => 0,
        };
    }

    fn current_index(&self) -> Option<usize> {
        self.visible.get(self.selected).copied()
    }

    pub fn current(&self) -> Option<&str> {
        self.current_index().map(|i| self.items[i].as_str())
    }

    pub fn move_by(&mut self, delta: isize) {
        self.selected = moved(self.selected, delta, self.visible.len());
    }

    pub fn push(&mut self, c: char) {
        self.filter.push(c);
        self.refilter();
    }

    pub fn pop(&mut self) -> bool {
        let popped = self.filter.pop().is_some();
        if popped {
            self.refilter();
        }
        popped
    }

    pub fn toggle(&mut self) {
        if let Some(index) = self.current_index() {
            self.marked[index] = !self.marked[index];
        }
    }

    /// The marked items, in list order.
    pub fn chosen(&self) -> Vec<String> {
        self.items
            .iter()
            .zip(&self.marked)
            .filter(|(_, &marked)| marked)
            .map(|(item, _)| item.clone())
            .collect()
    }
}

/// The new-task form's fields, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Project,
    Repos,
    Branch,
}

impl Field {
    const ALL: [Field; 4] = [Field::Name, Field::Project, Field::Repos, Field::Branch];

    fn step(self, delta: isize) -> Field {
        let at = Field::ALL.iter().position(|&f| f == self).unwrap_or(0) as isize;
        let len = Field::ALL.len() as isize;
        Field::ALL[(at + delta).rem_euclid(len) as usize]
    }
}

/// `n`: a name, and optionally a project, repositories and a branch.
/// Everything left blank makes an empty canvas under a made-up name.
#[derive(Debug, Clone)]
pub struct NewTaskForm {
    pub field: Field,
    pub name: String,
    /// What a blank name becomes.
    pub default_name: String,
    pub projects: Vec<String>,
    /// Index into `projects`; None for no project.
    pub project: Option<usize>,
    pub repos: Picker,
    pub branch: String,
    /// Why the last ⏎ did not create anything.
    pub error: Option<String>,
}

/// A task to create, as the form describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub name: String,
    pub project: Option<String>,
    pub repos: Vec<String>,
    pub branch: Option<String>,
}

impl NewTaskForm {
    pub fn new(
        default_name: String,
        projects: Vec<String>,
        project: Option<&str>,
        repos: Vec<String>,
    ) -> Self {
        let project = project.and_then(|p| projects.iter().position(|n| n == p));
        NewTaskForm {
            field: Field::Name,
            name: String::new(),
            default_name,
            projects,
            project,
            repos: Picker::new(repos),
            branch: String::new(),
            error: None,
        }
    }

    pub fn project_name(&self) -> Option<&str> {
        self.project
            .and_then(|i| self.projects.get(i))
            .map(String::as_str)
    }

    /// Step through "no project" and each project.
    fn cycle_project(&mut self, delta: isize) {
        let slots = self.projects.len() + 1;
        let at = self.project.map_or(0, |i| i + 1) as isize;
        let next = (at + delta).rem_euclid(slots as isize) as usize;
        self.project = next.checked_sub(1);
    }

    /// The task the form describes, or why it describes none.
    pub fn spec(&self) -> std::result::Result<TaskSpec, String> {
        let name = match self.name.trim() {
            "" => self.default_name.clone(),
            name => name.to_string(),
        };
        let repos = self.repos.chosen();
        let branch = Some(self.branch.trim().to_string()).filter(|b| !b.is_empty());
        if branch.is_some() && repos.len() != 1 {
            return Err("a branch needs exactly one repository: it names that one's branch".into());
        }
        Ok(TaskSpec {
            name,
            project: self.project_name().map(str::to_string),
            repos,
            branch,
        })
    }
}

/// Which field of the add-worktree form has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddField {
    Repo,
    Branch,
}

/// One session of a task's agent, for the session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChoice {
    pub id: String,
    pub name: String,
    /// What it last reported, e.g. `idle` or `ended`.
    pub state: String,
    pub last: i64,
}

/// Somewhere the explorer can open in a task: the canvas or a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    /// The worktree's repository; None for the task's canvas.
    pub repo_id: Option<String>,
}

/// One worktree in the worktrees panel.
#[derive(Debug, Clone)]
pub struct WorktreeLine {
    pub record: TaskWorktreeRecord,
    pub path: PathBuf,
    pub present: bool,
    /// The branch it is on now, or `detached @ sha`.
    pub head: String,
    pub status: WorktreeStatus,
}

/// One project in the projects panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLine {
    pub name: String,
    pub tasks: usize,
    /// `CLAUDE.md, 2 skills`, or why the project cannot be read.
    pub contents: String,
    pub repos: String,
}

/// What a confirm overlay is asking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Archive { task_id: String },
    Delete { task_id: String },
    RemoveWorktree { task_id: String, dir_name: String },
    DeleteProject { name: String },
    ForceResync { task_id: String },
}

/// An overlay over the list, which takes the keyboard while it is up.
#[derive(Debug, Clone)]
pub enum Panel {
    Help {
        scroll: u16,
    },
    Message {
        title: String,
        lines: Vec<String>,
        /// The panel to go back to when it is dismissed.
        then: Option<Box<Panel>>,
    },
    Confirm {
        title: String,
        lines: Vec<String>,
        action: Pending,
    },
    Sessions {
        task_id: String,
        sessions: Vec<SessionChoice>,
        selected: usize,
    },
    Places {
        task_id: String,
        places: Vec<Place>,
        selected: usize,
    },
    Worktrees {
        task_id: String,
        lines: Vec<WorktreeLine>,
        selected: usize,
    },
    AddWorktree {
        task_id: String,
        repos: Picker,
        branch: String,
        field: AddField,
    },
    NewTask(NewTaskForm),
    Projects {
        lines: Vec<ProjectLine>,
        selected: usize,
    },
    NewProject {
        input: String,
    },
    Prs {
        task_id: String,
        prs: Vec<TaskPrRecord>,
        selected: usize,
        /// GitHub is being asked for news of them.
        looking: bool,
        /// Why the last lookup came back empty-handed, if it did.
        note: Option<String>,
    },
    /// `C`: the assistant's sessions.
    Assistant(session_picker::Picker),
    /// `t`: the assistant's todo list, as last read.
    Todo {
        selected: usize,
        /// The new item being typed, after `a`.
        adding: Option<String>,
    },
    /// `i`: what is queued for the assistant, the GitHub events jeet saw and
    /// what the assistant did, newest first.
    Inbox {
        selected: usize,
    },
}

/// Which of the assistant's sessions to attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatSession {
    /// The most recently active, or a new one if it has none.
    Latest,
    New,
    Session(String),
}

/// What a key asks for that needs more than the state: the index, git,
/// tmux, or the terminal. [`run`] carries it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// ⏎: attach to the agent, starting it — or asking which session — first.
    Open {
        task_id: String,
        session: Option<String>,
    },
    /// `e`: the explorer, by way of the worktree picker if there is a choice.
    Explore {
        task_id: String,
    },
    ExploreAt {
        task_id: String,
        place: Place,
    },
    Worktrees {
        task_id: String,
    },
    AddWorktreeForm {
        task_id: String,
    },
    AddWorktree {
        task_id: String,
        repo: String,
        branch: Option<String>,
    },
    AskRemoveWorktree {
        task_id: String,
        line: Box<WorktreeLineKey>,
    },
    RemoveWorktree {
        task_id: String,
        dir_name: String,
        force: bool,
    },
    Prs {
        task_id: String,
    },
    OpenPr {
        repo_id: String,
        number: u64,
    },
    /// `v`: a review task's PR in the review viewer.
    ReviewViewer {
        task_id: String,
    },
    NewTaskForm {
        project: Option<String>,
    },
    CreateTask(TaskSpec),
    Archive {
        task_id: String,
        confirmed: bool,
    },
    Unarchive {
        task_id: String,
    },
    AskDelete {
        task_id: String,
    },
    Delete {
        task_id: String,
    },
    Projects,
    CreateProject {
        name: String,
    },
    EditProject {
        name: String,
    },
    AskDeleteProject {
        name: String,
    },
    DeleteProject {
        name: String,
    },
    Resync {
        task_id: String,
        force: bool,
    },
    Refresh,
    /// `c`, and ⏎ or `n` in its sessions: attach to one of the assistant's
    /// sessions, starting it first if need be.
    Chat(ChatSession),
    /// `C`: the assistant's sessions.
    AssistantSessions,
    RenameSession {
        id: String,
        name: String,
    },
    EndSession {
        id: String,
    },
    /// `*`: make it the session events go to first, or no longer.
    TogglePrimary {
        id: String,
    },
    /// space in the todo panel: item `id`, which says `text`, done or not.
    TodoDone {
        id: usize,
        text: String,
        done: bool,
    },
    TodoAdd {
        text: String,
    },
    /// ⏎ in the inbox: the page an event is about.
    OpenUrl {
        url: String,
    },
}

/// What the remove-worktree prompt needs from the panel line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeLineKey {
    pub dir_name: String,
    pub lines: Vec<String>,
}

pub struct TasksUi {
    /// Every task, active and archived, newest activity first.
    pub rows: Vec<TaskRow>,
    pub view: View,
    pub filter: String,
    /// `/` was pressed: typed characters go to the filter until ⏎ or esc.
    pub filtering: bool,
    /// Indices into `rows` on screen, in display order.
    pub visible: Vec<usize>,
    /// Cursor within `visible`.
    pub selected: usize,
    pub panel: Option<Panel>,
    pub status_line: String,
    /// Set while a slow operation runs, so the screen can say so.
    pub working: Option<String>,
    pub list: ListState,
    /// Where the list was drawn last frame, for mouse clicks.
    pub list_area: Rect,
    /// Set when the user is done; how the shell should be left.
    pub exit: Option<Exit>,
    /// The PR links on screen last frame, made clickable after it is drawn.
    pub links: Vec<term::Link>,
    /// The assistant, once its thread first reports.
    pub assistant: Option<AssistantInfo>,
    /// `o`: tasks the todo list links first, by its priorities.
    pub by_priority: bool,
}

impl Default for TasksUi {
    fn default() -> Self {
        TasksUi::new(Vec::new())
    }
}

impl TasksUi {
    pub fn new(rows: Vec<TaskRow>) -> Self {
        let mut ui = TasksUi {
            rows: Vec::new(),
            view: View::Active,
            filter: String::new(),
            filtering: false,
            visible: Vec::new(),
            selected: 0,
            panel: None,
            status_line: String::new(),
            working: None,
            list: ListState::default(),
            list_area: Rect::default(),
            exit: None,
            links: Vec::new(),
            assistant: None,
            by_priority: false,
        };
        ui.set_rows(rows);
        ui
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_line = msg.into();
    }

    /// Replace the rows with a fresh read of the index, keeping what the
    /// background threads already reported and the cursor where it was.
    pub fn set_rows(&mut self, rows: Vec<TaskRow>) {
        let keep = self.selected_id().map(str::to_string);
        let old = std::mem::take(&mut self.rows);
        self.rows = rows
            .into_iter()
            .map(|mut row| {
                if let Some(was) = old.iter().find(|o| o.task.id == row.task.id) {
                    if row.task.state == was.task.state {
                        row.activity = was.activity.clone();
                    }
                    row.size = was.size;
                    row.last_activity = row.last_activity.max(was.last_activity);
                }
                row
            })
            .collect();
        self.resort(keep.as_deref());
    }

    fn resort(&mut self, keep: Option<&str>) {
        sort_rows(&mut self.rows);
        if self.by_priority {
            let items = self.assistant.as_ref().map_or(&[][..], |a| &a.todo[..]);
            sort_by_todo(&mut self.rows, items);
        }
        self.refresh_visible(keep);
    }

    /// The assistant's todo list, as last read.
    pub fn todo(&self) -> &[ItemView] {
        self.assistant.as_ref().map_or(&[], |a| a.todo.as_slice())
    }

    /// What the inbox panel lists, newest first.
    pub fn feed(&self) -> &[assistant_info::FeedEntry] {
        self.assistant.as_ref().map_or(&[], |a| a.feed.as_slice())
    }

    /// Recompute what is on screen from the view and the filter, leaving
    /// the cursor on `keep` if it is still shown.
    fn refresh_visible(&mut self, keep: Option<&str>) {
        let state = self.view.state();
        let in_view: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].task.state == state)
            .collect();
        let names = in_view.iter().map(|&i| self.rows[i].task.name.as_str());
        self.visible = rank_matches(names, &self.filter)
            .into_iter()
            .map(|i| in_view[i])
            .collect();
        self.selected = keep
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|&i| self.rows[i].task.id == id)
            })
            .unwrap_or_else(|| self.selected.min(self.visible.len().saturating_sub(1)));
    }

    pub fn selected_row(&self) -> Option<&TaskRow> {
        self.visible.get(self.selected).map(|&i| &self.rows[i])
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_row().map(|row| row.task.id.as_str())
    }

    pub fn row(&self, id: &str) -> Option<&TaskRow> {
        self.rows.iter().find(|row| row.task.id == id)
    }

    /// Put the cursor on task `id`, switching to the view that holds it and
    /// dropping a filter that hides it.
    pub fn select(&mut self, id: &str) {
        let Some(row) = self.row(id) else {
            return;
        };
        self.view = match row.task.state {
            TaskState::Active => View::Active,
            TaskState::Archived => View::Archived,
        };
        self.refresh_visible(Some(id));
        if self.selected_id() != Some(id) {
            self.filter.clear();
            self.refresh_visible(Some(id));
        }
    }

    pub fn count(&self, view: View) -> usize {
        self.rows
            .iter()
            .filter(|row| row.task.state == view.state())
            .count()
    }

    /// Take in what a background thread found out.
    pub fn apply(&mut self, update: Update) {
        let keep = self.selected_id().map(str::to_string);
        match update {
            Update::Status {
                id,
                activity,
                last_activity,
            } => {
                let Some(row) = self.rows.iter_mut().find(|r| r.task.id == id) else {
                    return;
                };
                row.activity = activity;
                if last_activity > row.last_activity {
                    row.last_activity = last_activity;
                    self.resort(keep.as_deref());
                }
            }
            Update::Size { id, bytes } => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.task.id == id) {
                    row.size = Some(bytes);
                }
            }
            // Recorded by the screen, which has the index; the panel is
            // updated from what it then reads back.
            Update::Prs(_) => {}
            Update::Assistant(info) => {
                let todo_changed = self.todo() != info.todo.as_slice();
                // An open panel stays on the line it was on as lines come
                // and go above it.
                match &mut self.panel {
                    Some(Panel::Todo { selected, .. }) => {
                        let old = self.assistant.as_ref().map_or(&[][..], |a| &a.todo[..]);
                        *selected = follow(old, &info.todo, *selected, |a, b| a.text == b.text);
                    }
                    Some(Panel::Inbox { selected }) => {
                        let old = self.assistant.as_ref().map_or(&[][..], |a| &a.feed[..]);
                        *selected = follow(old, &info.feed, *selected, |a, b| a == b);
                    }
                    _ => {}
                }
                self.assistant = Some(*info);
                if todo_changed && self.by_priority {
                    self.resort(keep.as_deref());
                }
            }
        }
    }

    /// `item` is how the todo list has it now, after the screen changed it:
    /// show it so before the assistant's thread next reads the list.
    pub fn todo_changed(&mut self, item: &ItemView) {
        let Some(info) = &mut self.assistant else {
            return;
        };
        let Some(slot) = info.todo.get_mut(item.id.wrapping_sub(1)) else {
            return;
        };
        if slot.text == item.text {
            *slot = item.clone();
            if self.by_priority {
                let keep = self.selected_id().map(str::to_string);
                self.resort(keep.as_deref());
            }
        }
    }

    /// Put the cursor on the task a todo item links, the first of them that
    /// is a task, saying why not when none is.
    fn jump_to_task(&mut self, item: &ItemView) -> bool {
        let Some(first) = item.tasks.first() else {
            self.set_status("the item links no task — an @task-slug in its text links one");
            return false;
        };
        let found = item.tasks.iter().find_map(|slug| {
            self.rows
                .iter()
                .find(|row| &row.task.slug == slug)
                .map(|row| (row.task.id.clone(), row.task.name.clone()))
        });
        match found {
            Some((id, name)) => {
                self.select(&id);
                self.set_status(format!("{name}, from the todo list"));
                true
            }
            None => {
                self.set_status(format!("there is no task {first}"));
                false
            }
        }
    }

    /// `prs` are now what the index has for task `task_id`: show them if its
    /// PR panel is open, keeping the cursor on the PR it was on.
    pub fn prs_found(&mut self, task_id: &str, prs: Vec<TaskPrRecord>, note: Option<String>) {
        let Some(Panel::Prs {
            task_id: open,
            prs: shown,
            selected,
            looking,
            note: shown_note,
        }) = &mut self.panel
        else {
            return;
        };
        if open != task_id {
            return;
        }
        let was = shown
            .get(*selected)
            .map(|pr| (pr.repo_id.clone(), pr.number));
        *selected = was
            .and_then(|(repo, number)| {
                prs.iter()
                    .position(|pr| pr.repo_id == repo && pr.number == number)
            })
            .unwrap_or(0);
        *shown = prs;
        *looking = false;
        *shown_note = note;
    }

    fn move_cursor(&mut self, delta: isize) {
        self.selected = moved(self.selected, delta, self.visible.len());
    }

    fn quit(&mut self, exit: Exit) {
        self.exit = Some(exit);
    }

    /// Show `lines` in a message box, coming back to `then` afterwards.
    pub fn message(&mut self, title: &str, lines: Vec<String>, then: Option<Panel>) {
        self.panel = Some(Panel::Message {
            title: title.to_string(),
            lines,
            then: then.map(Box::new),
        });
    }

    /// Turn a key into a change of state, and maybe an action for [`run`].
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // The ways out, before a panel or the filter can swallow them.
        if ctrl {
            match key.code {
                KeyCode::Char('c') => {
                    self.quit(Exit::Stay);
                    return None;
                }
                // Like the explorer's: quit, leaving the shell in the task.
                KeyCode::Char('q') => {
                    let exit = match self.selected_row() {
                        Some(row) if row.dir.is_dir() => Exit::ChangeDir(row.dir.clone()),
                        _ => Exit::Stay,
                    };
                    self.quit(exit);
                    return None;
                }
                _ => {}
            }
        }
        if let Some(panel) = self.panel.take() {
            return self.panel_key(panel, key);
        }
        if self.filtering {
            self.filter_key(key);
            return None;
        }
        self.list_key(key)
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let keep = self.selected_id().map(str::to_string);
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
                self.refresh_visible(keep.as_deref());
            }
            // The filter stays; the letters are commands again.
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                if self.filter.pop().is_none() {
                    self.filtering = false;
                }
                self.refresh_visible(None);
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.refresh_visible(keep.as_deref());
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.filter.push(c);
                // The best match goes to the top, and so does the cursor.
                self.selected = 0;
                self.refresh_visible(None);
            }
            _ => {}
        }
        if self.visible.is_empty() && !self.filter.is_empty() {
            self.set_status("nothing matches — ⌫ to widen the filter");
        } else {
            self.set_status("");
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<Action> {
        let id = self.selected_id().map(str::to_string);
        let needs_task = |ui: &mut TasksUi, make: &dyn Fn(String) -> Action| match &id {
            Some(id) => Some(make(id.clone())),
            None => {
                ui.set_status(NO_TASK);
                None
            }
        };
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refresh_visible(id.as_deref());
                self.set_status("");
                None
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.quit(Exit::Stay);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                None
            }
            KeyCode::PageUp => {
                self.move_cursor(-10);
                None
            }
            KeyCode::PageDown => {
                self.move_cursor(10);
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.visible.len().saturating_sub(1);
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = self.view.other();
                self.selected = 0;
                self.refresh_visible(None);
                self.set_status("");
                None
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.set_status("type to filter · ⏎ keep it · esc drop it");
                None
            }
            KeyCode::Char('?') => {
                self.panel = Some(Panel::Help { scroll: 0 });
                None
            }
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char('c') => Some(Action::Chat(ChatSession::Latest)),
            KeyCode::Char('C') => Some(Action::AssistantSessions),
            KeyCode::Char('t') => {
                self.panel = Some(Panel::Todo {
                    selected: 0,
                    adding: None,
                });
                None
            }
            KeyCode::Char('i') => {
                self.panel = Some(Panel::Inbox { selected: 0 });
                None
            }
            KeyCode::Char('o') => {
                self.by_priority = !self.by_priority;
                self.resort(id.as_deref());
                self.set_status(match self.by_priority {
                    true => "by todo priority: the tasks its open items link first · o undoes",
                    false => "by latest activity",
                });
                None
            }
            KeyCode::Char('n') => Some(Action::NewTaskForm { project: None }),
            KeyCode::Char('P') => Some(Action::Projects),
            KeyCode::Enter => match self.selected_row() {
                Some(row) if row.task.state == TaskState::Archived => {
                    let name = row.task.name.clone();
                    self.set_status(format!("{name} is archived — a brings it back"));
                    None
                }
                _ => needs_task(self, &|task_id| Action::Open {
                    task_id,
                    session: None,
                }),
            },
            KeyCode::Char('e') => needs_task(self, &|task_id| Action::Explore { task_id }),
            KeyCode::Char('w') => needs_task(self, &|task_id| Action::Worktrees { task_id }),
            KeyCode::Char('p') => needs_task(self, &|task_id| Action::Prs { task_id }),
            KeyCode::Char('v') => needs_task(self, &|task_id| Action::ReviewViewer { task_id }),
            KeyCode::Char('a') => match self.view {
                View::Active => needs_task(self, &|task_id| Action::Archive {
                    task_id,
                    confirmed: false,
                }),
                View::Archived => needs_task(self, &|task_id| Action::Unarchive { task_id }),
            },
            KeyCode::Char('D') => needs_task(self, &|task_id| Action::AskDelete { task_id }),
            KeyCode::Char('S') => needs_task(self, &|task_id| Action::Resync {
                task_id,
                force: false,
            }),
            _ => None,
        }
    }

    fn panel_key(&mut self, panel: Panel, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let typed = |key: &KeyEvent| match key.code {
            KeyCode::Char(c) if !ctrl && !alt => Some(c),
            _ => None,
        };
        match panel {
            Panel::Help { scroll } => {
                let scroll = match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => return None,
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        (scroll + 1).min(tasks_ui::HELP.len() as u16)
                    }
                    KeyCode::PageUp | KeyCode::Home => 0,
                    _ => scroll,
                };
                self.panel = Some(Panel::Help { scroll });
                None
            }
            Panel::Message { title, lines, then } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.panel = then.map(|p| *p)
                    }
                    _ => self.panel = Some(Panel::Message { title, lines, then }),
                }
                None
            }
            Panel::Confirm {
                title,
                lines,
                action,
            } => {
                let yes = matches!(key.code, KeyCode::Char('y' | 'Y'));
                let force = matches!(key.code, KeyCode::Char('f' | 'F'));
                let no = matches!(key.code, KeyCode::Char('n' | 'N') | KeyCode::Esc);
                match (&action, yes, force, no) {
                    (Pending::Archive { task_id }, true, _, _) => Some(Action::Archive {
                        task_id: task_id.clone(),
                        confirmed: true,
                    }),
                    (Pending::Delete { task_id }, true, _, _) => Some(Action::Delete {
                        task_id: task_id.clone(),
                    }),
                    (Pending::RemoveWorktree { task_id, dir_name }, y, f, _) if y || f => {
                        Some(Action::RemoveWorktree {
                            task_id: task_id.clone(),
                            dir_name: dir_name.clone(),
                            force: f,
                        })
                    }
                    (Pending::RemoveWorktree { task_id, .. }, _, _, true) => {
                        Some(Action::Worktrees {
                            task_id: task_id.clone(),
                        })
                    }
                    (Pending::DeleteProject { name }, true, _, _) => {
                        Some(Action::DeleteProject { name: name.clone() })
                    }
                    (Pending::DeleteProject { .. }, _, _, true) => Some(Action::Projects),
                    (Pending::ForceResync { task_id }, _, true, _) => Some(Action::Resync {
                        task_id: task_id.clone(),
                        force: true,
                    }),
                    (_, _, _, true) => {
                        self.set_status("nothing changed");
                        None
                    }
                    _ => {
                        self.panel = Some(Panel::Confirm {
                            title,
                            lines,
                            action,
                        });
                        None
                    }
                }
            }
            Panel::Sessions {
                task_id,
                sessions,
                mut selected,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Enter => sessions.get(selected).map(|s| Action::Open {
                    task_id: task_id.clone(),
                    session: Some(s.id.clone()),
                }),
                code => {
                    selected = list_move(code, selected, sessions.len());
                    self.panel = Some(Panel::Sessions {
                        task_id,
                        sessions,
                        selected,
                    });
                    None
                }
            },
            Panel::Places {
                task_id,
                places,
                mut selected,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Enter | KeyCode::Right => {
                    places.get(selected).map(|place| Action::ExploreAt {
                        task_id: task_id.clone(),
                        place: place.clone(),
                    })
                }
                code => {
                    selected = list_move(code, selected, places.len());
                    self.panel = Some(Panel::Places {
                        task_id,
                        places,
                        selected,
                    });
                    None
                }
            },
            Panel::Worktrees {
                task_id,
                lines,
                mut selected,
            } => {
                let line = lines.get(selected).cloned();
                let action = match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'w') => return None,
                    KeyCode::Char('r') => return Some(Action::Worktrees { task_id }),
                    KeyCode::Char('n') => return Some(Action::AddWorktreeForm { task_id }),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('e') => match line {
                        Some(line) if line.present => Some(Action::ExploreAt {
                            task_id: task_id.clone(),
                            place: Place {
                                label: line.record.dir_name.clone(),
                                path: line.path.clone(),
                                repo_id: Some(line.record.repo_id.clone()),
                            },
                        }),
                        Some(line) => {
                            self.set_status(format!(
                                "{} is not on disk — the task is archived",
                                line.record.dir_name
                            ));
                            None
                        }
                        None => None,
                    },
                    KeyCode::Char('D') => match line {
                        Some(line) => {
                            return Some(Action::AskRemoveWorktree {
                                task_id,
                                line: Box::new(WorktreeLineKey {
                                    dir_name: line.record.dir_name.clone(),
                                    lines: removal_summary(&line),
                                }),
                            })
                        }
                        None => None,
                    },
                    code => {
                        selected = list_move(code, selected, lines.len());
                        None
                    }
                };
                if action.is_none() {
                    self.panel = Some(Panel::Worktrees {
                        task_id,
                        lines,
                        selected,
                    });
                }
                action
            }
            Panel::AddWorktree {
                task_id,
                mut repos,
                mut branch,
                mut field,
            } => {
                match (key.code, field) {
                    (KeyCode::Esc, _) => return Some(Action::Worktrees { task_id }),
                    (KeyCode::Enter, _) => {
                        // A filter that matches nothing is taken as it was
                        // typed: a URL, or a repository the store lacks yet,
                        // which adding a worktree clones.
                        let repo = match repos.current() {
                            Some(repo) => Some(repo.to_string()),
                            None => Some(repos.filter.trim().to_string()).filter(|r| !r.is_empty()),
                        };
                        match repo {
                            Some(repo) => {
                                return Some(Action::AddWorktree {
                                    task_id,
                                    repo,
                                    branch: Some(branch.trim().to_string())
                                        .filter(|b| !b.is_empty()),
                                })
                            }
                            None => self.set_status("pick a repository, or type a clone URL"),
                        }
                    }
                    (KeyCode::Tab | KeyCode::BackTab, AddField::Repo) => field = AddField::Branch,
                    (KeyCode::Tab | KeyCode::BackTab, AddField::Branch) => field = AddField::Repo,
                    (KeyCode::Up, AddField::Repo) => repos.move_by(-1),
                    (KeyCode::Down, AddField::Repo) => repos.move_by(1),
                    (KeyCode::Backspace, AddField::Repo) => {
                        repos.pop();
                    }
                    (KeyCode::Backspace, AddField::Branch) => {
                        branch.pop();
                    }
                    (KeyCode::Char('u'), AddField::Branch) if ctrl => branch.clear(),
                    (_, AddField::Repo) => {
                        if let Some(c) = typed(&key) {
                            repos.push(c);
                        }
                    }
                    (_, AddField::Branch) => {
                        if let Some(c) = typed(&key) {
                            branch.push(c);
                        }
                    }
                }
                self.panel = Some(Panel::AddWorktree {
                    task_id,
                    repos,
                    branch,
                    field,
                });
                None
            }
            Panel::NewTask(mut form) => {
                match (key.code, form.field) {
                    (KeyCode::Esc, _) => return None,
                    (KeyCode::Enter, _) => match form.spec() {
                        Ok(spec) => {
                            // Left up while the task is made, to come back to
                            // if that fails.
                            self.panel = Some(Panel::NewTask(form));
                            return Some(Action::CreateTask(spec));
                        }
                        Err(why) => form.error = Some(why),
                    },
                    (KeyCode::Tab, _) => form.field = form.field.step(1),
                    (KeyCode::BackTab, _) => form.field = form.field.step(-1),
                    (KeyCode::Up, Field::Repos) => form.repos.move_by(-1),
                    (KeyCode::Down, Field::Repos) => form.repos.move_by(1),
                    (KeyCode::Up, _) => form.field = form.field.step(-1),
                    (KeyCode::Down, _) => form.field = form.field.step(1),
                    (KeyCode::Left, Field::Project) => form.cycle_project(-1),
                    (KeyCode::Right | KeyCode::Char(' '), Field::Project) => form.cycle_project(1),
                    (KeyCode::Char(' '), Field::Repos) => form.repos.toggle(),
                    (KeyCode::Backspace, Field::Repos) => {
                        form.repos.pop();
                    }
                    (KeyCode::Backspace, Field::Name) => {
                        form.name.pop();
                    }
                    (KeyCode::Backspace, Field::Branch) => {
                        form.branch.pop();
                    }
                    (KeyCode::Char('u'), Field::Name) if ctrl => form.name.clear(),
                    (KeyCode::Char('u'), Field::Branch) if ctrl => form.branch.clear(),
                    (_, field) => match (typed(&key), field) {
                        (Some(c), Field::Name) => form.name.push(c),
                        (Some(c), Field::Branch) => form.branch.push(c),
                        (Some(c), Field::Repos) => form.repos.push(c),
                        _ => {}
                    },
                }
                if key.code != KeyCode::Enter {
                    form.error = None;
                }
                self.panel = Some(Panel::NewTask(form));
                None
            }
            Panel::Projects {
                lines,
                mut selected,
            } => {
                let name = lines.get(selected).map(|l| l.name.clone());
                let action = match (key.code, name) {
                    (KeyCode::Esc | KeyCode::Char('q' | 'P'), _) => return None,
                    (KeyCode::Char('r'), _) => return Some(Action::Projects),
                    (KeyCode::Char('n'), _) => {
                        self.panel = Some(Panel::NewProject {
                            input: String::new(),
                        });
                        return None;
                    }
                    (KeyCode::Char('e'), Some(name)) => Some(Action::EditProject { name }),
                    (KeyCode::Char('D'), Some(name)) => Some(Action::AskDeleteProject { name }),
                    (KeyCode::Enter, Some(name)) => Some(Action::NewTaskForm {
                        project: Some(name),
                    }),
                    (code, _) => {
                        selected = list_move(code, selected, lines.len());
                        None
                    }
                };
                if action.is_none() {
                    self.panel = Some(Panel::Projects { lines, selected });
                }
                action
            }
            Panel::NewProject { mut input } => {
                match key.code {
                    KeyCode::Esc => return Some(Action::Projects),
                    KeyCode::Enter => {
                        let name = input.trim().to_string();
                        if !name.is_empty() {
                            return Some(Action::CreateProject { name });
                        }
                        self.set_status("a project needs a name");
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char('u') if ctrl => input.clear(),
                    _ => {
                        if let Some(c) = typed(&key) {
                            input.push(c);
                        }
                    }
                }
                self.panel = Some(Panel::NewProject { input });
                None
            }
            Panel::Prs {
                task_id,
                prs,
                mut selected,
                looking,
                note,
            } => {
                let action = match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'p') => return None,
                    KeyCode::Enter | KeyCode::Char('o') => {
                        prs.get(selected).map(|pr| Action::OpenPr {
                            repo_id: pr.repo_id.clone(),
                            number: pr.number,
                        })
                    }
                    code => {
                        selected = list_move(code, selected, prs.len());
                        None
                    }
                };
                self.panel = Some(Panel::Prs {
                    task_id,
                    prs,
                    selected,
                    looking,
                    note,
                });
                action
            }
            Panel::Assistant(mut picker) => {
                let action = match picker.on_key(key) {
                    Step::Stay => None,
                    Step::Done(Pick::Quit) => return None,
                    Step::Done(Pick::New) => Some(Action::Chat(ChatSession::New)),
                    Step::Done(Pick::Session(id)) => Some(Action::Chat(ChatSession::Session(id))),
                    Step::Rename { id, name } => Some(Action::RenameSession { id, name }),
                    Step::End { id } => Some(Action::EndSession { id }),
                    Step::Primary { id } => Some(Action::TogglePrimary { id }),
                };
                // Left up for the screen to bring up to date.
                self.panel = Some(Panel::Assistant(picker));
                action
            }
            Panel::Todo {
                mut selected,
                adding: Some(mut input),
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.panel = Some(Panel::Todo {
                            selected,
                            adding: None,
                        });
                        return None;
                    }
                    KeyCode::Enter if input.trim().is_empty() => {
                        self.set_status("a todo item needs some text")
                    }
                    KeyCode::Enter => {
                        self.panel = Some(Panel::Todo {
                            selected,
                            adding: None,
                        });
                        return Some(Action::TodoAdd { text: input });
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char('u') if ctrl => input.clear(),
                    KeyCode::Up | KeyCode::Down => {
                        selected = list_move(key.code, selected, self.todo().len())
                    }
                    _ => {
                        if let Some(c) = typed(&key) {
                            input.push(c);
                        }
                    }
                }
                self.panel = Some(Panel::Todo {
                    selected,
                    adding: Some(input),
                });
                None
            }
            Panel::Todo {
                mut selected,
                adding: None,
            } => {
                let item = self.todo().get(selected).cloned();
                let mut adding = None;
                let action = match (key.code, item) {
                    (KeyCode::Esc | KeyCode::Char('q' | 't'), _) => return None,
                    (KeyCode::Char('a'), _) => {
                        adding = Some(String::new());
                        None
                    }
                    (KeyCode::Char(' '), Some(item)) => Some(Action::TodoDone {
                        id: item.id,
                        done: !item.done,
                        text: item.text,
                    }),
                    (KeyCode::Enter, Some(item)) => {
                        if self.jump_to_task(&item) {
                            return None;
                        }
                        None
                    }
                    (code, _) => {
                        selected = list_move(code, selected, self.todo().len());
                        None
                    }
                };
                self.panel = Some(Panel::Todo { selected, adding });
                action
            }
            Panel::Inbox { mut selected } => {
                let entry = self.feed().get(selected).cloned();
                let action = match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'i') => return None,
                    KeyCode::Enter | KeyCode::Char('o') => match entry.and_then(|e| e.url) {
                        Some(url) => Some(Action::OpenUrl { url }),
                        None => {
                            self.set_status("nothing to open: only GitHub events link to a page");
                            None
                        }
                    },
                    code => {
                        selected = list_move(code, selected, self.feed().len());
                        None
                    }
                };
                self.panel = Some(Panel::Inbox { selected });
                action
            }
        }
    }

    /// A click or the wheel, while the list is in front.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.panel.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::ScrollDown => self.move_cursor(1),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                let inside = mouse.column > area.x
                    && mouse.column < area.x + area.width.saturating_sub(1)
                    && mouse.row > area.y
                    && mouse.row < area.y + area.height.saturating_sub(1);
                if inside {
                    let row = self.list.offset() + (mouse.row - area.y - 1) as usize;
                    if row < self.visible.len() {
                        self.selected = row;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Newest activity first; ties by name, so the order is stable.
pub fn sort_rows(rows: &mut [TaskRow]) {
    rows.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.task.name.to_lowercase().cmp(&b.task.name.to_lowercase()))
            .then_with(|| a.task.id.cmp(&b.task.id))
    });
}

/// Tasks the open todo items link first, by the best item that links each;
/// the rest after them. Stable, so each keeps the order it had otherwise.
pub fn sort_by_todo(rows: &mut [TaskRow], items: &[ItemView]) {
    let ranks = assistant_info::todo_ranks(items);
    rows.sort_by_key(|row| {
        ranks
            .get(&row.task.slug)
            .copied()
            .unwrap_or((u16::MAX, usize::MAX))
    });
}

/// Where the cursor at `selected` in `old` goes in `new`: to the same line
/// if it is still there, else where it was, within the list.
fn follow<T>(old: &[T], new: &[T], selected: usize, same: impl Fn(&T, &T) -> bool) -> usize {
    old.get(selected)
        .and_then(|was| new.iter().position(|line| same(was, line)))
        .unwrap_or(selected)
        .min(new.len().saturating_sub(1))
}

/// `cursor` moved by `delta` within a list of `len`, clamped to it.
fn moved(cursor: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (cursor as isize + delta).clamp(0, len as isize - 1) as usize
}

/// The arrow keys of a panel's list.
fn list_move(code: KeyCode, selected: usize, len: usize) -> usize {
    match code {
        KeyCode::Up | KeyCode::Char('k') => moved(selected, -1, len),
        KeyCode::Down | KeyCode::Char('j') => moved(selected, 1, len),
        KeyCode::PageUp | KeyCode::Home => 0,
        KeyCode::PageDown | KeyCode::End => len.saturating_sub(1),
        _ => selected,
    }
}

/// `task 1`, `task 2`… — the first that no task has as its name or slug.
pub fn default_task_name<'a>(taken: impl IntoIterator<Item = &'a TaskRecord>) -> String {
    let taken: Vec<&TaskRecord> = taken.into_iter().collect();
    (1..)
        .map(|n| format!("task {n}"))
        .find(|name| {
            let slug = tasks::slugify(name);
            !taken
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(name) || t.slug == slug)
        })
        .unwrap_or_else(|| "task".to_string())
}

/// Where `e` can open: the canvas first, then each worktree on disk.
pub fn places(dir: &Path, worktrees: &[TaskWorktreeRecord]) -> Vec<Place> {
    let mut places = vec![Place {
        label: "task canvas (the task directory)".to_string(),
        path: dir.to_path_buf(),
        repo_id: None,
    }];
    for row in worktrees {
        let path = dir.join(&row.dir_name);
        if !path.is_dir() {
            continue;
        }
        let branch = row.branch.as_deref().unwrap_or("detached");
        places.push(Place {
            label: format!("{} · {} · {branch}", row.dir_name, row.repo_id),
            path,
            repo_id: Some(row.repo_id.clone()),
        });
    }
    places
}

/// The place `e` opens straight on, when there is no choice to make: the
/// only worktree, or the canvas when there are none.
pub fn only_place(places: &[Place]) -> Option<&Place> {
    match places {
        [canvas] => Some(canvas),
        [_, worktree] => Some(worktree),
        _ => None,
    }
}

/// What removing a worktree would lose, for the confirm prompt.
fn removal_summary(line: &WorktreeLine) -> Vec<String> {
    let mut lines = vec![
        format!(
            "{} ({}, {})",
            line.record.dir_name, line.record.repo_id, line.head
        ),
        line.path.display().to_string(),
        String::new(),
    ];
    if !line.present {
        lines.push("not on disk; only the record goes".to_string());
        return lines;
    }
    let status = &line.status;
    if status.dirty > 0 {
        lines.push(format!("⚠ {}", plural(status.dirty, "uncommitted change")));
    }
    if status.ahead > 0 {
        lines.push(format!(
            "⚠ {} not on the default branch",
            plural(status.ahead, "commit")
        ));
    }
    if let Some(why) = &status.unknown {
        lines.push(format!("⚠ could not assess it: {why}"));
    }
    if status.ignored > 0 {
        lines.push(format!(
            "⚠ {} (.env, build output) will be deleted",
            plural(status.ignored, "ignored file")
        ));
    }
    if !status.has_anything_to_lose() {
        lines.push("nothing to lose — clean and fully merged".to_string());
    }
    lines
}

/// What the archive confirm says: that a running agent is ended, and which
/// files git ignores go with the worktrees, as nothing keeps them.
fn archive_warning(name: &str, running: bool, unkept: &[String]) -> Vec<String> {
    let mut lines = match running {
        true => vec![
            format!("{name} has its agent running"),
            "archiving ends it, then frees the worktrees' disk".into(),
        ],
        false => vec![format!("archiving {name} frees its worktrees' disk")],
    };
    lines.push("(uncommitted work is kept, and comes back on unarchive)".into());
    if !unkept.is_empty() {
        lines.push(String::new());
        lines.push("⚠ files git ignores are not kept — they go with the worktrees:".into());
        lines.extend(unkept.iter().map(|line| format!("  {line}")));
    }
    lines
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Sessions newest first, by what the index recorded and the hooks reported.
pub fn sort_sessions(sessions: &mut [SessionChoice]) {
    sessions.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.id.cmp(&b.id)));
}

// ---------------------------------------------------------------------------
// Background
// ---------------------------------------------------------------------------

/// What the status poller needs to know about one task: no index, so it can
/// run on a thread of its own.
#[derive(Debug, Clone)]
pub struct Probe {
    pub id: String,
    pub dir: PathBuf,
    pub session: String,
    pub touched: i64,
    pub active: bool,
}

/// The background threads: their inboxes, and the one channel they all
/// answer on. Dropping it stops them.
struct Background {
    probes: Sender<Vec<Probe>>,
    sizes: Sender<Vec<(String, PathBuf)>>,
    prs: Sender<Vec<pr_history::Job>>,
    assistant: Sender<assistant_info::Probe>,
    updates: Receiver<Update>,
}

impl Background {
    fn start() -> Self {
        let (tx, updates) = mpsc::channel();
        let (probes, probe_rx) = mpsc::channel();
        let (sizes, size_rx) = mpsc::channel();
        let (prs, pr_rx) = mpsc::channel();
        let (assistant, assistant_rx) = mpsc::channel();
        let status_tx = tx.clone();
        let size_tx = tx.clone();
        let assistant_tx = tx.clone();
        std::thread::spawn(move || poll_statuses(probe_rx, status_tx));
        std::thread::spawn(move || walk_sizes(size_rx, size_tx));
        std::thread::spawn(move || poll_assistant(assistant_rx, assistant_tx));
        std::thread::spawn(move || find_prs(pr_rx, tx));
        Background {
            probes,
            sizes,
            prs,
            assistant,
            updates,
        }
    }

    /// Have the assistant's thread look at it now, with its sessions as the
    /// index has them.
    fn look_at_assistant(&self, probe: assistant_info::Probe) {
        let _ = self.assistant.send(probe);
    }

    /// Have the PR thread look these tasks' PRs up, before any it has yet
    /// to get to.
    fn find_prs(&self, jobs: Vec<pr_history::Job>) {
        if !jobs.is_empty() {
            let _ = self.prs.send(jobs);
        }
    }

    /// Point both threads at the tasks now listed, and have them report
    /// straight away.
    fn watch(&self, rows: &[TaskRow]) {
        self.probe(rows);
        self.resize(rows);
    }

    /// Have the status poller read every task's status now.
    fn probe(&self, rows: &[TaskRow]) {
        let probes = rows
            .iter()
            .map(|row| Probe {
                id: row.task.id.clone(),
                dir: row.dir.clone(),
                session: runtime::session_name(&row.task),
                touched: row.task.last_activity,
                active: row.task.state == TaskState::Active,
            })
            .collect();
        let _ = self.probes.send(probes);
    }

    fn resize(&self, rows: &[TaskRow]) {
        let dirs = rows
            .iter()
            .map(|row| (row.task.id.clone(), row.dir.clone()))
            .collect();
        let _ = self.sizes.send(dirs);
    }
}

/// Read every task's agent status now and every [`POLL_EVERY`], and again at
/// once whenever a new list of tasks arrives.
fn poll_statuses(probes: Receiver<Vec<Probe>>, tx: Sender<Update>) {
    let tmux_ok = tmux::available();
    let mut current: Vec<Probe> = Vec::new();
    loop {
        for probe in &current {
            if tx.send(probe_status(probe, tmux_ok)).is_err() {
                return;
            }
        }
        match probes.recv_timeout(POLL_EVERY) {
            Ok(list) => current = list,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

pub fn probe_status(probe: &Probe, tmux_ok: bool) -> Update {
    Update::Status {
        id: probe.id.clone(),
        activity: probe
            .active
            .then(|| runtime::activity_at(&probe.dir, &probe.session, tmux_ok)),
        last_activity: runtime::last_activity_at(&probe.dir, probe.touched),
    }
}

/// Look at the assistant now and every [`POLL_EVERY`], and again at once
/// whenever a new probe arrives.
fn poll_assistant(probes: Receiver<assistant_info::Probe>, tx: Sender<Update>) {
    let tmux_ok = tmux::available();
    let mut current: Option<assistant_info::Probe> = None;
    loop {
        if let Some(probe) = &current {
            let windows = match tmux_ok {
                true => tmux::windows(assistant::TMUX_SESSION),
                false => Vec::new(),
            };
            let info = assistant_info::gather(probe, &windows);
            if tx.send(Update::Assistant(Box::new(info))).is_err() {
                return;
            }
        }
        match probes.recv_timeout(POLL_EVERY) {
            Ok(probe) => current = Some(probe),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Walk every task's directory for its size, again every [`RESIZE_EVERY`];
/// a new list of tasks starts the walk over.
fn walk_sizes(jobs: Receiver<Vec<(String, PathBuf)>>, tx: Sender<Update>) {
    let mut current: Vec<(String, PathBuf)> = Vec::new();
    'walk: loop {
        for (id, dir) in &current {
            match jobs.try_recv() {
                Ok(list) => {
                    current = list;
                    continue 'walk;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            let update = Update::Size {
                id: id.clone(),
                bytes: dir_size(dir),
            };
            if tx.send(update).is_err() {
                return;
            }
        }
        match jobs.recv_timeout(RESIZE_EVERY) {
            Ok(list) => current = list,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Look up each task's PRs in turn, as [`enqueue`] orders them.
fn find_prs(jobs: Receiver<Vec<pr_history::Job>>, tx: Sender<Update>) {
    let mut queue: VecDeque<pr_history::Job> = VecDeque::new();
    loop {
        // Everything asked for so far, waiting for something when idle.
        loop {
            let batch = match queue.is_empty() {
                true => match jobs.recv() {
                    Ok(batch) => batch,
                    Err(_) => return,
                },
                false => match jobs.try_recv() {
                    Ok(batch) => batch,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                },
            };
            enqueue(&mut queue, batch);
        }
        let Some(job) = queue.pop_front() else {
            continue;
        };
        if tx.send(Update::Prs(pr_history::discover(&job))).is_err() {
            return;
        }
    }
}

/// Put `batch` at the front of the queue, in its own order: the latest ask
/// is the one somebody is waiting on — the PR panel just opened. An older
/// job for the same task goes, as the new one supersedes it.
pub fn enqueue(queue: &mut VecDeque<pr_history::Job>, batch: Vec<pr_history::Job>) {
    queue.retain(|old| !batch.iter().any(|new| new.task_id == old.task_id));
    for job in batch.into_iter().rev() {
        queue.push_front(job);
    }
}

/// Bytes in the files under `dir`. Symlinks are not followed, so a link to
/// somewhere big costs nothing; a worktree's `.git` is a file pointing into
/// the trunk, so the shared object store is never counted either.
pub fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Where the tasks screen opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    List,
    /// `jeet projects` on a terminal: the projects panel, over the list.
    Projects,
}

/// Run the tasks screen until the user leaves it, returning where the shell
/// should end up.
pub fn run(app: &App, start: Start) -> Result<Exit> {
    // Keep git's own output off the alternate screen.
    git::set_capture_output(true);
    let mut terminal = term::init_terminal()?;
    let mut screen = Screen {
        app,
        terminal: &mut terminal,
        ui: TasksUi::default(),
        bg: Background::start(),
    };
    let result = screen.go(start);
    let exit = screen.ui.exit.take();
    drop(screen);
    term::restore_terminal(&mut terminal)?;
    git::set_capture_output(false);
    result?;
    Ok(exit.unwrap_or(Exit::Stay))
}

struct Screen<'a> {
    app: &'a App,
    terminal: &'a mut Tui,
    ui: TasksUi,
    bg: Background,
}

impl Screen<'_> {
    fn go(&mut self, start: Start) -> Result<()> {
        self.ui
            .set_status("? for keys · ⏎ opens a task's agent · n starts a task");
        self.refresh(true);
        if start == Start::Projects {
            self.perform(Action::Projects);
        }
        let mut last_read = Instant::now();
        let mut last_watch = Instant::now();
        while self.ui.exit.is_none() {
            while let Ok(update) = self.bg.updates.try_recv() {
                self.take(update);
            }
            // GitHub is polled by a jeet of its own, started off the draw
            // path; it reports through the assistant, tasks and
            // notifications, not this screen.
            if last_watch.elapsed() >= WATCH_EVERY {
                last_watch = Instant::now();
                let home = self.app.home.clone();
                let config = self.app.config.clone();
                std::thread::spawn(move || watch::maybe_background(&home, &config));
            }
            // The agent adds and removes worktrees as it goes, and other
            // jeets make and archive tasks: keep the rows in step.
            if last_read.elapsed() >= REREAD_EVERY && self.ui.panel.is_none() {
                last_read = Instant::now();
                if let Err(e) = self.reread() {
                    self.ui.set_status(format!("could not read tasks: {e:#}"));
                }
            }
            let frame = self
                .terminal
                .draw(|frame| tasks_ui::draw(frame, &mut self.ui))?;
            if !self.ui.links.is_empty() {
                let buffer = frame.buffer.clone();
                term::paint_links(self.terminal, &buffer, &self.ui.links)?;
            }
            // Wake for the background threads' answers as well as for keys.
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = self.ui.handle_key(key) {
                        self.perform(action);
                    }
                }
                Event::Mouse(mouse) => self.ui.handle_mouse(mouse),
                _ => {}
            }
        }
        Ok(())
    }

    /// Take in what a background thread found out. PRs are written to the
    /// index here, on the screen's thread: that is quick, and the threads
    /// that ask GitHub have no index of their own.
    fn take(&mut self, update: Update) {
        let Update::Prs(found) = update else {
            self.ui.apply(update);
            return;
        };
        if let Err(e) = pr_history::record(self.app, &found) {
            self.ui
                .set_status(format!("could not record pull requests: {e:#}"));
            return;
        }
        match self.app.db.list_task_prs(&found.task_id) {
            Ok(prs) => {
                let note = found.errors.first().cloned();
                self.ui.prs_found(&found.task_id, prs, note);
            }
            Err(e) => self
                .ui
                .set_status(format!("could not read pull requests: {e:#}")),
        }
    }

    /// Have every active task's PRs looked up in the background.
    fn discover_prs(&mut self) {
        let jobs = self
            .ui
            .rows
            .iter()
            .filter(|row| row.task.state == TaskState::Active)
            .filter_map(|row| pr_history::job(self.app, &row.task).ok())
            .collect();
        self.bg.find_prs(jobs);
    }

    /// Run `job` off the draw path, with a spinner saying `label`.
    fn progress<T: Send>(&mut self, label: &str, job: impl FnOnce() -> T + Send) -> Result<T> {
        let ui = &mut self.ui;
        let outcome = term::with_progress(
            self.terminal,
            label,
            |frame, working| {
                ui.working = Some(working.to_string());
                tasks_ui::draw(frame, ui);
            },
            job,
        );
        ui.working = None;
        outcome
    }

    /// Read the tasks again — after reconciling the index with the disk,
    /// when `reconcile` — and point the background threads at them.
    fn refresh(&mut self, reconcile: bool) {
        let app = self.app;
        let loaded = match reconcile {
            true => self
                .progress("reading tasks", || {
                    let warning = tasks::reconcile(app)
                        .err()
                        .map(|e| format!("could not reconcile worktrees: {e:#}"));
                    load_rows(app).map(|rows| (rows, warning))
                })
                .and_then(|inner| inner),
            false => load_rows(app).map(|rows| (rows, None)),
        };
        match loaded {
            Ok((rows, warning)) => {
                let changed = self.set_rows(rows);
                // Sizes are walked again only when there are new tasks to
                // measure: a walk restarted on every action never finishes
                // a big task.
                if reconcile || changed {
                    self.bg.watch(&self.ui.rows);
                } else {
                    self.bg.probe(&self.ui.rows);
                }
                // Only on a full refresh — opening, and `r` — as it is a
                // `gh` call per worktree.
                if reconcile {
                    self.discover_prs();
                }
                if let Some(warning) = warning {
                    self.ui.set_status(warning);
                }
            }
            Err(e) => self.ui.set_status(format!("could not read tasks: {e:#}")),
        }
        self.look_at_assistant();
    }

    /// Have the assistant looked at again, with its sessions as the index
    /// has them now.
    fn look_at_assistant(&mut self) {
        match assistant_info::probe(self.app) {
            Ok(probe) => self.bg.look_at_assistant(probe),
            Err(e) => self
                .ui
                .set_status(format!("could not read the assistant: {e:#}")),
        }
    }

    /// Read the tasks again, quietly: the background threads hear of it
    /// only when tasks came, went or changed state.
    fn reread(&mut self) -> Result<()> {
        let rows = load_rows(self.app)?;
        if self.set_rows(rows) {
            self.bg.watch(&self.ui.rows);
        }
        // New sessions are recorded in the index as they start.
        self.bg.look_at_assistant(assistant_info::probe(self.app)?);
        Ok(())
    }

    /// Replace the rows, saying whether the tasks themselves changed rather
    /// than only what is known about them.
    fn set_rows(&mut self, rows: Vec<TaskRow>) -> bool {
        let tasks = |rows: &[TaskRow]| {
            let mut ids: Vec<(String, TaskState)> = rows
                .iter()
                .map(|r| (r.task.id.clone(), r.task.state))
                .collect();
            ids.sort_by(|a, b| a.0.cmp(&b.0));
            ids
        };
        let before = tasks(&self.ui.rows);
        self.ui.set_rows(rows);
        tasks(&self.ui.rows) != before
    }

    fn task(&self, id: &str) -> Option<TaskRecord> {
        self.ui.row(id).map(|row| row.task.clone())
    }

    /// Carry out `action`; whatever goes wrong ends up on screen.
    fn perform(&mut self, action: Action) {
        if let Err(e) = self.try_perform(action) {
            self.ui.message("error", vec![format!("{e:#}")], None);
        }
    }

    fn try_perform(&mut self, action: Action) -> Result<()> {
        let app = self.app;
        match action {
            Action::Refresh => {
                self.refresh(true);
                self.ui.set_status("refreshed");
            }
            Action::Open { task_id, session } => self.open(&task_id, session)?,
            Action::Explore { task_id } => {
                let Some(row) = self.ui.row(&task_id).cloned() else {
                    return Ok(());
                };
                let places = places(&row.dir, &row.worktrees);
                match only_place(&places) {
                    Some(place) => self.explore_at(place.clone())?,
                    None => {
                        self.ui.panel = Some(Panel::Places {
                            task_id,
                            places,
                            selected: 0,
                        })
                    }
                }
            }
            Action::ExploreAt { place, .. } => self.explore_at(place)?,
            Action::Worktrees { task_id } => self.worktrees(&task_id)?,
            Action::AddWorktreeForm { task_id } => {
                let repos = app.db.list_repos(None)?.into_iter().map(|r| r.id).collect();
                self.ui.panel = Some(Panel::AddWorktree {
                    task_id,
                    repos: Picker::new(repos),
                    branch: String::new(),
                    field: AddField::Repo,
                });
            }
            Action::AddWorktree {
                task_id,
                repo,
                branch,
            } => {
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                let added = self
                    .progress(&format!("adding a worktree of {repo}"), || {
                        tasks::add_worktree(app, &task, &repo, branch.as_deref(), None)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                self.bg.resize(&self.ui.rows);
                match added {
                    Ok(added) => {
                        self.worktrees(&task_id)?;
                        self.ui.set_status(with_warnings(
                            format!("added {}", added.record.dir_name),
                            &added.warnings,
                        ));
                    }
                    Err(e) => {
                        let back = self.worktrees_panel(&task_id).ok();
                        self.ui
                            .message("could not add the worktree", vec![format!("{e:#}")], back);
                    }
                }
            }
            Action::AskRemoveWorktree { task_id, line } => {
                self.ui.panel = Some(Panel::Confirm {
                    title: "remove worktree".into(),
                    lines: line.lines,
                    action: Pending::RemoveWorktree {
                        task_id,
                        dir_name: line.dir_name,
                    },
                });
            }
            Action::RemoveWorktree {
                task_id,
                dir_name,
                force,
            } => {
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                let removed = self
                    .progress(&format!("removing {dir_name}"), || {
                        tasks::remove_worktree(app, &task, &dir_name, force)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                self.bg.resize(&self.ui.rows);
                match removed {
                    Ok(warnings) => {
                        self.worktrees(&task_id)?;
                        self.ui
                            .set_status(with_warnings(format!("removed {dir_name}"), &warnings));
                    }
                    Err(e) => {
                        let back = self.worktrees_panel(&task_id).ok();
                        self.ui.message(
                            "not removed",
                            vec![
                                dir_name,
                                format!("{e:#}"),
                                String::new(),
                                "press D again and then f to remove it anyway".into(),
                            ],
                            back,
                        );
                    }
                }
            }
            Action::Prs { task_id } => self.prs(&task_id)?,
            Action::OpenPr { repo_id, number } => {
                let Some(repo) = app.db.get_repo(&repo_id)? else {
                    self.ui
                        .set_status(format!("{repo_id} is no longer in the store"));
                    return Ok(());
                };
                let trunk = PathBuf::from(&repo.trunk_path);
                let opened = self
                    .progress("opening the browser", || {
                        github::open_in_browser(&trunk, number)
                    })
                    .and_then(|inner| inner);
                match opened {
                    Ok(()) => self
                        .ui
                        .set_status(format!("opened #{number} in the browser")),
                    Err(e) => self
                        .ui
                        .set_status(format!("could not open #{number}: {e:#}")),
                }
            }
            Action::ReviewViewer { task_id } => self.review_viewer(&task_id)?,
            Action::NewTaskForm { project } => {
                let projects = projects::list(app)?;
                let repos = app.db.list_repos(None)?.into_iter().map(|r| r.id).collect();
                let default_name = default_task_name(self.ui.rows.iter().map(|r| &r.task));
                self.ui.panel = Some(Panel::NewTask(NewTaskForm::new(
                    default_name,
                    projects,
                    project.as_deref(),
                    repos,
                )));
            }
            Action::CreateTask(spec) => {
                let label = match spec.repos.len() + usize::from(spec.project.is_some()) {
                    0 => format!("creating {}", spec.name),
                    _ => format!("creating {} and its worktrees", spec.name),
                };
                let made = self
                    .progress(&label, || {
                        tasks::create(
                            app,
                            &NewTask {
                                name: &spec.name,
                                project: spec.project.as_deref(),
                                repos: &spec.repos,
                                branch: spec.branch.as_deref(),
                            },
                        )
                    })
                    .and_then(|inner| inner);
                match made {
                    Ok(created) => {
                        self.ui.panel = None;
                        self.refresh(false);
                        self.ui.select(&created.task.id);
                        self.ui.set_status(with_warnings(
                            format!("created task {} — ⏎ starts its agent", created.task.name),
                            &created.warnings,
                        ));
                    }
                    Err(e) => {
                        let back = self.ui.panel.take();
                        self.ui
                            .message("could not create the task", vec![format!("{e:#}")], back);
                    }
                }
            }
            Action::Archive { task_id, confirmed } => {
                let Some(row) = self.ui.row(&task_id).cloned() else {
                    return Ok(());
                };
                // Not polled yet is not "not running": ask tmux.
                let running = row.agent_running()
                    || (row.activity.is_none()
                        && tmux::pane_alive(&runtime::session_name(&row.task)));
                let task = row.task;
                let unkept = match confirmed {
                    true => Vec::new(),
                    false => self
                        .progress("checking what archiving would not keep", || {
                            tasks::unkept(app, &task)
                        })
                        .and_then(|inner| inner)?,
                };
                if !confirmed && (running || !unkept.is_empty()) {
                    self.ui.panel = Some(Panel::Confirm {
                        title: format!("archive task {}", task.name),
                        lines: archive_warning(&task.name, running, &unkept),
                        action: Pending::Archive { task_id },
                    });
                    return Ok(());
                }
                let archived = self
                    .progress(&format!("archiving {}", task.name), || {
                        tasks::archive(app, &task)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                match archived {
                    Ok(warnings) => self.ui.set_status(with_warnings(
                        format!("archived {} — ⇥ shows archived tasks", task.name),
                        &warnings,
                    )),
                    Err(e) => self.ui.message(
                        "could not finish archiving",
                        vec![
                            format!("{e:#}"),
                            String::new(),
                            "a on it in the archived list tries again".into(),
                        ],
                        None,
                    ),
                }
            }
            Action::Unarchive { task_id } => {
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                let back = self
                    .progress(&format!("bringing {} back", task.name), || {
                        tasks::unarchive(app, &task)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                match back {
                    Ok(warnings) => {
                        self.ui.select(&task_id);
                        self.ui
                            .set_status(with_warnings(format!("{} is back", task.name), &warnings));
                    }
                    Err(e) => self
                        .ui
                        .message("could not unarchive", vec![format!("{e:#}")], None),
                }
            }
            Action::AskDelete { task_id } => self.ask_delete(&task_id)?,
            Action::Delete { task_id } => {
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                let deleted = self
                    .progress(&format!("deleting {}", task.name), || {
                        tasks::delete(app, &task)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                match deleted {
                    Ok(warnings) => self
                        .ui
                        .set_status(with_warnings(format!("deleted {}", task.name), &warnings)),
                    Err(e) => self.ui.message(
                        "could not finish deleting",
                        vec![
                            format!("{e:#}"),
                            String::new(),
                            "D on it again tries again".into(),
                        ],
                        None,
                    ),
                }
            }
            Action::Projects => {
                let lines = project_lines(app)?;
                self.ui.panel = Some(Panel::Projects { lines, selected: 0 });
            }
            Action::CreateProject { name } => match projects::create(app, &name, &[]) {
                Ok(_) => {
                    self.show_projects(Some(&name))?;
                    self.ui.set_status(format!(
                        "created project {name} — e edits its repositories, ⏎ starts a task"
                    ));
                }
                Err(e) => self.ui.message(
                    "could not create the project",
                    vec![format!("{e:#}")],
                    Some(Panel::NewProject { input: name }),
                ),
            },
            Action::EditProject { name } => {
                let path = projects::config_path(app, &name)?;
                let dir = projects::existing_dir(app, &name)?;
                let argv = agent::editor_argv(&app.config)?;
                let file = vec![path.display().to_string()];
                let code = term::suspended(self.terminal, || agent::run_in(&argv, &dir, &file))?;
                self.show_projects(Some(&name))?;
                match (code, projects::load(app, &name)) {
                    (Ok(0), Ok(project)) => {
                        let repos = match project.config.repos.len() {
                            1 => "1 repository".to_string(),
                            n => format!("{n} repositories"),
                        };
                        self.ui.set_status(format!("project {name}: {repos}"));
                    }
                    (Ok(0), Err(e)) => {
                        let back = self.ui.panel.take();
                        self.ui.message(
                            "project.toml is not valid",
                            vec![format!("{e:#}"), String::new(), "e edits it again".into()],
                            back,
                        );
                    }
                    (Ok(code), _) => self
                        .ui
                        .set_status(format!("the editor exited with status {code}")),
                    (Err(e), _) => self
                        .ui
                        .set_status(format!("could not open the editor: {e:#}")),
                }
            }
            Action::AskDeleteProject { name } => {
                let users = projects::tasks_using(app, &name)?.len();
                let mut lines = vec![format!(
                    "removes {}",
                    projects::existing_dir(app, &name)?.display()
                )];
                if users > 0 {
                    lines.push(format!(
                        "{} made from it keep their files, but can no longer resync",
                        plural(users, "task")
                    ));
                }
                self.ui.panel = Some(Panel::Confirm {
                    title: format!("delete project {name}"),
                    lines,
                    action: Pending::DeleteProject { name },
                });
            }
            Action::DeleteProject { name } => {
                projects::remove(app, &name)?;
                self.show_projects(None)?;
                self.ui.set_status(format!("deleted project {name}"));
            }
            Action::Resync { task_id, force } => self.resync(&task_id, force)?,
            Action::Chat(which) => self.chat(which)?,
            Action::AssistantSessions => {
                let sessions = self.assistant_sessions()?;
                self.ui.panel = Some(Panel::Assistant(session_picker::Picker::new(sessions)));
            }
            Action::RenameSession { id, name } => {
                let said = match assistant::session_name(&name) {
                    Ok(name) => {
                        app.db.rename_task_session(&id, &name)?;
                        format!("renamed to {}", printable(&name))
                    }
                    Err(e) => format!("{e:#}"),
                };
                self.reload_sessions(said)?;
            }
            Action::TogglePrimary { id } => {
                let Some(task) = app.db.get_assistant_task()? else {
                    return Ok(());
                };
                let was = self
                    .shown_session(&id)
                    .is_some_and(|session| session.primary);
                app.db
                    .set_primary_session(&task.id, (!was).then_some(id.as_str()))?;
                let said = match was {
                    true => "no primary session: events go to the latest running one",
                    false => "primary: events go to this session while it runs",
                };
                self.reload_sessions(said.to_string())?;
            }
            Action::EndSession { id } => {
                let Some(session) = self.shown_session(&id) else {
                    return Ok(());
                };
                let name = printable(&session.name);
                let said = match session.running() {
                    false => format!("{name} is not running"),
                    // Ending waits for the agent to exit, which takes a while.
                    true => match self
                        .progress(&format!("ending {name}"), || assistant::end(&session))
                        .and_then(|inner| inner)
                    {
                        Ok(()) => format!("ended {name}"),
                        Err(e) => format!("could not end it: {e:#}"),
                    },
                };
                self.reload_sessions(said)?;
            }
            Action::TodoDone { id, text, done } => {
                let dir = assistant::home_dir(&app.home);
                let changed = self
                    .progress("updating the todo list", || {
                        assistant_info::set_todo_done(&dir, id, &text, done)
                    })
                    .and_then(|inner| inner);
                self.look_at_assistant();
                match changed {
                    Ok(item) => {
                        self.ui.todo_changed(&item);
                        let what = if item.done { "done" } else { "not done" };
                        self.ui
                            .set_status(format!("{what}: {}", printable(&item.title)));
                    }
                    Err(e) => self
                        .ui
                        .set_status(format!("could not change the todo list: {e:#}")),
                }
            }
            Action::TodoAdd { text } => {
                let dir = assistant::home_dir(&app.home);
                let added = self
                    .progress("adding to the todo list", || {
                        assistant_info::add_todo(&dir, &text)
                    })
                    .and_then(|inner| inner);
                self.look_at_assistant();
                match added {
                    Ok(item) => self
                        .ui
                        .set_status(format!("added: {}", printable(&item.text))),
                    Err(e) => self.ui.set_status(format!("could not add it: {e:#}")),
                }
            }
            Action::OpenUrl { url } => {
                let opened = self
                    .progress("opening the browser", || assistant_info::open_url(&url))
                    .and_then(|inner| inner);
                match opened {
                    Ok(()) => self.ui.set_status("opened it in the browser"),
                    Err(e) => self.ui.set_status(format!("could not open it: {e:#}")),
                }
            }
        }
        Ok(())
    }

    /// `c`: attach to one of the assistant's sessions, starting it — and
    /// setting up the assistant's home, the first time — if need be.
    fn chat(&mut self, which: ChatSession) -> Result<()> {
        let app = self.app;
        let opened = self
            .progress("opening the assistant", || {
                let task = assistant::ensure(app)?;
                let how = match &which {
                    ChatSession::Latest => Open::Latest,
                    ChatSession::New => Open::New {
                        name: None,
                        prompt: None,
                    },
                    ChatSession::Session(id) => Open::Session(id),
                };
                assistant::open(app, &task, how)
            })
            .and_then(|inner| inner)?;
        self.ui.panel = None;
        let attached = term::suspended(self.terminal, || assistant::attach(&opened.window))?;
        self.refresh(false);
        let name = printable(&opened.name);
        match attached {
            Ok(_) => self
                .ui
                .set_status(format!("back from the assistant ({name})")),
            Err(e) => self.ui.set_status(format!("could not attach: {e:#}")),
        }
        Ok(())
    }

    /// The assistant's sessions, setting its home up the first time. Runs
    /// tmux, so off the draw path.
    fn assistant_sessions(&mut self) -> Result<Vec<assistant::Session>> {
        let app = self.app;
        self.progress("reading the assistant's sessions", || {
            let task = assistant::ensure(app)?;
            assistant::sessions(app, &task)
        })
        .and_then(|inner| inner)
    }

    /// Session `id`, as the sessions panel shows it.
    fn shown_session(&self, id: &str) -> Option<assistant::Session> {
        match &self.ui.panel {
            Some(Panel::Assistant(picker)) => picker.sessions.iter().find(|s| s.id == id).cloned(),
            _ => None,
        }
    }

    /// Bring the sessions panel up to date, saying `said`.
    fn reload_sessions(&mut self, said: String) -> Result<()> {
        let sessions = self.assistant_sessions()?;
        self.look_at_assistant();
        if let Some(Panel::Assistant(picker)) = &mut self.ui.panel {
            picker.reload(sessions);
            picker.status = said;
        }
        Ok(())
    }

    /// ⏎: attach to the task's agent. A running agent is attached to as it
    /// is — there is one per task — so the session picker only comes up when
    /// one has to be started and there is more than one session to resume.
    fn open(&mut self, task_id: &str, session: Option<String>) -> Result<()> {
        let app = self.app;
        let Some(row) = self.ui.row(task_id).cloned() else {
            return Ok(());
        };
        let task = row.task;
        if task.state == TaskState::Archived {
            self.ui
                .set_status(format!("{} is archived — a brings it back", task.name));
            return Ok(());
        }
        // Before any picker: without tmux there is nothing to pick for.
        tmux::require()?;
        let name = runtime::session_name(&task);
        if session.is_none() && !(tmux::has_session(&name) && tmux::pane_alive(&name)) {
            let spec = AgentSpec::from_config(&app.config)?;
            if spec.supports_sessions() {
                let sessions = session_choices(app, &task, &row.dir)?;
                if sessions.len() > 1 {
                    self.ui.panel = Some(Panel::Sessions {
                        task_id: task_id.to_string(),
                        sessions,
                        selected: 0,
                    });
                    return Ok(());
                }
            }
        }
        let running = self
            .progress(&format!("starting {}'s agent", task.name), || {
                runtime::ensure_running(app, &task, session.as_deref(), None)
            })
            .and_then(|inner| inner)?;
        let attached = term::suspended(self.terminal, || tmux::attach(&running.tmux_session))?;
        let _ = app.db.touch_task(&task.id, now_secs());
        self.refresh(false);
        match attached {
            Ok(_) => self.ui.set_status(with_warnings(
                format!("back from {}", task.name),
                &running.warnings,
            )),
            Err(e) => self.ui.set_status(format!("could not attach: {e:#}")),
        }
        Ok(())
    }

    /// The explorer, on this terminal, coming back here on esc.
    fn explore_at(&mut self, place: Place) -> Result<()> {
        let app = self.app;
        let repo = match &place.repo_id {
            Some(id) => match app.db.get_repo(id)? {
                Some(repo) => Some(repo),
                None => anyhow::bail!("{id} is no longer in the store"),
            },
            None => None,
        };
        let exit = super::explore(
            self.terminal,
            app,
            repo.as_ref(),
            &place.path,
            &place.path,
            Some(BACK_LABEL),
        )?;
        match exit {
            Exit::Back => {
                self.refresh(false);
                self.ui.set_status("");
            }
            // ctrl-c or ctrl-q in the explorer: done with jeet altogether.
            exit => self.ui.quit(exit),
        }
        Ok(())
    }

    fn worktrees_panel(&mut self, task_id: &str) -> Result<Panel> {
        let app = self.app;
        let Some(row) = self.ui.row(task_id).cloned() else {
            anyhow::bail!("the task has gone");
        };
        let lines = self
            .progress("reading worktrees", || worktree_lines(app, &row))
            .and_then(|inner| inner)?;
        Ok(Panel::Worktrees {
            task_id: task_id.to_string(),
            lines,
            selected: 0,
        })
    }

    fn worktrees(&mut self, task_id: &str) -> Result<()> {
        let panel = self.worktrees_panel(task_id)?;
        self.ui.panel = Some(panel);
        Ok(())
    }

    /// `p`: the PRs recorded for the task, straight away, while GitHub is
    /// asked in the background for any news of them.
    fn prs(&mut self, task_id: &str) -> Result<()> {
        let Some(task) = self.task(task_id) else {
            return Ok(());
        };
        let prs = self.app.db.list_task_prs(&task.id)?;
        let job = pr_history::job(self.app, &task)?;
        let looking = !job.sources.is_empty() || !job.open.is_empty();
        if looking {
            self.bg.find_prs(vec![job]);
        }
        self.ui.panel = Some(Panel::Prs {
            task_id: task_id.to_string(),
            prs,
            selected: 0,
            looking,
            note: None,
        });
        Ok(())
    }

    /// `v`: a review task's PR in the review viewer.
    fn review_viewer(&mut self, task_id: &str) -> Result<()> {
        let Some(row) = self.ui.row(task_id).cloned() else {
            return Ok(());
        };
        if row.task.kind != TaskKind::Review {
            self.ui.set_status(format!(
                "{} is not a review task; `jeet review <pr>` makes one",
                row.task.name
            ));
            return Ok(());
        }
        let Some((path, number)) = row
            .worktrees
            .iter()
            .filter_map(|wt| Some((row.dir.join(&wt.dir_name), wt.pr_number?)))
            .find(|(path, _)| path.is_dir())
        else {
            self.ui.set_status(format!(
                "{} has no checkout of its pull request on disk",
                row.task.name
            ));
            return Ok(());
        };
        let pr = self
            .progress(&format!("reading #{number}"), || {
                github::pr_view(&path, number)
            })
            .and_then(|inner| inner)?;
        let outcome = super::review::view(self.terminal, &path, &pr)?;
        if outcome.exit == super::review::Exit::Quit {
            self.ui.quit(Exit::Stay);
        }
        Ok(())
    }

    fn ask_delete(&mut self, task_id: &str) -> Result<()> {
        let app = self.app;
        let Some(row) = self.ui.row(task_id).cloned() else {
            return Ok(());
        };
        let task = row.task;
        if crate::commands::task::standing_in(&row.dir) {
            self.ui.message(
                "cannot delete",
                vec![
                    format!("the shell jeet runs in is inside {}", row.dir.display()),
                    "cd out of it, then delete the task".into(),
                ],
                None,
            );
            return Ok(());
        }
        // Before asking, not after: the answer would count for nothing.
        if let Err(e) = runtime::refuse_inside_session(&task, "delete") {
            self.ui
                .message("cannot delete", vec![format!("{e:#}")], None);
            return Ok(());
        }
        let losses = self
            .progress("checking what would be lost", || tasks::losses(app, &task))
            .and_then(|inner| inner)?;
        let mut lines = vec![
            format!(
                "removes {} and its {}",
                row.dir.display(),
                plural(row.worktrees.len(), "worktree")
            ),
            String::new(),
        ];
        if losses.is_empty() {
            lines.push("nothing would be lost — no unpushed or uncommitted work".into());
        } else {
            lines.push("⚠ this work would be lost:".into());
            lines.extend(losses.iter().map(|loss| format!("  {loss}")));
        }
        self.ui.panel = Some(Panel::Confirm {
            title: format!("delete task {}", task.name),
            lines,
            action: Pending::Delete {
                task_id: task_id.to_string(),
            },
        });
        Ok(())
    }

    fn show_projects(&mut self, select: Option<&str>) -> Result<()> {
        let lines = project_lines(self.app)?;
        let selected = select
            .and_then(|name| lines.iter().position(|l| l.name == name))
            .unwrap_or(0);
        self.ui.panel = Some(Panel::Projects { lines, selected });
        Ok(())
    }

    /// `S`: copy the task's project files in again; changed copies are kept
    /// unless the user then asks for them to be replaced too.
    fn resync(&mut self, task_id: &str, force: bool) -> Result<()> {
        let Some(task) = self.task(task_id) else {
            return Ok(());
        };
        let Some(project) = task.project.clone() else {
            self.ui.set_status(format!(
                "{} was not made from a project, so has nothing to resync",
                task.name
            ));
            return Ok(());
        };
        let sync = projects::resync(self.app, &task, force)?;
        let mut lines: Vec<String> = Vec::new();
        lines.extend(sync.written.iter().map(|rel| format!("updated {rel}")));
        lines.extend(sync.removed.iter().map(|rel| format!("removed {rel}")));
        lines.extend(sync.notes());
        if !sync.kept.is_empty() && !force {
            lines.push(String::new());
            lines.push("the task changed these, so they were kept".into());
            self.ui.panel = Some(Panel::Confirm {
                title: format!("resync from {project}"),
                lines,
                action: Pending::ForceResync {
                    task_id: task_id.to_string(),
                },
            });
            return Ok(());
        }
        if lines.is_empty() {
            self.ui.set_status(format!(
                "{} already has project {project}'s files",
                task.name
            ));
        } else {
            self.ui
                .message(&format!("resynced from {project}"), lines, None);
        }
        Ok(())
    }
}

/// Every task in the index, as rows not yet polled or measured.
fn load_rows(app: &App) -> Result<Vec<TaskRow>> {
    app.db
        .list_tasks(None)?
        .into_iter()
        .map(|task| {
            Ok(TaskRow {
                dir: tasks::task_dir(app, &task),
                worktrees: app.db.list_task_worktrees(&task.id)?,
                activity: None,
                last_activity: task.last_activity,
                size: None,
                task,
            })
        })
        .collect()
}

/// The task's sessions for the picker, newest first.
pub(crate) fn session_choices(
    app: &App,
    task: &TaskRecord,
    dir: &Path,
) -> Result<Vec<SessionChoice>> {
    let mut choices: Vec<SessionChoice> = app
        .db
        .list_task_sessions(&task.id)?
        .into_iter()
        .map(|s| {
            let reported = status::read_status(dir, &s.session_id);
            let state = match &reported {
                Some(st) => format!("{:?}", st.state).to_lowercase(),
                None => "no status".to_string(),
            };
            SessionChoice {
                last: s.last_activity.max(reported.map_or(0, |st| st.at)),
                id: s.session_id,
                name: s.name,
                state,
            }
        })
        .collect();
    sort_sessions(&mut choices);
    Ok(choices)
}

/// Each of the task's worktrees with its branch, divergence and diff.
fn worktree_lines(app: &App, row: &TaskRow) -> Result<Vec<WorktreeLine>> {
    let mut lines = Vec::new();
    for record in &row.worktrees {
        let path = row.dir.join(&record.dir_name);
        let present = path.is_dir();
        let repo = app.db.get_repo(&record.repo_id)?;
        let (head, status) = match (&repo, present) {
            (Some(repo), true) => {
                let branch = git::head_branch(&path);
                let entry = WorktreeEntry {
                    path: path.clone(),
                    branch: branch.clone(),
                    kind: WorktreeKind::Task,
                    last_used: 0,
                    missing: false,
                };
                let status = worktrees::status_against(&entry, &worktrees::comparison_base(repo));
                (entry.display_name(), status)
            }
            (None, true) => (
                git::head_branch(&path).unwrap_or_else(|| "detached".into()),
                WorktreeStatus {
                    unknown: Some(format!("{} is no longer in the store", record.repo_id)),
                    ..WorktreeStatus::default()
                },
            ),
            (_, false) => (
                record
                    .branch
                    .clone()
                    .unwrap_or_else(|| "detached".to_string()),
                WorktreeStatus::default(),
            ),
        };
        lines.push(WorktreeLine {
            record: record.clone(),
            path,
            present,
            head,
            status,
        });
    }
    Ok(lines)
}

fn project_lines(app: &App) -> Result<Vec<ProjectLine>> {
    let mut lines = Vec::new();
    for name in projects::list(app)? {
        let tasks = projects::tasks_using(app, &name)?.len();
        let (contents, repos) = match projects::load(app, &name) {
            Ok(project) => (
                project.contents_summary(),
                match project.config.repos.as_slice() {
                    [] => "-".to_string(),
                    repos => repos.join(", "),
                },
            ),
            Err(e) => (format!("broken: {e:#}"), "-".to_string()),
        };
        lines.push(ProjectLine {
            name,
            tasks,
            contents,
            repos,
        });
    }
    Ok(lines)
}

fn with_warnings(what: String, warnings: &[String]) -> String {
    match warnings {
        [] => what,
        warnings => format!("{what} — {}", warnings.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::TaskKind;

    #[test]
    fn the_archive_confirm_names_what_is_not_kept() {
        let quiet = archive_warning("Fix login", false, &[]);
        assert_eq!(quiet[0], "archiving Fix login frees its worktrees' disk");
        assert!(!quiet.iter().any(|l| l.contains("ignores")));

        let lines = archive_warning("Fix login", true, &["api: .env, target/".into()]);
        assert_eq!(lines[0], "Fix login has its agent running");
        assert!(lines.iter().any(|l| l.contains("git ignores are not kept")));
        assert_eq!(lines.last().unwrap(), "  api: .env, target/");
    }

    fn task(id: &str, name: &str, state: TaskState, last: i64) -> TaskRow {
        TaskRow {
            task: TaskRecord {
                id: id.into(),
                slug: tasks::slugify(name),
                name: name.into(),
                project: None,
                kind: TaskKind::Work,
                state,
                created_at: 0,
                last_activity: last,
            },
            dir: PathBuf::from(format!("/tasks/{}", tasks::slugify(name))),
            worktrees: Vec::new(),
            activity: None,
            last_activity: last,
            size: None,
        }
    }

    fn press(ui: &mut TasksUi, code: KeyCode) -> Option<Action> {
        ui.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(ui: &mut TasksUi, text: &str) {
        for c in text.chars() {
            press(ui, KeyCode::Char(c));
        }
    }

    fn sample() -> TasksUi {
        TasksUi::new(vec![
            task("a", "fix login", TaskState::Active, 10),
            task("b", "api cleanup", TaskState::Active, 30),
            task("c", "old spike", TaskState::Archived, 50),
            task("d", "docs", TaskState::Active, 20),
        ])
    }

    fn shown(ui: &TasksUi) -> Vec<&str> {
        ui.visible
            .iter()
            .map(|&i| ui.rows[i].task.name.as_str())
            .collect()
    }

    #[test]
    fn active_tasks_are_listed_newest_activity_first() {
        let ui = sample();
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        assert_eq!(ui.count(View::Active), 3);
        assert_eq!(ui.count(View::Archived), 1);
    }

    #[test]
    fn tab_switches_to_the_archived_tasks_and_back() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Archived);
        assert_eq!(shown(&ui), ["old spike"]);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
    }

    #[test]
    fn activity_reported_later_reorders_but_keeps_the_cursor() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Down); // docs
        assert_eq!(ui.selected_id(), Some("d"));
        ui.apply(Update::Status {
            id: "a".into(),
            activity: Some(Activity::Running { detail: None }),
            last_activity: 99,
        });
        assert_eq!(shown(&ui), ["fix login", "api cleanup", "docs"]);
        assert_eq!(ui.selected_id(), Some("d"), "the cursor follows its task");
        // An older time never moves a task down.
        ui.apply(Update::Status {
            id: "a".into(),
            activity: None,
            last_activity: 1,
        });
        assert_eq!(shown(&ui)[0], "fix login");
        ui.apply(Update::Size {
            id: "b".into(),
            bytes: 42,
        });
        assert_eq!(ui.row("b").unwrap().size, Some(42));
    }

    #[test]
    fn reloading_keeps_what_the_background_reported() {
        let mut ui = sample();
        ui.apply(Update::Size {
            id: "a".into(),
            bytes: 7,
        });
        ui.apply(Update::Status {
            id: "a".into(),
            activity: Some(Activity::Ended),
            last_activity: 5,
        });
        ui.set_rows(vec![task("a", "fix login", TaskState::Active, 10)]);
        let row = ui.row("a").unwrap();
        assert_eq!(row.size, Some(7));
        assert_eq!(row.activity, Some(Activity::Ended));
        // Archiving changes what the agent can be doing.
        ui.set_rows(vec![task("a", "fix login", TaskState::Archived, 10)]);
        assert_eq!(ui.row("a").unwrap().activity, None);
    }

    #[test]
    fn slash_filters_and_letters_stop_being_commands() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Char('/'));
        assert!(ui.filtering);
        // `n` and `a` would make and archive a task outside the filter.
        type_text(&mut ui, "an");
        assert!(ui.panel.is_none());
        assert_eq!(ui.filter, "an");
        assert_eq!(shown(&ui), ["api cleanup"]);
        // ⏎ keeps the filter and gives the letters back.
        press(&mut ui, KeyCode::Enter);
        assert!(!ui.filtering);
        assert_eq!(shown(&ui), ["api cleanup"]);
        assert_eq!(
            press(&mut ui, KeyCode::Char('w')),
            Some(Action::Worktrees {
                task_id: "b".into()
            })
        );
        // esc drops the filter before it quits.
        press(&mut ui, KeyCode::Esc);
        assert!(ui.filter.is_empty());
        assert!(ui.exit.is_none());
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.exit, Some(Exit::Stay));
    }

    #[test]
    fn the_filter_ranks_like_the_explorers() {
        let mut ui = TasksUi::new(vec![
            task("1", "redocs", TaskState::Active, 30),
            task("2", "docs-site", TaskState::Active, 20),
            task("3", "docs", TaskState::Active, 10),
        ]);
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "docs");
        assert_eq!(shown(&ui), ["docs", "docs-site", "redocs"]);
        assert_eq!(ui.selected_id(), Some("3"));
        press(&mut ui, KeyCode::Esc);
        assert!(!ui.filtering);
        assert_eq!(shown(&ui).len(), 3);
    }

    #[test]
    fn keys_ask_for_their_actions_on_the_selected_task() {
        let mut ui = sample();
        let id = || "b".to_string();
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: id(),
                session: None
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('e')),
            Some(Action::Explore { task_id: id() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('p')),
            Some(Action::Prs { task_id: id() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('v')),
            Some(Action::ReviewViewer { task_id: id() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive {
                task_id: id(),
                confirmed: false
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('D')),
            Some(Action::AskDelete { task_id: id() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('S')),
            Some(Action::Resync {
                task_id: id(),
                force: false
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewTaskForm { project: None })
        );
        assert_eq!(press(&mut ui, KeyCode::Char('P')), Some(Action::Projects));
        assert_eq!(press(&mut ui, KeyCode::Char('r')), Some(Action::Refresh));
        // Lowercase d is not delete.
        assert_eq!(press(&mut ui, KeyCode::Char('d')), None);
        press(&mut ui, KeyCode::Char('?'));
        assert!(matches!(ui.panel, Some(Panel::Help { .. })));
        press(&mut ui, KeyCode::Esc);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn in_the_archived_view_a_unarchives_and_enter_explains() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Tab);
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Unarchive {
                task_id: "c".into()
            })
        );
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert!(ui.status_line.contains("archived"), "{}", ui.status_line);
    }

    #[test]
    fn an_empty_list_asks_for_no_task_actions() {
        let mut ui = TasksUi::default();
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert_eq!(press(&mut ui, KeyCode::Char('D')), None);
        assert_eq!(ui.status_line, NO_TASK);
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewTaskForm { project: None })
        );
    }

    #[test]
    fn ctrl_q_leaves_the_shell_in_the_selected_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut row = task("a", "here", TaskState::Active, 1);
        row.dir = dir.path().to_path_buf();
        let mut ui = TasksUi::new(vec![row]);
        ui.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(ui.exit, Some(Exit::ChangeDir(dir.path().to_path_buf())));

        let mut ui = sample();
        ui.panel = Some(Panel::Help { scroll: 0 });
        ui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            ui.exit,
            Some(Exit::Stay),
            "ctrl-c quits from inside a panel"
        );
    }

    #[test]
    fn confirming_asks_for_the_pending_action_and_anything_else_keeps_asking() {
        let mut ui = sample();
        let confirm = |action| Panel::Confirm {
            title: "t".into(),
            lines: Vec::new(),
            action,
        };
        ui.panel = Some(confirm(Pending::Delete {
            task_id: "b".into(),
        }));
        assert_eq!(press(&mut ui, KeyCode::Char('x')), None);
        assert!(ui.panel.is_some(), "a stray key leaves the question up");
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Delete {
                task_id: "b".into()
            })
        );

        ui.panel = Some(confirm(Pending::Delete {
            task_id: "b".into(),
        }));
        assert_eq!(press(&mut ui, KeyCode::Char('n')), None);
        assert!(ui.panel.is_none());

        ui.panel = Some(confirm(Pending::RemoveWorktree {
            task_id: "b".into(),
            dir_name: "api".into(),
        }));
        assert_eq!(
            press(&mut ui, KeyCode::Char('f')),
            Some(Action::RemoveWorktree {
                task_id: "b".into(),
                dir_name: "api".into(),
                force: true
            })
        );
        ui.panel = Some(confirm(Pending::RemoveWorktree {
            task_id: "b".into(),
            dir_name: "api".into(),
        }));
        assert_eq!(
            press(&mut ui, KeyCode::Esc),
            Some(Action::Worktrees {
                task_id: "b".into()
            }),
            "backing out returns to the worktrees panel"
        );

        // Delete has no force: `f` is just a stray key there.
        ui.panel = Some(confirm(Pending::Delete {
            task_id: "b".into(),
        }));
        assert_eq!(press(&mut ui, KeyCode::Char('f')), None);
        assert!(ui.panel.is_some());

        ui.panel = Some(confirm(Pending::ForceResync {
            task_id: "b".into(),
        }));
        assert_eq!(press(&mut ui, KeyCode::Char('y')), None);
        assert_eq!(
            press(&mut ui, KeyCode::Char('f')),
            Some(Action::Resync {
                task_id: "b".into(),
                force: true
            })
        );
    }

    #[test]
    fn a_message_goes_back_to_the_panel_it_came_from() {
        let mut ui = sample();
        ui.message(
            "oops",
            vec!["x".into()],
            Some(Panel::NewProject {
                input: "half".into(),
            }),
        );
        press(&mut ui, KeyCode::Enter);
        match &ui.panel {
            Some(Panel::NewProject { input }) => assert_eq!(input, "half"),
            other => panic!("{other:?}"),
        }
    }

    fn form() -> NewTaskForm {
        NewTaskForm::new(
            "task 3".into(),
            vec!["web".into(), "infra".into()],
            None,
            vec![
                "github.com/acme/api".into(),
                "github.com/acme/web".into(),
                "github.com/acme/docs".into(),
            ],
        )
    }

    #[test]
    fn a_blank_form_makes_an_empty_canvas_under_the_default_name() {
        assert_eq!(
            form().spec(),
            Ok(TaskSpec {
                name: "task 3".into(),
                project: None,
                repos: Vec::new(),
                branch: None
            })
        );
    }

    #[test]
    fn a_branch_needs_exactly_one_repository() {
        let mut f = form();
        f.branch = "feature".into();
        assert!(f.spec().is_err());
        f.repos.toggle();
        assert_eq!(f.spec().unwrap().branch.as_deref(), Some("feature"));
        f.repos.move_by(1);
        f.repos.toggle();
        assert!(f.spec().is_err());
    }

    #[test]
    fn the_form_is_filled_in_from_the_keyboard() {
        let mut ui = sample();
        ui.panel = Some(Panel::NewTask(form()));
        type_text(&mut ui, "fix it");
        press(&mut ui, KeyCode::Tab); // project
        press(&mut ui, KeyCode::Right); // web
        press(&mut ui, KeyCode::Right); // infra
        press(&mut ui, KeyCode::Right); // back to none
        press(&mut ui, KeyCode::Right); // web
        press(&mut ui, KeyCode::Tab); // repos
        type_text(&mut ui, "doc");
        press(&mut ui, KeyCode::Char(' '));
        press(&mut ui, KeyCode::Tab); // branch
        type_text(&mut ui, "fix/it");
        let action = press(&mut ui, KeyCode::Enter);
        assert_eq!(
            action,
            Some(Action::CreateTask(TaskSpec {
                name: "fix it".into(),
                project: Some("web".into()),
                repos: vec!["github.com/acme/docs".into()],
                branch: Some("fix/it".into()),
            }))
        );
    }

    #[test]
    fn an_invalid_form_stays_up_and_says_why() {
        let mut ui = sample();
        let mut f = form();
        f.field = Field::Branch;
        ui.panel = Some(Panel::NewTask(f));
        type_text(&mut ui, "b");
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        match &ui.panel {
            Some(Panel::NewTask(f)) => assert!(f.error.is_some()),
            other => panic!("{other:?}"),
        }
        assert_eq!(press(&mut ui, KeyCode::Esc), None);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn the_projects_panel_starts_a_task_from_a_project() {
        let mut ui = sample();
        ui.panel = Some(Panel::Projects {
            lines: vec![ProjectLine {
                name: "web".into(),
                tasks: 0,
                contents: "-".into(),
                repos: "-".into(),
            }],
            selected: 0,
        });
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::NewTaskForm {
                project: Some("web".into())
            })
        );
        ui.panel = Some(Panel::Projects {
            lines: Vec::new(),
            selected: 0,
        });
        // Nothing to edit or delete in an empty panel; `n` still works.
        assert_eq!(press(&mut ui, KeyCode::Char('e')), None);
        press(&mut ui, KeyCode::Char('n'));
        type_text(&mut ui, "infra");
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::CreateProject {
                name: "infra".into()
            })
        );
    }

    #[test]
    fn the_add_worktree_form_picks_a_repo_or_takes_what_was_typed() {
        let mut ui = sample();
        let panel = || Panel::AddWorktree {
            task_id: "b".into(),
            repos: Picker::new(vec![
                "github.com/acme/api".into(),
                "github.com/acme/web".into(),
            ]),
            branch: String::new(),
            field: AddField::Repo,
        };
        ui.panel = Some(panel());
        type_text(&mut ui, "web");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "topic");
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::AddWorktree {
                task_id: "b".into(),
                repo: "github.com/acme/web".into(),
                branch: Some("topic".into())
            })
        );
        ui.panel = Some(panel());
        type_text(&mut ui, "https://example.com/x.git");
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::AddWorktree {
                task_id: "b".into(),
                repo: "https://example.com/x.git".into(),
                branch: None
            })
        );
    }

    #[test]
    fn the_picker_narrows_and_remembers_marks() {
        let mut picker = Picker::new(vec!["api".into(), "web".into(), "webhooks".into()]);
        picker.push('w');
        picker.push('e');
        assert_eq!(picker.current(), Some("web"));
        picker.toggle();
        picker.move_by(1);
        picker.toggle();
        picker.pop();
        picker.pop();
        assert_eq!(picker.visible.len(), 3);
        assert_eq!(picker.chosen(), ["web", "webhooks"]);
        picker.push('z');
        assert_eq!(picker.current(), None);
    }

    #[test]
    fn default_names_skip_the_taken_ones() {
        let rows = sample();
        assert_eq!(
            default_task_name(rows.rows.iter().map(|r| &r.task)),
            "task 1"
        );
        let taken = [
            task("1", "task 1", TaskState::Active, 0).task,
            task("2", "Task 2", TaskState::Archived, 0).task,
        ];
        assert_eq!(default_task_name(&taken), "task 3");
    }

    #[test]
    fn e_opens_straight_on_the_only_worktree_or_the_bare_canvas() {
        let dir = tempfile::tempdir().unwrap();
        let wt = |name: &str| TaskWorktreeRecord {
            id: 0,
            task_id: "t".into(),
            repo_id: format!("github.com/acme/{name}"),
            dir_name: name.into(),
            branch: None,
            created_branch: false,
            head_oid: None,
            stash_ref: None,
            pr_number: None,
            created_at: 0,
        };
        let canvas = places(dir.path(), &[]);
        assert_eq!(only_place(&canvas).unwrap().repo_id, None);

        std::fs::create_dir(dir.path().join("api")).unwrap();
        let one = places(dir.path(), &[wt("api"), wt("gone")]);
        assert_eq!(one.len(), 2, "a worktree not on disk is no place to go");
        let only = only_place(&one).unwrap();
        assert_eq!(only.path, dir.path().join("api"));
        assert_eq!(only.repo_id.as_deref(), Some("github.com/acme/api"));

        std::fs::create_dir(dir.path().join("web")).unwrap();
        let two = places(dir.path(), &[wt("api"), wt("web")]);
        assert!(only_place(&two).is_none(), "a choice means the picker");
        assert_eq!(two[0].path, dir.path(), "the canvas comes first");
    }

    #[test]
    fn sessions_are_offered_newest_first() {
        let choice = |id: &str, last| SessionChoice {
            id: id.into(),
            name: "n".into(),
            state: "idle".into(),
            last,
        };
        let mut sessions = vec![choice("a", 5), choice("b", 9), choice("c", 7)];
        sort_sessions(&mut sessions);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn sizes_count_files_but_never_follow_links() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), vec![0u8; 23]).unwrap();
        std::fs::write(elsewhere.path().join("big"), vec![0u8; 10_000]).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("link")).unwrap();
            std::os::unix::fs::symlink(elsewhere.path().join("big"), dir.path().join("big"))
                .unwrap();
        }
        assert_eq!(dir_size(dir.path()), 123);
        assert_eq!(dir_size(&dir.path().join("missing")), 0);
    }

    #[test]
    fn the_poller_reports_archived_tasks_without_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let probe = Probe {
            id: "x".into(),
            dir: dir.path().to_path_buf(),
            session: "jeet-x".into(),
            touched: 77,
            active: false,
        };
        assert_eq!(
            probe_status(&probe, false),
            Update::Status {
                id: "x".into(),
                activity: None,
                last_activity: 77
            }
        );
        let probe = Probe {
            active: true,
            ..probe
        };
        match probe_status(&probe, false) {
            Update::Status { activity, .. } => assert_eq!(activity, Some(Activity::NotStarted)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_background_threads_answer_and_stop_with_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), [1u8; 9]).unwrap();
        let bg = Background::start();
        let mut row = task("t", "t", TaskState::Archived, 3);
        row.dir = dir.path().to_path_buf();
        bg.watch(&[row]);
        let mut got = Vec::new();
        while got.len() < 2 {
            got.push(bg.updates.recv_timeout(Duration::from_secs(10)).unwrap());
        }
        assert!(got.contains(&Update::Size {
            id: "t".into(),
            bytes: 9
        }));
        assert!(got
            .iter()
            .any(|u| matches!(u, Update::Status { id, .. } if id == "t")));
        drop(bg);
    }

    fn pr(number: u64) -> TaskPrRecord {
        TaskPrRecord {
            task_id: "a".into(),
            repo_id: "github.com/acme/api".into(),
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/acme/api/pull/{number}"),
            state: "OPEN".into(),
            is_draft: false,
            additions: 1,
            deletions: 1,
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_pr_panel_opens_the_pr_under_the_cursor() {
        let mut ui = sample();
        ui.panel = Some(Panel::Prs {
            task_id: "a".into(),
            prs: vec![pr(1), pr(2)],
            selected: 0,
            looking: true,
            note: None,
        });
        assert_eq!(press(&mut ui, KeyCode::Down), None);
        assert_eq!(
            press(&mut ui, KeyCode::Char('o')),
            Some(Action::OpenPr {
                repo_id: "github.com/acme/api".into(),
                number: 2
            })
        );
        // Still open, still looking, for the next one.
        assert!(matches!(
            ui.panel,
            Some(Panel::Prs {
                selected: 1,
                looking: true,
                ..
            })
        ));
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::OpenPr {
                repo_id: "github.com/acme/api".into(),
                number: 2
            })
        );
        assert_eq!(press(&mut ui, KeyCode::Esc), None);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn found_prs_land_in_the_open_panel_keeping_the_cursor() {
        let mut ui = sample();
        ui.panel = Some(Panel::Prs {
            task_id: "a".into(),
            prs: vec![pr(1), pr(2)],
            selected: 1,
            looking: true,
            note: None,
        });
        // Another task's answer leaves the panel be.
        ui.prs_found("b", vec![pr(9)], None);
        assert!(matches!(&ui.panel, Some(Panel::Prs { prs, looking: true, .. }) if prs.len() == 2));
        // A new PR at the top: the cursor stays on #2.
        ui.prs_found("a", vec![pr(3), pr(1), pr(2)], None);
        match &ui.panel {
            Some(Panel::Prs {
                prs,
                selected,
                looking,
                ..
            }) => {
                assert_eq!(prs[*selected].number, 2);
                assert!(!looking);
            }
            other => panic!("{other:?}"),
        }
        // With no panel open there is nothing to update.
        ui.panel = None;
        ui.prs_found("a", vec![pr(1)], None);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn the_latest_pr_lookups_go_first_and_replace_older_ones() {
        let job = |id: &str| pr_history::Job {
            task_id: id.into(),
            sources: Vec::new(),
            open: Vec::new(),
        };
        let mut queue = VecDeque::new();
        enqueue(&mut queue, vec![job("a"), job("b"), job("c")]);
        enqueue(&mut queue, vec![job("c")]);
        let order: Vec<&str> = queue.iter().map(|j| j.task_id.as_str()).collect();
        assert_eq!(order, ["c", "a", "b"]);
    }

    #[test]
    fn the_pr_thread_answers_for_each_task() {
        let bg = Background::start();
        bg.find_prs(vec![pr_history::Job {
            task_id: "t".into(),
            sources: Vec::new(),
            open: Vec::new(),
        }]);
        match bg.updates.recv_timeout(Duration::from_secs(10)).unwrap() {
            Update::Prs(found) => {
                assert_eq!(found.task_id, "t");
                assert!(found.prs.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    fn assistant_session(id: &str, primary: bool, running: bool) -> assistant::Session {
        assistant::Session {
            id: id.into(),
            name: format!("{id} name"),
            primary,
            last: 1,
            activity: match running {
                true => Activity::Idle { since: 1 },
                false => Activity::NotRunning,
            },
            window: running.then(|| "@1".to_string()),
        }
    }

    fn feed_entry(at: i64, text: &str, url: Option<&str>) -> assistant_info::FeedEntry {
        assistant_info::FeedEntry {
            at,
            kind: assistant_info::FeedKind::Event,
            what: "review requested".into(),
            text: text.into(),
            place: "acme/api #1 → the assistant".into(),
            url: url.map(str::to_string),
        }
    }

    /// The sample tasks, with the assistant's todo list saying `todo`.
    fn with_todo(todo: &str) -> TasksUi {
        let mut ui = sample();
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            todo: crate::todo::parse(todo).views(),
            ..AssistantInfo::default()
        })));
        ui
    }

    #[test]
    fn c_talks_to_the_assistant_and_capital_c_picks_a_session() {
        let mut ui = sample();
        assert_eq!(
            press(&mut ui, KeyCode::Char('c')),
            Some(Action::Chat(ChatSession::Latest))
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('C')),
            Some(Action::AssistantSessions)
        );
        // Neither needs a task: the assistant is not one.
        let mut empty = TasksUi::default();
        assert_eq!(
            press(&mut empty, KeyCode::Char('c')),
            Some(Action::Chat(ChatSession::Latest))
        );
    }

    #[test]
    fn the_sessions_panel_asks_for_what_its_keys_do() {
        let mut ui = sample();
        ui.panel = Some(Panel::Assistant(session_picker::Picker::new(vec![
            assistant_session("a", false, true),
            assistant_session("b", true, false),
        ])));
        assert_eq!(press(&mut ui, KeyCode::Down), None);
        assert_eq!(
            press(&mut ui, KeyCode::Char('*')),
            Some(Action::TogglePrimary { id: "b".into() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('x')),
            Some(Action::EndSession { id: "b".into() })
        );
        assert!(
            matches!(&ui.panel, Some(Panel::Assistant(p)) if p.selected == 1),
            "left up for the screen to bring up to date"
        );
        press(&mut ui, KeyCode::Char('R'));
        type_text(&mut ui, " x");
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::RenameSession {
                id: "b".into(),
                name: "b name x".into()
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Chat(ChatSession::Session("b".into())))
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::Chat(ChatSession::New))
        );
        assert_eq!(press(&mut ui, KeyCode::Esc), None);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn the_todo_panel_ticks_off_adds_and_goes_to_tasks() {
        let mut ui =
            with_todo("# Todo\n- [ ] !p1 fix it @fix-login\n- [x] done @gone\n- [ ] no link\n");
        press(&mut ui, KeyCode::Char('t'));
        assert!(matches!(ui.panel, Some(Panel::Todo { selected: 0, .. })));
        assert_eq!(
            press(&mut ui, KeyCode::Char(' ')),
            Some(Action::TodoDone {
                id: 1,
                text: "!p1 fix it @fix-login".into(),
                done: true
            })
        );
        press(&mut ui, KeyCode::Down);
        assert_eq!(
            press(&mut ui, KeyCode::Char(' ')),
            Some(Action::TodoDone {
                id: 2,
                text: "done @gone".into(),
                done: false
            })
        );
        // A link to no task, then no link: the panel stays and says why.
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert!(ui.panel.is_some());
        assert!(
            ui.status_line.contains("no task gone"),
            "{}",
            ui.status_line
        );
        press(&mut ui, KeyCode::Char('j'));
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert!(
            ui.status_line.contains("links no task"),
            "{}",
            ui.status_line
        );

        // `a` takes a line of text, letters and all.
        press(&mut ui, KeyCode::Char('a'));
        type_text(&mut ui, "!p2 tqa");
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::TodoAdd {
                text: "!p2 tqa".into()
            })
        );
        assert!(matches!(ui.panel, Some(Panel::Todo { adding: None, .. })));
        press(&mut ui, KeyCode::Char('a'));
        assert_eq!(press(&mut ui, KeyCode::Enter), None, "blank adds nothing");
        press(&mut ui, KeyCode::Esc);
        assert!(matches!(ui.panel, Some(Panel::Todo { adding: None, .. })));

        // ⏎ on a linked item closes the panel on its task.
        press(&mut ui, KeyCode::Home);
        press(&mut ui, KeyCode::Enter);
        assert!(ui.panel.is_none());
        assert_eq!(ui.selected_id(), Some("a"));
    }

    #[test]
    fn going_to_a_task_finds_it_in_the_archived_view_and_past_the_filter() {
        let mut ui = with_todo("- [ ] look again @old-spike\n");
        ui.filter = "api".into();
        ui.refresh_visible(None);
        press(&mut ui, KeyCode::Char('t'));
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.view, View::Archived);
        assert_eq!(ui.selected_id(), Some("c"));
    }

    #[test]
    fn o_puts_the_tasks_the_todo_list_links_first() {
        let mut ui = with_todo(
            "- [ ] whenever @docs\n- [ ] !p1 first @fix-login\n- [x] !p1 finished @api-cleanup\n",
        );
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        press(&mut ui, KeyCode::Down); // docs
        press(&mut ui, KeyCode::Char('o'));
        assert!(ui.by_priority);
        assert_eq!(shown(&ui), ["fix login", "docs", "api cleanup"]);
        assert_eq!(ui.selected_id(), Some("d"), "the cursor stays on its task");
        // The list changes: so does the order.
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            todo: crate::todo::parse("- [ ] !p1 now @api-cleanup\n").views(),
            ..AssistantInfo::default()
        })));
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        press(&mut ui, KeyCode::Char('o'));
        assert!(!ui.by_priority);
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
    }

    #[test]
    fn a_ticked_off_item_shows_before_the_list_is_read_again() {
        let mut ui = with_todo("- [ ] one\n- [ ] two\n");
        let item = crate::todo::parse("- [ ] one\n- [x] two\n").views()[1].clone();
        ui.todo_changed(&item);
        assert!(ui.todo()[1].done);
        // Not the item the list has there: left for the next read.
        let other = crate::todo::parse("- [x] three\n").views()[0].clone();
        ui.todo_changed(&other);
        assert_eq!(ui.todo()[0].text, "one");
    }

    #[test]
    fn open_panels_keep_their_line_as_the_assistant_changes() {
        let mut ui = with_todo("- [ ] a\n- [ ] b\n");
        ui.panel = Some(Panel::Todo {
            selected: 1,
            adding: None,
        });
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            todo: crate::todo::parse("- [ ] new\n- [ ] a\n- [ ] b\n").views(),
            ..AssistantInfo::default()
        })));
        assert!(matches!(ui.panel, Some(Panel::Todo { selected: 2, .. })));
        ui.apply(Update::Assistant(Box::default()));
        assert!(matches!(ui.panel, Some(Panel::Todo { selected: 0, .. })));

        let feed = vec![feed_entry(5, "x", None), feed_entry(4, "y", None)];
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            feed: feed.clone(),
            ..AssistantInfo::default()
        })));
        ui.panel = Some(Panel::Inbox { selected: 1 });
        let mut newer = vec![feed_entry(9, "z", None)];
        newer.extend(feed);
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            feed: newer,
            ..AssistantInfo::default()
        })));
        assert!(matches!(ui.panel, Some(Panel::Inbox { selected: 2 })));
    }

    #[test]
    fn the_inbox_opens_the_links_events_have() {
        let mut ui = sample();
        ui.apply(Update::Assistant(Box::new(AssistantInfo {
            feed: vec![
                feed_entry(9, "with a link", Some("https://github.com/acme/api/pull/1")),
                feed_entry(5, "without", None),
            ],
            ..AssistantInfo::default()
        })));
        press(&mut ui, KeyCode::Char('i'));
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::OpenUrl {
                url: "https://github.com/acme/api/pull/1".into()
            })
        );
        assert!(ui.panel.is_some(), "still open, for the next one");
        press(&mut ui, KeyCode::Down);
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert!(ui.status_line.contains("nothing to open"));
        assert_eq!(press(&mut ui, KeyCode::Char('i')), None);
        assert!(ui.panel.is_none());
        // An empty inbox opens, and asks for nothing.
        let mut ui = sample();
        press(&mut ui, KeyCode::Char('i'));
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
    }

    #[test]
    fn the_assistant_thread_reports_what_its_probe_finds() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("assistant");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("todo.md"), "- [ ] !p1 one\n").unwrap();
        let bg = Background::start();
        bg.look_at_assistant(assistant_info::Probe {
            home: home.path().to_path_buf(),
            dir,
            records: Vec::new(),
            primary: None,
        });
        loop {
            match bg.updates.recv_timeout(Duration::from_secs(10)).unwrap() {
                Update::Assistant(info) => {
                    assert_eq!(info.todo.len(), 1);
                    assert_eq!(info.todo[0].priority, Some(1));
                    break;
                }
                _ => continue,
            }
        }
    }

    #[test]
    fn removal_summary_warns_about_work() {
        let line = WorktreeLine {
            record: TaskWorktreeRecord {
                id: 0,
                task_id: "t".into(),
                repo_id: "github.com/acme/api".into(),
                dir_name: "api".into(),
                branch: Some("topic".into()),
                created_branch: true,
                head_oid: None,
                stash_ref: None,
                pr_number: None,
                created_at: 0,
            },
            path: PathBuf::from("/t/api"),
            present: true,
            head: "topic".into(),
            status: WorktreeStatus {
                dirty: 2,
                ahead: 1,
                ..WorktreeStatus::default()
            },
        };
        let text = removal_summary(&line).join("\n");
        assert!(text.contains("2 uncommitted changes"), "{text}");
        assert!(
            text.contains("1 commit not on the default branch"),
            "{text}"
        );
        let clean = WorktreeLine {
            status: WorktreeStatus::default(),
            ..line
        };
        assert!(removal_summary(&clean)
            .join("\n")
            .contains("nothing to lose"));
    }
}
