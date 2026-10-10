//! The terminal every jeet screen draws on: raw mode and the alternate
//! screen going up and coming down, the safety net that puts them back on a
//! panic or a signal, stepping out of the way for a program that wants the
//! terminal to itself, and a spinner while slow work runs off the draw path.
//!
//! The explorer, the review viewer and the tasks list all share these, so a
//! screen opened from another one hands the same terminal on rather than
//! tearing it down and putting it up again.

use std::io::{self, Stdout};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event;
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{Frame, Terminal};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Ask the terminal for button and wheel reports, in SGR encoding.
///
/// Not crossterm's `EnableMouseCapture`: that also turns on `?1003h`,
/// any-motion tracking, and then every wipe of the pointer across the window
/// wakes the event loop and redraws the whole listing for nothing. `?1002h`
/// reports presses, releases and drags, which is all we act on.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1015l\x1b[?1002l\x1b[?1000l";

pub fn init_terminal() -> Result<Tui> {
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

pub fn restore_terminal(terminal: &mut Tui) -> Result<()> {
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

/// Drop out of the alternate screen, run `f` — an editor, a diff tool, a
/// tmux attach — then put the screen back.
pub fn suspended<T>(terminal: &mut Tui, f: impl FnOnce() -> T) -> Result<T> {
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

/// Run `job` on a background thread while the screen keeps drawing.
///
/// git is slow and the network is slower — a `git push` on worktree creation
/// froze the whole explorer for as long as the remote took, with nothing on
/// screen to say why. `draw` paints the screen with the spinner text it is
/// given. Keystrokes that arrive meanwhile are dropped rather than queued, so
/// they cannot fire against a screen the user never saw.
pub fn with_progress<T: Send>(
    terminal: &mut Tui,
    label: &str,
    mut draw: impl FnMut(&mut Frame, &str),
    job: impl FnOnce() -> T + Send,
) -> Result<T> {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| -> Result<T> {
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
            let working = format!(" {} {label} ", FRAMES[tick % FRAMES.len()]);
            terminal.draw(|frame| draw(frame, &working))?;
            tick += 1;
            while event::poll(Duration::from_millis(0))? {
                let _ = event::read();
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    })
}
