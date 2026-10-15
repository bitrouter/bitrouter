//! Persisted attempt observations shared by gateway accounting and learning.

use super::session_identity::SessionIdentity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const EXECUTION_KIND: &str = "execution";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHop {
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
    pub protocol: String,
    pub status: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Completed,
    Failed,
    ClientDisconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionContext {
    /// The effective selector at the admission boundary, before this controller
    /// changes it. This never contains a post-hoc task classification or score.
    pub selector: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteIntent {
    pub request_id: String,
    pub identity: SessionIdentity,
    pub context: DecisionContext,
    pub selected_route: String,
    pub applied_block: Option<String>,
    pub reason: String,
    pub bypass: Option<String>,
    pub mode_epoch: u64,
    pub control_generation: u64,
    /// Joint assignment, including blocks that did not match this request. The
    /// session's eventual outcome is not copied once per intent.
    pub assignments: BTreeMap<String, String>,
    #[serde(default)]
    pub monitoring: BTreeMap<String, String>,
    pub captured_watermark: i64,
    pub routing_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub archived_routing_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub trial_ready: bool,
    pub recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSettlement {
    pub settled_at: String,
    pub hops: Vec<ExecutionHop>,
    pub final_model: String,
    pub final_provider: String,
    pub error_code: Option<String>,
    pub duration_ms: u64,
    /// Filled by the terminal observer, including disconnects without an error.
    pub outcome: Option<ExecutionOutcome>,
    /// Metering's final observation; multiple attempts may have additional cost.
    pub final_metered_cost_micro_usd: Option<u64>,
    /// Complete request cost only when every attempted generation is covered.
    pub total_cost_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub request_id: String,
    pub attempt_id: String,
    pub identity: SessionIdentity,
    pub connection_id: String,
    pub captured_watermark: i64,
    pub started_at: String,
    pub intent: Option<RouteIntent>,
    /// Effective route after the arm's request-capability guard.
    pub dispatch_route: String,
    pub route_guard_reason: Option<String>,
    /// None is an unfinished or interrupted request, never a zero-cost success.
    pub settlement: Option<ExecutionSettlement>,
}
