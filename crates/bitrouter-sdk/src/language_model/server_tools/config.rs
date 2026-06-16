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
    /// Multi-model deliberation (Fusion). When set, the `bitrouter:fusion`
    /// server tool and the `bitrouter/fusion` model alias are enabled.
    pub fusion: Option<super::fusion::config::FusionSettings>,
}
