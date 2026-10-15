//! Deployment settings remain readable without the server-tool runtime.

use serde::Deserialize;

pub mod fusion;
pub mod web_fetch;
pub mod web_search;

/// The OSS `server_tools` config section. Names the MCP servers whose tools
/// BitRouter attaches to LLM requests and executes inside the loop, with an
/// optional override of the loop's iteration cap. An empty `mcp_servers`
/// leaves the pipeline strictly single-shot.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ServerToolsConfig {
    /// MCP server names (keys of `mcp_servers`) whose tools are injected into
    /// LLM requests and executed by the loop.
    pub mcp_servers: Vec<String>,
    /// Optional override of the loop's maximum tool-execution rounds.
    pub max_iterations: Option<u32>,
    /// Enable the `advisor` server tool (consult a stronger model mid-task).
    /// Advertised per-request only when the caller declares `bitrouter:advisor`.
    pub advisor: bool,
    /// Enable the `subagent` server tool (delegate a task to a worker model).
    /// Advertised per-request only when the caller declares `bitrouter:subagent`.
    pub subagent: bool,
    /// Multi-model deliberation (Fusion). When set, the `bitrouter:fusion`
    /// server tool and the `bitrouter/fusion` model alias are enabled.
    pub fusion: Option<fusion::FusionSettings>,
    /// Built-in `web_search` server tool (BYOK). When set, the
    /// `bitrouter:web_search` tool is enabled, served by the configured search
    /// backends. Advertised per-request only when the caller declares it.
    pub web_search: Option<web_search::WebSearchSettings>,
    /// Built-in `web_fetch` server tool (BYOK). When set, the
    /// `bitrouter:web_fetch` tool is enabled, served by the configured fetch
    /// backends. Advertised per-request only when the caller declares it.
    pub web_fetch: Option<web_fetch::WebFetchSettings>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_tools_config_accepts_web_fetch() {
        let s: ServerToolsConfig = serde_json::from_value(serde_json::json!({
            "web_fetch": { "backends": [{ "kind": "exa" }] }
        }))
        .unwrap();
        assert!(s.web_fetch.is_some());
    }
}
