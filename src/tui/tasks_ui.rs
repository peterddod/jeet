//! Rendering for the tasks list.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::assistant_info::{FeedEntry, FeedKind, Info as AssistantInfo};
use super::chat;
use super::managers::{At, NodeInfo, TreeLine, Urgency};
use super::reviews::{PrReview, ReviewLine, Source, Verdict};
use super::state::human_size;
use super::tasks::{
    review_name, AddField, Field, NewProjectForm, NewTaskForm, Panel, Picker, ProjectField,
    TaskRow, TasksUi, View, REVIEW_MENU,
};
use super::term::Link;
use super::ui::{centered_rect, content_rect, draw_working, fitted_rect, truncate};
use crate::agent::age_label;
use crate::commands::task::printable;
use crate::db::tasks::{RunState, Setter, TaskKind, TaskPrRecord, TaskState};
use crate::manager::budget;
use crate::messages;
use crate::pr_history;
use crate::status::{first_line, Activity};
use crate::tasks;
use crate::todo::ItemView;
use crate::worktrees::WorktreeStatus;

/// The key list, as `?` shows it.
pub const HELP: &[(&str, &str)] = &[
    (
        "⏎",
        "open the task's agent in tmux (Ctrl-Q comes back here, leaving it running)",
    ),
    ("e", "explore the task: its canvas or one of its worktrees"),
    ("w", "worktrees: n adds one, D removes one, ⏎ explores it"),
    (
        "p",
        "the task's pull requests; ⏎ or o opens one in the browser, v in the review viewer",
    ),
    (
        "v",
        "the task's pull request in the review viewer (a review's own, else its open one)",
    ),
    (
        "n",
        "new task: blank for an empty canvas, or a project/repos",
    ),
    (
        "a",
        "archive (frees the disk) · unarchive, in the archived view; the tasks under it too",
    ),
    (
        "D",
        "delete the task, and the tasks under it, after listing any work that would go",
    ),
    ("S", "copy the task's project files in again"),
    ("P", "projects: n new, e edit, D delete, ⏎ start a task"),
    ("", ""),
    (
        "c",
        "talk to the assistant: its latest session (Ctrl-Q comes back)",
    ),
    (
        "C",
        "the assistant's sessions: ⏎ attach, n new, R rename, x end, * primary",
    ),
    ("t", "its todo list: space done, ⏎ go to the task, a add"),
    (
        "i",
        "its inbox: GitHub events, messages, what it did; ⏎ opens a link",
    ),
    (
        "o",
        "sort tasks by todo priority, or back by latest activity",
    ),
    ("", ""),
    (
        "⇥ / ⇧⇥",
        "next / previous view: active, agents (the tree under the assistant), reviews, archived",
    ),
    ("/", "filter by name; ⏎ keeps the filter, esc drops it"),
    ("↑ ↓ j k", "move · PgUp/PgDn ten rows · g/G top and bottom"),
    ("r", "refresh: re-read tasks and the worktrees on disk"),
    ("?", "this list"),
    ("q / esc", "quit, leaving the shell where it is"),
    ("ctrl-q", "quit, leaving the shell in the selected task"),
    ("", ""),
    ("status", "● running  ◆ needs input  ○ idle  ✕ not running"),
    ("", "· no agent, or it ended  ▪ archived"),
    (
        "columns",
        "status · task · worktrees · size · last activity",
    ),
];

/// The agents view's key list (Q21, Q22).
pub const HELP_AGENTS: &[(&str, &str)] = &[
    (
        "⏎",
        "attach to the node's agent; on the assistant, pick one of its sessions",
    ),
    (
        "→ / ←",
        "open / close a manager; else to the first task under it / to its parent",
    ),
    (
        "l",
        "the worklog (notes/worklog.md) of the assistant or a manager",
    ),
    (
        "i",
        "its open ask, inbox and reports, newest first; ⏎ its pull requests",
    ),
    (
        "b",
        "its brief, grant, what it may do, budget, run state and tasks",
    ),
    (
        "p / P",
        "pause the node and everything below it / let it run again",
    ),
    (
        "x",
        "stop it and everything below it, after a confirm; on the assistant, all",
    ),
    (
        "n",
        "a task or manager under the assistant or a manager; a manager starts at once",
    ),
    (
        "a",
        "answer the open question here or nearest below; one with you first",
    ),
    (
        "",
        "l, i, b, p, P, x, n and a only from your own terminal, not a pane of jeet's",
    ),
    ("", ""),
    (
        "c",
        "talk to the assistant: its latest session (Ctrl-Q comes back)",
    ),
    (
        "C",
        "the assistant's sessions: ⏎ attach, n new, R rename, x end, * primary",
    ),
    ("t", "its todo list: space done, ⏎ go to the task, a add"),
    ("", ""),
    (
        "⇥ / ⇧⇥",
        "next / previous view: active, agents, reviews, archived",
    ),
    (
        "/",
        "filter by name, keeping the managers above a match; esc drops it",
    ),
    ("↑ ↓ j k", "move · PgUp/PgDn ten rows · g/G top and bottom"),
    ("r", "refresh: re-read tasks and the worktrees on disk"),
    ("?", "this list"),
    ("q / esc", "quit, leaving the shell where it is"),
    (
        "ctrl-q",
        "quit, leaving the shell in the selected node's directory",
    ),
    ("", ""),
    (
        "status",
        "◆ needs you / needs input  ! blocked or a question  $ budget",
    ),
    ("", "● running  ○ idle  ✓ done  ⏸ paused  ■ stopped"),
    ("", "(n): how many below it are so; held: how many it holds"),
    (
        "columns",
        "node · status · budget used/limit · inbox, todo, PR, budget %, queued",
    ),
    ("", ""),
    (
        "",
        "worktree, PR, archive and delete keys are the active view's: ⇥ to it",
    ),
];

/// The reviews view's key list.
pub const HELP_REVIEWS: &[(&str, &str)] = &[
    (
        "⏎",
        "the review's menu: v view the diff, s open its session",
    ),
    (
        "v",
        "the review's PR in the review viewer, showing its diff",
    ),
    (
        "",
        "the session is the review task's agent: attached, resumed or started",
    ),
    (
        "e / w / p",
        "explore the review's task / its worktrees / its pull requests",
    ),
    (
        "a",
        "archive the review's task · unarchive, on an archived one",
    ),
    (
        "D",
        "delete the review's task, after listing any work that would go",
    ),
    ("", ""),
    (
        "⇥ / ⇧⇥",
        "next / previous view: active, agents, reviews, archived",
    ),
    ("/", "filter by PR, task name or title; esc drops it"),
    ("↑ ↓ j k", "move · PgUp/PgDn ten rows · g/G top and bottom"),
    (
        "r",
        "refresh: re-read tasks and ask GitHub again (it is asked every 2 minutes)",
    ),
    ("?", "this list"),
    ("q / esc", "quit, leaving the shell where it is"),
    (
        "ctrl-q",
        "quit, leaving the shell in the selected review's task",
    ),
    ("", ""),
    (
        "session",
        "● running  ◆ needs input  ○ idle  ✕ not running  · ended  ▪ archived",
    ),
    (
        "",
        "a review with no task is an older jeet review's job: no session",
    ),
    ("PR", "open · draft · merged · closed, as GitHub last said"),
    (
        "review",
        "approved · changes requested · commented · review required · awaiting review",
    ),
    (
        "columns",
        "session · PR · PR state · review state · title · last activity",
    ),
];

/// The key list of `view`.
pub fn help(view: View) -> &'static [(&'static str, &'static str)] {
    match view {
        View::Agents => HELP_AGENTS,
        View::Reviews => HELP_REVIEWS,
        View::Active | View::Archived => HELP,
    }
}

/// Width of the key column in the help panel.
const KEY_WIDTH: usize = 10;

const HINTS: &[&str] = &[
    "⏎ open",
    "e explore",
    "w worktrees",
    "n new",
    "c assistant",
    "t todo",
    "i inbox",
    "a archive",
    "D delete",
    "p PRs",
    "P projects",
    "S resync",
    "/ filter",
    "⇥ switch",
    "? help",
    "q quit",
];

const HINTS_AGENTS: &[&str] = &[
    "⏎ attach",
    "→← open/close",
    "l worklog",
    "i inbox",
    "b brief",
    "p/P pause/resume",
    "x stop",
    "n new under",
    "c assistant",
    "t todo",
    "/ filter",
    "⇥ switch",
    "? help",
    "q quit",
];

const HINTS_REVIEWS: &[&str] = &[
    "⏎ menu",
    "v diff",
    "e explore",
    "p PRs",
    "a archive",
    "r refresh",
    "/ filter",
    "⇥ switch",
    "? help",
    "q quit",
];

/// As many of `view`'s hints as fit, always ending with help and quit.
fn hints(width: u16, view: View) -> String {
    let hints = match view {
        View::Agents => HINTS_AGENTS,
        View::Reviews => HINTS_REVIEWS,
        View::Active | View::Archived => HINTS,
    };
    let width = width as usize;
    let tail = &hints[hints.len() - 2..];
    let head = &hints[..hints.len() - 2];
    (0..=head.len())
        .rev()
        .map(|keep| {
            head[..keep]
                .iter()
                .chain(tail)
                .copied()
                .collect::<Vec<_>>()
                .join("  ")
        })
        .find(|line| line.width() <= width)
        .unwrap_or_else(|| truncate("q quit", width))
}

pub fn draw(frame: &mut Frame, ui: &mut TasksUi) {
    // In the agents view the assistant is the tree's root, not pinned.
    let pinned = match ui.view {
        View::Agents | View::Reviews => 0,
        View::Active | View::Archived => 1,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(pinned),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], ui);
    if pinned > 0 {
        frame.render_widget(
            Paragraph::new(assistant_line(ui.assistant.as_ref())),
            chunks[1],
        );
    }
    ui.list_area = chunks[2];
    match ui.view {
        View::Agents => draw_tree(frame, chunks[2], ui),
        View::Reviews => draw_reviews(frame, chunks[2], ui),
        View::Active | View::Archived => draw_list(frame, chunks[2], ui),
    }

    // What was last done says more than a standing hint.
    let status = match ui.status_line.is_empty() {
        true => unanchored_hint(ui.selected_row()).unwrap_or_default(),
        false => ui.status_line.clone(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Yellow),
        ))),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints(chunks[4].width, ui.view),
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[4],
    );

    // Scrolled no further than its end, which only the draw knows.
    if let Some(Panel::Text { lines, scroll, .. }) = &mut ui.panel {
        let area = text_area(frame.area());
        let rows = text_lines(lines, area.width.saturating_sub(2)).len();
        let room = area.height.saturating_sub(2) as usize;
        *scroll = (*scroll).min(rows.saturating_sub(room).min(u16::MAX as usize) as u16);
    }
    ui.links = match &ui.panel {
        Some(panel) => draw_panel(frame, ui, panel),
        None => Vec::new(),
    };
    if let Some(working) = &ui.working {
        draw_working(frame, working);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, ui: &TasksUi) {
    let tab = |view: View, label: &str| {
        let text = format!(" {label} {} ", ui.count(view));
        if ui.view == view {
            Span::styled(
                text,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(text, Style::default().fg(Color::DarkGray))
        }
    };
    let mut spans = vec![
        tab(View::Active, "active"),
        Span::raw(" "),
        tab(View::Agents, "agents"),
        Span::raw(" "),
        tab(View::Reviews, "reviews"),
        Span::raw(" "),
        tab(View::Archived, "archived"),
        Span::raw("   "),
    ];
    if ui.view == View::Agents {
        spans.extend(agents_cells(ui));
    }
    match (ui.filtering, ui.filter.is_empty()) {
        (true, _) => {
            spans.push(Span::styled(
                "filter › ",
                Style::default().fg(Color::Magenta),
            ));
            spans.push(Span::styled(
                ui.filter.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
        }
        (false, false) => {
            spans.push(Span::styled(
                "filter ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                ui.filter.clone(),
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                "  (esc clears)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        (false, true) => spans.push(Span::styled(
            "/ filter  ⇥ switch",
            Style::default().fg(Color::DarkGray),
        )),
    }
    let title = match ui.view {
        View::Agents => " jeet · agents ",
        View::Reviews => " jeet · reviews ",
        View::Active | View::Archived => " jeet · tasks ",
    };
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        title,
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// The most running sessions the assistant's row names.
const SESSIONS_SHOWN: usize = 4;

/// The assistant, pinned above the list: its sessions and what each running
/// one is doing, what is queued for it, and its open todo items.
pub fn assistant_line(info: Option<&AssistantInfo>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " ✦ assistant  ",
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )];
    let Some(info) = info else {
        spans.push(dim("…"));
        return Line::from(spans);
    };
    match info.sessions.len() {
        0 => spans.push(dim("no sessions yet — c starts one")),
        n => {
            spans.push(Span::styled(
                format!("{n} session{}", if n == 1 { "" } else { "s" }),
                Style::default().fg(Color::Blue),
            ));
            let running: Vec<_> = info.sessions.iter().filter(|s| s.running()).collect();
            if running.is_empty() {
                spans.push(dim(" · none running"));
            }
            for session in running.iter().take(SESSIONS_SHOWN) {
                let (glyph, color) = activity_glyph(Some(&session.activity));
                let star = if session.primary { "*" } else { "" };
                spans.push(Span::styled(
                    format!(
                        "  {glyph} {star}{}",
                        truncate(&printable(&session.name), 18)
                    ),
                    Style::default().fg(color),
                ));
            }
            if running.len() > SESSIONS_SHOWN {
                spans.push(dim(format!("  +{}", running.len() - SESSIONS_SHOWN)));
            }
        }
    }
    if info.unanchored > 0 {
        spans.push(Span::styled(
            format!(
                " · {} unanchored: jeet task restart assistant",
                info.unanchored
            ),
            Style::default().fg(Color::Red),
        ));
    }
    let inbox = format!(" · inbox {}", info.inbox);
    spans.push(match info.inbox {
        0 => dim(inbox),
        _ => Span::styled(
            inbox,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    });
    match &info.todo_error {
        Some(_) => spans.push(Span::styled(
            " · todo.md unreadable",
            Style::default().fg(Color::Red),
        )),
        None => spans.push(dim(format!(" · todo {} open", info.open_todo()))),
    }
    spans.push(dim("   c chat  C sessions  t todo  i inbox"));
    Line::from(spans)
}

fn draw_list(frame: &mut Frame, area: Rect, ui: &mut TasksUi) {
    // The agents view draws its tree instead ([`draw_tree`]).
    let which = match ui.view {
        View::Active | View::Agents | View::Reviews => "active",
        View::Archived => "archived",
    };
    let total = ui.count(ui.view);
    let mut title = match (ui.filter.is_empty(), ui.visible.len()) {
        (true, 0) => format!(" no {which} tasks "),
        (true, n) => format!(" {n} {which} task{} ", if n == 1 { "" } else { "s" }),
        (false, 0) => format!(" nothing matches \"{}\" ", ui.filter),
        (false, n) => format!(" {n} of {total} match \"{}\" ", ui.filter),
    };
    if ui.by_priority {
        title.push_str("· by todo priority ");
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(Color::Green));

    if ui.visible.is_empty() {
        let hint = match (ui.view, total) {
            (View::Active, 0) => vec![
                "no tasks yet.".to_string(),
                String::new(),
                "n starts one: leave everything blank for an empty canvas the agent".to_string(),
                "adds worktrees to as it goes, or pick a project or repositories.".to_string(),
            ],
            (View::Archived, 0) => {
                vec!["nothing archived. a on an active task archives it.".into()]
            }
            _ => vec!["⌫ or esc widens the filter".into()],
        };
        let lines: Vec<Line> = hint
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::DarkGray))))
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let inner = area.width.saturating_sub(2) as usize;
    let columns = Columns::for_width(inner);
    // Every task but the assistant is a row: a parent not among them is it.
    let slugs: std::collections::HashMap<&str, &str> = ui
        .rows
        .iter()
        .map(|r| (r.task.id.as_str(), r.task.slug.as_str()))
        .collect();
    let items: Vec<ListItem> = ui
        .visible
        .iter()
        .map(|&i| {
            let row = &ui.rows[i];
            let parent = row
                .task
                .parent_id
                .as_deref()
                .map(|id| slugs.get(id).copied().unwrap_or("assistant"));
            ListItem::new(row_line(row, parent, &columns))
        })
        .collect();
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = std::mem::take(&mut ui.list);
    state.select(Some(ui.selected.min(ui.visible.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    ui.list = state;
}

/// The reviews view: one line per review, with its session and where its
/// PR and its review stand.
fn draw_reviews(frame: &mut Frame, area: Rect, ui: &mut TasksUi) {
    let total = ui.count(View::Reviews);
    let title = match (ui.filter.is_empty(), ui.reviews.len()) {
        (true, 0) => " no reviews ".to_string(),
        (true, n) => format!(" {n} review{} ", if n == 1 { "" } else { "s" }),
        (false, 0) => format!(" nothing matches \"{}\" ", ui.filter),
        (false, n) => format!(" {n} of {total} match \"{}\" ", ui.filter),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(Color::Green));
    if ui.reviews.is_empty() {
        let hint = match total {
            0 => "no reviews yet. jeet review <pr> checks one out in a task of its own.",
            _ => "⌫ or esc widens the filter",
        };
        frame.render_widget(
            Paragraph::new(Line::from(dim(hint)))
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let columns = ReviewColumns::for_width(area.width.saturating_sub(2) as usize);
    let items: Vec<ListItem> = ui
        .reviews
        .iter()
        .map(|line| ListItem::new(review_line(ui, line, &columns)))
        .collect();
    let list = List::new(items).block(block).highlight_style(highlighted());
    let mut state = std::mem::take(&mut ui.list);
    state.select(Some(ui.selected.min(ui.reviews.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    ui.list = state;
}

/// How wide each column of the reviews view is; zero drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewColumns {
    pub session: usize,
    pub pr: usize,
    pub state: usize,
    pub review: usize,
    pub title: usize,
    pub age: usize,
}

impl ReviewColumns {
    pub fn for_width(width: usize) -> ReviewColumns {
        const GLYPH: usize = 2;
        let (session, pr, state, review, age) = match width {
            w if w >= 100 => (18, 18, 7, 17, 9),
            w if w >= 80 => (12, 16, 7, 17, 9),
            w if w >= 60 => (0, 14, 7, 17, 0),
            _ => (0, 12, 7, 0, 0),
        };
        let gaps = [session, pr, state, review, age]
            .iter()
            .filter(|&&w| w > 0)
            .count();
        let fixed = GLYPH + session + pr + state + review + age + gaps;
        ReviewColumns {
            session,
            pr,
            state,
            review,
            title: width.saturating_sub(fixed),
            age,
        }
    }
}

/// The colour of a PR state's label: open green, draft grey, merged
/// purple, closed red, as GitHub has them.
fn state_color(label: &str) -> Color {
    match label {
        "open" => Color::Green,
        "draft" => Color::DarkGray,
        "merged" => Color::Magenta,
        _ => Color::Red,
    }
}

fn verdict_color(verdict: Verdict) -> Color {
    match verdict {
        Verdict::Approved => Color::Green,
        Verdict::ChangesRequested => Color::Red,
        Verdict::Commented => Color::Cyan,
        Verdict::ReviewRequired | Verdict::Awaiting => Color::Yellow,
    }
}

/// A review task's session: what its agent is doing, after what holds it
/// back — paused (at its `[review]` caps, or by hand), stopped, or queued
/// on a `max_live`.
pub fn review_session(row: &TaskRow) -> String {
    let held = match (row.task.run_state, &row.task.run_state_by) {
        _ if row.task.state == TaskState::Archived => None,
        (RunState::Paused, Some(Setter::Budget)) => Some("paused at its caps"),
        (RunState::Paused, _) => Some("paused"),
        (RunState::Stopped, _) => Some("stopped"),
        (RunState::Run, _) if row.queued => Some("queued on max_live"),
        (RunState::Run, _) => None,
    };
    let status = status_text(row);
    match held {
        // Paused, a turn under way still finishes: say so.
        Some(held) if row.agent_running() => format!("{held} · {status}"),
        Some(held) => held.to_string(),
        None => status,
    }
}

fn review_line(ui: &TasksUi, line: &ReviewLine, columns: &ReviewColumns) -> Line<'static> {
    let (glyph, color, session, name, when) = match line.source {
        Source::Task(i) => {
            let row = &ui.rows[i];
            let (glyph, color) = glyph(row);
            (
                glyph,
                color,
                review_session(row),
                row.task.name.clone(),
                row.last_activity,
            )
        }
        Source::Legacy(i) => {
            let job = &ui.legacy[i];
            let (glyph, color, session) = match job.running {
                true => ("●", Color::Green, "job running"),
                false => ("·", Color::DarkGray, "no task"),
            };
            (
                glyph,
                color,
                session.to_string(),
                job.title.clone(),
                job.started_at,
            )
        }
    };
    let found = line.pr.as_ref().and_then(|pr| ui.reviewed.get(pr));
    let pr = match &line.pr {
        Some(pr) => pr.label(),
        None => name.clone(),
    };
    let (state, state_fg, review, review_fg, title) = match found {
        None if line.pr.is_none() => (
            "",
            Color::DarkGray,
            "no PR recorded".into(),
            Color::DarkGray,
            name,
        ),
        None => ("…", Color::DarkGray, String::new(), Color::DarkGray, name),
        Some(Ok(PrReview {
            title,
            state,
            verdict,
        })) => (
            *state,
            state_color(state),
            verdict.label().to_string(),
            verdict_color(*verdict),
            title.clone(),
        ),
        Some(Err(why)) => ("?", Color::Red, format!("unknown: {why}"), Color::Red, name),
    };
    // Titles are whoever opened the PR's, and errors are gh's: only their
    // printable text reaches the terminal.
    let (pr, review, title) = (printable(&pr), printable(&review), printable(&title));
    let mut spans = vec![Span::styled(
        format!("{glyph} "),
        Style::default().fg(color),
    )];
    if columns.session > 0 {
        spans.push(Span::styled(
            format!("{} ", cell(&session, columns.session)),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled(
        format!("{} ", cell(&pr, columns.pr)),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("{} ", cell(state, columns.state)),
        Style::default().fg(state_fg),
    ));
    if columns.review > 0 {
        spans.push(Span::styled(
            format!("{} ", cell(&review, columns.review)),
            Style::default().fg(review_fg),
        ));
    }
    spans.push(Span::raw(cell(&title, columns.title)));
    if columns.age > 0 {
        spans.push(Span::styled(
            format!(" {:>width$}", age_label(when), width = columns.age),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// How wide each column is at a given width; zero drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    pub status: usize,
    pub name: usize,
    pub worktrees: usize,
    pub size: usize,
    pub age: usize,
}

impl Columns {
    pub fn for_width(width: usize) -> Columns {
        // The glyph and the gaps between columns.
        const GLYPH: usize = 2;
        let (status, worktrees, size, age) = match width {
            w if w >= 110 => (30, 26, 7, 9),
            w if w >= 90 => (26, 22, 7, 9),
            w if w >= 72 => (22, 18, 7, 9),
            w if w >= 56 => (20, 0, 7, 9),
            w if w >= 40 => (14, 0, 0, 9),
            _ => (10, 0, 0, 0),
        };
        let gaps = [status, worktrees, size, age]
            .iter()
            .filter(|&&w| w > 0)
            .count();
        let fixed = GLYPH + status + worktrees + size + age + gaps;
        Columns {
            status,
            name: width.saturating_sub(fixed).max(6),
            worktrees,
            size,
            age,
        }
    }
}

/// `text` cut or padded to exactly `width` columns.
fn cell(text: &str, width: usize) -> String {
    let cut = truncate(text, width);
    let pad = width.saturating_sub(cut.width());
    format!("{cut}{}", " ".repeat(pad))
}

/// The status glyph and its colour, for a task.
pub fn glyph(row: &TaskRow) -> (&'static str, Color) {
    if row.task.state == TaskState::Archived {
        return ("▪", Color::DarkGray);
    }
    activity_glyph(row.activity.as_ref())
}

/// The glyph and colour for what an agent is doing; `…` before anyone has
/// looked.
pub fn activity_glyph(activity: Option<&Activity>) -> (&'static str, Color) {
    match activity {
        None => ("…", Color::DarkGray),
        Some(Activity::Running { .. } | Activity::Live | Activity::Starting) => ("●", Color::Green),
        Some(Activity::Waiting { .. }) => ("◆", Color::Yellow),
        Some(Activity::Idle { .. }) => ("○", Color::Cyan),
        Some(Activity::NotRunning) => ("✕", Color::Red),
        Some(Activity::NotStarted | Activity::Ended) => ("·", Color::DarkGray),
    }
}

/// What the status column says for a task.
pub fn status_text(row: &TaskRow) -> String {
    if row.task.state == TaskState::Archived {
        return "archived".to_string();
    }
    let label = match &row.activity {
        // Not polled yet: the glyph says so, and a blank beats a guess.
        None => String::new(),
        Some(activity) => activity.label(),
    };
    match row.unanchored {
        // First, so a narrow column cuts the activity, not this.
        true => format!("unanchored · {label}"),
        false => label,
    }
}

/// What to do about the selected task's agent, when jeet refuses its
/// commands for want of a record of starting it.
pub fn unanchored_hint(row: Option<&TaskRow>) -> Option<String> {
    let row = row.filter(|row| row.unanchored)?;
    let slug = &row.task.slug;
    Some(format!(
        "{slug}'s agent is unanchored (an older jeet started it), so jeet refuses its \
         commands: `jeet task restart {slug}` when idle, or `jeet task adopt {slug}` from a \
         terminal outside jeet's tmux sessions"
    ))
}

/// The columns of a name a parent marker leaves it, if the name has them.
const MIN_NAME: usize = 4;

/// `parent` is the slug of the task's parent, `assistant` for the
/// assistant: a slug, which jeet made, never a name a manager wrote.
fn row_line(row: &TaskRow, parent: Option<&str>, columns: &Columns) -> Line<'static> {
    let (glyph, color) = glyph(row);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{} ", cell(&status_text(row), columns.status)),
            Style::default().fg(color),
        ),
    ];
    let name = match row.task.kind {
        // Which PR it reviews, when its checkout says.
        TaskKind::Review => tasks::review_label(&row.task, &row.worktrees)
            .unwrap_or_else(|| format!("{} (review)", row.task.name)),
        TaskKind::Manager => format!("{} (manager)", row.task.name),
        TaskKind::Work | TaskKind::Assistant => row.task.name.clone(),
    };
    // The name gives way before the marker does: a cut-off marker would pass
    // for a top-level task. A slug too long to leave the name a few columns
    // is shortened inside the marker, so `↳` is always there.
    let marker = parent.map(|p| {
        let room = columns.name.saturating_sub(MIN_NAME.min(name.width()));
        format!(" ↳ {}", truncate(p, room.saturating_sub(" ↳ ".width())))
    });
    let name = match marker {
        Some(marker) if columns.name > marker.width() => {
            format!("{}{marker}", truncate(&name, columns.name - marker.width()))
        }
        Some(marker) => truncate(&format!("{name}{marker}"), columns.name),
        None => truncate(&name, columns.name),
    };
    // The last report, dim, in what the name leaves of its column: the
    // columns after it fill the row.
    let report = row.report.as_ref().map(|report| {
        let body = messages::clean(&report.body).replace('\t', " ");
        format!(
            " · {}: {}",
            report.kind.as_str(),
            first_line(&body, 120).unwrap_or_default()
        )
    });
    let rest = columns.name.saturating_sub(name.width());
    spans.push(Span::styled(
        name,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(dim(cell(report.as_deref().unwrap_or_default(), rest)));
    if columns.worktrees > 0 {
        spans.push(Span::styled(
            format!(" {}", cell(&row.summary(), columns.worktrees)),
            Style::default().fg(Color::Magenta),
        ));
    }
    if columns.size > 0 {
        let size = row.size.map(human_size).unwrap_or_else(|| "…".into());
        spans.push(Span::styled(
            format!(" {size:>width$}", width = columns.size),
            Style::default().fg(Color::Blue),
        ));
    }
    if columns.age > 0 {
        spans.push(Span::styled(
            format!(
                " {:>width$}",
                age_label(row.last_activity),
                width = columns.age
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// The agents view's header cells (Q8): how many agents are live against
/// `max_live`, how many prompts wait on the user, how many starts wait on
/// `max_live`. The last two only when there are some.
fn agents_cells(ui: &TasksUi) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(info) = &ui.tree_info {
        let live = match info.max_live {
            Some(max) => format!("live {}/{max}   ", info.live.len()),
            None => format!("live {}   ", info.live.len()),
        };
        spans.push(Span::styled(live, Style::default().fg(Color::Green)));
    }
    let needs_you = needs_you(ui);
    if needs_you > 0 {
        spans.push(Span::styled(
            format!("needs you {needs_you}   "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let queued = ui.tree_info.as_ref().map_or(0, |info| info.queued);
    if queued > 0 {
        spans.push(Span::styled(
            format!("queued {queued}   "),
            Style::default().fg(Color::Blue),
        ));
    }
    spans
}

/// The prompts in the tree waiting on the user, each once: its nodes' own,
/// and the assistant's sessions'; and the questions passed to the user
/// (R16).
fn needs_you(ui: &TasksUi) -> usize {
    let waiting = |a: Option<&Activity>| matches!(a, Some(Activity::Waiting { .. }));
    let nodes = ui
        .rows
        .iter()
        .filter(|row| ui.shown.contains_key(&row.task.id) && waiting(row.activity.as_ref()))
        .count();
    let root = match (&ui.assistant_row, &ui.assistant) {
        (Some(_), Some(info)) => info
            .sessions
            .iter()
            .filter(|s| waiting(Some(&s.activity)))
            .count(),
        _ => 0,
    };
    let asks = ui.tree_info.as_ref().map_or(0, |info| {
        info.nodes
            .iter()
            .filter(|(id, node)| {
                // A node at a prompt is already counted above.
                ui.shown.contains_key(*id)
                    && node.ask.is_some_and(|(_, _, user)| user)
                    && !ui
                        .rows
                        .iter()
                        .any(|row| &row.task.id == *id && waiting(row.activity.as_ref()))
            })
            .count()
    });
    nodes + root + asks
}

/// The agents view: the assistant and the active tasks under it, as a tree
/// (design §6).
fn draw_tree(frame: &mut Frame, area: Rect, ui: &mut TasksUi) {
    let total = ui.count(View::Agents);
    let title = match (ui.filter.is_empty(), ui.tree.len()) {
        (true, 0) => " no agents ".to_string(),
        (true, n) if n == total => format!(" {n} agent{} ", if n == 1 { "" } else { "s" }),
        // Closed managers hide the rest.
        (true, n) => format!(" {n} of {total} agents shown "),
        (false, 0) => format!(" nothing matches \"{}\" ", ui.filter),
        (false, n) => format!(" {n} of {total} shown for \"{}\" ", ui.filter),
    };
    let block = list_block(title);
    if ui.tree.is_empty() {
        let hint = match ui.assistant_row {
            None => vec![
                "no assistant yet.".to_string(),
                String::new(),
                "c starts it; the managers and tasks it makes, and theirs, show here".to_string(),
                "as a tree, with what each needs rolled up to the nodes above it.".to_string(),
            ],
            Some(_) => vec!["⌫ or esc widens the filter".into()],
        };
        let lines: Vec<Line> = hint.into_iter().map(|l| Line::from(dim(l))).collect();
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let columns = TreeColumns::for_width(area.width.saturating_sub(2) as usize);
    let items: Vec<ListItem> = ui
        .tree
        .iter()
        .map(|line| ListItem::new(tree_line(ui, line, &columns)))
        .collect();
    let list = List::new(items).block(block).highlight_style(highlighted());
    let mut state = std::mem::take(&mut ui.list);
    state.select(Some(ui.selected.min(ui.tree.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    ui.list = state;
}

/// How wide each column of the agents view is at a given width; zero
/// drops it. Narrower, the notes go first, then the budget, then the
/// status's detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeColumns {
    pub name: usize,
    pub status: usize,
    pub budget: usize,
    pub notes: usize,
}

impl TreeColumns {
    pub fn for_width(width: usize) -> TreeColumns {
        // Room, when there is some, for a hold and what is below it.
        let (status, budget, notes) = match width {
            w if w >= 120 => (36, 12, true),
            w if w >= 80 => (24, 12, true),
            w if w >= 64 => (24, 12, false),
            w if w >= 44 => (22, 0, false),
            _ => (12, 0, false),
        };
        // A space after each column but the last.
        let gaps = 1 + usize::from(budget > 0) + usize::from(notes);
        let rest = width.saturating_sub(status + budget + gaps);
        let name = match notes {
            true => rest.saturating_sub(16).clamp(16, 32),
            false => rest.max(6),
        };
        TreeColumns {
            name,
            status,
            budget,
            notes: match notes {
                true => rest.saturating_sub(name),
                false => 0,
            },
        }
    }
}

/// One node of the tree: its place and name, what it shows (Q7), its
/// budget and its notes (Q9).
fn tree_line(ui: &TasksUi, line: &TreeLine, columns: &TreeColumns) -> Line<'static> {
    let row = match line.at {
        At::Root => ui.assistant_row.as_ref(),
        At::Row(i) => ui.rows.get(i),
    };
    let Some(row) = row else {
        return Line::from("");
    };
    let id = row.task.id.as_str();
    let node = ui.tree_info.as_ref().and_then(|info| info.nodes.get(id));
    let shown = ui.shown.get(id).copied().unwrap_or((Urgency::Idle, 0));
    // A slug, which jeet made, never a name a manager wrote.
    let name = match line.at {
        At::Root => "✦ assistant".to_string(),
        At::Row(_) => {
            let rails: String = line
                .rails
                .iter()
                .map(|&on| if on { "│  " } else { "   " })
                .collect();
            let branch = if line.last { "└─ " } else { "├─ " };
            let marker = match (line.manager, line.open) {
                (true, true) => "▾ ",
                (true, false) => "▸ ",
                (false, _) => "",
            };
            format!("{rails}{branch}{marker}{}", printable(&row.task.slug))
        }
    };
    let name_style = match line.at {
        At::Root => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        At::Row(_) => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };
    let (status, color) = node_status(ui, line.at, row, node, shown);
    let mut spans = vec![
        Span::styled(cell(&name, columns.name), name_style),
        Span::styled(
            format!(" {}", cell(&status, columns.status)),
            Style::default().fg(color),
        ),
    ];
    if columns.budget > 0 {
        spans.push(Span::styled(
            format!(" {}", cell(&budget_cell(line.at, node), columns.budget)),
            Style::default().fg(Color::Blue),
        ));
    }
    if columns.notes > 0 {
        spans.push(dim(format!(
            " {}",
            fitted_notes(&notes(ui, line.at, node), columns.notes)
        )));
    }
    Line::from(spans)
}

/// What a node's status column says (Q7), and in what colour: held first,
/// with what rolled up from below if it needs looking at; else the most
/// urgent of its own and its subtree's, with how many below are so.
fn node_status(
    ui: &TasksUi,
    at: At,
    row: &TaskRow,
    node: Option<&NodeInfo>,
    (urgency, below): (Urgency, u32),
) -> (String, Color) {
    let (glyph, color) = urgency_glyph(urgency);
    let rolled = match below {
        0 => own_status(ui, at, row, node, urgency),
        n => (
            glyph.to_string(),
            format!("{} ({n})", urgency_label(urgency)),
        ),
    };
    if let Some(node) = node.filter(|n| n.run != RunState::Run) {
        let (held, word, held_color) = match node.run {
            RunState::Paused => ("⏸", "paused", Color::Magenta),
            _ => ("■", "stopped", Color::Red),
        };
        let mut text = match node.held {
            0 => format!("{held} {word}"),
            n => format!("{held} {word} ({n} below)"),
        };
        // A pause does not answer a prompt.
        if urgency >= Urgency::Blocked {
            text.push_str(&format!(" · {}", rolled.1));
            return (text, color);
        }
        return (text, held_color);
    }
    let (glyph, text) = rolled;
    let color = match (below, urgency) {
        // Its own idle: as the active view colours what its agent is doing.
        (0, Urgency::Idle) => match at {
            At::Root => Color::Cyan,
            At::Row(_) => activity_glyph(row.activity.as_ref()).1,
        },
        _ => color,
    };
    match text.is_empty() {
        true => (glyph, color),
        false => (format!("{glyph} {text}"), color),
    }
}

/// A node's own state, as its glyph and words: what its agent is doing,
/// with its detail (the user may see it, Q10), or what it asks.
fn own_status(
    ui: &TasksUi,
    at: At,
    row: &TaskRow,
    node: Option<&NodeInfo>,
    urgency: Urgency,
) -> (String, String) {
    let (glyph, _) = urgency_glyph(urgency);
    let glyph = glyph.to_string();
    if at == At::Root {
        let sessions = ui.assistant.as_ref().map_or(&[][..], |a| &a.sessions[..]);
        let waiting = sessions
            .iter()
            .find(|s| matches!(s.activity, Activity::Waiting { .. }));
        let running = sessions.iter().filter(|s| s.running()).count();
        return match (ui.assistant.is_some(), waiting, running) {
            (false, _, _) => ("…".into(), String::new()),
            (true, Some(s), _) => (glyph, printable(&s.activity.label_with(true))),
            (true, None, 0) if sessions.is_empty() => ("·".into(), "no sessions".into()),
            (true, None, 0) => (glyph, "idle".into()),
            (true, None, 1) => (glyph, "running".into()),
            (true, None, n) => (glyph, format!("{n} running")),
        };
    }
    let activity = || match &row.activity {
        Some(activity) => printable(&activity.label_with(true)),
        None => String::new(),
    };
    match urgency {
        Urgency::Blocked => {
            let kind = node
                .and_then(|n| n.ask)
                .map_or("blocked", |(_, k, _)| k.as_str());
            (glyph, kind.to_string())
        }
        // Only a question passed to the user is its own.
        Urgency::NeedsYou => match node.and_then(|n| n.ask) {
            Some((_, kind, _)) => (glyph, format!("needs you – {}", kind.as_str())),
            None => (glyph, activity()),
        },
        Urgency::Budget => (glyph, "budget".into()),
        Urgency::Done => (glyph, "done".into()),
        Urgency::Idle => (
            activity_glyph(row.activity.as_ref()).0.to_string(),
            activity(),
        ),
        Urgency::NeedsInput | Urgency::Running => (glyph, activity()),
    }
}

fn urgency_glyph(urgency: Urgency) -> (&'static str, Color) {
    match urgency {
        Urgency::NeedsYou | Urgency::NeedsInput => ("◆", Color::Yellow),
        Urgency::Blocked => ("!", Color::Red),
        Urgency::Budget => ("$", Color::Magenta),
        Urgency::Running => ("●", Color::Green),
        Urgency::Idle => ("○", Color::Cyan),
        Urgency::Done => ("✓", Color::Green),
    }
}

fn urgency_label(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::NeedsYou => "needs you",
        Urgency::NeedsInput => "needs input",
        Urgency::Blocked => "blocked",
        Urgency::Budget => "budget",
        Urgency::Running => "running",
        Urgency::Idle => "idle",
        Urgency::Done => "done",
    }
}

/// `0h40/2h` against a time limit, else `12/40 turns`, else nothing; for
/// the assistant, which has no budget, its children.
fn budget_cell(at: At, node: Option<&NodeInfo>) -> String {
    let Some(node) = node else {
        return String::new();
    };
    if at == At::Root {
        return match node.children {
            1 => "1 child".into(),
            n => format!("{n} children"),
        };
    }
    let hours = |secs: i64, short: bool| {
        let (h, m) = (secs / 3600, secs % 3600 / 60);
        match (short, m) {
            (true, 0) => format!("{h}h"),
            _ => format!("{h}h{m:02}"),
        }
    };
    match &node.budget {
        Some((row, (secs, turns))) => match (row.max_secs, row.max_turns) {
            (Some(max), _) => format!("{}/{}", hours(*secs, false), hours(max, true)),
            (None, Some(max)) => format!("{turns}/{max} turns"),
            (None, None) => String::new(),
        },
        None => String::new(),
    }
}

/// What else is worth knowing of a node, most first.
fn notes(ui: &TasksUi, at: At, node: Option<&NodeInfo>) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(node) = node else {
        return notes;
    };
    if let Some(n) = node.inbox.filter(|&n| n > 0) {
        notes.push(format!("inbox {n}"));
    }
    if at == At::Root {
        let open = ui.assistant.as_ref().map_or(0, |a| a.open_todo());
        if open > 0 {
            notes.push(format!("todo {open}"));
        }
    }
    if let Some(pr) = &node.pr {
        notes.push(printable(pr));
    }
    let pct = node
        .budget
        .as_ref()
        .and_then(|(row, used)| budget::percent(row, *used));
    if let Some(pct) = pct.filter(|&pct| pct >= 80) {
        notes.push(format!("budget {pct}%"));
    }
    if node.queued {
        notes.push("queued".into());
    }
    notes
}

/// As many of `notes` as fit in `width`, dropped from the right.
fn fitted_notes(notes: &[String], width: usize) -> String {
    let mut out = String::new();
    for note in notes {
        let next = match out.is_empty() {
            true => note.clone(),
            false => format!("{out}  {note}"),
        };
        if next.width() > width {
            break;
        }
        out = next;
    }
    out
}

/// Where a [`Panel::Text`] is drawn in `full`.
fn text_area(full: Rect) -> Rect {
    centered_rect(90, 80, full)
}

/// Columns between tab stops in a [`Panel::Text`].
const TAB: usize = 8;

/// `lines` as drawn `width` wide: tabs expanded (ratatui gives them no
/// width), control characters dropped, long lines wrapped.
fn text_lines(lines: &[String], width: u16) -> Vec<String> {
    let width = (width as usize).max(1);
    let mut out = Vec::new();
    for line in lines {
        let mut expanded = String::new();
        let mut col = 0;
        for c in line.chars() {
            if c == '\t' {
                let n = TAB - col % TAB;
                expanded.push_str(&" ".repeat(n));
                col += n;
            } else if !c.is_control() {
                expanded.push(c);
                col += c.width().unwrap_or(0);
            }
        }
        let mut part = String::new();
        let mut used = 0;
        for c in expanded.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width && !part.is_empty() {
                out.push(std::mem::take(&mut part));
                used = 0;
            }
            part.push(c);
            used += w;
        }
        out.push(part);
    }
    out
}

fn list_block(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(Color::Green))
}

fn highlighted() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(Color::DarkGray))
}

/// A list of `items` in `area`, with `selected` highlighted.
fn render_list(
    frame: &mut Frame,
    area: Rect,
    title: String,
    items: Vec<ListItem>,
    selected: usize,
) {
    frame.render_widget(Clear, area);
    let mut state = ListState::default();
    state.select((!items.is_empty()).then(|| selected.min(items.len() - 1)));
    let list = List::new(items)
        .block(list_block(title))
        .highlight_style(highlighted());
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_text(frame: &mut Frame, area: Rect, title: String, color: Color, body: Vec<Line>) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_style(Style::default().fg(color)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn task_name(ui: &TasksUi, id: &str) -> String {
    ui.row(id).map(|r| r.task.name.clone()).unwrap_or_default()
}

/// Draw `panel` over the list, returning the links it shows.
fn draw_panel(frame: &mut Frame, ui: &TasksUi, panel: &Panel) -> Vec<Link> {
    let full = frame.area();
    match panel {
        Panel::Help { scroll } => {
            let help = help(ui.view);
            let lines: Vec<Line> = help
                .iter()
                .map(|(key, what)| {
                    Line::from(vec![
                        Span::styled(
                            format!("{key:<KEY_WIDTH$}"),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(what.to_string()),
                    ])
                })
                .collect();
            let widest = help
                .iter()
                .map(|(_, what)| KEY_WIDTH + what.width())
                .max()
                .unwrap_or(0);
            let area = fitted_rect(widest, help.len(), full);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(list_block(" tasks · keys · esc close ".into()))
                    .scroll((*scroll, 0)),
                area,
            );
        }
        Panel::Message { title, lines, .. } => {
            let mut body: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
            body.push(Line::from(""));
            body.push(Line::from(dim("⏎ or esc to continue")));
            let area = content_rect(70, body.len() + 2, full);
            render_text(frame, area, format!(" {title} "), Color::Yellow, body);
        }
        Panel::Confirm {
            title,
            lines,
            action,
        } => {
            let mut body: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
            body.push(Line::from(""));
            let keys = match action {
                super::tasks::Pending::RemoveWorktree { .. } => {
                    "y remove · f force (lose the above) · n keep it"
                }
                super::tasks::Pending::ForceResync { .. } => {
                    "f replace the task's copies too · n keep them"
                }
                super::tasks::Pending::Archive {
                    recursive: true, ..
                } => "y archive them all · n keep them as they are",
                super::tasks::Pending::Archive { .. } => "y archive · n keep it as it is",
                super::tasks::Pending::Delete {
                    recursive: true, ..
                } => "y delete them all · n keep them",
                super::tasks::Pending::Stop { .. } => "y stop · n leave it as it is",
                _ => "y delete · n keep it",
            };
            body.push(Line::from(dim(keys)));
            // Sized to its content: a destructive prompt that clips its own
            // warnings, or its y/n line, is worse than no prompt.
            let area = content_rect(70, body.len() + 2, full);
            render_text(frame, area, format!(" {title} "), Color::Red, body);
        }
        Panel::Sessions {
            task_id,
            sessions,
            selected,
        } => {
            let items = sessions
                .iter()
                .map(|s| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:<10}", age_label(s.last)),
                            Style::default().fg(Color::Blue),
                        ),
                        Span::styled(format!("{:<10}", s.state), Style::default().fg(Color::Cyan)),
                        Span::raw(format!("{}  ", truncate(&s.name, 30))),
                        dim(s.id.clone()),
                    ]))
                })
                .collect();
            let area = centered_rect(80, 60, full);
            let title = format!(
                " {} · which session? · ⏎ resume  esc cancel ",
                task_name(ui, task_id)
            );
            render_list(frame, area, title, items, *selected);
        }
        Panel::ReviewMenu {
            task_id,
            pr,
            label,
            selected,
        } => {
            let items = REVIEW_MENU
                .iter()
                .enumerate()
                .map(|(i, (key, what))| {
                    // A review job with no task has no session to open.
                    let off = i == 1 && task_id.is_none();
                    let what = match off {
                        true => format!("{what} (it has no task)"),
                        false => what.to_string(),
                    };
                    let style = match off {
                        true => Style::default().fg(Color::DarkGray),
                        false => Style::default(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{key}  "),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(what, style),
                    ]))
                })
                .collect();
            let label = match task_id {
                Some(_) => printable(label),
                None => review_name(pr.as_ref()),
            };
            let area = content_rect(70, REVIEW_MENU.len() + 2, full);
            let title = format!(" {label} · ⏎ choose  esc cancel ");
            render_list(frame, area, title, items, *selected);
        }
        Panel::Places {
            task_id,
            places,
            selected,
        } => {
            let items = places
                .iter()
                .map(|p| {
                    let color = match p.repo_id {
                        None => Color::Magenta,
                        Some(_) => Color::Yellow,
                    };
                    ListItem::new(Line::from(Span::styled(
                        p.label.clone(),
                        Style::default().fg(color),
                    )))
                })
                .collect();
            let area = content_rect(70, places.len() + 2, full);
            let title = format!(" explore {} · ⏎ open  esc cancel ", task_name(ui, task_id));
            render_list(frame, area, title, items, *selected);
        }
        Panel::Worktrees {
            task_id,
            lines,
            selected,
        } => {
            let area = centered_rect(85, 70, full);
            let title = format!(
                " {} · worktrees · ⏎ explore  n add  D remove  r refresh ",
                task_name(ui, task_id)
            );
            if lines.is_empty() {
                let body = vec![
                    Line::from("no worktrees: the task is an empty canvas"),
                    Line::from(""),
                    Line::from(dim(
                        "n adds one, or the agent does with `jeet worktree add <repo>`",
                    )),
                ];
                render_text(frame, area, title, Color::Green, body);
                return Vec::new();
            }
            let items = lines
                .iter()
                .map(|line| {
                    let mut spans = vec![
                        Span::styled(
                            format!("{:<18}", truncate(&line.record.dir_name, 17)),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<24}", truncate(&line.head, 23)),
                            Style::default().fg(Color::Magenta),
                        ),
                    ];
                    if line.present {
                        spans.extend(worktree_status_spans(&line.status));
                    } else {
                        spans.push(dim("archived — not on disk"));
                    }
                    spans.push(dim(format!("  {}", line.record.repo_id)));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            render_list(frame, area, title, items, *selected);
        }
        Panel::AddWorktree {
            task_id,
            repos,
            branch,
            field,
        } => {
            let mut body = vec![
                field_line("repo", &repos.filter, *field == AddField::Repo, ""),
                field_line(
                    "branch",
                    branch,
                    *field == AddField::Branch,
                    "blank: detached at the default branch",
                ),
                Line::from(""),
            ];
            body.extend(picker_lines(repos, false, 10));
            body.push(Line::from(""));
            body.push(Line::from(dim(
                "⇥ switch field · ↑↓ pick · ⏎ add (a URL not in the store is cloned) · esc back",
            )));
            let title = format!(" add a worktree to {} ", task_name(ui, task_id));
            // Room for the hint to wrap on a narrow terminal.
            let area = content_rect(74, body.len() + 1, full);
            render_text(frame, area, title, Color::Green, body);
        }
        Panel::NewTask(form) => {
            let body = new_task_lines(form);
            // Room for the hint, and for a refusal, to wrap.
            let slack = 1 + 3 * usize::from(form.error.is_some());
            let area = content_rect(74, body.len() + slack, full);
            let title = match &form.parent {
                Some((_, label)) => format!(" new task under {label} "),
                None => " new task ".into(),
            };
            render_text(frame, area, title, Color::Green, body);
        }
        Panel::Projects { lines, selected } => {
            let area = centered_rect(85, 60, full);
            let title = " projects · ⏎ new task  n new  e edit  D delete  r refresh ".to_string();
            if lines.is_empty() {
                let body = vec![
                    Line::from("no projects yet"),
                    Line::from(""),
                    Line::from(dim(
                        "a project presets repositories, AGENTS.md and skills for new tasks;",
                    )),
                    Line::from(dim("n makes one")),
                ];
                render_text(frame, area, title, Color::Green, body);
                return Vec::new();
            }
            let items = lines
                .iter()
                .map(|line| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:<20}", truncate(&line.name, 19)),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<9}", format!("{} tasks", line.tasks)),
                            Style::default().fg(Color::Blue),
                        ),
                        Span::styled(
                            format!("{:<22}", truncate(&line.contents, 21)),
                            Style::default().fg(Color::Magenta),
                        ),
                        dim(line.repos.clone()),
                    ]))
                })
                .collect();
            render_list(frame, area, title, items, *selected);
        }
        Panel::NewProject(form) => {
            let body = new_project_lines(form);
            // Room for the hint to wrap on a narrow terminal.
            let area = content_rect(74, body.len() + 1, full);
            render_text(frame, area, " new project ".into(), Color::Green, body);
        }
        Panel::Prs {
            task_id,
            prs,
            selected,
            looking,
            note,
        } => {
            let area = centered_rect(85, 60, full);
            let name = task_name(ui, task_id);
            if prs.is_empty() {
                let said = match (looking, note) {
                    (true, _) => "looking for pull requests…".to_string(),
                    (false, Some(note)) => format!("could not look them up: {note}"),
                    (false, None) => {
                        "none yet: PRs opened from the task's worktrees show up here".into()
                    }
                };
                let body = vec![Line::from(""), Line::from(dim(format!("  {said}")))];
                render_text(
                    frame,
                    area,
                    format!(" {name} · pull requests "),
                    Color::Green,
                    body,
                );
                return Vec::new();
            }
            let looking = if *looking { " · looking…" } else { "" };
            let title = format!(
                " {name} · pull requests{looking} · ⏎/o open in the browser · v view  esc close "
            );
            return draw_prs(frame, area, title, prs, *selected);
        }
        Panel::Assistant(picker) => {
            let area = centered_rect(85, 60, full);
            let [list, footer] = with_footer(frame, area);
            let title = " assistant · sessions ".to_string();
            if picker.sessions.is_empty() {
                let body = vec![
                    Line::from("no sessions yet"),
                    Line::from(""),
                    Line::from(dim("n starts one")),
                ];
                render_text(frame, list, title, Color::Green, body);
            } else {
                let items = picker.sessions.iter().map(chat::session_line).collect();
                render_list(frame, list, title, items, picker.selected);
            }
            let line = match &picker.renaming {
                Some(input) => field_line("new name", input, true, "⏎ save  esc cancel"),
                None if !picker.status.is_empty() => Line::from(Span::styled(
                    picker.status.clone(),
                    Style::default().fg(Color::Yellow),
                )),
                None => Line::from(dim(chat::KEYS.replace("q quit", "esc back"))),
            };
            frame.render_widget(Paragraph::new(line), footer);
        }
        Panel::Todo { selected, adding } => {
            let area = centered_rect(80, 60, full);
            let [list, footer] = with_footer(frame, area);
            let open = ui.todo().iter().filter(|i| !i.done).count();
            let title =
                format!(" todo · {open} open · space done  ⏎ go to task  a add  esc close ");
            if ui.todo().is_empty() {
                let said = match &ui.assistant {
                    None => "reading todo.md…".to_string(),
                    Some(info) => match &info.todo_error {
                        Some(error) => format!("could not read todo.md: {error}"),
                        None => "nothing to do".to_string(),
                    },
                };
                let body = vec![
                    Line::from(printable(&said)),
                    Line::from(""),
                    Line::from(dim(
                        "a adds an item; the assistant keeps the list too, in todo.md",
                    )),
                ];
                render_text(frame, list, title, Color::Green, body);
            } else {
                let items = ui.todo().iter().map(todo_line).collect();
                render_list(frame, list, title, items, *selected);
            }
            let line = match adding {
                Some(input) => field_line(
                    "new item",
                    input,
                    true,
                    "!p1 gives it a priority, @task-slug links a task",
                ),
                None => Line::from(""),
            };
            frame.render_widget(Paragraph::new(line), footer);
        }
        Panel::Inbox { selected } => {
            let area = centered_rect(90, 70, full);
            let queued = ui.assistant.as_ref().map_or(0, |a| a.inbox);
            let title =
                format!(" inbox & activity · {queued} queued · ⏎ open the link  esc close ");
            if ui.feed().is_empty() {
                let body = vec![
                    Line::from(match ui.assistant {
                        None => "reading…",
                        Some(_) => "nothing yet",
                    }),
                    Line::from(""),
                    Line::from(dim(
                        "messages queued for the assistant, what `jeet watch` saw on GitHub",
                    )),
                    Line::from(dim("and what the assistant did show up here, newest first")),
                ];
                render_text(frame, area, title, Color::Green, body);
                return Vec::new();
            }
            let items = ui.feed().iter().map(feed_line).collect();
            render_list(frame, area, title, items, *selected);
        }
        Panel::Answer {
            title,
            question,
            input,
            error,
            ..
        } => {
            // The question, wrapped, as much of it as leaves room for the
            // input, the error and the keys.
            let width = content_rect(70, 0, full).width.saturating_sub(2);
            let room = (full.height as usize).saturating_sub(8).max(1);
            let mut asked = text_lines(question, width);
            if asked.len() > room {
                asked.truncate(room.saturating_sub(1));
                asked.push("… (esc, then i shows the rest)".into());
            }
            let mut body: Vec<Line> = asked.into_iter().map(Line::from).collect();
            body.push(Line::from(""));
            body.push(field_line("answer", &printable(input), true, ""));
            body.push(match error {
                Some(error) => Line::from(Span::styled(
                    printable(error),
                    Style::default().fg(Color::Red),
                )),
                None => Line::from(""),
            });
            body.push(Line::from(dim("⏎ answer as you · esc cancel")));
            let area = content_rect(70, body.len() + 2, full);
            render_text(
                frame,
                area,
                format!(" {} ", printable(title)),
                Color::Yellow,
                body,
            );
        }
        Panel::Text {
            title,
            lines,
            scroll,
            prs,
        } => {
            let area = text_area(full);
            let keys = match prs {
                Some(_) => "↑↓ PgUp PgDn g G scroll · ⏎ its pull requests · esc close",
                None => "↑↓ PgUp PgDn g G scroll · esc close",
            };
            let body: Vec<Line> = text_lines(lines, area.width.saturating_sub(2))
                .into_iter()
                .map(Line::from)
                .collect();
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(body)
                    .block(list_block(format!(" {} · {keys} ", printable(title))))
                    .scroll((*scroll, 0)),
                area,
            );
        }
    }
    Vec::new()
}

/// `area`, cleared, as a box and the line below it.
fn with_footer(frame: &mut Frame, area: Rect) -> [Rect; 2] {
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    [rows[0], rows[1]]
}

/// `[ ] p1 Fix the login cookie  @fix-login`, dimmed once it is done.
fn todo_line(item: &ItemView) -> ListItem<'static> {
    let (mark, style) = match item.done {
        true => ("[x] ", Style::default().fg(Color::DarkGray)),
        false => ("[ ] ", Style::default()),
    };
    let mut spans = vec![Span::styled(mark, style)];
    if let Some(p) = item.priority {
        let color = match (item.done, p) {
            (true, _) => Color::DarkGray,
            (false, 1) => Color::Red,
            (false, 2) => Color::Yellow,
            (false, _) => Color::Blue,
        };
        spans.push(Span::styled(format!("p{p} "), Style::default().fg(color)));
    }
    spans.push(Span::styled(printable(&item.title), style));
    for slug in &item.tasks {
        spans.push(Span::styled(
            format!("  @{slug}"),
            Style::default().fg(Color::Magenta),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// When, what, the text and where: one entry of the inbox panel. `↗` marks
/// the ones ⏎ opens.
fn feed_line(entry: &FeedEntry) -> ListItem<'static> {
    let color = match (entry.kind, entry.what.as_str()) {
        (FeedKind::Queued, _) => Color::Yellow,
        (FeedKind::Event, _) => Color::Cyan,
        (FeedKind::Action, "done") => Color::Green,
        (FeedKind::Action, _) => Color::Red,
    };
    let mut spans = vec![
        Span::styled(
            format!("{:<10}", age_label(entry.at)),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(
            format!("{:<18}", truncate(&entry.what, 17)),
            Style::default().fg(color),
        ),
        Span::raw(printable(&entry.text)),
    ];
    if !entry.place.is_empty() {
        spans.push(dim(format!("  {}", printable(&entry.place))));
    }
    if entry.url.is_some() {
        spans.push(Span::styled(" ↗", Style::default().fg(Color::Cyan)));
    }
    ListItem::new(Line::from(spans))
}

/// The PR history: state, size, number and title — the title a link to the
/// PR, written in after the frame by [`super::term::paint_links`]. Returns
/// where the links are.
fn draw_prs(
    frame: &mut Frame,
    area: Rect,
    title: String,
    prs: &[TaskPrRecord],
    selected: usize,
) -> Vec<Link> {
    let added: Vec<String> = prs.iter().map(|pr| format!("+{}", pr.additions)).collect();
    let removed: Vec<String> = prs.iter().map(|pr| format!("−{}", pr.deletions)).collect();
    let numbers: Vec<String> = prs.iter().map(|pr| format!("#{}", pr.number)).collect();
    let widest = |column: &[String]| column.iter().map(|c| c.width()).max().unwrap_or(0);
    let (wa, wr, wn) = (widest(&added), widest(&removed), widest(&numbers));
    // state, then the columns, each followed by a space.
    let lead = 7 + 1 + wa + 1 + wr + 1 + wn + 1;

    let items: Vec<ListItem> = prs
        .iter()
        .enumerate()
        .map(|(i, pr)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<7} ", pr_history::state_label(pr)),
                    Style::default().fg(pr_state_color(pr)),
                ),
                Span::styled(
                    format!("{:>wa$} ", added[i]),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    format!("{:>wr$} ", removed[i]),
                    Style::default().fg(Color::Red),
                ),
                Span::styled(
                    format!("{:<wn$} ", numbers[i]),
                    Style::default().fg(Color::Blue),
                ),
                Span::raw(printable(&pr.title)),
            ]))
        })
        .collect();

    frame.render_widget(Clear, area);
    let mut state = ListState::default();
    state.select(Some(selected.min(prs.len() - 1)));
    let list = List::new(items)
        .block(list_block(title))
        .highlight_style(highlighted());
    frame.render_stateful_widget(list, area, &mut state);

    let inner = area.inner(ratatui::layout::Margin::new(1, 1));
    let room = (inner.width as usize).saturating_sub(lead);
    prs.iter()
        .enumerate()
        .skip(state.offset())
        .take(inner.height as usize)
        .enumerate()
        .filter_map(|(row, (_, pr))| {
            let width = printable(&pr.title).width().min(room);
            (width > 0).then(|| Link {
                x: inner.x + lead as u16,
                y: inner.y + row as u16,
                width: width as u16,
                url: pr.url.clone(),
            })
        })
        .collect()
}

/// Open green, draft grey, merged purple, closed red, as GitHub has them.
pub fn pr_state_color(pr: &TaskPrRecord) -> Color {
    state_color(pr_history::state_label(pr))
}

/// Uncommitted changes, commits ahead of and behind the default branch, and
/// the diff against it. Unlike the explorer's single word, all of them: a
/// dirty worktree can be ahead too, and both are at stake when removing it.
fn worktree_status_spans(status: &WorktreeStatus) -> Vec<Span<'static>> {
    // "Could not tell" must never read as clean.
    if status.unknown.is_some() {
        return vec![Span::styled(
            format!("{:<22}", "unknown"),
            Style::default().fg(Color::Red),
        )];
    }
    let dirty = match status.dirty {
        0 => Span::styled(
            format!("{:<14}", "clean"),
            Style::default().fg(Color::Green),
        ),
        n => Span::styled(
            format!("{:<14}", format!("{n} uncommitted")),
            Style::default().fg(Color::Red),
        ),
    };
    vec![
        dirty,
        Span::styled(
            format!("{:<10}", format!("↑{} ↓{}", status.ahead, status.behind)),
            Style::default().fg(Color::Blue),
        ),
        dim(status.diff_summary()),
    ]
}

/// `label  value█  hint` — a text field, with the cursor when it has focus.
fn field_line(label: &str, value: &str, focused: bool, hint: &str) -> Line<'static> {
    let label_style = if focused {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut spans = vec![
        Span::styled(format!("{label:<9}"), label_style),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if focused {
        spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
    }
    if value.is_empty() && !hint.is_empty() {
        spans.push(dim(format!(" {hint}")));
    }
    Line::from(spans)
}

/// Up to `rows` of a picker around its cursor, with marks when `marks`.
fn picker_lines(picker: &Picker, marks: bool, rows: usize) -> Vec<Line<'static>> {
    // A multi-select picker adds what was typed on space; a single one
    // takes it on ⏎.
    let add = if marks { "space" } else { "⏎" };
    if picker.visible.is_empty() && picker.filter.trim().is_empty() {
        return vec![Line::from(dim(format!(
            "  no repositories in the store yet — type a clone URL, {add} adds it"
        )))];
    }
    if picker.visible.is_empty() {
        return vec![Line::from(dim(format!(
            "  nothing matches — {add} adds it as a new repository"
        )))];
    }
    let start = picker
        .selected
        .saturating_sub(rows.saturating_sub(1))
        .min(picker.visible.len().saturating_sub(rows));
    picker
        .visible
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(at, &i)| {
            let cursor = if at == picker.selected { "▸ " } else { "  " };
            let mark = match (marks, picker.marked[i]) {
                (false, _) => "",
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
            };
            let style = if at == picker.selected {
                highlighted()
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{cursor}{mark}{}", picker.items[i]),
                style,
            ))
        })
        .collect()
}

/// The marked repositories by short name, for a form's repos field.
fn chosen_names(picker: &Picker) -> String {
    picker
        .chosen()
        .iter()
        .map(|r| {
            let r = r.trim_end_matches('/').trim_end_matches(".git");
            r.rsplit(['/', ':']).next().unwrap_or(r).to_string()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A focused repos field's filter hint and picker, shared by the new-task
/// and new-project forms.
fn repos_picker_lines(picker: &Picker) -> Vec<Line<'static>> {
    let filter = if picker.filter.is_empty() {
        "type to narrow or paste a clone URL · space marks".to_string()
    } else {
        format!("matching \"{}\" · ^a adds it as typed", picker.filter)
    };
    let mut lines = vec![Line::from(dim(format!("         {filter}")))];
    lines.extend(picker_lines(picker, true, 8));
    lines
}

fn new_project_lines(form: &NewProjectForm) -> Vec<Line<'static>> {
    let focus = form.field;
    let mut lines = vec![
        field_line(
            "name",
            &form.name,
            focus == ProjectField::Name,
            "letters, digits, - _ and .",
        ),
        field_line(
            "repos",
            &chosen_names(&form.repos),
            focus == ProjectField::Repos,
            "none: tasks from it start with no worktrees",
        ),
    ];
    if focus == ProjectField::Repos {
        lines.extend(repos_picker_lines(&form.repos));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(dim("⇥ switch field · ⏎ create · esc back")));
    lines
}

fn new_task_lines(form: &NewTaskForm) -> Vec<Line<'static>> {
    let focus = form.field;
    // A value chosen with ←→ or space, rather than typed.
    let choice = |label: &str, value: String, field: Field, hint: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("{label:<9}"),
                if focus == field {
                    Style::default().fg(Color::Magenta)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(value, Style::default().fg(Color::Yellow)),
            dim(hint),
        ])
    };
    let manager = form.kind == TaskKind::Manager;
    let mut lines = Vec::new();
    for &field in form.fields() {
        let focused = focus == field;
        match field {
            Field::Kind => lines.push(choice(
                "kind",
                match manager {
                    true => "‹ manager ›".to_string(),
                    false => "‹ task ›".to_string(),
                },
                field,
                match manager {
                    true => "  space switches · starts at once with its brief",
                    false => "  space switches · ⏎ on it later starts its agent",
                },
            )),
            Field::Name => lines.push(field_line(
                "name",
                &form.name,
                focused,
                &format!("blank: {}", form.default_name),
            )),
            Field::Goal => lines.push(field_line(
                "goal",
                &form.goal,
                focused,
                match manager {
                    true => "what it works towards (needed)",
                    false => "what its parent wants of it",
                },
            )),
            Field::Done => lines.push(field_line(
                "done",
                &form.done,
                focused,
                "when the goal is met",
            )),
            Field::Budget => lines.push(field_line(
                "budget",
                &form.budget,
                focused,
                "e.g. 2h,40turns; blank: the default",
            )),
            Field::Allow => lines.push(field_line(
                "allow",
                &form.allow,
                focused,
                "capabilities, comma-separated; blank: the default",
            )),
            Field::Project => lines.push(choice(
                "project",
                match form.project_name() {
                    Some(name) => format!("‹ {name} ›"),
                    None => "‹ none ›".to_string(),
                },
                field,
                match form.projects.is_empty() {
                    true => "  no projects yet (P makes one)",
                    false => "  ←→ choose",
                },
            )),
            Field::Repos => {
                lines.push(field_line(
                    "repos",
                    &chosen_names(&form.repos),
                    focused,
                    "none: the agent adds worktrees as it goes",
                ));
                if focused {
                    lines.extend(repos_picker_lines(&form.repos));
                }
            }
            Field::Branch => lines.push(field_line(
                "branch",
                &form.branch,
                focused,
                "only with a single repository",
            )),
        }
    }
    lines.push(Line::from(""));
    if let Some(error) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("⚠ {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(dim(match form.parent {
        Some(_) => "⇥ next field · ⏎ create · esc cancel",
        None => "⇥ next field · ⏎ create · esc cancel · all blank: an empty canvas",
    })));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{
        ReportKind, RunState, TaskRecord, TaskReportRecord, TaskWorktreeRecord,
    };
    use crate::tui::tasks::{NewTaskForm, Panel};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn row(name: &str, activity: Option<Activity>) -> TaskRow {
        TaskRow {
            task: TaskRecord {
                id: name.into(),
                slug: name.into(),
                name: name.into(),
                project: None,
                kind: TaskKind::Work,
                state: TaskState::Active,
                created_at: 0,
                last_activity: crate::worktrees::now_secs() - 120,
                parent_id: None,
                placement: crate::placement::Placement::Local,
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            },
            dir: PathBuf::from("/t"),
            worktrees: vec![TaskWorktreeRecord {
                id: 1,
                task_id: name.into(),
                repo_id: "github.com/acme/api".into(),
                dir_name: "api".into(),
                branch: None,
                created_branch: false,
                head_oid: None,
                stash_ref: None,
                pr_number: None,
                created_at: 0,
                read_only: false,
            }],
            activity,
            last_activity: crate::worktrees::now_secs() - 120,
            size: Some(3 * 1024 * 1024),
            report: None,
            review_jobs: Vec::new(),
            unanchored: false,
            queued: false,
        }
    }

    #[test]
    fn an_unanchored_agent_says_so_first_with_its_fix() {
        let mut r = row("api", Some(Activity::Idle { since: 0 }));
        assert_eq!(unanchored_hint(Some(&r)), None);
        r.unanchored = true;
        assert!(
            status_text(&r).starts_with("unanchored · idle"),
            "{}",
            status_text(&r)
        );
        let hint = unanchored_hint(Some(&r)).unwrap();
        assert!(hint.contains("jeet task restart api"), "{hint}");
        assert!(hint.contains("jeet task adopt api"), "{hint}");
        assert_eq!(unanchored_hint(None), None);
    }

    #[test]
    fn the_assistant_line_counts_its_unanchored_windows() {
        let text = |info: &AssistantInfo| {
            assistant_line(Some(info))
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let mut i = info();
        assert!(!text(&i).contains("unanchored"));
        i.unanchored = 2;
        assert!(
            text(&i).contains("2 unanchored: jeet task restart assistant"),
            "{}",
            text(&i)
        );
    }

    fn render(ui: &mut TasksUi, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, ui)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_row_shows_status_name_worktrees_size_and_age() {
        let mut ui = TasksUi::new(vec![row(
            "fix-login",
            Some(Activity::Running {
                detail: Some("Edit".into()),
            }),
        )]);
        let screen = render(&mut ui, 120, 12).join("\n");
        assert!(screen.contains("● running – Edit"), "{screen}");
        assert!(screen.contains("fix-login"), "{screen}");
        assert!(screen.contains("1 wt: api"), "{screen}");
        assert!(screen.contains("3.0M"), "{screen}");
        assert!(screen.contains("2m ago"), "{screen}");
        assert!(screen.contains("active 1"), "{screen}");
    }

    #[test]
    fn a_review_row_names_its_pr() {
        let mut review = row("review-api-55", None);
        review.task.kind = TaskKind::Review;
        review.worktrees[0].pr_number = Some(55);
        let mut ui = TasksUi::new(vec![review]);
        let screen = render(&mut ui, 120, 12).join("\n");
        assert!(screen.contains("review api#55"), "{screen}");
        assert!(!screen.contains("review-api-55"), "{screen}");
    }

    fn reviews_ui() -> TasksUi {
        use crate::tui::reviews::{Legacy, PrKey, PrReview, Verdict};
        let key = |number| PrKey {
            repo_id: "github.com/acme/api".into(),
            number,
        };
        let mut running = row("review-api-55", Some(Activity::Running { detail: None }));
        running.task.kind = TaskKind::Review;
        running.worktrees[0].pr_number = Some(55);
        let mut archived = row("review-api-9", None);
        archived.task.kind = TaskKind::Review;
        archived.task.state = TaskState::Archived;
        archived.worktrees[0].pr_number = Some(9);
        let mut ui = TasksUi::new(vec![running, archived, row("not a review", None)]);
        ui.legacy = vec![Legacy {
            pr: key(3),
            title: "old \u{1b}[2Jjob".into(),
            running: false,
            started_at: 0,
        }];
        ui.reviewed.insert(
            key(55),
            Ok(PrReview {
                title: "Add \u{1b}]8;;x\u{7}reviews".into(),
                state: "draft",
                verdict: Verdict::ChangesRequested,
            }),
        );
        ui.reviewed.insert(
            key(9),
            Ok(PrReview {
                title: "Old fix".into(),
                state: "merged",
                verdict: Verdict::Approved,
            }),
        );
        ui.reviewed
            .insert(key(3), Err("\u{1b}[2Jgh is not installed".into()));
        ui.view = View::Reviews;
        ui.set_rows(ui.rows.clone());
        ui
    }

    #[test]
    fn a_held_review_says_what_holds_it() {
        let mut review = row("r", None);
        assert_eq!(review_session(&review), "");
        review.queued = true;
        assert_eq!(review_session(&review), "queued on max_live");
        review.task.run_state = RunState::Paused;
        review.task.run_state_by = Some(Setter::Budget);
        assert_eq!(review_session(&review), "paused at its caps");
        review.activity = Some(Activity::Running { detail: None });
        assert!(review_session(&review).starts_with("paused at its caps · running"));
        review.task.run_state_by = Some(Setter::User);
        review.activity = Some(Activity::NotRunning);
        assert_eq!(review_session(&review), "paused");
        review.task.state = TaskState::Archived;
        assert_eq!(review_session(&review), "archived");
    }

    #[test]
    fn the_reviews_view_shows_each_review_with_its_pr_and_review_state() {
        let mut ui = reviews_ui();
        let screen = render(&mut ui, 140, 14);
        let all = screen.join("\n");
        assert!(all.contains(" reviews 3 "), "{all}");
        assert!(all.contains("3 reviews"), "{all}");
        let running = line_of(&screen, "api#55");
        assert!(running.contains("● running"), "{running}");
        assert!(running.contains("draft"), "{running}");
        assert!(running.contains("changes requested"), "{running}");
        // A title is shown, never run: its escapes go.
        assert!(running.contains("Add ]8;;xreviews"), "{running}");
        let archived = line_of(&screen, "api#9");
        assert!(archived.contains("▪ archived"), "{archived}");
        assert!(archived.contains("merged") && archived.contains("approved"));
        let legacy = line_of(&screen, "api#3");
        assert!(legacy.contains("no task"), "{legacy}");
        assert!(legacy.contains("unknown: [2Jgh"), "{legacy}");
        assert!(!all.contains("not a review"), "{all}");
        assert!(screen.iter().all(|l| !l.contains('\u{1b}')), "{screen:?}");
        // Archived after active, and the job no task holds last.
        let at = |what: &str| screen.iter().position(|l| l.contains(what)).unwrap();
        assert!(at("api#55") < at("api#9") && at("api#9") < at("api#3"));
        for (w, h) in [(160, 40), (90, 24), (70, 20), (40, 12), (1, 1)] {
            for panel in all_panels() {
                ui.panel = panel;
                render(&mut ui, w, h);
            }
        }
    }

    #[test]
    fn every_width_and_panel_draws_without_panicking() {
        let mut child = row("c", Some(Activity::Idle { since: 0 }));
        child.task.parent_id = Some("assistant-id".into());
        child.report = Some(TaskReportRecord {
            id: 1,
            task_id: "c".into(),
            to_id: "assistant-id".into(),
            kind: ReportKind::Blocked,
            body: format!("\u{1b}[2J{}\twide\u{1b}]8;;x\u{7}", "漢字".repeat(200)),
            at: 0,
            needs_user: false,
            answer: None,
            answered_by: None,
        });
        let mut ui = TasksUi::new(vec![
            row("a", Some(Activity::Waiting { detail: None })),
            row("b", None),
            child,
        ]);
        for info in [None, Some(info())] {
            ui.assistant = info;
            for panel in all_panels() {
                ui.panel = panel;
                for (w, h) in [(160, 40), (80, 24), (50, 12), (20, 6), (1, 1)] {
                    let screen = render(&mut ui, w, h);
                    if (w, ui.panel.is_none()) == (160, true) {
                        let all = screen.join("\n");
                        assert!(all.contains("c ↳ assistant · blocked: [2J"), "{all}");
                    }
                    assert!(screen.iter().all(|l| !l.contains('\u{1b}')), "{screen:?}");
                }
            }
        }
        // The agents view, with and without a tree.
        for mut ui in [agents_ui(), TasksUi::new(Vec::new())] {
            ui.view = View::Agents;
            ui.set_rows(ui.rows.clone());
            for panel in all_panels() {
                ui.panel = panel;
                for (w, h) in [(160, 40), (80, 24), (60, 20), (50, 12), (20, 6), (1, 1)] {
                    let screen = render(&mut ui, w, h);
                    assert!(screen.iter().all(|l| !l.contains('\u{1b}')), "{screen:?}");
                }
            }
        }
    }

    #[test]
    fn the_stop_confirm_is_never_clipped() {
        let running: Vec<String> = ["fix-token-refresh", "api-docs", "backend"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ui = agents_ui();
        ui.view = View::Agents;
        ui.set_rows(ui.rows.clone());
        ui.panel = Some(Panel::Confirm {
            title: "stop manager backend".into(),
            lines: crate::tui::tasks::stop_warning("manager backend", &running, false),
            action: crate::tui::tasks::Pending::Stop {
                task_id: Some("backend".into()),
            },
        });
        let all = render(&mut ui, 70, 24).join("\n");
        for want in [
            "stop manager backend",
            "stopping manager backend ends its agent",
            "fix-token-refresh",
            "api-docs",
            "only you can resume it",
            "y stop · n leave it as it is",
        ] {
            assert!(all.contains(want), "{want}: {all}");
        }
    }

    #[test]
    fn the_form_under_a_node_shows_a_managers_fields_or_a_workers() {
        let mut ui = agents_ui();
        ui.view = View::Agents;
        ui.set_rows(ui.rows.clone());
        let mut form = NewTaskForm::new(
            "task 1".into(),
            Vec::new(),
            None,
            vec!["github.com/acme/api".into()],
        )
        .under("backend".into(), "manager backend".into());
        ui.panel = Some(Panel::NewTask(Box::new(form.clone())));
        let worker = render(&mut ui, 100, 30).join("\n");
        for want in [
            "new task under manager backend",
            "‹ task ›",
            "goal",
            "done",
            "budget",
            "allow",
            "repos",
            "branch",
        ] {
            assert!(worker.contains(want), "{want}: {worker}");
        }
        form.kind = TaskKind::Manager;
        form.error =
            Some("a manager needs --goal or --brief-file: it is what it works towards".into());
        ui.panel = Some(Panel::NewTask(Box::new(form)));
        let manager = render(&mut ui, 70, 30).join("\n");
        for want in [
            "‹ manager ›",
            "goal",
            "done",
            "budget",
            "allow",
            "a manager needs --goal",
            "esc cancel",
        ] {
            assert!(manager.contains(want), "{want}: {manager}");
        }
        for unwanted in ["repos", "branch", "project"] {
            assert!(!manager.contains(unwanted), "{unwanted}: {manager}");
        }
    }

    /// No panel, and one of each.
    fn all_panels() -> Vec<Option<Panel>> {
        vec![
            None,
            Some(Panel::Help { scroll: 3 }),
            Some(Panel::NewTask(Box::new(NewTaskForm::new(
                "task 1".into(),
                vec!["web".into()],
                None,
                vec!["github.com/acme/api".into()],
            )))),
            Some(Panel::NewTask(Box::new(
                NewTaskForm::new("task 1".into(), Vec::new(), None, Vec::new())
                    .under("assistant".into(), "the assistant".into()),
            ))),
            Some(Panel::Projects {
                lines: Vec::new(),
                selected: 0,
            }),
            Some(Panel::Worktrees {
                task_id: "a".into(),
                lines: Vec::new(),
                selected: 0,
            }),
            Some(Panel::Assistant(chat::Picker::new(Vec::new()))),
            Some(Panel::Assistant(chat::Picker::new(vec![session(
                "s", true, true,
            )]))),
            Some(Panel::Todo {
                selected: 5,
                adding: Some("half".into()),
            }),
            Some(Panel::Inbox { selected: 9 }),
            Some(Panel::Confirm {
                title: "stop every task under the assistant".into(),
                lines: crate::tui::tasks::stop_warning(
                    "every task under the assistant",
                    &["漢字".repeat(60)],
                    true,
                ),
                action: crate::tui::tasks::Pending::Stop { task_id: None },
            }),
            Some(Panel::Text {
                title: "w \u{1b}[2J· worklog".into(),
                lines: vec![format!("{}\twide\u{7}", "漢字".repeat(200)); 80],
                scroll: u16::MAX,
                prs: Some("w".into()),
            }),
            Some(Panel::Text {
                title: String::new(),
                lines: Vec::new(),
                scroll: 3,
                prs: None,
            }),
            Some(Panel::ReviewMenu {
                task_id: Some("a".into()),
                pr: None,
                label: "review \u{1b}[2Japi#7".into(),
                selected: 1,
            }),
            Some(Panel::ReviewMenu {
                task_id: None,
                pr: Some(crate::tui::reviews::PrKey {
                    repo_id: "github.com/acme/api".into(),
                    number: 7,
                }),
                label: String::new(),
                selected: 0,
            }),
            Some(Panel::Answer {
                id: 42,
                title: "#42 question from task \u{1b}[2Jw".into(),
                question: vec![format!("{}\twide\u{7}", "漢字".repeat(200)); 80],
                input: "漢字".repeat(100),
                error: Some("漢字".repeat(100)),
            }),
        ]
    }

    #[test]
    fn a_long_name_gives_way_before_the_parent_marker() {
        let mut child = row(
            "fix-the-login-redirect-loop-on-safari",
            Some(Activity::Idle { since: 0 }),
        );
        child.task.parent_id = Some("assistant-id".into());
        let mut ui = TasksUi::new(vec![child]);
        let screen = render(&mut ui, 80, 12).join("\n");
        assert!(screen.contains("fix… ↳ assistant"), "{screen}");
    }

    /// A task under a manager names that manager, not the assistant.
    #[test]
    fn a_task_under_a_manager_names_its_manager() {
        let mut api = row("api", Some(Activity::Idle { since: 0 }));
        api.task.kind = TaskKind::Manager;
        api.task.parent_id = Some("assistant-id".into());
        let mut child = row("fix-x", Some(Activity::Idle { since: 0 }));
        child.task.parent_id = Some("api".into());
        let mut ui = TasksUi::new(vec![api, child]);
        let screen = render(&mut ui, 120, 12).join("\n");
        assert!(screen.contains("api (manager) ↳ assistant"), "{screen}");
        assert!(screen.contains("fix-x ↳ api"), "{screen}");
        assert!(!screen.contains("fix-x ↳ assistant"), "{screen}");
    }

    /// A manager's slug may be long; its children still show `↳`.
    #[test]
    fn a_long_manager_slug_is_shortened_inside_the_marker() {
        let mut api = row("api-migration", Some(Activity::Idle { since: 0 }));
        api.task.kind = TaskKind::Manager;
        api.task.parent_id = Some("assistant-id".into());
        let mut child = row("fix-the-login-bug", Some(Activity::Idle { since: 0 }));
        child.task.parent_id = Some("api-migration".into());
        let mut ui = TasksUi::new(vec![api, child]);
        let screen = render(&mut ui, 80, 12).join("\n");
        assert!(screen.contains("fix… ↳ api-migr…"), "{screen}");
        let screen = render(&mut ui, 160, 12).join("\n");
        assert!(
            screen.contains("fix-the-login-bug ↳ api-migration"),
            "{screen}"
        );
    }

    fn session(id: &str, primary: bool, running: bool) -> crate::assistant::Session {
        crate::assistant::Session {
            id: id.into(),
            name: format!("{id} name"),
            primary,
            last: 1,
            activity: match running {
                true => Activity::Running { detail: None },
                false => Activity::NotRunning,
            },
            window: running.then(|| "@1".to_string()),
        }
    }

    fn info() -> AssistantInfo {
        AssistantInfo {
            sessions: vec![session("plan", true, true), session("triage", false, false)],
            inbox: 2,
            todo: crate::todo::parse(
                "- [ ] !p1 fix \u{1b}[2Jit @fix-login\n- [x] shipped\n- [ ] later\n",
            )
            .views(),
            todo_error: None,
            feed: vec![FeedEntry {
                at: crate::worktrees::now_secs() - 60,
                kind: FeedKind::Event,
                what: "review requested".into(),
                text: "Widen\u{1b}]8;;x\u{7} the store".into(),
                place: "acme/api #4 → the assistant".into(),
                url: Some("https://github.com/acme/api/pull/4".into()),
            }],
            unanchored: 0,
        }
    }

    #[test]
    fn the_assistant_is_pinned_above_the_tasks() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        let screen = render(&mut ui, 140, 12);
        assert!(screen[3].contains("✦ assistant  …"), "{}", screen[3]);
        ui.assistant = Some(AssistantInfo::default());
        let screen = render(&mut ui, 140, 12);
        assert!(
            screen[3].contains("no sessions yet — c starts one"),
            "{}",
            screen[3]
        );
        ui.assistant = Some(info());
        let screen = render(&mut ui, 140, 12);
        let line = &screen[3];
        assert!(line.contains("2 sessions  ● *plan name"), "{line}");
        assert!(!line.contains("triage"), "only running sessions: {line}");
        assert!(line.contains("inbox 2 · todo 2 open"), "{line}");
        // The tasks are below it, as before.
        assert!(screen[5].contains("fix-login"), "{}", screen[5]);
    }

    #[test]
    fn the_todo_and_inbox_panels_show_what_was_read_and_nothing_it_could_do() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        ui.assistant = Some(info());
        ui.panel = Some(Panel::Todo {
            selected: 0,
            adding: None,
        });
        let screen = render(&mut ui, 120, 30).join("\n");
        assert!(screen.contains("todo · 2 open"), "{screen}");
        assert!(screen.contains("[ ] p1 fix [2Jit  @fix-login"), "{screen}");
        assert!(screen.contains("[x] shipped"), "{screen}");
        ui.panel = Some(Panel::Todo {
            selected: 0,
            adding: Some("!p2 new".into()),
        });
        assert!(render(&mut ui, 120, 30)
            .join("\n")
            .contains("new item !p2 new█"));

        ui.panel = Some(Panel::Inbox { selected: 0 });
        let screen = render(&mut ui, 120, 30).join("\n");
        assert!(screen.contains("2 queued"), "{screen}");
        assert!(
            screen
                .contains("review requested  Widen]8;;x the store  acme/api #4 → the assistant ↗"),
            "{screen}"
        );
        assert!(!screen.contains('\u{1b}'));

        ui.assistant = Some(AssistantInfo::default());
        assert!(render(&mut ui, 120, 30).join("\n").contains("nothing yet"));
        ui.panel = Some(Panel::Todo {
            selected: 0,
            adding: None,
        });
        assert!(render(&mut ui, 120, 30)
            .join("\n")
            .contains("nothing to do"));
    }

    #[test]
    fn sorting_by_priority_says_so() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        ui.by_priority = true;
        assert!(render(&mut ui, 120, 12)
            .join("\n")
            .contains("1 active task · by todo priority"));
    }

    #[test]
    fn narrow_terminals_drop_columns_rather_than_the_name() {
        for width in [20, 40, 56, 72, 90, 110, 200] {
            let c = Columns::for_width(width);
            assert!(c.name >= 6, "{width}: {c:?}");
            let used = 2 + c.status + c.worktrees + c.size + c.age + c.name;
            assert!(used <= width + 6, "{width}: {c:?}");
        }
        assert_eq!(Columns::for_width(60).worktrees, 0);
        assert!(Columns::for_width(100).worktrees > 0);
    }

    #[test]
    fn glyphs_tell_the_states_apart() {
        let mut r = row("x", None);
        assert_eq!(glyph(&r).0, "…");
        r.activity = Some(Activity::Waiting { detail: None });
        assert_eq!(glyph(&r).0, "◆");
        r.activity = Some(Activity::Idle { since: 0 });
        assert_eq!(glyph(&r).0, "○");
        r.activity = Some(Activity::NotRunning);
        assert_eq!(glyph(&r).0, "✕");
        r.task.state = TaskState::Archived;
        assert_eq!(glyph(&r).0, "▪");
        assert_eq!(status_text(&r), "archived");
    }

    fn history(number: u64, state: &str, title: &str) -> TaskPrRecord {
        TaskPrRecord {
            task_id: "fix-login".into(),
            repo_id: "github.com/acme/api".into(),
            number,
            title: title.into(),
            url: format!("https://github.com/acme/api/pull/{number}"),
            state: state.into(),
            is_draft: false,
            additions: number * 100,
            deletions: number,
            updated_at: String::new(),
        }
    }

    fn prs_panel(prs: Vec<TaskPrRecord>, selected: usize) -> Panel {
        Panel::Prs {
            task_id: "fix-login".into(),
            prs,
            selected,
            looking: false,
            note: None,
        }
    }

    #[test]
    fn the_pr_panel_lines_up_state_size_number_and_a_linked_title() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        let mut merged = history(12, "MERGED", "Widen the store");
        merged.additions = 1200;
        ui.panel = Some(prs_panel(
            vec![merged, history(3, "OPEN", "Fix \x1b[2Jit")],
            0,
        ));
        let screen = render(&mut ui, 120, 30);
        let first = screen.iter().find(|l| l.contains("Widen")).unwrap();
        let second = screen.iter().find(|l| l.contains("Fix")).unwrap();
        assert!(
            first.contains("merged  +1200 −12 #12 Widen the store"),
            "{first}"
        );
        assert!(
            second.contains("open     +300  −3 #3  Fix [2Jit"),
            "{second}"
        );

        // Each title is a link to its PR, exactly where it was drawn.
        assert_eq!(ui.links.len(), 2);
        let link = &ui.links[0];
        assert_eq!(link.url, "https://github.com/acme/api/pull/12");
        let line = &screen[link.y as usize];
        let at: String = line
            .chars()
            .skip(link.x as usize)
            .take(link.width as usize)
            .collect();
        assert_eq!(at, "Widen the store");
        // No panel, no links.
        ui.panel = None;
        render(&mut ui, 120, 30);
        assert!(ui.links.is_empty());
    }

    #[test]
    fn links_follow_the_pr_list_as_it_scrolls() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        let prs: Vec<_> = (1..=40)
            .map(|n| history(n, "OPEN", &format!("change {n}")))
            .collect();
        ui.panel = Some(prs_panel(prs, 39));
        let screen = render(&mut ui, 120, 30);
        let last = ui.links.last().unwrap();
        assert_eq!(last.url, "https://github.com/acme/api/pull/40");
        assert!(screen[last.y as usize].contains("change 40"));
        assert!(ui
            .links
            .iter()
            .all(|l| screen[l.y as usize].contains("change")));
    }

    #[test]
    fn an_empty_pr_panel_says_whether_it_is_still_looking() {
        let mut ui = TasksUi::new(vec![row("fix-login", None)]);
        ui.panel = Some(Panel::Prs {
            task_id: "fix-login".into(),
            prs: Vec::new(),
            selected: 0,
            looking: true,
            note: None,
        });
        assert!(render(&mut ui, 120, 30)
            .join("\n")
            .contains("looking for pull requests"));
        ui.prs_found("fix-login", Vec::new(), Some("gh is not installed".into()));
        let screen = render(&mut ui, 120, 30).join("\n");
        assert!(
            screen.contains("could not look them up: gh is not installed"),
            "{screen}"
        );
        ui.prs_found("fix-login", Vec::new(), None);
        assert!(render(&mut ui, 120, 30).join("\n").contains("none yet"));
    }

    #[test]
    fn pr_states_have_their_colours() {
        let mut pr = history(1, "OPEN", "x");
        assert_eq!(pr_state_color(&pr), Color::Green);
        pr.is_draft = true;
        assert_eq!(pr_state_color(&pr), Color::DarkGray);
        pr.state = "MERGED".into();
        assert_eq!(pr_state_color(&pr), Color::Magenta);
        pr.state = "CLOSED".into();
        assert_eq!(pr_state_color(&pr), Color::Red);
    }

    /// A node under `parent`.
    fn node(name: &str, kind: TaskKind, parent: &str, activity: Option<Activity>) -> TaskRow {
        let mut r = row(name, activity);
        r.task.kind = kind;
        r.task.parent_id = Some(parent.into());
        r
    }

    fn node_info(budget: Option<(i64, i64)>) -> NodeInfo {
        NodeInfo {
            run: RunState::Run,
            held: 0,
            setter: None,
            budget: budget.map(|(max, used)| {
                let row = crate::db::tasks::BudgetRow {
                    max_secs: Some(max),
                    max_turns: None,
                    used_secs: 0,
                    used_turns: 0,
                    warned: 0,
                };
                (row, (used, 0))
            }),
            inbox: Some(0),
            pr: None,
            queued: false,
            ask: None,
            done: false,
            children: 0,
        }
    }

    /// The tree of §6's mock, in the agents view: frontend paused and
    /// closed, a prompt under backend.
    fn agents_ui() -> TasksUi {
        let h = 3600;
        let mut root = row("assistant", None);
        root.task.kind = TaskKind::Assistant;
        let rows = vec![
            node(
                "backend",
                TaskKind::Manager,
                "assistant",
                Some(Activity::Running { detail: None }),
            ),
            node(
                "fix-token-refresh",
                TaskKind::Work,
                "backend",
                Some(Activity::Waiting {
                    detail: Some("Bash".into()),
                }),
            ),
            node(
                "api-docs",
                TaskKind::Work,
                "backend",
                Some(Activity::Running {
                    detail: Some("Edit".into()),
                }),
            ),
            node(
                "frontend",
                TaskKind::Manager,
                "assistant",
                Some(Activity::Idle { since: 0 }),
            ),
            node("fe-a", TaskKind::Work, "frontend", None),
            node("fe-b", TaskKind::Work, "frontend", None),
            node(
                "fix-login",
                TaskKind::Work,
                "assistant",
                Some(Activity::Idle { since: 0 }),
            ),
            // Not in the tree.
            row("top-level", Some(Activity::Idle { since: 0 })),
        ];
        let mut nodes = std::collections::HashMap::new();
        nodes.insert(
            "assistant".to_string(),
            NodeInfo {
                children: 3,
                inbox: Some(2),
                ..node_info(None)
            },
        );
        nodes.insert(
            "backend".into(),
            NodeInfo {
                inbox: Some(1),
                ..node_info(Some((6 * h, 2 * h + 600)))
            },
        );
        nodes.insert(
            "fix-token-refresh".into(),
            NodeInfo {
                pr: Some("api#131".into()),
                ..node_info(Some((2 * h, 2400)))
            },
        );
        nodes.insert("api-docs".into(), node_info(Some((h, 300))));
        let paused = |held, budget| NodeInfo {
            run: RunState::Paused,
            held,
            setter: Some("the user".into()),
            ..node_info(budget)
        };
        nodes.insert(
            "frontend".into(),
            paused(2, Some((4 * h, 3 * h + 50 * 60 + 30))),
        );
        nodes.insert("fe-a".into(), paused(0, None));
        nodes.insert("fe-b".into(), paused(0, None));
        nodes.insert(
            "fix-login".into(),
            NodeInfo {
                done: true,
                pr: Some("web#88".into()),
                ..node_info(None)
            },
        );
        let mut ui = TasksUi::new(Vec::new());
        ui.view = View::Agents;
        ui.assistant = Some(info());
        ui.tree_info = Some(crate::tui::managers::TreeInfo {
            live: [
                "assistant",
                "backend",
                "fix-token-refresh",
                "api-docs",
                "fix-login",
            ]
            .map(String::from)
            .into(),
            max_live: Some(8),
            queued: 0,
            nodes,
        });
        ui.collapsed.insert("frontend".into());
        ui.assistant_row = Some(root);
        ui.set_rows(rows);
        ui
    }

    /// The screen line that holds `what`.
    fn line_of<'a>(screen: &'a [String], what: &str) -> &'a str {
        screen
            .iter()
            .find(|l| l.contains(what))
            .unwrap_or_else(|| panic!("no {what} in {screen:#?}"))
    }

    #[test]
    fn the_agents_view_draws_the_tree_with_what_rolls_up() {
        let mut ui = agents_ui();
        let screen = render(&mut ui, 120, 20);
        let all = screen.join("\n");
        assert!(all.contains("jeet · agents"), "{all}");
        assert!(all.contains("live 5/8   needs you 1"), "{all}");
        assert!(!all.contains("queued"), "{all}");
        // The assistant is the root, not pinned above the list.
        assert!(!all.contains("C sessions"), "{all}");
        // Siblings by slug, as `manager ls` has them.
        let root = line_of(&screen, "✦ assistant");
        assert!(root.contains("◆ needs you (1)"), "{root}");
        assert!(root.contains("3 children"), "{root}");
        assert!(root.contains("inbox 2  todo 2"), "{root}");
        let backend = line_of(&screen, "├─ ▾ backend");
        assert!(backend.contains("◆ needs you (1)"), "{backend}");
        assert!(backend.contains("2h10/6h"), "{backend}");
        assert!(backend.contains("inbox 1"), "{backend}");
        let asking = line_of(&screen, "│  └─ fix-token-refresh");
        assert!(asking.contains("◆ needs input – Bash"), "{asking}");
        assert!(asking.contains("0h40/2h"), "{asking}");
        assert!(asking.contains("api#131"), "{asking}");
        let docs = line_of(&screen, "│  ├─ api-docs");
        assert!(docs.contains("● running – Edit"), "{docs}");
        assert!(docs.contains("0h05/1h"), "{docs}");
        let frontend = line_of(&screen, "└─ ▸ frontend");
        assert!(frontend.contains("⏸ paused (2 below)"), "{frontend}");
        assert!(frontend.contains("3h50/4h"), "{frontend}");
        assert!(frontend.contains("budget 96%"), "{frontend}");
        assert!(!all.contains("fe-a"), "closed: {all}");
        let login = line_of(&screen, "├─ fix-login");
        assert!(login.contains("✓ done"), "{login}");
        assert!(login.contains("web#88"), "{login}");
        assert!(!all.contains("top-level"), "{all}");
        assert!(all.contains("agents 8"), "{all}");
        assert!(all.contains("6 of 8 agents shown"), "{all}");
        // With its own session waiting and nothing below, the assistant
        // needs input itself.
        ui.assistant.as_mut().unwrap().sessions[0].activity = Activity::Waiting {
            detail: Some("Edit".into()),
        };
        ui.apply(crate::tui::tasks::Update::Status {
            id: "fix-token-refresh".into(),
            activity: Some(Activity::Idle { since: 0 }),
            last_activity: 0,
            unanchored: false,
        });
        let screen = render(&mut ui, 120, 20);
        let root = line_of(&screen, "✦ assistant");
        assert!(root.contains("◆ needs input – Edit"), "{root}");
        assert!(screen.join("\n").contains("needs you 1"));
    }

    #[test]
    fn a_question_with_the_user_needs_you_on_its_asker_and_above() {
        let mut ui = agents_ui();
        // The prompt goes; api-docs's question is passed to the user.
        ui.apply(crate::tui::tasks::Update::Status {
            id: "fix-token-refresh".into(),
            activity: Some(Activity::Idle { since: 0 }),
            last_activity: 0,
            unanchored: false,
        });
        let all = render(&mut ui, 120, 20).join("\n");
        assert!(!all.contains("needs you"), "{all}");
        ui.tree_info
            .as_mut()
            .unwrap()
            .nodes
            .get_mut("api-docs")
            .unwrap()
            .ask = Some((42, ReportKind::Question, true));
        ui.retree();
        let screen = render(&mut ui, 120, 20);
        assert!(screen.join("\n").contains("needs you 1"), "{screen:#?}");
        let docs = line_of(&screen, "│  ├─ api-docs");
        assert!(docs.contains("◆ needs you – question"), "{docs}");
        let backend = line_of(&screen, "├─ ▾ backend");
        assert!(backend.contains("◆ needs you (1)"), "{backend}");
        let root = line_of(&screen, "✦ assistant");
        assert!(root.contains("◆ needs you (1)"), "{root}");
    }

    #[test]
    fn an_asker_at_a_prompt_counts_once() {
        let mut ui = agents_ui();
        ui.tree_info
            .as_mut()
            .unwrap()
            .nodes
            .get_mut("fix-token-refresh")
            .unwrap()
            .ask = Some((42, ReportKind::Question, true));
        ui.retree();
        let all = render(&mut ui, 120, 20).join("\n");
        assert!(all.contains("needs you 1"), "{all}");
        assert!(!all.contains("needs you 2"), "{all}");
    }

    #[test]
    fn a_long_question_says_how_to_see_the_rest() {
        let mut ui = agents_ui();
        ui.panel = Some(Panel::Answer {
            id: 42,
            title: "#42 question".into(),
            question: (0..40).map(|n| format!("line {n}")).collect(),
            input: String::new(),
            error: None,
        });
        let all = render(&mut ui, 70, 24).join("\n");
        assert!(all.contains("esc, then i shows the rest"), "{all}");
    }

    #[test]
    fn the_answer_form_shows_the_question_without_its_escapes() {
        let mut ui = agents_ui();
        ui.panel = Some(Panel::Answer {
            id: 42,
            title: "#42 question from task assistant/backend/api-docs · with you".into(),
            question: vec!["keep v1?\u{1b}[2J or drop it".into()],
            input: "drop it".into(),
            error: Some("#42 was answered by the user".into()),
        });
        let all = render(&mut ui, 70, 24).join("\n");
        assert!(all.contains("#42 question from task"), "{all}");
        assert!(all.contains("api-docs"), "{all}");
        assert!(all.contains("keep v1?[2J or drop it"), "{all}");
        assert!(!all.contains('\u{1b}'), "{all}");
        assert!(all.contains("answer   drop it"), "{all}");
        assert!(all.contains("#42 was answered by the user"), "{all}");
        assert!(all.contains("⏎ answer as you · esc cancel"), "{all}");
    }

    #[test]
    fn a_held_node_still_shows_a_prompt_below_it() {
        let mut ui = agents_ui();
        ui.collapsed.clear();
        ui.set_rows(ui.rows.clone());
        ui.apply(crate::tui::tasks::Update::Status {
            id: "fe-a".into(),
            activity: Some(Activity::Waiting { detail: None }),
            last_activity: 0,
            unanchored: false,
        });
        let screen = render(&mut ui, 140, 20);
        let frontend = line_of(&screen, "└─ ▾ frontend");
        assert!(
            frontend.contains("⏸ paused (2 below) · needs you (1)"),
            "{frontend}"
        );
        let fe_a = line_of(&screen, "   ├─ fe-a");
        assert!(fe_a.contains("⏸ paused · needs input"), "{fe_a}");
        assert!(render(&mut ui, 140, 20).join("\n").contains("needs you 2"));
    }

    #[test]
    fn a_node_not_gathered_yet_draws_its_activity_alone() {
        let mut ui = agents_ui();
        ui.tree_info.as_mut().unwrap().nodes.remove("api-docs");
        ui.retree();
        let screen = render(&mut ui, 120, 20);
        let docs = line_of(&screen, "│  ├─ api-docs");
        assert!(docs.contains("● running – Edit"), "{docs}");
        assert!(!docs.contains('/'), "no budget: {docs}");
        assert!(
            docs.trim_end_matches(['│', ' '])
                .ends_with("running – Edit"),
            "no notes: {docs}"
        );
        // Nothing gathered at all: no live cell, and every node still drawn.
        ui.tree_info = None;
        ui.retree();
        let all = render(&mut ui, 120, 20).join("\n");
        assert!(!all.contains("live "), "{all}");
        assert!(all.contains("needs you 1"), "{all}");
        assert!(line_of(&render(&mut ui, 120, 20), "└─ ▸ frontend").contains("○ idle"));
    }

    #[test]
    fn a_narrow_tree_drops_the_notes_then_the_budget_before_the_status() {
        let mut ui = agents_ui();
        let screen = render(&mut ui, 72, 20);
        let asking = line_of(&screen, "fix-token-refresh");
        assert!(asking.contains("needs input – Bash"), "{asking}");
        assert!(asking.contains("0h40/2h"), "{asking}");
        assert!(!asking.contains("api#131"), "{asking}");
        let screen = render(&mut ui, 60, 20);
        let asking = line_of(&screen, "fix-token-refresh");
        assert!(asking.contains("needs input – Bash"), "{asking}");
        assert!(!asking.contains("0h40"), "{asking}");
        for width in [20, 44, 60, 64, 80, 120, 200] {
            let c = TreeColumns::for_width(width);
            let used = c.name + c.status + c.budget + c.notes;
            assert!(c.name >= 6, "{width}: {c:?}");
            assert!(used <= width.max(18), "{width}: {c:?}");
        }
    }

    #[test]
    fn nothing_an_agent_wrote_reaches_the_terminal_raw() {
        let mut ui = agents_ui();
        let mut rows = ui.rows.clone();
        let asking = rows
            .iter_mut()
            .find(|r| r.task.id == "fix-token-refresh")
            .unwrap();
        asking.task.slug = "evil\u{1b}[2J\u{7}".into();
        ui.set_rows(rows);
        ui.apply(crate::tui::tasks::Update::Status {
            id: "fix-token-refresh".into(),
            activity: Some(Activity::Waiting {
                detail: Some("\u{1b}[2JBash\u{7}".into()),
            }),
            last_activity: 0,
            unanchored: false,
        });
        let screen = render(&mut ui, 120, 20).join("\n");
        assert!(screen.contains("evil[2J"), "{screen}");
        assert!(screen.contains("needs input – [2JBash"), "{screen}");
        assert!(
            !screen.contains('\u{1b}') && !screen.contains('\u{7}'),
            "{screen}"
        );

        ui.user = true;
        ui.panel = Some(Panel::Text {
            title: "manager backend · worklog".into(),
            lines: vec![
                "## \u{1b}[2Jtoday\u{7}".into(),
                "a\tb".into(),
                "    #12 question · body\u{1b}]8;;x\u{7}".into(),
            ],
            scroll: 0,
            prs: None,
        });
        let screen = render(&mut ui, 120, 30).join("\n");
        assert!(screen.contains("manager backend · worklog"), "{screen}");
        assert!(screen.contains("## [2Jtoday"), "{screen}");
        assert!(
            screen.contains("a       b"),
            "a tab to the next stop: {screen}"
        );
        assert!(screen.contains("#12 question · body]8;;x"), "{screen}");
        assert!(
            !screen.contains('\u{1b}') && !screen.contains('\u{7}'),
            "{screen}"
        );
    }

    #[test]
    fn a_text_panel_scrolls_no_further_than_its_end() {
        let mut ui = agents_ui();
        ui.panel = Some(Panel::Text {
            title: "t".into(),
            lines: (1..=100).map(|n| format!("line {n}")).collect(),
            scroll: u16::MAX,
            prs: Some("backend".into()),
        });
        let screen = render(&mut ui, 100, 30).join("\n");
        assert!(screen.contains("line 100"), "{screen}");
        assert!(!screen.contains("line 1 "), "{screen}");
        assert!(screen.contains("⏎ its pull requests"), "{screen}");
        let Some(Panel::Text { scroll, .. }) = &ui.panel else {
            panic!();
        };
        // 30 rows at 80%, less the border.
        assert_eq!(*scroll, 100 - 22);
        // A long line wraps, and counts as the lines it takes.
        assert_eq!(text_lines(&["abcdef".into()], 4), ["abcd", "ef"]);
        assert_eq!(
            text_lines(&["\t漢字".into(), String::new()], 9),
            ["        ", "漢字", ""]
        );
    }

    #[test]
    fn the_help_lists_the_keys_of_the_view() {
        let mut ui = agents_ui();
        ui.panel = Some(Panel::Help { scroll: 0 });
        let screen = render(&mut ui, 120, 60).join("\n");
        assert!(screen.contains("the worklog"), "{screen}");
        assert!(!screen.contains("explore the task"), "{screen}");
        assert!(help(View::Active).iter().any(|(k, _)| *k == "e"));
    }

    #[test]
    fn hints_always_keep_help_and_quit() {
        for width in [10u16, 20, 40, 80, 200] {
            let line = hints(width, View::Active);
            assert!(line.width() <= width as usize, "{width}: {line}");
        }
        assert!(hints(20, View::Active).contains("q quit"));
        assert!(hints(200, View::Active).contains("⏎ open"));
        assert!(hints(200, View::Agents).contains("l worklog"));
        assert!(hints(200, View::Reviews).contains("⏎ menu"));
        assert!(help(View::Reviews).iter().any(|(k, _)| *k == "v"));
        assert!(!hints(200, View::Agents).contains("archive"));
    }
}
