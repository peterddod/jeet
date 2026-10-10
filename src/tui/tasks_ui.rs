//! Rendering for the tasks list.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::state::human_size;
use super::tasks::{AddField, Field, NewTaskForm, Panel, Picker, TaskRow, TasksUi, View};
use super::term::Link;
use super::ui::{centered_rect, content_rect, draw_working, fitted_rect, truncate};
use crate::agent::age_label;
use crate::commands::task::printable;
use crate::db::tasks::{TaskKind, TaskPrRecord, TaskState};
use crate::pr_history;
use crate::status::Activity;
use crate::worktrees::WorktreeStatus;

/// The key list, as `?` shows it.
pub const HELP: [(&str, &str); 22] = [
    ("⏎", "open the task's agent in tmux (C-b d comes back here)"),
    ("e", "explore the task: its canvas or one of its worktrees"),
    ("w", "worktrees: n adds one, D removes one, ⏎ explores it"),
    (
        "p",
        "the task's pull requests; ⏎ or o opens one in the browser",
    ),
    ("v", "a review task's pull request in the review viewer"),
    (
        "n",
        "new task: blank for an empty canvas, or a project/repos",
    ),
    (
        "a",
        "archive (frees the disk) · unarchive, in the archived view",
    ),
    ("D", "delete the task, after listing any work that would go"),
    ("S", "copy the task's project files in again"),
    ("P", "projects: n new, e edit, D delete, ⏎ start a task"),
    ("", ""),
    ("⇥", "switch between active and archived tasks"),
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

/// Width of the key column in the help panel.
const KEY_WIDTH: usize = 10;

const HINTS: [&str; 13] = [
    "⏎ open",
    "e explore",
    "w worktrees",
    "n new",
    "a archive",
    "D delete",
    "p PRs",
    "P projects",
    "S resync",
    "/ filter",
    "⇥ archived",
    "? help",
    "q quit",
];

/// As many hints as fit, always ending with help and quit.
fn hints(width: u16) -> String {
    let width = width as usize;
    let tail = &HINTS[HINTS.len() - 2..];
    let head = &HINTS[..HINTS.len() - 2];
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], ui);
    ui.list_area = chunks[1];
    draw_list(frame, chunks[1], ui);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            ui.status_line.clone(),
            Style::default().fg(Color::Yellow),
        ))),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints(chunks[3].width),
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[3],
    );

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
        tab(View::Archived, "archived"),
        Span::raw("   "),
    ];
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
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " jeet · tasks ",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_list(frame: &mut Frame, area: Rect, ui: &mut TasksUi) {
    let which = match ui.view {
        View::Active => "active",
        View::Archived => "archived",
    };
    let total = ui.count(ui.view);
    let title = match (ui.filter.is_empty(), ui.visible.len()) {
        (true, 0) => format!(" no {which} tasks "),
        (true, n) => format!(" {n} {which} task{} ", if n == 1 { "" } else { "s" }),
        (false, 0) => format!(" nothing matches \"{}\" ", ui.filter),
        (false, n) => format!(" {n} of {total} match \"{}\" ", ui.filter),
    };
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
    let items: Vec<ListItem> = ui
        .visible
        .iter()
        .map(|&i| ListItem::new(row_line(&ui.rows[i], &columns)))
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
    match &row.activity {
        None => ("…", Color::DarkGray),
        Some(Activity::Running { .. } | Activity::Live) => ("●", Color::Green),
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
    match &row.activity {
        // Not polled yet: the glyph says so, and a blank beats a guess.
        None => String::new(),
        Some(activity) => activity.label(),
    }
}

fn row_line(row: &TaskRow, columns: &Columns) -> Line<'static> {
    let (glyph, color) = glyph(row);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{} ", cell(&status_text(row), columns.status)),
            Style::default().fg(color),
        ),
    ];
    let name = match row.task.kind {
        TaskKind::Review => format!("{} (review)", row.task.name),
        TaskKind::Work => row.task.name.clone(),
    };
    spans.push(Span::styled(
        cell(&name, columns.name),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
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
            let lines: Vec<Line> = HELP
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
            let widest = HELP
                .iter()
                .map(|(_, what)| KEY_WIDTH + what.width())
                .max()
                .unwrap_or(0);
            let area = fitted_rect(widest, HELP.len(), full);
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
                super::tasks::Pending::Archive { .. } => "y archive · n keep it as it is",
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
            let area = content_rect(74, body.len() + 1, full);
            render_text(frame, area, " new task ".into(), Color::Green, body);
        }
        Panel::Projects { lines, selected } => {
            let area = centered_rect(85, 60, full);
            let title = " projects · ⏎ new task  n new  e edit  D delete  r refresh ".to_string();
            if lines.is_empty() {
                let body = vec![
                    Line::from("no projects yet"),
                    Line::from(""),
                    Line::from(dim(
                        "a project presets repositories, CLAUDE.md and skills for new tasks;",
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
        Panel::NewProject { input } => {
            let area = content_rect(60, 4, full);
            let body = vec![
                field_line("name", input, true, ""),
                Line::from(""),
                Line::from(dim("letters, digits, - _ and . · ⏎ create · esc back")),
            ];
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
            let title =
                format!(" {name} · pull requests{looking} · ⏎/o open in the browser  esc close ");
            return draw_prs(frame, area, title, prs, *selected);
        }
    }
    Vec::new()
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
    match pr_history::state_label(pr) {
        "open" => Color::Green,
        "draft" => Color::DarkGray,
        "merged" => Color::Magenta,
        _ => Color::Red,
    }
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
    if picker.items.is_empty() {
        return vec![Line::from(dim(
            "  no repositories in the store yet — `jeet clone <url>` adds one",
        ))];
    }
    if picker.visible.is_empty() {
        return vec![Line::from(dim(format!(
            "  nothing matches \"{}\"",
            picker.filter
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

fn new_task_lines(form: &NewTaskForm) -> Vec<Line<'static>> {
    let focus = form.field;
    let project = match form.project_name() {
        Some(name) => format!("‹ {name} ›"),
        None => "‹ none ›".to_string(),
    };
    let chosen = form.repos.chosen();
    let repos_value = match chosen.as_slice() {
        [] => String::new(),
        repos => repos
            .iter()
            .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut lines = vec![
        field_line(
            "name",
            &form.name,
            focus == Field::Name,
            &format!("blank: {}", form.default_name),
        ),
        Line::from(vec![
            Span::styled(
                format!("{:<9}", "project"),
                if focus == Field::Project {
                    Style::default().fg(Color::Magenta)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(project, Style::default().fg(Color::Yellow)),
            dim(if form.projects.is_empty() {
                "  no projects yet (P makes one)"
            } else {
                "  ←→ choose"
            }),
        ]),
        field_line(
            "repos",
            &repos_value,
            focus == Field::Repos,
            "none: the agent adds worktrees as it goes",
        ),
    ];
    if focus == Field::Repos {
        let filter = if form.repos.filter.is_empty() {
            "type to narrow · space marks".to_string()
        } else {
            format!("matching \"{}\"", form.repos.filter)
        };
        lines.push(Line::from(dim(format!("         {filter}"))));
        lines.extend(picker_lines(&form.repos, true, 8));
    }
    lines.push(field_line(
        "branch",
        &form.branch,
        focus == Field::Branch,
        "only with a single repository",
    ));
    lines.push(Line::from(""));
    if let Some(error) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("⚠ {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(dim(
        "⇥ next field · ⏎ create · esc cancel · all blank: an empty canvas",
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tasks::{TaskRecord, TaskWorktreeRecord};
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
            }],
            activity,
            last_activity: crate::worktrees::now_secs() - 120,
            size: Some(3 * 1024 * 1024),
        }
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
    fn every_width_and_panel_draws_without_panicking() {
        let mut ui = TasksUi::new(vec![
            row("a", Some(Activity::Waiting { detail: None })),
            row("b", None),
        ]);
        let panels = vec![
            None,
            Some(Panel::Help { scroll: 3 }),
            Some(Panel::NewTask(NewTaskForm::new(
                "task 1".into(),
                vec!["web".into()],
                None,
                vec!["github.com/acme/api".into()],
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
        ];
        for panel in panels {
            ui.panel = panel;
            for (w, h) in [(160, 40), (80, 24), (50, 12), (20, 6), (1, 1)] {
                render(&mut ui, w, h);
            }
        }
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

    #[test]
    fn hints_always_keep_help_and_quit() {
        for width in [10u16, 20, 40, 80, 200] {
            let line = hints(width);
            assert!(line.width() <= width as usize, "{width}: {line}");
        }
        assert!(hints(20).contains("q quit"));
        assert!(hints(200).contains("⏎ open"));
    }
}
