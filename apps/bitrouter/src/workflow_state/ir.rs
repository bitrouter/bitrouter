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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteRisk {
    Normal,
    Context,
    Guarded,
}

impl fmt::Display for RouteRisk {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedActionKind {
    Read,
    Mutate,
    Test,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedActionHistory {
    pub last_action: Option<NormalizedActionKind>,
    pub last_failed: bool,
    pub failure_count: u8,
    pub mutation_count: u8,
    pub complete: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteProjection {
    pub schema_version: u8,
    pub state_kind: WorkflowStateKind,
    pub risk: RouteRisk,
}

impl RouteProjection {
    pub fn key(&self) -> String {
        format!(
            "agent_trace/v{}|{}|{}",
            self.schema_version, self.state_kind, self.risk
        )
    }

    /// Parse a canonical policy/ledger key without recovering information from
    /// legacy source-specific keys.
    pub fn parse_key(value: &str) -> Option<Self> {
        let mut segments = value.split('|');
        let (Some(namespace_version), Some(state), Some(risk), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return None;
        };
        let schema_version = match namespace_version {
            "agent_trace/v1" => 1,
            "agent_trace/v2" => 2,
            _ => return None,
        };
        let risk = parse_route_risk(risk)?;
        if schema_version == 1 && risk == RouteRisk::Context {
            return None;
        }
        Some(Self {
            schema_version,
            state_kind: parse_workflow_state_kind(state)?,
            risk,
        })
    }

    /// Whether `value` is a persistence-safe learning key. A policy namespace
    /// may prefix the canonical projection with one NUL separator; the suffix
    /// is always validated and never inferred from legacy routing data.
    pub fn is_canonical_learning_key(value: &str) -> bool {
        match value.rsplit_once('\0') {
            Some((namespace, projection)) => {
                !namespace.is_empty()
                    && !namespace.contains('\0')
                    && Self::parse_key(projection).is_some()
            }
            None => Self::parse_key(value).is_some(),
        }
    }
}

fn parse_workflow_state_kind(value: &str) -> Option<WorkflowStateKind> {
    match value {
        "unknown" => Some(WorkflowStateKind::Unknown),
        "opening" => Some(WorkflowStateKind::Opening),
        "planning" => Some(WorkflowStateKind::Planning),
        "tool_followup" => Some(WorkflowStateKind::ToolFollowup),
        "edit" => Some(WorkflowStateKind::Edit),
        "test" => Some(WorkflowStateKind::Test),
        "debug" => Some(WorkflowStateKind::Debug),
        "review" => Some(WorkflowStateKind::Review),
        "recovery" => Some(WorkflowStateKind::Recovery),
        "subagent_dispatch" => Some(WorkflowStateKind::SubagentDispatch),
        "finalization" => Some(WorkflowStateKind::Finalization),
        _ => None,
    }
}

pub(crate) fn parse_route_risk(value: &str) -> Option<RouteRisk> {
    match value {
        "normal" => Some(RouteRisk::Normal),
        "context" => Some(RouteRisk::Context),
        "guarded" => Some(RouteRisk::Guarded),
        _ => None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_action_history: Option<NormalizedActionHistory>,
    pub capability_constraints: CapabilityConstraints,
    pub session: SessionSignal,
    #[serde(default)]
    pub identity: WorkflowIdentity,
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl WorkflowStateIR {
    /// Detailed legacy evidence key. New policy routing uses [`Self::route_projection`].
    pub fn legacy_routing_key(&self) -> String {
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

    pub fn route_projection(&self) -> RouteProjection {
        let hard_guarded_state = matches!(
            self.state_kind,
            WorkflowStateKind::Unknown
                | WorkflowStateKind::Debug
                | WorkflowStateKind::Recovery
                | WorkflowStateKind::Finalization
        );
        let hard_guarded_constraint = matches!(
            self.capability_constraints.expected_redo_penalty,
            RequirementLevel::High
        ) || matches!(
            self.capability_constraints.output_precision,
            RequirementLevel::High
        );
        let risk = if self.recovery_signal == RecoverySignal::LikelyRecovery
            || hard_guarded_state
            || hard_guarded_constraint
        {
            RouteRisk::Guarded
        } else if matches!(
            self.capability_constraints.context_pressure,
            RequirementLevel::High
        ) {
            RouteRisk::Context
        } else {
            RouteRisk::Normal
        };

        RouteProjection {
            schema_version: 2,
            state_kind: self.state_kind.clone(),
            risk,
        }
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
            normalized_action_history: None,
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
        assert_eq!(first.legacy_routing_key(), second.legacy_routing_key());
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
        assert_eq!(first.legacy_routing_key(), second.legacy_routing_key());
    }

    #[test]
    fn route_projection_key_is_shared_by_equivalent_harnesses() {
        let harnesses = [
            HarnessId::Codex,
            HarnessId::ClaudeCode,
            HarnessId::Hermes,
            HarnessId::Terminus2,
            HarnessId::OpenClaw,
            HarnessId::Smithers,
            HarnessId::Generic,
        ];

        for harness_id in harnesses {
            let mut ir = sample_ir();
            ir.harness_id = harness_id;
            ir.protocol = ProtocolKind::Responses;
            ir.state_kind = WorkflowStateKind::Edit;
            ir.recovery_signal = RecoverySignal::None;
            ir.capability_constraints = CapabilityConstraints::default();

            assert_eq!(ir.route_projection().key(), "agent_trace/v2|edit|normal");
        }
    }

    #[test]
    fn route_projection_parser_accepts_canonical_v1_and_v2_keys() {
        let projection = RouteProjection::parse_key("agent_trace/v1|tool_followup|normal")
            .expect("canonical agent trace projection");
        assert_eq!(projection.key(), "agent_trace/v1|tool_followup|normal");
        assert!(RouteProjection::is_canonical_learning_key(
            "coding\0agent_trace/v1|tool_followup|normal"
        ));

        for key in [
            "codex|responses|tool_followup|-|-|exec_command",
            "agent_trace/v3|tool_followup|normal",
            "agent_trace/v1|tool_followup|context",
            "agent_trace/v1|not_a_state|normal",
            "agent_trace/v1|tool_followup|not_a_risk",
            "agent_trace/v1|tool_followup|normal|extra",
            "coding\0agent_trace/v1|tool_followup|normal|extra",
            "coding\0codex|responses|tool_followup|-|-|exec_command",
        ] {
            assert!(RouteProjection::parse_key(key).is_none(), "{key}");
        }
    }

    #[test]
    fn route_projection_ignores_source_specific_identity() {
        let mut baseline = sample_ir();
        baseline.state_kind = WorkflowStateKind::Edit;
        baseline.recovery_signal = RecoverySignal::None;
        baseline.capability_constraints = CapabilityConstraints::default();
        let expected = "agent_trace/v2|edit|normal";

        let mut changed = baseline.clone();
        changed.harness_id = HarnessId::ClaudeCode;
        assert_eq!(changed.route_projection().key(), expected);

        let mut changed = baseline.clone();
        changed.protocol = ProtocolKind::Messages;
        assert_eq!(changed.route_projection().key(), expected);

        let mut changed = baseline.clone();
        changed.active_workflow = Some("review-release".to_string());
        assert_eq!(changed.route_projection().key(), expected);

        let mut changed = baseline.clone();
        changed.subagent_role = Some("reviewer".to_string());
        assert_eq!(changed.route_projection().key(), expected);

        let mut changed = baseline;
        changed.last_tool_name = Some("apply_patch".to_string());
        assert_eq!(changed.route_projection().key(), expected);
    }

    #[test]
    fn route_projection_keys_normal_and_guarded_risk() {
        let cases = [
            (WorkflowStateKind::Edit, "agent_trace/v2|edit|normal"),
            (WorkflowStateKind::Test, "agent_trace/v2|test|normal"),
            (
                WorkflowStateKind::ToolFollowup,
                "agent_trace/v2|tool_followup|normal",
            ),
            (WorkflowStateKind::Unknown, "agent_trace/v2|unknown|guarded"),
            (WorkflowStateKind::Debug, "agent_trace/v2|debug|guarded"),
            (WorkflowStateKind::Review, "agent_trace/v2|review|normal"),
            (
                WorkflowStateKind::Recovery,
                "agent_trace/v2|recovery|guarded",
            ),
            (
                WorkflowStateKind::Finalization,
                "agent_trace/v2|finalization|guarded",
            ),
        ];

        for (state_kind, expected) in cases {
            let mut ir = sample_ir();
            ir.state_kind = state_kind;
            ir.recovery_signal = RecoverySignal::None;
            ir.capability_constraints = CapabilityConstraints::default();
            assert_eq!(ir.route_projection().key(), expected);
        }
    }

    #[test]
    fn route_projection_guards_recovery_and_high_cost_constraints() {
        let cases = [
            (
                RecoverySignal::LikelyRecovery,
                CapabilityConstraints::default(),
                "agent_trace/v2|edit|guarded",
            ),
            (
                RecoverySignal::None,
                CapabilityConstraints {
                    context_pressure: RequirementLevel::High,
                    ..CapabilityConstraints::default()
                },
                "agent_trace/v2|edit|context",
            ),
            (
                RecoverySignal::None,
                CapabilityConstraints {
                    expected_redo_penalty: RequirementLevel::High,
                    ..CapabilityConstraints::default()
                },
                "agent_trace/v2|edit|guarded",
            ),
            (
                RecoverySignal::None,
                CapabilityConstraints {
                    output_precision: RequirementLevel::High,
                    ..CapabilityConstraints::default()
                },
                "agent_trace/v2|edit|guarded",
            ),
        ];

        for (recovery_signal, capability_constraints, expected) in cases {
            let mut ir = sample_ir();
            ir.state_kind = WorkflowStateKind::Edit;
            ir.recovery_signal = recovery_signal;
            ir.capability_constraints = capability_constraints;
            assert_eq!(ir.route_projection().key(), expected);
        }
    }

    #[test]
    fn route_projection_v2_splits_context_from_hard_guardrails() {
        let mut review = sample_ir();
        review.state_kind = WorkflowStateKind::Review;
        review.recovery_signal = RecoverySignal::None;
        review.capability_constraints = CapabilityConstraints::default();
        assert_eq!(
            review.route_projection().key(),
            "agent_trace/v2|review|normal"
        );

        let mut long_context_edit = review.clone();
        long_context_edit.state_kind = WorkflowStateKind::Edit;
        long_context_edit.capability_constraints.context_pressure = RequirementLevel::High;
        assert_eq!(
            long_context_edit.route_projection().key(),
            "agent_trace/v2|edit|context"
        );

        long_context_edit
            .capability_constraints
            .expected_redo_penalty = RequirementLevel::High;
        assert_eq!(
            long_context_edit.route_projection().key(),
            "agent_trace/v2|edit|guarded"
        );
    }

    #[test]
    fn route_projection_rejects_retired_trace_v1_keys() {
        let parsed = RouteProjection::parse_key("agent_trace/v2|edit|context")
            .expect("canonical v2 context projection");
        assert_eq!(parsed.risk, RouteRisk::Context);
        assert!(RouteProjection::parse_key("agent_trace/v1|edit|context").is_none());
    }
}
