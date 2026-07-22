use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessId {
    Generic,
    Hermes,
    ClaudeCode,
    Codex,
    Smithers,
    #[serde(rename = "terminus_2", alias = "terminus-2", alias = "terminus2")]
    Terminus2,
    #[serde(alias = "openclaw")]
    OpenClaw,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolKind {
    ChatCompletions,
    Messages,
    Responses,
    OpenClawRuntime,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStateKind {
    Unknown,
    Opening,
    Planning,
    ToolFollowup,
    Edit,
    Test,
    Debug,
    Review,
    Recovery,
    SubagentDispatch,
    Finalization,
}

impl fmt::Display for WorkflowStateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&enum_key(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDensity {
    None,
    Low,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSizeBucket {
    Unknown,
    Small,
    Medium,
    Large,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoverySignal {
    None,
    LikelyRecovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementLevel {
    Unknown,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityConstraints {
    pub tool_reliability: RequirementLevel,
    pub code_reasoning: RequirementLevel,
    pub context_pressure: RequirementLevel,
    pub latency_sensitivity: RequirementLevel,
    pub expected_redo_penalty: RequirementLevel,
    pub output_precision: RequirementLevel,
    #[serde(default)]
    pub compatibility: Vec<String>,
}

impl Default for CapabilityConstraints {
    fn default() -> Self {
        Self {
            tool_reliability: RequirementLevel::Unknown,
            code_reasoning: RequirementLevel::Unknown,
            context_pressure: RequirementLevel::Unknown,
            latency_sensitivity: RequirementLevel::Unknown,
            expected_redo_penalty: RequirementLevel::Unknown,
            output_precision: RequirementLevel::Unknown,
            compatibility: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfidence {
    #[default]
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSignal {
    pub key: Option<String>,
    pub confidence: SessionConfidence,
    pub source: Option<String>,
}

/// Terminus 2 agent role within one context-compaction workflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Primary task-solving agent.
    Main,
    /// First compaction subagent, producing a summary.
    Summary,
    /// Second compaction subagent, asking clarification questions.
    Questions,
    /// Third compaction subagent, answering those questions.
    Answers,
    /// Role could not be identified safely.
    #[default]
    Unknown,
}

impl AgentRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Summary => "summary",
            Self::Questions => "questions",
            Self::Answers => "answers",
            Self::Unknown => "unknown",
        }
    }
}

/// Transition represented by the current request in a context epoch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTransition {
    /// No context transition on this request.
    #[default]
    None,
    /// A summary subagent opened a new compacted context epoch.
    CompactionStart,
    /// A question/answer subagent continued the active compaction.
    CompactionContinuation,
    /// The main agent resumed inside the compacted context.
    MainResume,
}

/// Structured workflow/session identity used for attribution and joins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowIdentity {
    /// Immutable benchmark run identifier, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_run_id: Option<String>,
    /// Immutable benchmark trial identifier, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_id: Option<String>,
    /// Agent invocation identity, explicit or deterministically derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// Parent task session shared across compaction subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Agent role within the parent workflow.
    pub role: AgentRole,
    /// Monotonic context-compaction epoch within the parent workflow.
    pub context_epoch: u32,
    /// Transition represented by this request.
    pub transition: ContextTransition,
    /// Stable digest of run, trial, parent session, and context epoch.
    pub fingerprint: String,
    /// Identity source (`explicit_headers` or `inferred`).
    pub source: String,
    /// Confidence inherited from the parent session signal.
    pub confidence: SessionConfidence,
}

impl Default for SessionSignal {
    fn default() -> Self {
        Self {
            key: None,
            confidence: SessionConfidence::None,
            source: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    Observed,
    Inferred,
    DocumentedStub,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: String,
    pub value: String,
    pub confidence: f32,
    pub level: EvidenceLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStateIR {
    pub harness_id: HarnessId,
    pub protocol: ProtocolKind,
    pub state_kind: WorkflowStateKind,
    pub active_workflow: Option<String>,
    pub subagent_role: Option<String>,
    pub last_tool_name: Option<String>,
    pub tool_density: ToolDensity,
    pub context_size: ContextSizeBucket,
    pub recovery_signal: RecoverySignal,
    pub capability_constraints: CapabilityConstraints,
    pub session: SessionSignal,
    #[serde(default)]
    pub identity: WorkflowIdentity,
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl WorkflowStateIR {
    pub fn routing_key(&self) -> String {
        let mut compatibility = self.capability_constraints.compatibility.clone();
        compatibility.sort();
        [
            enum_key(&self.harness_id),
            enum_key(&self.protocol),
            enum_key(&self.state_kind),
            option_key(self.active_workflow.as_deref()),
            option_key(self.subagent_role.as_deref()),
            option_key(self.last_tool_name.as_deref()),
            enum_key(&self.tool_density),
            enum_key(&self.context_size),
            enum_key(&self.recovery_signal),
            enum_key(&self.capability_constraints.tool_reliability),
            enum_key(&self.capability_constraints.code_reasoning),
            enum_key(&self.capability_constraints.context_pressure),
            enum_key(&self.capability_constraints.latency_sensitivity),
            enum_key(&self.capability_constraints.expected_redo_penalty),
            enum_key(&self.capability_constraints.output_precision),
            compatibility.join(","),
        ]
        .join("|")
    }
}

fn option_key(value: Option<&str>) -> String {
    value.unwrap_or("-").to_ascii_lowercase()
}

fn enum_key<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"unknown\"".to_string())
        .trim_matches('"')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ir() -> WorkflowStateIR {
        WorkflowStateIR {
            harness_id: HarnessId::Hermes,
            protocol: ProtocolKind::ChatCompletions,
            state_kind: WorkflowStateKind::ToolFollowup,
            active_workflow: Some("superpowers:test-driven-development".to_string()),
            subagent_role: None,
            last_tool_name: Some("bash".to_string()),
            tool_density: ToolDensity::High,
            context_size: ContextSizeBucket::Medium,
            recovery_signal: RecoverySignal::LikelyRecovery,
            capability_constraints: CapabilityConstraints {
                tool_reliability: RequirementLevel::High,
                code_reasoning: RequirementLevel::Medium,
                context_pressure: RequirementLevel::Medium,
                latency_sensitivity: RequirementLevel::Low,
                expected_redo_penalty: RequirementLevel::High,
                output_precision: RequirementLevel::Medium,
                compatibility: vec!["requires_structured_tools".to_string()],
            },
            session: SessionSignal {
                key: Some("job-123".to_string()),
                confidence: SessionConfidence::Medium,
                source: Some("fixture.job_id".to_string()),
            },
            identity: WorkflowIdentity::default(),
            confidence: 0.86,
            evidence: vec![
                Evidence {
                    kind: "last_tool".to_string(),
                    value: "bash".to_string(),
                    confidence: 0.9,
                    level: EvidenceLevel::Observed,
                },
                Evidence {
                    kind: "error_marker".to_string(),
                    value: "exit code 1".to_string(),
                    confidence: 0.8,
                    level: EvidenceLevel::Inferred,
                },
            ],
        }
    }

    #[test]
    fn workflow_state_ir_serializes_with_stable_field_names() {
        let value = serde_json::to_value(sample_ir()).unwrap();
        assert!(value.get("state_kind").is_some());
        assert!(value.get("harness_id").is_some());
        assert!(value.get("protocol").is_some());
        assert!(value.get("capability_constraints").is_some());
        assert!(value.get("session").is_some());
    }

    #[test]
    fn workflow_state_fingerprint_ignores_evidence_order() {
        let first = sample_ir();
        let mut second = sample_ir();
        second.evidence.reverse();
        assert_eq!(first.routing_key(), second.routing_key());
    }

    #[test]
    fn capability_constraints_are_model_agnostic() {
        let value = serde_json::to_string(&sample_ir().capability_constraints).unwrap();
        assert!(!value.contains("model"));
        assert!(!value.contains("tier"));
    }

    #[test]
    fn workflow_identity_does_not_change_routing_key() {
        let first = sample_ir();
        let mut second = first.clone();
        second.identity = WorkflowIdentity {
            parent_session_id: Some("parent".to_string()),
            role: AgentRole::Summary,
            context_epoch: 7,
            fingerprint: "sha256:test".to_string(),
            ..Default::default()
        };
        assert_eq!(first.routing_key(), second.routing_key());
    }
}
