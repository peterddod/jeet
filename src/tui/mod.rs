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
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
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
    install_safety_net();
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")
}

/// Put the terminal back however we leave: panic, or a signal from outside.
///
/// Raw mode swallows ctrl-c (we handle it as a key), so the only way a signal
/// arrives is from elsewhere — `pkill`, an IDE tearing the session down, the
/// window closing. Without this the user is left with no echo, no line editing
/// and the alternate screen still up, recoverable only with `reset`.
fn install_safety_net() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            previous(info);
        }));

        #[cfg(unix)]
        for signal in [
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGQUIT,
        ] {
            // SAFETY: the handler only restores terminal modes and re-raises;
            // leaving the terminal wedged is the worse outcome by far.
            unsafe {
                let _ = signal_hook::low_level::register(signal, move || {
                    emergency_restore();
                    let _ = signal_hook::low_level::emulate_default_handler(signal);
                });
            }
        }
    });
}

/// Best-effort teardown for paths that cannot return a `Result`.
fn emergency_restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
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

/// Run `job` on a background thread while the UI keeps drawing.
///
/// git is slow and the network is slower — a `git push` on worktree creation
/// froze the whole explorer for as long as the remote took, with nothing on
/// screen to say why. Keystrokes that arrive meanwhile are dropped rather than
/// queued, so they cannot fire against a screen the user never saw.
fn with_progress<T: Send>(
    terminal: &mut Tui,
    explorer: &mut Explorer,
    label: &str,
    job: impl FnOnce() -> T + Send,
) -> Result<T> {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let (tx, rx) = mpsc::channel();

    let outcome = std::thread::scope(|scope| -> Result<T> {
        scope.spawn(move || {
            let _ = tx.send(job());
        });
        let mut tick = 0usize;
        loop {
            match rx.try_recv() {
                Ok(value) => return Ok(value),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("background task failed")
                }
            }
            explorer.working = Some(format!(" {} {label} ", FRAMES[tick % FRAMES.len()]));
            terminal.draw(|frame| ui::draw(frame, explorer))?;
            tick += 1;
            while event::poll(Duration::from_millis(0))? {
                let _ = event::read();
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    });

    explorer.working = None;
    outcome
}

fn event_loop(app: &App, terminal: &mut Tui, explorer: &mut Explorer) -> Result<()> {
    while !explorer.should_quit {
        terminal.draw(|frame| ui::draw(frame, explorer))?;
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
        KeyCode::Char('w') => open_worktrees(app, terminal, explorer),
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
        Overlay::Help => {
            if !matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                explorer.overlay = Some(overlay);
            }
        }
        Overlay::Message { from_panel, .. } => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                if from_panel {
                    open_worktrees(app, terminal, explorer);
                }
            } else {
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
                            from_panel: true,
                        });
                    }
                    Some(row) => match switch_worktree(app, explorer, &row.entry.path) {
                        Ok(()) => {
                            explorer.set_status(format!("switched to {}", row.entry.display_name()))
                        }
                        Err(e) => {
                            explorer.overlay = Some(Overlay::Message {
                                title: "could not switch".into(),
                                lines: vec![format!("{e:#}")],
                                from_panel: true,
                            })
                        }
                    },
                    None => explorer.overlay = Some(Overlay::Worktrees { selected }),
                }
            }
            KeyCode::Char('n') => {
                explorer.overlay = Some(Overlay::NewWorktree {
                    input: String::new(),
                })
            }
            KeyCode::Char('e') => {
                create_worktree(app, terminal, explorer, None)?;
            }
            KeyCode::Char('m') => match explorer.worktree_rows.get(selected) {
                Some(row) if row.entry.kind == WorktreeKind::Trunk => {
                    explorer.overlay = Some(Overlay::Message {
                        title: "cannot rename".into(),
                        lines: vec!["the trunk checkout keeps the default branch".into()],
                        from_panel: true,
                    });
                }
                Some(row) => {
                    explorer.overlay = Some(Overlay::RenameWorktree {
                        index: selected,
                        input: row.entry.branch.clone().unwrap_or_default(),
                    });
                }
                None => explorer.overlay = Some(Overlay::Worktrees { selected }),
            },
            KeyCode::Char('d') => match explorer.worktree_rows.get(selected) {
                Some(row) if row.entry.kind == WorktreeKind::Trunk => {
                    explorer.overlay = Some(Overlay::Message {
                        title: "cannot delete".into(),
                        lines: vec!["the trunk checkout is not removable".into()],
                        from_panel: true,
                    });
                }
                Some(row) if row.current => {
                    explorer.overlay = Some(Overlay::Message {
                        title: "cannot delete".into(),
                        lines: vec![
                            "this is the worktree you are browsing".into(),
                            "switch somewhere else first (⏎ on another row)".into(),
                        ],
                        from_panel: true,
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
                None => explorer.overlay = Some(Overlay::Worktrees { selected }),
            },
            KeyCode::Char('r') => open_worktrees(app, terminal, explorer),
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
        Overlay::RenameWorktree { index, mut input } => match key.code {
            KeyCode::Esc => open_worktrees(app, terminal, explorer),
            KeyCode::Enter => {
                rename_worktree(app, terminal, explorer, index, input.trim().to_string())?
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                explorer.overlay = Some(Overlay::RenameWorktree { index, input });
            }
            KeyCode::Backspace => {
                input.pop();
                explorer.overlay = Some(Overlay::RenameWorktree { index, input });
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                input.push(c);
                explorer.overlay = Some(Overlay::RenameWorktree { index, input });
            }
            _ => explorer.overlay = Some(Overlay::RenameWorktree { index, input }),
        },
        Overlay::NewWorktree { mut input } => match key.code {
            KeyCode::Esc => open_worktrees(app, terminal, explorer),
            KeyCode::Enter => {
                let name = input.trim().to_string();
                create_worktree(
                    app,
                    terminal,
                    explorer,
                    Some(name).filter(|n| !n.is_empty()),
                )?;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                explorer.overlay = Some(Overlay::NewWorktree { input });
            }
            KeyCode::Backspace => {
                input.pop();
                explorer.overlay = Some(Overlay::NewWorktree { input });
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
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
            KeyCode::Char('y') | KeyCode::Char('Y') => match action {
                PendingAction::RemoveWorktree => {
                    remove_worktree(app, terminal, explorer, index, false)?
                }
            },
            KeyCode::Char('f') | KeyCode::Char('F') => match action {
                PendingAction::RemoveWorktree => {
                    remove_worktree(app, terminal, explorer, index, true)?
                }
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                open_worktrees(app, terminal, explorer);
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
    if let Some(why) = &row.status.unknown {
        lines.push(format!("⚠ could not assess this worktree: {why}"));
    }
    if row.status.ignored > 0 {
        lines.push(format!(
            "⚠ {} ignored file{} (.env, build output) will be deleted",
            row.status.ignored,
            if row.status.ignored == 1 { "" } else { "s" }
        ));
    }
    if !row.status.has_anything_to_lose() {
        lines.push("nothing to lose — clean and fully merged".to_string());
    }
    lines.push(format!("diff: {}", row.status.diff_summary()));
    lines
}

fn open_worktrees(app: &App, terminal: &mut Tui, explorer: &mut Explorer) {
    let repo = explorer.repo.clone();
    let root = explorer.root.clone();
    let rows = with_progress(terminal, explorer, "reading worktrees", || {
        collect_rows(app, &repo, &root)
    })
    .and_then(|inner| inner);
    match rows {
        Ok(rows) => {
            let selected = rows.iter().position(|r| r.current).unwrap_or(0);
            explorer.worktree_rows = rows;
            explorer.overlay = Some(Overlay::Worktrees { selected });
        }
        Err(e) => {
            explorer.worktree_rows = Vec::new();
            explorer.overlay = Some(Overlay::Message {
                title: "could not list worktrees".into(),
                lines: vec![format!("{e:#}")],
                from_panel: false,
            });
        }
    }
}

fn collect_rows(app: &App, repo: &RepoRecord, current_root: &Path) -> Result<Vec<WorktreeRow>> {
    // One comparison base for the whole repo rather than one per row: it costs
    // a git subprocess and the answer cannot differ between worktrees.
    let base = worktrees::comparison_base(repo);
    let entries = worktrees::list(app, repo)?;

    // Each row costs several git subprocesses, and they are independent, so
    // fan them out instead of paying for them one after another.
    const LANES: usize = 8;
    let lane_size = entries.len().div_ceil(LANES).max(1);
    let lanes: Vec<Vec<_>> = entries
        .chunks(lane_size)
        .map(|chunk| chunk.to_vec())
        .collect();

    let rows = std::thread::scope(|scope| {
        let handles: Vec<_> = lanes
            .into_iter()
            .map(|lane| {
                let base = base.clone();
                scope.spawn(move || {
                    lane.into_iter()
                        .map(|entry| WorktreeRow {
                            status: worktrees::status_against(&entry, &base),
                            current: crate::resolve::same_path(&entry.path, current_root),
                            entry,
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect::<Vec<_>>()
    });
    Ok(rows)
}

fn open_sessions(explorer: &mut Explorer) {
    if !explorer.agent.supports_sessions() {
        explorer.overlay = Some(Overlay::Message {
            title: "sessions".into(),
            lines: vec![format!(
                "jeet does not know where `{}` stores its sessions",
                explorer.agent.display()
            )],
            from_panel: false,
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
                from_panel: false,
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
                lines: vec![format!("{e:#}")],
                from_panel: false,
            })
        }
    }
}

fn create_worktree(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    name: Option<String>,
) -> Result<()> {
    let repo = explorer.repo.clone();
    let label = match &name {
        Some(branch) => format!("creating {branch} and publishing it"),
        None => "creating a detached worktree".to_string(),
    };
    let branch = name.clone();
    let result = with_progress(terminal, explorer, &label, move || match &branch {
        Some(branch) => worktrees::create_named(app, &repo, branch, true),
        None => worktrees::create_detached(app, &repo).map(|path| worktrees::Outcome {
            path,
            warnings: Vec::new(),
        }),
    })?;
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
                lines: vec![format!("{e:#}")],
                from_panel: true,
            });
        }
    }
    Ok(())
}

/// Rename the worktree at `index`, following it if it is the one we are in.
fn rename_worktree(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    index: usize,
    new_name: String,
) -> Result<()> {
    let Some(row) = explorer.worktree_rows.get(index).cloned() else {
        return Ok(());
    };
    let was = row.entry.display_name();
    let following = crate::resolve::same_path(&row.entry.path, &explorer.root);
    let sub_path = explorer
        .cwd
        .strip_prefix(&row.entry.path)
        .map(|rest| rest.to_path_buf())
        .ok();

    let repo = explorer.repo.clone();
    let entry = row.entry.clone();
    let target = new_name.clone();
    let outcome = with_progress(
        terminal,
        explorer,
        &format!("renaming to {new_name} and publishing it"),
        move || worktrees::rename(app, &repo, &entry, &target, true),
    )?;
    match outcome {
        Ok(renamed) => {
            if following {
                switch_worktree(app, explorer, &renamed.path)?;
                // Stay in the directory we were browsing, under its new home.
                if let Some(rest) = sub_path.filter(|p| !p.as_os_str().is_empty()) {
                    let landing = renamed.path.join(rest);
                    if landing.is_dir() {
                        explorer.cwd = landing;
                        explorer.reload(None)?;
                    }
                }
            }
            open_worktrees(app, terminal, explorer);
            let mut status = format!("renamed {was} to {new_name}");
            if !renamed.warnings.is_empty() {
                status.push_str(&format!(" — {}", renamed.warnings.join("; ")));
            }
            explorer.set_status(status);
        }
        Err(e) => {
            explorer.overlay = Some(Overlay::Message {
                title: "could not rename".into(),
                lines: vec![format!("{e:#}")],
                from_panel: true,
            });
        }
    }
    Ok(())
}

/// Remove the worktree at `index`. Without `force`, git independently
/// re-checks for modified, untracked and submodule content at removal time —
/// which catches anything written since the dialog's status was sampled.
fn remove_worktree(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    index: usize,
    force: bool,
) -> Result<()> {
    let Some(row) = explorer.worktree_rows.get(index).cloned() else {
        open_worktrees(app, terminal, explorer);
        return Ok(());
    };
    let repo = explorer.repo.clone();
    let entry = row.entry.clone();
    let outcome = with_progress(
        terminal,
        explorer,
        &format!("removing {}", row.entry.display_name()),
        move || worktrees::remove(app, &repo, &entry, force),
    )?;
    match outcome {
        Ok(()) => {
            open_worktrees(app, terminal, explorer);
            explorer.set_status(format!("removed {}", row.entry.display_name()));
        }
        Err(e) => {
            explorer.overlay = Some(Overlay::Message {
                title: "not removed".into(),
                lines: vec![
                    row.entry.display_name(),
                    format!("{e:#}"),
                    String::new(),
                    "press d again and then f to remove it anyway".into(),
                ],
                from_panel: true,
            });
        }
    }
    Ok(())
}

/// Move the explorer to another worktree, leaving it where it was if the new
/// root cannot be listed (it may have been removed since the panel was built).
fn switch_worktree(app: &App, explorer: &mut Explorer, path: &Path) -> Result<()> {
    let entries = state::read_dir(path, explorer.show_hidden)?;
    let (label, kind, status) = describe_root(app, &explorer.repo, path);
    explorer.root = path.to_path_buf();
    explorer.root_label = label;
    explorer.root_kind = kind;
    explorer.root_status = status;
    explorer.cwd = path.to_path_buf();
    explorer.entries = entries;
    explorer.selected = 0;
    explorer.overlay = None;
    Ok(())
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
