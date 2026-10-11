//! Where a task's agent runs: its placement, chosen when the task is made
//! and recorded in the index, and the runtime that starts and interrupts it
//! there.
//!
//! Every node, managers below the root included, runs `local`: in a tmux
//! pane on the user's machine, as the user, cooperative and unconfined. A
//! confined placement (a local sandbox such as sandvault, say) can plug in
//! here later as another [`Placement`] with its own [`NodeRuntime`].
//!
//! The other [`NodeRuntime`] methods (send, stop, activity, attach, collect)
//! arrive with the second runtime; see design §11.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::context::App;
use crate::db::tasks::TaskRecord;
use crate::runtime::{self, Interrupted, Running};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// tmux panes on the user's machine: unconfined and cooperative.
    Local,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Local => "local",
        }
    }

    /// The placement stored as `s`, or `None` for one this jeet does not
    /// know, which is then never run.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Placement::Local),
            _ => None,
        }
    }
}

/// Starts and interrupts a task's agent where its placement says it runs.
pub trait NodeRuntime {
    /// Make sure the agent is running, as [`runtime::ensure_running`].
    fn start(
        &self,
        app: &App,
        task: &TaskRecord,
        session: Option<&str>,
        prompt: Option<&str>,
    ) -> Result<Running>;

    /// End the turn the agent is running, as [`runtime::interrupt`].
    fn interrupt(&self, app: &App, task: &TaskRecord) -> Result<Interrupted>;
}

/// The runtime for `task`'s placement: the one way a node is started or
/// interrupted. A placement this jeet does not know never reaches here: the
/// index's `CHECK` stores only known ones, a later jeet's placement comes
/// with a newer schema that `Database::migrate` refuses whole, and a row
/// hand-edited past the `CHECK` fails to read.
pub fn runtime_for(task: &TaskRecord) -> Result<&'static dyn NodeRuntime> {
    match task.placement {
        Placement::Local => Ok(&runtime::LocalTmux),
    }
}

/// Where a new task runs: on this machine, wherever it sits in the tree.
pub fn for_new() -> Placement {
    Placement::Local
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::{TaskKind, TaskState};
    use crate::db::Database;

    fn task(id: &str, kind: TaskKind, parent: Option<&str>) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            slug: id.into(),
            name: id.into(),
            project: None,
            kind,
            state: TaskState::Active,
            created_at: 0,
            last_activity: 0,
            parent_id: parent.map(Into::into),
            placement: for_new(),
        }
    }

    #[test]
    fn placements_round_trip_and_unknown_ones_are_refused() {
        assert_eq!(
            Placement::parse(Placement::Local.as_str()),
            Some(Placement::Local)
        );
        for unknown in ["Local", "cloud", "local-sandbox", "moon", ""] {
            assert_eq!(Placement::parse(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn a_node_below_a_manager_runs_locally_like_any_other() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let db = Database::open(&config::index_path(&home)).unwrap();
        // The root `a`, its child `m`, and `x` under `m` (a manager below the
        // root), and `y` under `x`.
        for t in [
            task("a", TaskKind::Assistant, None),
            task("m", TaskKind::Work, Some("a")),
            task("x", TaskKind::Work, Some("m")),
            task("y", TaskKind::Work, Some("x")),
        ] {
            db.insert_task(&t).unwrap();
        }
        for id in ["m", "x", "y"] {
            let t = db.get_task(id).unwrap().unwrap();
            assert_eq!(t.placement, Placement::Local, "{id}");
            assert!(runtime_for(&t).is_ok(), "{id}");
        }
    }
}
