//! The keys that work right now, as the explorer's bottom line and the help
//! window's first section show them.
//!
//! [`explorer_hints`] asks the same questions `handle_browse_key` does — is
//! there a repository, is a folder under the cursor, is a command being typed
//! — so a key shown is a key that acts, and one that acts is shown. Anything
//! added to that handler wants a line here.

use unicode_width::UnicodeWidthStr;

use super::state::Explorer;
use super::ui::truncate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    pub key: &'static str,
    pub label: &'static str,
}

const fn hint(key: &'static str, label: &'static str) -> Hint {
    Hint { key, label }
}

impl Hint {
    fn text(&self) -> String {
        format!("{} {}", self.key, self.label)
    }
}

/// Every key that does something in the browse state, most useful first. The
/// help key is not among them: [`hint_line`] always ends on it.
pub fn explorer_hints(explorer: &Explorer) -> Vec<Hint> {
    let mut hints = Vec::new();
    let filtering = !explorer.filter.is_empty();

    if explorer.in_command_mode() {
        // `→` and `←` only say so, and `/` is part of an operand: none of the
        // navigation applies until the command is run or dropped.
        hints.extend([
            hint("⏎", "run"),
            hint("esc", "drop"),
            hint("⌫", "delete"),
            hint("^u", "clear"),
            hint("↑↓", "move"),
        ]);
    } else {
        if !filtering {
            hints.push(hint("type", "to filter"));
        }
        match explorer.selected_entry() {
            Some(entry) if entry.is_dir => hints.push(hint("⏎", "open")),
            Some(_) => hints.push(hint("⏎", "edit")),
            None => {}
        }
        hints.push(hint("↑↓", "move"));
        if explorer.selected_entry().is_some_and(|e| e.is_dir) {
            hints.push(hint("→", "in"));
        }
        if explorer.can_ascend() {
            // ⌫ only backs out while there is nothing to delete.
            hints.push(hint(if filtering { "←" } else { "← ⌫" }, "out"));
        }
        if filtering {
            hints.extend([hint("⌫", "delete"), hint("esc", "clear")]);
        } else {
            hints.push(hint("esc", "quit"));
        }
    }
    // esc leaves the shell where it was; ^q moves it here.
    hints.push(hint("^q", "quit here"));
    // ⇥ often has nothing to add: an empty filter only fills in what every
    // name shares, and a filter that matches nothing has nothing to grow into.
    if explorer.can_complete() {
        hints.push(hint("⇥", "complete"));
    }
    if !explorer.in_command_mode() {
        if !filtering {
            hints.push(hint(":", "command"));
        }
        if explorer.selected_entry().is_some_and(|e| e.is_dir) {
            hints.push(hint("/", "in"));
        }
        if filtering {
            hints.push(hint("^u", "clear"));
        }
    }
    // A task's canvas is no repository: these four say so instead of acting.
    if explorer.repo.is_some() {
        hints.extend([
            hint("^f", "diff"),
            hint("^p", "PR"),
            hint("^o", "PRs"),
            hint("^w", "worktrees"),
        ]);
    }
    hints.extend([
        hint("^a", "agent"),
        hint("^d", "hidden"),
        hint("^r", "refresh"),
        hint("PgUp/Dn", "ten rows"),
        hint("Home/End", "top/bottom"),
    ]);
    hints
}

/// The hints that fit `width` on one line, closed by `F1 help` when all of
/// them fit and by `F1 +N more` — N being everything left out — when not.
/// A hint is shown whole or not at all.
pub fn hint_line(hints: &[Hint], width: usize) -> String {
    let texts: Vec<String> = hints.iter().map(Hint::text).collect();
    let line = |shown: usize, tail: String| {
        texts[..shown]
            .iter()
            .map(String::as_str)
            .chain([tail.as_str()])
            .collect::<Vec<_>>()
            .join("  ")
    };
    let all = line(texts.len(), "F1 help".to_string());
    if all.width() <= width {
        return all;
    }
    (0..texts.len())
        .rev()
        .map(|shown| line(shown, format!("F1 +{} more", texts.len() - shown)))
        .find(|line| line.width() <= width)
        // Not even the pointer fits beside nothing: say what there is room
        // for rather than let the terminal cut it wherever it lands.
        .unwrap_or_else(|| truncate(&format!("F1 +{} more", texts.len()), width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RepoRecord;
    use crate::worktrees::WorktreeStatus;
    use std::path::Path;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("src/inner")).unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        dir
    }

    fn explorer_at(root: &Path, repo: bool) -> Explorer {
        let repo = repo.then(|| RepoRecord {
            id: "github.com/acme/widget".into(),
            trunk_path: root.to_string_lossy().to_string(),
            remote_url: "https://github.com/acme/widget.git".into(),
            default_branch: "main".into(),
        });
        Explorer::new(
            repo,
            root.to_path_buf(),
            "main".into(),
            "trunk".into(),
            WorktreeStatus::default(),
            root.to_path_buf(),
        )
        .unwrap()
    }

    fn keys(explorer: &Explorer) -> Vec<&'static str> {
        explorer_hints(explorer).iter().map(|h| h.key).collect()
    }

    fn has(explorer: &Explorer, key: &str, label: &str) -> bool {
        explorer_hints(explorer)
            .iter()
            .any(|h| h.key == key && h.label == label)
    }

    #[test]
    fn a_canvas_has_no_repository_keys() {
        let dir = fixture();
        let canvas = keys(&explorer_at(dir.path(), false));
        for key in ["^f", "^p", "^o", "^w"] {
            assert!(!canvas.contains(&key), "{key}");
        }
        let repo = keys(&explorer_at(dir.path(), true));
        for key in ["^f", "^p", "^o", "^w", "^a", "^d", "^r", "^q"] {
            assert!(repo.contains(&key), "{key}");
        }
        assert!(canvas.contains(&"^a") && canvas.contains(&"^q"));
    }

    #[test]
    fn open_edit_and_in_follow_what_is_selected() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        assert_eq!(explorer.selected_entry().unwrap().name, "src");
        assert!(has(&explorer, "⏎", "open") && has(&explorer, "→", "in"));
        assert!(has(&explorer, "/", "in"));

        explorer.select_last();
        assert_eq!(explorer.selected_entry().unwrap().name, "README.md");
        assert!(has(&explorer, "⏎", "edit"));
        assert!(!keys(&explorer).contains(&"→") && !keys(&explorer).contains(&"/"));

        // A filter that matches nothing leaves nothing to act on.
        explorer.set_filter("zzz".into());
        let none = keys(&explorer);
        assert!(!none.contains(&"⏎") && !none.contains(&"→") && !none.contains(&"/"));
    }

    #[test]
    fn out_only_inside_a_subfolder() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        assert!(!keys(&explorer).iter().any(|k| k.contains('←')));
        assert!(explorer.descend().unwrap());
        assert!(has(&explorer, "← ⌫", "out"));
        explorer.push_filter('i');
        assert!(has(&explorer, "←", "out"));
    }

    #[test]
    fn the_filter_decides_between_quit_and_clear() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        assert!(has(&explorer, "esc", "quit") && has(&explorer, "type", "to filter"));
        assert!(has(&explorer, ":", "command"));
        assert!(!keys(&explorer).contains(&"^u"));
        explorer.push_filter('s');
        assert!(has(&explorer, "esc", "clear") && has(&explorer, "^u", "clear"));
        assert!(has(&explorer, "⌫", "delete"));
        assert!(!has(&explorer, "esc", "quit") && !has(&explorer, ":", "command"));
    }

    #[test]
    fn a_command_line_runs_and_does_not_navigate() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        for c in ":to".chars() {
            explorer.push_filter(c);
        }
        assert!(has(&explorer, "⏎", "run") && has(&explorer, "esc", "drop"));
        assert!(has(&explorer, "⇥", "complete") && has(&explorer, "⌫", "delete"));
        let keys = keys(&explorer);
        for key in ["→", "←", "/", "← ⌫"] {
            assert!(!keys.contains(&key), "{key}");
        }
    }

    /// ⇥ is offered only when pressing it would change the filter.
    #[test]
    fn tab_is_offered_only_when_it_completes() {
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        // `src` and `README.md` share nothing to fill in.
        assert!(!has(&explorer, "⇥", "complete"));
        explorer.push_filter('s');
        assert!(has(&explorer, "⇥", "complete"));
        assert!(explorer.complete());
        assert!(!has(&explorer, "⇥", "complete"), "{:?}", explorer.filter);
        explorer.set_filter("zzz".into());
        assert!(!has(&explorer, "⇥", "complete"));
    }

    fn sample() -> Vec<Hint> {
        vec![
            hint("⏎", "open"),
            hint("↑↓", "move"),
            hint("^q", "quit"),
            hint("^f", "diff"),
        ]
    }

    #[test]
    fn a_wide_line_shows_everything_and_ends_on_help() {
        let hints = sample();
        let line = hint_line(&hints, 200);
        assert!(line.ends_with("F1 help"), "{line}");
        assert!(hints.iter().all(|h| line.contains(&h.text())), "{line}");
        assert_eq!(line.matches("F1").count(), 1);
    }

    #[test]
    fn a_narrow_line_counts_what_it_left_out() {
        let hints = sample();
        // "⏎ open  ↑↓ move  F1 +2 more" is 27 columns.
        let line = hint_line(&hints, 27);
        assert_eq!(line, "⏎ open  ↑↓ move  F1 +2 more");
        for width in 0..=60 {
            let line = hint_line(&hints, width);
            assert!(line.width() <= width, "{width}: {line:?}");
            if width >= "F1 +4 more".len() {
                let shown = hints.iter().filter(|h| line.contains(&h.text())).count();
                assert!(
                    line.ends_with(&format!("F1 +{} more", hints.len() - shown))
                        || line.ends_with("F1 help"),
                    "{width}: {line}"
                );
                // Whole hints only, in order, from the front.
                let kept = line.split("  ").filter(|p| !p.starts_with("F1")).count();
                assert_eq!(kept, shown, "{width}: {line}");
            }
        }
    }

    #[test]
    fn a_very_narrow_line_still_fits() {
        let hints = sample();
        for width in 0..10 {
            assert!(hint_line(&hints, width).width() <= width, "{width}");
        }
        assert_eq!(hint_line(&hints, 5), "F1 +…");
    }

    #[test]
    fn the_help_window_lists_every_hint_first() {
        use ratatui::text::Line;
        let dir = fixture();
        let mut explorer = explorer_at(dir.path(), true);
        explorer.push_filter('s');
        let hints = explorer_hints(&explorer);
        let text: Vec<String> = super::super::ui::help_body(100, &hints)
            .iter()
            .map(Line::to_string)
            .collect();
        assert!(text[0].starts_with("now"), "{:?}", text[0]);
        // The keys that work now come first, before the blank line that
        // starts the reference, each with its key and label kept together.
        let gap = text.iter().position(|l| l.trim().is_empty()).unwrap();
        let now = text[..gap].join(" ");
        for h in &hints {
            let whole = format!("{} {}", h.key, h.label).replace(' ', "\u{a0}");
            assert!(now.contains(&whole), "{whole:?} not in {now:?}");
        }
        assert!(gap <= 4, "{gap} lines for the keys that work now");
        assert!(text[gap..].iter().any(|l| l.contains("this list")));
    }
}
