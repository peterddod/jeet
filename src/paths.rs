use std::path::{Path, PathBuf};

use crate::remote::RepoIdentity;

pub fn trunk_path(store_root: &Path, id: &RepoIdentity) -> PathBuf {
    store_root.join(&id.host).join(&id.owner).join(&id.repo)
}

#[cfg(test)]
mod tests {
    use super::*;

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
