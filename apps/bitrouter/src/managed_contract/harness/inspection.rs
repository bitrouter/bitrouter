//! Public harness inspection contracts. Execution and storage remain application-owned.

use bitrouter_ai::policy::{NextStep, TaskFamily};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub run_id: String,
    pub tasks: Vec<TaskView>,
    pub tasks_truncated: bool,
    pub evidence_blocks: usize,
    pub retained_evidence_bytes: usize,
    pub decisions: DecisionUsage,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: String,
    pub agent_id: String,
    pub parent_task_id: Option<String>,
    pub text_preview: String,
    pub status: AgentStatus,
    pub view_id: Option<String>,
    pub prompt_bytes: Option<usize>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub routing_reason: Option<String>,
    pub source_groups: usize,
    pub omitted_groups: usize,
    pub result_references: usize,
    /// Current-step presentation facts; absent in older observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingStatus>,
}

/// Public progress from the current task's acknowledged execution state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingStatus {
    pub phase: RoutingPhase,
    /// Accepted classifier labels, including explicit abstention as unknown.
    pub workflow: Option<Workflow>,
    /// Current serving attempt, or the selected plan before dispatch.
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPhase {
    Preparing,
    Classifying,
    Routing,
    Generating,
    Tools,
    Waiting,
    Cancelling,
    Finished,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub task_family: TaskFamily,
    pub next_step_role: NextStep,
    pub progress_state: ProgressState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub attempts: usize,
    pub pending: usize,
    pub failures: usize,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Frozen operator-price estimate, never a reported or reconciled bill.
    pub estimated_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Runnable,
    ModelRunning,
    WaitingTool,
    WaitingMessage,
    WaitingMaterial,
    Cancelling,
    Interrupted,
    RecoveryRequired,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Observed or predicted progress, with explicit uncertainty.
pub enum ProgressState {
    /// Work is beginning.
    Opening,
    /// Work is making progress.
    Progressing,
    /// Progress has stopped.
    Stalled,
    /// Work is recovering from an unsuccessful operation.
    Recovering,
    /// Work is approaching completion.
    NearDone,
    /// Insufficient information to choose a semantic label.
    Unknown,
}

impl AgentStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Completed | Self::Failed | Self::Cancelled
        )
    }
}

impl RoutingPhase {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Classifying => "classifying",
            Self::Routing => "routing",
            Self::Generating => "generating",
            Self::Tools => "tools",
            Self::Waiting => "waiting",
            Self::Cancelling => "cancelling",
            Self::Finished => "finished",
        }
    }
}

impl ProgressState {
    /// Stable policy/evidence label.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Progressing => "progressing",
            Self::Stalled => "stalled",
            Self::Recovering => "recovering",
            Self::NearDone => "near_done",
            Self::Unknown => "unknown",
        }
    }
}

redacted_contract_debug!(Inspection, TaskView, RoutingStatus);
