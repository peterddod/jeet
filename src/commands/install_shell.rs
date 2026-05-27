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
            eprintln!("Run jeet install-shell from a supported shell");
            anyhow::bail!("unsupported shell");
        }
    };

    let marker = "# jeet shell integration — run 'jeet cd'";
    let init_line = "eval \"$(jeet init-shell)\"";
    let full_line = format!("{}\n{}", marker, init_line);

    // Check if already configured
    if rc_file.exists() {
        let content = fs::read_to_string(&rc_file).context("read rc file")?;
        if content.contains(marker) || content.contains("jeet init-shell") {
            println!(
                "{}: jeet shell integration already configured",
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
    writeln!(file, "{}", full_line).context("write jeet config line")?;

    println!("Added to {}: {}", rc_file.display(), init_line);
    println!();
    println!(
        "Run `source {}` to enable jeet cd in this shell.",
        rc_file.display()
    );

    Ok(())
}

fn detect_rc_file(shell: &str) -> Option<String> {
    if shell.contains("zsh") {
        dirs::home_dir().map(|h| h.join(".zshrc").to_string_lossy().into_owned())
    } else if shell.contains("bash") {
        dirs::home_dir().map(|h| h.join(".bashrc").to_string_lossy().into_owned())
    } else {
        None
    }
}
