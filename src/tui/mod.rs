//! The `jeet` file explorer.
//!
//! Running `jeet` with no arguments inside a repository opens this: a single
//! level of the tree at a time, arrow keys to move through it, and shortcuts
//! for the things you actually came to do — switch worktree, edit a file, or
//! hand the worktree to a coding agent.

pub mod state;
pub mod ui;

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::widgets::ListState;
use ratatui::Terminal;

use crate::agent::{self, AgentSpec};
use crate::context::App;
use crate::db::RepoRecord;
use crate::resolve::RepoContext;
use crate::worktrees::{self, WorktreeKind, WorktreeStatus};

use state::{Exit, Explorer, Overlay, PendingAction, WorktreeRow};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Run the explorer, returning where the shell should end up.
pub fn run(app: &App, ctx: &RepoContext, start_dir: &Path) -> Result<Exit> {
    let spec = AgentSpec::from_config(&app.config)?;
    let (label, kind, status) = describe_root(app, &ctx.repo, &ctx.root);

    let mut explorer = Explorer::new(
        ctx.repo.clone(),
        ctx.root.clone(),
        label,
        kind,
        status,
        start_dir.to_path_buf(),
        spec,
    )?;
    explorer.set_status(format!(
        "{} · press ? for keys",
        explorer.repo.trunk_path.clone()
    ));

    // Keep git's own output off the alternate screen.
    crate::git::set_capture_output(true);
    let mut terminal = init_terminal()?;
    let result = event_loop(app, &mut terminal, &mut explorer);
    restore_terminal(&mut terminal)?;
    crate::git::set_capture_output(false);
    result?;
    Ok(explorer.exit.clone())
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

/// Drop out of the alternate screen, run `f`, then restore the explorer.
fn suspended<T>(terminal: &mut Tui, f: impl FnOnce() -> T) -> Result<T> {
    restore_terminal(terminal)?;
    let out = f();
    enable_raw_mode().context("enable raw mode")?;
    execute!(terminal.backend_mut(), EnterAlternateScreen).context("enter alternate screen")?;
    terminal.clear().context("clear terminal")?;
    Ok(out)
}

fn event_loop(app: &App, terminal: &mut Tui, explorer: &mut Explorer) -> Result<()> {
    let mut list_state = ListState::default();
    while !explorer.should_quit {
        terminal.draw(|frame| ui::draw(frame, explorer, &mut list_state))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Err(e) = handle_key(app, terminal, explorer, key) {
            explorer.set_status(format!("error: {e}"));
        }
    }
    Ok(())
}

fn handle_key(app: &App, terminal: &mut Tui, explorer: &mut Explorer, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        explorer.quit_in_place();
        return Ok(());
    }
    match explorer.overlay.take() {
        Some(overlay) => handle_overlay_key(app, terminal, explorer, overlay, key),
        None => handle_browse_key(app, terminal, explorer, key),
    }
}

fn handle_browse_key(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Char('q') => explorer.quit_here(),
        KeyCode::Esc => explorer.quit_in_place(),
        KeyCode::Up | KeyCode::Char('k') => explorer.move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => explorer.move_cursor(1),
        KeyCode::PageUp => explorer.move_cursor(-10),
        KeyCode::PageDown => explorer.move_cursor(10),
        KeyCode::Char('g') | KeyCode::Home => explorer.select_first(),
        KeyCode::Char('G') | KeyCode::End => explorer.select_last(),
        KeyCode::Right | KeyCode::Char('l') => {
            if !explorer.descend()? {
                explorer.set_status("not a directory — press ⏎ to open it");
            } else {
                explorer.set_status("");
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if explorer.ascend()? {
                explorer.set_status("");
            }
        }
        KeyCode::Enter => {
            let is_dir = explorer.selected_entry().map(|e| e.is_dir).unwrap_or(false);
            if is_dir {
                explorer.descend()?;
            } else if let Some(entry) = explorer.selected_entry().cloned() {
                open_editor(app, terminal, explorer, &entry.path)?;
            }
        }
        KeyCode::Char('.') => {
            let keep = explorer.selected_entry().map(|e| e.path.clone());
            explorer.show_hidden = !explorer.show_hidden;
            explorer.reload(keep.as_deref())?;
            explorer.set_status(if explorer.show_hidden {
                "showing hidden files"
            } else {
                "hiding hidden files"
            });
        }
        KeyCode::Char('r') => {
            let keep = explorer.selected_entry().map(|e| e.path.clone());
            explorer.reload(keep.as_deref())?;
            refresh_root_status(app, explorer);
            explorer.set_status("refreshed");
        }
        KeyCode::Char('c') => launch_agent(app, terminal, explorer, &[])?,
        KeyCode::Char('s') => open_sessions(explorer),
        KeyCode::Char('w') => open_worktrees(app, explorer),
        KeyCode::Char('?') => explorer.overlay = Some(Overlay::Help),
        _ => {}
    }
    Ok(())
}

fn handle_overlay_key(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    overlay: Overlay,
    key: KeyEvent,
) -> Result<()> {
    match overlay {
        Overlay::Help | Overlay::Message { .. } => {
            if !matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                explorer.overlay = Some(overlay);
            }
        }
        Overlay::Worktrees { mut selected } => match key.code {
            KeyCode::Esc | KeyCode::Char('w') | KeyCode::Char('q') => {}
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                explorer.overlay = Some(Overlay::Worktrees { selected });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = explorer.worktree_rows.len().saturating_sub(1);
                selected = (selected + 1).min(max);
                explorer.overlay = Some(Overlay::Worktrees { selected });
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                match explorer.worktree_rows.get(selected).cloned() {
                    Some(row) if row.entry.missing => {
                        explorer.overlay = Some(Overlay::Message {
                            title: "unavailable".into(),
                            lines: vec![format!(
                                "{} is registered with git but missing on disk",
                                row.entry.path.display()
                            )],
                        });
                    }
                    Some(row) => {
                        switch_worktree(app, explorer, &row.entry.path)?;
                        explorer.set_status(format!("switched to {}", row.entry.display_name()));
                    }
                    None => {}
                }
            }
            KeyCode::Char('n') => {
                explorer.overlay = Some(Overlay::NewWorktree {
                    input: String::new(),
                })
            }
            KeyCode::Char('e') => {
                create_worktree(app, explorer, None)?;
            }
            KeyCode::Char('d') => match explorer.worktree_rows.get(selected) {
                Some(row) if row.entry.kind == WorktreeKind::Trunk => {
                    explorer.overlay = Some(Overlay::Message {
                        title: "cannot delete".into(),
                        lines: vec!["the trunk checkout is not removable".into()],
                    });
                }
                Some(row) if row.current => {
                    explorer.overlay = Some(Overlay::Message {
                        title: "cannot delete".into(),
                        lines: vec![
                            "this is the worktree you are browsing".into(),
                            "switch somewhere else first (⏎ on another row)".into(),
                        ],
                    });
                }
                Some(row) => {
                    explorer.overlay = Some(Overlay::Confirm {
                        title: "delete worktree".into(),
                        lines: delete_summary(row),
                        action: PendingAction::RemoveWorktree,
                        index: selected,
                    });
                }
                None => {}
            },
            KeyCode::Char('r') => open_worktrees(app, explorer),
            _ => explorer.overlay = Some(Overlay::Worktrees { selected }),
        },
        Overlay::Sessions {
            sessions,
            mut selected,
        } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {}
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                explorer.overlay = Some(Overlay::Sessions { sessions, selected });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(sessions.len().saturating_sub(1));
                explorer.overlay = Some(Overlay::Sessions { sessions, selected });
            }
            KeyCode::Enter => match sessions.get(selected) {
                Some(session) => {
                    let args = explorer.agent.resume_args(&session.id).unwrap_or_default();
                    launch_agent(app, terminal, explorer, &args)?;
                }
                None => explorer.set_status("no session selected"),
            },
            _ => explorer.overlay = Some(Overlay::Sessions { sessions, selected }),
        },
        Overlay::NewWorktree { mut input } => match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let name = input.trim().to_string();
                create_worktree(app, explorer, Some(name).filter(|n| !n.is_empty()))?;
            }
            KeyCode::Backspace => {
                input.pop();
                explorer.overlay = Some(Overlay::NewWorktree { input });
            }
            KeyCode::Char(c) => {
                input.push(c);
                explorer.overlay = Some(Overlay::NewWorktree { input });
            }
            _ => explorer.overlay = Some(Overlay::NewWorktree { input }),
        },
        Overlay::Confirm {
            title,
            lines,
            action,
            index,
        } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                match action {
                    PendingAction::RemoveWorktree => remove_worktree(app, explorer, index)?,
                }
                open_worktrees(app, explorer);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                open_worktrees(app, explorer);
            }
            _ => {
                explorer.overlay = Some(Overlay::Confirm {
                    title,
                    lines,
                    action,
                    index,
                })
            }
        },
    }
    Ok(())
}

fn delete_summary(row: &WorktreeRow) -> Vec<String> {
    let mut lines = vec![
        format!("{} ({})", row.entry.display_name(), row.entry.kind.label()),
        ui::truncate_start(&row.entry.path.display().to_string(), 58),
        String::new(),
    ];
    if row.status.dirty > 0 {
        lines.push(format!(
            "⚠ {} uncommitted change{} will be discarded",
            row.status.dirty,
            if row.status.dirty == 1 { "" } else { "s" }
        ));
    }
    if row.status.ahead > 0 {
        lines.push(format!(
            "⚠ {} commit{} not on the default branch",
            row.status.ahead,
            if row.status.ahead == 1 { "" } else { "s" }
        ));
    }
    if !row.status.has_work() {
        lines.push("nothing to lose — clean and fully merged".to_string());
    }
    lines.push(format!("diff: {}", row.status.diff_summary()));
    lines
}

fn open_worktrees(app: &App, explorer: &mut Explorer) {
    let rows = collect_rows(app, &explorer.repo, &explorer.root);
    let selected = rows.iter().position(|r| r.current).unwrap_or(0);
    explorer.worktree_rows = rows;
    explorer.overlay = Some(Overlay::Worktrees { selected });
}

fn collect_rows(app: &App, repo: &RepoRecord, current_root: &Path) -> Vec<WorktreeRow> {
    match worktrees::list(app, repo) {
        Ok(entries) => entries
            .into_iter()
            .map(|entry| WorktreeRow {
                status: worktrees::status_for(repo, &entry),
                current: crate::resolve::same_path(&entry.path, current_root),
                entry,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn open_sessions(explorer: &mut Explorer) {
    if !explorer.agent.supports_sessions() {
        explorer.overlay = Some(Overlay::Message {
            title: "sessions".into(),
            lines: vec![format!(
                "jeet does not know where `{}` stores its sessions",
                explorer.agent.display()
            )],
        });
        return;
    }
    match agent::sessions_for(&explorer.agent, &explorer.root) {
        Ok(sessions) if sessions.is_empty() => {
            explorer.overlay = Some(Overlay::Message {
                title: "sessions".into(),
                lines: vec![
                    format!("no {} sessions recorded for", explorer.agent.display()),
                    explorer.root.display().to_string(),
                    String::new(),
                    "press c to start one".into(),
                ],
            });
        }
        Ok(sessions) => {
            explorer.overlay = Some(Overlay::Sessions {
                sessions,
                selected: 0,
            })
        }
        Err(e) => {
            explorer.overlay = Some(Overlay::Message {
                title: "sessions".into(),
                lines: vec![e.to_string()],
            })
        }
    }
}

fn create_worktree(app: &App, explorer: &mut Explorer, name: Option<String>) -> Result<()> {
    let result = match &name {
        Some(branch) => worktrees::create_named(app, &explorer.repo, branch, true),
        None => worktrees::create_detached(app, &explorer.repo).map(|path| worktrees::Created {
            path,
            warnings: Vec::new(),
        }),
    };
    match result {
        Ok(created) => {
            switch_worktree(app, explorer, &created.path)?;
            let what = match name {
                Some(branch) => format!("created worktree {branch}"),
                None => "created detached worktree".to_string(),
            };
            if created.warnings.is_empty() {
                explorer.set_status(what);
            } else {
                explorer.set_status(format!("{what} — {}", created.warnings.join("; ")));
            }
        }
        Err(e) => {
            explorer.overlay = Some(Overlay::Message {
                title: "could not create worktree".into(),
                lines: vec![e.to_string()],
            });
        }
    }
    Ok(())
}

fn remove_worktree(app: &App, explorer: &mut Explorer, index: usize) -> Result<()> {
    let Some(row) = explorer.worktree_rows.get(index).cloned() else {
        return Ok(());
    };
    match worktrees::remove(app, &explorer.repo, &row.entry, true) {
        Ok(()) => explorer.set_status(format!("removed {}", row.entry.display_name())),
        Err(e) => explorer.set_status(format!("could not remove: {e}")),
    }
    Ok(())
}

fn switch_worktree(app: &App, explorer: &mut Explorer, path: &Path) -> Result<()> {
    let (label, kind, status) = describe_root(app, &explorer.repo, path);
    explorer.root = path.to_path_buf();
    explorer.root_label = label;
    explorer.root_kind = kind;
    explorer.root_status = status;
    explorer.cwd = path.to_path_buf();
    explorer.overlay = None;
    explorer.reload(None)
}

fn refresh_root_status(app: &App, explorer: &mut Explorer) {
    let (label, kind, status) = describe_root(app, &explorer.repo, &explorer.root.clone());
    explorer.root_label = label;
    explorer.root_kind = kind;
    explorer.root_status = status;
}

/// Branch label, worktree kind and counters for the worktree at `root`.
fn describe_root(app: &App, repo: &RepoRecord, root: &Path) -> (String, String, WorktreeStatus) {
    let entries = worktrees::list(app, repo).unwrap_or_default();
    if let Some(entry) = entries
        .iter()
        .find(|e| crate::resolve::same_path(&e.path, root))
    {
        let status = worktrees::status_for(repo, entry);
        return (entry.display_name(), entry.kind.label().to_string(), status);
    }
    let label = crate::git::head_branch(root).unwrap_or_else(|| {
        crate::git::head_short_sha(root)
            .map(|sha| format!("detached @ {sha}"))
            .unwrap_or_else(|| "unknown".to_string())
    });
    (label, "worktree".to_string(), WorktreeStatus::default())
}

fn open_editor(app: &App, terminal: &mut Tui, explorer: &mut Explorer, file: &Path) -> Result<()> {
    let argv = agent::editor_argv(&app.config)?;
    let file_arg = vec![file.to_string_lossy().to_string()];
    let cwd = explorer.cwd.clone();
    let outcome = suspended(terminal, || agent::run_in(&argv, &cwd, &file_arg))?;
    match outcome {
        Ok(0) => explorer.set_status(format!("closed {}", display_relative(explorer, file))),
        Ok(code) => explorer.set_status(format!("editor exited with status {code}")),
        Err(e) => explorer.set_status(format!("could not open editor: {e}")),
    }
    let keep = explorer.selected_entry().map(|e| e.path.clone());
    explorer.reload(keep.as_deref())?;
    Ok(())
}

/// Launch the coding agent from the worktree root, so it sees the whole tree.
fn launch_agent(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    extra: &[String],
) -> Result<()> {
    let argv = explorer.agent.argv.clone();
    let root = explorer.root.clone();
    let extra = extra.to_vec();
    let outcome = suspended(terminal, || agent::run_in(&argv, &root, &extra))?;
    match outcome {
        Ok(0) => explorer.set_status(format!("{} exited", explorer.agent.display())),
        Ok(code) => explorer.set_status(format!(
            "{} exited with status {code}",
            explorer.agent.display()
        )),
        Err(e) => explorer.set_status(format!("could not start agent: {e}")),
    }
    refresh_root_status(app, explorer);
    let keep = explorer.selected_entry().map(|e| e.path.clone());
    explorer.reload(keep.as_deref())?;
    Ok(())
}

fn display_relative(explorer: &Explorer, path: &Path) -> String {
    path.strip_prefix(&explorer.root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Directory the explorer should start listing, given where the user ran jeet.
pub fn start_dir(ctx: &RepoContext, cwd: &Path) -> PathBuf {
    if cwd.starts_with(&ctx.root) {
        cwd.to_path_buf()
    } else {
        ctx.root.clone()
    }
}
