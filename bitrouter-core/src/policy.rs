//! Policy file types shared between the CLI and the server runtime.
//!
//! Policy files are JSON documents stored in `<home>/policies/`. Each policy
//! defines spend limits and per-provider tool allow-lists. A single policy
//! is attached to an API key via the JWT `pol` claim.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::routers::admin::ToolFilter;

// ── Per-provider policy ────────────────────────────────────────────

/// Per-provider tool policy configuration.
///
/// Defines an allow-list controlling which tools from this provider are
/// visible and callable. Used in policy files under the `tool_rules` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProviderPolicy {
    /// Visibility filter controlling which tools appear in discovery
    /// and are callable at request time.
    #[serde(default, flatten)]
    pub filter: ToolFilter,
}

// ── Data types ─────────────────────────────────────────────────────

/// Input context sent by OWS on stdin when invoking an executable policy.
///
/// Additional fields (`wallet`, `api_key`, etc.) are accepted and ignored
/// by serde's default behavior — only the fields needed for evaluation are
/// declared here.
#[derive(Debug, Deserialize)]
pub struct PolicyContext {
    /// CAIP-2 chain identifier (e.g. `"tempo:mainnet"`).
    #[serde(default)]
    pub chain: Option<String>,
    /// Transaction value in micro-USD.
    #[serde(default)]
    pub transaction_value: u64,
    /// Accumulated daily spend in micro-USD (provided by OWS).
    #[serde(default)]
    pub daily_total: u64,
    /// Accumulated monthly spend in micro-USD (provided by OWS).
    #[serde(default)]
    pub monthly_total: u64,
}

/// Result written to stdout after policy evaluation.
#[derive(Debug, Serialize)]
pub struct PolicyResult {
    pub allow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Operator-defined policy configuration stored in a policy file.
///
/// Combines OWS spend-limit rules with tool access control (per-provider
/// allow-lists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Human-readable policy name.
    pub name: String,

    /// Maximum daily spend in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_limit: Option<u64>,

    /// Maximum monthly spend in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_limit: Option<u64>,

    /// Maximum per-transaction value in micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_tx_max: Option<u64>,

    /// Allowed chains (CAIP-2). Empty means all chains allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chains: Vec<String>,

    /// Policy expiration (ISO 8601). After this time, policy denies all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// Per-provider tool access rules.
    ///
    /// Keys are provider/server names (e.g. `"github"`). Values define
    /// allow-list filters for that provider. When absent or empty, no
    /// tool restrictions apply.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tool_rules: HashMap<String, ToolProviderPolicy>,
}

/// Full on-disk policy file: config + OWS integration metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyFile {
    /// Unique policy ID (UUID).
    pub id: String,

    /// The policy configuration (spend limits + tool access rules).
    #[serde(flatten)]
    pub config: PolicyConfig,

    /// Path to the evaluator executable (populated by `create`).
    pub executable: String,

    /// When this policy was created (ISO 8601).
    pub created_at: String,
}

// ── Helpers ────────────────────────────────────────────────────────

/// Resolve the policy directory for a given BitRouter home.
pub fn policy_dir(home: &Path) -> PathBuf {
    home.join("policies")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_config_round_trips_through_json() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: Some(10_000_000),
            monthly_limit: None,
            per_tx_max: Some(1_000_000),
            allowed_chains: vec!["tempo:mainnet".into()],
            expires_at: None,
            tool_rules: HashMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PolicyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.daily_limit, Some(10_000_000));
        assert!(parsed.tool_rules.is_empty());
    }

    #[test]
    fn policy_file_with_tool_rules_deserializes() {
        let json = r#"{
            "id": "abc-123",
            "name": "restricted-agent",
            "tool_rules": {
                "github": {
                    "allow": ["search_code", "get_file"]
                }
            },
            "executable": "bitrouter policy eval",
            "created_at": "2026-04-10T00:00:00Z"
        }"#;
        let pf: PolicyFile = serde_json::from_str(json).unwrap();
        assert_eq!(pf.id, "abc-123");
        assert_eq!(pf.config.name, "restricted-agent");
        assert!(pf.config.tool_rules.contains_key("github"));
        let github = &pf.config.tool_rules["github"];
        assert!(github.filter.allow.is_some());
    }

    #[test]
    fn policy_file_without_tool_rules_deserializes() {
        let json = r#"{
            "id": "spend-only",
            "name": "Spend Limit",
            "daily_limit": 5000000,
            "executable": "bitrouter policy eval",
            "created_at": "2026-04-10T00:00:00Z"
        }"#;
        let pf: PolicyFile = serde_json::from_str(json).unwrap();
        assert_eq!(pf.config.daily_limit, Some(5_000_000));
        assert!(pf.config.tool_rules.is_empty());
    }
}
