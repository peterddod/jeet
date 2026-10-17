//! The `command` backend: a program of the user's, run directly (split like
//! `agent`) with the notification in `JEET_NOTIFY_TITLE`,
//! `JEET_NOTIFY_SUBTITLE`, `JEET_NOTIFY_BODY` and `JEET_NOTIFY_URL`.
//!
//! `[notify] command` names it for `via = ["command"]`; `$JEET_NOTIFY_CMD`,
//! in the process that sends, runs in place of the `desktop` backend, for
//! tests and for anyone who wants notifications somewhere else.

use anyhow::{bail, Result};

use crate::config::split_command;

use super::{
    one_line, openable_url, run, Event, Invocation, Notifier, Tokens, BODY_MAX, TITLE_MAX,
};

pub struct Command {
    /// Its name in `via`: `command`, or `desktop` when it stands in for it.
    pub name: &'static str,
    pub command: String,
}

impl Notifier for Command {
    fn name(&self) -> &str {
        self.name
    }

    fn send(&self, event: &Event, _: &dyn Tokens) -> Result<()> {
        let Some(run_it) = invocation(event, &self.command) else {
            bail!("the notify command is empty");
        };
        run(&run_it);
        Ok(())
    }
}

fn invocation(event: &Event, command: &str) -> Option<Invocation> {
    let argv = split_command(command);
    if argv.is_empty() {
        return None;
    }
    let url = event.link.as_deref().filter(|u| openable_url(u));
    let env = [
        ("JEET_NOTIFY_TITLE", one_line(&event.title, TITLE_MAX)),
        (
            "JEET_NOTIFY_SUBTITLE",
            event
                .subtitle
                .as_deref()
                .map(|s| one_line(s, TITLE_MAX))
                .unwrap_or_default(),
        ),
        ("JEET_NOTIFY_BODY", one_line(&event.body, BODY_MAX)),
        ("JEET_NOTIFY_URL", url.unwrap_or_default().to_string()),
    ];
    Some(Invocation {
        argv,
        env: env.map(|(k, v)| (k.to_string(), v)).to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_gets_the_notification_in_its_environment() {
        let n = Event {
            title: "t".into(),
            subtitle: Some("sub".into()),
            body: "b\nmore".into(),
            link: Some("file:///etc/passwd".into()),
            ..Event::default()
        };
        let run = invocation(&n, "/bin/stub --flag").unwrap();
        assert_eq!(run.argv, ["/bin/stub", "--flag"]);
        let get = |k: &str| {
            run.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("JEET_NOTIFY_TITLE"), Some("t"));
        assert_eq!(get("JEET_NOTIFY_SUBTITLE"), Some("sub"));
        assert_eq!(get("JEET_NOTIFY_BODY"), Some("b more"));
        // Not a web page: never opened.
        assert_eq!(get("JEET_NOTIFY_URL"), Some(""));
        assert!(invocation(&n, "  ").is_none());
    }
}
