//! `jeet sessions` — previous coding-agent sessions for the current worktree.

use anyhow::{Context, Result};

use crate::agent::AgentSpec;
use crate::context::App;
use crate::resolve;

pub fn run(app: &App) -> Result<()> {
    let cwd = std::env::current_dir().context("get cwd")?;
    let ctx = resolve::resolve_context(app, &cwd)?;
    let spec = AgentSpec::from_config(&app.config)?;

    let sessions = crate::agent::sessions_for(&spec, &ctx.root)?;
    println!(
        "{} sessions for {} ({})",
        spec.display(),
        ctx.label(),
        ctx.root.display()
    );
    if sessions.is_empty() {
        println!("  none recorded yet");
        return Ok(());
    }
    for session in sessions {
        println!(
            "  {:<38} {:<10} {:>5} msgs  {}",
            session.id,
            session.age(),
            session.entries,
            session.summary
        );
    }
    Ok(())
}
