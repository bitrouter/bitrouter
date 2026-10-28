//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use bitrouter_ai::harness::inspection::AgentStatus;
pub mod accounting;
pub mod allocation;
pub mod collaboration;
pub mod context;
pub mod reconstruction;
pub mod release;
pub mod responses;
pub mod root_queue;
pub mod routing;
pub mod signals;
pub mod steering;

use self::allocation::{ContextAllocation, ContextSource};
use self::collaboration::{Assignment, Call, Mail, RuntimeWait};
use self::routing::{ContextManifest, DecisionApplied, ExecutionReceipt, RoutingDecision};
use self::signals::SignalState;
use crate::gateway::pipeline::native::{
    NativeContextValidationReport, NativeInputCountReport, NativePlan,
};
use bitrouter_ai::core::protocol::{
    CoreError, HarnessManifest, Limits, MaterialRef, OperationReceipt, TaskInput, ToolExecute,
    ToolResult,
};
use bitrouter_ai::types::Message;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Waiting,
    Cancelling,
    RecoveryRequired,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub index: u32,
    pub receipt: Option<ExecutionReceipt>,
    /// Frozen canonical-result allowance reserved before provider dispatch.
    /// Missing legacy fields do not create a retrospective output contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_output_bytes: Option<u64>,
    /// Version 2 reserves canonical delivery; version 3 also bounds complete reports.
    /// Absence retains the original policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_output_version: Option<u32>,
    /// Complete report contract, including bounded metadata-rejection evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_report_bytes: Option<u64>,
    /// Actual integration work, including internal HTTP authentication retries.
    /// Empty legacy/custom-executor records do not prove complete coverage.
    #[serde(default)]
    pub provider_work: Vec<ProviderWorkRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderWorkRecord {
    pub work: crate::gateway::pipeline::native_work::NativeProviderWork,
    pub report: Option<crate::gateway::pipeline::native_work::NativeProviderWorkReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparationWorkRecord {
    pub work: crate::gateway::pipeline::native_preparation::NativePreparationWork,
    pub report: Option<crate::gateway::pipeline::native_preparation::NativePreparationWorkReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCountRecord {
    pub route_index: u32,
    pub report: Option<NativeInputCountReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextValidationRecord {
    pub request_id: String,
    /// True only when the allowed candidate also passed the current source gate.
    #[serde(default)]
    pub applied: bool,
    pub report: Option<NativeContextValidationReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStep {
    /// Version 1 reserves complete auxiliary reports and bounded failure evidence.
    /// A missing marker preserves the legacy contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_output_version: Option<u32>,
    /// Oversized terminal diagnostic, hashed as a serde JSON string. The
    /// readable original is not retained; terminal_reason explains the omission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_diagnostic: Option<bitrouter_ai::core::evidence::NativeEvidenceCommitment>,
    pub step_id: String,
    pub decision_id: String,
    pub context_revision: u64,
    pub signal_revision: u64,
    pub manifest: HarnessManifest,
    pub materials: Vec<MaterialRef>,
    pub context: ContextManifest,
    pub input_state_revision: u64,
    /// Complete paired history before material injection or app transforms.
    pub input_history: Vec<Message>,
    pub decision: Option<RoutingDecision>,
    pub application: Option<DecisionApplied>,
    pub plan: Option<NativePlan>,
    #[serde(default)]
    pub count_plan: Option<NativePlan>,
    #[serde(default)]
    pub input_counts: Vec<InputCountRecord>,
    #[serde(default)]
    pub rebuild: Option<crate::storage::checkpoint::snapshot::reconstruction::RebuildRecord>,
    #[serde(default)]
    pub reconstructed_from: Option<String>,
    #[serde(default)]
    pub context_validation: Option<ContextValidationRecord>,
    /// Configured App/pipeline callbacks; empty legacy entries do not establish
    /// that preparation performed no work or incurred no expense.
    #[serde(default)]
    pub preparation_work: Vec<PreparationWorkRecord>,
    pub attempts: Vec<AttemptRecord>,
    pub settled: bool,
    /// The driver was lost without applied output. Closing the step does not
    /// establish an outcome for any of its outstanding cost records.
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub dispatch: ToolExecute,
    pub public_call_id: String,
    pub provider_call_id: String,
    pub result: Option<ToolResult>,
    pub consumed: bool,
    pub result_limit_bytes: u64,
    /// Frozen archive growth for each first essential recovery observation.
    /// Absence preserves an already authorized legacy reply contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_archive_allowance: Option<u64>,
    pub effect: bitrouter_ai::core::protocol::ToolEffect,
    pub signal_revision: u64,
    /// Workspace version used to authorize this invocation, including explicit
    /// unknown. Legacy snapshots are additionally fenced by restoration.
    #[serde(default)]
    pub workspace_revision: Option<String>,
    /// An uncertain observation retained when authenticated restoration later
    /// supplies a definite outcome. Confirmed results cannot be overwritten.
    #[serde(default)]
    pub prior_uncertain_result: Option<ToolResult>,
    /// Fresh harness evidence retained by the latest restoration, not an
    /// authorization to infer the outcome of a later crash.
    #[serde(default)]
    pub recovery_observation: Option<bitrouter_ai::core::protocol::ToolObservation>,
    /// Revision at which restoration accepted the fresh harness observation.
    #[serde(default)]
    pub recovery_observation_revision: u64,
    /// Superseded restore evidence remains available for irreversible execution
    /// facts and artifact retention. The snapshot byte limit bounds this list.
    #[serde(default)]
    pub prior_recovery_observations: Vec<bitrouter_ai::core::protocol::ToolObservation>,
    /// Immutable observations keyed by operation ID, including new uncertainty
    /// episodes. The complete snapshot remains bounded by checkpoint limits.
    #[serde(default)]
    pub tool_observations: BTreeMap<String, bitrouter_ai::core::protocol::ToolObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRun {
    pub run_id: String,
    pub agent_turn_id: String,
    pub input: TaskInput,
    pub limits: Limits,
    pub status: RunStatus,
    pub model_attempts: u32,
    /// None for legacy snapshots without complete run-wide accounting evidence.
    #[serde(default)]
    pub token_accounting:
        Option<crate::storage::checkpoint::snapshot::accounting::RunTokenAccounting>,
    pub active_ms: u64,
    /// Immutable authenticated clock handoffs for this run. Each source head
    /// precedes the session.restored checkpoint that accepted the measurement.
    #[serde(default)]
    pub activity_reconciliations: Vec<bitrouter_ai::core::protocol::RunActivityReconciliation>,
    /// A committed resource failure stops new work while outcomes and cleanup
    /// remain admissible. It does not turn unknown effects into known results.
    #[serde(default)]
    pub resource_error: Option<CoreError>,
    /// Missing on legacy active-time failures. Capacity failures retain an
    /// explicit cause so restoration never treats them as an exhausted clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_constraint: Option<ResourceConstraint>,
    pub cancellation: Option<String>,
    pub final_answer: Option<String>,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceConstraint {
    ActiveTime,
    CheckpointCapacity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurn {
    pub run_id: String,
    pub agent_turn_id: String,
    pub assigned_by: String,
    pub input: TaskInput,
    pub allocation_id: Option<String>,
    /// Start of this work unit's mandatory history. Unknown legacy boundaries
    /// cannot authorize removal. Inherited contexts start at zero.
    #[serde(default)]
    pub history_start: Option<usize>,
    pub status: AgentStatus,
    /// Cancellation survives a temporary RecoveryRequired status, including
    /// subtree interruption without cancellation of the root run.
    #[serde(default)]
    pub cancellation_requested: bool,
    pub steps: Vec<ModelStep>,
    pub invocations: Vec<Invocation>,
    pub core_calls: Vec<Call>,
    pub final_answer: Option<String>,
    pub terminal_reason: Option<String>,
    pub notified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub display_path: String,
    pub depth: u32,
    pub context_revision: u64,
    pub history: Vec<Message>,
    pub required_instructions: Vec<String>,
    pub turn: Option<AgentTurn>,
    pub queue: VecDeque<Assignment>,
    pub mailbox: Vec<Mail>,
    pub task_scope: Option<String>,
    pub context_sources: Vec<ContextSource>,
    pub last_scheduled: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub agent_id: String,
    pub manifest: HarnessManifest,
    pub agents: BTreeMap<String, AgentState>,
    pub run: Option<RootRun>,
    pub operations: BTreeMap<String, OperationReceipt>,
    pub waits: BTreeMap<String, RuntimeWait>,
    pub signals: SignalState,
    pub allocations: BTreeMap<String, ContextAllocation>,
    /// Versioned opt-in task views. Missing legacy state conveys no authority
    /// to remove historical messages.
    #[serde(default)]
    pub context_store: crate::storage::checkpoint::snapshot::context::ContextStore,
    /// Run-wide cost exposure retained even after agents retire or runs change.
    /// Missing legacy entries mean unknown coverage, never a zero-cost run.
    #[serde(default)]
    pub cost_work:
        BTreeMap<String, crate::storage::checkpoint::snapshot::accounting::work::RunCostWork>,
    /// Late results are retained as evidence, never injected into agent history.
    #[serde(default)]
    pub provider_evidence: BTreeMap<String, bitrouter_ai::core::protocol::ProviderAttemptEvidence>,
    #[serde(default)]
    pub root_queue: crate::storage::checkpoint::snapshot::root_queue::RootQueue,
    #[serde(default)]
    pub steering: BTreeMap<String, crate::storage::checkpoint::snapshot::steering::SteeringRecord>,
    /// Committed ownership releases remain available across later grants.
    #[serde(default)]
    pub releases: BTreeMap<String, crate::storage::checkpoint::snapshot::release::ReleaseRecord>,
    /// Wire checkpoints may replace recovery histories with this immutable
    /// object. Public snapshots and scheduler state are always fully hydrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_archive: Option<bitrouter_ai::core::protocol::ArtifactRef>,
    /// Managed HTTP exchanges retain their own immutable completion boundary.
    #[serde(
        default,
        skip_serializing_if = "crate::storage::checkpoint::snapshot::responses::ResponseState::is_empty"
    )]
    pub responses: crate::storage::checkpoint::snapshot::responses::ResponseState,
}

impl SessionSnapshot {
    pub fn skill_selection(
        &self,
        agent_turn_id: &str,
    ) -> Option<&crate::storage::skills::SkillSelection> {
        self.context_store.work.get(agent_turn_id)?.skills.as_ref()
    }
    pub fn root_turn(&self) -> Option<&AgentTurn> {
        self.agents
            .get(&self.agent_id)
            .and_then(|agent| agent.turn.as_ref())
    }
}

pub(crate) fn pending_dependencies(state: &SessionSnapshot, agent_id: &str) -> bool {
    let descendants = collaboration::subtree(state, agent_id);
    state.agents.values().any(|agent| {
        if agent.agent_id == agent_id {
            return agent.mailbox.iter().any(|mail| !mail.consumed);
        }
        let descendant = descendants.contains(&agent.agent_id);
        agent
            .queue
            .iter()
            .any(|work| descendant || work.sender_id == agent_id)
            || agent.turn.as_ref().is_some_and(|turn| {
                (descendant || turn.assigned_by == agent_id)
                    && (!turn.status.terminal() || !turn.notified)
            })
    })
}
