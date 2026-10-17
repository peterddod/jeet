//! `jeet setup`: what the setup agent stages in `$JEET_HOME/setup/proposed/`
//! and what applies it. The agent only writes proposals; the user applies
//! each one at their own terminal ([`apply`]).

pub mod apply;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::permissions;

/// The product's name, for new user-facing text: one place to change when
/// it is renamed.
pub const PRODUCT: &str = "jeet";

/// The setup agent's home, `$JEET_HOME/setup`.
pub fn setup_dir(home: &Path) -> PathBuf {
    home.join("setup")
}

/// Where proposals are staged, each at its target id, beside
/// [`MANIFEST`].
pub fn proposed_dir(home: &Path) -> PathBuf {
    setup_dir(home).join("proposed")
}

/// What was applied, one line per file.
pub fn applied_log(home: &Path) -> PathBuf {
    setup_dir(home).join("applied.log")
}

/// Where apply copies a file before writing over it, one directory per run.
pub fn backups_root(home: &Path) -> PathBuf {
    home.join("backups")
}

/// The manifest in [`proposed_dir`]: each staged target with why, and its
/// hash as it was when staged.
pub const MANIFEST: &str = "manifest.toml";

/// One staged proposal, keyed in the manifest by its target id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The file it replaces, as the agent saw it; apply writes only where
    /// the target id says, and refuses an entry whose path disagrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// One line on why.
    #[serde(default)]
    pub reason: String,
    /// [`hash`] of the target when staged; absent when it did not exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

pub type Manifest = BTreeMap<String, Entry>;

/// The staged manifest; empty when nothing is staged.
pub fn read_manifest(home: &Path) -> Result<Manifest> {
    let path = proposed_dir(home).join(MANIFEST);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Write the manifest whole or not at all; an empty one is removed.
pub fn write_manifest(home: &Path, manifest: &Manifest) -> Result<()> {
    let path = proposed_dir(home).join(MANIFEST);
    if manifest.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
        };
    }
    let text = toml::to_string_pretty(manifest).context("serialize the manifest")?;
    write_atomic(&path, text.as_bytes())
}

/// What a file holds, as the manifest records it: 64-bit FNV-1a in hex,
/// enough to notice that it changed after it was staged.
pub fn hash(bytes: &[u8]) -> String {
    format!("{:016x}", crate::projects::fnv1a(bytes))
}

/// The target's hash now, `None` when there is no such file.
pub fn current_hash(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(hash(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The real path of target `id`, for the targets apply may write: the
/// config, the assistant's `AGENTS.md` and skills, and template overrides
/// under `templates/{root,manager,worker}/`. Anything else is refused.
pub fn target_path(home: &Path, id: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = id.split('/').collect();
    let plain = |p: &&str| {
        !p.is_empty() && *p != "." && *p != ".." && !p.contains('\\') && !p.contains('\0')
    };
    if !parts.iter().all(plain) {
        bail!("{id:?} is not a plain relative path");
    }
    let allowed = match parts.as_slice() {
        ["config.toml"] => true,
        ["assistant", "AGENTS.md"] => true,
        ["assistant", ".claude", "skills", _, "SKILL.md"] => true,
        ["templates", "root" | "manager" | "worker", rest @ ..] => !rest.is_empty(),
        _ => false,
    };
    if !allowed {
        bail!(
            "{id:?} is not a target {PRODUCT} setup applies: config.toml, assistant/AGENTS.md, \
             assistant/.claude/skills/<name>/SKILL.md, templates/{{root,manager,worker}}/…"
        );
    }
    Ok(home.join(id))
}

/// Check a staged `config.toml` as jeet would read it: its deserializer,
/// then the `[managers]` and `[review]` policies and the capabilities in
/// `[assistant] allow`, which are otherwise checked only where used.
pub fn check_config(text: &str) -> Result<()> {
    let config: Config = toml::from_str(text).context("parse")?;
    config.manager_policy()?;
    config.review.policy()?;
    for name in &config.assistant.allow {
        if let Err(e) = permissions::parse_capability(name) {
            bail!("[assistant] allow: {e}");
        }
    }
    Ok(())
}

/// Whether the manifest's `path` names `target`: as written, with `~/`
/// expanded, or through symlinks in its directory.
fn same_path(written: &str, target: &Path) -> bool {
    let written = config::expand_path(written);
    if written == target {
        return true;
    }
    let resolved = |p: &Path| -> Option<PathBuf> {
        let dir = p.parent()?.canonicalize().ok()?;
        Some(dir.join(p.file_name()?))
    };
    matches!((resolved(&written), resolved(target)), (Some(a), Some(b)) if a == b)
}

/// Write `path` whole or not at all: a temp file beside it, then a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a path with no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("a path with no file name")?;
    let tmp = dir.join(format!(".{}.setup-tmp", name.to_string_lossy()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_allowlisted_targets_have_a_path() {
        let home = Path::new("/h");
        for ok in [
            "config.toml",
            "assistant/AGENTS.md",
            "assistant/.claude/skills/jeet-assistant/SKILL.md",
            "templates/root/AGENTS.md",
            "templates/worker/SKILL.md",
            "templates/manager/.claude/skills/x/SKILL.md",
        ] {
            assert_eq!(target_path(home, ok).unwrap(), home.join(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../config.toml",
            "assistant/../config.toml",
            "assistant/./AGENTS.md",
            "assistant//AGENTS.md",
            "assistant/notes/worklog.md",
            "assistant/.claude/settings.json",
            "assistant/.claude/skills/x/run.sh",
            "templates/root",
            "templates/other/AGENTS.md",
            "tasks/x/AGENTS.md",
            "manifest.toml",
            "setup/AGENTS.md",
        ] {
            assert!(target_path(home, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn checks_a_config_as_jeet_reads_it() {
        assert!(check_config("scan_roots = [\"~/src\"]\n").is_ok());
        assert!(check_config("scan_roots = 3\n").is_err());
        assert!(check_config("[managers]\nmax_depth = 0\n").is_err());
        assert!(check_config("[review]\nturns = 0\n").is_err());
        let err = check_config("[assistant]\nallow = [\"task.creat\"]\n").unwrap_err();
        assert!(format!("{err:#}").contains("task.creat"), "{err:#}");
        assert!(check_config("[assistant]\nallow = [\"task.create\"]\n").is_ok());
    }

    #[test]
    fn the_manifest_round_trips_and_an_empty_one_goes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(read_manifest(home).unwrap().is_empty());
        let mut manifest = Manifest::new();
        manifest.insert(
            "assistant/AGENTS.md".into(),
            Entry {
                path: Some("~/.jeet/assistant/AGENTS.md".into()),
                reason: "shorter".into(),
                hash: Some(hash(b"old")),
            },
        );
        write_manifest(home, &manifest).unwrap();
        assert_eq!(read_manifest(home).unwrap(), manifest);
        write_manifest(home, &Manifest::new()).unwrap();
        assert!(!proposed_dir(home).join(MANIFEST).exists());
    }

    #[test]
    fn a_path_matches_through_tilde_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        assert!(same_path(&target.to_string_lossy(), &target));
        assert!(!same_path("/elsewhere/config.toml", &target));
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            assert!(same_path(
                &link.join("config.toml").to_string_lossy(),
                &target
            ));
        }
    }
}
