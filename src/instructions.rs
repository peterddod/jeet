//! The agent's instruction file: `AGENTS.md`, the name Codex, Cursor, Amp,
//! Gemini CLI and other coding agents read, next to a `CLAUDE.md` that only
//! imports it (`@AGENTS.md`).
//!
//! Recent Claude Code reads `AGENTS.md` itself, but only when no `CLAUDE.md`
//! is around, and some sessions (older versions, the first session after an
//! upgrade) read `CLAUDE.md` alone. The import covers those, Claude never
//! reads the file twice, and it is a plain file, which copies anywhere a
//! symlink might not.
//!
//! Before AGENTS.md, jeet wrote `CLAUDE.md` itself. [`migrate`] moves such a
//! file to `AGENTS.md` and leaves the import in its place, never replacing
//! anything.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

pub const AGENTS_MD: &str = "AGENTS.md";
pub const CLAUDE_MD: &str = "CLAUDE.md";

/// All a `CLAUDE.md` next to an `AGENTS.md` needs: Claude Code reads the
/// import as if the file were inlined.
pub const CLAUDE_SHIM: &str = "@AGENTS.md\n";

/// Whether `text` is the import and nothing else.
pub fn is_shim(text: &[u8]) -> bool {
    text.trim_ascii() == CLAUDE_SHIM.trim().as_bytes()
}

/// Move a `CLAUDE.md` in `dir` to `AGENTS.md`, leaving [`CLAUDE_SHIM`] in its
/// place. Only a plain `CLAUDE.md` with something of its own in it is moved,
/// and only where there is no `AGENTS.md`; otherwise nothing is touched.
/// Returns whether it moved.
///
/// Every step keeps the text somewhere: the copy is made with `create_new`,
/// so it never replaces an `AGENTS.md`, and only then is the shim renamed
/// over `CLAUDE.md`.
pub fn migrate(dir: &Path) -> Result<bool> {
    let claude = dir.join(CLAUDE_MD);
    let agents = dir.join(AGENTS_MD);
    match std::fs::symlink_metadata(&claude) {
        Ok(meta) if meta.is_file() => {}
        _ => return Ok(false),
    }
    if std::fs::symlink_metadata(&agents).is_ok() {
        return Ok(false);
    }
    let text = std::fs::read(&claude).with_context(|| format!("read {}", claude.display()))?;
    if is_shim(&text) {
        return Ok(false);
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&agents);
    let mut file = match file {
        Ok(file) => file,
        // Written meanwhile: theirs stands, and so does CLAUDE.md.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("write {}", agents.display())),
    };
    let copied = file.write_all(&text).and_then(|()| file.sync_all());
    if let Err(e) = copied {
        drop(file);
        let _ = std::fs::remove_file(&agents);
        return Err(e).with_context(|| format!("write {}", agents.display()));
    }

    let tmp = dir.join(".CLAUDE.md.jeet-tmp");
    std::fs::write(&tmp, CLAUDE_SHIM).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &claude) {
        let _ = std::fs::remove_file(&tmp);
        // CLAUDE.md is as it was, and AGENTS.md a copy of it: nothing lost.
        return Err(e).with_context(|| format!("replace {}", claude.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn an_edited_claude_md_moves_to_agents_md_behind_the_import() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CLAUDE_MD), "my rules\n").unwrap();
        assert!(migrate(dir.path()).unwrap());
        assert_eq!(read(&dir.path().join(AGENTS_MD)), "my rules\n");
        assert_eq!(read(&dir.path().join(CLAUDE_MD)), CLAUDE_SHIM);
        assert!(!dir.path().join(".CLAUDE.md.jeet-tmp").exists());
        // Done once: nothing more to move.
        assert!(!migrate(dir.path()).unwrap());
        assert_eq!(read(&dir.path().join(AGENTS_MD)), "my rules\n");
    }

    #[test]
    fn nothing_is_replaced_when_both_files_are_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CLAUDE_MD), "claude's\n").unwrap();
        std::fs::write(dir.path().join(AGENTS_MD), "everyone's\n").unwrap();
        assert!(!migrate(dir.path()).unwrap());
        assert_eq!(read(&dir.path().join(CLAUDE_MD)), "claude's\n");
        assert_eq!(read(&dir.path().join(AGENTS_MD)), "everyone's\n");
    }

    #[test]
    fn a_lone_shim_or_no_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!migrate(dir.path()).unwrap());
        assert!(!dir.path().join(AGENTS_MD).exists());
        std::fs::write(dir.path().join(CLAUDE_MD), "@AGENTS.md").unwrap();
        assert!(!migrate(dir.path()).unwrap());
        assert!(!dir.path().join(AGENTS_MD).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_claude_md_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("rules"), "elsewhere\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("rules"), dir.path().join(CLAUDE_MD))
            .unwrap();
        assert!(!migrate(dir.path()).unwrap());
        assert!(!dir.path().join(AGENTS_MD).exists());
    }

    #[test]
    fn the_shim_is_recognised_with_or_without_a_newline() {
        assert!(is_shim(CLAUDE_SHIM.as_bytes()));
        assert!(is_shim(b"  @AGENTS.md  \n\n"));
        assert!(!is_shim(b"@AGENTS.md\nand more\n"));
    }
}
