//! `bitrouter tui` — in-process multi-agent manager (M1: single agent).

mod event;
mod pump;
mod state;

use anyhow::{Context, Result};

/// Launch the TUI against `agent_id`, optionally inside a git worktree `name`.
/// M1 hosts a single session; multi-agent is M2.
pub async fn run(agent_id: &str, worktree: Option<&str>) -> Result<()> {
    // Real implementation lands in Task 7. Stubbed so the crate compiles now.
    let _ = (agent_id, worktree);
    Err(anyhow::anyhow!("bitrouter tui: not yet implemented")).context("tui::run")
}
