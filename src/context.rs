use std::path::PathBuf;

use anyhow::Result;

use crate::config::{self, Config};
use crate::db::Database;

pub struct App {
    pub home: PathBuf,
    pub config: Config,
    pub db: Database,
}

impl App {
    pub fn open() -> Result<Self> {
        let home = config::jeet_home()?;
        let config = config::load_or_create(&home)?;
        // `git worktree list` always reports symlink-resolved paths, so the
        // roots we compare them against have to be resolved too — on macOS
        // `/var` is a symlink to `/private/var`, which alone is enough to make
        // jeet stop recognising its own worktrees. Resolve the home once, up
        // front, so every derived root is stable whether or not it exists yet.
        let home = home.canonicalize().unwrap_or(home);
        let db = Database::open(&config::index_path(&home))?;
        Ok(Self { home, config, db })
    }

    pub fn store_root(&self) -> PathBuf {
        config::store_root(&self.home)
    }

    pub fn worktrees_root(&self) -> PathBuf {
        config::worktrees_root(&self.home)
    }

    pub fn ephemeral_root(&self) -> PathBuf {
        config::ephemeral_root(&self.home)
    }
}
