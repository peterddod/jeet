use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::remote::RepoIdentity;

/// `exe`, or the `plete` beside it when `exe` is the old `jeet` (or the
/// `pt` alias) and that exists: what gets baked into hooks and jobs is the
/// new name, however it was called.
pub fn prefer_plete(exe: PathBuf) -> PathBuf {
    let alias = matches!(
        exe.file_name().and_then(|n| n.to_str()),
        Some("jeet") | Some("pt")
    );
    if !alias {
        return exe;
    }
    let sibling = exe.with_file_name("plete");
    if sibling.exists() {
        return sibling;
    }
    linked_plete(&exe).unwrap_or(exe)
}

/// What `link` points to, through up to a few links, if it ends up at a
/// file called `plete`: `pt` in `~/.local/bin` is a link to one.
fn linked_plete(link: &Path) -> Option<PathBuf> {
    let mut current = link.to_path_buf();
    for _ in 0..8 {
        let target = std::fs::read_link(&current).ok()?;
        current = current.parent()?.join(target);
        if current.file_name().is_some_and(|n| n == "plete") {
            return Some(current);
        }
    }
    None
}

/// The running binary as it is called, as `plete` where that is beside it:
/// not canonicalized, so a package manager's link outlives an upgrade.
pub fn plete_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the plete binary")?;
    Ok(prefer_plete(exe))
}

pub fn trunk_path(store_root: &Path, id: &RepoIdentity) -> PathBuf {
    store_root.join(&id.host).join(&id.owner).join(&id.repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_name_is_swapped_for_the_plete_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let (jeet, pt, plete) = (
            dir.path().join("jeet"),
            dir.path().join("pt"),
            dir.path().join("plete"),
        );
        // No plete beside it: as it is.
        assert_eq!(prefer_plete(jeet.clone()), jeet);
        std::fs::write(&plete, "").unwrap();
        assert_eq!(prefer_plete(jeet), plete);
        assert_eq!(prefer_plete(pt), plete);
        assert_eq!(prefer_plete(plete.clone()), plete);
        let other = dir.path().join("plete-dev");
        assert_eq!(prefer_plete(other.clone()), other);
    }

    #[test]
    fn a_pt_link_is_followed_to_the_plete_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, home) = (dir.path().join("opt"), dir.path().join("local"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let plete = bin.join("plete");
        std::fs::write(&plete, "").unwrap();
        // pt -> mid -> plete, each link relative or absolute.
        std::os::unix::fs::symlink(&plete, home.join("mid")).unwrap();
        std::os::unix::fs::symlink("mid", home.join("pt")).unwrap();
        assert_eq!(prefer_plete(home.join("pt")), plete);
        // A link to something else is left as it is.
        std::os::unix::fs::symlink("/bin/ls", home.join("jeet")).unwrap();
        assert_eq!(prefer_plete(home.join("jeet")), home.join("jeet"));
    }

    #[test]
    fn trunk_path_layout() {
        let id = RepoIdentity {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "widget".into(),
        };
        let p = trunk_path(Path::new("/home/.plete/store"), &id);
        assert_eq!(
            p,
            PathBuf::from("/home/.plete/store/github.com/acme/widget")
        );
    }
}
