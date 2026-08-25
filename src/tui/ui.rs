//! Rendering for the explorer.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::state::{human_size, Explorer, Overlay};
use crate::worktrees::WorktreeStatus;

const HINTS: &str =
    "↑↓ move  → open  ← back  ⏎ edit  w worktrees  s sessions  c agent  . hidden  ? help  q quit";

pub fn draw(frame: &mut Frame, explorer: &mut Explorer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], explorer);
    {
        // Split the borrow: the list widget needs its scroll state mutably
        // while the entries it renders are borrowed immutably.
        let Explorer {
            entries,
            selected,
            list,
            ..
        } = &mut *explorer;
        draw_listing(frame, chunks[1], entries, *selected, list);
    }

    let status = Paragraph::new(Line::from(Span::styled(
        explorer.status_line.clone(),
        Style::default().fg(Color::Yellow),
    )))
    .wrap(Wrap { trim: true });
    frame.render_widget(status, chunks[2]);

    let hints = Paragraph::new(Line::from(Span::styled(
        HINTS,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(hints, chunks[3]);

    if let Some(overlay) = &explorer.overlay {
        draw_overlay(frame, explorer, overlay);
    }
    if let Some(working) = &explorer.working {
        draw_working(frame, working);
    }
}

/// A small banner shown while a background job runs, so a slow push or a big
/// worktree scan never looks like a hang.
fn draw_working(frame: &mut Frame, working: &str) {
    let area = content_rect(44, 1, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            working.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_header(frame: &mut Frame, area: Rect, explorer: &Explorer) {
    let worktree_line = Line::from(vec![
        Span::styled("worktree ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            explorer.root_label.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", explorer.root_kind),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("  "),
        status_span(&explorer.root_status),
        Span::raw("  "),
        Span::styled(
            explorer.root_status.diff_summary(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" vs {}", explorer.repo.default_branch),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let path_line = Line::from(vec![
        Span::styled("path     ", Style::default().fg(Color::DarkGray)),
        Span::styled(explorer.breadcrumb(), Style::default().fg(Color::Cyan)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" jeet · {} ", explorer.repo.id),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Left);

    frame.render_widget(
        Paragraph::new(vec![worktree_line, path_line]).block(block),
        area,
    );
}

pub fn status_span(status: &WorktreeStatus) -> Span<'static> {
    // "Could not tell" must never render as "clean" — that is the reading that
    // makes a worktree look safe to delete.
    if status.unknown.is_some() {
        return Span::styled("unknown", Style::default().fg(Color::Red));
    }
    if status.dirty > 0 {
        Span::styled(
            format!("{} uncommitted", status.dirty),
            Style::default().fg(Color::Red),
        )
    } else if status.ahead > 0 || status.behind > 0 {
        Span::styled(
            format!("↑{} ↓{}", status.ahead, status.behind),
            Style::default().fg(Color::Blue),
        )
    } else {
        Span::styled("clean", Style::default().fg(Color::Green))
    }
}

fn draw_listing(
    frame: &mut Frame,
    area: Rect,
    entries: &[super::state::FsEntry],
    selected: usize,
    list_state: &mut ListState,
) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = entries
        .iter()
        .map(|entry| {
            let (marker, style) = if entry.is_dir {
                ("▸ ", Style::default().fg(Color::Cyan))
            } else {
                ("  ", Style::default())
            };
            let name = entry.display_name();
            let size = if entry.is_dir {
                String::new()
            } else {
                human_size(entry.size)
            };
            let used = marker.chars().count() + name.chars().count() + size.chars().count();
            let pad = width.saturating_sub(used).max(1);
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(name, style),
                Span::raw(" ".repeat(pad)),
                Span::styled(size, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let title = if entries.is_empty() {
        " empty directory ".to_string()
    } else {
        format!(" {} items ", entries.len())
    };

    list_state.select(if entries.is_empty() {
        None
    } else {
        Some(selected.min(entries.len() - 1))
    });

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, list_state);
}

fn draw_overlay(frame: &mut Frame, explorer: &Explorer, overlay: &Overlay) {
    match overlay {
        Overlay::Worktrees { selected } => {
            let rows = &explorer.worktree_rows;
            let area = centered_rect(80, 70, frame.area());
            frame.render_widget(Clear, area);
            let items: Vec<ListItem> = rows
                .iter()
                .map(|row| {
                    let marker = if row.current { "● " } else { "  " };
                    let mut spans = vec![
                        Span::styled(marker, Style::default().fg(Color::Green)),
                        Span::styled(
                            format!("{:<26}", truncate(&row.entry.display_name(), 26)),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(
                            format!("{:<10}", row.entry.kind.label()),
                            Style::default().fg(Color::Magenta),
                        ),
                        status_span(&row.status),
                        Span::raw("  "),
                        Span::styled(
                            row.status.diff_summary(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];
                    if row.entry.missing {
                        spans.push(Span::styled(
                            "  MISSING",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();

            let mut state = ListState::default();
            state.select(if rows.is_empty() {
                None
            } else {
                Some((*selected).min(rows.len() - 1))
            });
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" worktrees · ⏎ switch  n new  e detached  m rename  d delete ")
                        .title_style(Style::default().fg(Color::Green)),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Sessions { sessions, selected } => {
            let area = centered_rect(85, 70, frame.area());
            frame.render_widget(Clear, area);
            let items: Vec<ListItem> = sessions
                .iter()
                .map(|session| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:<10}", session.age()),
                            Style::default().fg(Color::Blue),
                        ),
                        Span::styled(
                            format!("{:<9}", format!("{} msgs", session.entries)),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(truncate(&session.summary, 60)),
                    ]))
                })
                .collect();
            let mut state = ListState::default();
            state.select(if sessions.is_empty() {
                None
            } else {
                Some((*selected).min(sessions.len() - 1))
            });
            let title = format!(
                " {} sessions · ⏎ resume  esc close ",
                explorer.agent.display()
            );
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .title_style(Style::default().fg(Color::Green)),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_stateful_widget(list, area, &mut state);
        }
        Overlay::RenameWorktree { index, input } => {
            let area = centered_rect(64, 30, frame.area());
            frame.render_widget(Clear, area);
            let current = explorer
                .worktree_rows
                .get(*index)
                .map(|row| row.entry.display_name())
                .unwrap_or_default();
            let detached = explorer
                .worktree_rows
                .get(*index)
                .map(|row| row.entry.branch.is_none())
                .unwrap_or(false);
            let explain = if detached {
                "⏎ creates this branch at the scratchpad's HEAD and keeps your work"
            } else {
                "⏎ renames the branch and moves the worktree to match"
            };
            let body = vec![
                Line::from(vec![
                    Span::styled("renaming ", Style::default().fg(Color::DarkGray)),
                    Span::styled(current, Style::default().fg(Color::Magenta)),
                ]),
                Line::from(vec![
                    Span::styled("to       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        input.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(Span::styled(explain, Style::default().fg(Color::DarkGray))),
                Line::from(Span::styled(
                    "ctrl-u clear · esc cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" rename worktree ")
                            .title_style(Style::default().fg(Color::Green)),
                    )
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::NewWorktree { input } => {
            let area = centered_rect(60, 25, frame.area());
            frame.render_widget(Clear, area);
            let body = vec![
                Line::from(vec![
                    Span::styled("branch ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        input.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "⏎ create and publish to origin · empty name creates a detached checkout",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "esc cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" new worktree ")
                            .title_style(Style::default().fg(Color::Green)),
                    )
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::Confirm {
            title,
            lines,
            index: _,
            action: _,
        } => {
            // Sized to its content: a destructive prompt that silently clips
            // its own warnings (or its y/n line) is worse than no prompt.
            let area = content_rect(66, lines.len() + 4, frame.area());
            frame.render_widget(Clear, area);
            let mut body: Vec<Line> = lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect();
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "y remove · f force (discard the above) · n cancel",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(
                Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {title} "))
                            .title_style(Style::default().fg(Color::Red)),
                    )
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
        Overlay::Help => {
            let area = centered_rect(64, 70, frame.area());
            frame.render_widget(Clear, area);
            let rows = [
                ("↑ / k, ↓ / j", "move up and down this level"),
                ("→ / l", "expand: enter the highlighted folder"),
                ("← / h", "back: leave the folder (stops at the root)"),
                ("⏎", "folder: enter · file: open in your editor"),
                ("c", "start a coding agent at the worktree root"),
                ("s", "previous agent sessions for this worktree"),
                ("w", "worktrees: switch, create, rename or delete"),
                ("", "  in the panel: r refresh, esc close"),
                (".", "toggle hidden files"),
                ("g / G", "jump to the top / bottom"),
                ("r", "refresh the listing and counters"),
                ("q", "quit, leaving the shell in this directory"),
                ("esc", "quit without moving the shell"),
            ];
            let body: Vec<Line> = rows
                .iter()
                .map(|(key, description)| {
                    Line::from(vec![
                        Span::styled(
                            format!("{key:<14}"),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(*description),
                    ])
                })
                .collect();
            frame.render_widget(
                Paragraph::new(body).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" keys · esc close ")
                        .title_style(Style::default().fg(Color::Green)),
                ),
                area,
            );
        }
        Overlay::Message {
            title,
            lines,
            from_panel: _,
        } => {
            let area = content_rect(64, lines.len() + 4, frame.area());
            frame.render_widget(Clear, area);
            let mut body: Vec<Line> = lines
                .iter()
                .map(|l| Line::from(Span::raw(l.clone())))
                .collect();
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "esc close",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(
                Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {title} "))
                            .title_style(Style::default().fg(Color::Yellow)),
                    )
                    .wrap(Wrap { trim: true }),
                area,
            );
        }
    }
}

/// Truncate from the left, keeping the tail — right for paths.
pub fn truncate_start(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - width.saturating_sub(1)).collect();
    format!("…{tail}")
}

pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

/// A centered box `lines` rows tall (plus borders), clamped to the frame.
fn content_rect(percent_x: u16, lines: usize, area: Rect) -> Rect {
    let wanted = (lines as u16).saturating_add(2);
    let height = wanted.min(area.height);
    let top = area.height.saturating_sub(height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }

    /// The panel row must not call an unassessable worktree "clean".
    #[test]
    fn unknown_status_is_never_rendered_as_clean() {
        let unknown = WorktreeStatus {
            unknown: Some("could not compare against origin/main".into()),
            ..WorktreeStatus::default()
        };
        assert_eq!(status_span(&unknown).content, "unknown");
        assert_eq!(status_span(&WorktreeStatus::default()).content, "clean");
    }

    #[test]
    fn truncates_paths_from_the_left() {
        assert_eq!(truncate_start("/a/b", 10), "/a/b");
        assert_eq!(truncate_start("/very/long/path/file", 10), "…path/file");
    }
}
