//! `jeet open <url>`: take the user to what a `jeet://` link names.
//!
//! The nearest thing wins: a jeet screen that is open is sent to the link
//! and brought forward; failing that, a task's or review's tmux session is
//! switched to on the terminal already attached to jeet's; failing that,
//! the screen is run here if this is a terminal, and else in a terminal
//! that is started. It acts on the user's screen, so it is the user's own
//! terminal's alone.

use std::io::{self, IsTerminal};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::commands::tasks;
use crate::context::App;
use crate::link::registry::{self, Screen};
use crate::link::terminal::{self, Choice, STARTED_ENV};
use crate::link::{self, Link};
use crate::permissions;
use crate::runtime;
use crate::tmux::{self, Client};

pub fn run(app: &App, url: &str) -> Result<()> {
    if !permissions::at_terminal(app)? {
        bail!("jeet open acts on your screen: run it from your own terminal, not a jeet pane");
    }
    let link = link::parse(url)?;
    let screens = registry::live(&app.home);
    let clients = tmux::clients();
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let setting = app.config.notify.terminal.as_deref();
    match plan(&link, &screens, &clients, tmux::has_session, interactive) {
        Plan::Screen { screen, bring } => {
            registry::request(&app.home, screen.pid, &link)?;
            let said = match &bring {
                Bring::Pane { client, pane } => {
                    tmux::switch_client(client, pane).map(|()| "switched tmux to its pane")
                }
                Bring::Detach { client } => {
                    tmux::detach_client(client).map(|()| "detached tmux back to it")
                }
                Bring::Nothing => Ok("sent"),
            };
            let said = said.unwrap_or_else(|e| {
                eprintln!("jeet: could not bring it forward: {e:#}");
                "sent"
            });
            activate(screen.app.as_deref(), setting);
            println!("{said}: {link} on the jeet screen (pid {})", screen.pid);
            Ok(())
        }
        Plan::Session { client, session } => {
            tmux::switch_client(&client, &tmux::session_target(&session))?;
            activate(None, setting);
            println!("switched tmux to {session}");
            Ok(())
        }
        Plan::Here => tasks::run_link(app, link),
        Plan::Terminal => start_terminal(app, &link),
    }
}

/// How a screen that is open is brought in front of the user.
#[derive(Debug, PartialEq, Eq)]
enum Bring {
    /// It runs in a tmux pane: switch the client the user is at to it.
    Pane { client: String, pane: String },
    /// It is suspended under a `tmux attach` to an agent: detach that client
    /// and it comes back.
    Detach { client: String },
    /// There is no telling where it is; the link is sent and that is all.
    Nothing,
}

/// What `jeet open` does.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Send the link to a screen that is open.
    Screen { screen: Screen, bring: Bring },
    /// Switch the client attached to jeet's tmux to the session of a task or
    /// review.
    Session { client: String, session: String },
    /// Run the screen in this terminal.
    Here,
    /// Start a terminal to run it in.
    Terminal,
}

/// Decide what to do, given the screens that are open (any order), tmux's
/// clients, whether a tmux session exists, and whether this is a terminal.
fn plan(
    link: &Link,
    screens: &[Screen],
    clients: &[Client],
    session_exists: impl Fn(&str) -> bool,
    interactive: bool,
) -> Plan {
    if let Some(screen) = screens.iter().max_by_key(|s| (s.started, s.pid)) {
        let bring = match (&screen.tmux_pane, &screen.tty) {
            (Some(pane), _) => tmux::most_recent(clients)
                .map(|client| Bring::Pane {
                    client: client.name.clone(),
                    pane: pane.clone(),
                })
                .unwrap_or(Bring::Nothing),
            (None, Some(tty)) => clients
                .iter()
                .find(|client| client.tty == *tty)
                .map(|client| Bring::Detach {
                    client: client.name.clone(),
                })
                .unwrap_or(Bring::Nothing),
            (None, None) => Bring::Nothing,
        };
        return Plan::Screen {
            screen: screen.clone(),
            bring,
        };
    }
    let slug = match link {
        Link::Task(slug) | Link::Review(slug) => Some(slug),
        Link::Report(_) => None,
    };
    if let Some(slug) = slug {
        let session = runtime::session_name_for(slug);
        let attached: Vec<Client> = clients
            .iter()
            .filter(|c| c.session.starts_with(tmux::SESSION_PREFIX))
            .cloned()
            .collect();
        if let Some(client) = tmux::most_recent(&attached) {
            if session_exists(&session) {
                return Plan::Session {
                    client: client.name.clone(),
                    session,
                };
            }
        }
    }
    match interactive {
        true => Plan::Here,
        false => Plan::Terminal,
    }
}

/// Bring the terminal forward on macOS, best effort: the app hosting the
/// screen when it said which, else the configured terminal's.
fn activate(bundle: Option<&str>, setting: Option<&str>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let args: Vec<String> = match bundle {
        Some(id) => vec!["-b".into(), id.into()],
        None => match terminal::resolve(setting, terminal::os(), terminal::exists)
            .ok()
            .as_ref()
            .and_then(terminal::app_name)
        {
            Some(name) => vec!["-a".into(), name.into()],
            None => return,
        },
    };
    let _ = Command::new("open")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn start_terminal(app: &App, link: &Link) -> Result<()> {
    if std::env::var_os(STARTED_ENV).is_some() {
        bail!("the terminal jeet started is not one; not starting another");
    }
    let setting = app.config.notify.terminal.as_deref();
    let choice = terminal::resolve(setting, terminal::os(), terminal::exists)?;
    if choice == Choice::Off {
        bail!("no jeet screen is open and `[notify] terminal` is none");
    }
    let exe = std::env::current_exe().context("find the jeet binary")?;
    let path = std::env::var("PATH").unwrap_or_default();
    let jeet = terminal::open_argv(&exe, &app.home, &path, &link.to_string());
    let script = registry::dir(&app.home).join(format!("open-{}.command", std::process::id()));
    terminal::start(&terminal::invocation(&choice, &jeet, &script)?)?;
    println!("started a terminal for {link}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(pid: u32, started: i64, pane: Option<&str>, tty: Option<&str>) -> Screen {
        Screen {
            pid,
            tty: tty.map(str::to_string),
            tmux_pane: pane.map(str::to_string),
            app: None,
            started,
        }
    }

    fn client(name: &str, session: &str, activity: i64) -> Client {
        Client {
            name: name.into(),
            tty: name.into(),
            session: session.into(),
            activity,
        }
    }

    fn task() -> Link {
        Link::task("fix")
    }

    #[test]
    fn the_newest_screen_gets_the_link() {
        let screens = [
            screen(1, 10, None, None),
            screen(2, 30, None, None),
            screen(3, 20, None, None),
        ];
        let Plan::Screen { screen, bring } = plan(&task(), &screens, &[], |_| false, true) else {
            panic!("expected a screen");
        };
        assert_eq!(screen.pid, 2);
        assert_eq!(bring, Bring::Nothing);
    }

    #[test]
    fn a_screen_in_tmux_is_switched_to_on_the_busiest_client() {
        let screens = [screen(1, 10, Some("%4"), Some("/dev/ttys001"))];
        let clients = [
            client("/dev/c1", "work", 100),
            client("/dev/c2", "work", 200),
        ];
        let Plan::Screen { bring, .. } = plan(&task(), &screens, &clients, |_| false, false) else {
            panic!("expected a screen");
        };
        assert_eq!(
            bring,
            Bring::Pane {
                client: "/dev/c2".into(),
                pane: "%4".into()
            }
        );
    }

    #[test]
    fn a_screen_under_tmux_attach_is_detached_back() {
        let screens = [screen(1, 10, None, Some("/dev/ttys007"))];
        let clients = [
            client("/dev/ttys003", "jeet-a", 500),
            client("/dev/ttys007", "jeet-b", 100),
        ];
        let Plan::Screen { bring, .. } = plan(&task(), &screens, &clients, |_| false, false) else {
            panic!("expected a screen");
        };
        assert_eq!(
            bring,
            Bring::Detach {
                client: "/dev/ttys007".into()
            }
        );
        // No client on its terminal: nothing to detach.
        let other = [client("/dev/ttys003", "jeet-a", 500)];
        let Plan::Screen { bring, .. } = plan(&task(), &screens, &other, |_| false, false) else {
            panic!("expected a screen");
        };
        assert_eq!(bring, Bring::Nothing);
    }

    #[test]
    fn with_no_screen_a_live_session_is_switched_to() {
        let clients = [
            client("/dev/c1", "jeet-other", 100),
            client("/dev/c2", "work", 900),
            client("/dev/c3", "jeet-third", 300),
        ];
        let exists = |name: &str| name == "jeet-fix";
        assert_eq!(
            plan(&task(), &[], &clients, exists, true),
            Plan::Session {
                client: "/dev/c3".into(),
                session: "jeet-fix".into()
            }
        );
        assert_eq!(
            plan(&Link::review("fix"), &[], &clients, exists, true),
            Plan::Session {
                client: "/dev/c3".into(),
                session: "jeet-fix".into()
            }
        );
    }

    #[test]
    fn otherwise_it_runs_here_or_in_a_terminal() {
        let clients = [client("/dev/c1", "jeet-a", 100)];
        // A report, or a session that is gone, falls through.
        assert_eq!(
            plan(&Link::Report(3), &[], &clients, |_| true, true),
            Plan::Here
        );
        assert_eq!(
            plan(&task(), &[], &clients, |_| false, false),
            Plan::Terminal
        );
        // No client on a jeet session, whatever exists.
        let elsewhere = [client("/dev/c1", "work", 100)];
        assert_eq!(plan(&task(), &[], &elsewhere, |_| true, true), Plan::Here);
        assert_eq!(plan(&task(), &[], &[], |_| true, false), Plan::Terminal);
    }
}
