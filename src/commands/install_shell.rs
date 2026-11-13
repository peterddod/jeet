use anyhow::{Context, Result};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub fn run() -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_else(|_| String::from(""));
    let rc_file: PathBuf = match detect_rc_file(&shell) {
        Some(path) => path.into(),
        None => {
            eprintln!("Unsupported shell: {}\nSupported shells: zsh, bash", shell);
            eprintln!("Run plete install-shell from a supported shell");
            anyhow::bail!("unsupported shell");
        }
    };

    let _marker = "# plete shell integration — run 'plete cd'";
    let install_line = "source <(plete init-shell)";

    // Check if already configured (look for exact match at line start, skip comments)
    if rc_file.exists() {
        let content = fs::read_to_string(&rc_file).context("read rc file")?;
        if has_integration(&content) {
            println!(
                "{}: plete shell integration already configured",
                rc_file.display()
            );
            return Ok(());
        }
    }

    // Append to rc file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc_file)
        .context("open rc file")?;

    writeln!(file).context("write newline")?;
    writeln!(file, "{}", install_line).context("write plete config line")?;

    println!("Added to {}: {}", rc_file.display(), install_line);
    println!();
    println!(
        "Run `source {}` to enable plete cd in this shell.",
        rc_file.display()
    );

    Ok(())
}

/// Whether `rc` already sources the shell integration, under either name
/// (comments skipped).
fn has_integration(rc: &str) -> bool {
    rc.lines()
        .map(str::trim)
        .any(|line| line == "source <(plete init-shell)" || line == "source <(jeet init-shell)")
}

/// Whether `rc` still sources the integration through the old name.
pub fn has_old_integration(rc: &str) -> bool {
    rc.lines()
        .map(str::trim)
        .any(|line| line == "source <(jeet init-shell)")
}

pub(crate) fn detect_rc_file(shell: &str) -> Option<String> {
    if shell.contains("zsh") {
        dirs::home_dir().map(|h| h.join(".zshrc").to_string_lossy().into_owned())
    } else if shell.contains("bash") {
        dirs::home_dir().map(|h| h.join(".bashrc").to_string_lossy().into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_name_counts_as_installed() {
        assert!(has_integration("a\nsource <(plete init-shell)\n"));
        assert!(has_integration("  source <(jeet init-shell)  \n"));
        assert!(!has_integration(
            "# source <(plete init-shell)\nsource <(other init-shell)\n"
        ));
        assert!(has_old_integration("source <(jeet init-shell)"));
        assert!(!has_old_integration("source <(plete init-shell)"));
    }
}
