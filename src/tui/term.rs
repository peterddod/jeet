//! The terminal every jeet screen draws on: raw mode and the alternate
//! screen going up and coming down, the safety net that puts them back on a
//! panic or a signal, stepping out of the way for a program that wants the
//! terminal to itself, and a spinner while slow work runs off the draw path.
//!
//! The explorer, the review viewer and the tasks list all share these, so a
//! screen opened from another one hands the same terminal on rather than
//! tearing it down and putting it up again.

use std::io::{self, Stdout, Write};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::{MoveTo, Show};
use ratatui::crossterm::event;
use ratatui::crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{execute, queue};
use ratatui::style::Modifier;
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

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

/// `width` columns from (`x`, `y`) that link to `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub url: String,
}

/// `url`, if it is fit to put in an OSC 8 sequence: a web address with
/// nothing in it that could end the sequence early and start another.
pub fn link_target(url: &str) -> Option<&str> {
    let web = url.starts_with("https://") || url.starts_with("http://");
    (web && url.chars().all(|c| c.is_ascii_graphic())).then_some(url)
}

/// Make `links` clickable in what was just drawn, as OSC 8 hyperlinks.
pub fn paint_links(terminal: &mut Tui, buffer: &Buffer, links: &[Link]) -> Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    let out = terminal.backend_mut();
    write_links(out, buffer, links).context("draw links")?;
    out.flush().context("draw links")
}

/// Write each link's cells again, exactly as `buffer` has them, between the
/// sequences that open and close the link.
///
/// Not through the buffer: ratatui measures a cell by its symbol, and a
/// symbol carrying an escape sequence measures as wide as the URL, which
/// throws out everything after it on the line. Written over the top instead,
/// the buffer never knows, and as the cells have not changed it leaves them
/// be until something else is drawn there.
pub fn write_links(out: &mut impl Write, buffer: &Buffer, links: &[Link]) -> io::Result<()> {
    for link in links {
        let Some(url) = link_target(&link.url) else {
            continue;
        };
        queue!(
            out,
            MoveTo(link.x, link.y),
            Print(format!("\x1b]8;;{url}\x1b\\"))
        )?;
        let mut covered = 0;
        for x in link.x..link.x.saturating_add(link.width) {
            let Some(cell) = buffer.cell((x, link.y)) else {
                break;
            };
            // The columns a wide character spans hold blanks the terminal
            // fills in itself; writing them would push the line along.
            if covered > 0 {
                covered -= 1;
                continue;
            }
            covered = cell.symbol().width().saturating_sub(1);
            queue!(
                out,
                SetAttribute(Attribute::Reset),
                SetForegroundColor(cell.fg.into()),
                SetBackgroundColor(cell.bg.into())
            )?;
            for attribute in attributes(cell.modifier) {
                queue!(out, SetAttribute(attribute))?;
            }
            queue!(out, Print(cell.symbol()))?;
        }
        queue!(
            out,
            Print("\x1b]8;;\x1b\\"),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
    }
    Ok(())
}

/// The terminal attributes that draw `modifier`.
fn attributes(modifier: Modifier) -> Vec<Attribute> {
    [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINED, Attribute::Underlined),
        (Modifier::SLOW_BLINK, Attribute::SlowBlink),
        (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        (Modifier::REVERSED, Attribute::Reverse),
        (Modifier::HIDDEN, Attribute::Hidden),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
    ]
    .into_iter()
    .filter(|(m, _)| modifier.contains(*m))
    .map(|(_, a)| a)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    #[test]
    fn only_plain_web_addresses_become_links() {
        let url = "https://github.com/acme/api/pull/7";
        assert_eq!(link_target(url), Some(url));
        assert!(link_target("http://ghe.local/a/b/pull/1").is_some());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://x/\x1b]8;;https://evil\x1b\\",
            "https://x/\x07",
            "https://x/a b",
            "https://x/ü",
            "",
        ] {
            assert_eq!(link_target(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_link_rewrites_its_cells_as_drawn_inside_osc_8() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(2, 1, "a界b", Style::default().fg(Color::Red));
        let link = Link {
            x: 2,
            y: 1,
            width: 4,
            url: "https://github.com/acme/api/pull/7".into(),
        };
        let mut out = Vec::new();
        write_links(&mut out, &buffer, std::slice::from_ref(&link)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("\x1b[2;3H\x1b]8;;https://github.com/acme/api/pull/7\x1b\\"),
            "{text:?}"
        );
        assert!(
            text.contains("\x1b]8;;\x1b\\"),
            "the link is closed: {text:?}"
        );
        // Each character once, and not the blank under the wide one: no
        // escape sequence here has a space in it, so any space is that blank.
        assert_eq!(text.matches('界').count(), 1, "{text:?}");
        assert!(!text.contains(' '), "{text:?}");
        assert!(text.contains('a') && text.contains('b'), "{text:?}");

        // Nothing at all for a URL that could break out of the sequence.
        let mut out = Vec::new();
        let evil = Link {
            url: "https://x/\x1b[2J".into(),
            ..link
        };
        write_links(&mut out, &buffer, &[evil]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn modifiers_map_to_attributes() {
        assert_eq!(
            attributes(Modifier::BOLD | Modifier::REVERSED),
            [Attribute::Bold, Attribute::Reverse]
        );
        assert!(attributes(Modifier::empty()).is_empty());
    }
}
