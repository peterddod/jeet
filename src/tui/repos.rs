//! How a task row names its repository ([`Key`]); the projects tab groups by
//! jeet project instead ([`super::projects_tab`]).

/// A repository as a task row's project column names it, or the tasks with
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    NoRepo,
    /// A worktree's `repo_id`.
    Repo(String),
}

impl Key {
    /// `jeet`: the repository's own name, as worktree rows show it.
    pub fn name(&self) -> &str {
        match self {
            Key::NoRepo => "(no repo)",
            Key::Repo(id) => id.rsplit('/').next().unwrap_or(id),
        }
    }
}
