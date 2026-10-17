//! `jeet notify`: a notification, for the assistant to report what it has
//! done, or for any script; and the sender that sends them.

use anyhow::{bail, Result};

use crate::cli::NotifyCommands;
use crate::config::{self, NotifyLevel};
use crate::context::App;
use crate::link;
use crate::notify::{self, sender, Event};
use crate::permissions;

pub fn run(title: Option<&str>, body: Option<&str>, url: Option<&str>) -> Result<()> {
    let Some(title) = title else {
        bail!("give the notification's title: `jeet notify <title>`");
    };
    if let Some(url) = url {
        if !notify::openable_url(url) {
            bail!("--url must be an http(s) address, not {url:?}");
        }
    }
    let home = config::jeet_home()?;
    let assistant = permissions::assistant_mode();
    if assistant && !config::load_or_create(&home)?.assistant.notify() {
        eprintln!("jeet: notifications from the assistant are off (`[assistant] notify`)");
        return Ok(());
    }
    notify::event_in(
        &home,
        Event {
            kind: "message".into(),
            level: assistant.then_some(NotifyLevel::Assistant),
            title: title.into(),
            body: body.unwrap_or_default().into(),
            link: url.map(str::to_string),
            ..Event::default()
        },
    );
    Ok(())
}

pub fn run_command(command: NotifyCommands) -> Result<()> {
    let home = config::jeet_home()?;
    match command {
        NotifyCommands::Serve => sender::serve(&home),
        NotifyCommands::Install => sender::install(&home),
        NotifyCommands::Uninstall => sender::uninstall(),
        NotifyCommands::InstallLink => install_link(&App::open()?),
        NotifyCommands::UninstallLink => uninstall_link(&App::open()?),
    }
}

/// `jeet notify install-link`: make the system open `jeet://` links with
/// `jeet open`. A change to the user's system, so theirs alone to make.
pub fn install_link(app: &App) -> Result<()> {
    require_terminal(app)?;
    link::handler::install(&app.home)
}

/// `jeet notify uninstall-link`.
pub fn uninstall_link(app: &App) -> Result<()> {
    require_terminal(app)?;
    link::handler::uninstall()
}

fn require_terminal(app: &App) -> Result<()> {
    if !permissions::at_terminal(app)? {
        bail!("this changes your system: run it from your own terminal, not a jeet pane");
    }
    Ok(())
}
