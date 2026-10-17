//! `jeet notify`: a desktop notification, for the assistant to report what
//! it has done, or for any script.

use anyhow::{bail, Result};

use crate::config;
use crate::context::App;
use crate::link;
use crate::notify::{self, Notification};
use crate::permissions;

pub fn run(title: &str, body: Option<&str>, url: Option<&str>) -> Result<()> {
    if let Some(url) = url {
        if !notify::openable_url(url) {
            bail!("--url must be an http(s) address, not {url:?}");
        }
    }
    if permissions::assistant_mode() {
        let home = config::jeet_home()?;
        if !config::load_or_create(&home)?.assistant.notify() {
            eprintln!("jeet: notifications from the assistant are off (`[assistant] notify`)");
            return Ok(());
        }
    }
    notify::notify(&Notification {
        title,
        subtitle: None,
        body: body.unwrap_or_default(),
        url,
    });
    Ok(())
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
