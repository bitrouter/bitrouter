//! The `WorkerHarness` seam: each ACP agent we support knows how to pin a model,
//! route, and scoped key (`materialize`) and how to be launched (`spawn`).
//!
//! Launch argv comes from a vendored, version-pinned snapshot of the ACP registry
//! (`acp-registry.snapshot.json`). We deliberately do NOT live-fetch or auto-install.

use serde::Deserialize;

use bitrouter_sdk::{BitrouterError, Result};

use crate::subagent::acp_client::WorkerSpawn;
use crate::subagent::worker_config::WorkerWorkspace;

const REGISTRY_SNAPSHOT: &str = include_str!("acp-registry.snapshot.json");

#[derive(Debug, Deserialize)]
struct Snapshot {
    agents: std::collections::HashMap<String, LaunchSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct LaunchSpec {
    cmd: String,
    args: Vec<String>,
}

/// Look up an agent's launch `(cmd, args)` from the vendored registry snapshot.
fn launch_spec(id: &str) -> Result<LaunchSpec> {
    let snap: Snapshot = serde_json::from_str(REGISTRY_SNAPSHOT)
        .map_err(|e| BitrouterError::internal(format!("parsing acp registry snapshot: {e}")))?;
    snap.agents
        .get(id)
        .cloned()
        .ok_or_else(|| BitrouterError::bad_request(format!("unknown harness '{id}'")))
}

/// One ACP coding agent BitRouter can spawn as a budgeted subagent.
pub trait WorkerHarness: Send + Sync {
    /// Registry / config id (e.g. `"opencode"`).
    fn id(&self) -> &str;
    /// Build the isolated worktree + the env/cwd that pins model + route + key.
    fn materialize(
        &self,
        base_url: &str,
        model: &str,
        brvk_secret: &str,
        unique: &str,
    ) -> Result<WorkerWorkspace>;
    /// Build the spawn command for a materialized workspace.
    fn spawn(&self, ws: &WorkerWorkspace) -> Result<WorkerSpawn>;
}

/// opencode: launched `opencode acp --cwd <ws>`, pinned via a generated
/// `opencode.json` referenced by `OPENCODE_CONFIG`.
pub struct OpencodeHarness;

impl WorkerHarness for OpencodeHarness {
    fn id(&self) -> &str {
        "opencode"
    }

    fn materialize(
        &self,
        base_url: &str,
        model: &str,
        brvk_secret: &str,
        unique: &str,
    ) -> Result<WorkerWorkspace> {
        crate::subagent::worker_config::materialize(base_url, model, brvk_secret, unique)
    }

    fn spawn(&self, ws: &WorkerWorkspace) -> Result<WorkerSpawn> {
        let spec = launch_spec(self.id())?;
        let mut args = spec.args;
        args.push("--cwd".to_string());
        args.push(ws.cwd.clone());
        Ok(WorkerSpawn {
            command: spec.cmd,
            args,
            env: ws.env.clone(),
            working_dir: Some(ws.cwd.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_parses_and_has_opencode() {
        let spec = launch_spec("opencode").unwrap();
        assert_eq!(spec.cmd, "opencode");
        assert_eq!(spec.args, vec!["acp".to_string()]);
        assert!(launch_spec("nope").is_err());
    }

    #[test]
    fn opencode_spawn_appends_cwd() {
        let h = OpencodeHarness;
        assert_eq!(h.id(), "opencode");
        let ws = h
            .materialize(
                "http://127.0.0.1:4356/v1",
                "bitrouter/anthropic/claude-haiku-4.5",
                "brvk_x",
                "h1test",
            )
            .unwrap();
        let spawn = h.spawn(&ws).unwrap();
        assert_eq!(spawn.command, "opencode");
        assert_eq!(
            spawn.args,
            vec!["acp".to_string(), "--cwd".to_string(), ws.cwd.clone()]
        );
        assert_eq!(spawn.working_dir.as_deref(), Some(ws.cwd.as_str()));
        assert!(spawn.env.contains_key("OPENCODE_CONFIG"));
    }
}
