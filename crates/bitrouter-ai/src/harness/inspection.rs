//! Public harness inspection contracts. Execution and storage remain application-owned.

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
    pub next_step_role: NextStepRole,
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
/// Semantic role of the next execution step.
pub enum NextStepRole {
    /// Coordinate or decompose work.
    Orchestrate,
    /// Produce or modify the solution.
    Implement,
    /// Perform a narrow, well-specified operation.
    Mechanical,
    /// Check the solution against requirements.
    Verify,
    /// Deliver the completed result.
    Finalize,
    /// Insufficient information to choose a semantic label.
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Task intent inferred from admitted content or supplied task facts.
pub enum TaskFamily {
    /// Generate or implement code.
    CodeGeneration,
    /// Diagnose and fix a code defect.
    CodeDebugging,
    /// Review code and report findings.
    CodeReview,
    /// Work on SQL or database behavior.
    CodeSqlDatabase,
    /// Work on frontend or user interface behavior.
    CodeFrontendUi,
    /// Work on deployment or configuration.
    CodeDevopsConfig,
    /// Analyze a repository.
    CodeRepositoryAnalysis,
    /// Plan a sequence of dependent work.
    AgentMultiStepPlanning,
    /// Carry out a workflow.
    AgentWorkflowExecution,
    /// Research external information.
    AgentWebResearch,
    /// Manage retained context or memory.
    AgentMemoryOperations,
    /// General agent work.
    AgentGeneral,
    #[default]
    /// Insufficient information to choose a semantic label.
    Unknown,
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

impl NextStepRole {
    /// Stable policy/evidence label.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Orchestrate => "orchestrate",
            Self::Implement => "implement",
            Self::Mechanical => "mechanical",
            Self::Verify => "verify",
            Self::Finalize => "finalize",
            Self::Unknown => "unknown",
        }
    }

    /// Decode a canonical label, rejecting unknown spellings.
    pub fn parse_key(value: &str) -> Option<Self> {
        match value {
            "orchestrate" => Some(Self::Orchestrate),
            "implement" => Some(Self::Implement),
            "mechanical" => Some(Self::Mechanical),
            "verify" => Some(Self::Verify),
            "finalize" => Some(Self::Finalize),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl TaskFamily {
    /// Stable policy/evidence label.
    pub const fn key(self) -> &'static str {
        match self {
            Self::CodeGeneration => "code:generation",
            Self::CodeDebugging => "code:debugging",
            Self::CodeReview => "code:review",
            Self::CodeSqlDatabase => "code:sql_database",
            Self::CodeFrontendUi => "code:frontend_ui",
            Self::CodeDevopsConfig => "code:devops_config",
            Self::CodeRepositoryAnalysis => "code:repository_analysis",
            Self::AgentMultiStepPlanning => "agent:multi_step_planning",
            Self::AgentWorkflowExecution => "agent:workflow_execution",
            Self::AgentWebResearch => "agent:web_research",
            Self::AgentMemoryOperations => "agent:memory_operations",
            Self::AgentGeneral => "agent:general",
            Self::Unknown => "unknown",
        }
    }

    /// Decode a canonical label, rejecting unknown spellings.
    pub fn parse_key(value: &str) -> Option<Self> {
        match value {
            "code:generation" => Some(Self::CodeGeneration),
            "code:debugging" => Some(Self::CodeDebugging),
            "code:review" => Some(Self::CodeReview),
            "code:sql_database" => Some(Self::CodeSqlDatabase),
            "code:frontend_ui" => Some(Self::CodeFrontendUi),
            "code:devops_config" => Some(Self::CodeDevopsConfig),
            "code:repository_analysis" => Some(Self::CodeRepositoryAnalysis),
            "agent:multi_step_planning" => Some(Self::AgentMultiStepPlanning),
            "agent:workflow_execution" => Some(Self::AgentWorkflowExecution),
            "agent:web_research" => Some(Self::AgentWebResearch),
            "agent:memory_operations" => Some(Self::AgentMemoryOperations),
            "agent:general" => Some(Self::AgentGeneral),
            "unknown" => Some(Self::Unknown),
            _ => None,
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
