//! Explorer state: directory listing, cursor movement and overlay bookkeeping.
//!
//! Everything in here is pure enough to unit test — the terminal only ever
//! renders what these types describe.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use unicode_width::UnicodeWidthStr;

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
    Help {
        /// First line of the key list shown, for terminals too small for it.
        scroll: u16,
    },
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
    /// Everything in `cwd`, unfiltered.
    pub entries: Vec<FsEntry>,
    /// What the user has typed to narrow the listing down.
    pub filter: String,
    /// Indices into [`Explorer::entries`] that `filter` keeps, in display
    /// order. Everything the user sees and selects goes through this.
    pub matches: Vec<usize>,
    /// Cursor position within [`Explorer::matches`], not `entries`.
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
    /// Where the listing was drawn last frame, so a click can be mapped back
    /// to the row under it. Set by the renderer, read by the mouse handler.
    pub list_area: Rect,
    /// Screen cell the breadcrumb's leading `/` was drawn at, same deal, or
    /// none when the terminal was too short to draw the path line at all.
    pub breadcrumb_origin: Option<(u16, u16)>,
    /// When and where the last click landed, so the second press of a
    /// double-click can be told from a deliberate one.
    last_click: Option<(Instant, u16, u16)>,
}

/// Two presses on the same cell inside this window are one double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

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
            filter: String::new(),
            matches: Vec::new(),
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
            list_area: Rect::default(),
            breadcrumb_origin: None,
            last_click: None,
        };
        explorer.reload(None)?;
        Ok(explorer)
    }

    /// Re-read the current directory, keeping the filter and, where it can,
    /// the cursor on `keep`.
    ///
    /// This is the refresh path — after an editor or an agent has run — so
    /// unlike [`Explorer::show`] it must not throw away what the user typed.
    pub fn reload(&mut self, keep: Option<&Path>) -> Result<()> {
        let cwd = self.cwd.clone();
        // Put the filter back whether or not the listing worked: bailing out
        // with it cleared but the rows still filtered shows a subset of the
        // directory with nothing on screen to say why.
        let filter = std::mem::take(&mut self.filter);
        let listed = self.show(cwd, keep);
        self.filter = filter;
        self.refocus(keep);
        listed
    }

    /// Show or hide dotfiles, putting the flag back if the re-listing fails.
    ///
    /// A flag that disagrees with what is on screen is worse than the error:
    /// the next unrelated refresh silently changes the listing. Returns
    /// whether hidden files are now shown.
    pub fn toggle_hidden(&mut self, keep: Option<&Path>) -> Result<bool> {
        self.show_hidden = !self.show_hidden;
        if let Err(e) = self.reload(keep) {
            self.show_hidden = !self.show_hidden;
            return Err(e);
        }
        Ok(self.show_hidden)
    }

    /// Whether this press is the tail of a double-click on the same cell, and
    /// so should be swallowed rather than acted on.
    ///
    /// The window stays anchored on the press we acted on, never on the ones
    /// we discarded — otherwise a sustained series of clicks in one place,
    /// each inside the window of the last, would act exactly once however long
    /// it went on.
    pub fn is_double_click(&mut self, column: u16, row: u16) -> bool {
        let now = Instant::now();
        if self
            .last_click
            .is_some_and(|(at, c, r)| (c, r) == (column, row) && now - at < DOUBLE_CLICK)
        {
            return true;
        }
        self.last_click = Some((now, column, row));
        false
    }

    /// List `dir` and move there, leaving state untouched if it cannot be read.
    ///
    /// Committing the path before the listing succeeds is how you end up with a
    /// header describing one directory and a file list showing another — which
    /// then opens the wrong file.
    ///
    /// Moving directories clears the filter: it was typed against the level you
    /// just left, and carrying it over hides most of the one you arrived in.
    pub fn show(&mut self, dir: PathBuf, keep: Option<&Path>) -> Result<()> {
        let entries = read_dir(&dir, self.show_hidden)?;
        self.cwd = dir;
        self.entries = entries;
        self.filter.clear();
        self.refocus(keep);
        Ok(())
    }

    /// Recompute the visible rows for the current filter, keeping the cursor on
    /// `keep` when that entry survived and dropping it to the top otherwise.
    fn refocus(&mut self, keep: Option<&Path>) {
        self.matches = filter_matches(&self.entries, &self.filter);
        self.selected = keep
            .and_then(|path| {
                self.matches
                    .iter()
                    .position(|&i| self.entries[i].path == path)
            })
            .unwrap_or(0);
    }

    /// The rows on screen, in the order they are drawn.
    pub fn visible(&self) -> impl Iterator<Item = &FsEntry> {
        self.matches.iter().filter_map(|&i| self.entries.get(i))
    }

    pub fn visible_len(&self) -> usize {
        self.matches.len()
    }

    /// Replace the filter. The cursor goes back to the top match, so the row
    /// ⇥ would complete is always the highlighted one.
    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.refocus(None);
    }

    pub fn push_filter(&mut self, c: char) {
        let mut filter = std::mem::take(&mut self.filter);
        filter.push(c);
        self.set_filter(filter);
    }

    /// Delete the last character. Returns false when there was nothing to
    /// delete, so the caller can say so rather than looking inert.
    ///
    /// Nothing to delete means nothing changes at all: going through
    /// `set_filter` would send the cursor back to the top row, which is a
    /// visible edit in answer to a key that did nothing.
    pub fn pop_filter(&mut self) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        let mut filter = std::mem::take(&mut self.filter);
        filter.pop();
        self.set_filter(filter);
        true
    }

    /// Empty the filter, returning false when it was empty already — and, as
    /// with [`Explorer::pop_filter`], leaving the cursor alone in that case.
    pub fn clear_filter(&mut self) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        self.set_filter(String::new());
        true
    }

    /// ⇥: extend the filter as far as the matches agree, the way a shell does.
    ///
    /// Works towards the highlighted row, not blindly towards the first: the
    /// cursor stays where it was, and it is the highlighted name that gets
    /// taken outright once the matches stop agreeing. Otherwise arrowing down
    /// and pressing ⇥ would rewrite the filter around a different entry and
    /// pull the cursor onto it, and ⏎ would open something else again.
    ///
    /// Returns false when there is nothing left to add.
    pub fn complete(&mut self) -> bool {
        let names: Vec<&str> = self.visible().map(|e| e.name.as_str()).collect();
        let Some(completed) = completion(&names, self.selected, &self.filter) else {
            return false;
        };
        let keep = self.selected_entry().map(|e| e.path.clone());
        self.filter = completed;
        self.refocus(keep.as_deref());
        true
    }

    pub fn selected_entry(&self) -> Option<&FsEntry> {
        self.matches
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.matches.len() as isize;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len - 1) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
    }

    /// Put the cursor on a visible row, ignoring one that is not there.
    pub fn select_visible(&mut self, index: usize) -> bool {
        if index >= self.matches.len() {
            return false;
        }
        self.selected = index;
        true
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

    /// `/`: step into the folder the filter spells out, as you would while
    /// typing a path. Returns the name entered, or none when the filter does
    /// not settle on exactly one folder.
    pub fn descend_typed(&mut self) -> Result<Option<String>> {
        let Some(index) = self.typed_dir() else {
            return Ok(None);
        };
        let entry = self.entries[index].clone();
        self.show(entry.path, None)?;
        Ok(Some(entry.name))
    }

    /// The folder `/` would enter: the one the filter names outright, or the
    /// only thing it matches at all.
    fn typed_dir(&self) -> Option<usize> {
        if self.filter.is_empty() {
            return None;
        }
        let needle = self.filter.to_lowercase();
        let named = self
            .matches
            .iter()
            .copied()
            .find(|&i| self.entries[i].name.to_lowercase() == needle);
        let index = match named {
            Some(index) => index,
            None => match self.matches.as_slice() {
                [only] => *only,
                _ => return None,
            },
        };
        self.entries[index].is_dir.then_some(index)
    }

    /// Directory a click `offset` columns into the breadcrumb points at.
    ///
    /// The separator after a segment belongs to that segment, so clicking
    /// anywhere in `/src/` lands in `src`. Offsets are screen columns, so the
    /// walk measures display width — a directory named in CJK is two columns
    /// per character and a click past it must not land short.
    pub fn breadcrumb_target(&self, offset: usize) -> Option<PathBuf> {
        // Outside the worktree the breadcrumb is an absolute path whose
        // segments are not ours to walk back up.
        let rest = self.cwd.strip_prefix(&self.root).ok()?;
        if offset >= self.breadcrumb().width() {
            return None;
        }
        // The path is rebuilt from `cwd`'s own components, never from the
        // rendered string: `display()` is lossy, so a directory whose name is
        // not valid UTF-8 would come back full of U+FFFD and fail to open.
        // The rendered text only ever decides how wide each segment looks.
        let mut dir = self.root.clone();
        let mut cursor = 0usize;
        for component in rest.components() {
            if offset <= cursor {
                return Some(dir);
            }
            cursor += "/".width() + component.as_os_str().to_string_lossy().width();
            dir.push(component.as_os_str());
        }
        Some(dir)
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

/// Indices of the entries `filter` keeps, case-insensitively.
///
/// A name that starts with the filter sorts ahead of one that merely contains
/// it, so typing `src` puts `src/` above `mysrc/` no matter how the directory
/// itself sorts. Within each group the listing order is preserved.
pub fn filter_matches(entries: &[FsEntry], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..entries.len()).collect();
    }
    let needle = filter.to_lowercase();
    let mut prefixed = Vec::new();
    let mut contained = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.name.to_lowercase();
        if name.starts_with(&needle) {
            prefixed.push(index);
        } else if name.contains(&needle) {
            contained.push(index);
        }
    }
    prefixed.append(&mut contained);
    prefixed
}

/// What ⇥ should leave in the filter box, given the names on screen and which
/// of them is highlighted.
///
/// Like a shell: fill in as far as every candidate agrees, and once they stop
/// agreeing take the highlighted one outright rather than sitting there doing
/// nothing.
pub fn completion(names: &[&str], chosen: usize, filter: &str) -> Option<String> {
    let pick = *names.get(chosen).or_else(|| names.first())?;
    // Only names that start with the filter can extend it — and only while the
    // highlighted one is among them. Highlight a name the filter matches in
    // the middle and that shared prefix belongs to rows the user has arrowed
    // past; extending to it would drag the cursor onto one of them.
    if starts_with_ignore_case(pick, filter) {
        let agreed: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| starts_with_ignore_case(n, filter))
            .collect();
        let shared = common_prefix(&agreed);
        if shared.chars().count() > filter.chars().count() {
            return Some(shared);
        }
    }
    (pick != filter).then(|| pick.to_string())
}

fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Longest prefix every name shares, compared without case but returned with
/// the casing of the first name — so ⇥ types what is actually on disk.
///
/// Case folding is Unicode, matching what the filter itself does; ASCII-only
/// folding would call two names that differ only in an accented letter's case
/// wholly different and complete straight to the top one.
fn common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for name in &names[1..] {
        let shared = first
            .chars()
            .zip(name.chars())
            .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            .count();
        len = len.min(shared);
    }
    first.chars().take(len).collect()
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
    fn filtering_narrows_the_listing_case_insensitively() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("re".into());
        let names: Vec<_> = explorer.visible().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["README.md"]);

        explorer.set_filter("S".into());
        let names: Vec<_> = explorer.visible().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src"]);

        assert!(explorer.clear_filter());
        assert_eq!(explorer.visible_len(), 3);
    }

    /// A name that starts with what was typed beats one that merely contains
    /// it, even when the directory sort would put it second.
    #[test]
    fn prefix_matches_come_first() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("mysrc")).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("src".into());
        let names: Vec<_> = explorer.visible().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "mysrc"]);
    }

    /// The cursor and everything reached through it follow the filtered rows,
    /// not the underlying directory — otherwise ⏎ opens the wrong file.
    #[test]
    fn the_cursor_indexes_the_filtered_rows() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.set_filter("a".into());
        assert_eq!(explorer.visible_len(), 2); // alpha.txt, README.md
        assert_eq!(explorer.selected_entry().unwrap().name, "alpha.txt");

        explorer.move_cursor(1);
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
        explorer.move_cursor(5);
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
    }

    #[test]
    fn tab_fills_in_as_far_as_the_matches_agree() {
        // One match: complete it outright.
        assert_eq!(completion(&["src"], 0, "s"), Some("src".into()));
        // Several: stop where they diverge.
        assert_eq!(completion(&["source", "sound"], 0, "s"), Some("sou".into()));
        // Already at the shared prefix: take the highlighted one rather than
        // sit idle — and it is the highlighted one, not always the first.
        assert_eq!(
            completion(&["source", "sound"], 0, "sou"),
            Some("source".into())
        );
        assert_eq!(
            completion(&["source", "sound"], 1, "sou"),
            Some("sound".into())
        );
        // Nothing left to add.
        assert_eq!(completion(&["src"], 0, "src"), None);
        assert_eq!(completion(&[], 0, "src"), None);
        // Completion types the casing that is actually on disk.
        assert_eq!(
            completion(&["README.md"], 0, "re"),
            Some("README.md".into())
        );
    }

    /// A substring match must never shorten what the user typed.
    #[test]
    fn tab_never_takes_characters_away() {
        assert_eq!(
            completion(&["README.md", "html"], 0, "m"),
            Some("README.md".into())
        );
    }

    #[test]
    fn tab_completes_against_the_visible_rows() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.set_filter("s".into());
        assert!(explorer.complete());
        assert_eq!(explorer.filter, "src");
        assert!(!explorer.complete());
    }

    /// ⇥ works towards the row you are on. Completing around a different one
    /// and dragging the cursor there means ⏎ opens something you never chose.
    #[test]
    fn tab_completes_the_highlighted_row_and_stays_on_it() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("styles")).unwrap();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("s".into());
        explorer.move_cursor(1);
        assert_eq!(explorer.selected_entry().unwrap().name, "styles");

        // The shared prefix of both is "s", so ⇥ takes the highlighted name.
        assert!(explorer.complete());
        assert_eq!(explorer.filter, "styles");
        assert_eq!(explorer.selected_entry().unwrap().name, "styles");
        assert!(!explorer.complete());
    }

    /// Extending to a shared prefix must not pull the cursor off the row it
    /// was on, when that row is still there to be on.
    #[test]
    fn tab_keeps_the_cursor_where_it_was() {
        let dir = TempDir::new().unwrap();
        for name in ["sound", "source"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("s".into());
        explorer.move_cursor(1);
        assert_eq!(explorer.selected_entry().unwrap().name, "source");

        assert!(explorer.complete());
        assert_eq!(explorer.filter, "sou");
        assert_eq!(explorer.selected_entry().unwrap().name, "source");
    }

    /// The highlighted row can be one the filter matches in the middle. ⇥ must
    /// still work towards it, not towards the prefix matches above it.
    #[test]
    fn tab_respects_a_highlighted_substring_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mem.txt"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("me".into());
        explorer.move_cursor(1);
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");

        assert!(explorer.complete());
        assert_eq!(explorer.filter, "README.md");
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
    }

    /// A key that could do nothing must do nothing — including not moving the
    /// cursor, which is a visible edit in answer to an inert keystroke.
    #[test]
    fn backspace_and_clear_leave_an_empty_filter_alone() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.move_cursor(2);
        let was = explorer.selected;

        assert!(!explorer.pop_filter());
        assert_eq!(explorer.selected, was);
        assert!(!explorer.clear_filter());
        assert_eq!(explorer.selected, was);

        // With something typed they both do their job.
        explorer.set_filter("re".into());
        assert!(explorer.pop_filter());
        assert_eq!(explorer.filter, "r");
        assert!(explorer.clear_filter());
        assert!(explorer.filter.is_empty());
    }

    #[test]
    fn slash_enters_the_folder_the_filter_names() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("src".into());
        assert_eq!(explorer.descend_typed().unwrap().as_deref(), Some("src"));
        assert_eq!(explorer.cwd, dir.path().join("src"));
        assert!(
            explorer.filter.is_empty(),
            "the filter must reset on the way in"
        );
    }

    #[test]
    fn slash_does_nothing_without_a_single_folder_to_enter() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());

        // A file is not a folder.
        explorer.set_filter("alpha".into());
        assert_eq!(explorer.descend_typed().unwrap(), None);
        assert_eq!(explorer.cwd, dir.path());

        // Nothing typed at all.
        explorer.set_filter(String::new());
        assert_eq!(explorer.descend_typed().unwrap(), None);
        assert_eq!(explorer.cwd, dir.path());
    }

    /// Typing a folder's full name wins even when it is also a prefix of
    /// something else, so `src` enters `src` and not `src-old`.
    #[test]
    fn an_exact_name_beats_an_ambiguous_prefix() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("src-old")).unwrap();
        let mut explorer = explorer_at(dir.path());

        explorer.set_filter("src".into());
        assert_eq!(explorer.descend_typed().unwrap().as_deref(), Some("src"));
        assert_eq!(explorer.cwd, dir.path().join("src"));
    }

    #[test]
    fn moving_between_directories_resets_the_filter() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.set_filter("src".into());
        assert!(explorer.descend().unwrap());
        assert!(explorer.filter.is_empty());

        explorer.set_filter("zzz".into());
        assert!(explorer.ascend().unwrap());
        assert!(explorer.filter.is_empty());
        assert_eq!(explorer.visible_len(), 3);
    }

    /// Refreshing after an editor or an agent has run is not navigation: what
    /// the user typed, and where the cursor sat, both survive it.
    #[test]
    fn reloading_keeps_the_filter_and_the_cursor() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.set_filter("a".into());
        explorer.move_cursor(1);
        let keep = explorer.selected_entry().unwrap().path.clone();

        explorer.reload(Some(&keep)).unwrap();
        assert_eq!(explorer.filter, "a");
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
    }

    /// A double-click is two presses: the first enters the folder, and the
    /// second must not enter whatever the child listing slid under the cursor.
    #[test]
    fn the_second_press_of_a_double_click_is_swallowed() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());

        assert!(!explorer.is_double_click(4, 7));
        assert!(
            explorer.is_double_click(4, 7),
            "same cell, immediately after"
        );
        // A triple-click gets no third action either.
        assert!(explorer.is_double_click(4, 7));
        // A different cell is a deliberate click, however fast.
        assert!(!explorer.is_double_click(4, 8));

        let mut acted = 0;
        for _ in 0..4 {
            std::thread::sleep(DOUBLE_CLICK / 2);
            if !explorer.is_double_click(9, 9) {
                acted += 1;
            }
        }
        // Anchored on the acted press, every other one of these clears the
        // window. Anchored on every press — the bug — only the first ever
        // would, however long the clicking went on.
        assert!(acted >= 2, "only {acted} of 4 clicks acted");
    }

    /// If the re-listing fails the flag must go back: left flipped, it silently
    /// changes what the next unrelated refresh shows.
    #[test]
    fn a_failed_hidden_toggle_does_not_move_the_flag() {
        let dir = fixture();
        let gone = dir.path().join("src");
        let mut explorer = explorer_at(&gone);
        assert!(!explorer.show_hidden);

        std::fs::remove_dir_all(&gone).unwrap();
        assert!(explorer.toggle_hidden(None).is_err());
        assert!(!explorer.show_hidden);
    }

    #[test]
    fn toggling_hidden_files_relists() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        assert_eq!(explorer.visible_len(), 3);
        assert!(explorer.toggle_hidden(None).unwrap());
        assert_eq!(explorer.visible_len(), 4);
        assert!(!explorer.toggle_hidden(None).unwrap());
        assert_eq!(explorer.visible_len(), 3);
    }

    /// Case folding follows the filter's, which is Unicode: two names that
    /// differ only in an accented letter's case still share a prefix.
    #[test]
    fn completion_folds_case_beyond_ascii() {
        assert_eq!(
            completion(&["Éclair", "éclipse"], 0, "é"),
            Some("Écl".into())
        );
    }

    /// A refresh that fails must not leave the filter box empty while the
    /// listing is still filtered — the rows would lie about what is on screen.
    #[test]
    fn a_failed_reload_keeps_the_filter_it_was_showing() {
        let dir = fixture();
        let gone = dir.path().join("src");
        let mut explorer = explorer_at(&gone);
        std::fs::create_dir(gone.join("keep")).unwrap();
        explorer.reload(None).unwrap();
        explorer.set_filter("keep".into());

        std::fs::remove_dir_all(&gone).unwrap();
        assert!(explorer.reload(None).is_err());
        assert_eq!(explorer.filter, "keep");
        assert_eq!(explorer.visible_len(), 1);
    }

    /// Outside the worktree the breadcrumb is a bare absolute path, and its
    /// segments are not ancestors we may walk back up to.
    #[test]
    fn a_breadcrumb_outside_the_root_is_not_clickable() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.root = dir.path().join("src");
        assert_eq!(explorer.breadcrumb(), dir.path().display().to_string());
        assert_eq!(explorer.breadcrumb_target(1), None);
    }

    /// Clicks arrive as screen columns, and a wide character occupies two of
    /// them — measuring in `char`s would land a click short of its segment.
    #[test]
    fn crumb_offsets_are_screen_columns_not_characters() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("日本").join("tui");
        std::fs::create_dir_all(&nested).unwrap();
        let mut explorer = explorer_at(dir.path());
        explorer.show(nested.clone(), None).unwrap();
        assert_eq!(explorer.breadcrumb(), "/日本/tui");

        // "/" + 4 columns of 日本 + "/" = column 5 is the first of "tui".
        assert_eq!(
            explorer.breadcrumb_target(0),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(explorer.breadcrumb_target(4), Some(dir.path().join("日本")));
        assert_eq!(explorer.breadcrumb_target(5), Some(dir.path().join("日本")));
        assert_eq!(explorer.breadcrumb_target(6), Some(nested));
        assert_eq!(explorer.breadcrumb_target(9), None);
    }

    #[test]
    fn clicking_a_path_crumb_picks_the_segment_under_it() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path());
        explorer.show(dir.path().join("src"), None).unwrap();
        assert_eq!(explorer.breadcrumb(), "/src");

        // The leading slash is the root itself.
        assert_eq!(
            explorer.breadcrumb_target(0),
            Some(dir.path().to_path_buf())
        );
        for offset in 1..4 {
            assert_eq!(
                explorer.breadcrumb_target(offset),
                Some(dir.path().join("src")),
                "offset {offset}"
            );
        }
        // Past the end of the path there is nothing to click.
        assert_eq!(explorer.breadcrumb_target(4), None);
    }

    /// `display()` is lossy, so rebuilding a crumb's path from the rendered
    /// text would hand `show` a name full of U+FFFD that does not exist.
    #[test]
    #[cfg(unix)]
    fn crumbs_survive_a_directory_name_that_is_not_utf8() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = TempDir::new().unwrap();
        let odd = dir.path().join(OsString::from_vec(b"od\xffd".to_vec()));
        let nested = odd.join("tui");
        std::fs::create_dir_all(&nested).unwrap();
        let mut explorer = explorer_at(dir.path());
        explorer.show(nested.clone(), None).unwrap();

        // Clicking the replacement-charactered crumb still opens the real one.
        let target = explorer.breadcrumb_target(2).unwrap();
        assert_eq!(target, odd);
        assert!(
            explorer.show(target, None).is_ok(),
            "rebuilt a path that does not exist"
        );
    }

    #[test]
    fn clicking_a_nested_crumb_climbs_to_that_level() {
        let dir = fixture();
        let nested = dir.path().join("src").join("tui");
        std::fs::create_dir_all(&nested).unwrap();
        let mut explorer = explorer_at(dir.path());
        explorer.show(nested.clone(), None).unwrap();
        assert_eq!(explorer.breadcrumb(), "/src/tui");

        assert_eq!(
            explorer.breadcrumb_target(0),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(explorer.breadcrumb_target(2), Some(dir.path().join("src")));
        // The separator belongs to the segment it follows.
        assert_eq!(explorer.breadcrumb_target(4), Some(dir.path().join("src")));
        assert_eq!(explorer.breadcrumb_target(6), Some(nested));
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
