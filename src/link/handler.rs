//! Making the system hand `plete://` links to `plete open`.
//!
//! macOS has no way to register a URL scheme without an app bundle, so plete
//! builds a tiny one with `osacompile`: an applet whose only job is to run
//! `plete open` on the URL. It lives in plete's data directory, ad-hoc signed
//! so that no signing identity is needed, and is only made when asked for
//! (`plete notify install-link`). On Linux it is a `.desktop` file and an
//! `xdg-mime` default.
//!
//! The path of the plete binary is the one running at install time
//! (`current_exe`) and is written into the handler: install again after
//! moving or reinstalling plete. The terminal `plete open` starts bakes it
//! in the same way, but fresh each time.
//!
//! The text of both is built by pure functions, and what goes into them is
//! quoted for the format it lands in, as the paths come from the
//! environment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
                          LaunchServices.framework/Support/lsregister";

const DESKTOP_FILE: &str = "plete-link.desktop";
/// The desktop file before the rename, removed by an install or uninstall.
const LEGACY_DESKTOP_FILE: &str = "jeet-link.desktop";
/// The schemes the handler opens: the new name and the old.
const MIME_TYPES: [&str; 2] = ["x-scheme-handler/plete", "x-scheme-handler/jeet"];

/// `plete notify install-link`.
pub fn install(home: &Path) -> Result<()> {
    let exe = crate::paths::plete_exe()?;
    let path = std::env::var("PATH").context("read $PATH")?;
    match cfg!(target_os = "macos") {
        true => install_macos(home, &exe, &path),
        false => install_linux(home, &exe, &path),
    }
}

/// Whether `plete notify install-link` has put the handler in place, so a
/// `plete://` link is worth offering to click.
pub fn installed() -> bool {
    let path = match cfg!(target_os = "macos") {
        true => bundle_path(),
        false => applications_dir().map(|dir| dir.join(DESKTOP_FILE)),
    };
    path.is_ok_and(|p| p.exists())
}

/// Whether the handler from before the rename is still in place.
pub fn old_installed() -> bool {
    let path = match cfg!(target_os = "macos") {
        true => legacy_bundle_path(),
        false => applications_dir().map(|dir| dir.join(LEGACY_DESKTOP_FILE)),
    };
    path.is_ok_and(|p| p.exists())
}

/// `plete notify uninstall-link`.
pub fn uninstall() -> Result<()> {
    match cfg!(target_os = "macos") {
        true => uninstall_macos(),
        false => uninstall_linux(),
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

fn bundle_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("could not determine the data directory")?
        .join("plete")
        .join("PleteLink.app"))
}

/// Where the handler was built before the rename.
fn legacy_bundle_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("could not determine the data directory")?
        .join("jeet")
        .join("JeetLink.app"))
}

/// Unregister and remove the handler built before the rename, if any.
fn remove_legacy_bundle() -> Result<bool> {
    let old = legacy_bundle_path()?;
    if !old.exists() {
        return Ok(false);
    }
    let _ = run(LSREGISTER, &["-u", &old.to_string_lossy()]);
    fs::remove_dir_all(&old).with_context(|| format!("remove {}", old.display()))?;
    Ok(true)
}

/// A value as an AppleScript string literal's contents: `\` and `"`
/// escaped. Control characters, which could end the line, are refused.
fn applescript_string(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("cannot put {value:?} in a link handler: it has a control character");
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The applet's script: on a URL, run `plete open` on it with `PATH` and
/// `PLETE_HOME` as they are now, as an applet's shell has almost no
/// environment, and say what happened in `<home>/tui/link.log`.
fn applescript(path: &str, home: &Path, exe: &Path) -> Result<String> {
    let lit = |s: &str| applescript_string(s);
    let home_text = home.to_string_lossy();
    let log = home.join("tui").join("link.log");
    Ok(format!(
        "on open location theURL\n\
         \tdo shell script \"/usr/bin/env \" & quoted form of \"PATH={path}\" \
         & \" \" & quoted form of \"JEET_HOME={home}\" \
         & \" \" & quoted form of \"PLETE_HOME={home}\" \
         & \" \" & quoted form of \"{exe}\" & \" open \" & quoted form of theURL \
         & \" > \" & quoted form of \"{log}\" & \" 2>&1\"\n\
         end open location\n",
        path = lit(path)?,
        home = lit(&home_text)?,
        exe = lit(&exe.to_string_lossy())?,
        log = lit(&log.to_string_lossy())?,
    ))
}

fn install_macos(home: &Path, exe: &Path, path: &str) -> Result<()> {
    let bundle = bundle_path()?;
    let script = applescript(path, home, exe)?;
    let dir = bundle.parent().context("no directory for the handler")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::create_dir_all(home.join("tui"))?;

    // Build beside the old one, so that a build that fails leaves it be,
    // then swap, forgetting the old so the system does not keep two. The
    // new one's name ends in `.app` too: osacompile picks what it writes
    // by the extension, and `PleteLink.app.new` comes out a script file.
    let staging = dir.join("staging");
    let fresh = staging.join("PleteLink.app");
    fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    let source = dir.join(".PleteLink.applescript");
    let _ = fs::remove_dir_all(&fresh);
    fs::write(&source, script).with_context(|| format!("write {}", source.display()))?;
    let built = build_bundle(&source, &fresh);
    let _ = fs::remove_file(&source);
    if let Err(e) = built {
        let _ = fs::remove_dir_all(&fresh);
        return Err(e);
    }
    if bundle.exists() {
        let _ = run(LSREGISTER, &["-u", &bundle.to_string_lossy()]);
        fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    }
    fs::rename(&fresh, &bundle).with_context(|| format!("move into {}", bundle.display()))?;
    let _ = fs::remove_dir(&staging);
    run(LSREGISTER, &["-f", &bundle.to_string_lossy()])?;
    if remove_legacy_bundle()? {
        println!("removed the old JeetLink.app handler");
    }

    println!(
        "installed the plete:// link handler at {}",
        bundle.display()
    );
    println!("test it with: open plete://task/<a task's name>");
    println!(
        "a browser or Slack asks once before it opens a plete link; \
         `plete notify uninstall-link` removes the handler"
    );
    Ok(())
}

fn build_bundle(source: &Path, bundle: &Path) -> Result<()> {
    let bundle_text = bundle.to_string_lossy();
    run(
        "osacompile",
        &["-o", &bundle_text, &source.to_string_lossy()],
    )?;
    let plist = format!("{bundle_text}/Contents/Info.plist");
    run(
        "plutil",
        &[
            "-replace",
            "CFBundleIdentifier",
            "-string",
            "dev.plete.link",
            &plist,
        ],
    )?;
    run(
        "plutil",
        &["-replace", "LSUIElement", "-bool", "true", &plist],
    )?;
    run(
        "plutil",
        &[
            "-replace",
            "CFBundleURLTypes",
            "-json",
            r#"[{"CFBundleURLName":"plete link","CFBundleURLSchemes":["plete","jeet"]}]"#,
            &plist,
        ],
    )?;
    // Editing the plist broke the seal osacompile put on it: seal it again,
    // ad hoc.
    run("codesign", &["--force", "-s", "-", &bundle_text])
}

fn uninstall_macos() -> Result<()> {
    let bundle = bundle_path()?;
    let old = remove_legacy_bundle()?;
    if !bundle.exists() {
        match old {
            true => println!("removed the old JeetLink.app link handler"),
            false => println!("no plete:// link handler installed"),
        }
        return Ok(());
    }
    let _ = run(LSREGISTER, &["-u", &bundle.to_string_lossy()]);
    fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    println!("removed the plete:// link handler");
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

fn applications_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("could not determine the data directory")?
        .join("applications"))
}

/// An argument of a `.desktop` file's `Exec`, quoted as the Desktop Entry
/// specification has it: in double quotes, with `"`, `` ` ``, `$` and `\`
/// escaped, and `%` doubled, as `%u` and the like are field codes. The
/// file's own string escapes are undone before the quoting is read, so a
/// literal `\` is written as four.
fn desktop_quote(arg: &str) -> Result<String> {
    if arg.chars().any(char::is_control) {
        bail!("cannot put {arg:?} in a link handler: it has a control character");
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

fn desktop_entry(path: &str, home: &Path, exe: &Path) -> Result<String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=plete link\n\
         Exec=/usr/bin/env {} {} {} {} open %u\n\
         MimeType={mimes};\n\
         NoDisplay=true\n\
         Terminal=false\n",
        desktop_quote(&format!("PATH={path}"))?,
        desktop_quote(&format!("JEET_HOME={}", home.display()))?,
        desktop_quote(&format!("PLETE_HOME={}", home.display()))?,
        desktop_quote(&exe.to_string_lossy())?,
        mimes = MIME_TYPES.join(";"),
    ))
}

fn install_linux(home: &Path, exe: &Path, path: &str) -> Result<()> {
    let dir = applications_dir()?;
    let file = dir.join(DESKTOP_FILE);
    let entry = desktop_entry(path, home, exe)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(&file, entry).with_context(|| format!("write {}", file.display()))?;
    println!("wrote {}", file.display());
    if fs::remove_file(dir.join(LEGACY_DESKTOP_FILE)).is_ok() {
        println!("removed the old {LEGACY_DESKTOP_FILE}");
    }
    let registered = MIME_TYPES
        .iter()
        .try_for_each(|mime| run("xdg-mime", &["default", DESKTOP_FILE, mime]));
    match registered {
        Ok(()) => {
            if which("update-desktop-database") {
                let _ = run("update-desktop-database", &[&dir.to_string_lossy()]);
            }
            println!("plete:// links now open with it; test with: xdg-open plete://task/<name>");
        }
        Err(_) if !which("xdg-mime") => println!(
            "xdg-mime was not found: register {DESKTOP_FILE} as the handler of {} yourself",
            MIME_TYPES.join(" and ")
        ),
        Err(e) => bail!("could not register the handler: {e:#}"),
    }
    Ok(())
}

/// `mimeapps.list` without plete's own entry on its `x-scheme-handler/plete=`
/// lines: another app's stays, and a line left empty goes.
fn without_plete_handler(list: &str) -> String {
    MIME_TYPES
        .iter()
        .fold(list.to_string(), |list, mime| without_handler(&list, mime))
}

fn without_handler(list: &str, mime: &str) -> String {
    let key = format!("{mime}=");
    let mut out: String = list
        .lines()
        .filter_map(|line| {
            let Some(value) = line.strip_prefix(&key) else {
                return Some(line.to_string());
            };
            let kept: Vec<&str> = value
                .split(';')
                .filter(|app| {
                    !app.is_empty() && *app != DESKTOP_FILE && *app != LEGACY_DESKTOP_FILE
                })
                .collect();
            match kept.len() {
                0 => None,
                _ if kept.len() == value.split(';').filter(|a| !a.is_empty()).count() => {
                    Some(line.to_string())
                }
                _ => Some(format!("{key}{};", kept.join(";"))),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if list.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn uninstall_linux() -> Result<()> {
    let dir = applications_dir()?;
    let _ = fs::remove_file(dir.join(LEGACY_DESKTOP_FILE));
    let file = dir.join(DESKTOP_FILE);
    match fs::remove_file(&file) {
        Ok(()) => println!("removed {}", file.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no plete:// link handler installed")
        }
        Err(e) => return Err(e).with_context(|| format!("remove {}", file.display())),
    }
    if let Some(list) = dirs::config_dir().map(|dir| dir.join("mimeapps.list")) {
        if let Ok(text) = fs::read_to_string(&list) {
            let kept = without_plete_handler(&text);
            if kept != text {
                fs::write(&list, kept).with_context(|| format!("write {}", list.display()))?;
            }
        }
    }
    if which("update-desktop-database") {
        let _ = run("update-desktop-database", &[&dir.to_string_lossy()]);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Running tools
// ---------------------------------------------------------------------------

fn which(program: &str) -> bool {
    super::terminal::exists(program)
}

/// Run `program`, failing with what it said when it does.
fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_applet_script_embeds_quoted_values() {
        let script = applescript(
            "/usr/bin:/bin",
            Path::new("/Users/me/.plete"),
            Path::new("/opt/plete"),
        )
        .unwrap();
        assert_eq!(
            script,
            "on open location theURL\n\
             \tdo shell script \"/usr/bin/env \" & quoted form of \"PATH=/usr/bin:/bin\" \
             & \" \" & quoted form of \"JEET_HOME=/Users/me/.plete\" \
             & \" \" & quoted form of \"PLETE_HOME=/Users/me/.plete\" \
             & \" \" & quoted form of \"/opt/plete\" & \" open \" & quoted form of theURL \
             & \" > \" & quoted form of \"/Users/me/.plete/tui/link.log\" & \" 2>&1\"\n\
             end open location\n"
        );
    }

    #[test]
    fn applescript_literals_escape_and_refuse() {
        assert_eq!(
            applescript_string(r#"a"b\c"#).unwrap(),
            r#"a\"b\\c"#.to_string()
        );
        assert!(applescript_string("a\nb").is_err());
        assert!(applescript_string("a\u{0}b").is_err());
        let err = applescript("p\nq", Path::new("/h"), Path::new("/e"));
        assert!(err.is_err());
    }

    #[test]
    fn desktop_arguments_are_quoted_per_the_spec() {
        assert_eq!(desktop_quote("plain").unwrap(), "\"plain\"");
        assert_eq!(
            desktop_quote("a \"b\" `c` $d \\e 50%").unwrap(),
            "\"a \\\"b\\\" \\`c\\` \\$d \\\\\\\\e 50%%\""
        );
        assert!(desktop_quote("a\nb").is_err());
    }

    #[test]
    fn the_desktop_entry_has_the_scheme_and_exec() {
        let entry = desktop_entry(
            "/usr/bin:/bin",
            Path::new("/h/.plete"),
            Path::new("/opt/my plete"),
        )
        .unwrap();
        assert_eq!(
            entry,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=plete link\n\
             Exec=/usr/bin/env \"PATH=/usr/bin:/bin\" \"JEET_HOME=/h/.plete\" \"PLETE_HOME=/h/.plete\" \"/opt/my plete\" open %u\n\
             MimeType=x-scheme-handler/plete;x-scheme-handler/jeet;\n\
             NoDisplay=true\n\
             Terminal=false\n"
        );
    }

    #[test]
    fn the_mime_default_is_dropped_and_the_rest_kept() {
        let list = "[Default Applications]\nx-scheme-handler/plete=plete-link.desktop\n\
                    text/html=firefox.desktop\n\n[Added Associations]\n\
                    x-scheme-handler/plete=plete-link.desktop;\n";
        assert_eq!(
            without_plete_handler(list),
            "[Default Applications]\ntext/html=firefox.desktop\n\n[Added Associations]\n"
        );
        let old = "x-scheme-handler/jeet=jeet-link.desktop\ntext/html=firefox.desktop\n";
        assert_eq!(without_plete_handler(old), "text/html=firefox.desktop\n");
        let untouched = "[Default Applications]\ntext/html=firefox.desktop\n";
        assert_eq!(without_plete_handler(untouched), untouched);
        let shared = "x-scheme-handler/plete=other.desktop;plete-link.desktop;\n\
                      x-scheme-handler/plete=other.desktop\n\
                      x-scheme-handler/plete=plete-link.desktop-not\n";
        assert_eq!(
            without_plete_handler(shared),
            "x-scheme-handler/plete=other.desktop;\n\
             x-scheme-handler/plete=other.desktop\n\
             x-scheme-handler/plete=plete-link.desktop-not\n"
        );
    }
}
