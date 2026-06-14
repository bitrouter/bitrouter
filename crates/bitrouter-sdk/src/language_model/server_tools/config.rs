//! Configuration for the server-side tool loop: the runtime
//! [`ServerToolLoopConfig`] bounds and the deserialised [`ServerToolsConfig`]
//! YAML section.

use std::time::Duration;

use serde::Deserialize;

/// Bounds the [`ServerToolLoop`](super::loop_controller::ServerToolLoop): how
/// many tool rounds it runs, how long each tool may take, the total wall-clock
/// budget for the turn, and how many consecutive tool-error rounds it tolerates
/// before giving up.
#[derive(Debug, Clone)]
pub struct ServerToolLoopConfig {
    /// Maximum number of tool-execution rounds. Reaching it terminates the
    /// loop with a truncation finish reason. Default 10.
    pub max_iterations: u32,
    /// Per-tool execution timeout. Default 30s.
    pub tool_timeout: Duration,
    /// Total wall-clock budget for the whole turn. Default 120s.
    pub total_budget: Duration,
    /// Consecutive tool-error rounds tolerated before giving up. Default 3.
    pub max_consecutive_errors: u32,
}

impl Default for ServerToolLoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            tool_timeout: Duration::from_secs(30),
            total_budget: Duration::from_secs(120),
            max_consecutive_errors: 3,
        }
    }
}

/// The OSS `server_tools` config section. Names the MCP servers whose tools
/// BitRouter attaches to LLM requests and executes inside the loop, with an
/// optional override of the loop's iteration cap. An empty `mcp_servers`
/// leaves the pipeline strictly single-shot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerToolsConfig {
    /// MCP server names (keys of `mcp_servers`) whose tools are injected into
    /// LLM requests and executed by the loop.
    pub mcp_servers: Vec<String>,
    /// Optional override of the loop's maximum tool-execution rounds.
    pub max_iterations: Option<u32>,
    /// When set, enables the router-owned `spawn_subagent` tool (in addition to
    /// any `mcp_servers`). The agent calls it to spawn a budgeted subagent.
    pub spawn_subagent: Option<SpawnSubagentConfig>,
}

/// Settings for the `spawn_subagent` router tool. Names the model allowlist a
/// spawned worker may use, the base URL the worker should call back on (the
/// local daemon, so the worker's inferences are metered), and the operator
/// allowlist of permitted ACP worker harnesses.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SpawnSubagentConfig {
    /// The daemon URL the spawned worker routes its inferences to. Must point at
    /// THIS daemon (e.g. `http://127.0.0.1:4356/v1`) so the worker's calls carry
    /// its scoped `brvk_` and are metered + capped here.
    pub base_url: String,
    /// Operator allowlist of permitted ACP worker harnesses (by registry id,
    /// e.g. `opencode`, `claude-acp`). The FIRST entry is the default used for
    /// spawns. The model does NOT choose the binary. Default: `["opencode"]`.
    pub harnesses: Vec<String>,
    /// Models a spawned worker is allowed to use. A `spawn_subagent` call naming
    /// a model outside this list is rejected.
    pub models: Vec<String>,
}

impl Default for SpawnSubagentConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:4356/v1".to_string(),
            harnesses: vec!["opencode".to_string()],
            models: Vec::new(),
        }
    }
}

#[cfg(all(test, feature = "config_file"))]
mod spawn_subagent_config_tests {
    use super::*;

    #[test]
    fn deserializes_spawn_subagent_section() {
        let yaml = r#"
mcp_servers: []
spawn_subagent:
  base_url: "http://127.0.0.1:4356/v1"
  harnesses: ["opencode"]
  models:
    - "bitrouter/z-ai/glm-5.1"
"#;
        let cfg: ServerToolsConfig = serde_saphyr::from_str(yaml).unwrap();
        let sa = cfg.spawn_subagent.expect("section present");
        assert_eq!(sa.base_url, "http://127.0.0.1:4356/v1");
        assert_eq!(sa.harnesses, vec!["opencode".to_string()]);
        assert_eq!(sa.models, vec!["bitrouter/z-ai/glm-5.1".to_string()]);
    }

    #[test]
    fn spawn_subagent_absent_by_default() {
        let cfg: ServerToolsConfig = serde_saphyr::from_str("mcp_servers: []").unwrap();
        assert!(cfg.spawn_subagent.is_none());
    }
}
