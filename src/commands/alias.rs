//! `plete alias`: the short name `pt`, a link to plete made only when asked
//! for. Nothing here writes into a directory a package manager owns, nor
//! over a `pt` that is not plete's.
//!
//! What is on `PATH` and what a link points at are passed in, so the
//! decisions are pure and the tests run on temp directories.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cli::{AliasCommands, Commands};

const NAME: &str = "pt";
/// Where a link may go, under the user's home, in the order tried.
const HOME_DIRS: [&str; 2] = [".local/bin", "bin"];
/// Directories a package manager owns, which a link is never put in.
const SYSTEM_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// What `pt` is, looking along a `PATH`.
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing on the path is called `pt`.
    Free,
    /// The first `pt` is a link to plete.
    Ours(PathBuf),
    /// A link to plete is on the path, behind another `pt`.
    Shadowed { ours: PathBuf, by: PathBuf },
    /// A `pt` that is not plete's is first, and no link of plete's is there.
    Other(PathBuf),
}

impl State {
    pub fn describe(&self) -> String {
        match self {
            State::Free => "pt: not installed; `plete alias install` makes it".to_string(),
            State::Ours(p) => format!("pt: ours, at {}", p.display()),
            State::Shadowed { ours, by } => format!(
                "pt: the link at {} is shadowed by {}",
                ours.display(),
                by.display()
            ),
            State::Other(p) => format!(
                "pt: a different pt exists at {}; plete leaves it alone",
                p.display()
            ),
        }
    }
}

/// Every `pt` in `path_dirs`, in order. A dangling link counts.
fn found(path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    path_dirs
        .iter()
        .map(|dir| dir.join(NAME))
        .filter(|p| p.symlink_metadata().is_ok())
        .collect()
}

/// Whether `link` is a link to plete: to `exe`, or to anything called
/// `plete` or `jeet` (a package manager's path moves with each version).
fn is_ours(link: &Path, exe: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    matches!(
        target.file_name().and_then(|n| n.to_str()),
        Some("plete" | "jeet")
    ) || matches!((link.canonicalize(), exe.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

pub fn state(path_dirs: &[PathBuf], exe: &Path) -> State {
    let all = found(path_dirs);
    let Some(first) = all.first() else {
        return State::Free;
    };
    if is_ours(first, exe) {
        return State::Ours(first.clone());
    }
    match all.iter().find(|p| is_ours(p, exe)) {
        Some(ours) => State::Shadowed {
            ours: ours.clone(),
            by: first.clone(),
        },
        None => State::Other(first.clone()),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    crate::config::canonicalize_lenient(a) == crate::config::canonicalize_lenient(b)
}

/// Make the link to `exe`: in `dir` if given, else in the first of
/// `~/.local/bin` and `~/bin` that is on `path_dirs` and takes it. Returns
/// where `pt` is (already, if it was ours).
pub fn install(
    path_dirs: &[PathBuf],
    home: &Path,
    exe: &Path,
    dir: Option<&Path>,
) -> Result<PathBuf> {
    match state(path_dirs, exe) {
        State::Ours(p) => return Ok(p),
        State::Shadowed { ours, by } => bail!(
            "your pt link at {} is shadowed by {}; put its directory first on PATH",
            ours.display(),
            by.display()
        ),
        State::Other(p) => bail!("{} is already a pt; plete does not touch it", p.display()),
        State::Free => {}
    }
    let candidates: Vec<PathBuf> = match dir {
        Some(dir) => {
            if SYSTEM_DIRS.iter().any(|s| same_dir(dir, Path::new(s))) {
                bail!(
                    "{} belongs to a package manager; pick another --dir",
                    dir.display()
                );
            }
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            vec![dir.to_path_buf()]
        }
        None => HOME_DIRS
            .iter()
            .map(|d| home.join(d))
            .filter(|d| d.is_dir() && path_dirs.iter().any(|p| same_dir(p, d)))
            .collect(),
    };
    for candidate in &candidates {
        let link = candidate.join(NAME);
        if std::os::unix::fs::symlink(exe, &link).is_ok() {
            return Ok(link);
        }
    }
    let at = HOME_DIRS[0];
    bail!(
        "neither ~/{} nor ~/{} is on your PATH and writable; make the link in a directory of \
         your choosing, e.g.: ln -s {} ~/{at}/{NAME}",
        HOME_DIRS[0],
        HOME_DIRS[1],
        exe.display()
    )
}

/// Remove the links to plete named `pt` along `path_dirs` and in the
/// directories [`install`] chooses from. Returns what was removed.
pub fn uninstall(path_dirs: &[PathBuf], home: &Path, exe: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = path_dirs.to_vec();
    dirs.extend(HOME_DIRS.iter().map(|d| home.join(d)));
    let mut removed = Vec::new();
    for dir in dirs {
        let link = dir.join(NAME);
        if removed.contains(&link) || !is_ours(&link, exe) {
            continue;
        }
        std::fs::remove_file(&link).with_context(|| format!("remove {}", link.display()))?;
        removed.push(link);
    }
    Ok(removed)
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// `plete alias install|status|uninstall`.
pub fn run(command: AliasCommands) -> Result<()> {
    let exe = crate::paths::plete_exe()?;
    let path = path_dirs();
    let home = dirs::home_dir().context("could not determine home directory")?;
    match command {
        AliasCommands::Install { dir } => {
            let link = install(&path, &home, &exe, dir.as_deref())?;
            println!("{}", State::Ours(link.clone()).describe());
            if let Some(parent) = link.parent() {
                if !path.iter().any(|p| same_dir(p, parent)) {
                    eprintln!("plete: {} is not on your PATH", parent.display());
                }
            }
        }
        AliasCommands::Status => println!("{}", state(&path, &exe).describe()),
        AliasCommands::Uninstall => match uninstall(&path, &home, &exe)?.as_slice() {
            [] => println!("pt: no link to plete to remove"),
            removed => {
                for link in removed {
                    println!("removed {}", link.display());
                }
            }
        },
    }
    Ok(())
}

/// Whether the note behind `stamp` is due, marking it shown if so: when
/// there is no stamp, or (with `every`) when it is older than that. A stamp
/// that cannot be written means no note, rather than one on every run.
pub fn take_stamp(stamp: &Path, every: Option<Duration>) -> bool {
    let age = stamp
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .map(|t| t.elapsed().unwrap_or_default());
    let due = match (age, every) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(age), Some(every)) => age >= every,
    };
    due && std::fs::write(stamp, "").is_ok()
}

/// The invoked name's file name, which `jeet` means the old command.
fn invoked_as_jeet() -> bool {
    std::env::args_os()
        .next()
        .and_then(|a| Path::new(&a).file_name().map(|n| n == "jeet"))
        .unwrap_or(false)
}

/// The index home, when a note may be stamped in it and shown: only to a
/// person at a terminal, so hooks and jobs stay quiet.
fn home_for_notes() -> Option<PathBuf> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    crate::config::plete_home().ok().filter(|h| h.is_dir())
}

/// That `jeet` is now `plete`, to stderr, at most once a day (stamped in the
/// index home), when run under the old name. Never in a pipe.
pub fn old_name_note() {
    let Some(home) = home_for_notes() else {
        return;
    };
    if invoked_as_jeet()
        && take_stamp(
            &home.join(".jeet-renamed"),
            Some(Duration::from_secs(86_400)),
        )
    {
        eprintln!("plete: jeet is now called plete; the jeet command keeps working");
    }
}

/// Whether `command` is one where a person is at the keyboard and `pt` is
/// worth a mention: the tasks list and `plete setup`.
pub fn offers_pt(command: Option<&Commands>) -> bool {
    matches!(
        command,
        None | Some(Commands::Tasks | Commands::Setup { command: None, .. })
    )
}

/// A hint that `pt` is free, once ever (stamped in the index home), after an
/// interactive command has run. Never in a pipe, never makes the link.
pub fn pt_hint() {
    let Some(home) = home_for_notes() else {
        return;
    };
    if !std::io::stdin().is_terminal() {
        return;
    }
    let Ok(exe) = crate::paths::plete_exe() else {
        return;
    };
    if state(&path_dirs(), &exe) == State::Free && take_stamp(&home.join(".pt-hint"), None) {
        eprintln!("plete: pt is free: run `plete alias install` for a short command");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake exe, a `~/.local/bin` and `~/bin` under one temp dir.
    struct Lab {
        dir: tempfile::TempDir,
    }

    impl Lab {
        fn new() -> Lab {
            let lab = Lab {
                dir: tempfile::tempdir().unwrap(),
            };
            for d in ["opt", ".local/bin", "bin", "other"] {
                std::fs::create_dir_all(lab.dir.path().join(d)).unwrap();
            }
            std::fs::write(lab.exe(), "").unwrap();
            lab
        }

        fn p(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn exe(&self) -> PathBuf {
            self.p("opt/plete")
        }
    }

    #[test]
    fn a_link_goes_into_the_first_home_dir_on_the_path() {
        let lab = Lab::new();
        let path = vec![lab.p("other"), lab.p("bin"), lab.p(".local/bin")];
        let link = install(&path, lab.dir.path(), &lab.exe(), None).unwrap();
        // Of the two, ~/.local/bin is tried first.
        assert_eq!(link, lab.p(".local/bin/pt"));
        assert_eq!(std::fs::read_link(&link).unwrap(), lab.exe());
        // Again: already ours.
        assert_eq!(
            install(&path, lab.dir.path(), &lab.exe(), None).unwrap(),
            link
        );
        assert_eq!(state(&path, &lab.exe()), State::Ours(link));
    }

    #[test]
    fn with_neither_dir_on_the_path_it_refuses_with_the_command() {
        let lab = Lab::new();
        let path = vec![lab.p("other")];
        let e = install(&path, lab.dir.path(), &lab.exe(), None).unwrap_err();
        assert!(format!("{e}").contains("ln -s"), "{e}");
        assert!(!lab.p(".local/bin/pt").exists());
    }

    #[test]
    fn another_pt_is_never_touched_nor_shadowed() {
        let lab = Lab::new();
        std::fs::write(lab.p("other/pt"), "#!/bin/sh\n").unwrap();
        let path = vec![lab.p("other"), lab.p(".local/bin")];
        assert_eq!(state(&path, &lab.exe()), State::Other(lab.p("other/pt")));
        assert!(install(&path, lab.dir.path(), &lab.exe(), None).is_err());
        assert!(!lab.p(".local/bin/pt").exists());
        // Our link behind it is shadowed, and says by what.
        symlink(lab.exe(), lab.p(".local/bin/pt")).unwrap();
        assert_eq!(
            state(&path, &lab.exe()),
            State::Shadowed {
                ours: lab.p(".local/bin/pt"),
                by: lab.p("other/pt")
            }
        );
        assert!(install(&path, lab.dir.path(), &lab.exe(), None).is_err());
        // Uninstall takes only ours.
        let removed = uninstall(&path, lab.dir.path(), &lab.exe()).unwrap();
        assert_eq!(removed, [lab.p(".local/bin/pt")]);
        assert!(lab.p("other/pt").exists());
    }

    #[test]
    fn a_dir_is_taken_as_given_but_not_a_package_managers() {
        let lab = Lab::new();
        let link = install(&[], lab.dir.path(), &lab.exe(), Some(&lab.p("other"))).unwrap();
        assert_eq!(link, lab.p("other/pt"));
        let e = install(
            &[],
            lab.dir.path(),
            &lab.exe(),
            Some(Path::new("/usr/local/bin")),
        );
        assert!(e.is_err());
        let e = install(
            &[],
            lab.dir.path(),
            &lab.exe(),
            Some(Path::new("/opt/homebrew/bin")),
        );
        assert!(e.is_err());
    }

    #[test]
    fn a_link_to_a_moved_plete_is_still_ours() {
        let lab = Lab::new();
        symlink(
            "/opt/homebrew/Cellar/plete/0.15.0/bin/plete",
            lab.p("bin/pt"),
        )
        .unwrap();
        assert_eq!(
            state(&[lab.p("bin")], &lab.exe()),
            State::Ours(lab.p("bin/pt"))
        );
        assert_eq!(
            uninstall(&[lab.p("bin")], lab.dir.path(), &lab.exe()).unwrap(),
            [lab.p("bin/pt")]
        );
        assert_eq!(state(&[lab.p("bin")], &lab.exe()), State::Free);
    }

    #[test]
    fn a_stamp_shows_a_note_once_or_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join("s");
        assert!(take_stamp(&stamp, None));
        assert!(!take_stamp(&stamp, None));
        assert!(!take_stamp(&stamp, Some(Duration::from_secs(86_400))));
        assert!(take_stamp(&stamp, Some(Duration::ZERO)));
        // Unwritable: no note, not one every run.
        assert!(!take_stamp(&dir.path().join("no/such/dir/s"), None));
    }
}
