//! Public contracts for the authenticated administration control service.
//!
//! These credentials and scopes are independent of inference authorization.

pub mod reload;

use serde::{Deserialize, Serialize};

/// Current HTTP control protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Authority granted by a host operator, independently of inference credentials.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ControlScope {
    /// Host-wide operational inspection, including usage and policy metadata.
    #[serde(rename = "control:read")]
    Read,
    /// Reload server-owned configuration; requires the read grant as well.
    #[serde(rename = "control:reload")]
    Reload,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDescriptor {
    pub version: u32,
    pub required_scope: ControlScope,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesReport {
    #[serde(default)]
    pub resources: std::collections::BTreeMap<String, ResourceDescriptor>,
    #[serde(default)]
    pub limits: std::collections::BTreeMap<String, u64>,
    pub protocol: String,
    pub protocol_version: u32,
    pub server_version: String,
    pub actions: Vec<String>,
    #[serde(default)]
    pub action_descriptors: std::collections::BTreeMap<String, ActionDescriptor>,
    #[serde(default)]
    pub server_instance_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<ControlScope>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    pub version: u32,
    pub path: String,
    pub required_scope: ControlScope,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReport {
    /// Reload-coordinator state sampled after `config_state`. The nested
    /// configuration evidence carries its own instance and generation; callers
    /// must use those fields when a concurrent reload separates the samples.
    #[serde(flatten)]
    pub reload: reload::ReloadState,
    pub live_policy_digest: Option<String>,
    /// Redaction-safe evidence from this server's own saved and running
    /// configuration. Absent when an older or limited reloader cannot inspect
    /// both sides; absence never implies that they match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_state: Option<reload::ConfigurationState>,
}
