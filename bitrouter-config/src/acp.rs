//! ACP registry schema — pure serde types.
//!
//! Mirrors the JSON at <https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json>.
//! Fetching, caching, and conversion to the runtime [`AgentConfig`] live in
//! `bitrouter-providers::acp` so this crate stays free of async/HTTP deps.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, AgentProtocol, BinaryArchive, Distribution};

/// Default URL of the public ACP registry.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// Environment variable that overrides the configured registry URL.
pub const REGISTRY_URL_ENV: &str = "BITROUTER_ACP_REGISTRY_URL";

/// Top-level shape of the ACP registry JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryIndex {
    pub version: String,
    pub agents: Vec<RegistryAgent>,
}

/// One agent entry from the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub distribution: RegistryDistribution,
}

/// How an agent is distributed.  At least one variant should be populated.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegistryDistribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<RegistryNpx>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<RegistryUvx>,

    /// Map of platform target (e.g. `darwin-aarch64`) to archive metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub binary: HashMap<String, BinaryArchive>,
}

/// Launch via `npx <package> [args...]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryNpx {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Launch via `uvx <package> [args...]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryUvx {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Convert a [`RegistryAgent`] into the runtime [`AgentConfig`] used by the
/// ACP provider layer.
///
/// The registry's `{npx?, uvx?, binary?}` object collapses into a
/// `Vec<Distribution>` with one element per populated variant.  The returned
/// [`AgentConfig`] has `enabled = true` and an empty `args`/`a2a`/`session`
/// pair — callers can override these before merging into `BitrouterConfig`.
pub fn registry_agent_to_config(agent: &RegistryAgent) -> AgentConfig {
    let mut distribution: Vec<Distribution> = Vec::new();

    if let Some(npx) = &agent.distribution.npx {
        distribution.push(Distribution::Npx {
            package: npx.package.clone(),
            args: npx.args.clone(),
        });
    }
    if let Some(uvx) = &agent.distribution.uvx {
        distribution.push(Distribution::Uvx {
            package: uvx.package.clone(),
            args: uvx.args.clone(),
        });
    }
    if !agent.distribution.binary.is_empty() {
        distribution.push(Distribution::Binary {
            platforms: agent.distribution.binary.clone(),
        });
    }

    AgentConfig {
        protocol: AgentProtocol::Acp,
        binary: agent.id.clone(),
        args: Vec::new(),
        enabled: true,
        distribution,
        session: None,
        a2a: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": "1.0.0",
        "agents": [
            {
                "id": "claude-acp",
                "name": "Claude Agent",
                "version": "0.30.0",
                "description": "ACP wrapper for Anthropic's Claude",
                "license": "proprietary",
                "authors": ["Anthropic"],
                "distribution": {
                    "npx": {
                        "package": "@agentclientprotocol/claude-agent-acp@0.30.0"
                    }
                }
            },
            {
                "id": "codex-acp",
                "name": "Codex CLI",
                "version": "0.11.1",
                "authors": [],
                "distribution": {
                    "binary": {
                        "darwin-aarch64": {
                            "archive": "https://example.com/codex-darwin.tar.gz",
                            "cmd": "./codex-acp"
                        }
                    },
                    "npx": {
                        "package": "@zed-industries/codex-acp@0.11.1"
                    }
                }
            }
        ]
    }"#;

    #[test]
    fn deserializes_registry_shape() -> Result<(), Box<dyn std::error::Error>> {
        let idx: RegistryIndex = serde_json::from_str(SAMPLE)?;
        assert_eq!(idx.version, "1.0.0");
        assert_eq!(idx.agents.len(), 2);
        assert_eq!(idx.agents[0].id, "claude-acp");
        assert!(idx.agents[0].distribution.npx.is_some());
        assert!(
            idx.agents[1]
                .distribution
                .binary
                .contains_key("darwin-aarch64")
        );
        Ok(())
    }

    #[test]
    fn converts_npx_agent_to_config() -> Result<(), Box<dyn std::error::Error>> {
        let idx: RegistryIndex = serde_json::from_str(SAMPLE)?;
        let cfg = registry_agent_to_config(&idx.agents[0]);
        assert_eq!(cfg.binary, "claude-acp");
        assert_eq!(cfg.distribution.len(), 1);
        let Distribution::Npx { package, .. } = &cfg.distribution[0] else {
            return Err(format!("expected Npx, got {:?}", cfg.distribution[0]).into());
        };
        assert_eq!(package, "@agentclientprotocol/claude-agent-acp@0.30.0");
        Ok(())
    }

    #[test]
    fn converts_multi_distribution_agent() -> Result<(), Box<dyn std::error::Error>> {
        let idx: RegistryIndex = serde_json::from_str(SAMPLE)?;
        let cfg = registry_agent_to_config(&idx.agents[1]);
        // binary + npx → 2 entries; order is fixed (npx, uvx, binary).
        assert_eq!(cfg.distribution.len(), 2);
        assert!(matches!(cfg.distribution[0], Distribution::Npx { .. }));
        assert!(matches!(cfg.distribution[1], Distribution::Binary { .. }));
        Ok(())
    }

    #[test]
    fn rejects_malformed_json() {
        let bad = r#"{ "version": "1", "agents": "not-an-array" }"#;
        let result: Result<RegistryIndex, _> = serde_json::from_str(bad);
        assert!(result.is_err());
    }
}
