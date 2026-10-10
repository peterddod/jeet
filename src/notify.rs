//! Desktop notifications: Notification Center on macOS, `notify-send`
//! elsewhere.
//!
//! What a notification says is often someone else's words — a PR title, a
//! message the assistant was sent — so it only ever reaches the notifier as
//! separate arguments: never through a shell, and never quoted into an
//! AppleScript, where it would run as code.
//!
//! `$JEET_NOTIFY_CMD` replaces the notifier, for tests and for anyone who
//! wants notifications somewhere else. It is run directly (split like
//! `agent`), with the notification in `JEET_NOTIFY_TITLE`,
//! `JEET_NOTIFY_SUBTITLE`, `JEET_NOTIFY_BODY` and `JEET_NOTIFY_URL`.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::split_command;

pub const COMMAND_ENV: &str = "JEET_NOTIFY_CMD";

/// Longest title or subtitle shown; Notification Center cuts them shorter.
const TITLE_MAX: usize = 120;

/// Longest body passed on.
const BODY_MAX: usize = 500;

/// One notification. `url` is opened when it is clicked, where the notifier
/// can do that.
#[derive(Debug, Clone, Default)]
pub struct Notification<'a> {
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    pub body: &'a str,
    pub url: Option<&'a str>,
}

/// Raise `n`, best effort: a notification that cannot be shown is not worth
/// failing anything over.
pub fn notify(n: &Notification) {
    let Some(run) = invocation(n, os(), override_command().as_deref(), on_path) else {
        return;
    };
    let Some((program, args)) = run.argv.split_first() else {
        return;
    };
    let _ = Command::new(program)
        .args(args)
        .envs(run.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Whether `url` may be opened from a notification: a web page, nothing
/// else — not a file, not an app's own scheme.
pub fn openable_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
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

fn override_command() -> Option<String> {
    std::env::var(COMMAND_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// A program to run: its argv, and what to add to its environment.
#[derive(Debug, PartialEq, Eq)]
struct Invocation {
    argv: Vec<String>,
    env: Vec<(String, String)>,
}

/// How to raise `n` on `os`, given the override command and which programs
/// `found` says are installed; `None` when there is no way to.
fn invocation(
    n: &Notification,
    os: Os,
    command: Option<&str>,
    found: impl Fn(&str) -> bool,
) -> Option<Invocation> {
    let title = one_line(n.title, TITLE_MAX);
    let subtitle = n.subtitle.map(|s| one_line(s, TITLE_MAX));
    let body = one_line(n.body, BODY_MAX);
    let url = n.url.filter(|u| openable_url(u));

    if let Some(command) = command {
        let argv = split_command(command);
        if argv.is_empty() {
            return None;
        }
        let env = [
            ("JEET_NOTIFY_TITLE", title),
            ("JEET_NOTIFY_SUBTITLE", subtitle.unwrap_or_default()),
            ("JEET_NOTIFY_BODY", body),
            ("JEET_NOTIFY_URL", url.unwrap_or_default().to_string()),
        ];
        return Some(Invocation {
            argv,
            env: env.map(|(k, v)| (k.to_string(), v)).to_vec(),
        });
    }

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

/// `text` on one line: line breaks and tabs become spaces, other control
/// characters are dropped, and it is cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let cut: String = flat.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
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

    fn note<'a>(title: &'a str, body: &'a str, url: Option<&'a str>) -> Notification<'a> {
        Notification {
            title,
            subtitle: Some("sub"),
            body,
            url,
        }
    }

    #[test]
    fn terminal_notifier_is_preferred_and_opens_the_url() {
        let n = note(
            "Assistant",
            "created task fix-login",
            Some("https://x.test/1"),
        );
        let run = invocation(&n, Os::MacOs, None, |_| true).unwrap();
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
        let run = invocation(&n, Os::MacOs, None, |_| false).unwrap();
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
        let run = invocation(&n, Os::MacOs, None, |_| false).unwrap();
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
        let mut argv = invocation(&n, Os::MacOs, None, |_| false).unwrap().argv;
        argv[4] = "return (item 1 of argv) & \"|\" & (count of argv)".into();
        let output = Command::new(&argv[0])
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
        let run = invocation(&n, Os::Other, None, |p| p == "notify-send").unwrap();
        assert_eq!(
            run.argv,
            ["notify-send", "--", "-t 0", "sub\nbody\nhttps://x.test/1"]
        );
        assert!(invocation(&n, Os::Other, None, |_| false).is_none());
    }

    #[test]
    fn the_override_gets_the_notification_in_its_environment() {
        let n = note("t", "b\nmore", Some("file:///etc/passwd"));
        let run = invocation(&n, Os::MacOs, Some("/bin/stub --flag"), |_| true).unwrap();
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
    }

    #[test]
    fn values_terminal_notifier_would_misread_are_defused() {
        assert_eq!(defuse("plain"), "plain");
        assert_eq!(defuse("(a, b)"), "\u{200B}(a, b)");
        assert_eq!(defuse("-open"), "\u{200B}-open");
        assert_eq!(defuse(""), "");
    }

    #[test]
    fn text_is_flattened_and_cut() {
        assert_eq!(one_line(" a\nb\tc\u{1b}[2J ", 20), "a b c[2J");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn only_web_pages_are_openable() {
        assert!(openable_url("https://github.com/acme/widget/pull/7"));
        assert!(openable_url("http://localhost:3000"));
        for bad in [
            "file:///etc/passwd",
            "javascript:x",
            "https://a b",
            "ssh://x",
        ] {
            assert!(!openable_url(bad), "{bad}");
        }
    }
}
