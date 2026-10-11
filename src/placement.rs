//! Where a task's agent runs: its placement, chosen when the task is made
//! and recorded in the index, and the runtime that starts and interrupts it
//! there.
//!
//! The root and the tasks directly under it may run `local`, in tmux panes
//! on the user's machine. Managers below the root, and every node under
//! them, must run behind a boundary, and a child is never less confined than
//! its parent: [`runtime_for`] refuses any start or interrupt that breaks
//! this, and any placement not built yet.
//!
//! The other [`NodeRuntime`] methods (send, stop, activity, attach, collect)
//! arrive with the second runtime; see design §11.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::context::App;
use crate::db::tasks::{TaskKind, TaskRecord};
use crate::runtime::{self, Interrupted, Running};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// tmux panes on the user's machine: unconfined and cooperative.
    Local,
    /// Claude Code cloud sessions in Anthropic-managed VMs. Designed, not
    /// built.
    Cloud,
    /// A confined local runtime. Reserved, not built.
    LocalSandbox,
}

/// What keeps a node's agent from acting outside what jeet lets it do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    None,
    /// It runs on another machine.
    Machine,
    /// It runs confined on this one.
    Sandbox,
}

/// What a placement provides.
#[derive(Clone, Copy, Debug)]
pub struct Support {
    pub boundary: Boundary,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Local => "local",
            Placement::Cloud => "cloud",
            Placement::LocalSandbox => "local-sandbox",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Placement::Local),
            "cloud" => Some(Placement::Cloud),
            "local-sandbox" => Some(Placement::LocalSandbox),
            _ => None,
        }
    }

    pub fn support(self) -> Support {
        let boundary = match self {
            Placement::Local => Boundary::None,
            Placement::Cloud => Boundary::Machine,
            Placement::LocalSandbox => Boundary::Sandbox,
        };
        Support { boundary }
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

/// The runtime for `task`'s placement, or why it may not run there: the
/// one way a node is started or interrupted.
pub fn runtime_for(app: &App, task: &TaskRecord) -> Result<&'static dyn NodeRuntime> {
    require_boundary(app, task)?;
    match task.placement {
        Placement::Local => Ok(&runtime::LocalTmux),
        Placement::Cloud => bail!(
            "task {} runs in the cloud, which is not built yet: it waits on the choice of \
             cloud runtime (Q1) and the probes in docs/cloud-probes.md",
            task.name
        ),
        Placement::LocalSandbox => bail!(
            "task {} runs in a local sandbox, which is not built yet",
            task.name
        ),
    }
}

/// Refuse a task that must run behind a boundary but is placed where there
/// is none: one whose parent is not the root, or whose parent runs behind a
/// boundary itself.
fn require_boundary(app: &App, task: &TaskRecord) -> Result<()> {
    let Some(parent_id) = &task.parent_id else {
        return Ok(());
    };
    let Some(parent) = app.db.get_task(parent_id)? else {
        bail!(
            "task {}'s parent {parent_id} is not in the index, so where it may run cannot be \
             checked",
            task.name
        );
    };
    let needs =
        parent.kind != TaskKind::Assistant || parent.placement.support().boundary != Boundary::None;
    if needs && task.placement.support().boundary == Boundary::None {
        bail!(
            "task {} cannot run: managers below the root, and every node under them, must run \
             behind a boundary; placement {} has none",
            task.name,
            task.placement.as_str()
        );
    }
    Ok(())
}

/// Where a new task under `parent` runs: on this machine at the top level
/// or under the root, else where `[managers] placement` says.
pub fn for_new(config: &Config, parent: Option<&TaskRecord>) -> Result<Placement> {
    match parent {
        Some(parent) if parent.kind != TaskKind::Assistant => config.manager_placement(),
        _ => Ok(Placement::Local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::TaskState;
    use crate::db::Database;

    const EVERY: [Placement; 3] = [Placement::Local, Placement::Cloud, Placement::LocalSandbox];

    fn task(id: &str, kind: TaskKind, parent: Option<&str>, placement: Placement) -> TaskRecord {
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
            placement,
        }
    }

    /// An index in a tempdir with the root `a`, its local child `w`, and
    /// `c` placed in the cloud under it.
    fn lab() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let app = App {
            config: config::load_or_create(&home).unwrap(),
            db: Database::open(&config::index_path(&home)).unwrap(),
            home,
        };
        for t in [
            task("a", TaskKind::Assistant, None, Placement::Local),
            task("w", TaskKind::Work, Some("a"), Placement::Local),
            task("c", TaskKind::Work, Some("a"), Placement::Cloud),
        ] {
            app.db.insert_task(&t).unwrap();
        }
        (dir, app)
    }

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn placements_round_trip_and_say_what_confines_them() {
        for p in EVERY {
            assert_eq!(Placement::parse(p.as_str()), Some(p));
        }
        assert_eq!(Placement::parse("Local"), None);
        let boundaries = EVERY.map(|p| p.support().boundary);
        assert_eq!(
            boundaries,
            [Boundary::None, Boundary::Machine, Boundary::Sandbox]
        );
    }

    #[test]
    fn only_tasks_below_a_manager_take_the_managers_placement() {
        let root = task("a", TaskKind::Assistant, None, Placement::Local);
        let manager = task("m", TaskKind::Work, Some("a"), Placement::Cloud);
        let default = Config::default();
        assert_eq!(for_new(&default, None).unwrap(), Placement::Local);
        assert_eq!(for_new(&default, Some(&root)).unwrap(), Placement::Local);
        assert_eq!(for_new(&default, Some(&manager)).unwrap(), Placement::Cloud);
        let sandbox = config("[managers]\nplacement = \"local-sandbox\"\n");
        assert_eq!(
            for_new(&sandbox, Some(&manager)).unwrap(),
            Placement::LocalSandbox
        );
        let local = config("[managers]\nplacement = \"local\"\n");
        let err = for_new(&local, Some(&manager)).unwrap_err();
        assert!(err.to_string().contains("boundary"), "{err}");
        // The root's own children never need one.
        assert_eq!(for_new(&local, Some(&root)).unwrap(), Placement::Local);
    }

    #[test]
    fn a_local_node_below_a_manager_or_a_confined_parent_is_refused() {
        let (_dir, app) = lab();
        let w = app.db.get_task("w").unwrap().unwrap();
        require_boundary(&app, &w).unwrap();
        let top = task("top", TaskKind::Work, None, Placement::Local);
        require_boundary(&app, &top).unwrap();

        let under_worker = task("x", TaskKind::Work, Some("w"), Placement::Local);
        let err = require_boundary(&app, &under_worker).unwrap_err();
        assert!(err.to_string().contains("has none"), "{err}");
        let under_cloud = task("x", TaskKind::Work, Some("c"), Placement::Local);
        let err = require_boundary(&app, &under_cloud).unwrap_err();
        assert!(err.to_string().contains("has none"), "{err}");
        let confined = task("x", TaskKind::Work, Some("c"), Placement::LocalSandbox);
        require_boundary(&app, &confined).unwrap();

        let orphan = task("x", TaskKind::Work, Some("gone"), Placement::Cloud);
        let err = require_boundary(&app, &orphan).unwrap_err();
        assert!(err.to_string().contains("not in the index"), "{err}");
    }

    #[test]
    fn only_local_runs_until_another_runtime_is_built() {
        let (_dir, app) = lab();
        let w = app.db.get_task("w").unwrap().unwrap();
        assert!(runtime_for(&app, &w).is_ok());
        let c = app.db.get_task("c").unwrap().unwrap();
        let err = runtime_for(&app, &c).err().unwrap();
        assert!(err.to_string().contains("not built yet"), "{err}");
        assert!(err.to_string().contains("(Q1)"), "{err}");
        let sandbox = task("s", TaskKind::Work, Some("a"), Placement::LocalSandbox);
        let err = runtime_for(&app, &sandbox).err().unwrap();
        assert!(
            err.to_string()
                .contains("local sandbox, which is not built yet"),
            "{err}"
        );
        // The boundary rule comes first.
        let under_worker = task("x", TaskKind::Work, Some("w"), Placement::Local);
        let err = runtime_for(&app, &under_worker).err().unwrap();
        assert!(err.to_string().contains("has none"), "{err}");
    }
}
