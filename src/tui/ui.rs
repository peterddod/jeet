//! Rendering for the explorer.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::state::{human_size, Explorer, Overlay};
use crate::worktrees::WorktreeStatus;

/// The hint row, as segments. It is one non-wrapping line, so what does not
/// fit is not shortened but cut off — and the two that would go first are the
/// ones that replaced keys people knew. So the head and the tail always stay,
/// and the middle is dropped from the right until the rest fits.
const HINT_HEAD: &str = "type to filter";
const HINT_MIDDLE: [&str; 8] = [
    "⇥ complete",
    "/ enter",
    "↑↓ move",
    "⏎ open",
    "^w worktrees",
    "^s sessions",
    "^a agent",
    "^d hidden",
];
const HINT_TAIL: [&str; 2] = ["F1 help", "^q quit"];

/// The most hints that fit `width`, or the head and tail alone if none do.
fn hints(width: u16) -> String {
    let mut line = String::new();
    for keep in (0..=HINT_MIDDLE.len()).rev() {
        line = std::iter::once(HINT_HEAD)
            .chain(HINT_MIDDLE[..keep].iter().copied())
            .chain(HINT_TAIL)
            .collect::<Vec<_>>()
            .join("  ");
        if line.width() <= width as usize {
            break;
        }
    }
    line
}

/// Column the header's value column starts at: one for the border, plus the
/// width of the widest label. Clicks on the breadcrumb are measured from here.
const LABEL_WIDTH: u16 = 9;

/// Width of the key column in the help table.
const KEY_WIDTH: usize = 13;

pub fn draw(frame: &mut Frame, explorer: &mut Explorer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], explorer);
    // Remember where things landed so a click next frame can be mapped back to
    // the row or the path segment under it.
    explorer.list_area = chunks[1];
    // Only when the header actually got its three rows: ratatui shrinks a
    // `Length` constraint on a short terminal, and row 2 is then the header's
    // bottom border, or a listing row — clicking either must not navigate.
    explorer.breadcrumb_origin =
        (chunks[0].height >= 4).then(|| (chunks[0].x + 1 + LABEL_WIDTH, chunks[0].y + 2));
    {
        // Split the borrow: the list widget needs its scroll state mutably
        // while the entries it renders are borrowed immutably.
        let Explorer {
            entries,
            matches,
            selected,
            filter,
            list,
            ..
        } = &mut *explorer;
        let rows: Vec<&super::state::FsEntry> =
            matches.iter().filter_map(|&i| entries.get(i)).collect();
        draw_listing(
            frame,
            chunks[1],
            &rows,
            entries.len(),
            filter,
            *selected,
            list,
        );
    }

    let status = Paragraph::new(Line::from(Span::styled(
        explorer.status_line.clone(),
        Style::default().fg(Color::Yellow),
    )))
    .wrap(Wrap { trim: true });
    frame.render_widget(status, chunks[2]);

    let hints = Paragraph::new(Line::from(Span::styled(
        hints(chunks[3].width),
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

    // The filter always has a line of its own, cursor and all: it is live from
    // the moment the explorer opens, and nothing else says so.
    let filter_line = if explorer.filter.is_empty() {
        Line::from(vec![
            Span::styled("filter   ", Style::default().fg(Color::DarkGray)),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " type to narrow this level down",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("filter   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                explorer.filter.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("  {} of {}", explorer.visible_len(), explorer.entries.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };

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
        Paragraph::new(vec![worktree_line, path_line, filter_line]).block(block),
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
    entries: &[&super::state::FsEntry],
    total: usize,
    filter: &str,
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

    let title = match (entries.is_empty(), filter.is_empty()) {
        (true, true) => " empty directory ".to_string(),
        (true, false) => format!(" nothing matches \"{filter}\" "),
        (false, true) => format!(" {} items ", entries.len()),
        (false, false) => format!(" {} of {total} items ", entries.len()),
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
        Overlay::Help { scroll } => {
            let (area, max_scroll) = help_geometry(frame.area());
            frame.render_widget(Clear, area);
            let title = if max_scroll > 0 {
                " keys · ↑↓ scroll · esc close "
            } else {
                " keys · esc close "
            };
            frame.render_widget(
                Paragraph::new(help_body(area.width.saturating_sub(2)))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .title_style(Style::default().fg(Color::Green)),
                    )
                    .scroll(((*scroll).min(max_scroll), 0)),
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

/// The key list, as it is both rendered and measured.
const HELP: [(&str, &str); 22] = [
    (
        "a-z, 0-9, …",
        "type to filter this level (live, no key needed)",
    ),
    ("⇥", "complete the filter, as far as the matches agree"),
    ("/ or \\", "enter the folder the filter names"),
    ("⌫ / ctrl-u", "delete a character / clear the filter"),
    ("", ""),
    ("↑ ↓", "move up and down · PgUp/PgDn ten rows"),
    ("→", "expand: enter the highlighted folder"),
    ("←", "back: leave the folder (stops at the root)"),
    ("⏎", "folder: enter · file: open in your editor"),
    ("click", "a row to enter or select it, a path crumb to jump"),
    ("Home / End", "jump to the top / bottom"),
    ("", ""),
    ("ctrl-a", "start a coding agent at the worktree root"),
    ("ctrl-s", "previous agent sessions for this worktree"),
    ("ctrl-w", "worktrees: switch, create, rename or delete"),
    ("", "  in the panel: r refresh, esc close"),
    ("ctrl-d", "toggle hidden dotfiles"),
    ("ctrl-r", "refresh the listing and counters"),
    ("ctrl-q", "quit, leaving the shell in this directory"),
    ("F1 / ctrl-g", "this list"),
    ("esc", "clear the filter, or quit without moving the shell"),
    ("", ""),
];

/// The key list laid out for a panel `width` columns wide inside its border.
///
/// The wrapping is ours rather than `Wrap`'s so that the number of lines is
/// known exactly: the panel scrolls, and a scroll limit computed from a
/// different idea of where the lines break leaves the last rows unreachable.
/// It also lets a continuation line hang under the description rather than
/// restarting in the key column.
pub fn help_body(width: u16) -> Vec<Line<'static>> {
    let width = (width as usize).max(1);
    // Below this there is no room for a description beside its key, so the key
    // takes a line of its own and the description follows, indented.
    let two_column = width >= KEY_WIDTH + 16;
    let indent = if two_column { KEY_WIDTH } else { 2 };
    let mut lines = Vec::new();
    for (key, description) in HELP {
        if key.is_empty() && description.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        let key_span = || {
            Span::styled(
                format!("{key:<width$}", width = KEY_WIDTH),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        };
        let mut rest = wrap_columns(description, width.saturating_sub(indent).max(1));
        if !two_column {
            lines.push(Line::from(vec![key_span()]));
        } else {
            let first = if rest.is_empty() {
                String::new()
            } else {
                rest.remove(0)
            };
            lines.push(Line::from(vec![key_span(), Span::raw(first)]));
        }
        for line in rest {
            lines.push(Line::from(format!("{}{line}", " ".repeat(indent))));
        }
    }
    lines
}

/// Greedy word wrap, breaking a word that is wider than the line itself.
fn wrap_columns(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        for piece in split_word(word, width) {
            let sep = usize::from(!current.is_empty());
            if !current.is_empty() && current.width() + sep + piece.width() > width {
                lines.push(std::mem::take(&mut current));
            } else if sep == 1 {
                current.push(' ');
            }
            current.push_str(&piece);
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// A word split into chunks no wider than `width`, so one long token cannot
/// push a line past the panel.
fn split_word(word: &str, width: usize) -> Vec<String> {
    if word.width() <= width {
        return vec![word.to_string()];
    }
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for c in word.chars() {
        if chunk.width() + c.width().unwrap_or(0) > width {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Where the help panel goes, and how far it can scroll there.
///
/// Sized to the table rather than to a share of the frame: a key list that
/// clips its own descriptions explains nothing. When even that will not fit
/// the rows wrap, which makes the list longer than the box — hence the scroll,
/// so a small terminal loses nothing, only shows it a screen at a time.
pub fn help_geometry(frame: Rect) -> (Rect, u16) {
    let widest = HELP
        .iter()
        .map(|(_, description)| KEY_WIDTH + description.width())
        .max()
        .unwrap_or(0);
    let area = fitted_rect(widest, HELP.len(), frame);
    // Counted from the very lines that will be drawn, so the limit cannot
    // disagree with them and strand the last row.
    let lines = help_body(area.width.saturating_sub(2)).len();
    let inner_height = area.height.saturating_sub(2) as usize;
    (area, lines.saturating_sub(inner_height) as u16)
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

/// A centered box sized to its content (plus borders), clamped to the frame.
fn fitted_rect(cols: usize, lines: usize, area: Rect) -> Rect {
    let width = (cols as u16).saturating_add(2).min(area.width);
    let height = (lines as u16).saturating_add(2).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
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

    /// The hint row does not wrap, so what does not fit is lost — and the two
    /// that would go first are the ones that replaced keys people knew.
    #[test]
    fn hints_shrink_to_fit_the_terminal() {
        // Every width keeps the head and the tail, and never overflows.
        for width in [200u16, 118, 100, 90, 80, 70, 40, 32, 10] {
            let hint = hints(width);
            assert!(hint.starts_with(HINT_HEAD), "{width}: {hint}");
            assert!(
                hint.contains("F1 help") && hint.contains("^q quit"),
                "{width}: {hint}"
            );
            if width >= 32 {
                assert!(hint.width() <= width as usize, "{width}: {hint}");
            }
        }
        // Room for everything means everything is shown.
        let full = hints(200);
        assert!(HINT_MIDDLE.iter().all(|h| full.contains(h)), "{full}");
        // Widening the terminal never shows fewer hints.
        let widths: Vec<usize> = (30..=130).map(|w| hints(w).width()).collect();
        assert!(widths.windows(2).all(|w| w[0] <= w[1]), "{widths:?}");
    }

    /// Render the help panel at `scroll` and give back what is on screen.
    fn render_help(width: u16, height: u16, scroll: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let (area, max_scroll) = help_geometry(frame.area());
                frame.render_widget(
                    Paragraph::new(help_body(area.width.saturating_sub(2)))
                        .block(Block::default().borders(Borders::ALL))
                        .scroll((scroll.min(max_scroll), 0)),
                    area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The panel scrolls because a small terminal wraps its rows past the
    /// bottom. Scrolling all the way must actually reach the last one — a
    /// limit computed from a different idea of where lines break would not.
    #[test]
    fn scrolling_the_help_panel_reaches_its_last_row() {
        let last = HELP.last().map(|(k, _)| *k).unwrap();
        assert!(last.is_empty(), "the table ends with a spacer");
        let (key, description) = HELP[HELP.len() - 2];
        assert_eq!(key, "esc");

        // A word that appears in this row and nowhere else, so finding it on
        // screen really does mean the last row is on screen.
        let tail = "moving";
        assert!(description.contains(tail));
        assert_eq!(
            HELP.iter().filter(|(_, d)| d.contains(tail)).count(),
            1,
            "{tail} is no longer unique to the last row"
        );
        for width in [20u16, 22, 27, 28, 40, 60, 80, 120] {
            for height in [10u16, 16, 24, 40] {
                let (_, max_scroll) = help_geometry(Rect::new(0, 0, width, height));
                let screen = render_help(width, height, max_scroll);
                assert!(
                    screen.contains(key) && screen.contains(tail),
                    "{width}x{height}: last row unreachable at scroll {max_scroll}\n{screen}"
                );
                // And the test is not vacuous: where there is scrolling to do,
                // the last row is genuinely off-screen until it is done.
                if max_scroll > 0 {
                    assert!(
                        !render_help(width, height, 0).contains(tail),
                        "{width}x{height}: nothing was actually scrolled"
                    );
                }
            }
        }
    }

    /// Wrapping never puts more on a line than the panel has room for.
    #[test]
    fn the_help_panel_never_overflows_its_width() {
        for width in 16u16..100 {
            for line in help_body(width) {
                let drawn: usize = line.spans.iter().map(|s| s.content.width()).sum();
                assert!(drawn <= width as usize, "width {width}: {drawn} columns");
            }
        }
    }

    /// A terminal too small for the table wraps the rows past the bottom of
    /// the panel; every one of them still has to be reachable.
    #[test]
    fn the_help_panel_can_always_reach_its_last_row() {
        for (w, h) in [
            (200u16, 60u16),
            (100, 40),
            (80, 24),
            (60, 24),
            (40, 20),
            (30, 10),
        ] {
            let frame = Rect::new(0, 0, w, h);
            let (area, max_scroll) = help_geometry(frame);
            assert!(area.width <= w && area.height <= h, "{w}x{h}: {area:?}");

            let lines = help_body(area.width.saturating_sub(2)).len();
            let shown = area.height.saturating_sub(2) as usize + max_scroll as usize;
            assert!(
                shown >= lines,
                "{w}x{h}: {shown} lines reachable of {lines}"
            );
        }
    }

    /// Where there is room for the table, it is not scrollable and not clipped.
    #[test]
    fn the_help_panel_fits_a_normal_terminal_outright() {
        let (area, max_scroll) = help_geometry(Rect::new(0, 0, 80, 30));
        assert_eq!(max_scroll, 0);
        let widest = HELP
            .iter()
            .map(|(_, d)| KEY_WIDTH + d.width())
            .max()
            .unwrap();
        assert!(area.width as usize >= widest + 2, "{area:?} clips {widest}");
        assert!(area.height as usize >= HELP.len() + 2, "{area:?}");
    }

    #[test]
    fn truncates_paths_from_the_left() {
        assert_eq!(truncate_start("/a/b", 10), "/a/b");
        assert_eq!(truncate_start("/very/long/path/file", 10), "…path/file");
    }
}
