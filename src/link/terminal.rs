//! Starting a terminal that runs `jeet open <url>`, for when a link is
//! opened and no jeet screen is.
//!
//! `[notify] terminal` picks which: a name jeet has a recipe for, a command
//! that jeet's own command line is appended to, or `none`. Unset, it is the
//! platform's own terminal. What is run is worked out by pure functions of
//! the setting, the platform and which programs exist; [`start`] spawns it.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::split_command;
use crate::harness::shell_quote;

/// Set in the environment of the terminal jeet starts: one that then still
/// finds no terminal to run in must not start another, forever.
pub const STARTED_ENV: &str = "JEET_LINK_TERMINAL";

/// How long a terminal's launcher is waited for before it is left running:
/// `open` returns at once, a terminal started directly stays up.
const LAUNCH_WAIT: Duration = Duration::from_secs(3);

/// Linux terminals, in the order tried when none is configured.
const LINUX_TERMINALS: &[&str] = &[
    "ghostty",
    "x-terminal-emulator",
    "gnome-terminal",
    "konsole",
    "kitty",
    "alacritty",
    "wezterm",
    "xterm",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Other,
}

pub fn os() -> Os {
    match cfg!(target_os = "macos") {
        true => Os::MacOs,
        false => Os::Other,
    }
}

/// Which terminal `[notify] terminal` comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// `none`: never start one.
    Off,
    /// Ghostty on macOS.
    Ghostty,
    /// Terminal.app.
    TerminalApp,
    /// A Linux terminal jeet has a recipe for.
    Named(&'static str),
    /// Any other command, with jeet's appended.
    Custom(String),
}

/// The terminal `setting` names, given which programs `found` says exist.
/// A path starting `/` or `~/` is looked for as that file, anything else on
/// `$PATH`.
pub fn resolve(setting: Option<&str>, os: Os, found: impl Fn(&str) -> bool) -> Result<Choice> {
    if let Some(setting) = setting.map(str::trim).filter(|s| !s.is_empty()) {
        let name = setting.to_ascii_lowercase();
        return Ok(match (os, name.as_str()) {
            (_, "none") => Choice::Off,
            (Os::MacOs, "ghostty") => Choice::Ghostty,
            (Os::MacOs, "terminal") => Choice::TerminalApp,
            (Os::Other, name) => match LINUX_TERMINALS.iter().find(|t| **t == name) {
                Some(known) => Choice::Named(known),
                None => Choice::Custom(setting.to_string()),
            },
            _ => Choice::Custom(setting.to_string()),
        });
    }
    match os {
        Os::MacOs => {
            let ghostty = found("/Applications/Ghostty.app") || found("~/Applications/Ghostty.app");
            Ok(if ghostty {
                Choice::Ghostty
            } else {
                Choice::TerminalApp
            })
        }
        Os::Other => match LINUX_TERMINALS.iter().find(|t| found(t)) {
            Some(known) => Ok(Choice::Named(known)),
            None => bail!("no terminal found to start; set `[notify] terminal` in config.toml"),
        },
    }
}

/// A command to run, and a script it needs written first, if any.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    /// A file to write before running `argv`, with its text: an executable
    /// one.
    pub script: Option<(PathBuf, String)>,
    pub argv: Vec<String>,
}

/// What `jeet open <url>` is, run from anywhere: its environment is set in
/// the command, as a terminal started by another app or by `open` has not
/// jeet's.
pub fn open_argv(exe: &Path, home: &Path, path: &str, url: &str) -> Vec<String> {
    [
        "/usr/bin/env".to_string(),
        format!("PATH={path}"),
        format!("JEET_HOME={}", home.display()),
        format!("{STARTED_ENV}=1"),
        exe.display().to_string(),
        "open".to_string(),
        url.to_string(),
    ]
    .to_vec()
}

/// How to start `choice` running `jeet`, a whole command line. `script` is
/// where Terminal.app's command file goes.
pub fn invocation(choice: &Choice, jeet: &[String], script: &Path) -> Result<Invocation> {
    let with = |head: &[&str]| -> Vec<String> {
        head.iter()
            .map(|s| s.to_string())
            .chain(jeet.iter().cloned())
            .collect()
    };
    let plain = |argv| Invocation { script: None, argv };
    Ok(match choice {
        Choice::Off => bail!("no jeet screen is open and `[notify] terminal` is none"),
        Choice::Ghostty => plain(with(&[
            "open",
            "-na",
            "Ghostty.app",
            "--args",
            "--wait-after-command=false",
            "--quit-after-last-window-closed=true",
            "-e",
        ])),
        // Terminal.app runs a file, not a command: the file removes itself
        // and becomes jeet.
        Choice::TerminalApp => {
            let quoted: Vec<String> = jeet.iter().map(|w| shell_quote(w)).collect();
            Invocation {
                script: Some((
                    script.to_path_buf(),
                    format!("#!/bin/sh\nrm -f \"$0\"\nexec {}\n", quoted.join(" ")),
                )),
                argv: vec![
                    "open".into(),
                    "-a".into(),
                    "Terminal".into(),
                    script.display().to_string(),
                ],
            }
        }
        Choice::Named(name) => plain(match *name {
            "gnome-terminal" => with(&["gnome-terminal", "--"]),
            "kitty" => with(&["kitty"]),
            "wezterm" => with(&["wezterm", "start", "--"]),
            other => with(&[other, "-e"]),
        }),
        Choice::Custom(command) => {
            let mut argv = split_command(command);
            if argv.is_empty() {
                bail!("`[notify] terminal` is empty");
            }
            argv.extend(jeet.iter().cloned());
            plain(argv)
        }
    })
}

/// The macOS app name that brings `choice`'s windows forward, when it is
/// one jeet knows.
pub fn app_name(choice: &Choice) -> Option<&'static str> {
    match choice {
        Choice::Ghostty => Some("Ghostty"),
        Choice::TerminalApp => Some("Terminal"),
        _ => None,
    }
}

/// Whether `program` is found: a file at that path, or an executable on
/// `$PATH`.
pub fn exists(program: &str) -> bool {
    let path = match program.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|home| home.join(rest)),
        None => Some(PathBuf::from(program)),
    };
    match path {
        Some(path) if path.is_absolute() => path.exists(),
        _ => {
            let Some(paths) = std::env::var_os("PATH") else {
                return false;
            };
            std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(program)))
        }
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run `run`, detached: its output goes nowhere. A launcher that fails
/// within [`LAUNCH_WAIT`] is an error; one still running then is a terminal
/// that is up, and is left to itself.
pub fn start(run: &Invocation) -> Result<()> {
    if let Some((path, text)) = &run.script {
        write_script(path, text)?;
    }
    let Some((program, args)) = run.argv.split_first() else {
        bail!("no terminal command to run");
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let deadline = Instant::now() + LAUNCH_WAIT;
    while Instant::now() < deadline {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => bail!("{program} could not start the terminal ({status})"),
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
    // Still running: reap it whenever it ends, if this process lasts.
    thread::spawn(move || child.wait());
    Ok(())
}

fn write_script(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jeet() -> Vec<String> {
        open_argv(
            Path::new("/opt/bin/jeet"),
            Path::new("/home/me/.jeet"),
            "/usr/bin:/bin",
            "jeet://task/fix",
        )
    }

    fn resolved(setting: Option<&str>, os: Os, have: &[&str]) -> Result<Choice> {
        resolve(setting, os, |p| have.contains(&p))
    }

    #[test]
    fn jeet_open_is_run_with_its_environment() {
        assert_eq!(
            jeet(),
            [
                "/usr/bin/env",
                "PATH=/usr/bin:/bin",
                "JEET_HOME=/home/me/.jeet",
                "JEET_LINK_TERMINAL=1",
                "/opt/bin/jeet",
                "open",
                "jeet://task/fix"
            ]
        );
    }

    #[test]
    fn unset_picks_the_platform_default() {
        let mac = |have: &[&str]| resolved(None, Os::MacOs, have).unwrap();
        assert_eq!(mac(&[]), Choice::TerminalApp);
        assert_eq!(mac(&["/Applications/Ghostty.app"]), Choice::Ghostty);
        assert_eq!(mac(&["~/Applications/Ghostty.app"]), Choice::Ghostty);
        let linux = |have: &[&str]| resolved(Some("  "), Os::Other, have);
        assert_eq!(
            linux(&["xterm", "konsole"]).unwrap(),
            Choice::Named("konsole")
        );
        assert_eq!(
            linux(&["xterm", "ghostty"]).unwrap(),
            Choice::Named("ghostty")
        );
        let none = linux(&[]).unwrap_err().to_string();
        assert!(none.contains("[notify] terminal"), "{none}");
    }

    #[test]
    fn a_setting_is_a_name_none_or_a_command() {
        let pick = |setting, os| resolved(Some(setting), os, &[]).unwrap();
        assert_eq!(pick("none", Os::MacOs), Choice::Off);
        assert_eq!(pick("None", Os::Other), Choice::Off);
        assert_eq!(pick("Ghostty", Os::MacOs), Choice::Ghostty);
        assert_eq!(pick("terminal", Os::MacOs), Choice::TerminalApp);
        assert_eq!(pick("kitty", Os::Other), Choice::Named("kitty"));
        // Names of the other platform's, and unknown ones, are commands.
        assert_eq!(pick("kitty", Os::MacOs), Choice::Custom("kitty".into()));
        assert_eq!(
            pick("terminal", Os::Other),
            Choice::Custom("terminal".into())
        );
        assert_eq!(
            pick("wezterm start --", Os::MacOs),
            Choice::Custom("wezterm start --".into())
        );
    }

    #[test]
    fn ghostty_gets_the_jeet_argv_unparsed() {
        let run = invocation(&Choice::Ghostty, &jeet(), Path::new("/x")).unwrap();
        assert_eq!(run.script, None);
        assert_eq!(
            run.argv[..8],
            [
                "open",
                "-na",
                "Ghostty.app",
                "--args",
                "--wait-after-command=false",
                "--quit-after-last-window-closed=true",
                "-e",
                "/usr/bin/env"
            ]
        );
        assert_eq!(run.argv[7..], jeet());
    }

    #[test]
    fn terminal_app_runs_a_command_file_that_removes_itself() {
        let script = Path::new("/home/me/.jeet/tui/open-7.command");
        let mut argv = jeet();
        argv[2] = "JEET_HOME=/home/it's me/.jeet".into();
        let run = invocation(&Choice::TerminalApp, &argv, script).unwrap();
        assert_eq!(
            run.argv,
            [
                "open",
                "-a",
                "Terminal",
                "/home/me/.jeet/tui/open-7.command"
            ]
        );
        let (path, text) = run.script.unwrap();
        assert_eq!(path, script);
        assert_eq!(
            text,
            "#!/bin/sh\nrm -f \"$0\"\nexec '/usr/bin/env' 'PATH=/usr/bin:/bin' \
             'JEET_HOME=/home/it'\\''s me/.jeet' 'JEET_LINK_TERMINAL=1' '/opt/bin/jeet' \
             'open' 'jeet://task/fix'\n"
        );
    }

    #[test]
    fn linux_terminals_each_take_the_command_their_way() {
        let argv = |name: &'static str| {
            invocation(&Choice::Named(name), &["j".into()], Path::new("/x"))
                .unwrap()
                .argv
        };
        assert_eq!(argv("xterm"), ["xterm", "-e", "j"]);
        assert_eq!(argv("konsole"), ["konsole", "-e", "j"]);
        assert_eq!(
            argv("x-terminal-emulator"),
            ["x-terminal-emulator", "-e", "j"]
        );
        assert_eq!(argv("gnome-terminal"), ["gnome-terminal", "--", "j"]);
        assert_eq!(argv("kitty"), ["kitty", "j"]);
        assert_eq!(argv("wezterm"), ["wezterm", "start", "--", "j"]);
    }

    #[test]
    fn a_custom_command_is_split_and_given_jeet() {
        let run = invocation(
            &Choice::Custom("wezterm start --cwd '/a b' --".into()),
            &["j".into(), "open".into()],
            Path::new("/x"),
        )
        .unwrap();
        assert_eq!(
            run.argv,
            ["wezterm", "start", "--cwd", "/a b", "--", "j", "open"]
        );
    }

    #[test]
    fn none_starts_nothing() {
        let err = invocation(&Choice::Off, &jeet(), Path::new("/x")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no jeet screen is open and `[notify] terminal` is none"
        );
    }

    #[test]
    fn only_some_choices_have_an_app_to_bring_forward() {
        assert_eq!(app_name(&Choice::Ghostty), Some("Ghostty"));
        assert_eq!(app_name(&Choice::TerminalApp), Some("Terminal"));
        assert_eq!(app_name(&Choice::Custom("x".into())), None);
        assert_eq!(app_name(&Choice::Named("xterm")), None);
    }
}
