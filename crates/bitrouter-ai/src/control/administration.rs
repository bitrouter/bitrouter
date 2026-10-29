//! Bounded provider, agent, observation and policy inspection data.

use crate::routing::PolicyModelTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyView {
    #[default]
    Active,
    Disk,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    #[serde(default)]
    pub view: PolicyView,
    pub name: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    pub models: usize,
    pub active: bool,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersReport {
    pub resolved_via: String,
    pub providers: Vec<ProviderEntry>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub id: String,
    pub configured: bool,
    pub in_catalog: bool,
    pub description: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsReport {
    #[serde(default)]
    pub skill_delivery: std::collections::BTreeMap<String, String>,
    pub resolved_via: String,
    pub readiness_checked: bool,
    pub agents: Vec<AgentEntry>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveReport {
    pub daemon_reachable: bool,
    pub compiled_in: bool,
    pub exporter_wired: bool,
    pub sampler: Option<String>,
    pub sampler_arg: Option<f64>,
    pub metrics_enabled: bool,
    pub header_count: usize,
    pub resource_attribute_count: usize,
    pub api_key_count: usize,
    pub api_key_cap: usize,
    pub user_id_count: usize,
    pub user_id_cap: usize,
    pub active_spans: usize,
}

/// Explicitly selected policy fields; never serialize the source document.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDetail {
    pub tiers: BTreeMap<String, PolicyModelTarget>,
    pub routes: BTreeMap<String, String>,
    pub default_tier: Option<String>,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: Vec<String>,
    pub certificates: BTreeMap<String, CertificateIdentity>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificateIdentity {
    pub selected_tier: String,
    pub evidence_digest: String,
    pub compiler_config_digest: String,
    pub evaluator_config_digest: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyReport {
    pub view: PolicyView,
    pub availability: String,
    pub digest: Option<String>,
    pub mode: String,
    pub policies: Vec<String>,
    pub bindings: BTreeMap<String, String>,
    pub evidence_root: Option<String>,
    pub eval_snapshot_root: Option<String>,
    pub definitions: BTreeMap<String, PolicyDetail>,
}

impl PolicyInput {
    pub fn validate(&self) -> Result<(), InvalidIdentifier> {
        if let Some(name) = &self.name {
            validate_identifier(name)?;
        }
        Ok(())
    }
}

/// An identifier outside the existing control API grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("identifier must contain 1–256 bytes and no control characters")]
pub struct InvalidIdentifier;

/// Check the bounded identifier grammar without consulting a service.
pub fn validate_identifier(value: &str) -> Result<(), InvalidIdentifier> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(InvalidIdentifier)
    } else {
        Ok(())
    }
}
