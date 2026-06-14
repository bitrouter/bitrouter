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

/// Strip a trailing `/v1` (and any trailing slash) so the Anthropic SDK can
/// append its own `/v1/messages`.
fn anthropic_base(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_suffix("/v1") {
        // Guard a degenerate config (e.g. base_url just `/v1`) stripping to an
        // empty base — which would make the SDK fall back to the public
        // api.anthropic.com. Keep the (non-empty) trimmed value instead.
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => trimmed,
    }
}

/// claude-agent-acp: launched via `npx`, pinned via ENV only
/// (`ANTHROPIC_BASE_URL` = daemon base WITHOUT `/v1`, `ANTHROPIC_AUTH_TOKEN`,
/// `ANTHROPIC_MODEL`). Anthropic wire → daemon `/v1/messages`.
pub struct ClaudeAcpHarness;

impl WorkerHarness for ClaudeAcpHarness {
    fn id(&self) -> &str {
        "claude-acp"
    }

    fn materialize(
        &self,
        base_url: &str,
        model: &str,
        brvk_secret: &str,
        unique: &str,
    ) -> Result<WorkerWorkspace> {
        let (root, cwd) = crate::subagent::worker_config::make_worktree(unique)?;
        let wire = crate::subagent::worker_config::wire_model_id(model);
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            anthropic_base(base_url).to_string(),
        );
        env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), brvk_secret.to_string());
        env.insert("ANTHROPIC_MODEL".to_string(), wire.to_string());
        Ok(WorkerWorkspace { root, env, cwd })
    }

    fn spawn(&self, ws: &WorkerWorkspace) -> Result<WorkerSpawn> {
        let spec = launch_spec(self.id())?;
        Ok(WorkerSpawn {
            command: spec.cmd,
            args: spec.args,
            env: ws.env.clone(),
            working_dir: Some(ws.cwd.clone()),
        })
    }
}

/// Resolve a harness id (from the operator allowlist) to its implementation.
pub fn harness_for(id: &str) -> Result<Box<dyn WorkerHarness>> {
    match id {
        "opencode" => Ok(Box::new(OpencodeHarness)),
        "claude-acp" => Ok(Box::new(ClaudeAcpHarness)),
        other => Err(BitrouterError::bad_request(format!(
            "unknown harness '{other}'"
        ))),
    }
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
    fn anthropic_base_strips_v1() {
        assert_eq!(
            anthropic_base("http://127.0.0.1:4356/v1"),
            "http://127.0.0.1:4356"
        );
        assert_eq!(
            anthropic_base("http://127.0.0.1:4356/v1/"),
            "http://127.0.0.1:4356"
        );
        assert_eq!(anthropic_base("http://h:1/x"), "http://h:1/x");
    }

    #[test]
    fn claude_acp_materialize_sets_anthropic_env_no_file() {
        let h = ClaudeAcpHarness;
        assert_eq!(h.id(), "claude-acp");
        let ws = h
            .materialize(
                "http://127.0.0.1:4356/v1",
                "bitrouter/anthropic/claude-haiku-4.5",
                "brvk_x",
                "h2c",
            )
            .unwrap();
        assert_eq!(
            ws.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:4356")
        );
        assert_eq!(
            ws.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("brvk_x")
        );
        assert_eq!(
            ws.env.get("ANTHROPIC_MODEL").map(String::as_str),
            Some("anthropic/claude-haiku-4.5")
        );
        // env-only: no OPENCODE_CONFIG, no opencode.json on disk
        assert!(!ws.env.contains_key("OPENCODE_CONFIG"));
        assert!(!ws.root.join("opencode.json").exists());
        let spawn = h.spawn(&ws).unwrap();
        assert_eq!(spawn.command, "npx");
        assert!(spawn.args.iter().any(|a| a.contains("claude-agent-acp")));
        assert_eq!(spawn.working_dir.as_deref(), Some(ws.cwd.as_str()));
    }

    #[test]
    fn harness_for_resolves_known_and_rejects_unknown() {
        assert_eq!(harness_for("opencode").unwrap().id(), "opencode");
        assert_eq!(harness_for("claude-acp").unwrap().id(), "claude-acp");
        assert!(harness_for("nope").is_err());
    }

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
