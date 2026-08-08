use serde::{Deserialize, Serialize};

use bitrouter_sdk::language_model::types::{Content, Prompt, Role};

use crate::workflow_state::extractors::generic::tool_result_reports_failure;
use crate::workflow_state::ir::{
    EvidenceLevel, NormalizedActionKind, RecoverySignal, RequirementLevel, RouteProjection,
    RouteRisk, ToolDensity, WorkflowStateIR, WorkflowStateKind, parse_route_risk,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStepRole {
    Orchestrate,
    Implement,
    Mechanical,
    Verify,
    Finalize,
    Unknown,
}

impl NextStepRole {
    fn key(self) -> &'static str {
        match self {
            Self::Orchestrate => "orchestrate",
            Self::Implement => "implement",
            Self::Mechanical => "mechanical",
            Self::Verify => "verify",
            Self::Finalize => "finalize",
            Self::Unknown => "unknown",
        }
    }

    fn parse_key(value: &str) -> Option<Self> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextActionClass {
    ReasonOrPlan,
    InspectOrRead,
    Mutate,
    ExecuteOrTest,
    WaitOrPoll,
    AnswerOrSummarize,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Mechanical,
    Simple,
    Substantive,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Opening,
    Progressing,
    Stalled,
    Recovering,
    NearDone,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictiveHistoryCompleteness {
    Complete,
    BoundedPrefix,
    Truncated,
    Ambiguous,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictiveEvidence {
    pub code: String,
    pub weight: i16,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictiveRouteIR {
    pub schema_version: u8,
    pub observed: RouteProjection,
    pub next_step_role: NextStepRole,
    pub next_action_class: NextActionClass,
    pub task_complexity: TaskComplexity,
    pub progress_state: ProgressState,
    pub history_completeness: PredictiveHistoryCompleteness,
    pub route_risk: RouteRisk,
    pub confidence: f32,
    pub evidence: Vec<PredictiveEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictiveRouteProjection {
    pub next_step_role: NextStepRole,
    pub risk: RouteRisk,
}

impl PredictiveRouteProjection {
    pub const fn new(next_step_role: NextStepRole, risk: RouteRisk) -> Self {
        Self {
            next_step_role,
            risk,
        }
    }

    pub const fn schema_version(&self) -> u8 {
        1
    }

    pub fn key(&self) -> String {
        format!("agent_route/v1|{}|{}", self.next_step_role.key(), self.risk)
    }

    pub fn parse_key(value: &str) -> Option<Self> {
        let mut segments = value.split('|');
        let (Some(namespace_version), Some(next_step_role), Some(risk), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return None;
        };
        if namespace_version != "agent_route/v1" {
            return None;
        }
        let risk = parse_route_risk(risk)?;

        Some(Self::new(NextStepRole::parse_key(next_step_role)?, risk))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalPolicyProjection {
    Observed(RouteProjection),
    Predictive(PredictiveRouteProjection),
}

impl CanonicalPolicyProjection {
    pub fn parse_key(value: &str) -> Option<Self> {
        RouteProjection::parse_key(value)
            .map(Self::Observed)
            .or_else(|| PredictiveRouteProjection::parse_key(value).map(Self::Predictive))
    }
}

const ROLE_COUNT: usize = 5;
const MAX_PREDICTIVE_EVIDENCE: usize = 8;
const MAX_HISTORY_SIGNAL_COUNT: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PredictiveReasonCode {
    InstructionContradiction,
    HistoryAmbiguous,
    HistoryTruncated,
    HistoryBoundedPrefix,
    HistoryUnknown,
    OpeningBroadGoal,
    ConcreteMutationRequested,
    MutationRequested,
    VerificationRequested,
    NarrowPollRequested,
    FinalAnswerRequested,
    ReadResultAvailable,
    MutationResultAvailable,
    TestFailedOnce,
    RepeatedFailure,
    ProgressNearDone,
    TestSucceeded,
    ActionFailedOnce,
    RecoveryPressure,
    RedoPenaltyHigh,
    ContextPressureHigh,
    OutputPrecisionHigh,
    ToolContextAvailable,
    TrajectoryPressure,
    ScoreMarginLow,
}

impl PredictiveReasonCode {
    const ALL: &[Self] = &[
        Self::InstructionContradiction,
        Self::HistoryAmbiguous,
        Self::HistoryTruncated,
        Self::HistoryBoundedPrefix,
        Self::HistoryUnknown,
        Self::OpeningBroadGoal,
        Self::ConcreteMutationRequested,
        Self::MutationRequested,
        Self::VerificationRequested,
        Self::NarrowPollRequested,
        Self::FinalAnswerRequested,
        Self::ReadResultAvailable,
        Self::MutationResultAvailable,
        Self::TestFailedOnce,
        Self::RepeatedFailure,
        Self::ProgressNearDone,
        Self::TestSucceeded,
        Self::ActionFailedOnce,
        Self::RecoveryPressure,
        Self::RedoPenaltyHigh,
        Self::ContextPressureHigh,
        Self::OutputPrecisionHigh,
        Self::ToolContextAvailable,
        Self::TrajectoryPressure,
        Self::ScoreMarginLow,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::InstructionContradiction => "instruction_contradiction",
            Self::HistoryAmbiguous => "history_ambiguous",
            Self::HistoryTruncated => "history_truncated",
            Self::HistoryBoundedPrefix => "history_bounded_prefix",
            Self::HistoryUnknown => "history_unknown",
            Self::OpeningBroadGoal => "opening_broad_goal",
            Self::ConcreteMutationRequested => "concrete_mutation_requested",
            Self::MutationRequested => "mutation_requested",
            Self::VerificationRequested => "verification_requested",
            Self::NarrowPollRequested => "narrow_poll_requested",
            Self::FinalAnswerRequested => "final_answer_requested",
            Self::ReadResultAvailable => "read_result_available",
            Self::MutationResultAvailable => "mutation_result_available",
            Self::TestFailedOnce => "test_failed_once",
            Self::RepeatedFailure => "repeated_failure",
            Self::ProgressNearDone => "progress_near_done",
            Self::TestSucceeded => "test_succeeded",
            Self::ActionFailedOnce => "action_failed_once",
            Self::RecoveryPressure => "recovery_pressure",
            Self::RedoPenaltyHigh => "redo_penalty_high",
            Self::ContextPressureHigh => "context_pressure_high",
            Self::OutputPrecisionHigh => "output_precision_high",
            Self::ToolContextAvailable => "tool_context_available",
            Self::TrajectoryPressure => "trajectory_pressure",
            Self::ScoreMarginLow => "score_margin_low",
        }
    }
}

/// Returns true only for a categorical reason emitted by the predictor.
pub fn is_predictive_reason_code(code: &str) -> bool {
    PredictiveReasonCode::ALL
        .iter()
        .any(|candidate| candidate.as_str() == code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedAction {
    Read,
    Mutate,
    Test,
    Other,
}

#[derive(Debug, Default)]
struct InstructionFeatures {
    broad: bool,
    mutate: bool,
    verify: bool,
    poll: bool,
    finalize: bool,
    concrete: bool,
    contradictory: bool,
}

impl InstructionFeatures {
    fn has_signal(&self) -> bool {
        self.broad || self.mutate || self.verify || self.poll || self.finalize || self.concrete
    }
}

#[derive(Debug)]
struct HistoryFeatures {
    completeness: PredictiveHistoryCompleteness,
    last_action: Option<ObservedAction>,
    last_failed: bool,
    failure_count: u8,
    successful_test_after_mutation: bool,
    has_trajectory: bool,
}

pub fn predict_next_step(observed: &WorkflowStateIR, prompt: &Prompt) -> PredictiveRouteIR {
    let history = history_features(prompt, observed);
    let instruction = instruction_features(prompt, observed.normalized_action_history.is_some());
    let observed_projection = observed.route_projection();
    let mut evidence = Vec::new();

    if instruction.contradictory
        || matches!(
            history.completeness,
            PredictiveHistoryCompleteness::BoundedPrefix
                | PredictiveHistoryCompleteness::Truncated
                | PredictiveHistoryCompleteness::Ambiguous
                | PredictiveHistoryCompleteness::Unknown
        )
    {
        let code = if instruction.contradictory {
            PredictiveReasonCode::InstructionContradiction
        } else if history.completeness == PredictiveHistoryCompleteness::Ambiguous {
            PredictiveReasonCode::HistoryAmbiguous
        } else if history.completeness == PredictiveHistoryCompleteness::Truncated {
            PredictiveReasonCode::HistoryTruncated
        } else if history.completeness == PredictiveHistoryCompleteness::BoundedPrefix {
            PredictiveReasonCode::HistoryBoundedPrefix
        } else {
            PredictiveReasonCode::HistoryUnknown
        };
        push_evidence(&mut evidence, code, -8, 0.95);
        return unknown_prediction(observed_projection, history.completeness, evidence);
    }

    let mut scores = [0_i16; ROLE_COUNT];
    let opening = observed.state_kind == WorkflowStateKind::Opening && !history.has_trajectory;

    if instruction.broad {
        let weight = if opening { 9 } else { 5 };
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            weight,
            &mut evidence,
            PredictiveReasonCode::OpeningBroadGoal,
            0.85,
        );
    }
    if instruction.mutate {
        let weight = if instruction.concrete { 7 } else { 4 };
        add_score(
            &mut scores,
            NextStepRole::Implement,
            weight,
            &mut evidence,
            if instruction.concrete {
                PredictiveReasonCode::ConcreteMutationRequested
            } else {
                PredictiveReasonCode::MutationRequested
            },
            0.8,
        );
    }
    if instruction.verify {
        add_score(
            &mut scores,
            NextStepRole::Verify,
            4,
            &mut evidence,
            PredictiveReasonCode::VerificationRequested,
            0.75,
        );
    }
    if instruction.poll {
        add_score(
            &mut scores,
            NextStepRole::Mechanical,
            9,
            &mut evidence,
            PredictiveReasonCode::NarrowPollRequested,
            0.9,
        );
    }
    if instruction.finalize {
        add_score(
            &mut scores,
            NextStepRole::Finalize,
            3,
            &mut evidence,
            PredictiveReasonCode::FinalAnswerRequested,
            0.65,
        );
    }

    match (history.last_action, history.last_failed) {
        (Some(ObservedAction::Read), false) => add_score(
            &mut scores,
            NextStepRole::Implement,
            9,
            &mut evidence,
            PredictiveReasonCode::ReadResultAvailable,
            0.9,
        ),
        (Some(ObservedAction::Mutate), false) => add_score(
            &mut scores,
            NextStepRole::Verify,
            9,
            &mut evidence,
            PredictiveReasonCode::MutationResultAvailable,
            0.9,
        ),
        (Some(ObservedAction::Test), true) if history.failure_count == 1 => add_score(
            &mut scores,
            NextStepRole::Implement,
            9,
            &mut evidence,
            PredictiveReasonCode::TestFailedOnce,
            0.9,
        ),
        (Some(ObservedAction::Test), true) => add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            12,
            &mut evidence,
            PredictiveReasonCode::RepeatedFailure,
            0.95,
        ),
        (Some(ObservedAction::Test), false) if history.successful_test_after_mutation => add_score(
            &mut scores,
            NextStepRole::Finalize,
            12,
            &mut evidence,
            PredictiveReasonCode::ProgressNearDone,
            0.95,
        ),
        (Some(ObservedAction::Test), false) => add_score(
            &mut scores,
            NextStepRole::Finalize,
            7,
            &mut evidence,
            PredictiveReasonCode::TestSucceeded,
            0.8,
        ),
        (Some(ObservedAction::Other), false) => {}
        (Some(ObservedAction::Read | ObservedAction::Mutate | ObservedAction::Other), true) => {
            add_score(
                &mut scores,
                NextStepRole::Implement,
                5,
                &mut evidence,
                PredictiveReasonCode::ActionFailedOnce,
                0.7,
            );
        }
        (None, _) => {}
    }

    if history.failure_count >= 2 && observed.recovery_signal == RecoverySignal::LikelyRecovery {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            4,
            &mut evidence,
            PredictiveReasonCode::RecoveryPressure,
            0.9,
        );
    }
    if observed.capability_constraints.expected_redo_penalty == RequirementLevel::High
        && history.failure_count >= 2
    {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            2,
            &mut evidence,
            PredictiveReasonCode::RedoPenaltyHigh,
            0.8,
        );
    }
    if observed.capability_constraints.context_pressure == RequirementLevel::High {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            2,
            &mut evidence,
            PredictiveReasonCode::ContextPressureHigh,
            0.7,
        );
    }
    if observed.capability_constraints.output_precision == RequirementLevel::High
        && history.last_action == Some(ObservedAction::Mutate)
    {
        add_score(
            &mut scores,
            NextStepRole::Verify,
            2,
            &mut evidence,
            PredictiveReasonCode::OutputPrecisionHigh,
            0.75,
        );
    }
    if observed.tool_density == ToolDensity::High
        && history.last_action == Some(ObservedAction::Read)
    {
        add_score(
            &mut scores,
            NextStepRole::Implement,
            1,
            &mut evidence,
            PredictiveReasonCode::ToolContextAvailable,
            0.65,
        );
    }
    if observed
        .evidence
        .iter()
        .any(|item| item.kind == "trajectory_pressure")
    {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            3,
            &mut evidence,
            PredictiveReasonCode::TrajectoryPressure,
            0.85,
        );
    }

    let coverage = u8::from(instruction.has_signal())
        + u8::from(history.has_trajectory)
        + u8::from(observed.confidence > 0.0);
    let (role, top_score, runner_up) = choose_role(scores);
    let margin = top_score.saturating_sub(runner_up);
    if top_score < 5 || margin < 2 || coverage < 2 {
        push_evidence(&mut evidence, PredictiveReasonCode::ScoreMarginLow, -4, 0.8);
        return unknown_prediction(observed_projection, history.completeness, evidence);
    }

    let next_action_class = action_for_role(role);
    let task_complexity = if role == NextStepRole::Mechanical {
        TaskComplexity::Mechanical
    } else if instruction.broad || instruction.concrete || history.failure_count > 0 {
        TaskComplexity::Substantive
    } else {
        TaskComplexity::Simple
    };
    let progress_state = if history.successful_test_after_mutation {
        ProgressState::NearDone
    } else if history.failure_count > 0
        && observed.recovery_signal == RecoverySignal::LikelyRecovery
    {
        ProgressState::Recovering
    } else if opening {
        ProgressState::Opening
    } else {
        ProgressState::Progressing
    };
    let route_risk = if history.failure_count >= 2 {
        RouteRisk::Guarded
    } else {
        observed_projection.risk
    };

    PredictiveRouteIR {
        schema_version: 1,
        observed: observed_projection,
        next_step_role: role,
        next_action_class,
        task_complexity,
        progress_state,
        history_completeness: history.completeness,
        route_risk,
        confidence: confidence_band(margin),
        evidence,
    }
}

fn instruction_features(
    prompt: &Prompt,
    normalized_plain_text_history: bool,
) -> InstructionFeatures {
    let messages = if normalized_plain_text_history {
        let boundary = prompt
            .messages
            .iter()
            .position(|message| message.role == Role::Assistant)
            .unwrap_or(prompt.messages.len());
        &prompt.messages[..boundary]
    } else {
        &prompt.messages
    };
    let text = messages
        .iter()
        .rev()
        .filter(|message| matches!(message.role, Role::User | Role::System))
        .find_map(message_text)
        .or_else(|| prompt.system.clone())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let broad = contains_any(
        &text,
        &[
            "plan",
            "investigate",
            "analyze",
            "architecture",
            "design",
            "understand",
            "decompose",
            "identify the affected",
            "choose a direction",
        ],
    );
    let mutate = contains_any(
        &text,
        &[
            "fix",
            "implement",
            "update",
            "change",
            "add",
            "remove",
            "correct",
            "repair",
            "refactor",
        ],
    );
    let verify = contains_any(&text, &["verify", "test", "review", "check", "validate"]);
    let poll = contains_any(&text, &["poll", "status", "wait", "watch", "monitor"]);
    let finalize = contains_any(
        &text,
        &[
            "summarize",
            "final answer",
            "report",
            "handoff",
            "explain the completed",
        ],
    );
    let concrete = has_concrete_evidence(&text);
    let contradictory = mutate
        && contains_any(
            &text,
            &[
                "do not modify",
                "without changing",
                "only summarize",
                "no changes",
            ],
        );

    InstructionFeatures {
        broad,
        mutate,
        verify,
        poll,
        finalize,
        concrete,
        contradictory,
    }
}

fn message_text(message: &bitrouter_sdk::language_model::types::Message) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn has_concrete_evidence(text: &str) -> bool {
    contains_any(
        text,
        &[
            "error:",
            "failed",
            "acceptance",
            "expected ",
            "actual ",
            "line ",
            ".rs",
            ".py",
            ".ts",
            ".js",
            ".go",
            ".toml",
            ".yaml",
            ".json",
            "src/",
            "tests/",
        ],
    )
}

fn contains_any(text: &str, terms: &[&str]) -> bool {
    terms.iter().any(|term| text.contains(term))
}

fn history_features(prompt: &Prompt, observed: &WorkflowStateIR) -> HistoryFeatures {
    let mut calls = Vec::<(String, ObservedAction, bool)>::new();
    let mut failure_count = 0_u8;
    let mut mutation_count = 0_u8;
    let mut last_action = None;
    let mut last_failed = false;
    let mut unmatched_result = false;
    let mut result_count = 0_u8;

    for content in prompt.messages.iter().flat_map(|message| &message.content) {
        match content {
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let action = classify_action(name, arguments, observed);
                if action == ObservedAction::Mutate {
                    mutation_count = bounded_signal_count(mutation_count);
                }
                calls.push((id.clone(), action, false));
            }
            Content::ToolResult {
                call_id, output, ..
            } => {
                let Some((_, action, matched)) = calls
                    .iter_mut()
                    .rev()
                    .find(|(id, _, matched)| id == call_id && !*matched)
                else {
                    unmatched_result = true;
                    continue;
                };
                *matched = true;
                let failed = tool_result_reports_failure(output);
                if failed {
                    failure_count = bounded_signal_count(failure_count);
                }
                result_count = result_count.saturating_add(1).min(3);
                last_action = Some(*action);
                last_failed = failed;
            }
            _ => {}
        }
    }

    if let Some(last) = last_action
        && last == ObservedAction::Other
    {
        last_action = match observed.state_kind {
            WorkflowStateKind::Edit => Some(ObservedAction::Mutate),
            WorkflowStateKind::Test => Some(ObservedAction::Test),
            _ => Some(ObservedAction::Other),
        };
    }
    if calls.is_empty()
        && let Some(normalized) = observed.normalized_action_history.as_ref()
    {
        let last_action = normalized.last_action.map(|action| match action {
            NormalizedActionKind::Read => ObservedAction::Read,
            NormalizedActionKind::Mutate => ObservedAction::Mutate,
            NormalizedActionKind::Test => ObservedAction::Test,
            NormalizedActionKind::Other => ObservedAction::Other,
        });
        return HistoryFeatures {
            completeness: if normalized.complete {
                PredictiveHistoryCompleteness::Complete
            } else {
                PredictiveHistoryCompleteness::Truncated
            },
            last_action,
            last_failed: normalized.last_failed,
            failure_count: normalized.failure_count,
            successful_test_after_mutation: last_action == Some(ObservedAction::Test)
                && !normalized.last_failed
                && normalized.mutation_count > 0,
            has_trajectory: true,
        };
    }
    let unmatched_call = calls.iter().any(|(_, _, matched)| !matched);
    let first_role = prompt.messages.first().map(|message| message.role);
    let server_side_context_gap = observed.evidence.iter().any(|evidence| {
        evidence.kind == "server_side_context_gap" && evidence.level == EvidenceLevel::Missing
    });
    let completeness = if prompt.messages.is_empty()
        || (server_side_context_gap && !has_complete_visible_causal_history(prompt))
    {
        PredictiveHistoryCompleteness::Unknown
    } else if unmatched_result {
        PredictiveHistoryCompleteness::Ambiguous
    } else if unmatched_call {
        PredictiveHistoryCompleteness::Truncated
    } else if matches!(first_role, Some(Role::Assistant | Role::Tool)) {
        PredictiveHistoryCompleteness::BoundedPrefix
    } else {
        PredictiveHistoryCompleteness::Complete
    };

    HistoryFeatures {
        completeness,
        last_action,
        last_failed,
        failure_count,
        successful_test_after_mutation: last_action == Some(ObservedAction::Test)
            && !last_failed
            && mutation_count > 0,
        has_trajectory: !calls.is_empty() || result_count > 0,
    }
}

pub(crate) fn has_complete_visible_causal_history(prompt: &Prompt) -> bool {
    let mut unmatched_calls = Vec::<String>::new();
    let mut completed_assistant_message = None;
    let mut invalid_result = false;
    for (message_index, message) in prompt.messages.iter().enumerate() {
        for content in &message.content {
            match content {
                Content::Text { text, .. }
                    if message.role == Role::Assistant && !text.trim().is_empty() =>
                {
                    completed_assistant_message = Some(message_index);
                }
                Content::ToolCall { id, .. } if message.role == Role::Assistant => {
                    unmatched_calls.push(id.clone());
                }
                Content::ToolCall { .. } => invalid_result = true,
                Content::ToolResult { call_id, .. } => {
                    if message.role != Role::Tool {
                        invalid_result = true;
                        continue;
                    }
                    let Some(position) = unmatched_calls.iter().rposition(|id| id == call_id)
                    else {
                        invalid_result = true;
                        continue;
                    };
                    unmatched_calls.remove(position);
                    completed_assistant_message = Some(message_index);
                }
                _ => {}
            }
        }
    }
    prompt
        .messages
        .iter()
        .find(|message| message.role != Role::System)
        .map(|message| message.role)
        == Some(Role::User)
        && !invalid_result
        && unmatched_calls.is_empty()
        && completed_assistant_message.is_some_and(|assistant_index| {
            prompt.messages[assistant_index.saturating_add(1)..]
                .iter()
                .any(|message| message.role == Role::User)
        })
}

fn bounded_signal_count(count: u8) -> u8 {
    count.saturating_add(1).min(MAX_HISTORY_SIGNAL_COUNT)
}

fn classify_action(name: &str, arguments: &str, observed: &WorkflowStateIR) -> ObservedAction {
    let name = name.to_ascii_lowercase();
    if contains_any(
        &name,
        &[
            "edit", "write", "patch", "create", "delete", "move", "rename",
        ],
    ) {
        return ObservedAction::Mutate;
    }
    if contains_any(
        &name,
        &[
            "read", "search", "find", "grep", "glob", "list", "view", "inspect",
        ],
    ) {
        return ObservedAction::Read;
    }
    if contains_any(&name, &["test", "check", "lint", "build"]) {
        return ObservedAction::Test;
    }
    if contains_any(&name, &["bash", "shell", "terminal", "exec", "command"])
        && let Some(command) = command_argument(arguments)
    {
        if command_is_test(&command) {
            return ObservedAction::Test;
        }
        if command_is_read(&command) {
            return ObservedAction::Read;
        }
    }
    match observed.state_kind {
        WorkflowStateKind::Edit => ObservedAction::Mutate,
        WorkflowStateKind::Test => ObservedAction::Test,
        _ => ObservedAction::Other,
    }
}

fn command_argument(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    value
        .as_object()?
        .iter()
        .find_map(|(key, value)| {
            matches!(key.as_str(), "cmd" | "command")
                .then(|| value.as_str())
                .flatten()
        })
        .map(str::to_ascii_lowercase)
}

fn command_is_test(command: &str) -> bool {
    contains_any(
        command,
        &[
            "cargo test",
            "cargo check",
            "cargo clippy",
            "pytest",
            "npm test",
            "pnpm test",
            "yarn test",
            "go test",
            "ctest",
        ],
    )
}

fn command_is_read(command: &str) -> bool {
    let command = command.trim_start();
    [
        "cat ",
        "sed ",
        "rg ",
        "grep ",
        "ls",
        "git diff",
        "git status",
    ]
    .iter()
    .any(|prefix| command.starts_with(prefix))
}

fn add_score(
    scores: &mut [i16; ROLE_COUNT],
    role: NextStepRole,
    weight: i16,
    evidence: &mut Vec<PredictiveEvidence>,
    code: PredictiveReasonCode,
    confidence: f32,
) {
    if let Some(index) = role_index(role) {
        scores[index] = scores[index].saturating_add(weight);
        push_evidence(evidence, code, weight, confidence);
    }
}

fn role_index(role: NextStepRole) -> Option<usize> {
    match role {
        NextStepRole::Orchestrate => Some(0),
        NextStepRole::Implement => Some(1),
        NextStepRole::Mechanical => Some(2),
        NextStepRole::Verify => Some(3),
        NextStepRole::Finalize => Some(4),
        NextStepRole::Unknown => None,
    }
}

fn choose_role(scores: [i16; ROLE_COUNT]) -> (NextStepRole, i16, i16) {
    let roles = [
        NextStepRole::Orchestrate,
        NextStepRole::Implement,
        NextStepRole::Mechanical,
        NextStepRole::Verify,
        NextStepRole::Finalize,
    ];
    let mut best_index = 0;
    let mut best_score = scores[0];
    let mut runner_up = i16::MIN;
    for (index, score) in scores.into_iter().enumerate().skip(1) {
        if score > best_score {
            runner_up = best_score.max(runner_up);
            best_index = index;
            best_score = score;
        } else {
            runner_up = runner_up.max(score);
        }
    }
    (roles[best_index], best_score, runner_up.max(0))
}

fn action_for_role(role: NextStepRole) -> NextActionClass {
    match role {
        NextStepRole::Orchestrate => NextActionClass::ReasonOrPlan,
        NextStepRole::Implement => NextActionClass::Mutate,
        NextStepRole::Mechanical => NextActionClass::WaitOrPoll,
        NextStepRole::Verify => NextActionClass::ExecuteOrTest,
        NextStepRole::Finalize => NextActionClass::AnswerOrSummarize,
        NextStepRole::Unknown => NextActionClass::Unknown,
    }
}

fn confidence_band(margin: i16) -> f32 {
    match margin {
        8.. => 0.9,
        5..=7 => 0.8,
        3..=4 => 0.7,
        _ => 0.6,
    }
}

fn push_evidence(
    evidence: &mut Vec<PredictiveEvidence>,
    code: PredictiveReasonCode,
    weight: i16,
    confidence: f32,
) {
    if evidence.len() < MAX_PREDICTIVE_EVIDENCE
        && !evidence.iter().any(|item| item.code == code.as_str())
    {
        evidence.push(PredictiveEvidence {
            code: code.as_str().to_string(),
            weight,
            confidence,
        });
    }
}

fn unknown_prediction(
    observed: RouteProjection,
    history_completeness: PredictiveHistoryCompleteness,
    evidence: Vec<PredictiveEvidence>,
) -> PredictiveRouteIR {
    PredictiveRouteIR {
        schema_version: 1,
        route_risk: observed.risk,
        observed,
        next_step_role: NextStepRole::Unknown,
        next_action_class: NextActionClass::Unknown,
        task_complexity: TaskComplexity::Ambiguous,
        progress_state: ProgressState::Unknown,
        history_completeness,
        confidence: 0.35,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role, ToolResultOutput,
    };

    use crate::workflow_state::extractors::generic::GenericPromptExtractor;
    use crate::workflow_state::extractors::{
        ExtractorInput, WorkflowStateExtractor, extract_workflow_state,
    };
    use crate::workflow_state::fixture::WorkflowTraceFixture;
    use crate::workflow_state::ir::{Evidence, HarnessId, ProtocolKind};

    const OPENING_PLAN_FIXTURE: &str =
        include_str!("../../tests/fixtures/workflow_state/predictive/opening-plan.json");
    const POST_READ_IMPLEMENT_FIXTURE: &str =
        include_str!("../../tests/fixtures/workflow_state/predictive/post-read-implement.json");
    const POST_EDIT_VERIFY_FIXTURE: &str =
        include_str!("../../tests/fixtures/workflow_state/predictive/post-edit-verify.json");
    const REPEATED_FAILURE_REPLAN_FIXTURE: &str =
        include_str!("../../tests/fixtures/workflow_state/predictive/repeated-failure-replan.json");
    const NEAR_DONE_FINALIZE_FIXTURE: &str =
        include_str!("../../tests/fixtures/workflow_state/predictive/near-done-finalize.json");

    fn prompt(messages: Vec<Message>) -> Prompt {
        Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn assistant_call(id: &str, name: &str, arguments: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn tool_result(call_id: &str, output: ToolResultOutput) -> Message {
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: call_id.to_string(),
                tool_name: None,
                output,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn observed(prompt: &Prompt) -> crate::workflow_state::ir::WorkflowStateIR {
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({});
        GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt,
        })
    }

    fn fixture_input(text: &str) -> Option<(crate::workflow_state::ir::WorkflowStateIR, Prompt)> {
        let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
        let fixture = WorkflowTraceFixture::from_value(value).ok()?;
        let ir = extract_workflow_state(&ExtractorInput {
            harness_hint: Some(fixture.harness),
            protocol_hint: fixture.protocol,
            headers: &fixture.headers,
            raw_body: &fixture.raw_body,
            prompt: &fixture.prompt,
        });
        Some((ir, fixture.prompt))
    }

    #[test]
    fn predictive_projection_uses_a_stable_canonical_key() {
        let projection = PredictiveRouteProjection::new(NextStepRole::Implement, RouteRisk::Normal);

        assert_eq!(projection.key(), "agent_route/v1|implement|normal");
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(CanonicalPolicyProjection::parse_key("agent_trace/v2|edit|normal").is_some());
        assert!(CanonicalPolicyProjection::parse_key("agent_route/v1|implement|normal").is_some());
        assert!(CanonicalPolicyProjection::parse_key("agent_route/v2|implement|normal").is_none());
    }

    #[test]
    fn predictive_projection_round_trips_exactly() {
        let projection = PredictiveRouteProjection::new(NextStepRole::Implement, RouteRisk::Normal);

        assert_eq!(projection.schema_version(), 1);
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(
            serde_json::from_str::<PredictiveRouteProjection>(
                r#"{"schema_version":2,"next_step_role":"implement","risk":"normal"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn predictive_enums_serialize_as_stable_snake_case_values() {
        assert!(matches!(
            serde_json::to_string(&NextStepRole::Orchestrate),
            Ok(value) if value == "\"orchestrate\""
        ));
        assert!(matches!(
            serde_json::to_string(&NextActionClass::ExecuteOrTest),
            Ok(value) if value == "\"execute_or_test\""
        ));
        assert!(matches!(
            serde_json::to_string(&TaskComplexity::Substantive),
            Ok(value) if value == "\"substantive\""
        ));
        assert!(matches!(
            serde_json::to_string(&ProgressState::NearDone),
            Ok(value) if value == "\"near_done\""
        ));
        assert!(matches!(
            serde_json::to_string(&PredictiveHistoryCompleteness::BoundedPrefix),
            Ok(value) if value == "\"bounded_prefix\""
        ));
    }

    #[test]
    fn predictive_reason_code_validator_accepts_every_producer_category() {
        for code in PredictiveReasonCode::ALL {
            assert!(is_predictive_reason_code(code.as_str()));
        }
        assert!(!is_predictive_reason_code("customer_secret"));
        assert!(!is_predictive_reason_code(""));
    }

    #[test]
    fn predictive_key_excludes_source_specific_identity() {
        let projection = PredictiveRouteProjection::new(NextStepRole::Implement, RouteRisk::Normal);
        let key = projection.key();

        assert_eq!(key, "agent_route/v1|implement|normal");
        for source_identity in ["codex", "claude_code", "hermes", "smithers", "openclaw"] {
            assert!(!key.contains(source_identity), "{source_identity}");
        }
    }

    #[test]
    fn predicts_roles_from_http_native_history() {
        let cases = [
            (
                "broad complex opening",
                OPENING_PLAN_FIXTURE,
                NextStepRole::Orchestrate,
                NextActionClass::ReasonOrPlan,
                RouteRisk::Normal,
            ),
            (
                "successful repository read",
                POST_READ_IMPLEMENT_FIXTURE,
                NextStepRole::Implement,
                NextActionClass::Mutate,
                RouteRisk::Normal,
            ),
            (
                "successful mutation",
                POST_EDIT_VERIFY_FIXTURE,
                NextStepRole::Verify,
                NextActionClass::ExecuteOrTest,
                RouteRisk::Normal,
            ),
            (
                "repeated failure recovery pressure",
                REPEATED_FAILURE_REPLAN_FIXTURE,
                NextStepRole::Orchestrate,
                NextActionClass::ReasonOrPlan,
                RouteRisk::Guarded,
            ),
            (
                "successful final test",
                NEAR_DONE_FINALIZE_FIXTURE,
                NextStepRole::Finalize,
                NextActionClass::AnswerOrSummarize,
                RouteRisk::Normal,
            ),
        ];

        for (name, fixture_text, expected_role, expected_action, expected_risk) in cases {
            let Some((ir, prompt)) = fixture_input(fixture_text) else {
                assert!(false, "invalid prediction fixture: {name}");
                continue;
            };
            let prediction = predict_next_step(&ir, &prompt);

            assert_eq!(prediction.next_step_role, expected_role, "{name}");
            assert_eq!(prediction.next_action_class, expected_action, "{name}");
            assert_eq!(prediction.route_risk, expected_risk, "{name}");
        }
    }

    #[test]
    fn predicts_concrete_fix_first_failure_and_narrow_poll() {
        let concrete_fix = prompt(vec![Message::text(
            Role::User,
            "Fix the parser error in src/parser.rs: expected a closing delimiter.",
        )]);
        let first_failure = prompt(vec![
            Message::text(Role::User, "Make src/parser.rs pass its parser tests."),
            assistant_call("test-1", "bash", r#"{"cmd":"cargo test -p parser"}"#),
            tool_result(
                "test-1",
                ToolResultOutput::ErrorText {
                    value: "assertion mismatch".to_string(),
                },
            ),
        ]);
        let narrow_poll = prompt(vec![Message::text(
            Role::User,
            "Poll the deployment status once and report whether it is complete.",
        )]);
        let cases = [
            (
                "concrete fix with file and error",
                concrete_fix,
                NextStepRole::Implement,
                NextActionClass::Mutate,
            ),
            (
                "first failed test",
                first_failure,
                NextStepRole::Implement,
                NextActionClass::Mutate,
            ),
            (
                "narrow status poll",
                narrow_poll,
                NextStepRole::Mechanical,
                NextActionClass::WaitOrPoll,
            ),
        ];

        for (name, prompt, expected_role, expected_action) in cases {
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.next_step_role, expected_role, "{name}");
            assert_eq!(prediction.next_action_class, expected_action, "{name}");
        }
    }

    #[test]
    fn predicts_implementation_after_native_nonzero_test_exit() {
        for output in ["command exited with code 1", "exit code 1", "exit_code: 1"] {
            let failed_test = prompt(vec![
                Message::text(Role::User, "Fix src/parser.rs and pass its tests."),
                assistant_call("edit-1", "Edit", r#"{"file_path":"src/parser.rs"}"#),
                tool_result(
                    "edit-1",
                    ToolResultOutput::Text {
                        value: "file updated".to_string(),
                    },
                ),
                assistant_call("test-1", "Bash", r#"{"command":"cargo test -p parser"}"#),
                tool_result(
                    "test-1",
                    ToolResultOutput::Text {
                        value: output.to_string(),
                    },
                ),
            ]);

            let prediction = predict_next_step(&observed(&failed_test), &failed_test);

            assert_eq!(
                prediction.next_step_role,
                NextStepRole::Implement,
                "{output}"
            );
            assert_eq!(
                prediction.next_action_class,
                NextActionClass::Mutate,
                "{output}"
            );
            assert!(
                prediction
                    .evidence
                    .iter()
                    .any(|item| item.code == "test_failed_once"),
                "{output}"
            );
        }
    }

    #[test]
    fn predicts_unknown_for_missing_or_contradictory_history() {
        let missing = prompt(Vec::new());
        let contradictory = prompt(vec![tool_result(
            "missing-call",
            ToolResultOutput::Text {
                value: "completed".to_string(),
            },
        )]);
        let truncated = prompt(vec![
            Message::text(Role::User, "Implement the correction in src/parser.rs."),
            assistant_call("missing-result", "read_file", r#"{"path":"src/parser.rs"}"#),
        ]);

        for (name, prompt) in [
            ("missing", missing),
            ("contradictory", contradictory),
            ("truncated", truncated),
        ] {
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.next_step_role, NextStepRole::Unknown, "{name}");
            assert_eq!(
                prediction.next_action_class,
                NextActionClass::Unknown,
                "{name}"
            );
        }
    }

    #[test]
    fn predicts_unknown_for_bounded_prefix_history() {
        let bounded_prefix = prompt(vec![
            assistant_call("read-1", "read_file", r#"{"path":"src/parser.rs"}"#),
            tool_result(
                "read-1",
                ToolResultOutput::Text {
                    value: "parser source available".to_string(),
                },
            ),
        ]);

        let prediction = predict_next_step(&observed(&bounded_prefix), &bounded_prefix);

        assert_eq!(
            prediction.history_completeness,
            PredictiveHistoryCompleteness::BoundedPrefix
        );
        assert_eq!(prediction.next_step_role, NextStepRole::Unknown);
        assert_eq!(prediction.next_action_class, NextActionClass::Unknown);
    }

    #[test]
    fn hidden_server_history_is_unknown_until_causal_history_is_visible() {
        let hidden = prompt(vec![Message::text(Role::User, "Continue implementation")]);
        let mut hidden_ir = observed(&hidden);
        hidden_ir.evidence.push(Evidence {
            kind: "server_side_context_gap".to_owned(),
            value: "previous response may hide ancestry".to_owned(),
            confidence: 0.95,
            level: EvidenceLevel::Missing,
        });

        let hidden_prediction = predict_next_step(&hidden_ir, &hidden);

        assert_eq!(
            hidden_prediction.history_completeness,
            PredictiveHistoryCompleteness::Unknown
        );
        assert_eq!(hidden_prediction.next_step_role, NextStepRole::Unknown);
        assert!(
            hidden_prediction
                .evidence
                .iter()
                .any(|item| item.code == "history_unknown")
        );

        let visible = prompt(vec![
            Message::text(Role::User, "Inspect the repository"),
            assistant_call("read-1", "read_file", r#"{"path":"src/lib.rs"}"#),
            tool_result(
                "read-1",
                ToolResultOutput::Text {
                    value: "source available".to_owned(),
                },
            ),
            Message::text(Role::User, "Implement the approved change"),
        ]);
        let mut visible_ir = observed(&visible);
        visible_ir.evidence = hidden_ir.evidence;

        let visible_prediction = predict_next_step(&visible_ir, &visible);

        assert_eq!(
            visible_prediction.history_completeness,
            PredictiveHistoryCompleteness::Complete
        );
        assert_eq!(visible_prediction.next_step_role, NextStepRole::Implement);
    }

    #[test]
    fn visible_causal_history_accepts_text_turns_and_rejects_partial_prefixes() {
        let text_history = prompt(vec![
            Message::text(Role::User, "Design the change"),
            Message::text(Role::Assistant, "Here is the approved design"),
            Message::text(Role::User, "Implement it"),
        ]);
        let system_prefixed_text_history = prompt(vec![
            Message::text(Role::System, "Follow the repository rules"),
            Message::text(Role::User, "Design the change"),
            Message::text(Role::Assistant, "Here is the approved design"),
            Message::text(Role::User, "Implement it"),
        ]);
        let system_and_current_user = prompt(vec![
            Message::text(Role::System, "Follow the repository rules"),
            Message::text(Role::User, "Implement it"),
        ]);
        let only_current_user = prompt(vec![Message::text(Role::User, "Implement it")]);
        let assistant_prefix = prompt(vec![
            Message::text(Role::Assistant, "Earlier answer"),
            Message::text(Role::User, "Continue"),
        ]);
        let unmatched_call = prompt(vec![
            Message::text(Role::User, "Inspect"),
            assistant_call("read-1", "read_file", r#"{"path":"src/lib.rs"}"#),
            Message::text(Role::User, "Continue"),
        ]);
        let unmatched_result = prompt(vec![
            Message::text(Role::User, "Inspect"),
            tool_result(
                "missing",
                ToolResultOutput::Text {
                    value: "source".to_owned(),
                },
            ),
            Message::text(Role::User, "Continue"),
        ]);

        assert!(has_complete_visible_causal_history(&text_history));
        assert!(has_complete_visible_causal_history(
            &system_prefixed_text_history
        ));
        for incomplete in [
            system_and_current_user,
            only_current_user,
            assistant_prefix,
            unmatched_call,
            unmatched_result,
        ] {
            assert!(!has_complete_visible_causal_history(&incomplete));
        }
    }

    #[test]
    fn predicts_without_task_name_or_source_identity() {
        let named = prompt(vec![Message::text(
            Role::User,
            "Fix path-tracing-reverse in src/solver.rs: error: wrong edge order.",
        )]);
        let renamed = prompt(vec![Message::text(
            Role::User,
            "Fix opaque-work-item in src/solver.rs: error: wrong edge order.",
        )]);
        let mut named_ir = observed(&named);
        named_ir.harness_id = HarnessId::Codex;
        named_ir.active_workflow = Some("private-phase".to_string());
        let mut renamed_ir = observed(&renamed);
        renamed_ir.harness_id = HarnessId::Hermes;
        renamed_ir.active_workflow = Some("another-private-phase".to_string());

        let named_prediction = predict_next_step(&named_ir, &named);
        let renamed_prediction = predict_next_step(&renamed_ir, &renamed);

        assert_eq!(
            PredictiveRouteProjection::new(
                named_prediction.next_step_role,
                named_prediction.route_risk
            ),
            PredictiveRouteProjection::new(
                renamed_prediction.next_step_role,
                renamed_prediction.route_risk
            )
        );
        assert_eq!(
            named_prediction.next_action_class,
            renamed_prediction.next_action_class
        );
        assert_eq!(named_prediction.evidence, renamed_prediction.evidence);
    }

    #[test]
    fn predictor_preserves_prompt_tools_and_excludes_private_evidence() {
        let Some((ir, prompt)) = fixture_input(POST_EDIT_VERIFY_FIXTURE) else {
            assert!(false, "invalid post-edit fixture");
            return;
        };
        let original = prompt.clone();

        let prediction = predict_next_step(&ir, &prompt);

        assert_eq!(prompt, original);
        assert!(prediction.evidence.len() <= 8);
        assert!(prediction.evidence.iter().all(|evidence| {
            evidence.code.len() <= 32
                && evidence
                    .code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                && !evidence.code.contains("src/")
                && !evidence.code.contains("apply_patch")
        }));
    }

    #[test]
    fn bounds_failure_and_mutation_signal_counts() {
        let mut failure_count = 0;
        let mut mutation_count = 0;
        for _ in 0..5 {
            failure_count = bounded_signal_count(failure_count);
            mutation_count = bounded_signal_count(mutation_count);
        }

        assert_eq!(failure_count, 3);
        assert_eq!(mutation_count, 3);
    }

    #[test]
    fn score_ties_follow_stable_role_order() {
        let cases = [
            ([4, 4, 0, 0, 0], NextStepRole::Orchestrate, 4, 4),
            ([0, 6, 6, 6, 6], NextStepRole::Implement, 6, 6),
            ([0, 0, 5, 5, 5], NextStepRole::Mechanical, 5, 5),
            ([0, 0, 0, 3, 3], NextStepRole::Verify, 3, 3),
        ];

        for (scores, expected_role, expected_top, expected_runner_up) in cases {
            assert_eq!(
                choose_role(scores),
                (expected_role, expected_top, expected_runner_up)
            );
        }
    }

    #[test]
    fn predictor_boundary_excludes_headers_and_task_labels() {
        let pure_predictor: fn(&WorkflowStateIR, &Prompt) -> PredictiveRouteIR = predict_next_step;
        let neutral = prompt(vec![Message::text(
            Role::User,
            "Fix opaque-work-item in src/solver.rs: error: wrong edge order.",
        )]);
        let header_shaped = prompt(vec![Message::text(
            Role::User,
            concat!(
                "Fix x-bitrouter-agent-role/mechanical ",
                "x-superpowers-task/path-tracing-reverse in src/solver.rs: ",
                "error: wrong edge order."
            ),
        )]);

        let neutral_prediction = pure_predictor(&observed(&neutral), &neutral);
        let labeled_prediction = pure_predictor(&observed(&header_shaped), &header_shaped);

        assert_eq!(neutral_prediction, labeled_prediction);
    }
}
