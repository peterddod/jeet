//! `jeet notify`: a notification, for the assistant to report what it has
//! done, or for any script; and the sender that sends them.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::cli::{NotifyCommands, NotifySetup};
use crate::config::{self, NotifyLevel};
use crate::context::App;
use crate::link::{self, Link};
use crate::notify::secrets::{self, Keyring};
use crate::notify::{self, ntfy, sender, slack, Event, Why};
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
        NotifyCommands::Serve => {
            let app = App::open()?;
            sender::serve(&home, !permissions::at_terminal(&app)?)
        }
        NotifyCommands::Install => sender::install(&home),
        NotifyCommands::Uninstall => sender::uninstall(),
        NotifyCommands::InstallLink => install_link(&App::open()?),
        NotifyCommands::UninstallLink => uninstall_link(&App::open()?),
        NotifyCommands::Setup {
            backend: NotifySetup::Slack,
        } => setup_slack(&App::open()?),
        NotifyCommands::Setup {
            backend: NotifySetup::Ntfy,
        } => setup_ntfy(&App::open()?),
        NotifyCommands::Test => test(&App::open()?),
        NotifyCommands::SlackManifest => {
            println!("{}", slack::manifest());
            Ok(())
        }
    }
}

/// `jeet notify setup slack`: the bot token into the OS keyring, and
/// `[notify.slack] user`. The user's alone, at their own terminal: no
/// agent ever handles the token.
fn setup_slack(app: &App) -> Result<()> {
    if !permissions::at_terminal(app)? {
        bail!("this asks for a Slack token: run it from your own terminal, not a jeet pane");
    }
    if !std::io::stdin().is_terminal() {
        bail!("this asks for a Slack token: run it at a terminal");
    }
    eprintln!(
        "Paste the app's Bot User OAuth Token (xoxb-…; make the app from \
         `jeet notify slack-manifest` and install it to your workspace first)."
    );
    let token = read_hidden("Bot token: ")?;
    let token = token.trim();
    if !slack::bot_token(token) {
        bail!("that is not a bot token (xoxb-…); nothing was stored");
    }
    secrets::OsKeyring
        .set(&secrets::account(slack::NAME, &app.home), token)
        .context("store the token in the OS keyring")?;
    eprintln!("jeet: the Slack bot token is in the OS keyring");

    let path = config::config_path(&app.home);
    let notify = &app.config.notify;
    match notify.slack_user() {
        Some(user) => eprintln!("jeet: [notify.slack] user is {user}"),
        None => {
            eprint!("Your Slack member id (in Slack: your profile, ⋮, Copy member ID): ");
            std::io::stderr().flush()?;
            let mut user = String::new();
            std::io::stdin().lock().read_line(&mut user)?;
            let user = user.trim();
            if !slack::member_id(user) {
                bail!(
                    "{user:?} is not a member id (U…); set `[notify.slack] user` in {} yourself",
                    path.display()
                );
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            match with_slack_user(&text, user)? {
                Some(text) => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!(
                        "jeet: set [notify.slack] user = \"{user}\" in {}",
                        path.display()
                    );
                }
                None => eprintln!(
                    "jeet: set `user = \"{user}\"` under [notify.slack] in {}",
                    path.display()
                ),
            }
        }
    }
    if !notify.via().iter().any(|v| v == slack::NAME) {
        eprintln!(
            "jeet: add \"slack\" to [notify] via in {}, e.g. via = [\"desktop\", \"slack\"]",
            path.display()
        );
    }
    if !notify::sender_running(&app.home) {
        eprintln!("jeet: only the sender sends to Slack: `jeet notify install` keeps one running");
    }
    eprintln!("jeet: then `jeet notify test`");
    Ok(())
}

/// `text`, a config.toml, with `[notify.slack] user` added at its end;
/// `None` if it has a `[notify.slack]` already, which is the user's to
/// change, or would not read back.
fn with_slack_user(text: &str, user: &str) -> Result<Option<String>> {
    let config: config::Config = toml::from_str(text).context("parse config")?;
    if config.notify.slack.is_some() {
        return Ok(None);
    }
    let mut text = text.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("\n[notify.slack]\nuser = \"{user}\"\n"));
    let read: config::Config = match toml::from_str(&text) {
        Ok(read) => read,
        Err(_) => return Ok(None),
    };
    Ok((read.notify.slack_user() == Some(user)).then_some(text))
}

/// `jeet notify setup ntfy`: `[notify.ntfy]`'s server and topic, and an
/// access token, if the user gives one, into the OS keyring. The user's
/// alone, at their own terminal, as for Slack.
fn setup_ntfy(app: &App) -> Result<()> {
    if !permissions::at_terminal(app)? {
        bail!("this asks for an ntfy token: run it from your own terminal, not a jeet pane");
    }
    if !std::io::stdin().is_terminal() {
        bail!("this asks for an ntfy token: run it at a terminal");
    }
    let path = config::config_path(&app.home);
    let notify = &app.config.notify;
    match notify.ntfy_topic() {
        Some(topic) => eprintln!(
            "jeet: [notify.ntfy] publishes to {topic} on {}",
            notify.ntfy_server()
        ),
        None => {
            let server = read_line(&format!(
                "ntfy server (Enter for {}): ",
                config::DEFAULT_NTFY_SERVER
            ))?;
            let server = server.trim().trim_end_matches('/');
            let server = match server {
                "" => config::DEFAULT_NTFY_SERVER,
                server if notify::openable_url(server) => server,
                server => bail!("{server:?} is not an http(s) address; nothing was changed"),
            };
            eprintln!(
                "On a public server such as ntfy.sh, anyone who knows a topic can read what \
                 is published to it: keep the topic unguessable, or protect it with an \
                 access token."
            );
            let suggested = ntfy::random_topic();
            let topic = read_line(&format!("Topic (Enter for {suggested}): "))?;
            let topic = match topic.trim() {
                "" => suggested.as_str(),
                topic if ntfy::valid_topic(topic) => topic,
                topic => bail!(
                    "{topic:?} is not an ntfy topic (1 to 64 letters, digits, `-` or `_`); nothing was changed"
                ),
            };
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            match with_ntfy(&text, server, topic)? {
                Some(text) => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!("jeet: set [notify.ntfy] in {}", path.display());
                }
                None => eprintln!(
                    "jeet: set `server = \"{server}\"` and `topic = \"{topic}\"` under \
                     [notify.ntfy] in {}",
                    path.display()
                ),
            }
            eprintln!("jeet: subscribe to {server}/{topic} in the ntfy app");
        }
    }
    eprintln!(
        "An access token (tk_…) for a protected topic or your own server, or Enter for none \
         (one stored before stays)."
    );
    let token = read_hidden("Access token: ")?;
    match token.trim() {
        "" => eprintln!("jeet: no new token stored"),
        token if ntfy::plausible_token(token) => {
            secrets::OsKeyring
                .set(&secrets::account(ntfy::NAME, &app.home), token)
                .context("store the token in the OS keyring")?;
            eprintln!("jeet: the ntfy access token is in the OS keyring");
        }
        _ => bail!("that is not an access token; nothing was stored"),
    }
    if !notify.via().iter().any(|v| v == ntfy::NAME) {
        eprintln!(
            "jeet: add \"ntfy\" to [notify] via in {}, e.g. via = [\"desktop\", \"ntfy\"]",
            path.display()
        );
    }
    if !notify::sender_running(&app.home) {
        eprintln!("jeet: only the sender sends to ntfy: `jeet notify install` keeps one running");
    }
    eprintln!("jeet: then `jeet notify test`");
    Ok(())
}

/// `text`, a config.toml, with `[notify.ntfy]` added at its end (`server`
/// only if not ntfy.sh); `None` if it has a `[notify.ntfy]` already, which
/// is the user's to change, or would not read back.
fn with_ntfy(text: &str, server: &str, topic: &str) -> Result<Option<String>> {
    let config: config::Config = toml::from_str(text).context("parse config")?;
    if config.notify.ntfy.is_some() {
        return Ok(None);
    }
    let mut text = text.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n[notify.ntfy]\n");
    if server != config::DEFAULT_NTFY_SERVER {
        text.push_str(&format!("server = {}\n", toml::Value::from(server)));
    }
    text.push_str(&format!("topic = {}\n", toml::Value::from(topic)));
    let read: config::Config = match toml::from_str(&text) {
        Ok(read) => read,
        Err(_) => return Ok(None),
    };
    let notify = &read.notify;
    Ok((notify.ntfy_topic() == Some(topic) && notify.ntfy_server() == server).then_some(text))
}

/// A line read from the terminal.
fn read_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

/// A line read from the terminal without echoing it.
fn read_hidden(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let stty = |arg: &str| Command::new("stty").arg(arg).status();
    let hidden = stty("-echo").is_ok_and(|s| s.success());
    if !hidden {
        bail!("could not turn the terminal's echo off; nothing was read");
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    let _ = stty("echo");
    eprintln!();
    read.context("read the token")?;
    Ok(line)
}

/// `jeet notify test`: queue a notification, and say whether the sender
/// took it.
fn test(app: &App) -> Result<()> {
    let link = app
        .db
        .get_assistant_task()?
        .map(|task| link::link_for(&Link::Task(task.slug)));
    notify::event_in(
        &app.home,
        Event {
            why: Why::Info,
            kind: "test".into(),
            title: "jeet: test notification".into(),
            body: "Notifications reach you here. Try its links.".into(),
            link,
            ..Event::default()
        },
    );
    let via = app.config.notify.via();
    eprintln!("jeet: queued a test notification for {}", via.join(", "));
    if !notify::sender_running(&app.home) {
        if let Some(notice) = notify::notice(&app.home, &app.config.notify) {
            eprintln!("jeet: {notice}");
        }
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && waits(&app.home) {
        std::thread::sleep(Duration::from_millis(250));
    }
    match waits(&app.home) {
        true => eprintln!(
            "jeet: the sender has not sent it yet; it tries again later, and {} says why",
            sender::log_hint(&app.home)
        ),
        false => eprintln!(
            "jeet: the sender took it from the outbox (unless a [[notify.rules]] entry or \
             max_per_hour held it back); {} has any error",
            sender::log_hint(&app.home)
        ),
    }
    Ok(())
}

/// Whether a test notification waits in the outbox.
fn waits(home: &Path) -> bool {
    std::fs::read_to_string(notify::outbox_path(home)).is_ok_and(|text| {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Event>(l).ok())
            .any(|e| e.kind == "test")
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_adds_the_slack_user_only_where_none_is() {
        let text = "scan_roots = []\n[notify]\nvia = [\"slack\"]";
        let added = with_slack_user(text, "U012AB3CD").unwrap().unwrap();
        assert!(added.starts_with(text), "{added}");
        let config: config::Config = toml::from_str(&added).unwrap();
        assert_eq!(config.notify.slack_user(), Some("U012AB3CD"));
        assert_eq!(config.notify.via(), ["slack"]);
        assert!(with_slack_user("", "U012AB3CD").unwrap().is_some());

        let set = "[notify.slack]\n";
        assert_eq!(with_slack_user(set, "U012AB3CD").unwrap(), None);
        let inline = "[notify]\nslack = { user = \"U9\" }\n";
        assert_eq!(with_slack_user(inline, "U012AB3CD").unwrap(), None);
        assert!(with_slack_user("[notify\n", "U012AB3CD").is_err());
    }

    #[test]
    fn setup_adds_ntfy_only_where_none_is() {
        let text = "scan_roots = []\n[notify]\nvia = [\"ntfy\"]";
        let added = with_ntfy(text, "https://ntfy.sh", "jeet-abc")
            .unwrap()
            .unwrap();
        assert!(added.starts_with(text), "{added}");
        assert!(!added.contains("server"), "{added}");
        let config: config::Config = toml::from_str(&added).unwrap();
        assert_eq!(config.notify.ntfy_topic(), Some("jeet-abc"));
        assert!(notify::check(&config.notify).is_ok());

        let own = with_ntfy("", "https://ntfy.example.com/x", "t")
            .unwrap()
            .unwrap();
        let config: config::Config = toml::from_str(&own).unwrap();
        assert_eq!(config.notify.ntfy_server(), "https://ntfy.example.com/x");
        // Quoted as TOML, whatever it holds.
        let odd = with_ntfy("", "https://a\"b", "t").unwrap().unwrap();
        let config: config::Config = toml::from_str(&odd).unwrap();
        assert_eq!(config.notify.ntfy_server(), "https://a\"b");

        assert_eq!(
            with_ntfy("[notify.ntfy]\n", "https://ntfy.sh", "t").unwrap(),
            None
        );
        assert!(with_ntfy("[notify\n", "https://ntfy.sh", "t").is_err());
    }
}
