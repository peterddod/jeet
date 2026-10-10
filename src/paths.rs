use std::path::{Path, PathBuf};

use crate::remote::RepoIdentity;

pub fn trunk_path(store_root: &Path, id: &RepoIdentity) -> PathBuf {
    store_root.join(&id.host).join(&id.owner).join(&id.repo)
}

pub fn worktree_path(worktrees_root: &Path, id: &RepoIdentity, branch: &str) -> PathBuf {
    worktrees_root
        .join(&id.host)
        .join(&id.owner)
        .join(&id.repo)
        .join(branch_slug(branch))
}

pub fn branch_slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| match c {
            '/' | '\\' | ' ' | ':' | '@' | '{' | '}' | '^' | '%' | '`' | '"' | '\'' | '<' | '>'
            | '~' | '?' | '*' | '[' | ']' | '|' => '-',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_replaces_slashes() {
        assert_eq!(branch_slug("feat/my-feature"), "feat-my-feature");
    }

    #[test]
    fn trunk_path_layout() {
        let id = RepoIdentity {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "widget".into(),
        };
        let p = trunk_path(Path::new("/home/.jeet/store"), &id);
        assert_eq!(p, PathBuf::from("/home/.jeet/store/github.com/acme/widget"));
    }
}
