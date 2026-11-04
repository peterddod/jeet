use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::{self, Config};
use crate::db::Database;

pub struct App {
    /// The index home: the index, config, logs and the assistant live here.
    pub home: PathBuf,
    /// Where new item directories (tasks, reviews, projects, clones) are made
    /// when that is not `home`; see [`config::default_homes`].
    pub new_home: Option<PathBuf>,
    pub config: Config,
    pub db: Database,
}

impl App {
    pub fn open() -> Result<Self> {
        let (home, new_home) = config::jeet_homes()?;
        let config = config::load_or_create(&home)?;
        // `git worktree list` always reports symlink-resolved paths, so the
        // roots we compare them against have to be resolved too — on macOS
        // `/var` is a symlink to `/private/var`, which alone is enough to make
        // jeet stop recognising its own worktrees. Resolve the home once, up
        // front, so every derived root is stable whether or not it exists yet.
        let home = home.canonicalize().unwrap_or(home);
        let db = Database::open(&config::index_path(&home))?;
        Ok(Self {
            home,
            new_home,
            config,
            db,
        })
    }

    /// The directory of the item `name` under the root `root` picks (say
    /// [`config::tasks_root`]): the one in `home` if it exists, else the one
    /// in `new_home` if there is one, which is where a new item goes.
    pub fn item_dir(&self, root: fn(&Path) -> PathBuf, name: &str) -> PathBuf {
        let old = root(&self.home).join(name);
        match &self.new_home {
            Some(new_home) if !old.exists() => root(new_home).join(name),
            _ => old,
        }
    }

    /// The roots `root` picks in every home, `home`'s first.
    pub fn roots(&self, root: fn(&Path) -> PathBuf) -> Vec<PathBuf> {
        let mut roots = vec![root(&self.home)];
        roots.extend(self.new_home.as_deref().map(root));
        roots
    }

    /// Whether `path` is under the store in any home.
    pub fn in_store(&self, path: &Path) -> bool {
        self.roots(config::store_root)
            .iter()
            .any(|root| crate::store::in_store(root, path))
    }

    /// Where a new item directory is made: the root in `new_home` if there is
    /// one, else `home`'s.
    pub fn new_root(&self, root: fn(&Path) -> PathBuf) -> PathBuf {
        root(self.new_home.as_ref().unwrap_or(&self.home))
    }

    #[cfg(test)]
    pub fn tasks_root(&self) -> PathBuf {
        config::tasks_root(&self.home)
    }

    /// Legacy: see [`config::worktrees_root`].
    pub fn worktrees_root(&self) -> PathBuf {
        config::worktrees_root(&self.home)
    }

    /// Legacy: see [`config::ephemeral_root`].
    pub fn ephemeral_root(&self) -> PathBuf {
        config::ephemeral_root(&self.home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(home: &Path, new_home: Option<&Path>) -> App {
        App {
            config: config::load_or_create(home).unwrap(),
            db: Database::open(&config::index_path(home)).unwrap(),
            new_home: new_home.map(Path::to_path_buf),
            home: home.to_path_buf(),
        }
    }

    #[test]
    fn an_item_dir_is_where_it_exists_else_where_new_ones_go() {
        let (old, new) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tasks = |home: &Path| config::tasks_root(home).join("t");

        // One home: always there.
        let single = app(old.path(), None);
        assert_eq!(single.item_dir(config::tasks_root, "t"), tasks(old.path()));
        assert_eq!(
            single.new_root(config::tasks_root),
            config::tasks_root(old.path())
        );
        assert_eq!(
            single.roots(config::tasks_root),
            [config::tasks_root(old.path())]
        );

        // Two: a new item goes to the new home...
        let split = app(old.path(), Some(new.path()));
        assert_eq!(split.item_dir(config::tasks_root, "t"), tasks(new.path()));
        assert_eq!(
            split.new_root(config::tasks_root),
            config::tasks_root(new.path())
        );
        assert_eq!(
            split.roots(config::tasks_root),
            [
                config::tasks_root(old.path()),
                config::tasks_root(new.path())
            ]
        );
        // ...an existing one stays where it is, whichever home has it...
        std::fs::create_dir_all(tasks(old.path())).unwrap();
        assert_eq!(split.item_dir(config::tasks_root, "t"), tasks(old.path()));
        std::fs::remove_dir(tasks(old.path())).unwrap();
        std::fs::create_dir_all(tasks(new.path())).unwrap();
        assert_eq!(split.item_dir(config::tasks_root, "t"), tasks(new.path()));
        // ...and the old home wins when both have it.
        std::fs::create_dir_all(tasks(old.path())).unwrap();
        assert_eq!(split.item_dir(config::tasks_root, "t"), tasks(old.path()));

        // A nested name, as a clone's trunk is.
        let trunk = "github.com/acme/api";
        assert_eq!(
            split.item_dir(config::store_root, trunk),
            config::store_root(new.path()).join(trunk)
        );
        assert!(!split.in_store(&tasks(new.path())));
        assert!(split.in_store(&config::store_root(new.path()).join(trunk)));
        assert!(split.in_store(&config::store_root(old.path()).join(trunk)));
    }
}
