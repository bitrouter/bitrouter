//! Native Decisions service response contracts; no generation conversion.

use serde::{Deserialize, Serialize};

/// The native Decisions response, preserving its original wire usage.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionsResponse {
    pub model: String,
    pub answers: Vec<crate::decisions::DecisionAnswer>,
    pub usage: DecisionsUsage,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Required native Decisions usage fields and additive provider details.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionsUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub input_tokens_details: DecisionsInputTokenDetails,
    pub output_tokens_details: DecisionsOutputTokenDetails,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Required native cached-input counts.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionsInputTokenDetails {
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Required native reasoning-token count.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionsOutputTokenDetails {
    pub reasoning_tokens: u64,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

redacted_contract_debug!(
    DecisionsResponse,
    DecisionsUsage,
    DecisionsInputTokenDetails,
    DecisionsOutputTokenDetails
);
