//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::core::protocol::MaterialRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalState {
    pub revision: u64,
    pub observed_at: Option<String>,
    pub source: Option<String>,
    pub materials: BTreeMap<String, MaterialRef>,
    /// Retain immutable identities even if a later inventory omits a version.
    /// Content bodies stay in the current inventory and frozen model steps.
    pub versions: BTreeMap<String, BTreeMap<String, MaterialRef>>,
    pub facts: BTreeMap<String, Value>,
    pub requests: BTreeMap<String, MaterialRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterialRequest {
    pub request_id: String,
    /// The first consumer owns a shared fetch even after its turn is retired.
    /// Older checkpoints without attribution cannot establish cost coverage.
    #[serde(default)]
    pub origin: Option<MaterialRequestOrigin>,
    pub signal_revision: u64,
    pub reference: MaterialRef,
    pub resolved: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialRequestOrigin {
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
}
