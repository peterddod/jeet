//! `jeet notify`: a desktop notification, for the assistant to report what
//! it has done, or for any script.

use anyhow::{bail, Result};

use crate::config;
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
