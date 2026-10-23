//! Request contracts for BitRouter's managed Responses HTTP profile.
//!
//! This is a distinct ingress contract from ordinary model Responses requests.
//! Session binding and durable checkpoint acknowledgement use the core channel;
//! these values neither establish execution authority nor execute tools.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::protocol::{
    ArtifactRef, DiscardableHistory, Limits, RoutingSettings, ToolOutcome, Verification,
};

/// A managed response creation or continuation, with the existing strict fields.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub model: String,
    /// Initial text or a list of [`FunctionOutput`] values for a continuation.
    /// Interpretation and admission belong to the service.
    pub input: Value,
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub stream: bool,
    pub multi_agent: MultiAgent,
    pub bitrouter: ManagedExecution,
    pub reasoning: Option<Reasoning>,
    pub max_output_tokens: Option<u32>,
}

/// Explicit multi-agent opt-in and an optional per-request concurrency bound.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MultiAgent {
    pub enabled: bool,
    pub max_concurrent_subagents: Option<u32>,
}

/// Requested reasoning effort; provider selection and validation stay in the service.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reasoning {
    pub effort: String,
}

/// Identity and requested policy for an already bound managed session.
/// The service checks the version, `managed` execution value and all authority.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExecution {
    pub version: u32,
    pub execution: String,
    pub session_id: String,
    pub execution_epoch: u64,
    pub operation_id: String,
    pub expected_state_revision: Option<u64>,
    pub routing: Option<RoutingSettings>,
    pub limits: Option<Limits>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub required_materials: Vec<String>,
    pub verification: Option<Verification>,
    pub discardable_history: Option<DiscardableHistory>,
}

/// Caller-supplied result metadata, subject to the session's existing tool fences.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultMetadata {
    pub operation_id: String,
    pub status: ToolOutcome,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    pub workspace_revision: Option<String>,
}

/// A `function_call_output` continuation item. Constructing this does not execute
/// or acknowledge a tool; the harness supplies its actual result and operation ID.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub call_id: String,
    pub output: String,
    pub bitrouter: ResultMetadata,
}
