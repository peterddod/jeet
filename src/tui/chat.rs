//! `jeet chat --pick`: the assistant's sessions, to resume one, start a new
//! one, rename or end one, or mark the one events go to first.
//!
//! The screen only picks. Whatever is chosen is opened and attached to by
//! `jeet chat` once the screen is down, so tmux has the terminal to itself.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::term::{self, init_terminal, restore_terminal, Tui};
use super::ui::truncate;
use crate::agent::age_label;
use crate::assistant::{self, Session};
use crate::commands::task::printable;
use crate::context::App;
use crate::db::tasks::TaskRecord;

/// What the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Resume, or attach to, this session.
    Session(String),
    /// Start a new one.
    New,
    Quit,
}

/// What a key asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stay,
    Done(Pick),
    Rename {
        id: String,
        name: String,
    },
    End {
        id: String,
    },
    /// Make it the primary session, or, if it is, no longer.
    Primary {
        id: String,
    },
}

/// The picker's state.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub sessions: Vec<Session>,
    pub selected: usize,
    /// The new name being typed for the highlighted session.
    pub renaming: Option<String>,
    pub status: String,
}

impl Picker {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            ..Self::default()
        }
    }

    fn current(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    /// Replace the sessions, keeping the same one highlighted if it is
    /// still there.
    pub fn reload(&mut self, sessions: Vec<Session>) {
        let id = self.current().map(|s| s.id.clone());
        self.sessions = sessions;
        self.selected = id
            .and_then(|id| self.sessions.iter().position(|s| s.id == id))
            .unwrap_or(0)
            .min(self.sessions.len().saturating_sub(1));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Step {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Step::Done(Pick::Quit);
        }
        if let Some(input) = &mut self.renaming {
            match key.code {
                KeyCode::Esc => self.renaming = None,
                KeyCode::Enter => {
                    let name = input.clone();
                    self.renaming = None;
                    if let Some(s) = self.current() {
                        return Step::Rename {
                            id: s.id.clone(),
                            name,
                        };
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !c.is_control() => input.push(c),
                _ => {}
            }
            return Step::Stay;
        }
        let id = self.current().map(|s| s.id.clone());
        match (key.code, id) {
            (KeyCode::Esc | KeyCode::Char('q'), _) => Step::Done(Pick::Quit),
            (KeyCode::Char('n'), _) => Step::Done(Pick::New),
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                self.selected = self.selected.saturating_sub(1);
                Step::Stay
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                }
                Step::Stay
            }
            (KeyCode::Enter, Some(id)) => Step::Done(Pick::Session(id)),
            (KeyCode::Char('R'), Some(_)) => {
                self.renaming = self.current().map(|s| s.name.clone());
                Step::Stay
            }
            (KeyCode::Char('x'), Some(id)) => Step::End { id },
            (KeyCode::Char('*'), Some(id)) => Step::Primary { id },
            (KeyCode::Enter | KeyCode::Char('R' | 'x' | '*'), None) => {
                self.status = "no sessions yet — n starts one".to_string();
                Step::Stay
            }
            _ => Step::Stay,
        }
    }
}

/// Show the picker until something is picked.
pub fn run(app: &App, task: &TaskRecord) -> Result<Pick> {
    let sessions = assistant::sessions(app, task)?;
    let mut terminal = init_terminal()?;
    let result = pick(&mut terminal, app, task, Picker::new(sessions));
    restore_terminal(&mut terminal)?;
    result
}

fn pick(terminal: &mut Tui, app: &App, task: &TaskRecord, mut picker: Picker) -> Result<Pick> {
    loop {
        terminal.draw(|frame| draw(frame, &picker, None))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match picker.on_key(key) {
            Step::Stay => {}
            Step::Done(pick) => return Ok(pick),
            Step::Rename { id, name } => match assistant::session_name(&name) {
                Ok(name) => {
                    app.db.rename_task_session(&id, &name)?;
                    picker.status = format!("renamed to {name}");
                    picker.reload(assistant::sessions(app, task)?);
                }
                Err(e) => picker.status = format!("{e:#}"),
            },
            Step::Primary { id } => {
                let was = picker.current().is_some_and(|s| s.primary);
                app.db
                    .set_primary_session(&task.id, (!was).then_some(id.as_str()))?;
                picker.status = match was {
                    true => "no primary session: events go to the latest running one".into(),
                    false => "primary: events go to this session while it runs".into(),
                };
                picker.reload(assistant::sessions(app, task)?);
            }
            Step::End { id } => {
                let Some(session) = picker.sessions.iter().find(|s| s.id == id).cloned() else {
                    continue;
                };
                if !session.running() {
                    picker.status = format!("{} is not running", printable(&session.name));
                    continue;
                }
                // Ending waits for the agent to exit, which takes a while.
                let shown = picker.clone();
                let ended = term::with_progress(
                    terminal,
                    &format!("ending {}", printable(&session.name)),
                    |frame, working| draw(frame, &shown, Some(working)),
                    || {
                        assistant::end(&session)?;
                        assistant::sessions(app, task)
                    },
                )?;
                match ended {
                    Ok(sessions) => {
                        picker.status = format!("ended {}", printable(&session.name));
                        picker.reload(sessions);
                    }
                    Err(e) => picker.status = format!("could not end it: {e:#}"),
                }
            }
        }
    }
}

pub(crate) const KEYS: &str = "⏎ attach  n new  R rename  x end  * primary  q quit";

fn draw(frame: &mut Frame, picker: &Picker, working: Option<&str>) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" assistant · sessions ")
        .title_style(Style::default().fg(Color::Green));
    if picker.sessions.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("no sessions yet"),
                Line::from(""),
                Line::from(Span::styled(
                    "n starts one",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(block),
            rows[0],
        );
    } else {
        let items: Vec<ListItem> = picker.sessions.iter().map(session_line).collect();
        let mut state = ListState::default();
        state.select(Some(picker.selected.min(picker.sessions.len() - 1)));
        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, rows[0], &mut state);
    }
    let footer = match (working, &picker.renaming) {
        (Some(working), _) => Line::from(Span::styled(
            working.to_string(),
            Style::default().fg(Color::Yellow),
        )),
        (None, Some(input)) => Line::from(vec![
            Span::styled("new name: ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{input}▏")),
            Span::styled("  ⏎ save  esc cancel", Style::default().fg(Color::DarkGray)),
        ]),
        (None, None) if !picker.status.is_empty() => Line::from(picker.status.clone()),
        (None, None) => Line::from(Span::styled(KEYS, Style::default().fg(Color::DarkGray))),
    };
    frame.render_widget(Paragraph::new(footer), rows[1]);
}

pub(crate) fn session_line(s: &Session) -> ListItem<'static> {
    let color = match s.running() {
        true => Color::Cyan,
        false => Color::DarkGray,
    };
    ListItem::new(Line::from(vec![
        Span::styled(
            if s.primary { "* " } else { "  " },
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("{:<32}", truncate(&printable(&s.name), 30)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<24}", truncate(&s.activity.label(), 22)),
            Style::default().fg(color),
        ),
        Span::styled(
            format!("{:<10}", age_label(s.last)),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(s.id.clone(), Style::default().fg(Color::DarkGray)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Activity;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn session(id: &str, primary: bool, running: bool) -> Session {
        Session {
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keys_pick_move_and_act_on_the_highlighted_session() {
        let mut picker = Picker::new(vec![session("a", false, true), session("b", true, false)]);
        assert_eq!(picker.on_key(key(KeyCode::Down)), Step::Stay);
        assert_eq!(picker.on_key(key(KeyCode::Char('j'))), Step::Stay);
        assert_eq!(picker.selected, 1);
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            Step::Done(Pick::Session("b".into()))
        );
        assert_eq!(
            picker.on_key(key(KeyCode::Char('*'))),
            Step::Primary { id: "b".into() }
        );
        assert_eq!(
            picker.on_key(key(KeyCode::Char('x'))),
            Step::End { id: "b".into() }
        );
        picker.on_key(key(KeyCode::Char('k')));
        assert_eq!(picker.selected, 0);
        assert_eq!(
            picker.on_key(key(KeyCode::Char('n'))),
            Step::Done(Pick::New)
        );
        assert_eq!(
            picker.on_key(key(KeyCode::Char('q'))),
            Step::Done(Pick::Quit)
        );
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Step::Done(Pick::Quit)
        );
    }

    #[test]
    fn renaming_edits_the_name_in_place() {
        let mut picker = Picker::new(vec![session("a", false, true)]);
        picker.on_key(key(KeyCode::Char('R')));
        assert_eq!(picker.renaming.as_deref(), Some("a name"));
        for _ in 0..4 {
            picker.on_key(key(KeyCode::Backspace));
        }
        // Letters that are keys elsewhere are just letters here.
        for c in "planq".chars() {
            assert_eq!(picker.on_key(key(KeyCode::Char(c))), Step::Stay);
        }
        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            Step::Rename {
                id: "a".into(),
                name: "a planq".into()
            }
        );
        assert!(picker.renaming.is_none());
        picker.on_key(key(KeyCode::Char('R')));
        picker.on_key(key(KeyCode::Esc));
        assert!(picker.renaming.is_none());
    }

    #[test]
    fn with_no_sessions_only_new_and_quit_do_anything() {
        let mut picker = Picker::new(Vec::new());
        assert_eq!(picker.on_key(key(KeyCode::Enter)), Step::Stay);
        assert!(picker.status.contains("n starts one"));
        assert_eq!(picker.on_key(key(KeyCode::Char('x'))), Step::Stay);
        assert_eq!(
            picker.on_key(key(KeyCode::Char('n'))),
            Step::Done(Pick::New)
        );
    }

    #[test]
    fn reloading_keeps_the_highlight_on_the_same_session() {
        let mut picker = Picker::new(vec![session("a", false, true), session("b", false, true)]);
        picker.selected = 1;
        picker.reload(vec![session("c", false, true), session("b", false, true)]);
        assert_eq!(picker.selected, 1);
        picker.reload(vec![session("c", false, true)]);
        assert_eq!(picker.selected, 0);
        picker.reload(Vec::new());
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn the_primary_session_is_starred() {
        let picker = Picker::new(vec![session("a", false, true), session("b", true, false)]);
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &picker, None)).unwrap();
        let text: Vec<String> = (0..8)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        let row_a = text.iter().find(|l| l.contains("a name")).unwrap();
        let row_b = text.iter().find(|l| l.contains("b name")).unwrap();
        assert!(!row_a.contains('*'), "{row_a}");
        assert!(row_b.contains("* b name"), "{row_b}");
        assert!(row_b.contains("not running"), "{row_b}");
        assert!(text[7].contains("⏎ attach"), "{}", text[7]);
    }
}
