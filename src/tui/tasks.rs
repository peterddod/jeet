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
//! `⇥` cycles active → agents → reviews → projects, and `A` in the active
//! view lists the archived tasks instead (a flat list), and back. The agents view shows the
//! assistant at the root of the tree of the active tasks under it, with what
//! each needs rolled up to the nodes above ([`managers`]): `⏎` attaches,
//! `→`/`←` open and close managers, `l`, `i` and `b` read a node's
//! worklog, inbox and reports, and brief, `p`, `P` and `x` pause, resume
//! and stop its subtree, and `n` makes a task or manager under it, through
//! the CLI's own checks, as the user.
//! Its index facts come from a tree thread of its own, running only while
//! the view is shown.
//! The reviews view lists every review, with where its PR and its review
//! stand on GitHub, asked on a thread of its own ([`reviews`]); `⏎` offers
//! its diff in the review viewer or its session, and `a` archives it and
//! `D` deletes it, as for a task. Reviews are rows of their own, not tasks.
//! The projects view groups the active tasks and managers by the project
//! they were started from ([`projects_tab`](super::projects_tab)): `→` or
//! `⏎` goes into one, where the active view's walk applies and the task keys
//! act on the task under the cursor; `A` lists the projects with no active
//! work instead.
//!
//! [`TasksUi`] holds the state and turns keys into [`Action`]s without
//! touching the terminal, the index or git, so the whole key map is unit
//! tested; [`run`] carries the actions out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use super::assistant_info::{self, Info as AssistantInfo};
use super::chat::{self as session_picker, Pick, Step};
use super::managers::{self, At, TreeInfo, TreeLine, Urgency};
use super::projects_tab::{Groups, ProjectGroup};
use super::reviews::{self, PrKey, PrReview, ReviewLine, ReviewRow};
use super::state::{rank_matches, Exit};
use super::tasks_ui;
use super::term::{self, Tui};
use crate::agent::{self, age_label, AgentSpec};
use crate::assistant::{self, Open};
use crate::cli::Delegation;
use crate::commands::{self, task::printable, task::printable_text};
use crate::config::{self, Config};
use crate::context::App;
use crate::db::reviews::ReviewRecord;
use crate::db::tasks::{
    RunState, TaskKind, TaskPrRecord, TaskRecord, TaskReportRecord, TaskState, TaskWorktreeRecord,
};
use crate::db::Database;
use crate::git;
use crate::github;
use crate::manager::budget::{self, BudgetPart};
use crate::manager::{self, run};
use crate::messages;
use crate::permissions::{self, Caller, Verb};
use crate::pr_history;
use crate::projects;
use crate::reports;
use crate::runtime::{self, Admission};
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

/// How many updates the pollers may have sent that the screen has yet to
/// take before they stop looking: while the screen is away — attached to an
/// agent in tmux, in the explorer — nobody takes them, and a poll every two
/// seconds piled up gigabytes in a day.
const BACKLOG_MAX: usize = 256;

/// What the screen esc in the explorer comes back to.
const BACK_LABEL: &str = "tasks";

/// Said when a key needs a task and the list has none under the cursor.
const NO_TASK: &str = "no task selected — n makes one";

/// Said in the agents view when it has no tree: none without the assistant.
const NO_NODE: &str = "no assistant yet — c starts it, and the tree grows under it";

/// Said when a key of the agents view that shows an agent's text, or acts,
/// is pressed by anyone but the user at their own terminal (Q2, R17).
const NOT_USER: &str = "the agents pane acts as you; open jeet from your own terminal, outside \
     jeet's tmux sessions and any task's pane";

/// Said for a key of the active view the agents view does not take (Q22).
const NOT_IN_AGENTS: &str = "not in the agents view; ⇥ to active";

/// Said when a key of the reviews view has no review under the cursor.
const NO_REVIEW: &str = "no review selected — jeet review <pr> starts one";

/// Said for ⏎ on an empty answer.
const TYPE_AN_ANSWER: &str = "type an answer";

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
    /// What a task with a parent last told it, or jeet did for it.
    pub report: Option<TaskReportRecord>,
    /// Its agent runs without jeet knowing its process, as the last poll
    /// found ([`runtime::unanchored`]).
    pub unanchored: bool,
}

impl TaskRow {
    pub fn summary(&self) -> String {
        tasks::worktree_summary(&self.worktrees)
    }

    /// Whether the agent is running in the task's tmux session, as far as
    /// the last poll knows.
    pub fn agent_running(&self) -> bool {
        running(self.activity.as_ref())
    }
}

/// Whether `activity`, as last polled, is an agent running in its session.
pub fn running(activity: Option<&Activity>) -> bool {
    matches!(
        activity,
        Some(
            Activity::Live
                | Activity::Starting
                | Activity::Idle { .. }
                | Activity::Running { .. }
                | Activity::Waiting { .. }
        )
    )
}

/// Which tasks the list shows; ⇥ cycles, ⇧⇥ goes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Active,
    /// The tree under the assistant (design §6).
    Agents,
    /// The active reviews, or, toggled with `A`, the archived reviews:
    /// reviews are shown here alone.
    Reviews,
    /// The active tasks and managers by the project they were
    /// started from, or, toggled with `A`, the projects with no active work.
    Projects,
}

impl View {
    /// The state of the tasks it lists; `None` for the tree, which has its
    /// own rules.
    pub fn state(self) -> Option<TaskState> {
        match self {
            View::Active => Some(TaskState::Active),
            View::Agents | View::Reviews | View::Projects => None,
        }
    }

    fn next(self) -> View {
        match self {
            View::Active => View::Agents,
            View::Agents => View::Reviews,
            View::Reviews => View::Projects,
            View::Projects => View::Active,
        }
    }

    fn prev(self) -> View {
        match self {
            View::Active => View::Projects,
            View::Agents => View::Active,
            View::Reviews => View::Agents,
            View::Projects => View::Reviews,
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
        unanchored: bool,
    },
    Size {
        id: String,
        bytes: u64,
    },
    /// A task's PRs, as GitHub has them now.
    Prs(pr_history::Found),
    /// The assistant, as its files have it now.
    Assistant(Box<AssistantInfo>),
    /// The tree, as the index has it now, for the gather asked for as
    /// `seq`; why it could not be read, if it could not.
    Tree {
        seq: u64,
        info: Result<Box<TreeInfo>, String>,
    },
    /// A reviewed PR, as GitHub has it now, or why it could not be asked.
    Review {
        pr: PrKey,
        found: Result<PrReview, String>,
    },
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

    /// Take the filter as a repository the store may not have yet — a URL or
    /// `owner/repo` — and mark it. False when nothing was typed.
    pub fn add_typed(&mut self) -> bool {
        let typed = self.filter.trim().to_string();
        if typed.is_empty() {
            return false;
        }
        let index = match self.items.iter().position(|i| *i == typed) {
            Some(index) => index,
            None => {
                self.items.push(typed);
                self.marked.push(false);
                self.items.len() - 1
            }
        };
        self.marked[index] = true;
        self.filter.clear();
        self.refilter();
        self.selected = self.visible.iter().position(|&i| i == index).unwrap_or(0);
        true
    }

    /// The keys a multi-select picker takes: ↑↓ move, space marks (or adds
    /// what was typed when it matches nothing), ^a adds what was typed,
    /// anything else narrows. False for a key it has no use for.
    fn multi_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char(' ') if self.visible.is_empty() => {
                self.add_typed();
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('a') if ctrl => {
                self.add_typed();
            }
            KeyCode::Backspace => {
                self.pop();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.push(c),
            _ => return false,
        }
        true
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

/// The new-task form's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Under a node: a task or a manager.
    Kind,
    Name,
    Goal,
    Done,
    Budget,
    Allow,
    Project,
    Repos,
    Branch,
}

/// `n`: a name, and optionally a project, repositories and a branch.
/// Everything left blank makes an empty canvas under a made-up name. Under
/// a node in the agents view, also its kind, goal, done, budget and what it
/// may do (Q17).
#[derive(Debug, Clone)]
pub struct NewTaskForm {
    pub field: Field,
    /// What `--parent` would name (`assistant`, or the node's id) and how
    /// the node is shown; `None` for a top-level task.
    pub parent: Option<(String, String)>,
    /// Work or Manager, under a node.
    pub kind: TaskKind,
    pub name: String,
    /// What a blank name becomes.
    pub default_name: String,
    pub goal: String,
    pub done: String,
    /// Comma-separated, as `--budget` takes it.
    pub budget: String,
    /// Comma-separated capabilities, as `--allow` takes them.
    pub allow: String,
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

/// A task or manager to make under a node, as the form describes it: what
/// `jeet task new --parent` or `jeet manager new` would be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// `assistant`, or the node's id.
    pub parent: String,
    pub kind: TaskKind,
    pub name: String,
    pub goal: Option<String>,
    pub done: Option<String>,
    pub budget: Vec<BudgetPart>,
    pub allow: Option<Vec<String>>,
    pub project: Option<String>,
    pub repos: Vec<String>,
    pub branch: Option<String>,
}

impl ChildSpec {
    /// As `--parent`, `--goal`, `--done`, `--allow` and `--budget` give it.
    pub fn delegation(&self) -> Delegation {
        Delegation {
            parent: Some(self.parent.clone()),
            goal: self.goal.clone(),
            done_when: self.done.clone(),
            allow: self.allow.clone(),
            budget: self.budget.clone(),
        }
    }
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
            parent: None,
            kind: TaskKind::Work,
            name: String::new(),
            default_name,
            goal: String::new(),
            done: String::new(),
            budget: String::new(),
            allow: String::new(),
            projects,
            project,
            repos: Picker::new(repos),
            branch: String::new(),
            error: None,
        }
    }

    /// The form for a task under `parent` (what `--parent` names), shown as
    /// `label`; it starts on the kind.
    pub fn under(self, parent: String, label: String) -> Self {
        NewTaskForm {
            field: Field::Kind,
            parent: Some((parent, label)),
            ..self
        }
    }

    /// The fields shown, in order: a manager has a project but no worktrees
    /// (`manager new` takes none).
    pub fn fields(&self) -> &'static [Field] {
        use Field::*;
        match (&self.parent, self.kind) {
            (None, _) => &[Name, Project, Repos, Branch],
            (Some(_), TaskKind::Manager) => &[Kind, Name, Goal, Done, Budget, Allow, Project],
            (Some(_), _) => &[
                Kind, Name, Goal, Done, Budget, Allow, Project, Repos, Branch,
            ],
        }
    }

    /// The field `delta` steps from the focused one.
    fn step(&self, delta: isize) -> Field {
        let fields = self.fields();
        let at = fields.iter().position(|&f| f == self.field).unwrap_or(0) as isize;
        fields[(at + delta).rem_euclid(fields.len() as isize) as usize]
    }

    /// The text `field` holds, for those typed into.
    fn text_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Goal => Some(&mut self.goal),
            Field::Done => Some(&mut self.done),
            Field::Budget => Some(&mut self.budget),
            Field::Allow => Some(&mut self.allow),
            Field::Branch => Some(&mut self.branch),
            Field::Kind | Field::Project | Field::Repos => None,
        }
    }

    /// A task, or a manager, under a node.
    fn toggle_kind(&mut self) {
        self.kind = match self.kind {
            TaskKind::Manager => TaskKind::Work,
            _ => TaskKind::Manager,
        };
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
    fn name_or_default(&self) -> String {
        match self.name.trim() {
            "" => self.default_name.clone(),
            name => name.to_string(),
        }
    }

    pub fn spec(&self) -> std::result::Result<TaskSpec, String> {
        let name = self.name_or_default();
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

    /// The task under a node the form describes, or why it describes none:
    /// what can be told without the index, as clap parses `--budget` and
    /// `--allow`. The rest is `plan_child`'s.
    pub fn child_spec(&self) -> std::result::Result<ChildSpec, String> {
        let Some((parent, _)) = &self.parent else {
            return Err("a task under a node needs its parent".into());
        };
        let task = match self.kind {
            // A manager shows a project but no repos or branch, so only
            // the project can say what it takes.
            TaskKind::Manager => TaskSpec {
                name: self.name_or_default(),
                project: self.project_name().map(str::to_string),
                repos: Vec::new(),
                branch: None,
            },
            _ => self.spec()?,
        };
        let text = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let parts = |s: &str| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let budget = parts(&self.budget)
            .iter()
            .map(|p| budget::parse_part(p))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| format!("budget: {e}"))?;
        let allow = match parts(&self.allow) {
            caps if caps.is_empty() => None,
            caps => Some(
                caps.iter()
                    .map(|c| permissions::parse_capability(c))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| format!("allow: {e}"))?,
            ),
        };
        Ok(ChildSpec {
            parent: parent.clone(),
            kind: self.kind,
            name: task.name,
            goal: text(&self.goal),
            done: text(&self.done),
            budget,
            allow,
            project: task.project,
            repos: task.repos,
            branch: task.branch,
        })
    }
}

/// Which field of the new-project form has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectField {
    Name,
    Repos,
}

/// `n` in the projects panel: a name, and the repositories every task made
/// from the project starts with.
#[derive(Debug, Clone)]
pub struct NewProjectForm {
    pub field: ProjectField,
    pub name: String,
    pub repos: Picker,
}

impl NewProjectForm {
    pub fn new(repos: Vec<String>) -> Self {
        NewProjectForm {
            field: ProjectField::Name,
            name: String::new(),
            repos: Picker::new(repos),
        }
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
    /// `AGENTS.md, 2 skills`, or why the project cannot be read.
    pub contents: String,
    pub repos: String,
}

/// What a confirm overlay is asking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// `recursive`: with every active task under it (M23).
    Archive {
        task_id: String,
        recursive: bool,
    },
    /// `recursive`: with every task under it (M23).
    Delete {
        task_id: String,
        recursive: bool,
    },
    RemoveWorktree {
        task_id: String,
        dir_name: String,
    },
    DeleteProject {
        name: String,
    },
    ForceResync {
        task_id: String,
    },
    /// `x` in the agents view: a node and its subtree, or with `None` every
    /// task under the assistant (Q16).
    Stop {
        task_id: Option<String>,
    },
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
    NewTask(Box<NewTaskForm>),
    Projects {
        lines: Vec<ProjectLine>,
        selected: usize,
    },
    NewProject(NewProjectForm),
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
    /// `a` in the agents view: the answer to open ask `id`, being typed.
    Answer {
        id: i64,
        /// Which ask, from whom.
        title: String,
        /// The question, as the asker wrote it, made printable.
        question: Vec<String>,
        input: String,
        /// Why the last ⏎ was refused.
        error: Option<String>,
    },
    /// `l`, `i` and `b` in the agents view: text to read, scrolled.
    Text {
        title: String,
        lines: Vec<String>,
        /// Lines scrolled past, as drawn; the draw keeps it in range.
        scroll: u16,
        /// The task whose PRs ⏎ opens, when it has some.
        prs: Option<String>,
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
    /// `v`: the task's PR in the review viewer.
    ReviewViewer {
        task_id: String,
    },
    /// `v` in the reviews view: the review's PR in the review viewer, from
    /// the task's checkout when it has one.
    ReviewDiff {
        task_id: String,
        pr: PrKey,
    },
    /// `R` in the reviews view: ask the review's agent to review again.
    ReviewRerun {
        task_id: String,
    },
    /// `v` in the PRs panel: that PR in the review viewer.
    ViewPr {
        task_id: String,
        repo_id: String,
        number: u64,
    },
    NewTaskForm {
        project: Option<String>,
    },
    CreateTask(TaskSpec),
    /// `n` in the agents view: the form for a task under the node.
    NewChildForm {
        parent_id: String,
    },
    /// Make, as the user, the task or manager the form describes under its
    /// node; a manager starts at once (Q17).
    CreateChild(ChildSpec),
    /// With tasks under it, `a` archives them all, leaves first, once
    /// confirmed: `recursive` says the confirm was for them all.
    Archive {
        task_id: String,
        confirmed: bool,
        recursive: bool,
    },
    /// With every archived task under it, top-down.
    Unarchive {
        task_id: String,
    },
    AskDelete {
        task_id: String,
    },
    /// `recursive`: with every task under it, as confirmed.
    Delete {
        task_id: String,
        recursive: bool,
    },
    Projects,
    NewProjectForm,
    CreateProject {
        name: String,
        repos: Vec<String>,
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
    /// `l` in the agents view: the node's `notes/worklog.md`.
    Worklog {
        task_id: String,
    },
    /// `i` in the agents view: the node's open ask, inbox and reports.
    Reports {
        task_id: String,
    },
    /// `b` in the agents view: the node's brief, grant, lists and budget.
    Brief {
        task_id: String,
    },
    /// `p` in the agents view: pause the node and its subtree.
    Pause {
        task_id: String,
    },
    /// `P` in the agents view: let the node run again.
    Resume {
        task_id: String,
    },
    /// `a` in the agents view: read the open ask of task `task_id` and
    /// open the answer form for it.
    AskAnswer {
        task_id: String,
    },
    /// ⏎ in the answer form: answer ask `id` as the user (R15).
    Answer {
        id: i64,
        text: String,
    },
    /// `x` in the agents view: ask before stopping the node and its
    /// subtree, or with `None`, on the assistant, every task under it.
    AskStop {
        task_id: Option<String>,
    },
    /// The stop, once confirmed.
    Stop {
        task_id: Option<String>,
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
    /// Indices into `rows` on screen, in display order; none in the agents
    /// view, which shows `tree`.
    pub visible: Vec<usize>,
    /// The agents view's lines, in display order.
    pub tree: Vec<TreeLine>,
    /// The managers whose subtree the agents view hides, by task id.
    pub collapsed: HashSet<String>,
    /// The managers the active view has gone into, outermost first: it
    /// lists the tasks under the last, or the top level with none.
    pub path: Vec<String>,
    /// What each active task shows rolled up from below, by task id
    /// ([`managers::active_rollup`]): a manager's own state or its
    /// subtree's, the most urgent.
    pub rolled: HashMap<String, (Urgency, u32)>,
    /// Cursor within `visible`, or `tree` in the agents view.
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
    /// The assistant's own row, the tree's root, once it has one.
    pub assistant_row: Option<TaskRow>,
    /// The tree thread's latest read of the index.
    pub tree_info: Option<TreeInfo>,
    /// The latest gather asked of the tree thread: older answers are
    /// dropped, so one begun before an action never shows what it undid.
    pub tree_seq: u64,
    /// The last gather failed, and the status line said so.
    pub tree_failed: bool,
    /// What each tree node shows, by task id ([`managers::rollup`]).
    pub shown: HashMap<String, (Urgency, u32)>,
    /// jeet runs as the user, not inside an agent's pane: the agents
    /// view's keys that show an agent's text or act need it (Q2).
    pub user: bool,
    /// The reviews, newest activity first: not tasks, so not in `rows`.
    pub review_rows: Vec<ReviewRow>,
    /// The reviews view's lines, in display order.
    pub reviews: Vec<ReviewLine>,
    /// `A` in the reviews view: it lists the archived reviews instead.
    pub archived_reviews: bool,
    /// `A` in the active view: it lists the archived tasks instead, flat.
    pub archived_tasks: bool,
    /// What GitHub last said of each reviewed PR, or why it could not say.
    pub reviewed: HashMap<PrKey, Result<PrReview, String>>,
    /// Every project's name (`projects::list`), as last read.
    pub known_projects: Vec<String>,
    /// The projects of the rows, grouped when they are read.
    pub groups: Groups,
    /// The projects view's top level, in display order.
    pub projects: Vec<ProjectGroup>,
    /// `A` in the projects view: it lists the other projects instead.
    pub archived_projects: bool,
    /// The project the projects view has gone into, or the top level.
    pub project: Option<String>,
    /// The managers gone into within the project, outermost first.
    pub project_path: Vec<String>,
    /// The ids of everyone in `project`.
    pub proj_members: HashSet<String>,
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
            tree: Vec::new(),
            collapsed: HashSet::new(),
            path: Vec::new(),
            rolled: HashMap::new(),
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
            assistant_row: None,
            tree_info: None,
            tree_seq: 0,
            tree_failed: false,
            shown: HashMap::new(),
            user: false,
            review_rows: Vec::new(),
            reviews: Vec::new(),
            archived_reviews: false,
            archived_tasks: false,
            reviewed: HashMap::new(),
            known_projects: Vec::new(),
            groups: Groups::default(),
            projects: Vec::new(),
            archived_projects: false,
            project: None,
            project_path: Vec::new(),
            proj_members: HashSet::new(),
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
                        row.unanchored = was.unanchored;
                    }
                    row.size = was.size;
                    row.last_activity = row.last_activity.max(was.last_activity);
                }
                row
            })
            .collect();
        self.resort(keep.as_deref());
        self.retree();
    }

    /// Replace the reviews with a fresh read of the index, keeping what the
    /// background threads already reported and the cursor where it was.
    pub fn set_reviews(&mut self, reviews: Vec<ReviewRow>) {
        let keep = self.selected_id().map(str::to_string);
        let old = std::mem::take(&mut self.review_rows);
        self.review_rows = reviews
            .into_iter()
            .map(|mut row| {
                if let Some(was) = old.iter().find(|o| o.id() == row.id()) {
                    if row.review.state == was.review.state {
                        row.activity = was.activity.clone();
                        row.unanchored = was.unanchored;
                    }
                    row.size = was.size;
                    row.last_activity = row.last_activity.max(was.last_activity);
                }
                row
            })
            .collect();
        self.resort_reviews(keep.as_deref());
    }

    fn resort_reviews(&mut self, keep: Option<&str>) {
        reviews::sort_rows(&mut self.review_rows);
        self.refresh_visible(keep);
    }

    /// Roll the tree's urgencies up again from what the threads last
    /// found: pure, so the draw reads no index and no tmux.
    pub fn retree(&mut self) {
        self.rolled = managers::active_rollup(&self.rows, self.tree_info.as_ref());
        self.shown = match &self.assistant_row {
            Some(root) => {
                let sessions = self.assistant.as_ref().map_or(&[][..], |a| &a.sessions[..]);
                managers::retree(root, &self.rows, sessions, self.tree_info.as_ref())
            }
            None => HashMap::new(),
        };
    }

    fn resort(&mut self, keep: Option<&str>) {
        sort_rows(&mut self.rows);
        if self.by_priority {
            let items = self.assistant.as_ref().map_or(&[][..], |a| &a.todo[..]);
            sort_by_todo(&mut self.rows, items);
        }
        // After the last sort: its members are indices into the rows.
        self.groups = Groups::new(&self.rows, &self.known_projects);
        self.refresh_visible(keep);
    }

    /// Replace the names of the projects that exist, regrouping.
    pub fn set_known_projects(&mut self, names: Vec<String>) {
        if names == self.known_projects {
            return;
        }
        self.known_projects = names;
        let keep = self.selected_id().map(str::to_string);
        self.groups = Groups::new(&self.rows, &self.known_projects);
        self.refresh_visible(keep.as_deref());
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
        if self.view == View::Reviews {
            self.visible.clear();
            let reviewed = &self.reviewed;
            self.reviews = reviews::lines(
                &self.review_rows,
                self.archived_reviews,
                &self.filter,
                |pr| reviewed.get(pr)?.as_ref().ok().map(|r| r.title.clone()),
            );
            self.selected = keep
                .and_then(|id| {
                    self.reviews
                        .iter()
                        .position(|line| self.review_rows[line.row].id() == id)
                })
                .unwrap_or_else(|| self.selected.min(self.reviews.len().saturating_sub(1)));
            return;
        }
        self.reviews.clear();
        if self.view == View::Projects {
            self.refresh_projects(keep);
            return;
        }
        let Some(mut state) = self.view.state() else {
            self.visible.clear();
            self.tree = managers::lines(
                self.assistant_row.as_ref(),
                &self.rows,
                &self.collapsed,
                &self.filter,
            );
            let (root, rows) = (self.assistant_row.as_ref(), &self.rows);
            self.selected = keep
                .and_then(|id| {
                    self.tree
                        .iter()
                        .position(|line| managers::id_at(line, root, rows) == Some(id))
                })
                .unwrap_or_else(|| self.selected.min(self.tree.len().saturating_sub(1)));
            return;
        };
        self.tree.clear();
        if self.archived_tasks {
            state = TaskState::Archived;
        }
        // A manager gone from the active tasks takes the levels below it.
        if let Some(gone) = self
            .path
            .iter()
            .position(|id| self.active_row(id).is_none())
        {
            self.path.truncate(gone);
        }
        // The active view lists one level; a filter searches them all.
        let here = self.path.last().map(String::as_str);
        let level = self.in_tree() && self.filter.is_empty();
        let in_view: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].task.state == state)
            .filter(|&i| !level || self.tree_parent(&self.rows[i]) == here)
            .collect();
        self.show_ranked(in_view, keep);
    }

    /// Show `in_view` as the filter ranks it, the cursor on `keep` if it is
    /// still shown.
    fn show_ranked(&mut self, in_view: Vec<usize>, keep: Option<&str>) {
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

    /// Whether the list shows the archived ones: the active view's tasks,
    /// the reviews view's reviews or the projects view's projects.
    pub fn archived(&self) -> bool {
        match self.view {
            View::Active => self.archived_tasks,
            View::Reviews => self.archived_reviews,
            View::Projects => self.archived_projects,
            View::Agents => false,
        }
    }

    /// The projects view: the projects, or, inside one, its members a level
    /// at a time (archived ones flat).
    fn refresh_projects(&mut self, keep: Option<&str>) {
        let archived = self.archived_projects;
        let members = self
            .project
            .as_ref()
            .and_then(|name| self.groups.members(archived, name))
            .map(<[usize]>::to_vec);
        let Some(members) = members else {
            // A project nobody is left in takes the levels in it.
            self.project = None;
            self.project_path.clear();
            self.proj_members.clear();
            self.visible.clear();
            // The cursor follows its project as they reorder; a key that
            // moves it to the top empties `projects` first.
            let was = self
                .projects
                .get(self.selected)
                .map(|line| line.name.clone());
            self.projects = self.groups.lines(archived, &self.filter);
            self.selected = was
                .and_then(|name| self.projects.iter().position(|line| line.name == name))
                .unwrap_or_else(|| self.selected.min(self.projects.len().saturating_sub(1)));
            return;
        };
        self.projects.clear();
        self.proj_members = members
            .iter()
            .map(|&i| self.rows[i].task.id.clone())
            .collect();
        if let Some(gone) = self
            .project_path
            .iter()
            .position(|id| !self.proj_members.contains(id))
        {
            self.project_path.truncate(gone);
        }
        let here = self.project_path.last().map(String::as_str);
        let level = self.in_tree() && self.filter.is_empty();
        let in_view: Vec<usize> = members
            .into_iter()
            .filter(|&i| !level || self.level_parent(&self.rows[i]) == here)
            .collect();
        self.show_ranked(in_view, keep);
    }

    /// Whether the list is a tree of active tasks, walked a level at a time:
    /// the active view, or inside a project. Archived tasks are flat.
    pub fn in_tree(&self) -> bool {
        match self.view {
            View::Active => !self.archived_tasks,
            View::Projects => self.project.is_some() && !self.archived_projects,
            View::Agents | View::Reviews => false,
        }
    }

    /// How many lines the list shows: tasks, or the tree's nodes.
    pub fn shown_len(&self) -> usize {
        match self.view {
            View::Agents => self.tree.len(),
            View::Reviews => self.reviews.len(),
            View::Active => self.visible.len(),
            View::Projects if self.project.is_none() => self.projects.len(),
            View::Projects => self.visible.len(),
        }
    }

    /// The task under the cursor; in the agents view, the assistant's own
    /// row at the root.
    pub fn selected_row(&self) -> Option<&TaskRow> {
        match self.view {
            View::Agents => match self.tree.get(self.selected)?.at {
                At::Root => self.assistant_row.as_ref(),
                At::Row(i) => self.rows.get(i),
            },
            View::Reviews => None,
            View::Active | View::Projects => {
                self.visible.get(self.selected).map(|&i| &self.rows[i])
            }
        }
    }

    /// The review under the cursor, in the reviews view.
    pub fn selected_review(&self) -> Option<&ReviewLine> {
        match self.view {
            View::Reviews => self.reviews.get(self.selected),
            _ => None,
        }
    }

    /// The review under the cursor, in the reviews view.
    pub fn selected_review_row(&self) -> Option<&ReviewRow> {
        self.review_rows.get(self.selected_review()?.row)
    }

    /// The active review of `pr`, when its agent is idle as last polled:
    /// what to archive now that GitHub says the PR is done.
    pub fn idle_review_of(&self, pr: &PrKey) -> Option<&ReviewRow> {
        self.review_rows.iter().find(|row| {
            !row.archived()
                && row.pr() == *pr
                && row.activity.as_ref().is_some_and(crate::review::idle)
        })
    }

    /// Every PR the reviews view lists, active or archived, filter or not:
    /// what GitHub is asked.
    pub fn reviewed_prs(&self) -> Vec<PrKey> {
        let mut seen = HashSet::new();
        let lines = |archived| reviews::lines(&self.review_rows, archived, "", |_| None);
        lines(false)
            .into_iter()
            .chain(lines(true))
            .map(|line| line.pr)
            .filter(|pr| seen.insert(pr.clone()))
            .collect()
    }

    /// The id of the task, or in the reviews view the review, under the
    /// cursor.
    pub fn selected_id(&self) -> Option<&str> {
        match self.view {
            View::Reviews => self.selected_review_row().map(ReviewRow::id),
            _ => self.selected_row().map(|row| row.task.id.as_str()),
        }
    }

    pub fn review_row(&self, id: &str) -> Option<&ReviewRow> {
        self.review_rows.iter().find(|row| row.id() == id)
    }

    pub fn row(&self, id: &str) -> Option<&TaskRow> {
        self.rows.iter().find(|row| row.task.id == id)
    }

    fn active_row(&self, id: &str) -> Option<&TaskRow> {
        self.row(id)
            .filter(|row| row.task.state == TaskState::Active)
    }

    /// The active task `row` sits under in the active view's tree; `None`
    /// at the top level, under the assistant, with no parent, or with one
    /// no longer active.
    pub fn tree_parent(&self, row: &TaskRow) -> Option<&str> {
        let parent = self.active_row(row.task.parent_id.as_deref()?)?;
        // A task on a cycle of parents, which only a bad index has, is on
        // top: above it the walk would never end.
        let mut seen = HashSet::new();
        let mut at = Some(parent);
        while let Some(up) = at {
            if up.task.id == row.task.id || !seen.insert(up.task.id.as_str()) {
                return None;
            }
            at = up
                .task
                .parent_id
                .as_deref()
                .and_then(|id| self.active_row(id));
        }
        Some(parent.task.id.as_str())
    }

    /// [`Self::tree_parent`], within the project when inside one: a parent
    /// that is not in it leaves the task on top.
    fn level_parent(&self, row: &TaskRow) -> Option<&str> {
        let mut up = self.tree_parent(row);
        if self.view != View::Projects {
            return up;
        }
        // Inside a project, the nearest ancestor that is in it.
        for _ in 0..=self.rows.len() {
            let id = up?;
            if self.proj_members.contains(id) {
                return Some(id);
            }
            up = self.active_row(id).and_then(|r| self.tree_parent(r));
        }
        None
    }

    /// Whether the active view goes into `row`: a manager, or a task with
    /// active tasks under it all the same.
    pub fn enterable(&self, row: &TaskRow) -> bool {
        if self.view == View::Projects {
            // Only what has a member of the project below it.
            return self.rows.iter().any(|r| {
                self.proj_members.contains(&r.task.id)
                    && self.level_parent(r) == Some(row.task.id.as_str())
            });
        }
        row.task.kind == TaskKind::Manager
            || self.rows.iter().any(|r| {
                r.task.state == TaskState::Active
                    && r.task.parent_id.as_deref() == Some(&row.task.id)
            })
    }

    /// The managers above active task `id`, outermost first: the path that
    /// shows it.
    pub fn ancestors(&self, id: &str) -> Vec<String> {
        let mut path = Vec::new();
        let mut at = self.active_row(id);
        while let Some(parent) = at.and_then(|row| self.level_parent(row)) {
            // A cycle in the parents stops where it comes round.
            if path.iter().any(|p| p == parent) {
                break;
            }
            path.push(parent.to_string());
            at = self.active_row(parent);
        }
        path.reverse();
        path
    }

    /// The managers gone into in the view: the active view's, or the
    /// project's.
    fn path_mut(&mut self) -> &mut Vec<String> {
        match self.view {
            View::Projects => &mut self.project_path,
            _ => &mut self.path,
        }
    }

    /// The managers gone into, outermost first.
    pub fn path(&self) -> &[String] {
        match self.view {
            View::Projects => &self.project_path,
            _ => &self.path,
        }
    }

    /// Where the breadcrumb starts: `assistant`, or `projects` and the
    /// project gone into.
    pub fn crumb_root(&self) -> Vec<String> {
        match (&self.project, self.view) {
            (Some(name), View::Projects) => vec!["projects".to_string(), name.clone()],
            _ => vec!["assistant".to_string()],
        }
    }

    /// The breadcrumb: its root, then the slugs of the managers gone into.
    pub fn breadcrumb(&self) -> Vec<String> {
        let mut crumbs = self.crumb_root();
        crumbs.extend(
            self.path()
                .iter()
                .filter_map(|id| self.row(id))
                .map(|row| row.task.slug.clone()),
        );
        crumbs
    }

    /// → on a manager in the active view: list what is under it. From
    /// a filter's matches, it goes there and drops the filter.
    fn descend(&mut self, id: &str) {
        let mut path = self.ancestors(id);
        path.push(id.to_string());
        *self.path_mut() = path;
        self.filter.clear();
        self.selected = 0;
        self.refresh_visible(None);
        self.set_status(match self.visible.is_empty() {
            true => "nothing under it yet · ← back up",
            false => "",
        });
    }

    /// Drop the filter, the cursor kept on what it was on: in the active
    /// view, on that task's level, wherever the match was.
    fn drop_filter(&mut self, keep: Option<&str>) {
        self.filter.clear();
        if let Some(id) = keep.filter(|_| self.in_tree()) {
            *self.path_mut() = self.ancestors(id);
        }
        self.refresh_visible(keep);
    }

    /// ← or ⌫ in the active view: back up a level, the cursor on the
    /// manager left. With a filter, it drops the filter instead, to the
    /// level of the match under the cursor.
    fn ascend(&mut self) {
        if !self.filter.is_empty() {
            let keep = self.selected_id().map(str::to_string);
            self.drop_filter(keep.as_deref());
            self.set_status("");
            return;
        }
        match self.path_mut().pop() {
            Some(left) => {
                self.refresh_visible(Some(&left));
                self.set_status("");
            }
            None if self.view == View::Projects && self.project.is_some() => {
                let left = self.project.take();
                self.refresh_visible(None);
                if let Some(at) = left.and_then(|k| self.projects.iter().position(|l| l.name == k))
                {
                    self.selected = at;
                }
                self.set_status("");
            }
            None => self.set_status("at the top"),
        }
    }

    /// → or ⏎ on a project: list who is in it.
    fn enter_project(&mut self) {
        let Some(line) = self.projects.get(self.selected) else {
            return;
        };
        self.project = Some(line.name.clone());
        self.project_path.clear();
        self.filter.clear();
        self.selected = 0;
        self.refresh_visible(None);
    }

    /// A node of the tree: a task's row, or the assistant's.
    pub fn node(&self, id: &str) -> Option<&TaskRow> {
        self.row(id).or_else(|| {
            self.assistant_row
                .as_ref()
                .filter(|root| root.task.id == id)
        })
    }

    /// Put the cursor on task `id`, switching to the view that holds it and
    /// dropping a filter that hides it.
    pub fn select(&mut self, id: &str) {
        let (archived, review) = match (self.row(id), self.review_row(id)) {
            (Some(row), _) => (row.task.state == TaskState::Archived, false),
            (None, Some(row)) => (row.archived(), true),
            (None, None) => return,
        };
        self.view = match review {
            true => {
                self.archived_reviews = archived;
                View::Reviews
            }
            false => {
                self.archived_tasks = archived;
                View::Active
            }
        };
        if self.view == View::Active && !archived {
            self.path = self.ancestors(id);
        }
        self.refresh_visible(Some(id));
        if self.selected_id() != Some(id) {
            self.filter.clear();
            self.refresh_visible(Some(id));
        }
    }

    /// How many tasks `view` has; for the agents view, the tree's nodes,
    /// and for the reviews view, its active lines.
    pub fn count(&self, view: View) -> usize {
        match view.state() {
            Some(_) => self.task_count(false),
            None if view == View::Reviews => self.review_count(false),
            None if view == View::Projects => self.groups.count(false),
            None => self.shown.len(),
        }
    }

    /// How many tasks the active view has, unfiltered: active, or
    /// `archived`.
    pub fn task_count(&self, archived: bool) -> usize {
        let state = match archived {
            true => TaskState::Archived,
            false => TaskState::Active,
        };
        self.rows
            .iter()
            .filter(|row| row.task.state == state)
            .count()
    }

    /// How many lines the reviews view has, unfiltered: active, or
    /// `archived`.
    pub fn review_count(&self, archived: bool) -> usize {
        reviews::lines(&self.review_rows, archived, "", |_| None).len()
    }

    /// Take in what a background thread found out.
    pub fn apply(&mut self, update: Update) {
        let keep = self.selected_id().map(str::to_string);
        match update {
            Update::Status {
                id,
                activity,
                last_activity,
                unanchored,
            } => {
                if let Some(row) = self.review_rows.iter_mut().find(|r| r.id() == id) {
                    row.activity = activity;
                    row.unanchored = unanchored;
                    if last_activity > row.last_activity {
                        row.last_activity = last_activity;
                        self.resort_reviews(keep.as_deref());
                    }
                    return;
                }
                let Some(row) = self.rows.iter_mut().find(|r| r.task.id == id) else {
                    return;
                };
                let changed = row.activity != activity;
                row.activity = activity;
                row.unanchored = unanchored;
                if last_activity > row.last_activity {
                    row.last_activity = last_activity;
                    self.resort(keep.as_deref());
                }
                if changed {
                    self.retree();
                }
            }
            Update::Size { id, bytes } => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.task.id == id) {
                    row.size = Some(bytes);
                }
                if let Some(row) = self.review_rows.iter_mut().find(|r| r.id() == id) {
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
                self.retree();
            }
            Update::Review { pr, found } => {
                // A lookup that failed keeps what was found before.
                let known = matches!(self.reviewed.get(&pr), Some(Ok(_)));
                if found.is_ok() || !known {
                    self.reviewed.insert(pr, found);
                }
                if self.view == View::Reviews && !self.filter.is_empty() {
                    self.refresh_visible(keep.as_deref());
                }
            }
            Update::Tree { seq, .. } if seq < self.tree_seq => {}
            Update::Tree {
                info: Ok(mut info), ..
            } => {
                // An inbox a hook held is counted as it was last.
                if let Some(old) = &self.tree_info {
                    for (id, node) in info.nodes.iter_mut() {
                        if node.inbox.is_none() {
                            node.inbox = old.nodes.get(id).and_then(|n| n.inbox);
                        }
                    }
                }
                self.tree_info = Some(*info);
                self.tree_failed = false;
                self.retree();
            }
            // The last read stands; said once, not every two seconds (an
            // index a hook is writing can fail a read).
            Update::Tree { info: Err(e), .. } => {
                if !self.tree_failed {
                    self.tree_failed = true;
                    self.set_status(format!("could not read the tree: {e}"));
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
        self.selected = moved(self.selected, delta, self.shown_len());
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
                    let dir = match self.view {
                        View::Reviews => self.selected_review_row().map(|row| &row.dir),
                        _ => self.selected_row().map(|row| &row.dir),
                    };
                    let exit = match dir {
                        Some(dir) if dir.is_dir() => Exit::ChangeDir(dir.clone()),
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
                self.filtering = false;
                self.drop_filter(keep.as_deref());
            }
            // The filter stays; the letters are commands again.
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                if self.filter.pop().is_none() {
                    self.filtering = false;
                }
                match self.filter.is_empty() {
                    // Its last letter gone: as esc, onto the match's level.
                    true => self.drop_filter(keep.as_deref()),
                    false => self.refresh_visible(None),
                }
            }
            KeyCode::Char('u') if ctrl => self.drop_filter(keep.as_deref()),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.filter.push(c);
                // The best match goes to the top, and so does the cursor.
                self.selected = 0;
                self.projects.clear();
                self.refresh_visible(None);
            }
            _ => {}
        }
        if self.shown_len() == 0 && !self.filter.is_empty() {
            self.set_status("nothing matches — ⌫ to widen the filter");
        } else {
            self.set_status("");
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.view == View::Agents {
            if let Some(action) = self.agents_key(key) {
                return action;
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let id = self.selected_id().map(str::to_string);
        let needs_task = |ui: &mut TasksUi, make: &dyn Fn(String) -> Action| match &id {
            Some(id) => Some(make(id.clone())),
            None => {
                ui.set_status(NO_TASK);
                None
            }
        };
        let needs_review = |ui: &mut TasksUi, make: &dyn Fn(String) -> Action| match ui
            .selected_review_row()
            .map(|row| row.id().to_string())
        {
            Some(id) => Some(make(id)),
            None => {
                ui.set_status(NO_REVIEW);
                None
            }
        };
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.drop_filter(id.as_deref());
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
                self.selected = self.shown_len().saturating_sub(1);
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = match key.code {
                    KeyCode::Tab => self.view.next(),
                    _ => self.view.prev(),
                };
                self.selected = 0;
                self.projects.clear();
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
            // A review has one worktree and one pull request, and no project
            // or priority: the other task keys have nothing to do.
            KeyCode::Char('w' | 'p' | 'S' | 'n' | 'o') if self.view == View::Reviews => None,
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
            KeyCode::Enter if self.view == View::Reviews => {
                needs_review(self, &|task_id| Action::Open {
                    task_id,
                    session: None,
                })
            }
            KeyCode::Char('v') if self.view == View::Reviews => self.review_diff(),
            KeyCode::Char('R') if self.view == View::Reviews => {
                needs_review(self, &|task_id| Action::ReviewRerun { task_id })
            }
            KeyCode::Char('A') if self.view == View::Active => {
                self.archived_tasks = !self.archived_tasks;
                self.selected = 0;
                self.refresh_visible(None);
                self.set_status(match self.archived_tasks {
                    true => "archived tasks · A back to the active ones",
                    false => "active tasks · A shows the archived ones",
                });
                None
            }
            KeyCode::Char('A') if self.view == View::Projects => {
                self.archived_projects = !self.archived_projects;
                self.project = None;
                self.project_path.clear();
                self.selected = 0;
                self.refresh_visible(None);
                self.set_status(match self.archived_projects {
                    true => "other projects · A back to the active ones",
                    false => "active projects · A shows the other ones",
                });
                None
            }
            // The projects view lists projects; → or ⏎ goes into one,
            // and then the active view's walk applies.
            KeyCode::Enter | KeyCode::Right
                if self.view == View::Projects && self.project.is_none() =>
            {
                self.enter_project();
                None
            }
            KeyCode::Char('A') if self.view == View::Reviews => {
                self.archived_reviews = !self.archived_reviews;
                self.selected = 0;
                self.refresh_visible(None);
                self.set_status(match self.archived_reviews {
                    true => "archived reviews · A back to the active ones",
                    false => "active reviews · A shows the archived ones",
                });
                None
            }
            // The active view is a tree, walked like the explorer's folders:
            // → goes into a manager, ⏎ opens its agent, as ctrl-a does.
            KeyCode::Enter | KeyCode::Right if self.in_tree() => match self.selected_row() {
                Some(row)
                    if self.enterable(row)
                        && (key.code == KeyCode::Right || row.task.kind != TaskKind::Manager) =>
                {
                    let id = row.task.id.clone();
                    self.descend(&id);
                    None
                }
                Some(_) if key.code == KeyCode::Right => {
                    self.set_status("a task has nothing under it · ⏎ opens its agent");
                    None
                }
                _ => needs_task(self, &|task_id| Action::Open {
                    task_id,
                    session: None,
                }),
            },
            KeyCode::Left | KeyCode::Backspace if self.in_tree() || self.view == View::Projects => {
                self.ascend();
                None
            }
            // The agent under the cursor, as ⏎; the explorer's key.
            KeyCode::Char('a') if ctrl && self.in_tree() => {
                needs_task(self, &|task_id| Action::Open {
                    task_id,
                    session: None,
                })
            }
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
                View::Active if self.archived_tasks => {
                    needs_task(self, &|task_id| Action::Unarchive { task_id })
                }
                View::Active => needs_task(self, &|task_id| Action::Archive {
                    task_id,
                    confirmed: false,
                    recursive: false,
                }),
                View::Reviews if self.archived_reviews => {
                    needs_task(self, &|task_id| Action::Unarchive { task_id })
                }
                View::Projects
                    if self.selected_row().map(|row| row.task.state)
                        == Some(TaskState::Archived) =>
                {
                    needs_task(self, &|task_id| Action::Unarchive { task_id })
                }
                View::Reviews | View::Projects => needs_task(self, &|task_id| Action::Archive {
                    task_id,
                    confirmed: false,
                    recursive: false,
                }),
                // Taken by `agents_key`.
                View::Agents => None,
            },
            KeyCode::Char('D') => needs_task(self, &|task_id| Action::AskDelete { task_id }),
            KeyCode::Char('S') => needs_task(self, &|task_id| Action::Resync {
                task_id,
                force: false,
            }),
            _ => None,
        }
    }

    /// `v` in the reviews view: the review's PR in the review viewer.
    fn review_diff(&mut self) -> Option<Action> {
        let Some(line) = self.selected_review().cloned() else {
            self.set_status(NO_REVIEW);
            return None;
        };
        Some(Action::ReviewDiff {
            task_id: self.review_rows[line.row].id().to_string(),
            pr: line.pr,
        })
    }

    /// The keys the agents view has of its own (Q22), or `None` for one it
    /// shares with the other views: moving, `/`, `?`, `r`, `q`, `⇥`, `c`,
    /// `C` and `t`.
    fn agents_key(&mut self, key: KeyEvent) -> Option<Option<Action>> {
        let line = self.tree.get(self.selected).cloned();
        let id = self.selected_id().map(str::to_string);
        let action = match key.code {
            // The assistant's sessions, as `C`; a node's agent, as in the
            // active view.
            KeyCode::Enter => match (line.map(|l| l.at), id) {
                (Some(At::Root), _) => Some(Action::AssistantSessions),
                (Some(At::Row(_)), Some(task_id)) => Some(Action::Open {
                    task_id,
                    session: None,
                }),
                _ => {
                    self.set_status(NO_NODE);
                    None
                }
            },
            // Open a collapsed manager, else go to its first child. A filter
            // ignores collapse, so it leaves it alone.
            KeyCode::Right => {
                let line = line?;
                match (line.manager, line.open, id) {
                    (true, false, Some(id)) if self.filter.is_empty() => {
                        self.collapsed.remove(&id);
                        self.refresh_visible(Some(&id));
                    }
                    _ => {
                        let below = self.tree.get(self.selected + 1);
                        if below.is_some_and(|next| next.depth > line.depth) {
                            self.selected += 1;
                        }
                    }
                }
                None
            }
            // Close an open manager, else go to its parent (not while filtering).
            KeyCode::Left => {
                let line = line?;
                match (line.manager, line.open, id) {
                    (true, true, Some(id)) if self.filter.is_empty() => {
                        self.collapsed.insert(id.clone());
                        self.refresh_visible(Some(&id));
                    }
                    _ => {
                        if let Some(up) = managers::parent(&self.tree, self.selected) {
                            self.selected = up;
                        }
                    }
                }
                None
            }
            KeyCode::Char(c @ ('l' | 'i' | 'b')) => match (self.user, id) {
                (false, _) => {
                    self.set_status(NOT_USER);
                    None
                }
                (true, None) => {
                    self.set_status(NO_NODE);
                    None
                }
                (true, Some(task_id)) => Some(match c {
                    'l' => Action::Worklog { task_id },
                    'i' => Action::Reports { task_id },
                    _ => Action::Brief { task_id },
                }),
            },
            // The assistant is refused a pause by `in_scope`, as the CLI
            // refuses it; `x` on it is `stop --all`, the user's alone (P8).
            KeyCode::Char(c @ ('p' | 'P' | 'x')) => match (self.user, line.map(|l| l.at), id) {
                (false, _, _) => {
                    self.set_status(NOT_USER);
                    None
                }
                (true, Some(At::Root), _) if c == 'x' => Some(Action::AskStop { task_id: None }),
                (true, _, Some(task_id)) => Some(match c {
                    'p' => Action::Pause { task_id },
                    'P' => Action::Resume { task_id },
                    _ => Action::AskStop {
                        task_id: Some(task_id),
                    },
                }),
                (true, _, None) => {
                    self.set_status(NO_NODE);
                    None
                }
            },
            // The open question at or nearest below the node, one with the
            // user first, as the tree thread last read them (R15).
            KeyCode::Char('a') => match (self.user, id, &self.assistant_row) {
                (false, _, _) => {
                    self.set_status(NOT_USER);
                    None
                }
                (true, Some(at), Some(root)) => {
                    let nodes = managers::tree_nodes(root, &self.rows);
                    let found = self
                        .tree_info
                        .as_ref()
                        .and_then(|info| managers::nearest_ask(&at, &nodes, info));
                    match found {
                        Some((task_id, _)) => Some(Action::AskAnswer { task_id }),
                        None => {
                            let slug = match self.node(&at) {
                                Some(row) => printable(&row.task.slug),
                                None => at,
                            };
                            self.set_status(format!("no open question at or below {slug}"));
                            None
                        }
                    }
                }
                (true, _, _) => {
                    self.set_status(NO_NODE);
                    None
                }
            },
            // Refused here on a worker, in `plan_child`'s words, rather than
            // after a whole form.
            KeyCode::Char('n') => {
                let node = self.selected_row().map(|row| row.task.clone());
                match (self.user, node) {
                    (false, _) => {
                        self.set_status(NOT_USER);
                        None
                    }
                    (true, None) => {
                        self.set_status(NO_NODE);
                        None
                    }
                    (true, Some(node))
                        if matches!(node.kind, TaskKind::Assistant | TaskKind::Manager) =>
                    {
                        Some(Action::NewChildForm { parent_id: node.id })
                    }
                    (true, Some(node)) => {
                        self.set_status(format!(
                            "task {} is not a manager: only the assistant and managers have \
                             tasks under them",
                            printable(&node.slug)
                        ));
                        None
                    }
                }
            }
            KeyCode::Char('e' | 'w' | 'v' | 'o' | 'D' | 'S') => {
                self.set_status(NOT_IN_AGENTS);
                None
            }
            _ => return None,
        };
        Some(action)
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
                        (scroll + 1).min(tasks_ui::help(self.view).len() as u16)
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
                    (Pending::Archive { task_id, recursive }, true, _, _) => {
                        Some(Action::Archive {
                            task_id: task_id.clone(),
                            confirmed: true,
                            recursive: *recursive,
                        })
                    }
                    (Pending::Delete { task_id, recursive }, true, _, _) => Some(Action::Delete {
                        task_id: task_id.clone(),
                        recursive: *recursive,
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
                    (Pending::Stop { task_id }, true, _, _) => Some(Action::Stop {
                        task_id: task_id.clone(),
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
                    (KeyCode::Enter, _) => {
                        let action = match form.parent {
                            Some(_) => form.child_spec().map(Action::CreateChild),
                            None => form.spec().map(Action::CreateTask),
                        };
                        match action {
                            Ok(action) => {
                                // Left up while the task is made, to come
                                // back to if that fails.
                                self.panel = Some(Panel::NewTask(form));
                                return Some(action);
                            }
                            Err(why) => form.error = Some(why),
                        }
                    }
                    (KeyCode::Tab, _) => form.field = form.step(1),
                    (KeyCode::BackTab, _) => form.field = form.step(-1),
                    (_, Field::Repos) => {
                        form.repos.multi_key(&key);
                    }
                    (KeyCode::Up, _) => form.field = form.step(-1),
                    (KeyCode::Down, _) => form.field = form.step(1),
                    (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), Field::Kind) => {
                        form.toggle_kind()
                    }
                    (KeyCode::Left, Field::Project) => form.cycle_project(-1),
                    (KeyCode::Right | KeyCode::Char(' '), Field::Project) => form.cycle_project(1),
                    (KeyCode::Backspace, field) => {
                        if let Some(text) = form.text_mut(field) {
                            text.pop();
                        }
                    }
                    (KeyCode::Char('u'), field) if ctrl => {
                        if let Some(text) = form.text_mut(field) {
                            text.clear();
                        }
                    }
                    (_, field) => {
                        if let (Some(c), Some(text)) = (typed(&key), form.text_mut(field)) {
                            text.push(c);
                        }
                    }
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
                    (KeyCode::Char('n'), _) => return Some(Action::NewProjectForm),
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
            Panel::NewProject(mut form) => {
                match (key.code, form.field) {
                    (KeyCode::Esc, _) => return Some(Action::Projects),
                    (KeyCode::Enter, _) => {
                        let name = form.name.trim().to_string();
                        if !name.is_empty() {
                            let repos = form.repos.chosen();
                            // Left up while the project is made, to come
                            // back to if that fails.
                            self.panel = Some(Panel::NewProject(form));
                            return Some(Action::CreateProject { name, repos });
                        }
                        form.field = ProjectField::Name;
                        self.set_status("a project needs a name");
                    }
                    (KeyCode::Tab | KeyCode::BackTab, ProjectField::Name) => {
                        form.field = ProjectField::Repos
                    }
                    (KeyCode::Tab | KeyCode::BackTab, ProjectField::Repos) => {
                        form.field = ProjectField::Name
                    }
                    (KeyCode::Down, ProjectField::Name) => form.field = ProjectField::Repos,
                    (KeyCode::Backspace, ProjectField::Name) => {
                        form.name.pop();
                    }
                    (KeyCode::Char('u'), ProjectField::Name) if ctrl => form.name.clear(),
                    (_, ProjectField::Name) => {
                        if let Some(c) = typed(&key) {
                            form.name.push(c);
                        }
                    }
                    (_, ProjectField::Repos) => {
                        form.repos.multi_key(&key);
                    }
                }
                self.panel = Some(Panel::NewProject(form));
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
                    KeyCode::Char('v') => prs.get(selected).map(|pr| Action::ViewPr {
                        task_id: task_id.clone(),
                        repo_id: pr.repo_id.clone(),
                        number: pr.number,
                    }),
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
            Panel::Answer {
                id,
                title,
                question,
                mut input,
                mut error,
            } => {
                let mut action = None;
                match key.code {
                    KeyCode::Esc => return None,
                    KeyCode::Enter if input.trim().is_empty() => {
                        error = Some(TYPE_AN_ANSWER.to_string())
                    }
                    // The form stays up until the answer is in: a refusal
                    // is shown on it.
                    KeyCode::Enter => {
                        error = None;
                        action = Some(Action::Answer {
                            id,
                            text: input.clone(),
                        });
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
                self.panel = Some(Panel::Answer {
                    id,
                    title,
                    question,
                    input,
                    error,
                });
                action
            }
            Panel::Text {
                title,
                lines,
                scroll,
                prs,
            } => {
                // Past the end is drawn as the end ([`tasks_ui::draw`]).
                let scroll = match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return None,
                    KeyCode::Enter => return prs.map(|task_id| Action::Prs { task_id }),
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                    KeyCode::PageUp => scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll.saturating_add(10),
                    KeyCode::Home | KeyCode::Char('g') => 0,
                    KeyCode::End | KeyCode::Char('G') => u16::MAX,
                    _ => scroll,
                };
                self.panel = Some(Panel::Text {
                    title,
                    lines,
                    scroll,
                    prs,
                });
                None
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
                    // Active and archived tasks take two lines each.
                    let lines = match self.view {
                        View::Active | View::Projects => 2,
                        View::Agents | View::Reviews => 1,
                    };
                    let row = self.list.offset() + (mouse.row - area.y - 1) as usize / lines;
                    if row < self.shown_len() {
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

/// What the confirm to archive `name` and the active tasks under it says:
/// that every agent among them ends, and, for each, leaves first, which
/// files git ignores go with its worktrees.
fn subtree_archive_warning(name: &str, unkept: &[(String, Vec<String>)]) -> Vec<String> {
    let mut lines = vec![
        format!(
            "archiving {name} archives the {} under it first, leaves first",
            plural(unkept.len().saturating_sub(1), "active task")
        ),
        "every agent among them is ended, then their worktrees' disk freed".into(),
        "(uncommitted work is kept, and comes back on unarchive)".into(),
        String::new(),
    ];
    for (node, files) in unkept {
        match files.is_empty() {
            true => lines.push(format!("  {node}: nothing lost")),
            false => {
                lines.push(format!(
                    "⚠ {node}: files git ignores go with its worktrees:"
                ));
                lines.extend(files.iter().map(|line| format!("    {line}")));
            }
        }
    }
    lines
}

/// What the confirm to stop `name` and everything below it says, or with
/// `all` every task under the assistant: which of them run, leaves first,
/// as the tree last saw them, and that only the user starts them again.
pub(super) fn stop_warning(name: &str, running: &[String], all: bool) -> Vec<String> {
    let mut lines = vec![
        match all {
            true => "stopping every task under the assistant ends each one's agent, leaves first"
                .to_string(),
            false => {
                format!("stopping {name} ends its agent and every agent below it, leaves first")
            }
        },
        "and starts none of them again until it is resumed".into(),
        String::new(),
    ];
    match running.is_empty() {
        true => lines.push("none of them is running now".into()),
        false => {
            lines.push("running now:".into());
            lines.extend(running.iter().map(|node| format!("  {node}")));
        }
    }
    lines.push(String::new());
    lines.push(match all {
        true => "only you can resume them".into(),
        false => "only you can resume it".into(),
    });
    lines
}

/// What the confirm to delete `name` and every task under it says: for
/// each, leaves first, the directory that goes and the work it would lose.
fn subtree_delete_warning(name: &str, losses: &[(String, PathBuf, Vec<String>)]) -> Vec<String> {
    let mut lines = vec![
        format!(
            "deleting {name} deletes the {} under it first, leaves first",
            plural(losses.len().saturating_sub(1), "task")
        ),
        "every agent among them is ended, then each one's directory removed".into(),
        String::new(),
    ];
    for (node, dir, lost) in losses {
        lines.push(format!("  {node}: removes {}", dir.display()));
        match lost.is_empty() {
            true => lines.push("    nothing would be lost".into()),
            false => {
                lines.push("  ⚠ this work would be lost:".into());
                lines.extend(lost.iter().map(|loss| format!("    {loss}")));
            }
        }
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
    /// A review's: a job of its own may stand in for its agent.
    pub review: bool,
}

/// The background threads: their inboxes, and the one channel they all
/// answer on. Dropping it stops them.
struct Background {
    probes: Sender<Vec<Probe>>,
    sizes: Sender<Vec<(String, PathBuf)>>,
    prs: Sender<Vec<pr_history::Job>>,
    assistant: Sender<assistant_info::Probe>,
    tree: Sender<TreeAsk>,
    reviews: Sender<Vec<reviews::Ask>>,
    updates: Updates,
}

/// The threads' end of the channel they answer on, counting what they sent
/// that is not yet taken.
#[derive(Clone)]
pub(super) struct Outbox {
    tx: Sender<Update>,
    backlog: Arc<AtomicUsize>,
}

impl Outbox {
    /// Fails once the screen is gone.
    pub(super) fn send(&self, update: Update) -> Result<(), mpsc::SendError<Update>> {
        self.backlog.fetch_add(1, Ordering::Relaxed);
        self.tx.send(update)
    }

    /// Whether the screen has stopped taking updates: a poller skips its
    /// round, tmux and all, until it takes them again.
    fn stalled(&self) -> bool {
        self.backlog.load(Ordering::Relaxed) >= BACKLOG_MAX
    }
}

/// The screen's end: taking an update counts it off the backlog.
struct Updates {
    rx: Receiver<Update>,
    backlog: Arc<AtomicUsize>,
}

impl Updates {
    fn taken(&self, update: Update) -> Update {
        self.backlog.fetch_sub(1, Ordering::Relaxed);
        update
    }

    fn try_recv(&self) -> Result<Update, TryRecvError> {
        self.rx.try_recv().map(|u| self.taken(u))
    }

    #[cfg(test)]
    fn recv_timeout(&self, timeout: Duration) -> Result<Update, RecvTimeoutError> {
        self.rx.recv_timeout(timeout).map(|u| self.taken(u))
    }
}

/// What the tree thread is told: gather now and every [`POLL_EVERY`],
/// answering as `seq`, or stop until told again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeAsk {
    Run(u64),
    Stop,
}

/// Where to find what was just archived.
fn shows_archived(review: bool) -> &'static str {
    match review {
        true => "A shows archived reviews",
        false => "A shows archived tasks",
    }
}

/// Whether the tree thread should start (`Some(true)`) or stop
/// (`Some(false)`) when the view goes from `was` to `now`, or go on as it is.
fn tree_wanted(was: View, now: View) -> Option<bool> {
    match (was == View::Agents, now == View::Agents) {
        (false, true) => Some(true),
        (true, false) => Some(false),
        _ => None,
    }
}

impl Background {
    fn start(home: &Path, config: &Config) -> Self {
        let (tx, rx) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let updates = Updates {
            rx,
            backlog: backlog.clone(),
        };
        let tx = Outbox { tx, backlog };
        let (probes, probe_rx) = mpsc::channel();
        let (sizes, size_rx) = mpsc::channel();
        let (prs, pr_rx) = mpsc::channel();
        let (assistant, assistant_rx) = mpsc::channel();
        let (tree, tree_rx) = mpsc::channel();
        let (reviews, reviews_rx) = mpsc::channel();
        let status_tx = tx.clone();
        let reviews_tx = tx.clone();
        let size_tx = tx.clone();
        let assistant_tx = tx.clone();
        let tree_tx = tx.clone();
        let (status_home, assistant_home) = (home.to_path_buf(), home.to_path_buf());
        let (tree_home, tree_config) = (home.to_path_buf(), config.clone());
        std::thread::spawn(move || poll_statuses(probe_rx, status_tx, &status_home));
        std::thread::spawn(move || walk_sizes(size_rx, size_tx));
        std::thread::spawn(move || poll_assistant(assistant_rx, assistant_tx, &assistant_home));
        std::thread::spawn(move || poll_tree(tree_rx, tree_tx, tree_home, tree_config));
        std::thread::spawn(move || reviews::look(reviews_rx, reviews_tx));
        std::thread::spawn(move || find_prs(pr_rx, tx));
        Background {
            probes,
            sizes,
            prs,
            assistant,
            tree,
            reviews,
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

    /// Point both threads at the tasks and reviews now listed, and have
    /// them report straight away.
    fn watch(&self, rows: &[TaskRow], reviews: &[ReviewRow]) {
        self.probe(rows, reviews);
        self.resize(rows, reviews);
    }

    /// Have the status poller read every task's and review's status now.
    fn probe(&self, rows: &[TaskRow], reviews: &[ReviewRow]) {
        let tasks = rows.iter().map(|row| Probe {
            id: row.task.id.clone(),
            dir: row.dir.clone(),
            session: runtime::session_name(&row.task),
            touched: row.task.last_activity,
            active: row.task.state == TaskState::Active,
            review: false,
        });
        let reviews = reviews.iter().map(|row| Probe {
            id: row.id().to_string(),
            dir: row.dir.clone(),
            session: crate::review::session_name(&row.review),
            touched: row.review.last_activity,
            active: !row.archived(),
            review: true,
        });
        let _ = self.probes.send(tasks.chain(reviews).collect());
    }

    fn resize(&self, rows: &[TaskRow], reviews: &[ReviewRow]) {
        let tasks = rows
            .iter()
            .map(|row| (row.task.id.clone(), row.dir.clone()));
        let reviews = reviews
            .iter()
            .map(|row| (row.id().to_string(), row.dir.clone()));
        let _ = self.sizes.send(tasks.chain(reviews).collect());
    }
}

/// Read every task's agent status now and every [`POLL_EVERY`], and again at
/// once whenever a new list of tasks arrives.
fn poll_statuses(probes: Receiver<Vec<Probe>>, tx: Outbox, home: &Path) {
    let tmux_ok = tmux::available();
    // Its own connection, for the anchors; without one, none is unanchored.
    let db = open_index(home);
    let mut current: Vec<Probe> = Vec::new();
    loop {
        for probe in current.iter().filter(|_| !tx.stalled()) {
            if tx.send(probe_status(probe, tmux_ok, db.as_ref())).is_err() {
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

/// A connection of a background thread's own to the index in `home`, if
/// there is one: it never makes one.
fn open_index(home: &Path) -> Option<Database> {
    let path = config::index_path(home);
    path.exists().then(|| Database::open(&path).ok()).flatten()
}

pub fn probe_status(probe: &Probe, tmux_ok: bool, db: Option<&Database>) -> Update {
    Update::Status {
        id: probe.id.clone(),
        activity: probe.active.then(|| {
            let activity = runtime::activity_at(&probe.dir, &probe.session, tmux_ok);
            match probe.review {
                true => crate::review::with_job(activity, db, &probe.id),
                false => activity,
            }
        }),
        last_activity: runtime::last_activity_at(&probe.dir, probe.touched),
        unanchored: probe.active
            && tmux_ok
            && db.is_some_and(|db| runtime::unanchored_in(db, &probe.id, &probe.session)),
    }
}

/// Look at the assistant now and every [`POLL_EVERY`], and again at once
/// whenever a new probe arrives.
fn poll_assistant(probes: Receiver<assistant_info::Probe>, tx: Outbox, home: &Path) {
    let tmux_ok = tmux::available();
    let db = open_index(home);
    let mut current: Option<assistant_info::Probe> = None;
    loop {
        if let Some(probe) = current.as_ref().filter(|_| !tx.stalled()) {
            let windows = match tmux_ok {
                true => tmux::windows(assistant::TMUX_SESSION),
                false => Vec::new(),
            };
            let mut info = assistant_info::gather(probe, &windows);
            if let (Some(db), Some(id)) = (&db, &probe.task_id) {
                info.unanchored = assistant::unanchored_windows(db, id, &windows).len();
            }
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

/// Read the tree from the index on each [`TreeAsk::Run`] and every
/// [`POLL_EVERY`] after, until told to stop: only while the agents view is
/// shown. Its own connection, opened as the other threads open theirs; a
/// home with no index yet is looked at again on the next ask.
fn poll_tree(asks: Receiver<TreeAsk>, tx: Outbox, home: PathBuf, config: Config) {
    let tmux_ok = tmux::available();
    let mut app: Option<App> = None;
    let mut running: Option<u64> = None;
    loop {
        let ask = match running {
            Some(_) => asks.recv_timeout(POLL_EVERY),
            None => asks.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match ask {
            Ok(ask) => {
                // Only the latest of several asks counts.
                let ask = asks.try_iter().last().unwrap_or(ask);
                running = match ask {
                    TreeAsk::Run(seq) => Some(seq),
                    TreeAsk::Stop => None,
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let Some(seq) = running else {
            continue;
        };
        if tx.stalled() {
            continue;
        }
        if app.is_none() {
            app = open_index(&home).map(|db| App {
                home: home.clone(),
                config: config.clone(),
                db,
            });
        }
        let Some(app) = &app else {
            continue;
        };
        let info = managers::gather(app, tmux_ok)
            .map(Box::new)
            .map_err(|e| format!("{e:#}"));
        if tx.send(Update::Tree { seq, info }).is_err() {
            return;
        }
    }
}

/// Walk every task's directory for its size, again every [`RESIZE_EVERY`];
/// a new list of tasks starts the walk over.
fn walk_sizes(jobs: Receiver<Vec<(String, PathBuf)>>, tx: Outbox) {
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
            // The rest of the walk waits for the screen to take updates
            // again, or for a new list.
            while tx.stalled() {
                match jobs.recv_timeout(POLL_EVERY) {
                    Ok(list) => {
                        current = list;
                        continue 'walk;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
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
fn find_prs(jobs: Receiver<Vec<pr_history::Job>>, tx: Outbox) {
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

/// Whether the caller is known to be the user at their own terminal
/// (R17): a top-level task's agent is [`Caller::User`] but not at one.
/// Anyone else, and who it is not being told, fail closed.
fn is_user(caller: Result<Caller>, at_terminal: Result<bool>) -> bool {
    matches!(caller, Ok(Caller::User)) && matches!(at_terminal, Ok(true))
}

/// Run the tasks screen until the user leaves it, returning where the shell
/// should end up.
pub fn run(app: &App, start: Start) -> Result<Exit> {
    // Keep git's own output off the alternate screen.
    git::set_capture_output(true);
    // Inside an agent's pane jeet is that agent: the agents view will not
    // act or show agents' text for it, nor when who it is cannot be told.
    let ui = TasksUi {
        user: is_user(permissions::caller(app), permissions::at_terminal(app)),
        ..TasksUi::default()
    };
    let mut terminal = term::init_terminal()?;
    let mut screen = Screen {
        app,
        terminal: &mut terminal,
        ui,
        bg: Background::start(&app.home, &app.config),
        reviews_asked: None,
        kept_reviews: HashSet::new(),
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
    /// When GitHub was last asked about the reviews, if it has been.
    reviews_asked: Option<Instant>,
    /// Reviews whose PR is done that archiving refused or kept back: not
    /// tried again while the screen is open.
    kept_reviews: HashSet<String>,
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
            // GitHub is asked about the reviews again while they are shown.
            let stale = self
                .reviews_asked
                .is_none_or(|at| at.elapsed() >= reviews::LOOK_EVERY);
            if self.ui.view == View::Reviews && stale {
                self.look_at_reviews();
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
                    // Keys and actions change the view with no way to the
                    // tree thread: tell it here.
                    let was = self.ui.view;
                    if let Some(action) = self.ui.handle_key(key) {
                        self.perform(action);
                    }
                    self.viewed(was);
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
        // A reviewed PR merged or closed archives its review, once idle.
        let done = match &update {
            Update::Review {
                pr,
                found: Ok(found),
            } if crate::review::pr_done(found.state) => self.ui.idle_review_of(pr).map(|row| {
                (
                    row.id().to_string(),
                    pr.clone(),
                    found.state,
                    found.closed_at,
                )
            }),
            _ => None,
        };
        let Update::Prs(found) = update else {
            self.ui.apply(update);
            if let Some((id, pr, state, closed_at)) = done {
                self.retire_review(&id, &pr, state, closed_at);
            }
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

    /// Archive review `id`, as `jeet watch` does, now GitHub says its
    /// PR is `state`: not while its agent is mid-turn (a later look tries
    /// again), and not when archiving would delete ignored files a build
    /// does not make again.
    fn retire_review(&mut self, id: &str, pr: &PrKey, state: &str, closed_at: Option<i64>) {
        if self.kept_reviews.contains(id) {
            return;
        }
        let Ok(Some(review)) = self.app.db.get_review(id) else {
            return;
        };
        let app = self.app;
        let label = format!("archiving the review of {}: it is {state}", pr.label());
        let outcome = self
            .progress(&label, || crate::review::retire(app, &review, closed_at))
            .and_then(|inner| inner);
        match outcome {
            Ok(crate::review::Retired::Archived(warnings)) => {
                let mut line = format!(
                    "archived the review of {}: it is {state} · A shows archived reviews",
                    pr.label()
                );
                if let Some(warning) = warnings.first() {
                    line.push_str(&format!(" · {warning}"));
                }
                self.refresh(false);
                self.ui.set_status(line);
            }
            Ok(crate::review::Retired::Busy(_)) => {}
            Ok(crate::review::Retired::BroughtBack) => {
                self.kept_reviews.insert(id.to_string());
            }
            Ok(crate::review::Retired::Kept(why)) => {
                self.kept_reviews.insert(id.to_string());
                self.ui.set_status(format!(
                    "kept the review of {} though it is {state}: {why} · a archives it",
                    pr.label()
                ));
            }
            Err(e) => {
                self.kept_reviews.insert(id.to_string());
                self.ui.set_status(format!(
                    "could not archive the review of {}: {e:#}",
                    pr.label()
                ));
            }
        }
    }

    /// Have GitHub asked, in the background, about every PR the reviews
    /// view lists. A PR whose repository has left the store says so.
    fn look_at_reviews(&mut self) {
        self.reviews_asked = Some(Instant::now());
        let mut asks = Vec::new();
        for pr in self.ui.reviewed_prs() {
            match self.app.db.get_repo(&pr.repo_id) {
                Ok(Some(repo)) => asks.push(reviews::Ask {
                    pr,
                    trunk: PathBuf::from(&repo.trunk_path),
                }),
                Ok(None) => self.ui.apply(Update::Review {
                    pr,
                    found: Err("its repository is no longer in the store".into()),
                }),
                Err(e) => self.ui.apply(Update::Review {
                    pr,
                    found: Err(format!("{e:#}")),
                }),
            }
        }
        let _ = self.bg.reviews.send(asks);
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
                    let mut warning = tasks::reconcile(app)
                        .err()
                        .map(|e| format!("could not reconcile worktrees: {e:#}"));
                    // Start what waits on `max_live` and has room now, as
                    // `jeet watch` does each poll (M22).
                    let drained = match run::drain(app) {
                        Ok(warnings) => warnings.into_iter().next(),
                        Err(e) => Some(format!("could not start what waits on max_live: {e:#}")),
                    };
                    warning = warning.or(drained);
                    load_rows(app).map(|loaded| (loaded, warning))
                })
                .and_then(|inner| inner),
            false => load_rows(app).map(|loaded| (loaded, None)),
        };
        match loaded {
            Ok((loaded, warning)) => {
                self.read_projects(false);
                let changed = self.set_rows(loaded);
                // Sizes are walked again only when there are new tasks to
                // measure: a walk restarted on every action never finishes
                // a big task.
                if reconcile || changed {
                    self.bg.watch(&self.ui.rows, &self.ui.review_rows);
                } else {
                    self.bg.probe(&self.ui.rows, &self.ui.review_rows);
                }
                // Only on a full refresh — opening, and `r` — as it is a
                // `gh` call per worktree.
                if reconcile {
                    self.discover_prs();
                    if self.ui.view == View::Reviews {
                        self.look_at_reviews();
                    }
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
        self.read_projects(false);
        if self.set_rows(load_rows(self.app)?) {
            self.bg.watch(&self.ui.rows, &self.ui.review_rows);
            if self.ui.view == View::Reviews {
                self.look_at_reviews();
            }
        }
        // New sessions are recorded in the index as they start.
        self.bg.look_at_assistant(assistant_info::probe(self.app)?);
        Ok(())
    }

    /// Read the names of the projects again, for the projects tab's other
    /// projects: a directory listing, on each read of the rows, never per draw.
    /// `regroup`: nothing else is about to (`set_rows` does).
    fn read_projects(&mut self, regroup: bool) {
        match projects::list(self.app) {
            Ok(names) if regroup => self.ui.set_known_projects(names),
            Ok(names) => self.ui.known_projects = names,
            Err(e) => self
                .ui
                .set_status(format!("could not list the projects: {e:#}")),
        }
    }

    /// Replace the rows, saying whether the tasks or reviews themselves
    /// changed rather than only what is known about them.
    fn set_rows(&mut self, loaded: Loaded) -> bool {
        let ids = |rows: &[TaskRow], reviews: &[ReviewRow]| {
            let mut ids: Vec<(String, TaskState)> = rows
                .iter()
                .map(|r| (r.task.id.clone(), r.task.state))
                .chain(reviews.iter().map(|r| (r.id().to_string(), r.review.state)))
                .collect();
            ids.sort_by(|a, b| a.0.cmp(&b.0));
            ids
        };
        let before = ids(&self.ui.rows, &self.ui.review_rows);
        self.ui.assistant_row = loaded.root;
        self.ui.set_rows(loaded.rows);
        self.ui.set_reviews(loaded.reviews);
        let changed = ids(&self.ui.rows, &self.ui.review_rows) != before;
        if changed && self.ui.view == View::Agents {
            self.read_tree();
        }
        changed
    }

    /// Have the tree thread gather now, and every two seconds after; an
    /// answer to an older ask is dropped.
    fn read_tree(&mut self) {
        self.ui.tree_seq += 1;
        let _ = self.bg.tree.send(TreeAsk::Run(self.ui.tree_seq));
    }

    /// The view was `was`: start the tree thread on entering the agents
    /// view, and stop it on leaving.
    fn viewed(&mut self, was: View) {
        // GitHub is asked by the loop, when what it said is stale or a
        // review came along meanwhile that it has not been asked about.
        if was != View::Reviews && self.ui.view == View::Reviews {
            let unknown = self
                .ui
                .reviewed_prs()
                .iter()
                .any(|pr| !self.ui.reviewed.contains_key(pr));
            if unknown {
                self.reviews_asked = None;
            }
        }
        match tree_wanted(was, self.ui.view) {
            Some(true) => self.read_tree(),
            Some(false) => {
                let _ = self.bg.tree.send(TreeAsk::Stop);
            }
            None => {}
        }
    }

    fn task(&self, id: &str) -> Option<TaskRecord> {
        self.ui.row(id).map(|row| row.task.clone())
    }

    /// The review row `id` is, as the index has it now.
    fn review(&self, id: &str) -> Option<ReviewRecord> {
        self.ui.review_row(id)?;
        self.app.db.get_review(id).ok().flatten()
    }

    /// `a` on a review: archive it, once the user has seen what would not
    /// be kept.
    fn archive_review(&mut self, review: ReviewRecord, confirmed: bool) -> Result<()> {
        let app = self.app;
        let running = tmux::pane_alive(&crate::review::session_name(&review));
        let unkept = match confirmed {
            true => Vec::new(),
            false => crate::review::unkept(app, &review),
        };
        if !confirmed && (running || !unkept.is_empty()) {
            self.ui.panel = Some(Panel::Confirm {
                title: format!("archive review {}", review.label()),
                lines: archive_warning(&review.slug, running, &unkept),
                action: Pending::Archive {
                    task_id: review.id.clone(),
                    recursive: false,
                },
            });
            return Ok(());
        }
        let archived = self
            .progress(&format!("archiving {}", review.label()), || {
                crate::review::archive(app, &review)
            })
            .and_then(|inner| inner);
        self.refresh(false);
        match archived {
            Ok(warnings) => self.ui.set_status(with_warnings(
                format!("archived {} — {}", review.label(), shows_archived(true)),
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
        Ok(())
    }

    fn unarchive_review(&mut self, review: ReviewRecord) -> Result<()> {
        let app = self.app;
        let back = self
            .progress(&format!("bringing {} back", review.label()), || {
                crate::review::unarchive(app, &review)
            })
            .and_then(|inner| inner);
        self.refresh(false);
        match back {
            Ok(warnings) => {
                self.ui.select(&review.id);
                self.ui.set_status(with_warnings(
                    format!("{} is back", review.label()),
                    &warnings,
                ));
            }
            Err(e) => self
                .ui
                .message("could not unarchive", vec![format!("{e:#}")], None),
        }
        Ok(())
    }

    fn delete_review(&mut self, review: ReviewRecord) -> Result<()> {
        let app = self.app;
        let deleted = self
            .progress(&format!("deleting {}", review.label()), || {
                crate::review::delete(app, &review)
            })
            .and_then(|inner| inner);
        self.refresh(false);
        match deleted {
            Ok(warnings) => self.ui.set_status(with_warnings(
                format!("deleted {}", review.label()),
                &warnings,
            )),
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
        Ok(())
    }

    /// `D` on a review: ask once, naming what it would lose.
    fn ask_delete_review(&mut self, review: ReviewRecord) -> Result<()> {
        let app = self.app;
        let dir = crate::review::dir(app, &review);
        if crate::commands::task::standing_in(&dir) {
            self.ui.message(
                "cannot delete",
                vec![
                    format!("the shell jeet runs in is inside {}", dir.display()),
                    "cd out of it, then delete the review".into(),
                ],
                None,
            );
            return Ok(());
        }
        if let Err(e) = runtime::refuse_inside_agent_session(
            &review.id,
            &review.slug,
            &crate::review::subject(&review),
            "delete",
        ) {
            self.ui
                .message("cannot delete", vec![format!("{e:#}")], None);
            return Ok(());
        }
        let losses = self
            .progress("checking what would be lost", || {
                crate::review::losses(app, &review)
            })
            .and_then(|inner| inner)?;
        let mut lines = vec![
            format!("removes {} and its worktree", dir.display()),
            String::new(),
        ];
        if losses.is_empty() {
            lines.push("nothing would be lost — no unpushed or uncommitted work".into());
        } else {
            lines.push("⚠ this work would be lost:".into());
            lines.extend(losses.iter().map(|loss| format!("  {loss}")));
        }
        self.ui.panel = Some(Panel::Confirm {
            title: format!("delete review {}", review.label()),
            lines,
            action: Pending::Delete {
                task_id: review.id.clone(),
                recursive: false,
            },
        });
        Ok(())
    }

    /// ⏎ on a review row's session: attach to its agent, starting it first
    /// if it is not running.
    fn open_review(&mut self, review: ReviewRecord, session: Option<String>) -> Result<()> {
        let app = self.app;
        if review.state == TaskState::Archived {
            self.ui
                .set_status(format!("{} is archived — a brings it back", review.label()));
            return Ok(());
        }
        tmux::require()?;
        let name = crate::review::session_name(&review);
        let live = tmux::has_session(&name) && tmux::pane_alive(&name);
        if !live {
            if let Some(job) = crate::review::running_job(app, &review) {
                self.ui.message(
                    "review running",
                    vec![crate::review::running_refusal(&review, &job)],
                    None,
                );
                return Ok(());
            }
        }
        let running = self
            .progress(&format!("starting {}'s agent", review.label()), || {
                crate::review::ensure_running(
                    app,
                    &review,
                    session.as_deref(),
                    None,
                    Admission::Bypass,
                )
            })
            .and_then(|inner| inner)?;
        let attached = term::suspended(self.terminal, || tmux::attach(&running.tmux_session))?;
        let _ = app.db.touch_review(&review.id, now_secs());
        self.refresh(false);
        match attached {
            Ok(_) => self.ui.set_status(with_warnings(
                format!("back from {}", review.label()),
                &running.warnings,
            )),
            Err(e) => self.ui.set_status(format!("could not attach: {e:#}")),
        }
        Ok(())
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
                let (dir, worktrees) = match self.ui.review_row(&task_id) {
                    Some(row) => (row.dir.clone(), row.review.worktree().into_iter().collect()),
                    None => match self.ui.row(&task_id) {
                        Some(row) => (row.dir.clone(), row.worktrees.clone()),
                        None => return Ok(()),
                    },
                };
                let places = places(&dir, &worktrees);
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
                self.bg.resize(&self.ui.rows, &self.ui.review_rows);
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
                self.bg.resize(&self.ui.rows, &self.ui.review_rows);
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
            Action::ViewPr {
                task_id,
                repo_id,
                number,
            } => self.view_pr(&task_id, &repo_id, number)?,
            Action::ReviewDiff { task_id, pr } => self.view_pr(&task_id, &pr.repo_id, pr.number)?,
            Action::ReviewRerun { task_id } => self.rerun_review(&task_id)?,
            Action::NewTaskForm { project } => {
                let projects = projects::list(app)?;
                let repos = app.db.list_repos(None)?.into_iter().map(|r| r.id).collect();
                let default_name = default_task_name(self.ui.rows.iter().map(|r| &r.task));
                self.ui.panel = Some(Panel::NewTask(Box::new(NewTaskForm::new(
                    default_name,
                    projects,
                    project.as_deref(),
                    repos,
                ))));
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
                                child: None,
                                inherit_project: false,
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
            Action::NewChildForm { parent_id } => {
                let Some(parent) = self.ui.node(&parent_id).map(|row| row.task.clone()) else {
                    return Ok(());
                };
                // `find_exact` leaves the assistant out even by id (C7).
                let named = match parent.kind {
                    TaskKind::Assistant => assistant::SLUG.to_string(),
                    _ => parent.id.clone(),
                };
                let projects = projects::list(app)?;
                let repos = app.db.list_repos(None)?.into_iter().map(|r| r.id).collect();
                let default_name = default_task_name(self.ui.rows.iter().map(|r| &r.task));
                // The parent's project is the default; the choice made on
                // the form is then the whole answer.
                let form =
                    NewTaskForm::new(default_name, projects, parent.project.as_deref(), repos);
                self.ui.panel = Some(Panel::NewTask(Box::new(
                    form.under(named, printable(&manager::label(&parent))),
                )));
            }
            Action::CreateChild(spec) => self.create_child(spec),
            Action::Archive {
                task_id,
                confirmed,
                recursive,
            } => {
                if let Some(review) = self.review(&task_id) {
                    return self.archive_review(review, confirmed);
                }
                let Some(row) = self.ui.row(&task_id).cloned() else {
                    return Ok(());
                };
                // With tasks under it, they all go, after one confirm
                // (M23); once confirmed, what was confirmed.
                let recursive = match confirmed {
                    true => recursive,
                    false => app.db.child_count(&row.task.id, true)? > 0,
                };
                if recursive {
                    return self.archive_subtree(row.task, confirmed);
                }
                // Not polled yet is not "not running": ask tmux.
                let running = row.agent_running()
                    || (row.activity.is_none()
                        && tmux::pane_alive(&runtime::session_name(&row.task)));
                let task = row.task;
                // Before asking, not after: the answer would count for nothing.
                if let Err(e) = app.db.check_archive(&task.id) {
                    self.ui
                        .message("cannot archive", vec![format!("{e:#}")], None);
                    return Ok(());
                }
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
                        action: Pending::Archive {
                            task_id,
                            recursive: false,
                        },
                    });
                    return Ok(());
                }
                let archived = self
                    .progress(&format!("archiving {}", task.name), || {
                        tasks::archive(app, &task, false)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                match archived {
                    Ok(warnings) => self.ui.set_status(with_warnings(
                        format!("archived {} — {}", task.name, shows_archived(false)),
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
                if let Some(review) = self.review(&task_id) {
                    return self.unarchive_review(review);
                }
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                // The tasks under it come back with it, as `a` archived
                // them with it.
                let under = tasks::subtree_leaves_first(app, &task)?
                    .iter()
                    .filter(|node| node.state == TaskState::Archived && node.id != task.id)
                    .count();
                let back = self
                    .progress(&format!("bringing {} back", task.name), || {
                        tasks::unarchive(app, &task, true)
                    })
                    .and_then(|inner| inner);
                self.refresh(false);
                match back {
                    Ok(warnings) => {
                        self.ui.select(&task_id);
                        let said = match under {
                            0 => format!("{} is back", task.name),
                            n => format!(
                                "{} is back, with {} under it",
                                task.name,
                                plural(n, "task")
                            ),
                        };
                        self.ui.set_status(with_warnings(said, &warnings));
                    }
                    Err(e) => self
                        .ui
                        .message("could not unarchive", vec![format!("{e:#}")], None),
                }
            }
            Action::AskDelete { task_id } => self.ask_delete(&task_id)?,
            Action::Delete { task_id, recursive } => {
                if let Some(review) = self.review(&task_id) {
                    return self.delete_review(review);
                }
                let Some(task) = self.task(&task_id) else {
                    return Ok(());
                };
                let deleted = self
                    .progress(&format!("deleting {}", task.name), || {
                        tasks::delete(app, &task, recursive)
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
            Action::NewProjectForm => {
                let repos = app.db.list_repos(None)?.into_iter().map(|r| r.id).collect();
                self.ui.panel = Some(Panel::NewProject(NewProjectForm::new(repos)));
            }
            Action::CreateProject { name, repos } => match projects::create(app, &name, &repos) {
                Ok(_) => {
                    self.show_projects(Some(&name))?;
                    self.ui.set_status(format!(
                        "created project {name} — e edits it, ⏎ starts a task"
                    ));
                }
                Err(e) => {
                    let back = self.ui.panel.take();
                    self.ui
                        .message("could not create the project", vec![format!("{e:#}")], back);
                }
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
            Action::Worklog { task_id } => {
                let Some(row) = self.ui.node(&task_id).cloned() else {
                    return Ok(());
                };
                let label = manager::label(&row.task);
                if !matches!(row.task.kind, TaskKind::Assistant | TaskKind::Manager) {
                    self.ui.set_status(format!("{label} keeps no worklog"));
                    return Ok(());
                }
                let path = row.dir.join(manager::WORKLOG_MD);
                let read = self
                    .progress("reading the worklog", || worklog_lines(&path))
                    .and_then(|inner| inner)?;
                match read {
                    Some(lines) => self.ui.panel = Some(text_panel(&label, "worklog", lines, None)),
                    None => self.ui.set_status(format!("{label} has no worklog yet")),
                }
            }
            Action::Reports { task_id } => {
                let Some(row) = self.ui.node(&task_id).cloned() else {
                    return Ok(());
                };
                let (lines, prs) = self
                    .progress("reading its inbox and reports", || {
                        report_lines(app, &row.task, &row.dir)
                    })
                    .and_then(|inner| inner)?;
                let label = manager::label(&row.task);
                let prs = prs.then_some(task_id);
                self.ui.panel = Some(text_panel(&label, "inbox & reports", lines, prs));
            }
            // A read for the user, the assistant's too: `manager show`'s
            // scope, which refuses the assistant, is not asked (C13).
            Action::Brief { task_id } => {
                let Some(row) = self.ui.node(&task_id).cloned() else {
                    return Ok(());
                };
                let lines = self
                    .progress("reading its brief", || brief_lines(app, &row.task))
                    .and_then(|inner| inner)?;
                let label = manager::label(&row.task);
                self.ui.panel = Some(text_panel(&label, "brief", lines, None));
            }
            Action::Pause { task_id } => self.run_verb(Verb::ManagerPause, Some(&task_id)),
            Action::Resume { task_id } => self.run_verb(Verb::ManagerResume, Some(&task_id)),
            Action::AskAnswer { task_id } => self.ask_answer(&task_id)?,
            Action::Answer { id, text } => self.answer(id, &text),
            Action::AskStop { task_id } => self.ask_stop(task_id)?,
            Action::Stop { task_id } => self.run_verb(Verb::ManagerStop, task_id.as_deref()),
        }
        Ok(())
    }

    /// `n` in the agents view: make the task or manager `spec` describes,
    /// as `jeet task new --parent` and `jeet manager new` make it, as the
    /// user: `plan_child`, then `create`, then, for a manager, its start
    /// (Q17). A worker is not started, as `n` in the active view does not
    /// start one. What `plan_child` refuses stays on the form.
    fn create_child(&mut self, spec: ChildSpec) {
        let app = self.app;
        let user = Caller::User;
        let d = spec.delegation();
        let plan = match tasks::plan_child(app, &user, spec.kind, &spec.name, &d, None) {
            Ok(plan) => plan,
            Err(e) => {
                if let Some(Panel::NewTask(form)) = &mut self.ui.panel {
                    form.error = Some(printable(&format!("{e:#}")));
                }
                return;
            }
        };
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
                        child: Some((&user, &plan)),
                        inherit_project: false,
                    },
                )
            })
            .and_then(|inner| inner);
        let created = match made {
            Ok(created) => created,
            Err(e) => {
                let back = self.ui.panel.take();
                self.ui.message(
                    "could not create the task",
                    vec![printable(&format!("{e:#}"))],
                    back,
                );
                return;
            }
        };
        self.ui.panel = None;
        let task = created.task;
        let under = match self.ui.node(spec.parent.as_str()) {
            Some(row) => manager::label(&row.task),
            None => match spec.parent.as_str() {
                assistant::SLUG => "the assistant".to_string(),
                id => format!("task {id}"),
            },
        };
        let mut warnings = created.warnings;
        let made = match task.kind {
            // A manager has only its brief to work towards: it starts now,
            // or queued or held, as `jeet manager new` starts it (M35).
            TaskKind::Manager => {
                let started = self
                    .progress(&format!("starting {}", task.slug), || {
                        commands::task::start(app, &task, &manager::first_prompt(app, &task)?)
                    })
                    .and_then(|inner| inner);
                let how = match started {
                    // The start's warnings, then how it went.
                    Ok(start) => {
                        let mut lines = start.lines(&task);
                        let how = lines.pop().unwrap_or_default();
                        warnings.extend(lines);
                        how
                    }
                    Err(e) => format!("{e:#}"),
                };
                format!("made manager {} under {under} · {how}", task.slug)
            }
            _ => format!("made {} under {under} — ⏎ starts its agent", task.slug),
        };
        self.refresh(false);
        // The new node shows, under its parent opened, with the cursor on it.
        if self.ui.view == View::Agents {
            if let Some(parent) = &task.parent_id {
                self.ui.collapsed.remove(parent);
            }
            self.ui.refresh_visible(Some(&task.id));
        }
        self.read_tree();
        // A deep error path may have written to the terminal (Q20).
        if let Err(e) = term::blank(self.terminal) {
            self.ui.set_status(format!("could not repaint: {e}"));
            return;
        }
        // How it went fits the status line; what it warns of may not.
        let made = printable(&made);
        if !warnings.is_empty() {
            let lines = warnings.iter().map(|w| printable(w)).collect();
            self.ui.message(&made, lines, None);
        }
        self.ui.set_status(made);
    }

    /// `a` in the agents view: the answer form for the ask task `task_id`
    /// has open, as the index has it now.
    fn ask_answer(&mut self, task_id: &str) -> Result<()> {
        let app = self.app;
        let read = self
            .progress("reading the question", || -> Result<_> {
                let Some(asker) = app.db.get_task(task_id)? else {
                    return Ok(None);
                };
                let Some(row) = manager::open_ask(app, &asker)? else {
                    return Ok(None);
                };
                let path = manager::tree_path(app, &asker)?;
                Ok(Some((row, manager::label_at(&asker, &path))))
            })
            .and_then(|inner| inner)?;
        let Some((row, asker)) = read else {
            self.ui
                .set_status("that question was answered or withdrawn meanwhile");
            self.read_tree();
            return Ok(());
        };
        let with = match row.needs_user {
            true => " · with you",
            false => "",
        };
        self.ui.panel = Some(Panel::Answer {
            id: row.id,
            title: printable(&format!(
                "#{} {} from {asker}{with}",
                row.id,
                row.kind.as_str()
            )),
            question: printable_text(&row.body)
                .lines()
                .map(str::to_string)
                .collect(),
            input: String::new(),
            error: None,
        });
        Ok(())
    }

    /// ⏎ in the answer form: answer ask `id` as the user, as `jeet report
    /// answer` does at the user's terminal, which `ui.user` says this is
    /// (R17); then how the answer and each copy went. A refusal stays on
    /// the form.
    fn answer(&mut self, id: i64, text: &str) {
        let app = self.app;
        let answered = self
            .progress(&format!("answering #{id}"), || {
                reports::answer(app, &Caller::User, id, text)
            })
            .and_then(|inner| inner);
        // A deep error path may have written to the terminal (Q20).
        if let Err(e) = term::blank(self.terminal) {
            self.ui.set_status(format!("could not repaint: {e}"));
        }
        let answered = match answered {
            Ok(answered) => answered,
            Err(e) => {
                match &mut self.ui.panel {
                    Some(Panel::Answer { error, .. }) => {
                        *error = Some(printable(&format!("{e:#}")))
                    }
                    _ => self.ui.set_status(printable(&format!("{e:#}"))),
                }
                return;
            }
        };
        let asker = answered.asker.clone();
        let mut lines = vec![commands::report::delivered(
            &asker,
            "the answer",
            &format!("the answer to #{id} is kept, but not delivered to {asker}"),
            &answered.to_asker,
        )];
        for (to, delivered) in &answered.copies {
            lines.push(commands::report::delivered(
                to,
                "its copy",
                &format!("the copy for {to} was not delivered"),
                delivered,
            ));
        }
        let title = printable(&format!("answered #{id} for {asker}"));
        let lines = lines.iter().map(|l| printable(l)).collect();
        self.ui.message(&title, lines, None);
        self.ui.set_status(title);
        self.read_tree();
    }

    /// `x` in the agents view: ask before stopping task `task_id` and its
    /// subtree, or with `None` every task under the assistant, listing
    /// which of them run as the tree last saw them; the stop itself asks
    /// tmux (Q16).
    fn ask_stop(&mut self, task_id: Option<String>) -> Result<()> {
        let app = self.app;
        let (name, targets) = match &task_id {
            Some(id) => {
                let Some(task) = app.db.get_task(id)? else {
                    return Ok(());
                };
                (manager::label(&task), vec![task])
            }
            None => {
                let Some(root) = &self.ui.assistant_row else {
                    self.ui.set_status(NO_NODE);
                    return Ok(());
                };
                let children = app
                    .db
                    .children(&root.task.id)?
                    .into_iter()
                    .filter(|c| c.state == TaskState::Active)
                    .collect();
                ("every task under the assistant".to_string(), children)
            }
        };
        let mut running = Vec::new();
        for target in &targets {
            for node in tasks::subtree_leaves_first(app, target)? {
                // Before the first gather, as the status poller saw it.
                let live = match &self.ui.tree_info {
                    Some(info) => info.live.contains(&node.id),
                    None => self.ui.node(&node.id).is_some_and(TaskRow::agent_running),
                };
                if live {
                    running.push(printable(&node.slug));
                }
            }
        }
        self.ui.panel = Some(Panel::Confirm {
            title: format!("stop {name}"),
            lines: stop_warning(&name, &running, task_id.is_none()),
            action: Pending::Stop { task_id },
        });
        Ok(())
    }

    /// `p`, `P` and `x` in the agents view: `verb` on task `task_id` and its
    /// subtree, or with `None` `stop --all`, as `jeet manager pause`,
    /// `resume` and `stop` do it: `in_scope`, then [`run`], as the user
    /// (Q15). What came of each node is shown; then the screen is painted
    /// afresh and the tree gathered anew, so nothing older shows after it.
    fn run_verb(&mut self, verb: Verb, task_id: Option<&str>) {
        let app = self.app;
        let user = Caller::User;
        let name = match task_id {
            Some(id) => self
                .ui
                .node(id)
                .map_or_else(|| format!("task {id}"), |row| manager::label(&row.task)),
            None => "every task under the assistant".to_string(),
        };
        let (doing, done) = match verb {
            Verb::ManagerPause => ("pausing", "paused"),
            Verb::ManagerResume => ("resuming", "resumed"),
            _ => ("stopping", "stopped"),
        };
        let walked = self
            .progress(&format!("{doing} {name}"), || -> Result<_> {
                // As the index has it now, as the CLI reads it: a resume
                // undoes only what it checked.
                let task = match task_id {
                    Some(id) => match app.db.get_task(id)? {
                        Some(task) => Some(task),
                        None => bail!("task {id} is gone"),
                    },
                    None => None,
                };
                if let Some(task) = &task {
                    permissions::in_scope(app, &user, verb, task)?;
                }
                let outcomes = match (verb, &task) {
                    (Verb::ManagerPause, Some(task)) => run::pause(app, &user, task)?,
                    (Verb::ManagerResume, Some(task)) => run::resume(app, &user, task)?,
                    (Verb::ManagerStop, task) => run::stop(app, &user, task.as_ref())?,
                    _ => bail!("`jeet {}` needs a task", verb.name()),
                };
                let after = match &task {
                    Some(task) => app.db.get_task(&task.id)?.map(|t| t.run_state),
                    None => None,
                };
                Ok((task, outcomes, after))
            })
            .and_then(|inner| inner);
        self.refresh(false);
        self.read_tree();
        // A deep error path may have written to the terminal (Q20).
        if let Err(e) = term::blank(self.terminal) {
            self.ui.set_status(format!("could not repaint: {e}"));
        }
        match walked {
            Ok((task, outcomes, after)) => {
                let title = match after {
                    Some(RunState::Stopped) if verb == Verb::ManagerPause => {
                        format!("kept {name} stopped")
                    }
                    _ => format!("{done} {name}"),
                };
                // The CLI's own summary first; `resume` has none.
                let mut lines = match commands::manager::outcome_summary(verb, task.as_ref(), after)
                {
                    Some(summary) => vec![summary, String::new()],
                    None => Vec::new(),
                };
                lines.extend(
                    outcomes
                        .iter()
                        .map(|o| printable(&format!("{}: {}", o.slug, o.line))),
                );
                self.ui.message(&printable(&title), lines, None);
            }
            Err(e) => self.ui.message(
                &format!("not {done}"),
                vec![printable(&format!("{e:#}"))],
                None,
            ),
        }
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
        let mut back = format!("back from the assistant ({name})");
        if !opened.warnings.is_empty() {
            back.push_str(&format!(" — {}", opened.warnings.join("; ")));
        }
        match attached {
            Ok(_) => self.ui.set_status(back),
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
        if let Some(review) = self.review(task_id) {
            return self.open_review(review, session);
        }
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
        let live = tmux::has_session(&name) && tmux::pane_alive(&name);
        if session.is_none() && !live {
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
        // The user at the terminal asked for it: past `max_live` too (M22).
        let running = self
            .progress(&format!("starting {}'s agent", task.name), || {
                runtime::ensure_running(app, &task, session.as_deref(), None, Admission::Bypass)
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

    /// `v`: the task's PR in the review viewer: a review task's own, else
    /// the one open PR recorded for it, else the PRs panel to pick from.
    fn review_viewer(&mut self, task_id: &str) -> Result<()> {
        if let Some(review) = self.review(task_id) {
            let Some((path, number)) = crate::review::worktree_path(self.app, &review)
                .filter(|path| path.is_dir())
                .map(|path| (path, review.pr_number))
            else {
                self.ui.set_status(format!(
                    "{} has no checkout of its pull request on disk",
                    review.slug
                ));
                return Ok(());
            };
            return self.view_pr_at(&path, number);
        }
        let Some(row) = self.ui.row(task_id).cloned() else {
            return Ok(());
        };
        let prs = self.app.db.list_task_prs(&row.task.id)?;
        match pr_to_view(&prs) {
            PrToView::One(pr) => {
                let (repo_id, number) = (pr.repo_id.clone(), pr.number);
                self.view_pr(task_id, &repo_id, number)
            }
            PrToView::None => {
                self.ui.set_status(format!(
                    "{} has no pull request jeet knows of; p looks for them",
                    row.task.name
                ));
                Ok(())
            }
            PrToView::Pick => {
                self.prs(task_id)?;
                self.ui.set_status("v views the selected pull request");
                Ok(())
            }
        }
    }

    /// The task's PR `number` of `repo_id` in the review viewer, read from
    /// a checkout of the task's of that repository — the PR's own when it
    /// has one — else from the repository's trunk: the viewer asks GitHub
    /// for the diff, so any checkout of the repository will do.
    fn view_pr(&mut self, task_id: &str, repo_id: &str, number: u64) -> Result<()> {
        let (dir, worktrees) = match self.ui.review_row(task_id) {
            Some(row) => (row.dir.clone(), row.review.worktree().into_iter().collect()),
            None => match self.ui.row(task_id) {
                Some(row) => (row.dir.clone(), row.worktrees.clone()),
                None => return Ok(()),
            },
        };
        let mut checkouts: Vec<&TaskWorktreeRecord> = worktrees
            .iter()
            .filter(|wt| wt.repo_id == repo_id)
            .collect();
        checkouts.sort_by_key(|wt| wt.pr_number != Some(number));
        let path = match checkouts
            .iter()
            .map(|wt| dir.join(&wt.dir_name))
            .find(|path| path.is_dir())
        {
            Some(path) => path,
            None => match self.app.db.get_repo(repo_id)? {
                Some(repo) => PathBuf::from(&repo.trunk_path),
                None => {
                    self.ui
                        .set_status(format!("{repo_id} is no longer in the store"));
                    return Ok(());
                }
            },
        };
        self.view_pr_at(&path, number)
    }

    /// `R` on a review: `jeet review <pr> --rerun`, without the viewer.
    fn rerun_review(&mut self, task_id: &str) -> Result<()> {
        let Some(review) = self.review(task_id) else {
            return Ok(());
        };
        if review.state == TaskState::Archived {
            self.ui
                .set_status(format!("{} is archived — a brings it back", review.slug));
            return Ok(());
        }
        let app = self.app;
        let label = format!("asking for a review of #{}", review.pr_number);
        let outcome = self
            .progress(&label, || commands::review::rerun(app, &review))
            .and_then(|inner| inner);
        match outcome {
            Ok(line) => self.ui.set_status(line),
            Err(e) => self
                .ui
                .set_status(format!("could not rerun the review: {e:#}")),
        }
        self.refresh(false);
        Ok(())
    }

    fn view_pr_at(&mut self, path: &Path, number: u64) -> Result<()> {
        let pr = self
            .progress(&format!("reading #{number}"), || {
                github::pr_view(path, number)
            })
            .and_then(|inner| inner)?;
        let outcome = super::review::view(self.terminal, path, &pr)?;
        if outcome.exit == super::review::Exit::Quit {
            self.ui.quit(Exit::Stay);
        }
        Ok(())
    }

    fn ask_delete(&mut self, task_id: &str) -> Result<()> {
        let app = self.app;
        if let Some(review) = self.review(task_id) {
            return self.ask_delete_review(review);
        }
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
        // With tasks under it, they all go, after one confirm (M23).
        if app.db.child_count(&task.id, false)? > 0 {
            return self.ask_delete_subtree(task);
        }
        // Before asking, not after: the answer would count for nothing.
        let refused = tasks::refuse_delete_with_children(app, &task)
            .and_then(|()| runtime::refuse_inside_session(&task, "delete"));
        if let Err(e) = refused {
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
                recursive: false,
            },
        });
        Ok(())
    }

    /// Ask once before deleting `task` and every task under it, naming
    /// what each would lose, leaves first; or say why it cannot be.
    fn ask_delete_subtree(&mut self, task: TaskRecord) -> Result<()> {
        let app = self.app;
        let nodes = tasks::subtree_leaves_first(app, &task)?;
        // Before asking, not after: the answer would count for nothing.
        if let Some(dir) = nodes
            .iter()
            .map(|node| tasks::task_dir(app, node))
            .find(|dir| crate::commands::task::standing_in(dir))
        {
            self.ui.message(
                "cannot delete",
                vec![
                    format!("the shell jeet runs in is inside {}", dir.display()),
                    "cd out of it, then delete the task".into(),
                ],
                None,
            );
            return Ok(());
        }
        if let Err(e) = tasks::refuse_inside_any(&nodes, "delete") {
            self.ui
                .message("cannot delete", vec![format!("{e:#}")], None);
            return Ok(());
        }
        let losses = self
            .progress("checking what would be lost", || {
                tasks::subtree_losses(app, &task)
            })
            .and_then(|inner| inner)?;
        let losses: Vec<(String, PathBuf, Vec<String>)> = losses
            .into_iter()
            .map(|(node, lost)| {
                let dir = tasks::task_dir(app, &node);
                (node.name, dir, lost)
            })
            .collect();
        self.ui.panel = Some(Panel::Confirm {
            title: format!("delete task {} and the tasks under it", task.name),
            lines: subtree_delete_warning(&task.name, &losses),
            action: Pending::Delete {
                task_id: task.id,
                recursive: true,
            },
        });
        Ok(())
    }

    /// Archive `task` and every active task under it, once `confirmed`;
    /// until then, ask, naming what each would not keep.
    fn archive_subtree(&mut self, task: TaskRecord, confirmed: bool) -> Result<()> {
        let app = self.app;
        let nodes: Vec<TaskRecord> = tasks::subtree_leaves_first(app, &task)?
            .into_iter()
            .filter(|node| node.state == TaskState::Active)
            .collect();
        // Before asking, not after: the answer would count for nothing.
        if let Err(e) = tasks::refuse_inside_any(&nodes, "archive") {
            self.ui
                .message("cannot archive", vec![format!("{e:#}")], None);
            return Ok(());
        }
        if !confirmed {
            let unkept = self
                .progress("checking what archiving would not keep", || {
                    tasks::subtree_unkept(app, &task)
                })
                .and_then(|inner| inner)?;
            let unkept: Vec<(String, Vec<String>)> = unkept
                .into_iter()
                .map(|(node, lines)| (node.name, lines))
                .collect();
            self.ui.panel = Some(Panel::Confirm {
                title: format!("archive task {} and the tasks under it", task.name),
                lines: subtree_archive_warning(&task.name, &unkept),
                action: Pending::Archive {
                    task_id: task.id,
                    recursive: true,
                },
            });
            return Ok(());
        }
        let archived = self
            .progress(
                &format!("archiving {} and the tasks under it", task.name),
                || tasks::archive(app, &task, true),
            )
            .and_then(|inner| inner);
        self.refresh(false);
        match archived {
            Ok(warnings) => self.ui.set_status(with_warnings(
                format!(
                    "archived {} and {} under it — {}",
                    task.name,
                    plural(nodes.len().saturating_sub(1), "task"),
                    shows_archived(false)
                ),
                &warnings,
            )),
            Err(e) => self.ui.message(
                "could not finish archiving",
                vec![
                    format!("{e:#}"),
                    String::new(),
                    "a on it again tries again".into(),
                ],
                None,
            ),
        }
        Ok(())
    }

    fn show_projects(&mut self, select: Option<&str>) -> Result<()> {
        let lines = project_lines(self.app)?;
        self.read_projects(true);
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
/// What the index holds, as rows.
struct Loaded {
    rows: Vec<TaskRow>,
    reviews: Vec<ReviewRow>,
    /// The assistant's, the tree's root, if it has one yet: this never makes
    /// it.
    root: Option<TaskRow>,
}

/// Every task's row, every review's, and the assistant's.
fn load_rows(app: &App) -> Result<Loaded> {
    let rows = app
        .db
        .list_tasks(None)?
        .into_iter()
        .map(|task| task_row(app, task))
        .collect::<Result<_>>()?;
    let reviews = app
        .db
        .list_reviews(None)?
        .into_iter()
        .map(|review| {
            let dir = crate::review::dir(app, &review);
            ReviewRow::new(review, dir)
        })
        .collect();
    let root = app
        .db
        .get_assistant_task()?
        .map(|task| task_row(app, task))
        .transpose()?;
    Ok(Loaded {
        rows,
        reviews,
        root,
    })
}

fn task_row(app: &App, task: TaskRecord) -> Result<TaskRow> {
    Ok(TaskRow {
        dir: tasks::task_dir(app, &task),
        worktrees: app.db.list_task_worktrees(&task.id)?,
        activity: None,
        last_activity: task.last_activity,
        size: None,
        unanchored: false,
        report: match task.parent_id {
            Some(_) => app.db.last_report(&task.id)?,
            None => None,
        },
        task,
    })
}

/// Which of a task's PRs `v` shows.
#[derive(Debug, PartialEq, Eq)]
enum PrToView<'a> {
    /// Its one open PR, or the only one it has.
    One(&'a TaskPrRecord),
    None,
    /// Several: the PRs panel, to pick one.
    Pick,
}

fn pr_to_view(prs: &[TaskPrRecord]) -> PrToView<'_> {
    let mut open = prs.iter().filter(|pr| pr.state == "OPEN");
    match (open.next(), open.next(), prs) {
        (Some(pr), None, _) => PrToView::One(pr),
        (None, _, [pr]) => PrToView::One(pr),
        (None, _, []) => PrToView::None,
        _ => PrToView::Pick,
    }
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

/// The most of a worklog `l` reads: its end, where the latest is.
const WORKLOG_TAIL: u64 = 256 * 1024;

/// How many reports `i` lists.
const REPORTS_SHOWN: u32 = 50;

/// A [`Panel::Text`] about `who`, at the top.
fn text_panel(who: &str, what: &str, lines: Vec<String>, prs: Option<String>) -> Panel {
    Panel::Text {
        title: format!("{who} · {what}"),
        lines,
        scroll: 0,
        prs,
    }
}

/// The last [`WORKLOG_TAIL`] bytes of the worklog at `path`, as lines with
/// no control characters but tabs; `None` when there is none.
fn worklog_lines(path: &Path) -> Result<Option<Vec<String>>> {
    let read = || -> std::io::Result<Option<(Vec<u8>, bool)>> {
        let mut file = match messages::open_regular(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        let cut = len > WORKLOG_TAIL;
        if cut {
            file.seek(SeekFrom::Start(len - WORKLOG_TAIL))?;
        }
        let mut bytes = Vec::new();
        file.take(WORKLOG_TAIL).read_to_end(&mut bytes)?;
        Ok(Some((bytes, cut)))
    };
    let Some((bytes, cut)) =
        read().with_context(|| format!("could not read {}", path.display()))?
    else {
        return Ok(None);
    };
    // The cut can fall inside a character, and inside a line: start at
    // the next whole one.
    let all = String::from_utf8_lossy(&bytes);
    let text = match cut {
        true => all.split_once('\n').map_or("", |(_, rest)| rest),
        false => &all,
    };
    Ok(Some(
        printable_text(text).lines().map(str::to_string).collect(),
    ))
}

/// What `i` shows of `task`: the ask it has open, what is queued in its
/// inbox (in `dir`) and the reports by and to it, newest first; and
/// whether it has PRs, which ⏎ then opens. Its words are shown as written,
/// to the user (Q10).
fn report_lines(app: &App, task: &TaskRecord, dir: &Path) -> Result<(Vec<String>, bool)> {
    let who = |id: &str| -> Result<String> {
        Ok(match app.db.get_task(id)? {
            Some(t) => manager::label(&t),
            None => format!("task {id}"),
        })
    };
    let body = |text: &str| -> Vec<String> {
        printable_text(text)
            .lines()
            .map(|line| format!("    {line}"))
            .collect()
    };
    // Where an ask stands: answered, with the user, or with its holder,
    // who `→` names.
    let state = |r: &crate::db::tasks::TaskReportRecord| -> Result<String> {
        Ok(match (&r.answered_by, r.needs_user) {
            (Some(by), _) => format!(" · answered by {}", crate::reports::answerer(app, by)?),
            (None, true) => " · with the user".to_string(),
            (None, false) => String::new(),
        })
    };
    let mut lines = vec!["open ask".to_string()];
    match manager::open_ask(app, task)? {
        Some(r) => {
            lines.push(format!(
                "  #{} {} to {} · {}{}",
                r.id,
                r.kind.as_str(),
                who(&r.to_id)?,
                age_label(r.at),
                state(&r)?
            ));
            lines.extend(body(&r.body));
        }
        None => lines.push("  none".into()),
    }
    lines.push(String::new());
    let queued = messages::peek(dir);
    lines.push(format!("inbox · {} queued", queued.len()));
    for q in &queued {
        lines.push(format!("  {}", age_label(q.at)));
        lines.extend(body(&q.message));
    }
    lines.push(String::new());
    lines.push("reports, newest first".into());
    let reports = app.db.reports_for(&task.id, REPORTS_SHOWN)?;
    if reports.is_empty() {
        lines.push("  none yet".into());
    }
    for r in &reports {
        lines.push(format!(
            "  #{} {} · {} → {} · {}{}",
            r.id,
            r.kind.as_str(),
            who(&r.task_id)?,
            who(&r.to_id)?,
            age_label(r.at),
            state(r)?
        ));
        lines.extend(body(&r.body));
        if let Some(answer) = &r.answer {
            lines.push("    answer:".into());
            lines.extend(body(answer).into_iter().map(|line| format!("  {line}")));
        }
    }
    let prs = !app.db.list_task_prs(&task.id)?.is_empty();
    Ok((lines, prs))
}

/// What `b` shows of `task`: `jeet manager show`'s text, to the user, and
/// the ask it has open itself.
fn brief_lines(app: &App, task: &TaskRecord) -> Result<Vec<String>> {
    let mut text =
        commands::manager::shown_text(&commands::manager::shown(app, &Caller::User, task)?);
    if let Some(r) = manager::open_ask(app, task)? {
        text.push_str(&format!(
            "its own open ask: #{} {}\n",
            r.id,
            r.kind.as_str()
        ));
    }
    Ok(printable_text(&text).lines().map(str::to_string).collect())
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

    fn pr_in(number: u64, state: &str) -> TaskPrRecord {
        TaskPrRecord {
            task_id: "t".into(),
            repo_id: "github.com/acme/api".into(),
            number,
            title: String::new(),
            url: String::new(),
            state: state.into(),
            is_draft: false,
            additions: 0,
            deletions: 0,
            updated_at: String::new(),
        }
    }

    #[test]
    fn v_views_the_one_open_pr_else_asks() {
        assert_eq!(pr_to_view(&[]), PrToView::None);
        let merged = [pr_in(1, "MERGED")];
        assert_eq!(pr_to_view(&merged), PrToView::One(&merged[0]));
        let one_open = [pr_in(1, "MERGED"), pr_in(2, "OPEN")];
        assert_eq!(pr_to_view(&one_open), PrToView::One(&one_open[1]));
        assert_eq!(
            pr_to_view(&[pr_in(1, "OPEN"), pr_in(2, "OPEN")]),
            PrToView::Pick
        );
        assert_eq!(
            pr_to_view(&[pr_in(1, "MERGED"), pr_in(2, "CLOSED")]),
            PrToView::Pick
        );
    }
    use crate::db::tasks::TaskKind;

    #[test]
    fn the_subtree_confirms_name_each_task_leaves_first() {
        let lines = subtree_archive_warning(
            "lead",
            &[
                ("kid".into(), vec!["api: .env".into()]),
                ("lead".into(), Vec::new()),
            ],
        );
        assert!(lines[0].contains("the 1 active task under it"), "{lines:?}");
        let kid = lines.iter().position(|l| l.contains("kid:")).unwrap();
        assert!(lines[kid].starts_with('⚠'), "{lines:?}");
        assert_eq!(lines[kid + 1], "    api: .env");
        assert_eq!(lines.last().unwrap(), "  lead: nothing lost");

        let lines = subtree_delete_warning(
            "lead",
            &[
                ("a".into(), PathBuf::from("/t/a"), Vec::new()),
                (
                    "b".into(),
                    PathBuf::from("/t/b"),
                    vec!["api: 1 commit".into()],
                ),
                ("lead".into(), PathBuf::from("/t/lead"), Vec::new()),
            ],
        );
        assert!(lines[0].contains("the 2 tasks under it"), "{lines:?}");
        let order: Vec<&String> = lines.iter().filter(|l| l.contains("removes")).collect();
        assert_eq!(
            order,
            [
                "  a: removes /t/a",
                "  b: removes /t/b",
                "  lead: removes /t/lead"
            ]
        );
        assert!(
            lines.contains(&"    api: 1 commit".to_string()),
            "{lines:?}"
        );
    }

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
                parent_id: None,
                placement: crate::placement::Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            },
            dir: PathBuf::from(format!("/tasks/{}", tasks::slugify(name))),
            worktrees: Vec::new(),
            activity: None,
            last_activity: last,
            size: None,
            report: None,
            unanchored: false,
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

    /// The assistant's tree as the active view sees it: `ops` (a manager)
    /// over `deploy` (a manager) over `ship`, and `fix` beside `deploy`;
    /// `docs` at the top beside `ops`, and `loose` with no parent at all.
    fn nested() -> TasksUi {
        let under = |id: &str, kind: TaskKind, parent: Option<&str>, last: i64| {
            let mut row = task(id, id, TaskState::Active, last);
            row.task.kind = kind;
            row.task.parent_id = parent.map(str::to_string);
            row
        };
        TasksUi::new(vec![
            under("ops", TaskKind::Manager, Some("assistant-id"), 50),
            under("docs", TaskKind::Work, Some("assistant-id"), 40),
            under("loose", TaskKind::Work, None, 30),
            under("deploy", TaskKind::Manager, Some("ops"), 20),
            under("fix", TaskKind::Work, Some("ops"), 10),
            under("ship", TaskKind::Work, Some("deploy"), 5),
        ])
    }

    #[test]
    fn the_active_view_opens_on_the_top_level_and_walks_down_and_up() {
        let mut ui = nested();
        assert_eq!(shown(&ui), ["ops", "docs", "loose"]);
        assert_eq!(ui.breadcrumb(), ["assistant"]);
        // ⏎ on a manager opens its agent, as ctrl-a does, and stays put.
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "ops".into(),
                session: None
            })
        );
        assert_eq!(shown(&ui), ["ops", "docs", "loose"]);
        // → goes into it.
        assert_eq!(press(&mut ui, KeyCode::Right), None);
        assert_eq!(shown(&ui), ["deploy", "fix"]);
        assert_eq!(ui.breadcrumb(), ["assistant", "ops"]);
        assert_eq!(press(&mut ui, KeyCode::Right), None);
        assert_eq!(shown(&ui), ["ship"]);
        assert_eq!(ui.breadcrumb(), ["assistant", "ops", "deploy"]);
        // On a task, ⏎ opens its agent and → says there is nothing below.
        assert_eq!(press(&mut ui, KeyCode::Right), None);
        assert!(
            ui.status_line.contains("nothing under it"),
            "{}",
            ui.status_line
        );
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "ship".into(),
                session: None
            })
        );
        // ← and ⌫ back up, the cursor on the manager left.
        press(&mut ui, KeyCode::Left);
        assert_eq!(shown(&ui), ["deploy", "fix"]);
        assert_eq!(ui.selected_id(), Some("deploy"));
        press(&mut ui, KeyCode::Backspace);
        assert_eq!(shown(&ui), ["ops", "docs", "loose"]);
        assert_eq!(ui.selected_id(), Some("ops"));
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.status_line, "at the top");
    }

    #[test]
    fn enter_goes_into_a_plain_task_with_tasks_under_it() {
        let mut ui = nested();
        let mut rows = ui.rows.clone();
        rows.iter_mut()
            .find(|r| r.task.id == "loose")
            .unwrap()
            .task
            .parent_id = Some("docs".into());
        ui.set_rows(rows);
        ui.select("docs");
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert_eq!(ui.breadcrumb(), ["assistant", "docs"]);
        assert_eq!(shown(&ui), ["loose"]);
    }

    #[test]
    fn ctrl_a_opens_a_managers_own_agent() {
        let mut ui = nested();
        assert_eq!(
            ui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(Action::Open {
                task_id: "ops".into(),
                session: None
            })
        );
        // Only in the active view: the agents view's `a` answers, and an
        // archived task's agent is not opened.
        for (view, archived) in [(View::Agents, false), (View::Active, true)] {
            ui.view = view;
            ui.archived_tasks = archived;
            let got = ui.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            assert!(
                !matches!(got, Some(Action::Open { .. })),
                "{view:?} archived {archived}: {got:?}"
            );
        }
        ui.view = View::Active;
        ui.archived_tasks = false;
        ui.refresh_visible(None);
        // Plain `a` still archives.
        assert!(matches!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive { .. })
        ));
    }

    #[test]
    fn an_empty_manager_is_entered_and_says_so() {
        let mut ui = nested();
        ui.select("ship");
        press(&mut ui, KeyCode::Left);
        press(&mut ui, KeyCode::Down);
        assert_eq!(ui.selected_id(), Some("fix"));
        // A manager with nothing under it is still a level.
        let mut rows = ui.rows.clone();
        rows.retain(|r| r.task.id != "ship");
        ui.set_rows(rows);
        ui.selected = 0;
        assert_eq!(ui.selected_id(), Some("deploy"));
        press(&mut ui, KeyCode::Right);
        assert!(ui.visible.is_empty());
        assert_eq!(ui.breadcrumb(), ["assistant", "ops", "deploy"]);
        assert!(ui.status_line.contains("← back up"), "{}", ui.status_line);
    }

    #[test]
    fn going_to_a_task_opens_the_levels_above_it() {
        let mut ui = nested();
        ui.select("ship");
        assert_eq!(ui.path, ["ops", "deploy"]);
        assert_eq!(shown(&ui), ["ship"]);
        assert_eq!(ui.selected_id(), Some("ship"));
        ui.select("docs");
        assert!(ui.path.is_empty());
        assert_eq!(ui.selected_id(), Some("docs"));
    }

    #[test]
    fn a_manager_gone_from_the_active_tasks_takes_its_levels_with_it() {
        let mut ui = nested();
        ui.select("ship");
        let rows = ui
            .rows
            .iter()
            .cloned()
            .map(|mut row| {
                if ["deploy", "ship"].contains(&row.task.id.as_str()) {
                    row.task.state = TaskState::Archived;
                }
                row
            })
            .collect();
        ui.set_rows(rows);
        assert_eq!(ui.path, ["ops"]);
        assert_eq!(shown(&ui), ["fix"]);
    }

    #[test]
    fn the_filter_searches_every_level_and_goes_where_a_match_is() {
        let mut ui = nested();
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "ship");
        assert_eq!(shown(&ui), ["ship"]);
        press(&mut ui, KeyCode::Esc);
        // From inside a manager, it still finds what is above.
        ui.select("ship");
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "docs");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(shown(&ui), ["docs"]);
        // ← drops the filter, the cursor kept on the match, on its level.
        press(&mut ui, KeyCode::Left);
        assert!(ui.filter.is_empty());
        assert!(ui.path.is_empty());
        assert_eq!(ui.selected_id(), Some("docs"));
        // So does esc, from a match levels down.
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "ship");
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.path, ["ops", "deploy"]);
        assert_eq!(ui.selected_id(), Some("ship"));
        // ⏎ on a manager among the matches opens its agent; → goes into
        // it, filter dropped.
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "deploy");
        press(&mut ui, KeyCode::Enter);
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "deploy".into(),
                session: None
            })
        );
        press(&mut ui, KeyCode::Right);
        assert!(ui.filter.is_empty());
        assert_eq!(ui.breadcrumb(), ["assistant", "ops", "deploy"]);
        assert_eq!(shown(&ui), ["ship"]);
    }

    #[test]
    fn tasks_on_a_cycle_of_parents_are_on_the_top_level() {
        let mut ui = nested();
        let mut rows = ui.rows.clone();
        for (id, parent) in [("docs", "loose"), ("loose", "docs")] {
            rows.iter_mut()
                .find(|r| r.task.id == id)
                .unwrap()
                .task
                .parent_id = Some(parent.into());
        }
        ui.set_rows(rows);
        assert_eq!(shown(&ui), ["ops", "docs", "loose"]);
        assert!(ui.ancestors("docs").is_empty());
    }

    #[test]
    fn backspacing_the_filter_away_keeps_the_match() {
        let mut ui = nested();
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "ship");
        for _ in 0..4 {
            press(&mut ui, KeyCode::Backspace);
        }
        assert!(ui.filter.is_empty());
        assert_eq!(ui.path, ["ops", "deploy"]);
        assert_eq!(ui.selected_id(), Some("ship"));
    }

    #[test]
    fn a_task_its_own_parent_is_on_the_top_level() {
        let mut ui = nested();
        let mut rows = ui.rows.clone();
        rows.iter_mut()
            .find(|r| r.task.id == "docs")
            .unwrap()
            .task
            .parent_id = Some("docs".into());
        ui.set_rows(rows);
        assert!(shown(&ui).contains(&"docs"));
        assert!(ui.ancestors("docs").is_empty());
    }

    #[test]
    fn the_archived_view_stays_flat() {
        let mut ui = nested();
        ui.rows
            .iter_mut()
            .for_each(|r| r.task.state = TaskState::Archived);
        ui.archived_tasks = true;
        ui.refresh_visible(None);
        assert_eq!(ui.visible.len(), 6);
        // ⏎ there explains rather than going in.
        press(&mut ui, KeyCode::Enter);
        assert!(ui.status_line.contains("archived"), "{}", ui.status_line);
    }

    #[test]
    fn a_manager_rolls_up_the_most_urgent_below_it() {
        let mut ui = nested();
        let status = |ui: &mut TasksUi, id: &str, activity: Activity| {
            ui.apply(Update::Status {
                id: id.into(),
                activity: Some(activity),
                last_activity: 0,
                unanchored: false,
            })
        };
        for id in ["ops", "deploy", "fix", "ship", "docs"] {
            status(&mut ui, id, Activity::Idle { since: 0 });
        }
        assert_eq!(ui.rolled["ops"], (Urgency::Idle, 0));
        // A task running two levels down makes both managers running,
        // though their own chats are idle.
        status(&mut ui, "ship", Activity::Running { detail: None });
        assert_eq!(ui.rolled["deploy"], (Urgency::Running, 1));
        assert_eq!(ui.rolled["ops"], (Urgency::Running, 1));
        assert_eq!(ui.rolled["docs"], (Urgency::Idle, 0));
        // A prompt beats running, and is the user's above the asker.
        status(&mut ui, "fix", Activity::Waiting { detail: None });
        assert_eq!(ui.rolled["ops"], (Urgency::NeedsYou, 1));
        assert_eq!(ui.rolled["deploy"], (Urgency::Running, 1));
        status(&mut ui, "ship", Activity::Running { detail: None });
        status(&mut ui, "deploy", Activity::Running { detail: None });
        assert_eq!(ui.rolled["ops"], (Urgency::NeedsYou, 1));
    }

    #[test]
    fn active_tasks_are_listed_newest_activity_first() {
        let ui = sample();
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        assert_eq!(ui.count(View::Active), 3);
        assert_eq!(ui.task_count(true), 1);
    }

    #[test]
    fn a_toggles_the_active_view_to_the_archived_tasks_and_back() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Char('A'));
        assert!(ui.archived_tasks);
        assert_eq!(ui.view, View::Active);
        assert_eq!(ui.selected, 0);
        assert_eq!(shown(&ui), ["old spike"]);
        assert!(ui.status_line.contains("archived"), "{}", ui.status_line);
        // The toggle stays across views, as the reviews' does.
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Tab);
        assert_eq!((ui.view, ui.archived_tasks), (View::Active, true));
        press(&mut ui, KeyCode::Char('A'));
        assert!(!ui.archived_tasks);
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        // The agents view leaves `A` alone.
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Char('A'));
        assert!(!ui.archived_tasks);
    }

    /// `nested()` with projects: `fix` is of `api`, `ship` and `docs` of
    /// `web`, `ops`, `deploy` and `loose` of none, and an archived `old` of
    /// `gone`; `idle` is a project with no tasks at all.
    fn by_project() -> TasksUi {
        let ui = nested();
        let mut rows = ui.rows.clone();
        let of = |rows: &mut Vec<TaskRow>, id: &str, project: &str| {
            let row = rows.iter_mut().find(|r| r.task.id == id).unwrap();
            row.task.project = Some(project.to_string());
        };
        of(&mut rows, "fix", "api");
        of(&mut rows, "ship", "web");
        of(&mut rows, "docs", "web");
        rows.push(task("old", "old", TaskState::Archived, 1));
        of(&mut rows, "old", "gone");
        let mut ui = TasksUi::new(rows);
        ui.set_known_projects(["api", "gone", "idle", "web"].map(String::from).to_vec());
        ui.view = View::Projects;
        ui.refresh_visible(None);
        ui
    }

    fn projects(ui: &TasksUi) -> Vec<(&str, usize, usize)> {
        ui.projects
            .iter()
            .map(|p| (p.name.as_str(), p.managers, p.tasks))
            .collect()
    }

    #[test]
    fn the_archived_list_deletes_and_explores_its_task() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Char('A'));
        assert_eq!(
            press(&mut ui, KeyCode::Char('D')),
            Some(Action::AskDelete {
                task_id: "c".into()
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('e')),
            Some(Action::Explore {
                task_id: "c".into()
            })
        );
    }

    #[test]
    fn selecting_an_active_task_leaves_the_archived_list_and_walks_to_it() {
        let mut ui = nested();
        press(&mut ui, KeyCode::Char('A'));
        assert!(ui.archived_tasks);
        ui.select("ship");
        assert!(!ui.archived_tasks);
        assert_eq!(ui.view, View::Active);
        assert_eq!(ui.breadcrumb(), ["assistant", "ops", "deploy"]);
        assert_eq!(ui.selected_id(), Some("ship"));
    }

    #[test]
    fn a_capital_a_typed_in_the_filter_is_text() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Char('/'));
        press(&mut ui, KeyCode::Char('A'));
        assert!(!ui.archived_tasks);
        assert_eq!(ui.filter, "A");
    }

    #[test]
    fn the_projects_view_groups_by_project() {
        let ui = by_project();
        assert_eq!(projects(&ui), [("api", 1, 1), ("web", 2, 2)]);
        assert_eq!(ui.count(View::Projects), 2);
        // The top level has no task under the cursor.
        assert_eq!(ui.selected_row().map(|r| r.task.id.as_str()), None);
    }

    #[test]
    fn a_project_is_walked_like_the_active_tree() {
        let mut ui = by_project();
        // → goes into api: ops, which is in it for fix, tops it.
        assert_eq!(press(&mut ui, KeyCode::Right), None);
        assert_eq!(ui.breadcrumb(), ["projects", "api"]);
        assert_eq!(shown(&ui), ["ops"]);
        assert_eq!(press(&mut ui, KeyCode::Right), None);
        assert_eq!(ui.breadcrumb(), ["projects", "api", "ops"]);
        assert_eq!(shown(&ui), ["fix"]);
        // ⏎ opens, as in the active list.
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "fix".into(),
                session: None
            })
        );
        // ← and ⌫ back up, then out of the project, the cursor on it.
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.breadcrumb(), ["projects", "api"]);
        assert_eq!(ui.selected_id(), Some("ops"));
        press(&mut ui, KeyCode::Backspace);
        assert!(ui.project.is_none());
        assert_eq!(projects(&ui)[ui.selected].0, "api");
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.status_line, "at the top");
        // ⏎ on a project goes into it, without opening anything.
        press(&mut ui, KeyCode::Down);
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert_eq!(ui.breadcrumb(), ["projects", "web"]);
        assert_eq!(shown(&ui), ["ops", "docs"]);
    }

    #[test]
    fn task_keys_work_on_a_task_inside_a_project() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Right);
        press(&mut ui, KeyCode::Down);
        assert_eq!(ui.selected_id(), Some("docs"));
        assert_eq!(
            press(&mut ui, KeyCode::Char('e')),
            Some(Action::Explore {
                task_id: "docs".into()
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive {
                task_id: "docs".into(),
                confirmed: false,
                recursive: false
            })
        );
        // Not on a project row.
        press(&mut ui, KeyCode::Left);
        assert_eq!(press(&mut ui, KeyCode::Char('e')), None);
        assert_eq!(ui.status_line, NO_TASK);
    }

    #[test]
    fn a_toggles_the_archived_projects_and_back() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Char('A'));
        assert!(ui.archived_projects);
        assert_eq!(projects(&ui), [("gone", 0, 1), ("idle", 0, 0)]);
        assert!(ui.status_line.contains("other"), "{}", ui.status_line);
        // Inside, the archived tasks, flat; ⏎ explains, a brings back.
        press(&mut ui, KeyCode::Enter);
        assert_eq!(shown(&ui), ["old"]);
        press(&mut ui, KeyCode::Enter);
        assert!(ui.status_line.contains("archived"), "{}", ui.status_line);
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Unarchive {
                task_id: "old".into()
            })
        );
        // ← leaves the project, A the archived ones.
        press(&mut ui, KeyCode::Left);
        assert!(ui.project.is_none());
        press(&mut ui, KeyCode::Char('A'));
        assert!(!ui.archived_projects);
        assert_eq!(ui.projects.len(), 2);
    }

    #[test]
    fn a_manager_reaching_a_project_through_other_projects_tasks_is_walked_to_its_member() {
        let mut ui = by_project();
        let mut rows = ui.rows.clone();
        let mut sub = task("sub", "sub", TaskState::Active, 1);
        sub.task.parent_id = Some("ship".into());
        sub.task.project = Some("docs".into());
        rows.push(sub);
        ui.set_rows(rows);
        // docs: ops and deploy (managers above sub) and sub; ship is web's.
        assert_eq!(
            projects(&ui),
            [("api", 1, 1), ("docs", 2, 1), ("web", 2, 2)]
        );
        ui.selected = 1;
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.breadcrumb(), ["projects", "docs"]);
        assert_eq!(shown(&ui), ["ops"]);
        press(&mut ui, KeyCode::Right);
        assert_eq!(shown(&ui), ["deploy"]);
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.breadcrumb(), ["projects", "docs", "ops", "deploy"]);
        assert_eq!(shown(&ui), ["sub"]);
        // sub has nothing of docs below it.
        let sub = ui.row("sub").unwrap().clone();
        assert!(!ui.enterable(&sub));
        let ship = ui.row("ship").unwrap().clone();
        assert!(!ui.enterable(&ship));
    }

    #[test]
    fn an_archived_project_with_no_tasks_is_entered_empty_and_new_projects_appear() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Char('A'));
        press(&mut ui, KeyCode::Down);
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert_eq!(ui.breadcrumb(), ["projects", "idle"]);
        assert!(shown(&ui).is_empty());
        press(&mut ui, KeyCode::Left);
        assert!(ui.project.is_none());
        assert_eq!(projects(&ui)[ui.selected].0, "idle");
        // A project made meanwhile is listed; one deleted goes.
        ui.set_known_projects(["api", "fresh", "gone", "web"].map(String::from).to_vec());
        assert_eq!(projects(&ui), [("gone", 0, 1), ("fresh", 0, 0)]);
    }

    #[test]
    fn the_filter_narrows_projects_and_a_project_s_tasks() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "we");
        assert_eq!(projects(&ui), [("web", 2, 2)]);
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Right);
        assert!(ui.filter.is_empty());
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "docs");
        assert_eq!(shown(&ui), ["docs"]);
    }

    #[test]
    fn the_cursor_follows_its_project_when_they_reorder() {
        let mut ui = by_project();
        assert_eq!(projects(&ui)[0].0, "api");
        // Newer activity in web alone puts it first; the cursor stays on api.
        ui.apply(Update::Status {
            id: "docs".into(),
            activity: None,
            last_activity: 100,
            unanchored: false,
        });
        assert_eq!(projects(&ui)[0].0, "web");
        assert_eq!(ui.selected, 1);
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.breadcrumb(), ["projects", "api"]);
    }

    #[test]
    fn a_manager_archived_while_inside_takes_its_levels_with_it() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Right);
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.breadcrumb(), ["projects", "api", "ops"]);
        let mut rows = ui.rows.clone();
        rows.iter_mut()
            .find(|r| r.task.id == "ops")
            .unwrap()
            .task
            .state = TaskState::Archived;
        ui.set_rows(rows);
        assert_eq!(ui.breadcrumb(), ["projects", "api"]);
        assert_eq!(shown(&ui), ["fix"]);
        // Gone altogether, it is the same.
        press(&mut ui, KeyCode::Right);
        let rows = ui
            .rows
            .iter()
            .filter(|r| r.task.id != "deploy")
            .cloned()
            .collect();
        ui.set_rows(rows);
        assert_eq!(ui.breadcrumb(), ["projects", "api"]);
        assert_eq!(shown(&ui), ["fix"]);
    }

    #[test]
    fn tab_away_and_back_keeps_the_project_gone_into() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Right);
        press(&mut ui, KeyCode::Right);
        for _ in 0..4 {
            press(&mut ui, KeyCode::Tab);
        }
        assert_eq!(ui.view, View::Projects);
        assert_eq!(ui.breadcrumb(), ["projects", "api", "ops"]);
        assert_eq!(shown(&ui), ["fix"]);
    }

    #[test]
    fn a_click_picks_a_project_two_lines_high() {
        let mut ui = by_project();
        ui.list_area = Rect::new(0, 4, 80, 10);
        // The border, then two lines for each project.
        ui.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 7,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(projects(&ui)[ui.selected].0, "web");
    }

    #[test]
    fn a_project_lists_the_right_rows_when_sorted_by_todo_priority() {
        let mut ui = by_project();
        ui.assistant = Some(AssistantInfo {
            todo: vec![ItemView {
                id: 1,
                done: false,
                text: "docs".into(),
                title: "docs".into(),
                priority: Some(1),
                tasks: vec!["docs".into()],
            }],
            ..AssistantInfo::default()
        });
        ui.by_priority = true;
        ui.set_rows(ui.rows.clone());
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.breadcrumb(), ["projects", "web"]);
        assert_eq!(shown(&ui), ["docs", "ops"]);
    }

    #[test]
    fn a_project_nobody_is_left_in_closes() {
        let mut ui = by_project();
        press(&mut ui, KeyCode::Right);
        let rows = ui
            .rows
            .iter()
            .filter(|r| r.task.id == "loose")
            .cloned()
            .collect();
        ui.set_rows(rows);
        assert!(ui.project.is_none());
        assert!(projects(&ui).is_empty());
    }

    #[test]
    fn tab_cycles_active_agents_reviews_projects_and_back_tab_goes_back() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Agents);
        // No assistant, so no tree.
        assert_eq!(ui.shown_len(), 0);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Reviews);
        // No review tasks.
        assert_eq!(ui.shown_len(), 0);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Projects);
        // The three active tasks have no project.
        assert!(projects(&ui).is_empty());
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Active);
        assert_eq!(shown(&ui), ["api cleanup", "docs", "fix login"]);
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(ui.view, View::Projects);
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(ui.view, View::Reviews);
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(ui.view, View::Agents);
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(ui.view, View::Active);
    }

    fn pr_key(number: u64) -> PrKey {
        PrKey {
            repo_id: "github.com/acme/api".into(),
            number,
        }
    }

    fn review_row(id: &str, state: TaskState, last: i64, pr: u64) -> ReviewRow {
        let mut review = ReviewRecord::new(
            id.into(),
            format!("review-api-{pr}"),
            "github.com/acme/api",
            pr,
            last,
        );
        review.state = state;
        review.dir_name = Some("api".into());
        ReviewRow::new(review, PathBuf::from("/nowhere").join(id))
    }

    fn reviews_sample() -> TasksUi {
        let mut ui = TasksUi::new(vec![task("w", "some work", TaskState::Active, 90)]);
        ui.set_reviews(vec![
            review_row("old", TaskState::Archived, 80, 4),
            review_row("r1", TaskState::Active, 10, 1),
            review_row("r2", TaskState::Active, 20, 2),
        ]);
        ui
    }

    fn reviews_shown(ui: &TasksUi) -> Vec<String> {
        ui.reviews
            .iter()
            .map(|line| ui.review_rows[line.row].id().to_string())
            .collect()
    }

    #[test]
    fn the_reviews_view_lists_every_review() {
        let mut ui = reviews_sample();
        assert_eq!(ui.count(View::Reviews), 2);
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Reviews);
        // Active newest first; the archived one is not among them.
        assert_eq!(reviews_shown(&ui), ["r2", "r1"]);
        // GitHub is asked about the archived ones too.
        assert_eq!(ui.reviewed_prs(), [pr_key(2), pr_key(1), pr_key(4)]);
        // The filter also matches the title GitHub gave.
        ui.apply(Update::Review {
            pr: pr_key(1),
            found: Ok(PrReview {
                title: "Speed up login".into(),
                state: "open",
                verdict: reviews::Verdict::Awaiting,
                closed_at: None,
            }),
        });
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "login");
        assert_eq!(reviews_shown(&ui), ["r1"]);
        press(&mut ui, KeyCode::Esc);
        assert_eq!(reviews_shown(&ui).len(), 2);
    }

    #[test]
    fn a_toggles_the_reviews_view_to_the_archived_reviews_and_back() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        press(&mut ui, KeyCode::Char('A'));
        assert!(ui.archived_reviews);
        assert_eq!(reviews_shown(&ui), ["old"]);
        assert_eq!(ui.review_count(true), 1);
        // The tab still counts the active ones.
        assert_eq!(ui.count(View::Reviews), 2);
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Unarchive {
                task_id: "old".into()
            })
        );
        press(&mut ui, KeyCode::Char('A'));
        assert!(!ui.archived_reviews);
        assert_eq!(reviews_shown(&ui), ["r2", "r1"]);
        // Selecting a review goes to the reviews view's list that has it.
        ui.view = View::Active;
        ui.select("old");
        assert_eq!((ui.view, ui.archived_reviews), (View::Reviews, true));
        assert_eq!(ui.selected_id(), Some("old"));
        ui.select("r1");
        assert_eq!((ui.view, ui.archived_reviews), (View::Reviews, false));
        assert_eq!(ui.selected_id(), Some("r1"));
    }

    #[test]
    fn reviews_are_in_no_task_list() {
        let mut ui = reviews_sample();
        assert_eq!(ui.rows.len(), 1);
        assert_eq!(shown(&ui), ["some work"]);
        assert_eq!(ui.count(View::Active), 1);
        // Filtering searches every level, still without reviews.
        press(&mut ui, KeyCode::Char('/'));
        type_text(&mut ui, "review");
        assert!(ui.visible.is_empty());
        press(&mut ui, KeyCode::Esc);
        // Archived: none of the archived reviews.
        ui.archived_tasks = true;
        ui.set_rows(ui.rows.clone());
        assert!(ui.visible.is_empty());
        assert_eq!(ui.task_count(true), 0);
        // Nor does a review take part in the tree or the projects.
        assert!(ui.row("r1").is_none());
        assert!(ui.node("r1").is_none());
        assert_eq!(ui.groups.count(false), 0, "the work task has no project");
    }

    #[test]
    fn a_review_row_takes_the_polls_findings_and_keeps_them_when_reloaded() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        assert_eq!(reviews_shown(&ui), ["r2", "r1"]);
        ui.apply(Update::Status {
            id: "r1".into(),
            activity: Some(Activity::Running { detail: None }),
            last_activity: 99,
            unanchored: true,
        });
        ui.apply(Update::Size {
            id: "r1".into(),
            bytes: 42,
        });
        // Newer activity moves it up, and the cursor goes with its review.
        assert_eq!(reviews_shown(&ui), ["r1", "r2"]);
        assert_eq!(ui.selected_id(), Some("r2"));
        ui.set_reviews(vec![
            review_row("r1", TaskState::Active, 10, 1),
            review_row("r2", TaskState::Active, 20, 2),
        ]);
        let row = ui.review_row("r1").unwrap();
        assert_eq!(row.size, Some(42));
        assert_eq!(row.last_activity, 99);
        assert!(row.unanchored && row.agent_running());
        // Archived, it is no longer running anything.
        ui.set_reviews(vec![review_row("r1", TaskState::Archived, 10, 1)]);
        assert_eq!(ui.review_row("r1").unwrap().activity, None);
        assert!(ui.review_row("r2").is_none());
    }

    #[test]
    fn a_archives_unarchives_and_d_deletes_the_review_under_the_cursor() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        assert_eq!(ui.selected_id(), Some("r2"));
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive {
                task_id: "r2".into(),
                confirmed: false,
                recursive: false,
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('D')),
            Some(Action::AskDelete {
                task_id: "r2".into()
            })
        );
        // Archived ones are brought back with a, and deleted with D too.
        press(&mut ui, KeyCode::Char('A'));
        assert_eq!(ui.selected_id(), Some("old"));
        assert_eq!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Unarchive {
                task_id: "old".into()
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('D')),
            Some(Action::AskDelete {
                task_id: "old".into()
            })
        );
    }

    #[test]
    fn the_task_keys_a_review_has_no_use_for_do_nothing() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        for key in ['w', 'p', 'S', 'n', 'o'] {
            ui.set_status("");
            assert_eq!(press(&mut ui, KeyCode::Char(key)), None);
            assert_eq!(ui.status_line, "");
            assert!(ui.panel.is_none());
        }
        assert!(!ui.by_priority);
    }

    #[test]
    fn a_done_pr_finds_its_review_only_once_its_agent_is_idle() {
        let mut ui = reviews_sample();
        assert!(ui.idle_review_of(&pr_key(1)).is_none(), "not polled yet");
        let status = |activity| Update::Status {
            id: "r1".into(),
            activity: Some(activity),
            last_activity: 0,
            unanchored: false,
        };
        ui.apply(status(Activity::Running { detail: None }));
        assert!(ui.idle_review_of(&pr_key(1)).is_none(), "mid-turn");
        ui.apply(status(Activity::Waiting { detail: None }));
        assert!(ui.idle_review_of(&pr_key(1)).is_none(), "asking");
        ui.apply(status(Activity::Idle { since: 0 }));
        assert_eq!(ui.idle_review_of(&pr_key(1)).unwrap().id(), "r1");
        // An archived review is never archived again.
        assert!(ui.idle_review_of(&pr_key(4)).is_none());
    }

    #[test]
    fn a_failed_lookup_keeps_what_github_said_before() {
        let mut ui = reviews_sample();
        let found = PrReview {
            title: "t".into(),
            state: "open",
            verdict: reviews::Verdict::Approved,
            closed_at: None,
        };
        ui.apply(Update::Review {
            pr: pr_key(1),
            found: Ok(found.clone()),
        });
        ui.apply(Update::Review {
            pr: pr_key(1),
            found: Err("offline".into()),
        });
        assert_eq!(ui.reviewed[&pr_key(1)], Ok(found));
        ui.apply(Update::Review {
            pr: pr_key(2),
            found: Err("offline".into()),
        });
        assert_eq!(ui.reviewed[&pr_key(2)], Err("offline".into()));
    }

    #[test]
    fn enter_opens_the_reviews_session_and_the_letters_the_rest() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "r2".into(),
                session: None,
            })
        );
        assert!(ui.panel.is_none());
        assert_eq!(
            press(&mut ui, KeyCode::Char('v')),
            Some(Action::ReviewDiff {
                task_id: "r2".into(),
                pr: pr_key(2),
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('R')),
            Some(Action::ReviewRerun {
                task_id: "r2".into()
            })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('e')),
            Some(Action::Explore {
                task_id: "r2".into()
            })
        );
        assert!(matches!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive { task_id, .. }) if task_id == "r2"
        ));
    }

    #[test]
    fn r_in_the_archived_reviews_still_emits_the_rerun_which_try_perform_refuses() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        press(&mut ui, KeyCode::Char('A'));
        assert_eq!(
            press(&mut ui, KeyCode::Char('R')),
            Some(Action::ReviewRerun {
                task_id: "old".into()
            })
        );
    }

    #[test]
    fn r_and_enter_with_no_review_say_so() {
        let mut ui = reviews_sample();
        ui.view = View::Reviews;
        ui.set_reviews(Vec::new());
        for code in [KeyCode::Char('R'), KeyCode::Enter] {
            ui.set_status("");
            assert_eq!(press(&mut ui, code), None);
            assert_eq!(ui.status_line, NO_REVIEW);
        }
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
            unanchored: false,
        });
        assert_eq!(shown(&ui), ["fix login", "api cleanup", "docs"]);
        assert_eq!(ui.selected_id(), Some("d"), "the cursor follows its task");
        // An older time never moves a task down.
        ui.apply(Update::Status {
            id: "a".into(),
            activity: None,
            last_activity: 1,
            unanchored: false,
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
            unanchored: true,
        });
        ui.set_rows(vec![task("a", "fix login", TaskState::Active, 10)]);
        let row = ui.row("a").unwrap();
        assert_eq!(row.size, Some(7));
        assert_eq!(row.activity, Some(Activity::Ended));
        assert!(row.unanchored);
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
                confirmed: false,
                recursive: false
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
    fn in_the_archived_list_a_unarchives_and_enter_explains() {
        let mut ui = sample();
        press(&mut ui, KeyCode::Char('A'));
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
            recursive: false,
        }));
        assert_eq!(press(&mut ui, KeyCode::Char('x')), None);
        assert!(ui.panel.is_some(), "a stray key leaves the question up");
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Delete {
                task_id: "b".into(),
                recursive: false
            })
        );
        // What was confirmed for the tasks under it is what is done.
        ui.panel = Some(confirm(Pending::Delete {
            task_id: "b".into(),
            recursive: true,
        }));
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Delete {
                task_id: "b".into(),
                recursive: true
            })
        );
        ui.panel = Some(confirm(Pending::Archive {
            task_id: "b".into(),
            recursive: true,
        }));
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Archive {
                task_id: "b".into(),
                confirmed: true,
                recursive: true
            })
        );

        ui.panel = Some(confirm(Pending::Delete {
            task_id: "b".into(),
            recursive: false,
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
            recursive: false,
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
            Some(Panel::NewProject(NewProjectForm {
                name: "half".into(),
                ..NewProjectForm::new(Vec::new())
            })),
        );
        press(&mut ui, KeyCode::Enter);
        match &ui.panel {
            Some(Panel::NewProject(form)) => assert_eq!(form.name, "half"),
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
        ui.panel = Some(Panel::NewTask(Box::new(form())));
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
        ui.panel = Some(Panel::NewTask(Box::new(f)));
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
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewProjectForm)
        );
    }

    #[test]
    fn the_new_project_form_picks_repos_like_the_task_form() {
        let mut ui = sample();
        ui.panel = Some(Panel::NewProject(NewProjectForm::new(vec![
            "github.com/acme/api".into(),
            "github.com/acme/web".into(),
        ])));
        // A name is needed first.
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        type_text(&mut ui, "infra");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "web");
        press(&mut ui, KeyCode::Char(' '));
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::CreateProject {
                name: "infra".into(),
                repos: vec!["github.com/acme/web".into()],
            })
        );
        // Left up, so a failure can come back to it.
        assert!(matches!(ui.panel, Some(Panel::NewProject(_))));
        assert_eq!(press(&mut ui, KeyCode::Esc), Some(Action::Projects));
    }

    #[test]
    fn a_repos_picker_adds_a_repository_the_store_lacks() {
        let mut picker = Picker::new(vec!["github.com/acme/api".into()]);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for c in "https://github.com/acme/web".chars() {
            picker.multi_key(&key(KeyCode::Char(c)));
        }
        // Nothing matches, so space adds it rather than marking nothing.
        assert!(picker.visible.is_empty());
        picker.multi_key(&key(KeyCode::Char(' ')));
        assert_eq!(picker.chosen(), vec!["https://github.com/acme/web"]);
        assert!(picker.filter.is_empty());
        // ^a adds what was typed even when it narrows to something else.
        for c in "acme/api2".chars() {
            picker.multi_key(&key(KeyCode::Char(c)));
        }
        picker.multi_key(&key(KeyCode::Backspace));
        assert!(!picker.visible.is_empty());
        picker.multi_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(
            picker.chosen(),
            vec!["https://github.com/acme/web", "acme/api"]
        );
        // Typing an existing item marks it rather than adding it twice.
        for c in "github.com/acme/api".chars() {
            picker.multi_key(&key(KeyCode::Char(c)));
        }
        picker.multi_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(picker.items.len(), 3);
        assert_eq!(picker.chosen().len(), 3);
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
            read_only: false,
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
            review: false,
        };
        assert_eq!(
            probe_status(&probe, false, None),
            Update::Status {
                id: "x".into(),
                activity: None,
                last_activity: 77,
                unanchored: false
            }
        );
        let probe = Probe {
            active: true,
            ..probe
        };
        match probe_status(&probe, false, None) {
            Update::Status { activity, .. } => assert_eq!(activity, Some(Activity::NotStarted)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_background_threads_answer_and_stop_with_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), [1u8; 9]).unwrap();
        let bg = Background::start(Path::new("/nonexistent/jeet-home"), &Config::default());
        let mut row = task("t", "t", TaskState::Archived, 3);
        row.dir = dir.path().to_path_buf();
        bg.watch(&[row], &[]);
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

    #[test]
    fn the_pollers_hold_off_while_the_screen_takes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bg = Background::start(Path::new("/nonexistent/jeet-home"), &Config::default());
        let rows: Vec<TaskRow> = (0..10)
            .map(|i| {
                let mut row = task(&format!("t{i}"), "t", TaskState::Archived, 3);
                row.dir = dir.path().to_path_buf();
                row
            })
            .collect();
        // Many refresh cycles, as while the user is attached to an agent:
        // each new list is a round of the status poller, and nobody takes
        // what it sends.
        const CYCLES: usize = 200;
        for _ in 0..CYCLES {
            bg.probe(&rows, &[]);
            std::thread::sleep(Duration::from_millis(2));
        }
        // However slow the machine, the first round lands.
        bg.updates.recv_timeout(Duration::from_secs(10)).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let mut queued = 1;
        while bg.updates.try_recv().is_ok() {
            queued += 1;
        }
        // Unbounded, it would be a round of rows per cycle.
        assert!(
            queued <= BACKLOG_MAX + rows.len() && queued < CYCLES * rows.len() / 4,
            "{queued} updates piled up over {CYCLES} cycles"
        );
        // Taken again, it answers again.
        bg.probe(&rows, &[]);
        assert!(matches!(
            bg.updates.recv_timeout(Duration::from_secs(10)).unwrap(),
            Update::Status { .. }
        ));
        drop(bg);
    }

    #[test]
    fn the_tree_thread_gathers_when_told_and_stops_with_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let config = config::load_or_create(&home).unwrap();
        Database::open(&config::index_path(&home)).unwrap();
        let bg = Background::start(&home, &config);
        bg.tree.send(TreeAsk::Run(7)).unwrap();
        let seq = loop {
            match bg.updates.recv_timeout(Duration::from_secs(10)).unwrap() {
                Update::Tree { seq, info } => {
                    let info = info.unwrap();
                    // No assistant, and none made.
                    assert!(info.nodes.is_empty());
                    break seq;
                }
                _ => continue,
            }
        };
        assert_eq!(seq, 7);
        bg.tree.send(TreeAsk::Stop).unwrap();
        // An answer already on its way may still arrive; then, none.
        std::thread::sleep(Duration::from_millis(500));
        while bg.updates.try_recv().is_ok() {}
        let late = bg
            .updates
            .recv_timeout(POLL_EVERY + Duration::from_millis(500));
        assert!(!matches!(late, Ok(Update::Tree { .. })));
        drop(bg);
    }

    #[test]
    fn the_tree_thread_runs_only_in_the_agents_view() {
        assert_eq!(tree_wanted(View::Active, View::Agents), Some(true));
        assert_eq!(tree_wanted(View::Agents, View::Active), Some(false));
        assert_eq!(tree_wanted(View::Agents, View::Agents), None);
        assert_eq!(tree_wanted(View::Active, View::Reviews), None);
    }

    #[test]
    fn the_tree_thread_returns_the_nodes_of_a_populated_index() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let config = config::load_or_create(&home).unwrap();
        let db = Database::open(&config::index_path(&home)).unwrap();
        let mut root = task("root", "assistant", TaskState::Active, 0).task;
        root.kind = TaskKind::Assistant;
        db.insert_task(&root).unwrap();
        let bg = Background::start(&home, &config);
        bg.tree.send(TreeAsk::Run(1)).unwrap();
        loop {
            if let Update::Tree { info, .. } =
                bg.updates.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                assert!(info.unwrap().nodes.contains_key("root"));
                break;
            }
        }
        bg.tree.send(TreeAsk::Stop).unwrap();
    }

    /// The assistant, with `w` waiting on a prompt under manager `m`.
    fn tree_sample() -> TasksUi {
        let mut ui = sample();
        let mut root = task("root", "assistant", TaskState::Active, 0);
        root.task.kind = TaskKind::Assistant;
        let mut m = task("m", "m", TaskState::Active, 0);
        m.task.kind = TaskKind::Manager;
        m.task.parent_id = Some("root".into());
        let mut w = task("w", "w", TaskState::Active, 0);
        w.task.parent_id = Some("m".into());
        let mut rows = ui.rows.clone();
        rows.extend([m, w]);
        ui.assistant_row = Some(root);
        ui.set_rows(rows);
        ui
    }

    #[test]
    fn the_tree_rolls_up_as_statuses_and_tree_reads_arrive() {
        let mut ui = tree_sample();
        assert_eq!(ui.shown["m"], (Urgency::Idle, 0));
        ui.apply(Update::Status {
            id: "w".into(),
            activity: Some(Activity::Waiting { detail: None }),
            last_activity: 0,
            unanchored: false,
        });
        assert_eq!(ui.shown["m"], (Urgency::NeedsYou, 1));
        assert_eq!(ui.shown["root"], (Urgency::NeedsYou, 1));

        let mut info = TreeInfo::default();
        info.nodes.insert(
            "m".into(),
            managers::NodeInfo {
                run: crate::db::tasks::RunState::Run,
                held: 0,
                setter: None,
                budget: None,
                inbox: None,
                pr: None,
                queued: false,
                ask: Some((1, crate::db::tasks::ReportKind::Question, false)),
                done: false,
                children: 1,
            },
        );
        ui.tree_seq = 3;
        // A gather asked for before the latest is dropped.
        ui.apply(Update::Tree {
            seq: 2,
            info: Ok(Box::new(info.clone())),
        });
        assert_eq!(ui.tree_info, None);
        ui.apply(Update::Tree {
            seq: 3,
            info: Ok(Box::new(info.clone())),
        });
        assert_eq!(ui.tree_info.as_ref(), Some(&info));
        // Its own ask, under the prompt below it.
        assert_eq!(ui.shown["m"], (Urgency::NeedsYou, 1));
        ui.apply(Update::Status {
            id: "w".into(),
            activity: Some(Activity::Idle { since: 0 }),
            last_activity: 0,
            unanchored: false,
        });
        assert_eq!(ui.shown["m"], (Urgency::Blocked, 0));
        assert_eq!(ui.shown["root"], (Urgency::Blocked, 1));
    }

    #[test]
    fn a_failed_tree_read_keeps_the_last_and_is_said_once() {
        let mut ui = tree_sample();
        let info = TreeInfo {
            queued: 2,
            ..TreeInfo::default()
        };
        ui.apply(Update::Tree {
            seq: 0,
            info: Ok(Box::new(info.clone())),
        });
        let failed = || Update::Tree {
            seq: 0,
            info: Err("database is locked".into()),
        };
        ui.apply(failed());
        assert_eq!(
            ui.status_line,
            "could not read the tree: database is locked"
        );
        ui.set_status("something else");
        ui.apply(failed());
        assert_eq!(ui.status_line, "something else");
        assert_eq!(ui.tree_info.as_ref(), Some(&info));
        // Said again after a read that worked.
        ui.apply(Update::Tree {
            seq: 0,
            info: Ok(Box::new(info)),
        });
        ui.apply(failed());
        assert_eq!(
            ui.status_line,
            "could not read the tree: database is locked"
        );
    }

    /// [`tree_sample`] in the agents view: the assistant, `m`, `w`.
    fn agents_sample() -> TasksUi {
        let mut ui = tree_sample();
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.view, View::Agents);
        ui
    }

    #[test]
    fn the_agents_view_shows_the_tree_and_counts_its_nodes() {
        let ui = agents_sample();
        assert_eq!(ui.count(View::Agents), 3);
        assert_eq!(ui.shown_len(), 3);
        assert!(ui.visible.is_empty());
        assert_eq!(ui.selected_id(), Some("root"));
        // The other views never list the assistant.
        assert_eq!(ui.count(View::Active), 5);
    }

    #[test]
    fn arrows_open_and_close_managers_and_walk_the_tree() {
        let mut ui = agents_sample();
        // On the assistant, → goes to the first node under it.
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.selected_id(), Some("m"));
        // ← closes an open manager, keeping the cursor on it.
        press(&mut ui, KeyCode::Left);
        assert!(ui.collapsed.contains("m"));
        assert_eq!(ui.shown_len(), 2);
        assert_eq!(ui.selected_id(), Some("m"));
        // ← on a closed one goes to its parent.
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.selected_id(), Some("root"));
        press(&mut ui, KeyCode::Down);
        // → opens a closed manager, then goes into it.
        press(&mut ui, KeyCode::Right);
        assert!(!ui.collapsed.contains("m"));
        assert_eq!(ui.shown_len(), 3);
        assert_eq!(ui.selected_id(), Some("m"));
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.selected_id(), Some("w"));
        // A worker has nothing to open; ← goes up.
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.selected_id(), Some("w"));
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.selected_id(), Some("m"));
    }

    #[test]
    fn a_filter_ignores_collapse_and_the_arrows_leave_it_alone() {
        let mut ui = agents_sample();
        ui.filter = "w".to_string();
        ui.refresh_visible(None);
        press(&mut ui, KeyCode::Down);
        assert_eq!(ui.selected_id(), Some("m"));
        assert!(ui.tree[ui.selected].open);
        // ← goes to the parent, and nothing is collapsed behind the filter.
        press(&mut ui, KeyCode::Left);
        assert_eq!(ui.selected_id(), Some("root"));
        assert!(ui.collapsed.is_empty());
        // A flag set before the filter does not show, and → does not clear it.
        ui.filter.clear();
        ui.collapsed.insert("m".to_string());
        ui.filter = "w".to_string();
        ui.refresh_visible(None);
        press(&mut ui, KeyCode::Down);
        assert!(ui.tree[ui.selected].open);
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.selected_id(), Some("w"));
        assert!(ui.collapsed.contains("m"));
    }

    #[test]
    fn enter_attaches_to_a_node_and_picks_an_assistant_session_at_the_root() {
        let mut ui = agents_sample();
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::AssistantSessions)
        );
        press(&mut ui, KeyCode::Down);
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Open {
                task_id: "m".into(),
                session: None
            })
        );
        // No assistant, no tree: nothing to attach to.
        let mut bare = sample();
        press(&mut bare, KeyCode::Tab);
        assert_eq!(press(&mut bare, KeyCode::Enter), None);
        assert_eq!(bare.status_line, NO_NODE);
    }

    #[test]
    fn the_agents_view_keeps_its_own_keys() {
        let mut ui = agents_sample();
        press(&mut ui, KeyCode::Down);
        for c in ['e', 'w', 'v', 'o', 'D', 'S'] {
            ui.set_status("");
            assert_eq!(press(&mut ui, KeyCode::Char(c)), None, "{c}");
            assert_eq!(ui.status_line, NOT_IN_AGENTS, "{c}");
        }
        assert!(!ui.by_priority);
        // Not archive: answering, which is the user's.
        assert_eq!(press(&mut ui, KeyCode::Char('a')), None);
        assert_eq!(ui.status_line, NOT_USER);
        // The assistant's keys are not a node's.
        assert_eq!(
            press(&mut ui, KeyCode::Char('c')),
            Some(Action::Chat(ChatSession::Latest))
        );
        press(&mut ui, KeyCode::Char('t'));
        assert!(matches!(ui.panel, Some(Panel::Todo { .. })));
        press(&mut ui, KeyCode::Esc);
        press(&mut ui, KeyCode::Char('?'));
        assert!(matches!(ui.panel, Some(Panel::Help { .. })));
    }

    #[test]
    fn reading_a_node_is_for_the_user_alone() {
        let mut ui = agents_sample();
        press(&mut ui, KeyCode::Down);
        // Inside an agent's pane, or not known to be the user.
        for c in ['l', 'i', 'b'] {
            ui.set_status("");
            assert_eq!(press(&mut ui, KeyCode::Char(c)), None, "{c}");
            assert_eq!(ui.status_line, NOT_USER, "{c}");
        }
        ui.user = true;
        let m = || "m".to_string();
        assert_eq!(
            press(&mut ui, KeyCode::Char('l')),
            Some(Action::Worklog { task_id: m() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('i')),
            Some(Action::Reports { task_id: m() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('b')),
            Some(Action::Brief { task_id: m() })
        );
        // The assistant is read like any node.
        press(&mut ui, KeyCode::Up);
        assert_eq!(
            press(&mut ui, KeyCode::Char('b')),
            Some(Action::Brief {
                task_id: "root".into()
            })
        );
        // `i` is still the assistant's inbox in the active view.
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(press(&mut ui, KeyCode::Char('i')), None);
        assert!(matches!(ui.panel, Some(Panel::Inbox { .. })));
    }

    #[test]
    fn pausing_resuming_and_stopping_are_for_the_user_alone() {
        let mut ui = agents_sample();
        press(&mut ui, KeyCode::Down);
        for c in ['p', 'P', 'x'] {
            ui.set_status("");
            assert_eq!(press(&mut ui, KeyCode::Char(c)), None, "{c}");
            assert_eq!(ui.status_line, NOT_USER, "{c}");
        }
        ui.user = true;
        let m = || "m".to_string();
        assert_eq!(
            press(&mut ui, KeyCode::Char('p')),
            Some(Action::Pause { task_id: m() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('P')),
            Some(Action::Resume { task_id: m() })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('x')),
            Some(Action::AskStop { task_id: Some(m()) })
        );
        // On the assistant, `x` is `stop --all`; a pause goes to `in_scope`,
        // which refuses it as the CLI does.
        press(&mut ui, KeyCode::Up);
        assert_eq!(
            press(&mut ui, KeyCode::Char('x')),
            Some(Action::AskStop { task_id: None })
        );
        assert_eq!(
            press(&mut ui, KeyCode::Char('p')),
            Some(Action::Pause {
                task_id: "root".into()
            })
        );
        // `p` and `P` are still PRs and projects in the active view.
        press(&mut ui, KeyCode::BackTab);
        assert!(matches!(
            press(&mut ui, KeyCode::Char('p')),
            Some(Action::Prs { .. })
        ));
        assert_eq!(press(&mut ui, KeyCode::Char('P')), Some(Action::Projects));
        // No tree, no node.
        let mut bare = sample();
        bare.user = true;
        press(&mut bare, KeyCode::Tab);
        assert_eq!(press(&mut bare, KeyCode::Char('x')), None);
        assert_eq!(bare.status_line, NO_NODE);
    }

    #[test]
    fn n_opens_a_form_under_the_assistant_or_a_manager_only() {
        let mut ui = agents_sample();
        assert_eq!(press(&mut ui, KeyCode::Char('n')), None);
        assert_eq!(ui.status_line, NOT_USER);
        ui.user = true;
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewChildForm {
                parent_id: "root".into()
            })
        );
        press(&mut ui, KeyCode::Down);
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewChildForm {
                parent_id: "m".into()
            })
        );
        // A worker is refused at the key, in `plan_child`'s words.
        press(&mut ui, KeyCode::Down);
        assert_eq!(press(&mut ui, KeyCode::Char('n')), None);
        assert_eq!(
            ui.status_line,
            "task w is not a manager: only the assistant and managers have tasks under them"
        );
        // `n` is still a top-level task in the active view.
        press(&mut ui, KeyCode::BackTab);
        assert_eq!(
            press(&mut ui, KeyCode::Char('n')),
            Some(Action::NewTaskForm { project: None })
        );
    }

    fn child_form() -> NewTaskForm {
        form().under("m".into(), "manager m".into())
    }

    #[test]
    fn a_form_under_a_node_shows_its_fields_by_kind() {
        use Field::*;
        let mut f = child_form();
        assert_eq!(f.field, Kind);
        assert_eq!(
            f.fields(),
            [Kind, Name, Goal, Done, Budget, Allow, Project, Repos, Branch]
        );
        f.toggle_kind();
        assert_eq!(f.kind, TaskKind::Manager);
        // `manager new` takes a project but no worktrees.
        assert_eq!(f.fields(), [Kind, Name, Goal, Done, Budget, Allow, Project]);
        f.field = Project;
        assert_eq!(f.step(1), Kind);
        assert_eq!(form().fields(), [Name, Project, Repos, Branch]);
    }

    #[test]
    fn the_form_under_a_node_is_filled_in_from_the_keyboard() {
        let mut ui = agents_sample();
        ui.panel = Some(Panel::NewTask(Box::new(child_form())));
        press(&mut ui, KeyCode::Char(' ')); // a manager
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "sub");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "ship v2");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "it is out");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "2h, 40turns");
        press(&mut ui, KeyCode::Tab);
        type_text(&mut ui, "task.create,task.open");
        // A manager's fields wrap round after the project: no repos or branch.
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Tab);
        match &ui.panel {
            Some(Panel::NewTask(f)) => assert_eq!(f.field, Field::Kind),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::CreateChild(ChildSpec {
                parent: "m".into(),
                kind: TaskKind::Manager,
                name: "sub".into(),
                goal: Some("ship v2".into()),
                done: Some("it is out".into()),
                budget: vec![BudgetPart::Secs(7200), BudgetPart::Turns(40)],
                allow: Some(vec!["task.create".into(), "task.open".into()]),
                project: None,
                repos: Vec::new(),
                branch: None,
            }))
        );
        // Left up while it is made, so a refusal can come back to it.
        assert!(matches!(ui.panel, Some(Panel::NewTask(_))));
    }

    #[test]
    fn a_bad_budget_or_capability_stays_on_the_form() {
        for (field, text, why) in [
            (Field::Budget, "2x", "budget: 2x: not a budget"),
            (
                Field::Allow,
                "task.create,fly",
                "allow: \"fly\" is not a capability",
            ),
        ] {
            let mut ui = agents_sample();
            let mut f = child_form();
            f.field = field;
            ui.panel = Some(Panel::NewTask(Box::new(f)));
            type_text(&mut ui, text);
            assert_eq!(press(&mut ui, KeyCode::Enter), None, "{text}");
            match &ui.panel {
                Some(Panel::NewTask(f)) => {
                    let error = f.error.as_deref().unwrap_or_default();
                    assert!(error.starts_with(why), "{error}");
                }
                other => panic!("{other:?}"),
            }
        }
        // Blank ones are the defaults; a worker keeps its worktrees.
        let mut f = child_form();
        f.repos.toggle();
        let spec = f.child_spec().unwrap();
        assert_eq!((&spec.goal, &spec.allow), (&None, &None));
        assert!(spec.budget.is_empty());
        assert_eq!(spec.repos, ["github.com/acme/api"]);
        assert_eq!(spec.kind, TaskKind::Work);
        assert_eq!(spec.delegation().parent.as_deref(), Some("m"));
    }

    #[test]
    fn a_manager_ignores_the_fields_it_hides() {
        let mut f = child_form();
        f.branch = "feat".into();
        f.toggle_kind();
        let spec = f.child_spec().unwrap();
        assert_eq!(spec.kind, TaskKind::Manager);
        assert!(spec.repos.is_empty());
    }

    #[test]
    fn a_stop_is_done_only_once_confirmed() {
        let mut ui = agents_sample();
        let confirm = |task_id: Option<&str>| {
            Some(Panel::Confirm {
                title: "stop".into(),
                lines: Vec::new(),
                action: Pending::Stop {
                    task_id: task_id.map(str::to_string),
                },
            })
        };
        ui.panel = confirm(Some("m"));
        assert_eq!(press(&mut ui, KeyCode::Char('f')), None);
        assert!(ui.panel.is_some(), "no force: a stray key keeps asking");
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Stop {
                task_id: Some("m".into())
            })
        );
        ui.panel = confirm(None);
        assert_eq!(
            press(&mut ui, KeyCode::Char('y')),
            Some(Action::Stop { task_id: None })
        );
        ui.panel = confirm(Some("m"));
        assert_eq!(press(&mut ui, KeyCode::Char('n')), None);
        assert!(ui.panel.is_none());
        assert_eq!(ui.status_line, "nothing changed");
    }

    #[test]
    fn the_stop_warning_lists_what_runs_leaves_first() {
        let running = ["w".to_string(), "api".to_string()];
        let lines = stop_warning("manager api", &running, false);
        assert_eq!(
            lines,
            [
                "stopping manager api ends its agent and every agent below it, leaves first",
                "and starts none of them again until it is resumed",
                "",
                "running now:",
                "  w",
                "  api",
                "",
                "only you can resume it",
            ]
        );
        let all = stop_warning("every task under the assistant", &[], true);
        assert_eq!(
            all[0],
            "stopping every task under the assistant ends each one's agent, leaves first"
        );
        assert!(all.contains(&"none of them is running now".to_string()));
        assert_eq!(all.last().unwrap(), "only you can resume them");
    }

    #[test]
    fn the_cursor_stays_on_its_node_as_the_tree_changes() {
        let mut ui = agents_sample();
        press(&mut ui, KeyCode::Char('G'));
        assert_eq!(ui.selected_id(), Some("w"));
        // A reread with a new node above it.
        let mut rows = ui.rows.clone();
        let mut k = task("k", "k", TaskState::Active, 0);
        k.task.parent_id = Some("root".into());
        rows.push(k);
        ui.set_rows(rows);
        assert_eq!(ui.shown_len(), 4);
        assert_eq!(ui.selected_id(), Some("w"));
        // A click picks the line under it: the border, then the root.
        ui.list_area = Rect::new(0, 4, 80, 10);
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        ui.handle_mouse(click(6));
        assert_eq!(ui.selected_id(), Some("k"));
        ui.handle_mouse(click(12));
        assert_eq!(ui.selected_id(), Some("k"), "below the last line");
        // Status updates resort the rows the tree indexes.
        ui.apply(Update::Status {
            id: "w".into(),
            activity: Some(Activity::Running { detail: None }),
            last_activity: 999,
            unanchored: false,
        });
        assert_eq!(ui.selected_id(), Some("k"));
    }

    #[test]
    fn a_text_panel_scrolls_and_opens_its_prs() {
        let mut ui = agents_sample();
        let text = |prs: Option<&str>| Panel::Text {
            title: "t".into(),
            lines: vec!["a".into(); 50],
            scroll: 0,
            prs: prs.map(str::to_string),
        };
        let scroll = |ui: &TasksUi| match &ui.panel {
            Some(Panel::Text { scroll, .. }) => *scroll,
            other => panic!("{other:?}"),
        };
        ui.panel = Some(text(None));
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::PageDown);
        assert_eq!(scroll(&ui), 11);
        press(&mut ui, KeyCode::Char('k'));
        assert_eq!(scroll(&ui), 10);
        press(&mut ui, KeyCode::Char('G'));
        assert_eq!(scroll(&ui), u16::MAX, "the draw brings it back to the end");
        press(&mut ui, KeyCode::Char('g'));
        assert_eq!(scroll(&ui), 0);
        // ⏎ closes it, with no PRs to open.
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        assert!(ui.panel.is_none());
        ui.panel = Some(text(Some("m")));
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Prs {
                task_id: "m".into()
            })
        );
        ui.panel = Some(text(None));
        press(&mut ui, KeyCode::Esc);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn an_inbox_a_hook_held_keeps_its_last_count() {
        let mut ui = tree_sample();
        let with_inbox = |inbox| {
            let mut info = TreeInfo::default();
            info.nodes.insert(
                "m".into(),
                managers::NodeInfo {
                    run: crate::db::tasks::RunState::Run,
                    held: 0,
                    setter: None,
                    budget: None,
                    inbox,
                    pr: None,
                    queued: false,
                    ask: None,
                    done: false,
                    children: 1,
                },
            );
            Update::Tree {
                seq: 0,
                info: Ok(Box::new(info)),
            }
        };
        ui.apply(with_inbox(Some(3)));
        ui.apply(with_inbox(None));
        assert_eq!(ui.tree_info.as_ref().unwrap().nodes["m"].inbox, Some(3));
        ui.apply(with_inbox(Some(0)));
        assert_eq!(ui.tree_info.as_ref().unwrap().nodes["m"].inbox, Some(0));
    }

    fn app_at(home: &Path) -> App {
        App {
            config: config::load_or_create(home).unwrap(),
            db: Database::open(&config::index_path(home)).unwrap(),
            home: home.to_path_buf(),
        }
    }

    #[test]
    fn only_a_known_user_at_their_own_terminal_is_the_user() {
        assert!(is_user(Ok(Caller::User), Ok(true)));
        // A top-level task's agent.
        assert!(!is_user(Ok(Caller::User), Ok(false)));
        assert!(!is_user(Ok(Caller::User), Err(anyhow::anyhow!("who?"))));
        assert!(!is_user(Ok(Caller::Root), Ok(true)));
        assert!(!is_user(Err(anyhow::anyhow!("who?")), Ok(true)));
    }

    /// [`agents_sample`] with w asking report 42, with the user or not.
    fn asking_sample(user: bool) -> TasksUi {
        let mut ui = agents_sample();
        let mut info = TreeInfo::default();
        info.nodes.insert(
            "w".into(),
            managers::NodeInfo {
                run: crate::db::tasks::RunState::Run,
                held: 0,
                setter: None,
                budget: None,
                inbox: Some(0),
                pr: None,
                queued: false,
                ask: Some((42, crate::db::tasks::ReportKind::Question, user)),
                done: false,
                children: 0,
            },
        );
        ui.apply(Update::Tree {
            seq: 0,
            info: Ok(Box::new(info)),
        });
        ui
    }

    #[test]
    fn a_answers_the_open_question_at_or_below_the_node() {
        let mut ui = asking_sample(true);
        // Not the user at their own terminal.
        assert_eq!(press(&mut ui, KeyCode::Char('a')), None);
        assert_eq!(ui.status_line, NOT_USER);
        ui.user = true;
        let ask_w = Some(Action::AskAnswer {
            task_id: "w".into(),
        });
        // On the assistant, on the manager and on the asker.
        assert_eq!(press(&mut ui, KeyCode::Char('a')), ask_w);
        press(&mut ui, KeyCode::Down);
        assert_eq!(ui.selected_id(), Some("m"));
        assert_eq!(press(&mut ui, KeyCode::Char('a')), ask_w);
        press(&mut ui, KeyCode::Down);
        assert_eq!(press(&mut ui, KeyCode::Char('a')), ask_w);

        // Nothing open: said, and no action.
        let mut calm = agents_sample();
        calm.user = true;
        press(&mut calm, KeyCode::Down);
        assert_eq!(press(&mut calm, KeyCode::Char('a')), None);
        assert_eq!(calm.status_line, "no open question at or below m");

        // `a` is still archive in the active view.
        press(&mut ui, KeyCode::BackTab);
        assert!(matches!(
            press(&mut ui, KeyCode::Char('a')),
            Some(Action::Archive {
                confirmed: false,
                ..
            })
        ));
    }

    fn answer_panel() -> Panel {
        Panel::Answer {
            id: 42,
            title: "#42 question from task assistant/m/w".into(),
            question: vec!["keep v1?".into()],
            input: String::new(),
            error: None,
        }
    }

    #[test]
    fn the_answer_form_takes_an_answer_and_esc_closes_it() {
        let mut ui = asking_sample(true);
        ui.user = true;
        ui.panel = Some(answer_panel());
        // An empty answer is not sent.
        assert_eq!(press(&mut ui, KeyCode::Enter), None);
        match &ui.panel {
            Some(Panel::Answer { error, .. }) => {
                assert_eq!(error.as_deref(), Some(TYPE_AN_ANSWER))
            }
            other => panic!("{other:?}"),
        }
        // Ctrl-U clears the line, and is not typed.
        type_text(&mut ui, "junk");
        ui.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        match &ui.panel {
            Some(Panel::Answer { input, .. }) => assert_eq!(input.as_str(), ""),
            other => panic!("{other:?}"),
        }
        type_text(&mut ui, "drop itx");
        press(&mut ui, KeyCode::Backspace);
        assert_eq!(
            press(&mut ui, KeyCode::Enter),
            Some(Action::Answer {
                id: 42,
                text: "drop it".into()
            })
        );
        // Up until the answer is in, the error gone.
        match &ui.panel {
            Some(Panel::Answer { input, error, .. }) => {
                assert_eq!((input.as_str(), error), ("drop it", &None))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(press(&mut ui, KeyCode::Esc), None);
        assert!(ui.panel.is_none());
    }

    #[test]
    fn a_worklog_that_is_not_a_file_is_refused_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(worklog_lines(&fifo).is_err());
        let real = dir.path().join("real");
        std::fs::write(&real, "x\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(worklog_lines(&link).is_err());
    }

    #[test]
    fn the_worklog_is_read_from_its_end_from_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worklog.md");
        assert_eq!(worklog_lines(&path).unwrap(), None);
        std::fs::write(&path, "# log\n\x1b[2Jone\ttwo\x07\n").unwrap();
        assert_eq!(
            worklog_lines(&path).unwrap().unwrap(),
            ["# log", "[2Jone\ttwo"]
        );
        // Longer than is read: the cut falls inside "é" and inside a line.
        let mut long = "é".repeat(WORKLOG_TAIL as usize);
        long.push_str("\nlast\n");
        std::fs::write(&path, long).unwrap();
        let lines = worklog_lines(&path).unwrap().unwrap();
        assert_eq!(lines, ["last"]);
    }

    #[test]
    fn i_and_b_read_a_node_and_the_assistant_unfenced() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(&dir.path().canonicalize().unwrap());
        let record = |id: &str, kind, parent: Option<&str>| {
            let mut row = task(id, id, TaskState::Active, 0);
            row.task.kind = kind;
            row.task.parent_id = parent.map(str::to_string);
            app.db.insert_task(&row.task).unwrap();
            row.task
        };
        let root = record("root", TaskKind::Assistant, None);
        let api = record("api", TaskKind::Manager, Some("root"));
        let w = record("w", TaskKind::Work, Some("api"));
        app.db
            .insert_report(
                "w",
                "api",
                crate::db::tasks::ReportKind::Progress,
                "half",
                1,
            )
            .unwrap();
        let asked = app
            .db
            .insert_report(
                "w",
                "api",
                crate::db::tasks::ReportKind::Question,
                "keep v1?\n\x1b[2Jsay",
                2,
            )
            .unwrap();
        let w_dir = tasks::task_dir(&app, &w);
        crate::messages::Inbox::lock(&w_dir)
            .unwrap()
            .push("from api")
            .unwrap();

        let (lines, prs) = report_lines(&app, &w, &w_dir).unwrap();
        let text = lines.join("\n");
        assert!(!prs);
        assert!(
            text.starts_with(format!("open ask\n  #{asked} question to manager api").as_str()),
            "{text}"
        );
        assert!(text.contains("    keep v1?\n    [2Jsay"), "{text}");
        assert!(text.contains("inbox · 1 queued"), "{text}");
        assert!(text.contains("    from api"), "{text}");
        // Newest first, and no fence: the TUI is the user.
        let newest = text.find(format!("#{asked} question · task w → manager api").as_str());
        let older = text.find("progress · task w → manager api");
        assert!(
            newest.is_some() && older.is_some() && newest < older,
            "{text}"
        );
        assert!(!text.contains("jeet-report"), "{text}");

        // Its manager hears of them too, and has no ask of its own.
        let (lines, _) = report_lines(&app, &api, &tasks::task_dir(&app, &api)).unwrap();
        let text = lines.join("\n");
        assert!(text.starts_with("open ask\n  none"), "{text}");
        assert!(text.contains("question · task w → manager api"), "{text}");

        let text = brief_lines(&app, &w).unwrap().join("\n");
        assert!(text.contains("task root/api/w"), "{text}");
        assert!(
            text.contains(format!("its own open ask: #{asked} question").as_str()),
            "{text}"
        );
        // The CLI's `manager show` refuses the assistant; `b` reads it.
        let text = brief_lines(&app, &root).unwrap().join("\n");
        assert!(text.contains("- api (manager, active)"), "{text}");

        // Where an ask stands: with the user, then answered, with the
        // answer, which is kept whether or not it reached the asker.
        app.db.escalate_report(asked, "api", "root", false).unwrap();
        app.db.escalate_report(asked, "root", "root", true).unwrap();
        let (lines, _) = report_lines(&app, &w, &w_dir).unwrap();
        let text = lines.join("\n");
        assert!(
            text.contains(&format!("#{asked} question to the assistant · ")),
            "{text}"
        );
        assert!(text.contains(" ago · with the user\n"), "{text}");
        app.db
            .answer_report(asked, None, "drop it\x1b[2J", "user")
            .unwrap();
        let (lines, _) = report_lines(&app, &w, &w_dir).unwrap();
        let text = lines.join("\n");
        assert!(text.starts_with("open ask\n  none"), "{text}");
        assert!(
            text.contains(&format!("#{asked} question · task w → the assistant · ")),
            "{text}"
        );
        assert!(text.contains(" ago · answered by the user\n"), "{text}");
        assert!(text.contains("    answer:\n      drop it[2J"), "{text}");
        app.db
            .insert_report(
                "w",
                "api",
                crate::db::tasks::ReportKind::Question,
                "next?",
                3,
            )
            .unwrap();
        let next = app.db.last_own_report("w").unwrap().unwrap().id;
        app.db.answer_report(next, Some("api"), "x", "api").unwrap();
        let (lines, _) = report_lines(&app, &w, &w_dir).unwrap();
        assert!(
            lines.join("\n").contains("answered by manager api"),
            "{lines:?}"
        );
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
        let bg = Background::start(Path::new("/nonexistent/jeet-home"), &Config::default());
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
    fn going_to_a_task_finds_it_in_the_archived_list_and_past_the_filter() {
        let mut ui = with_todo("- [ ] look again @old-spike\n");
        ui.filter = "api".into();
        ui.refresh_visible(None);
        press(&mut ui, KeyCode::Char('t'));
        press(&mut ui, KeyCode::Enter);
        assert_eq!((ui.view, ui.archived_tasks), (View::Active, true));
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
        let bg = Background::start(Path::new("/nonexistent/jeet-home"), &Config::default());
        bg.look_at_assistant(assistant_info::Probe {
            home: home.path().to_path_buf(),
            dir,
            records: Vec::new(),
            primary: None,
            task_id: None,
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
                read_only: false,
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
