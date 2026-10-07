//! Source-independent semantic workflow labels shared by routing adapters.
//! A transport, runtime name or session identifier is not a semantic label.

use serde::{Deserialize, Serialize};

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
