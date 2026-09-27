//! The `jeet` file explorer.
//!
//! Running `jeet` with no arguments inside a repository opens this: a single
//! level of the tree at a time, arrow keys or the mouse to move through it,
//! and shortcuts for the things you actually came to do — switch worktree,
//! edit a file, or hand the worktree to a coding agent.
//!
//! Typing filters the level you are on from the moment the window opens, so
//! every command that is not navigation carries a ctrl: a bare letter belongs
//! to the filter, not to a shortcut.

pub mod state;
pub mod ui;

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::agent::{self, AgentSpec};
use crate::context::App;
use crate::db::RepoRecord;
use crate::github::{self, Verdict};
use crate::resolve::RepoContext;
use crate::review::{self, JobState};
use crate::worktrees::{self, WorktreeKind, WorktreeStatus};

use state::{DiffMap, Exit, Explorer, Overlay, PendingAction, PrLookup, Step, WorktreeRow};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Said when a key needs a highlighted row and the filter has left none.
const NO_MATCH: &str = "nothing matches — ⌫ to widen the filter";

/// Why there is no row under the cursor: the filter, or the directory itself.
/// Telling someone to widen a filter they have not typed helps nobody.
fn nothing_to_act_on(explorer: &Explorer) -> &'static str {
    // The directory, not the filter: typing into an empty one leaves nothing
    // for ⌫ to bring back, however much of it there is to delete. And a
    // directory holding only dotfiles is not empty — pointing at ⌫ or calling
    // it empty both steer away from the key that would actually show them.
    match (explorer.entries.is_empty(), explorer.hidden) {
        (false, _) => NO_MATCH,
        (true, 0) => "this directory is empty",
        (true, _) => "nothing here but hidden files — ctrl-d shows them",
    }
}

/// Said when the highlighted row is a file and the key wanted a folder.
const NOT_A_DIRECTORY: &str = "not a directory — press ⏎ to open it";

/// Report what `→` or `/` did. `Step` says which case it was, so there is
/// nothing here to work out — only which words to use.
fn report_step(explorer: &mut Explorer, step: Step) {
    match step {
        Step::Entered(name) => explorer.set_status(format!("entered {name}/")),
        Step::NotADirectory => explorer.set_status(NOT_A_DIRECTORY),
        // Nothing is highlighted, so there was nothing it could have meant:
        // the same two reasons ⇥ and ⏎ give.
        Step::Unresolved => {
            let why = nothing_to_act_on(explorer);
            explorer.set_status(why);
        }
    }
}

/// Ask the terminal for button and wheel reports, in SGR encoding.
///
/// Not crossterm's `EnableMouseCapture`: that also turns on `?1003h`,
/// any-motion tracking, and then every wipe of the pointer across the window
/// wakes the event loop and redraws the whole listing for nothing. `?1002h`
/// reports presses, releases and drags, which is all we act on.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1015l\x1b[?1002l\x1b[?1000l";

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
        "{} · type to filter · F1 for keys",
        explorer.repo.trunk_path.clone()
    ));
    refresh_diffs(&mut explorer);
    start_pr_lookup(&mut explorer);

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
    execute!(stdout, EnterAlternateScreen, Print(MOUSE_ON)).context("enter alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout)).context("create terminal")?;
    // `jeet review` talks on stderr right up to this point, and a terminal
    // that does not blank its alternate screen would leave that showing
    // through every cell the first frame leaves empty.
    terminal.clear().context("clear terminal")?;
    Ok(terminal)
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
    let _ = execute!(io::stdout(), Print(MOUSE_OFF), LeaveAlternateScreen, Show);
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        Print(MOUSE_OFF),
        LeaveAlternateScreen
    )
    .context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

/// Drop out of the alternate screen, run `f`, then restore the explorer.
fn suspended<T>(terminal: &mut Tui, f: impl FnOnce() -> T) -> Result<T> {
    restore_terminal(terminal)?;
    let out = f();
    enable_raw_mode().context("enable raw mode")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        Print(MOUSE_ON)
    )
    .context("enter alternate screen")?;
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

/// How often a running review is checked on, so the header notices it end.
const JOB_CHECK: Duration = Duration::from_secs(3);

fn event_loop(app: &App, terminal: &mut Tui, explorer: &mut Explorer) -> Result<()> {
    let mut last_job_check = Instant::now();
    while !explorer.should_quit {
        terminal.draw(|frame| ui::draw(frame, explorer))?;
        // Block on input unless something in the background may change what
        // is on screen: the PR lookup, or a review that is still running.
        let job_running = explorer
            .review_job
            .as_ref()
            .is_some_and(|job| job.state() == JobState::Running);
        if (explorer.pr_updates.is_some() || job_running)
            && !event::poll(Duration::from_millis(200))?
        {
            take_pr_update(app, explorer);
            if job_running && last_job_check.elapsed() >= JOB_CHECK {
                last_job_check = Instant::now();
                load_review_job(app, explorer);
            }
            continue;
        }
        let outcome = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                handle_key(app, terminal, explorer, key)
            }
            Event::Mouse(mouse) => handle_mouse(terminal, explorer, mouse),
            _ => continue,
        };
        if let Err(e) = outcome {
            explorer.set_status(format!("error: {e}"));
        }
    }
    Ok(())
}

fn handle_key(app: &App, terminal: &mut Tui, explorer: &mut Explorer, key: KeyEvent) -> Result<()> {
    // Both ways out, before anything else can swallow them: a panel takes the
    // whole keyboard while it is up, and these are the only keys that quit.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                explorer.quit_in_place();
                return Ok(());
            }
            KeyCode::Char('q') => {
                explorer.quit_here();
                return Ok(());
            }
            _ => {}
        }
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
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Commands first: with the filter live, every one of them needs a ctrl
        // to keep the letter itself available for typing. ctrl-q and ctrl-c
        // are handled before this, so they work from inside a panel too.
        KeyCode::Char('u') if ctrl => {
            if explorer.clear_filter() {
                explorer.set_status("");
            }
        }
        KeyCode::Char('d') if ctrl => {
            let keep = explorer.selected_entry().map(|e| e.path.clone());
            let shown = explorer.toggle_hidden(keep.as_deref())?;
            explorer.set_status(if shown {
                "showing hidden files"
            } else {
                "hiding hidden files"
            });
        }
        KeyCode::Char('r') if ctrl => {
            let keep = explorer.selected_entry().map(|e| e.path.clone());
            explorer.reload(keep.as_deref())?;
            refresh_root_status(app, explorer);
            start_pr_lookup(explorer);
            explorer.set_status("refreshed");
        }
        KeyCode::Char('f') if ctrl => {
            // The highlighted row, or the folder being listed when the filter
            // has left nothing highlighted.
            let target = explorer
                .selected_entry()
                .map(|e| e.path.clone())
                .unwrap_or_else(|| explorer.cwd.clone());
            view_diff(app, terminal, explorer, &target)?;
        }
        KeyCode::Char('p') if ctrl => open_pr_panel(app, terminal, explorer, false),
        KeyCode::Char('o') if ctrl => open_repo_prs(app, terminal, explorer, None),
        KeyCode::Char('a') if ctrl => launch_agent(app, terminal, explorer, &[])?,
        KeyCode::Char('s') if ctrl => open_sessions(explorer),
        KeyCode::Char('w') if ctrl => open_worktrees(app, terminal, explorer),
        // F1 alone would do, except macOS gives it to the media keys by
        // default and some terminals keep it for their own menu. ctrl-g is
        // "get help" in nano, and it always arrives.
        KeyCode::F(1) => explorer.overlay = Some(Overlay::Help { scroll: 0 }),
        KeyCode::Char('g') if ctrl => explorer.overlay = Some(Overlay::Help { scroll: 0 }),

        // Filter editing.
        KeyCode::Tab => {
            // "Nothing more to complete" is true of a filter that matches
            // nothing, and useless: say what → and ⏎ say instead.
            let said = if explorer.complete() {
                ""
            } else if explorer.visible_len() == 0 {
                nothing_to_act_on(explorer)
            } else {
                "nothing more to complete"
            };
            explorer.set_status(said);
        }
        KeyCode::Backspace => {
            // Nothing to delete: leave whatever the status line was saying
            // rather than blanking an error the user has not read yet.
            if explorer.pop_filter() {
                explorer.set_status("");
            }
        }
        // A path separator means "go in", the way it does while typing a path.
        // `/` cannot appear in a Unix filename, so it always navigates.
        KeyCode::Char('/') if !ctrl => {
            let step = explorer.descend_typed()?;
            report_step(explorer, step);
        }
        // `\` can, so it types whenever there is still a name it could be part
        // of — `weird\name.txt` stays reachable even beside a `weird/` — and
        // means "go in" only when there is not.
        KeyCode::Char('\\') if !ctrl => {
            if explorer.filter_would_match('\\') {
                explorer.push_filter('\\');
                explorer.set_status("");
            } else {
                // Nothing it could be part of, so it means "go in" — and when
                // there is nothing to go into either, say so rather than
                // typing a character that is guaranteed to match nothing.
                let step = explorer.descend_typed()?;
                report_step(explorer, step);
            }
        }
        KeyCode::Esc => {
            // Escape backs out of what you typed before it backs out of jeet.
            // Clearing takes the status with it: a "nothing matches" left over
            // the full listing describes a state that is no longer on screen.
            if explorer.clear_filter() {
                explorer.set_status("");
            } else {
                explorer.quit_in_place();
            }
        }

        // Navigation, which never collides with typing.
        KeyCode::Up => explorer.move_cursor(-1),
        KeyCode::Down => explorer.move_cursor(1),
        KeyCode::PageUp => explorer.move_cursor(-10),
        KeyCode::PageDown => explorer.move_cursor(10),
        KeyCode::Home => explorer.select_first(),
        KeyCode::End => explorer.select_last(),
        KeyCode::Right => {
            let step = explorer.descend_typed()?;
            report_step(explorer, step);
        }
        KeyCode::Left => {
            if explorer.ascend()? {
                explorer.set_status("");
            }
        }
        KeyCode::Enter => enter_selected(app, terminal, explorer)?,

        // Anything else printable is filter input.
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            explorer.push_filter(c);
            explorer.set_status("");
        }
        _ => {}
    }
    Ok(())
}

/// ⏎ and a click on the highlighted row: folders open, files go to the editor.
fn enter_selected(app: &App, terminal: &mut Tui, explorer: &mut Explorer) -> Result<()> {
    let Some(entry) = explorer.selected_entry().cloned() else {
        explorer.set_status(nothing_to_act_on(explorer));
        return Ok(());
    };
    if entry.is_dir {
        explorer.descend()?;
        explorer.set_status("");
    } else {
        open_editor(app, terminal, explorer, &entry.path)?;
    }
    Ok(())
}

/// Clicks and the wheel, while the browser is in front.
///
/// Overlays are keyboard-driven, so a click that lands on one is swallowed
/// rather than acting on the list hidden behind it.
fn handle_mouse(terminal: &mut Tui, explorer: &mut Explorer, mouse: MouseEvent) -> Result<()> {
    if explorer.overlay.is_some() || explorer.working.is_some() {
        return Ok(());
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && clicked_pr(explorer, mouse.column, mouse.row)
    {
        open_pr_in_browser(terminal, explorer);
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => explorer.move_cursor(-1),
        MouseEventKind::ScrollDown => explorer.move_cursor(1),
        MouseEventKind::Down(MouseButton::Left) => {
            // A double-click is two presses. Without this the first enters the
            // folder and the second enters whatever the child listing put back
            // under the pointer — which, directories sorting first, is usually
            // another folder.
            if explorer.is_double_click(mouse.column, mouse.row) {
                return Ok(());
            }
            if let Some(dir) = clicked_breadcrumb(explorer, mouse.column, mouse.row) {
                // Lexical, like `ascend`: a symlink that resolves back to a
                // parent (`ln -s . loop`) is a directory you can be inside,
                // and canonicalising here would refuse to let you climb out.
                if dir != explorer.cwd {
                    // Land on the folder we came out of, the way ← does, so a
                    // crumb click can be undone by pressing → straight back.
                    let came_from = descendant_of(&dir, &explorer.cwd);
                    explorer.show(dir, came_from.as_deref())?;
                    explorer.note_navigating_click(mouse.column, mouse.row);
                    explorer.set_status("");
                }
            } else if let Some(row) = clicked_row(explorer, mouse.column, mouse.row) {
                if explorer.select_visible(row) {
                    // A folder opens on a single click — that is what clicking
                    // a folder means everywhere else. Files only get selected;
                    // handing a file to an editor is too much for a stray click.
                    let is_dir = explorer.selected_entry().map(|e| e.is_dir) == Some(true);
                    if is_dir {
                        explorer.descend()?;
                        explorer.note_navigating_click(mouse.column, mouse.row);
                    }
                    explorer.set_status("");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Which visible row a click landed on, accounting for the border and for how
/// far the list has scrolled.
fn clicked_row(explorer: &Explorer, column: u16, row: u16) -> Option<usize> {
    let area = explorer.list_area;
    let inside = column > area.x
        && column < area.x + area.width.saturating_sub(1)
        && row > area.y
        && row < area.y + area.height.saturating_sub(1);
    if !inside {
        return None;
    }
    let offset = explorer.list.offset();
    Some(offset + (row - area.y - 1) as usize)
}

/// The child of `dir` that `inside` sits under, if any — the row to leave the
/// cursor on when climbing out to `dir`.
fn descendant_of(dir: &Path, inside: &Path) -> Option<PathBuf> {
    let rest = inside.strip_prefix(dir).ok()?;
    let first = rest.components().next()?;
    Some(dir.join(first.as_os_str()))
}

fn clicked_pr(explorer: &Explorer, column: u16, row: u16) -> bool {
    explorer
        .pr_area
        .is_some_and(|area| row == area.y && column >= area.x && column < area.x + area.width)
}

/// The ancestor directory a click on the header's path line points at.
fn clicked_breadcrumb(explorer: &Explorer, column: u16, row: u16) -> Option<PathBuf> {
    let area = explorer.breadcrumb_area?;
    if row != area.y || column < area.x || column >= area.x + area.width {
        return None;
    }
    explorer.breadcrumb_target((column - area.x) as usize)
}

fn handle_overlay_key(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    overlay: Overlay,
    key: KeyEvent,
) -> Result<()> {
    // A panel's own key closes it again, the way it did when these were bare
    // letters — the overlay is already taken, so returning drops it.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let toggled_shut = match (&overlay, key.code) {
        (Overlay::Worktrees { .. }, KeyCode::Char('w')) => ctrl,
        (Overlay::Sessions { .. }, KeyCode::Char('s')) => ctrl,
        (Overlay::PullRequest { .. }, KeyCode::Char('p')) => ctrl,
        (Overlay::RepoPrs { .. }, KeyCode::Char('o')) => ctrl,
        (Overlay::Help { .. }, KeyCode::Char('g')) => ctrl,
        (Overlay::Help { .. }, KeyCode::F(1)) => true,
        _ => false,
    };
    if toggled_shut {
        return Ok(());
    }

    // Otherwise the browse-mode shortcuts mean nothing in a panel, and letting
    // them through makes ctrl-d ask to delete a worktree and ctrl-e create one.
    // The prompts are the exception: they take typed input, and ctrl-u clears.
    let typing = matches!(
        overlay,
        Overlay::NewWorktree { .. } | Overlay::RenameWorktree { .. } | Overlay::ReviewBody { .. }
    );
    if !typing && ctrl {
        explorer.overlay = Some(overlay);
        return Ok(());
    }
    match overlay {
        Overlay::Help { scroll } => {
            // On a terminal too small for the whole list, the rows wrap past
            // the bottom of the panel; scrolling is how the rest is reached.
            // If the terminal cannot be measured, hold the scroll where it is
            // rather than let the `?` take the panel down with it.
            let max = match terminal.size() {
                Ok(size) => ui::help_geometry(Rect::new(0, 0, size.width, size.height)).1,
                Err(_) => scroll,
            };
            let moved = match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return Ok(()),
                // Clamped on the way up as well as down: a stored scroll left
                // over from a smaller terminal would otherwise take several
                // presses to come back into range, looking dead throughout.
                KeyCode::Up => scroll.min(max).saturating_sub(1),
                KeyCode::Down => (scroll + 1).min(max),
                KeyCode::PageUp | KeyCode::Home => 0,
                KeyCode::PageDown | KeyCode::End => max,
                _ => scroll,
            };
            explorer.overlay = Some(Overlay::Help { scroll: moved });
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
            KeyCode::Char('o') => {
                match explorer.worktree_rows.get(selected).and_then(|row| row.pr) {
                    Some(number) => {
                        let trunk = PathBuf::from(&explorer.repo.trunk_path);
                        let opened =
                            with_progress(terminal, explorer, "opening the browser", || {
                                github::open_in_browser(&trunk, number)
                            })?;
                        match opened {
                            Ok(()) => {
                                explorer.set_status(format!("opened #{number} in the browser"))
                            }
                            Err(e) => {
                                explorer.set_status(format!("could not open #{number}: {e:#}"))
                            }
                        }
                    }
                    None => explorer.set_status("no open pull request from this worktree's branch"),
                }
                explorer.overlay = Some(Overlay::Worktrees { selected });
            }
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
        Overlay::PullRequest {
            pending,
            pending_error,
        } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Char(c @ ('a' | 'c' | 'x')) => {
                let verdict = match c {
                    'a' => Verdict::Approve,
                    'c' => Verdict::Comment,
                    _ => Verdict::RequestChanges,
                };
                explorer.overlay = Some(Overlay::ReviewBody {
                    verdict,
                    input: String::new(),
                    pending,
                });
            }
            KeyCode::Char('o') => {
                open_pr_in_browser(terminal, explorer);
                explorer.overlay = Some(Overlay::PullRequest {
                    pending,
                    pending_error,
                });
            }
            KeyCode::Char('d') => {
                let root = explorer.root.clone();
                view_diff(app, terminal, explorer, &root)?;
            }
            KeyCode::Char('v') => {
                match explorer.review_job.as_ref().map(|job| job.log.clone()) {
                    Some(log) => open_editor(app, terminal, explorer, &log)?,
                    None => explorer.set_status("no review has been run for this pull request"),
                }
                explorer.overlay = Some(Overlay::PullRequest {
                    pending,
                    pending_error,
                });
            }
            KeyCode::Char('r') => open_pr_panel(app, terminal, explorer, true),
            _ => {
                explorer.overlay = Some(Overlay::PullRequest {
                    pending,
                    pending_error,
                })
            }
        },
        Overlay::RepoPrs { rows, mut selected } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(rows.len().saturating_sub(1));
                explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
            }
            KeyCode::PageUp | KeyCode::Home => {
                explorer.overlay = Some(Overlay::RepoPrs { rows, selected: 0 });
            }
            KeyCode::PageDown | KeyCode::End => {
                let selected = rows.len().saturating_sub(1);
                explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
            }
            KeyCode::Enter | KeyCode::Right => match rows.get(selected).map(|r| r.pr.number) {
                Some(number) => check_out_pr(app, terminal, explorer, number, rows, selected)?,
                None => explorer.overlay = Some(Overlay::RepoPrs { rows, selected }),
            },
            KeyCode::Char('o') => {
                if let Some(number) = rows.get(selected).map(|r| r.pr.number) {
                    let trunk = PathBuf::from(&explorer.repo.trunk_path);
                    let opened = with_progress(terminal, explorer, "opening the browser", || {
                        github::open_in_browser(&trunk, number)
                    })?;
                    match opened {
                        Ok(()) => explorer.set_status(format!("opened #{number} in the browser")),
                        Err(e) => explorer.set_status(format!("could not open #{number}: {e:#}")),
                    }
                }
                explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
            }
            KeyCode::Char('r') => {
                let keep = rows.get(selected).map(|r| r.pr.number);
                open_repo_prs(app, terminal, explorer, keep);
            }
            _ => explorer.overlay = Some(Overlay::RepoPrs { rows, selected }),
        },
        Overlay::ReviewBody {
            verdict,
            mut input,
            pending,
        } => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match key.code {
                KeyCode::Esc => {
                    explorer.overlay = Some(Overlay::PullRequest {
                        pending,
                        pending_error: None,
                    });
                    return Ok(());
                }
                // A newline: ctrl-j is a line feed in every terminal; alt-⏎
                // where the terminal reports it.
                KeyCode::Char('j') if ctrl => input.push('\n'),
                KeyCode::Enter if alt => input.push('\n'),
                KeyCode::Enter => {
                    return submit_review(terminal, explorer, verdict, input, pending);
                }
                KeyCode::Char('u') if ctrl => input.clear(),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !ctrl && !alt => input.push(c),
                _ => {}
            }
            explorer.overlay = Some(Overlay::ReviewBody {
                verdict,
                input,
                pending,
            });
        }
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

    let trunk = PathBuf::from(&repo.trunk_path);
    let (mut rows, prs) = std::thread::scope(|scope| {
        // One request for the whole repository, alongside the git work; a
        // machine without `gh` just has no PR column.
        let prs = scope.spawn(|| github::open_prs_by_branch(&trunk).unwrap_or_default());
        let handles: Vec<_> = lanes
            .into_iter()
            .map(|lane| {
                let base = base.clone();
                scope.spawn(move || {
                    lane.into_iter()
                        .map(|entry| WorktreeRow {
                            status: worktrees::status_against(&entry, &base),
                            current: crate::resolve::same_path(&entry.path, current_root),
                            pr: None,
                            entry,
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let rows = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect::<Vec<_>>();
        (rows, prs.join().unwrap_or_default())
    });
    for row in &mut rows {
        row.pr = row.entry.branch.as_ref().and_then(|branch| {
            prs.iter()
                .find(|(head, _)| head == branch)
                .map(|(_, number)| *number)
        });
    }
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
                    "press ctrl-a to start one".into(),
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
    // Listing first, and through `show` rather than by hand: it bails before
    // touching anything if the new root cannot be read, and it is the only
    // thing that keeps the filter and the visible rows in step with `entries`.
    explorer.show(path.to_path_buf(), None)?;
    let (label, kind, status) = describe_root(app, &explorer.repo, path);
    explorer.root = path.to_path_buf();
    explorer.root_label = label;
    explorer.root_kind = kind;
    explorer.root_status = status;
    explorer.overlay = None;
    refresh_diffs(explorer);
    start_pr_lookup(explorer);
    Ok(())
}

fn refresh_root_status(app: &App, explorer: &mut Explorer) {
    let (label, kind, status) = describe_root(app, &explorer.repo, &explorer.root.clone());
    explorer.root_label = label;
    explorer.root_kind = kind;
    explorer.root_status = status;
    refresh_diffs(explorer);
}

/// Per-file line counts against the merge base with the default branch —
/// the same base, and the same working-tree-inclusive diff, as the header's
/// counter, so the listing adds up to what the header says.
fn refresh_diffs(explorer: &mut Explorer) {
    let base = worktrees::comparison_base(&explorer.repo);
    let merge_base = crate::git::merge_base(&explorer.root, &base);
    let files = merge_base
        .as_deref()
        .and_then(|sha| crate::git::numstat_by_file(&explorer.root, sha))
        .unwrap_or_default();
    explorer.diffs = DiffMap::new(files, merge_base);
}

/// Look the worktree's PR up on a background thread; the event loop picks
/// the answer up when it lands, so opening the explorer never waits on it.
fn start_pr_lookup(explorer: &mut Explorer) {
    let root = explorer.root.clone();
    let default_branch = explorer.repo.default_branch.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let lookup = match crate::git::head_branch(&root) {
            // The default branch is where PRs go, not where they come from.
            Some(branch) if branch != default_branch => {
                match github::pr_for_branch(&root, &branch) {
                    Ok(Some(pr)) => PrLookup::Found(Box::new(pr)),
                    Ok(None) => PrLookup::Missing,
                    Err(e) => PrLookup::Failed(format!("{e:#}")),
                }
            }
            _ => PrLookup::Missing,
        };
        let _ = tx.send((root, lookup));
    });
    explorer.pr = PrLookup::Loading;
    explorer.pr_updates = Some(rx);
}

fn take_pr_update(app: &App, explorer: &mut Explorer) {
    let Some(updates) = &explorer.pr_updates else {
        return;
    };
    match updates.try_recv() {
        Ok((root, lookup)) => {
            explorer.pr_updates = None;
            // Answering for a worktree we have since switched away from.
            if root == explorer.root {
                explorer.pr = lookup;
                load_review_job(app, explorer);
            }
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            explorer.pr_updates = None;
            explorer.pr = PrLookup::Failed("the pull request lookup stopped".into());
        }
    }
}

fn load_review_job(app: &App, explorer: &mut Explorer) {
    explorer.review_job = match &explorer.pr {
        PrLookup::Found(pr) => review::load(app, &explorer.repo, pr.number),
        _ => None,
    };
}

/// ctrl-p: the PR panel, reading the user's pending review on the way in.
/// `refetch` also re-reads the PR itself, for `r` inside the panel.
fn open_pr_panel(app: &App, terminal: &mut Tui, explorer: &mut Explorer, refetch: bool) {
    let branch = crate::git::head_branch(&explorer.root);
    let pr = match (&explorer.pr, refetch) {
        (PrLookup::Found(pr), false) => Some((**pr).clone()),
        (PrLookup::Loading, false) => {
            explorer.set_status("still looking up the pull request…");
            return;
        }
        (PrLookup::Failed(why), false) => {
            explorer.overlay = Some(Overlay::Message {
                title: "pull request".into(),
                lines: vec!["could not look the pull request up:".into(), why.clone()],
                from_panel: false,
            });
            return;
        }
        _ => None,
    };
    let root = explorer.root.clone();
    let fetched = with_progress(terminal, explorer, "reading the pull request", || {
        let pr = match pr {
            Some(pr) => Ok(Some(pr)),
            None => match &branch {
                Some(branch) => github::pr_for_branch(&root, branch),
                None => Ok(None),
            },
        };
        let pending = match &pr {
            Ok(Some(pr)) => Some(github::pending_review(&root, pr.number)),
            _ => None,
        };
        (pr, pending)
    });
    let (pr, pending) = match fetched {
        Ok(fetched) => fetched,
        Err(e) => {
            explorer.set_status(format!("error: {e}"));
            return;
        }
    };
    match pr {
        Ok(Some(pr)) => {
            explorer.pr = PrLookup::Found(Box::new(pr));
            load_review_job(app, explorer);
            let (pending, pending_error) = match pending {
                Some(Ok(pending)) => (pending, None),
                Some(Err(e)) => (None, Some(format!("{e:#}"))),
                None => (None, None),
            };
            explorer.overlay = Some(Overlay::PullRequest {
                pending,
                pending_error,
            });
        }
        Ok(None) => {
            explorer.pr = PrLookup::Missing;
            explorer.overlay = Some(Overlay::Message {
                title: "pull request".into(),
                lines: vec![
                    match branch {
                        Some(branch) => format!("no pull request from {branch}"),
                        None => "a detached worktree has no pull request".into(),
                    },
                    String::new(),
                    "ctrl-o lists the repository's open pull requests".into(),
                ],
                from_panel: false,
            });
        }
        Err(e) => {
            explorer.pr = PrLookup::Failed(format!("{e:#}"));
            explorer.overlay = Some(Overlay::Message {
                title: "pull request".into(),
                lines: vec![format!("{e:#}")],
                from_panel: false,
            });
        }
    }
}

/// ctrl-o: the repository's open PRs, with the cursor on `keep` if it is
/// still open, else on this worktree's own PR, else at the top.
fn open_repo_prs(app: &App, terminal: &mut Tui, explorer: &mut Explorer, keep: Option<u64>) {
    let repo = explorer.repo.clone();
    let rows = with_progress(terminal, explorer, "reading pull requests", || {
        crate::commands::prs::rows(app, &repo)
    })
    .and_then(|inner| inner);
    match rows {
        Ok(rows) if rows.is_empty() => {
            explorer.overlay = Some(Overlay::Message {
                title: "pull requests".into(),
                lines: vec![format!("{} has no open pull requests", explorer.repo.id)],
                from_panel: false,
            });
        }
        Ok(rows) => {
            let current = match &explorer.pr {
                PrLookup::Found(pr) => Some(pr.number),
                _ => None,
            };
            let selected = keep
                .or(current)
                .and_then(|n| rows.iter().position(|r| r.pr.number == n))
                .unwrap_or(0);
            explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
        }
        Err(e) => {
            explorer.overlay = Some(Overlay::Message {
                title: "pull requests".into(),
                lines: vec![format!("{e:#}")],
                from_panel: false,
            });
        }
    }
}

/// ⏎ in the PR list: check the PR out — or find the worktree that already
/// has it — and browse it. Deliberately not `jeet review`: no review command
/// starts from here, this is only a way in.
fn check_out_pr(
    app: &App,
    terminal: &mut Tui,
    explorer: &mut Explorer,
    number: u64,
    rows: Vec<crate::commands::prs::PrRow>,
    selected: usize,
) -> Result<()> {
    let repo = explorer.repo.clone();
    let outcome = with_progress(
        terminal,
        explorer,
        &format!("checking out #{number}"),
        || {
            let pr = github::pr_view(std::path::Path::new(&repo.trunk_path), number)?;
            worktrees::checkout_pr(app, &repo, &pr)
        },
    )?;
    match outcome {
        Ok(checked_out) => {
            switch_worktree(app, explorer, &checked_out.path)?;
            let mut status = format!("checked out #{number}");
            if !checked_out.warnings.is_empty() {
                status.push_str(&format!(" — {}", checked_out.warnings.join("; ")));
            }
            explorer.set_status(status);
        }
        Err(e) => {
            // Back to the list, so the next PR is one keystroke away.
            explorer.set_status(format!("could not check out #{number}: {e:#}"));
            explorer.overlay = Some(Overlay::RepoPrs { rows, selected });
        }
    }
    Ok(())
}

fn open_pr_in_browser(terminal: &mut Tui, explorer: &mut Explorer) {
    let PrLookup::Found(pr) = &explorer.pr else {
        return;
    };
    let number = pr.number;
    let root = explorer.root.clone();
    let opened = with_progress(terminal, explorer, "opening the browser", || {
        github::open_in_browser(&root, number)
    });
    match opened {
        Ok(Ok(())) => explorer.set_status(format!("opened #{number} in the browser")),
        Ok(Err(e)) | Err(e) => explorer.set_status(format!("could not open #{number}: {e:#}")),
    }
}

/// Post the review. On failure the text typed stays where it was, so a
/// rejected approval (your own PR, say) does not cost the comment with it.
fn submit_review(
    terminal: &mut Tui,
    explorer: &mut Explorer,
    verdict: Verdict,
    input: String,
    pending: Option<github::PendingReview>,
) -> Result<()> {
    let PrLookup::Found(pr) = &explorer.pr else {
        return Ok(());
    };
    let number = pr.number;
    let body = input.trim().to_string();
    // GitHub refuses a comment or a change request that says nothing; better
    // to say so here than after the round trip.
    if body.is_empty() && verdict != Verdict::Approve && pending.is_none() {
        explorer.set_status(format!("{} needs a comment to go with it", verdict.label()));
        explorer.overlay = Some(Overlay::ReviewBody {
            verdict,
            input,
            pending,
        });
        return Ok(());
    }
    let root = explorer.root.clone();
    let submitting = pending.clone();
    let outcome = with_progress(
        terminal,
        explorer,
        &format!("submitting: {} #{number}", verdict.label()),
        move || github::submit_review(&root, number, verdict, &body, submitting.as_ref()),
    )?;
    match outcome {
        Ok(()) => {
            explorer.set_status(match verdict {
                Verdict::Approve => format!("approved #{number}"),
                Verdict::Comment => format!("commented on #{number}"),
                Verdict::RequestChanges => format!("requested changes on #{number}"),
            });
            start_pr_lookup(explorer);
        }
        Err(e) => {
            explorer.set_status(format!("not submitted: {e:#}"));
            explorer.overlay = Some(Overlay::ReviewBody {
                verdict,
                input,
                pending,
            });
        }
    }
    Ok(())
}

/// ctrl-f: step through the diff of a file or folder against the merge base,
/// in whatever `git difftool` is set up to use.
fn view_diff(app: &App, terminal: &mut Tui, explorer: &mut Explorer, path: &Path) -> Result<()> {
    let Some(base) = explorer.diffs.merge_base.clone() else {
        explorer.set_status(format!(
            "no common history with {} to diff against",
            explorer.repo.default_branch
        ));
        return Ok(());
    };
    let root = explorer.root.clone();
    let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
    let whole = rel.as_os_str().is_empty();
    let changed = if whole {
        !explorer.diffs.is_empty()
    } else {
        explorer.diffs.get(&root, path).is_some()
    };
    let shown = if whole {
        "this worktree".to_string()
    } else {
        rel.display().to_string()
    };
    if !changed {
        explorer.set_status(format!(
            "no changes in {shown} against {}",
            explorer.repo.default_branch
        ));
        return Ok(());
    }

    let mut argv: Vec<String> = ["git", "difftool", "--no-prompt", "--trust-exit-code"]
        .into_iter()
        .map(String::from)
        .collect();
    // With no diff.tool configured git guesses, and on a Mac with Xcode its
    // first guess is a GUI. Default to the terminal diff of the user's editor.
    if crate::git::configured_diff_tool(&root).is_none() {
        argv.push(format!("--tool={}", default_diff_tool(app)));
    }
    argv.push(base);
    argv.push("--".into());
    argv.push(if whole {
        ".".into()
    } else {
        rel.to_string_lossy().to_string()
    });
    let outcome = suspended(terminal, || agent::run_in(&argv, &root, &[]))?;
    match outcome {
        Ok(0) => explorer.set_status(format!("closed the diff of {shown}")),
        // `--trust-exit-code`: `:cq` in vimdiff stops the rest of the files.
        Ok(_) => explorer.set_status(format!("stopped the diff of {shown}")),
        Err(e) => explorer.set_status(format!("could not run git difftool: {e}")),
    }
    Ok(())
}

/// `nvimdiff` for a neovim user, `vimdiff` for everyone else.
fn default_diff_tool(app: &App) -> &'static str {
    let editor = agent::editor_argv(&app.config).unwrap_or_default();
    let program = editor
        .first()
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if program == "nvim" {
        "nvimdiff"
    } else {
        "vimdiff"
    }
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
    refresh_diffs(explorer);
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
