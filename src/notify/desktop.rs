//! The `desktop` backend: Notification Center on macOS (`terminal-notifier`,
//! else `osascript`), `notify-send` elsewhere.
//!
//! What a notification says is often someone else's words — a PR title, a
//! message the assistant was sent — so it only ever reaches the notifier as
//! separate arguments: never through a shell, and never quoted into an
//! AppleScript, where it would run as code.

use std::path::Path;

use anyhow::{bail, Result};

use crate::link;

use super::{
    one_line, openable_url, run, Event, Invocation, Notifier, Tokens, BODY_MAX, TITLE_MAX,
};

pub struct Desktop;

impl Notifier for Desktop {
    fn name(&self) -> &str {
        "desktop"
    }

    fn send(&self, event: &Event, _: &dyn Tokens) -> Result<()> {
        // A jeet:// link opens nothing until its handler is installed.
        let unhandled = event
            .link
            .as_deref()
            .is_some_and(|u| link::parse(u).is_ok())
            && !link::handler::installed();
        let event = &match unhandled {
            true => Event {
                link: None,
                ..event.clone()
            },
            false => event.clone(),
        };
        let Some(run_it) = invocation(event, os(), on_path) else {
            bail!("no notifier found (terminal-notifier, osascript or notify-send)");
        };
        run(&run_it);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    MacOs,
    Other,
}

fn os() -> Os {
    match cfg!(target_os = "macos") {
        true => Os::MacOs,
        false => Os::Other,
    }
}

/// How to raise `event` on `os`, given which programs `found` says are
/// installed; `None` when there is no way to.
fn invocation(event: &Event, os: Os, found: impl Fn(&str) -> bool) -> Option<Invocation> {
    let title = one_line(&event.title, TITLE_MAX);
    let subtitle = event.subtitle.as_deref().map(|s| one_line(s, TITLE_MAX));
    let body = one_line(&event.body, BODY_MAX);
    // A web page, or a jeet:// link `jeet open` takes, parsed strictly.
    let url = event
        .link
        .as_deref()
        .filter(|u| openable_url(u) || link::parse(u).is_ok());

    let argv = match os {
        Os::MacOs if found("terminal-notifier") => {
            let mut argv = vec![
                "terminal-notifier".to_string(),
                "-title".into(),
                defuse(&title),
                "-message".into(),
                defuse(&body),
                "-sound".into(),
                "Glass".into(),
            ];
            if let Some(subtitle) = &subtitle {
                argv.extend(["-subtitle".into(), defuse(subtitle)]);
            }
            if let Some(url) = url {
                argv.extend(["-open".into(), url.to_string()]);
            }
            argv
        }
        // The script reads the text from its arguments rather than having it
        // pasted in. `--` ends osascript's options: without it, a title
        // starting with `-e` would be one more line of script, and a script
        // can run any shell command.
        Os::MacOs => vec![
            "osascript".to_string(),
            "-e".into(),
            "on run argv".into(),
            "-e".into(),
            "display notification (item 3 of argv) with title (item 1 of argv) \
             subtitle (item 2 of argv) sound name \"Glass\""
                .into(),
            "-e".into(),
            "end run".into(),
            "--".into(),
            title,
            subtitle.unwrap_or_default(),
            body,
        ],
        Os::Other if found("notify-send") => {
            let mut text = body;
            if let Some(subtitle) = subtitle.filter(|s| !s.is_empty()) {
                text = format!("{subtitle}\n{text}");
            }
            if let Some(url) = url {
                text = format!("{text}\n{url}");
            }
            // `--`: a title that starts with a dash is a title, not a flag.
            vec!["notify-send".to_string(), "--".into(), title, text]
        }
        Os::Other => return None,
    };
    Some(Invocation {
        argv,
        env: Vec::new(),
    })
}

/// `terminal-notifier` reads its arguments as user defaults, which take a
/// value that looks like a property list — `(a, b)`, `{a = b;}`, `"x"` — as
/// one, and one starting with a dash as the next option. A zero-width space
/// in front keeps it plain text.
fn defuse(value: &str) -> String {
    match value.chars().next() {
        Some('-' | '(' | '{' | '<' | '"' | '\'' | '[' | '\\') => format!("\u{200B}{value}"),
        _ => value.to_string(),
    }
}

/// Whether `program` is an executable file somewhere on `$PATH`.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, body: &str, url: Option<&str>) -> Event {
        Event {
            title: title.into(),
            subtitle: Some("sub".into()),
            body: body.into(),
            link: url.map(str::to_string),
            ..Event::default()
        }
    }

    #[test]
    fn terminal_notifier_is_preferred_and_opens_the_url() {
        let n = note(
            "Assistant",
            "created task fix-login",
            Some("https://x.test/1"),
        );
        let run = invocation(&n, Os::MacOs, |_| true).unwrap();
        assert_eq!(
            run.argv,
            [
                "terminal-notifier",
                "-title",
                "Assistant",
                "-message",
                "created task fix-login",
                "-sound",
                "Glass",
                "-subtitle",
                "sub",
                "-open",
                "https://x.test/1"
            ]
        );
        assert!(run.env.is_empty());
    }

    #[test]
    fn osascript_takes_the_text_as_arguments_not_script() {
        let hostile = r#"" & (do shell script "touch /tmp/pwned") & ""#;
        let n = note(hostile, hostile, None);
        let run = invocation(&n, Os::MacOs, |_| false).unwrap();
        assert_eq!(run.argv[0], "osascript");
        // The script is fixed; the text only ever follows it.
        let script: Vec<&String> = run.argv[1..7].iter().collect();
        assert!(script.iter().all(|a| !a.contains("pwned")), "{script:?}");
        assert_eq!(run.argv[7..], ["--", hostile, "sub", hostile]);
    }

    /// A title that would be another `-e` line of script, were it read as
    /// an option, once `one_line` has trimmed the space that got it past
    /// jeet's own flags.
    const OPTION_TITLE: &str = r#" -eproperty p : (do shell script "echo INJECTED > injected")"#;

    #[test]
    fn osascript_never_reads_the_text_as_options() {
        let n = note(OPTION_TITLE, "-e body", None);
        let run = invocation(&n, Os::MacOs, |_| false).unwrap();
        let end = run.argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(end, 7, "{:?}", run.argv);
        assert_eq!(run.argv[8], OPTION_TITLE.trim_start());
        assert_eq!(run.argv[10], "-e body");
    }

    /// The same arguments, run, with the notification swapped for a script
    /// that hands back what it was given: the title comes back as text, and
    /// nothing in it has run.
    #[cfg(target_os = "macos")]
    #[test]
    fn osascript_hands_the_title_over_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let n = note(OPTION_TITLE, "body", None);
        let mut argv = invocation(&n, Os::MacOs, |_| false).unwrap().argv;
        argv[4] = "return (item 1 of argv) & \"|\" & (count of argv)".into();
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("{}|3", OPTION_TITLE.trim_start())
        );
        assert!(!dir.path().join("injected").exists());
    }

    #[test]
    fn notify_send_elsewhere_and_nothing_without_it() {
        let n = note("-t 0", "body", Some("https://x.test/1"));
        let run = invocation(&n, Os::Other, |p| p == "notify-send").unwrap();
        assert_eq!(
            run.argv,
            ["notify-send", "--", "-t 0", "sub\nbody\nhttps://x.test/1"]
        );
        assert!(invocation(&n, Os::Other, |_| false).is_none());
    }

    #[test]
    fn values_terminal_notifier_would_misread_are_defused() {
        assert_eq!(defuse("plain"), "plain");
        assert_eq!(defuse("(a, b)"), "\u{200B}(a, b)");
        assert_eq!(defuse("-open"), "\u{200B}-open");
        assert_eq!(defuse(""), "");
    }

    #[test]
    fn a_jeet_link_is_opened_on_click_and_nothing_like_one_is() {
        let opens = |url: &str| {
            let run = invocation(&note("t", "b", Some(url)), Os::MacOs, |_| true).unwrap();
            run.argv.iter().any(|a| a == "-open")
        };
        assert!(opens("jeet://task/fix-login"));
        assert!(opens("jeet://report/41"));
        for bad in [
            "jeet://task/a b",
            "jeet://task/x?y",
            "jeet://nope/x",
            "myapp://task/x",
        ] {
            assert!(!opens(bad), "{bad}");
        }
    }
}
