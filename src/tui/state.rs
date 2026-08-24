//! Explorer state: directory listing, cursor movement and overlay bookkeeping.
//!
//! Everything in here is pure enough to unit test — the terminal only ever
//! renders what these types describe.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ratatui::widgets::ListState;

use crate::agent::{AgentSession, AgentSpec};
use crate::db::RepoRecord;
use crate::worktrees::{WorktreeEntry, WorktreeStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

impl FsEntry {
    pub fn display_name(&self) -> String {
        let suffix = if self.is_dir { "/" } else { "" };
        let link = if self.is_symlink { "@" } else { "" };
        format!("{}{suffix}{link}", self.name)
    }
}

/// One row of the worktree overlay, with its computed counters.
#[derive(Debug, Clone)]
pub struct WorktreeRow {
    pub entry: WorktreeEntry,
    pub status: WorktreeStatus,
    pub current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    RemoveWorktree,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    /// Selection into [`Explorer::worktree_rows`].
    Worktrees {
        selected: usize,
    },
    Sessions {
        sessions: Vec<AgentSession>,
        selected: usize,
    },
    NewWorktree {
        input: String,
    },
    /// Rename the worktree at this index into [`Explorer::worktree_rows`].
    RenameWorktree {
        index: usize,
        input: String,
    },
    Confirm {
        title: String,
        lines: Vec<String>,
        action: PendingAction,
        /// Index into [`Explorer::worktree_rows`] the action applies to.
        index: usize,
    },
    Help,
    Message {
        title: String,
        lines: Vec<String>,
        /// Return to the worktree panel when dismissed, rather than the browser.
        from_panel: bool,
    },
}

/// How the explorer finished, which decides where the shell ends up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Leave the shell where it was.
    Stay,
    /// Ask the shell wrapper to cd here.
    ChangeDir(PathBuf),
}

pub struct Explorer {
    pub repo: RepoRecord,
    /// Root of the worktree being browsed.
    pub root: PathBuf,
    pub root_label: String,
    pub root_kind: String,
    pub root_status: WorktreeStatus,
    /// Directory currently listed.
    pub cwd: PathBuf,
    pub entries: Vec<FsEntry>,
    pub selected: usize,
    pub show_hidden: bool,
    pub overlay: Option<Overlay>,
    /// Worktrees of this repo, refreshed whenever the overlay is opened.
    pub worktree_rows: Vec<WorktreeRow>,
    pub status_line: String,
    /// Set while a slow operation runs in the background, so the UI can say so.
    pub working: Option<String>,
    /// Scroll position of the file list, kept here so a redraw mid-operation
    /// does not jump the view back to the top.
    pub list: ListState,
    pub agent: AgentSpec,
    pub should_quit: bool,
    pub exit: Exit,
    /// Where the explorer started, so quitting in place is a no-op.
    pub origin: PathBuf,
}

impl Explorer {
    pub fn new(
        repo: RepoRecord,
        root: PathBuf,
        root_label: String,
        root_kind: String,
        root_status: WorktreeStatus,
        cwd: PathBuf,
        agent: AgentSpec,
    ) -> Result<Self> {
        let mut explorer = Self {
            repo,
            root,
            root_label,
            root_kind,
            root_status,
            origin: cwd.clone(),
            cwd,
            entries: Vec::new(),
            selected: 0,
            show_hidden: false,
            overlay: None,
            worktree_rows: Vec::new(),
            status_line: String::new(),
            working: None,
            list: ListState::default(),
            agent,
            should_quit: false,
            exit: Exit::Stay,
        };
        explorer.reload(None)?;
        Ok(explorer)
    }

    /// Re-read the current directory, optionally keeping the cursor on `keep`.
    pub fn reload(&mut self, keep: Option<&Path>) -> Result<()> {
        let cwd = self.cwd.clone();
        self.show(cwd, keep)
    }

    /// List `dir` and move there, leaving state untouched if it cannot be read.
    ///
    /// Committing the path before the listing succeeds is how you end up with a
    /// header describing one directory and a file list showing another — which
    /// then opens the wrong file.
    pub fn show(&mut self, dir: PathBuf, keep: Option<&Path>) -> Result<()> {
        let entries = read_dir(&dir, self.show_hidden)?;
        self.selected = match keep {
            Some(path) => entries.iter().position(|e| e.path == path).unwrap_or(0),
            None => 0,
        };
        self.cwd = dir;
        self.entries = entries;
        Ok(())
    }

    pub fn selected_entry(&self) -> Option<&FsEntry> {
        self.entries.get(self.selected)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.entries.len() as isize;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len - 1) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.entries.len().saturating_sub(1);
    }

    /// Descend into the highlighted directory. Returns false when it is a file.
    pub fn descend(&mut self) -> Result<bool> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(false);
        };
        if !entry.is_dir {
            return Ok(false);
        }
        self.show(entry.path, None)?;
        Ok(true)
    }

    /// Go to the parent directory, never above the worktree root.
    ///
    /// The comparison is deliberately lexical: a symlink that resolves back to
    /// the root (`ln -s . loop`) is a directory you can descend into, and
    /// canonicalising here would refuse to let you back out of it.
    pub fn ascend(&mut self) -> Result<bool> {
        if self.cwd == self.root {
            self.status_line = "at the worktree root".to_string();
            return Ok(false);
        }
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };
        let previous = self.cwd.clone();
        self.show(parent, Some(&previous))?;
        Ok(true)
    }

    /// Path shown in the header, relative to the worktree root.
    pub fn breadcrumb(&self) -> String {
        match self.cwd.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => "/".to_string(),
            Ok(rest) => format!("/{}", rest.display()),
            Err(_) => self.cwd.display().to_string(),
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_line = msg.into();
    }

    pub fn quit_here(&mut self) {
        self.exit = if crate::resolve::same_path(&self.cwd, &self.origin) {
            Exit::Stay
        } else {
            Exit::ChangeDir(self.landing_dir())
        };
        self.should_quit = true;
    }

    pub fn quit_in_place(&mut self) {
        // A rename can move the directory the shell is sitting in out from
        // under it; leaving it somewhere that no longer exists helps nobody.
        self.exit = if self.origin.is_dir() {
            Exit::Stay
        } else {
            Exit::ChangeDir(self.landing_dir())
        };
        self.should_quit = true;
    }

    /// Somewhere that still exists to leave the shell in.
    fn landing_dir(&self) -> PathBuf {
        for candidate in [&self.cwd, &self.root] {
            if candidate.is_dir() {
                return candidate.clone();
            }
        }
        PathBuf::from(&self.repo.trunk_path)
    }
}

/// Directories first, then files, both case-insensitive by name.
pub fn read_dir(dir: &Path, show_hidden: bool) -> Result<Vec<FsEntry>> {
    let mut entries = Vec::new();
    let iter = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(e) => anyhow::bail!("cannot read {}: {e}", dir.display()),
    };
    for entry in iter.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let metadata = entry.metadata().ok();
        let file_type = entry.file_type().ok();
        let is_symlink = file_type.map(|t| t.is_symlink()).unwrap_or(false);
        let is_dir = entry.path().is_dir();
        entries.push(FsEntry {
            name,
            path: entry.path(),
            is_dir,
            is_symlink,
            size: metadata.map(|m| m.len()).unwrap_or(0),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "x").unwrap();
        dir
    }

    #[test]
    fn lists_dirs_first_and_hides_dotfiles() {
        let dir = fixture();
        let entries = read_dir(dir.path(), false).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "alpha.txt", "README.md"]);
    }

    #[test]
    fn shows_dotfiles_when_asked() {
        let dir = fixture();
        let entries = read_dir(dir.path(), true).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec![".hidden", "src", "alpha.txt", "README.md"]);
    }

    #[test]
    fn human_sizes_are_compact() {
        assert_eq!(human_size(12), "12B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    fn explorer_at(root: &Path) -> Explorer {
        let repo = RepoRecord {
            id: "github.com/acme/widget".into(),
            trunk_path: root.to_string_lossy().to_string(),
            remote_url: "https://github.com/acme/widget.git".into(),
            default_branch: "main".into(),
            managed: false,
        };
        let config = crate::config::Config {
            agent: Some("claude".into()),
            ..crate::config::Config::default()
        };
        Explorer::new(
            repo,
            root.to_path_buf(),
            "main".into(),
            "trunk".into(),
            WorktreeStatus::default(),
            root.to_path_buf(),
            AgentSpec::from_config(&config).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn descend_and_ascend_track_the_cursor() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        assert_eq!(explorer.breadcrumb(), "/");

        assert!(explorer.descend().unwrap());
        assert_eq!(explorer.cwd, dir.path().join("src"));
        assert_eq!(explorer.breadcrumb(), "/src");

        assert!(explorer.ascend().unwrap());
        assert_eq!(explorer.cwd, dir.path());
        assert_eq!(explorer.selected_entry().unwrap().name, "src");
    }

    #[test]
    fn ascend_stops_at_the_root() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        assert!(!explorer.ascend().unwrap());
        assert_eq!(explorer.cwd, dir.path());
    }

    #[test]
    fn descend_on_a_file_is_a_noop() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.select_last();
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
        assert!(!explorer.descend().unwrap());
        assert_eq!(explorer.cwd, dir.path());
    }

    #[test]
    fn cursor_movement_clamps() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.move_cursor(-5);
        assert_eq!(explorer.selected, 0);
        explorer.move_cursor(50);
        assert_eq!(explorer.selected, explorer.entries.len() - 1);
    }

    /// After a rename the explorer follows the worktree to its new home, so
    /// `origin` is stale. Quitting must hand the shell the new location — and
    /// crucially, one that exists.
    #[test]
    fn quitting_follows_a_worktree_that_moved_underneath_it() {
        let dir = fixture();
        let old = dir.path().join("src");
        let mut explorer = explorer_at(&old);

        // The rename: the tree moved, and the explorer moved with it.
        let new = dir.path().join("renamed");
        std::fs::create_dir(&new).unwrap();
        std::fs::remove_dir_all(&old).unwrap();
        explorer.root = new.clone();
        explorer.cwd = new.clone();

        explorer.quit_in_place();
        assert_eq!(explorer.exit, Exit::ChangeDir(new.clone()));
        assert!(new.is_dir(), "handed the shell a path that does not exist");
    }

    /// If everything under the cursor has gone, fall back to somewhere real
    /// rather than asking the shell to cd into a deleted directory.
    #[test]
    fn quitting_falls_back_when_the_browsed_directory_is_gone() {
        let dir = fixture();
        let gone = dir.path().join("src");
        let mut explorer = explorer_at(&gone);
        explorer.root = dir.path().to_path_buf();
        std::fs::remove_dir_all(&gone).unwrap();

        explorer.quit_in_place();
        match &explorer.exit {
            Exit::ChangeDir(path) => {
                assert_eq!(path, dir.path());
                assert!(path.is_dir());
            }
            other => panic!("expected a fallback directory, got {other:?}"),
        }
    }

    #[test]
    fn quitting_in_place_does_not_move_the_shell() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.quit_in_place();
        assert_eq!(explorer.exit, Exit::Stay);

        let mut explorer = explorer_at(dir.path());
        explorer.quit_here();
        assert_eq!(explorer.exit, Exit::Stay);

        let mut explorer = explorer_at(dir.path());
        explorer.descend().unwrap();
        explorer.quit_here();
        assert_eq!(explorer.exit, Exit::ChangeDir(dir.path().join("src")));
    }
}
