//! Public HTTP gateway discovery and error contracts.

pub mod execution;

use serde::{Deserialize, Serialize};

/// The model discovery response; optional client-specific additions are retained.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelEntry>,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A configured model route and its provider names.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub providers: Vec<String>,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The ordinary gateway health response.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HealthResponse {
    pub status: String,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Gateway error response. Raw upstream bad-request values remain representable.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayErrorEnvelope {
    pub error: GatewayErrorValue,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Structured gateway failure information.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GatewayErrorDetail {
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::protocol::present_nullable"
    )]
    #[serde(rename = "type")]
    pub kind: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::protocol::present_nullable"
    )]
    pub code: Option<Option<serde_json::Value>>,
    /// Additional wire fields, retained without provider interpretation.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Either a structured service error or the original upstream bad-request value.
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum GatewayErrorValue {
    Detail(GatewayErrorDetail),
    Other(serde_json::Value),
}

redacted_contract_debug!(
    ModelsResponse,
    ModelEntry,
    HealthResponse,
    GatewayErrorEnvelope,
    GatewayErrorDetail,
    GatewayErrorValue
);
