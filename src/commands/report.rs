//! `jeet report`: a task with a parent tells it how it is going.

use anyhow::{bail, Result};

use crate::context::App;
use crate::db::tasks::ReportKind;

pub fn run(_app: &App, _kind: ReportKind, _message: &str) -> Result<()> {
    bail!("not yet")
}
