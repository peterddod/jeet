//! Directory hand-off to the shell wrapper.
//!
//! The `plete` shell function created by `plete init-shell` sets `PLETE_CD_FILE`
//! to a temporary file and `cd`s to whatever path the binary writes there. This
//! lets commands (and the explorer) leave the shell in a new directory without
//! having to parse stdout.

use std::path::Path;

use anyhow::{Context, Result};

pub const CD_FILE_ENV: &str = "PLETE_CD_FILE";

/// Ask the shell wrapper to `cd` into `path` once plete exits.
pub fn request(path: &Path) -> Result<()> {
    let Some(file) = crate::env::var(CD_FILE_ENV).ok().filter(|f| !f.is_empty()) else {
        return Ok(());
    };
    std::fs::write(&file, path.to_string_lossy().as_bytes())
        .with_context(|| format!("write cd hand-off file {file}"))
}

/// Whether the shell wrapper is active, so `cd` hand-off will be honoured.
pub fn wrapper_active() -> bool {
    crate::env::var(CD_FILE_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}
