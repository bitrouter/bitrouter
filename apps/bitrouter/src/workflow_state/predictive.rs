use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use bitrouter_sdk::language_model::types::{Content, Prompt, Role};

use crate::workflow_state::extractors::generic::tool_result_reports_failure;
use crate::workflow_state::extractors::terminus_2::is_assistant_action;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFamily {
    CodeGeneration,
    CodeDebugging,
    CodeReview,
    CodeSqlDatabase,
    CodeFrontendUi,
    CodeDevopsConfig,
    CodeRepositoryAnalysis,
    AgentMultiStepPlanning,
    AgentWorkflowExecution,
    AgentWebResearch,
    AgentMemoryOperations,
    AgentGeneral,
    #[default]
    Unknown,
}

impl TaskFamily {
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
    #[serde(default)]
    pub predictor_contract_digest: String,
    #[serde(default)]
    pub confidence_kind: String,
    #[serde(default)]
    pub task_family: TaskFamily,
    #[serde(default)]
    pub task_family_confidence: f32,
    #[serde(default)]
    pub task_family_evidence: Vec<PredictiveEvidence>,
}

/// Signed-lock descriptor for the deterministic predictor compiled into this
/// BitRouter binary. Predictive locks are admitted only when this descriptor
/// exactly matches the compiled scorecard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictorContract {
    pub algorithm: String,
    pub version: u8,
    pub config_digest: String,
    pub confidence_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictiveRouteProjection {
    pub task_family: TaskFamily,
    pub next_step_role: NextStepRole,
    pub risk: RouteRisk,
}

impl PredictiveRouteProjection {
    pub const fn new(
        task_family: TaskFamily,
        next_step_role: NextStepRole,
        risk: RouteRisk,
    ) -> Self {
        Self {
            task_family,
            next_step_role,
            risk,
        }
    }

    pub const fn schema_version(&self) -> u8 {
        1
    }

    pub fn key(&self) -> String {
        format!(
            "agent_route/v1|{}|{}|{}",
            self.task_family.key(),
            self.next_step_role.key(),
            self.risk
        )
    }

    pub const fn unknown_baseline(&self) -> Self {
        Self::new(TaskFamily::Unknown, self.next_step_role, self.risk)
    }

    pub fn parse_key(value: &str) -> Option<Self> {
        let mut segments = value.split('|');
        let (Some(namespace_version), Some(task_family), Some(next_step_role), Some(risk), None) = (
            segments.next(),
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

        Some(Self::new(
            TaskFamily::parse_key(task_family)?,
            NextStepRole::parse_key(next_step_role)?,
            risk,
        ))
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
const COMPILED_SCORECARD_DIGEST: &str =
    "sha256:aa204ef3be199ffa8911e380e3dec214fb1070b28b113fa3c413e38703314ec6";
const PREDICTOR_ALGORITHM: &str = "deterministic_scorecard";
const PREDICTOR_CONFIDENCE_KIND: &str = "heuristic_margin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ConfidenceBand {
    minimum_margin: i16,
    confidence_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PredictorScorecardV1 {
    weights: BTreeMap<String, i16>,
    evidence_confidence_ppm: BTreeMap<String, u32>,
    repeated_failure_count: u8,
    minimum_top_score: i16,
    minimum_margin: i16,
    minimum_coverage: u8,
    maximum_evidence: usize,
    confidence_bands: Vec<ConfidenceBand>,
    unknown_confidence_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TaskFamilyScorecard {
    weights: BTreeMap<String, i16>,
    debugging_failure_fix_bonus: i16,
    review_bonus: i16,
    minimum_top_score: i16,
    minimum_evidence_count: u8,
    minimum_margin: i16,
    maximum_evidence: usize,
    confidence_ppm: u32,
    unknown_confidence_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PredictorBehaviorV1 {
    scorecard: PredictorScorecardV1,
    instruction_terms: BTreeMap<String, Vec<String>>,
    concrete_terms: Vec<String>,
    tool_name_terms: BTreeMap<String, Vec<String>>,
    command_test_terms: Vec<String>,
    command_read_prefixes: Vec<String>,
    role_tie_order: Vec<NextStepRole>,
    role_actions: BTreeMap<String, NextActionClass>,
    narrow_read_action: NextActionClass,
    algorithm_versions: BTreeMap<String, u8>,
    task_classifier_algorithm_version: u8,
    task_family_scorecard: TaskFamilyScorecard,
    task_family_terms: BTreeMap<String, Vec<String>>,
    task_family_modifier_families: BTreeMap<String, Vec<TaskFamily>>,
    task_family_intent_terms: BTreeMap<String, Vec<String>>,
    task_family_failure_terms: BTreeMap<String, Vec<String>>,
    task_family_precedence_terms: BTreeMap<String, Vec<String>>,
    task_family_anchor_terms: BTreeMap<String, Vec<String>>,
    task_family_code_subject_terms: Vec<String>,
    task_family_tie_order: Vec<TaskFamily>,
}

impl PredictorScorecardV1 {
    fn weight(&self, key: &str) -> i16 {
        match self.weights.get(key) {
            Some(weight) => *weight,
            None => 0,
        }
    }

    fn evidence_confidence(&self, code: PredictiveReasonCode) -> f32 {
        match self.evidence_confidence_ppm.get(code.as_str()) {
            Some(confidence) => *confidence as f32 / 1_000_000.0,
            None => 0.0,
        }
    }

    fn confidence_for_margin(&self, margin: i16) -> f32 {
        self.confidence_bands
            .iter()
            .find(|band| margin >= band.minimum_margin)
            .map_or(0.0, |band| band.confidence_ppm as f32 / 1_000_000.0)
    }

    fn unknown_confidence(&self) -> f32 {
        self.unknown_confidence_ppm as f32 / 1_000_000.0
    }
}

fn compiled_predictor_behavior() -> &'static PredictorBehaviorV1 {
    static BEHAVIOR: OnceLock<PredictorBehaviorV1> = OnceLock::new();
    BEHAVIOR.get_or_init(|| PredictorBehaviorV1 {
        scorecard: PredictorScorecardV1 {
            weights: BTreeMap::from([
                ("opening_broad".into(), 9),
                ("continuing_broad".into(), 5),
                ("concrete_mutation".into(), 7),
                ("mutation".into(), 4),
                ("verification".into(), 4),
                ("narrow_poll".into(), 9),
                ("narrow_read".into(), 9),
                ("finalize".into(), 3),
                ("read_result".into(), 9),
                ("mutation_result".into(), 9),
                ("test_failed_once".into(), 9),
                ("repeated_failure".into(), 12),
                ("near_done".into(), 12),
                ("test_succeeded".into(), 7),
                ("action_failed_once".into(), 5),
                ("recovery_pressure".into(), 4),
                ("redo_penalty".into(), 2),
                ("context_pressure".into(), 2),
                ("output_precision".into(), 2),
                ("tool_context".into(), 1),
                ("trajectory_pressure".into(), 3),
                ("incomplete_history".into(), -8),
                ("low_margin".into(), -4),
            ]),
            evidence_confidence_ppm: BTreeMap::from([
                ("instruction_contradiction".into(), 950_000),
                ("history_ambiguous".into(), 950_000),
                ("history_truncated".into(), 950_000),
                ("history_bounded_prefix".into(), 950_000),
                ("history_unknown".into(), 950_000),
                ("opening_broad_goal".into(), 850_000),
                ("concrete_mutation_requested".into(), 800_000),
                ("mutation_requested".into(), 800_000),
                ("verification_requested".into(), 750_000),
                ("narrow_poll_requested".into(), 900_000),
                ("narrow_read_requested".into(), 900_000),
                ("final_answer_requested".into(), 650_000),
                ("read_result_available".into(), 900_000),
                ("mutation_result_available".into(), 900_000),
                ("test_failed_once".into(), 900_000),
                ("repeated_failure".into(), 950_000),
                ("progress_near_done".into(), 950_000),
                ("test_succeeded".into(), 800_000),
                ("action_failed_once".into(), 700_000),
                ("recovery_pressure".into(), 900_000),
                ("redo_penalty_high".into(), 800_000),
                ("context_pressure_high".into(), 700_000),
                ("output_precision_high".into(), 750_000),
                ("tool_context_available".into(), 650_000),
                ("trajectory_pressure".into(), 850_000),
                ("score_margin_low".into(), 800_000),
            ]),
            repeated_failure_count: 2,
            minimum_top_score: 5,
            minimum_margin: 2,
            minimum_coverage: 2,
            maximum_evidence: MAX_PREDICTIVE_EVIDENCE,
            confidence_bands: vec![
                ConfidenceBand {
                    minimum_margin: 8,
                    confidence_ppm: 900_000,
                },
                ConfidenceBand {
                    minimum_margin: 5,
                    confidence_ppm: 800_000,
                },
                ConfidenceBand {
                    minimum_margin: 3,
                    confidence_ppm: 700_000,
                },
                ConfidenceBand {
                    minimum_margin: 0,
                    confidence_ppm: 600_000,
                },
            ],
            unknown_confidence_ppm: 350_000,
        },
        instruction_terms: BTreeMap::from([
            (
                "broad".into(),
                string_terms(&[
                    "plan",
                    "investigate",
                    "analyze",
                    "architecture",
                    "design",
                    "understand",
                    "decompose",
                    "identify the affected",
                    "choose a direction",
                    "repository",
                    "codebase",
                    "whole project",
                    "entire project",
                ]),
            ),
            (
                "mutate".into(),
                string_terms(&[
                    "fix",
                    "implement",
                    "update",
                    "change",
                    "add",
                    "remove",
                    "correct",
                    "repair",
                    "refactor",
                ]),
            ),
            (
                "verify".into(),
                string_terms(&["verify", "test", "review", "check", "validate"]),
            ),
            (
                "poll".into(),
                string_terms(&["poll", "status", "wait", "watch", "monitor"]),
            ),
            (
                "read".into(),
                string_terms(&[
                    "read ", "show ", "inspect ", "open ", "print ", "locate ", "find ",
                ]),
            ),
            (
                "finalize".into(),
                string_terms(&[
                    "summarize",
                    "final answer",
                    "report",
                    "handoff",
                    "explain the completed",
                ]),
            ),
            (
                "contradiction".into(),
                string_terms(&[
                    "make no changes",
                    "do not make changes",
                    "do not modify anything",
                    "no changes are allowed",
                    "only summarize",
                ]),
            ),
        ]),
        concrete_terms: string_terms(&[
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
        ]),
        tool_name_terms: BTreeMap::from([
            (
                "mutate".into(),
                string_terms(&[
                    "edit", "write", "patch", "create", "delete", "move", "rename",
                ]),
            ),
            (
                "read".into(),
                string_terms(&[
                    "read", "search", "find", "grep", "glob", "list", "view", "inspect",
                ]),
            ),
            (
                "test".into(),
                string_terms(&["test", "check", "lint", "build"]),
            ),
            (
                "command".into(),
                string_terms(&["bash", "shell", "terminal", "exec", "command"]),
            ),
        ]),
        command_test_terms: string_terms(&[
            "cargo test",
            "cargo check",
            "cargo clippy",
            "pytest",
            "npm test",
            "pnpm test",
            "yarn test",
            "go test",
            "ctest",
        ]),
        command_read_prefixes: string_terms(&[
            "cat ",
            "sed ",
            "rg ",
            "grep ",
            "ls",
            "git diff",
            "git status",
        ]),
        role_tie_order: vec![
            NextStepRole::Orchestrate,
            NextStepRole::Implement,
            NextStepRole::Mechanical,
            NextStepRole::Verify,
            NextStepRole::Finalize,
        ],
        role_actions: BTreeMap::from([
            ("orchestrate".into(), NextActionClass::ReasonOrPlan),
            ("implement".into(), NextActionClass::Mutate),
            ("mechanical".into(), NextActionClass::WaitOrPoll),
            ("verify".into(), NextActionClass::ExecuteOrTest),
            ("finalize".into(), NextActionClass::AnswerOrSummarize),
            ("unknown".into(), NextActionClass::Unknown),
        ]),
        narrow_read_action: NextActionClass::InspectOrRead,
        algorithm_versions: BTreeMap::from([
            ("instruction_features".into(), 1),
            ("history_pairing".into(), 1),
            ("tool_result_failure".into(), 1),
            ("visible_causal_history".into(), 1),
            ("role_scoring".into(), 1),
            ("task_complexity".into(), 1),
            ("risk_mapping".into(), 1),
            ("task_family_boundary_matching".into(), 1),
        ]),
        task_classifier_algorithm_version: 2,
        task_family_scorecard: TaskFamilyScorecard {
            weights: BTreeMap::from([
                ("agent:general".into(), 2),
                ("code:generation".into(), 3),
                ("code:debugging".into(), 4),
                ("code:review".into(), 4),
                ("code:sql_database".into(), 4),
                ("code:frontend_ui".into(), 4),
                ("code:devops_config".into(), 4),
                ("code:repository_analysis".into(), 4),
                ("agent:multi_step_planning".into(), 4),
                ("agent:workflow_execution".into(), 4),
                ("agent:web_research".into(), 4),
                ("agent:memory_operations".into(), 4),
            ]),
            debugging_failure_fix_bonus: 24,
            review_bonus: 24,
            minimum_top_score: 6,
            minimum_evidence_count: 2,
            minimum_margin: 1,
            maximum_evidence: MAX_PREDICTIVE_EVIDENCE,
            confidence_ppm: 800_000,
            unknown_confidence_ppm: 350_000,
        },
        task_family_terms: BTreeMap::from([
            (
                "code:generation".into(),
                string_terms(&["implement", "extend", "refactor", "new api", "new module"]),
            ),
            (
                "code:debugging".into(),
                string_terms(&[
                    "fix",
                    "panic",
                    "error",
                    "failed",
                    "failure",
                    "regression",
                    "exception",
                    "bug",
                ]),
            ),
            (
                "code:review".into(),
                string_terms(&[
                    "review",
                    "audit",
                    "pull request",
                    "diff",
                    "security",
                    "vulnerability",
                ]),
            ),
            (
                "code:sql_database".into(),
                string_terms(&["sql", "migration", "schema", "database", "query"]),
            ),
            (
                "code:frontend_ui".into(),
                string_terms(&[
                    "react",
                    "frontend",
                    "ui",
                    "css",
                    "dom",
                    "component",
                    "layout",
                ]),
            ),
            (
                "code:devops_config".into(),
                string_terms(&[
                    "deployment",
                    "ci",
                    "infrastructure",
                    "kubernetes",
                    "container",
                    "configuration",
                    "service",
                ]),
            ),
            (
                "code:repository_analysis".into(),
                string_terms(&[
                    "repository",
                    "codebase",
                    "dependency",
                    "dependencies",
                    "trace",
                    "call graph",
                    "scan",
                ]),
            ),
            (
                "agent:multi_step_planning".into(),
                string_terms(&[
                    "plan",
                    "multi-step",
                    "decompose",
                    "architecture",
                    "strategy",
                ]),
            ),
            (
                "agent:workflow_execution".into(),
                string_terms(&[
                    "orchestrate",
                    "workflow",
                    "pipeline",
                    "handoff",
                    "hand off",
                    "execute",
                ]),
            ),
            (
                "agent:web_research".into(),
                string_terms(&["browse", "web", "research", "sources", "current", "latest"]),
            ),
            (
                "agent:memory_operations".into(),
                string_terms(&["memory", "durable facts", "context", "synthesize", "saved"]),
            ),
            (
                "agent:general".into(),
                string_terms(&["agent", "assistant", "coordinate", "manage", "task"]),
            ),
        ]),
        task_family_modifier_families: BTreeMap::from([
            (
                "specific".into(),
                vec![
                    TaskFamily::CodeDebugging,
                    TaskFamily::CodeReview,
                    TaskFamily::CodeSqlDatabase,
                    TaskFamily::CodeFrontendUi,
                    TaskFamily::CodeDevopsConfig,
                    TaskFamily::CodeRepositoryAnalysis,
                    TaskFamily::AgentMultiStepPlanning,
                    TaskFamily::AgentWorkflowExecution,
                    TaskFamily::AgentWebResearch,
                    TaskFamily::AgentMemoryOperations,
                ],
            ),
            (
                "generic".into(),
                vec![TaskFamily::CodeGeneration, TaskFamily::AgentGeneral],
            ),
        ]),
        task_family_intent_terms: BTreeMap::from([
            (
                "debugging".into(),
                string_terms(&["fix", "repair", "correct", "debug"]),
            ),
            ("review".into(), string_terms(&["review", "audit"])),
        ]),
        task_family_failure_terms: BTreeMap::from([(
            "debugging".into(),
            string_terms(&[
                "panic",
                "error",
                "failed",
                "failure",
                "regression",
                "exception",
                "bug",
            ]),
        )]),
        task_family_precedence_terms: BTreeMap::from([(
            "review".into(),
            string_terms(&["review", "audit"]),
        )]),
        task_family_anchor_terms: BTreeMap::from([(
            "agent:workflow_execution".into(),
            string_terms(&[
                "agent",
                "orchestrate",
                "handoff",
                "hand off",
                "workflow control",
                "workflow controller",
            ]),
        )]),
        task_family_code_subject_terms: string_terms(&[
            "bug",
            "fix",
            "regression",
            "panic",
            "error",
            "code",
            "patch",
            "diff",
            "pull request",
            "security",
            "vulnerability",
            "parser",
            "api",
            "module",
            "function",
            "repository",
            "src/",
            ".rs",
            ".py",
            ".ts",
            ".js",
            ".go",
        ]),
        task_family_tie_order: vec![
            TaskFamily::CodeDebugging,
            TaskFamily::CodeReview,
            TaskFamily::CodeSqlDatabase,
            TaskFamily::CodeFrontendUi,
            TaskFamily::CodeDevopsConfig,
            TaskFamily::CodeRepositoryAnalysis,
            TaskFamily::AgentMultiStepPlanning,
            TaskFamily::AgentWorkflowExecution,
            TaskFamily::AgentWebResearch,
            TaskFamily::AgentMemoryOperations,
            TaskFamily::CodeGeneration,
            TaskFamily::AgentGeneral,
        ],
    })
}

fn compiled_scorecard_v1() -> &'static PredictorScorecardV1 {
    &compiled_predictor_behavior().scorecard
}

fn task_family_scorecard() -> &'static TaskFamilyScorecard {
    &compiled_predictor_behavior().task_family_scorecard
}

fn string_terms(terms: &[&str]) -> Vec<String> {
    terms.iter().map(|term| (*term).to_owned()).collect()
}

fn behavior_terms<'a>(terms: &'a BTreeMap<String, Vec<String>>, key: &str) -> &'a [String] {
    terms.get(key).map(Vec::as_slice).unwrap_or_default()
}

fn behavior_families<'a>(
    families: &'a BTreeMap<String, Vec<TaskFamily>>,
    key: &str,
) -> &'a [TaskFamily] {
    families.get(key).map(Vec::as_slice).unwrap_or_default()
}

fn classify_task_family(
    prompt: &Prompt,
    normalized_plain_text_history: bool,
) -> (TaskFamily, f32, Vec<PredictiveEvidence>) {
    let behavior = compiled_predictor_behavior();
    let scorecard = task_family_scorecard();
    let text = task_family_instruction_text(prompt, normalized_plain_text_history);
    let mut scores = BTreeMap::<TaskFamily, i16>::new();
    let mut evidence_counts = BTreeMap::<TaskFamily, u8>::new();

    for family in &behavior.task_family_tie_order {
        let key = family.key();
        let matched_count = behavior_terms(&behavior.task_family_terms, key)
            .iter()
            .filter(|term| contains_task_term(&text, term))
            .count()
            .min(u8::MAX as usize) as u8;
        let weight = scorecard.weights.get(key).copied().unwrap_or_default();
        scores.insert(*family, weight.saturating_mul(i16::from(matched_count)));
        evidence_counts.insert(*family, matched_count);
    }

    let debugging_intent = contains_any_task_terms(
        &text,
        behavior_terms(&behavior.task_family_intent_terms, "debugging"),
    );
    let debugging_failure = contains_any_task_terms(
        &text,
        behavior_terms(&behavior.task_family_failure_terms, "debugging"),
    );
    if debugging_intent && debugging_failure {
        add_task_family_bonus(
            &mut scores,
            TaskFamily::CodeDebugging,
            scorecard.debugging_failure_fix_bonus,
        );
        evidence_counts
            .entry(TaskFamily::CodeDebugging)
            .and_modify(|count| *count = (*count).max(scorecard.minimum_evidence_count));
    }
    let review_intent = contains_any_task_terms(
        &text,
        behavior_terms(&behavior.task_family_intent_terms, "review"),
    );
    let code_subject = contains_any_task_terms(&text, &behavior.task_family_code_subject_terms);
    let supported_review = review_intent && code_subject;
    if supported_review {
        add_task_family_bonus(&mut scores, TaskFamily::CodeReview, scorecard.review_bonus);
        evidence_counts
            .entry(TaskFamily::CodeReview)
            .and_modify(|count| *count = (*count).max(scorecard.minimum_evidence_count));
    }

    let workflow_family = TaskFamily::AgentWorkflowExecution;
    let workflow_anchor = contains_any_task_terms(
        &text,
        behavior_terms(&behavior.task_family_anchor_terms, workflow_family.key()),
    );
    if !workflow_anchor {
        scores.insert(workflow_family, 0);
        evidence_counts.insert(workflow_family, 0);
    }

    let review_precedence = contains_any_task_terms(
        &text,
        behavior_terms(&behavior.task_family_precedence_terms, "review"),
    );
    if supported_review && review_precedence {
        let score = scores
            .get(&TaskFamily::CodeReview)
            .copied()
            .unwrap_or_default();
        return accepted_task_family(TaskFamily::CodeReview, scorecard, score);
    }

    let has_specific_family =
        behavior_families(&behavior.task_family_modifier_families, "specific")
            .iter()
            .any(|family| {
                evidence_counts.get(family).copied().unwrap_or_default()
                    >= scorecard.minimum_evidence_count
            });
    if has_specific_family {
        for family in behavior_families(&behavior.task_family_modifier_families, "generic") {
            scores.insert(*family, 0);
        }
    }

    let mut best_family = TaskFamily::Unknown;
    let mut top_score = 0_i16;
    let mut runner_up = 0_i16;
    for family in &behavior.task_family_tie_order {
        let score = scores.get(family).copied().unwrap_or_default();
        if score > top_score {
            runner_up = top_score;
            best_family = *family;
            top_score = score;
        } else {
            runner_up = runner_up.max(score);
        }
    }
    let evidence_count = evidence_counts
        .get(&best_family)
        .copied()
        .unwrap_or_default();
    let margin = top_score.saturating_sub(runner_up);
    if top_score < scorecard.minimum_top_score
        || evidence_count < scorecard.minimum_evidence_count
        || margin < scorecard.minimum_margin
    {
        return (
            TaskFamily::Unknown,
            scorecard.unknown_confidence_ppm as f32 / 1_000_000.0,
            Vec::new(),
        );
    }

    accepted_task_family(best_family, scorecard, top_score)
}

fn accepted_task_family(
    family: TaskFamily,
    scorecard: &TaskFamilyScorecard,
    score: i16,
) -> (TaskFamily, f32, Vec<PredictiveEvidence>) {
    let evidence = (scorecard.maximum_evidence > 0)
        .then(|| PredictiveEvidence {
            code: task_family_reason_code(family).to_owned(),
            weight: score,
            confidence: scorecard.confidence_ppm as f32 / 1_000_000.0,
        })
        .into_iter()
        .collect();
    (
        family,
        scorecard.confidence_ppm as f32 / 1_000_000.0,
        evidence,
    )
}

fn task_family_instruction_text(prompt: &Prompt, normalized_plain_text_history: bool) -> String {
    latest_causal_instruction_text(prompt, normalized_plain_text_history)
}

fn contains_any_task_terms<T: AsRef<str>>(text: &str, terms: &[T]) -> bool {
    terms
        .iter()
        .any(|term| contains_task_term(text, term.as_ref()))
}

fn contains_task_term(text: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }

    text.match_indices(term).any(|(start, _)| {
        let end = start + term.len();
        task_term_boundary(text[..start].chars().next_back())
            && task_term_boundary(text[end..].chars().next())
    })
}

fn task_term_boundary(character: Option<char>) -> bool {
    character.is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
}

fn add_task_family_bonus(scores: &mut BTreeMap<TaskFamily, i16>, family: TaskFamily, bonus: i16) {
    let score = scores.entry(family).or_default();
    *score = score.saturating_add(bonus);
}

fn task_family_reason_code(family: TaskFamily) -> &'static str {
    match family {
        TaskFamily::CodeGeneration => "task_code_generation",
        TaskFamily::CodeDebugging => "task_code_debugging",
        TaskFamily::CodeReview => "task_code_review",
        TaskFamily::CodeSqlDatabase => "task_code_sql_database",
        TaskFamily::CodeFrontendUi => "task_code_frontend_ui",
        TaskFamily::CodeDevopsConfig => "task_code_devops_config",
        TaskFamily::CodeRepositoryAnalysis => "task_code_repository_analysis",
        TaskFamily::AgentMultiStepPlanning => "task_agent_multi_step_planning",
        TaskFamily::AgentWorkflowExecution => "task_agent_workflow_execution",
        TaskFamily::AgentWebResearch => "task_agent_web_research",
        TaskFamily::AgentMemoryOperations => "task_agent_memory_operations",
        TaskFamily::AgentGeneral => "task_agent_general",
        TaskFamily::Unknown => "task_unknown",
    }
}

/// Returns true only for a bounded categorical task-family reason emitted by
/// the deterministic classifier.
pub fn is_task_family_reason_code(code: &str) -> bool {
    matches!(
        code,
        "task_code_generation"
            | "task_code_debugging"
            | "task_code_review"
            | "task_code_sql_database"
            | "task_code_frontend_ui"
            | "task_code_devops_config"
            | "task_code_repository_analysis"
            | "task_agent_multi_step_planning"
            | "task_agent_workflow_execution"
            | "task_agent_web_research"
            | "task_agent_memory_operations"
            | "task_agent_general"
            | "task_unknown"
    )
}

pub fn compiled_scorecard_digest() -> &'static str {
    COMPILED_SCORECARD_DIGEST
}

pub fn compiled_predictor_contract() -> PredictorContract {
    PredictorContract {
        algorithm: PREDICTOR_ALGORITHM.to_owned(),
        version: 1,
        config_digest: COMPILED_SCORECARD_DIGEST.to_owned(),
        confidence_kind: PREDICTOR_CONFIDENCE_KIND.to_owned(),
        calibration_digest: None,
    }
}

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
    NarrowReadRequested,
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
        Self::NarrowReadRequested,
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
            Self::NarrowReadRequested => "narrow_read_requested",
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
    narrow_read: bool,
    finalize: bool,
    concrete: bool,
    contradictory: bool,
}

impl InstructionFeatures {
    fn has_signal(&self) -> bool {
        self.broad
            || self.mutate
            || self.verify
            || self.poll
            || self.narrow_read
            || self.finalize
            || self.concrete
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
    let scorecard = compiled_scorecard_v1();
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
        push_evidence(
            &mut evidence,
            code,
            scorecard.weight("incomplete_history"),
            scorecard.evidence_confidence(code),
        );
        return unknown_prediction(
            observed_projection,
            history.completeness,
            evidence,
            TaskFamily::Unknown,
            task_family_scorecard().unknown_confidence_ppm as f32 / 1_000_000.0,
            Vec::new(),
        );
    }

    let (task_family, task_family_confidence, task_family_evidence) =
        classify_task_family(prompt, observed.normalized_action_history.is_some());

    let mut scores = [0_i16; ROLE_COUNT];
    let opening = observed.state_kind == WorkflowStateKind::Opening && !history.has_trajectory;

    if instruction.broad {
        let weight = if opening {
            scorecard.weight("opening_broad")
        } else {
            scorecard.weight("continuing_broad")
        };
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            weight,
            &mut evidence,
            PredictiveReasonCode::OpeningBroadGoal,
            scorecard.evidence_confidence(PredictiveReasonCode::OpeningBroadGoal),
        );
    }
    if instruction.mutate {
        let weight = if instruction.concrete {
            scorecard.weight("concrete_mutation")
        } else {
            scorecard.weight("mutation")
        };
        let code = if instruction.concrete {
            PredictiveReasonCode::ConcreteMutationRequested
        } else {
            PredictiveReasonCode::MutationRequested
        };
        add_score(
            &mut scores,
            NextStepRole::Implement,
            weight,
            &mut evidence,
            code,
            scorecard.evidence_confidence(code),
        );
    }
    if instruction.verify {
        add_score(
            &mut scores,
            NextStepRole::Verify,
            scorecard.weight("verification"),
            &mut evidence,
            PredictiveReasonCode::VerificationRequested,
            scorecard.evidence_confidence(PredictiveReasonCode::VerificationRequested),
        );
    }
    if instruction.poll {
        add_score(
            &mut scores,
            NextStepRole::Mechanical,
            scorecard.weight("narrow_poll"),
            &mut evidence,
            PredictiveReasonCode::NarrowPollRequested,
            scorecard.evidence_confidence(PredictiveReasonCode::NarrowPollRequested),
        );
    }
    if instruction.narrow_read {
        add_score(
            &mut scores,
            NextStepRole::Mechanical,
            scorecard.weight("narrow_read"),
            &mut evidence,
            PredictiveReasonCode::NarrowReadRequested,
            scorecard.evidence_confidence(PredictiveReasonCode::NarrowReadRequested),
        );
    }
    if instruction.finalize {
        add_score(
            &mut scores,
            NextStepRole::Finalize,
            scorecard.weight("finalize"),
            &mut evidence,
            PredictiveReasonCode::FinalAnswerRequested,
            scorecard.evidence_confidence(PredictiveReasonCode::FinalAnswerRequested),
        );
    }

    match (history.last_action, history.last_failed) {
        (Some(ObservedAction::Read), false) => add_score(
            &mut scores,
            NextStepRole::Implement,
            scorecard.weight("read_result"),
            &mut evidence,
            PredictiveReasonCode::ReadResultAvailable,
            scorecard.evidence_confidence(PredictiveReasonCode::ReadResultAvailable),
        ),
        (Some(ObservedAction::Mutate), false) => add_score(
            &mut scores,
            NextStepRole::Verify,
            scorecard.weight("mutation_result"),
            &mut evidence,
            PredictiveReasonCode::MutationResultAvailable,
            scorecard.evidence_confidence(PredictiveReasonCode::MutationResultAvailable),
        ),
        (Some(ObservedAction::Test), true)
            if history.failure_count < scorecard.repeated_failure_count =>
        {
            add_score(
                &mut scores,
                NextStepRole::Implement,
                scorecard.weight("test_failed_once"),
                &mut evidence,
                PredictiveReasonCode::TestFailedOnce,
                scorecard.evidence_confidence(PredictiveReasonCode::TestFailedOnce),
            )
        }
        (Some(ObservedAction::Test), true) => add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            scorecard.weight("repeated_failure"),
            &mut evidence,
            PredictiveReasonCode::RepeatedFailure,
            scorecard.evidence_confidence(PredictiveReasonCode::RepeatedFailure),
        ),
        (Some(ObservedAction::Test), false) if history.successful_test_after_mutation => add_score(
            &mut scores,
            NextStepRole::Finalize,
            scorecard.weight("near_done"),
            &mut evidence,
            PredictiveReasonCode::ProgressNearDone,
            scorecard.evidence_confidence(PredictiveReasonCode::ProgressNearDone),
        ),
        (Some(ObservedAction::Test), false) => add_score(
            &mut scores,
            NextStepRole::Finalize,
            scorecard.weight("test_succeeded"),
            &mut evidence,
            PredictiveReasonCode::TestSucceeded,
            scorecard.evidence_confidence(PredictiveReasonCode::TestSucceeded),
        ),
        (Some(ObservedAction::Other), false) => {}
        (Some(ObservedAction::Read | ObservedAction::Mutate | ObservedAction::Other), true) => {
            add_score(
                &mut scores,
                NextStepRole::Implement,
                scorecard.weight("action_failed_once"),
                &mut evidence,
                PredictiveReasonCode::ActionFailedOnce,
                scorecard.evidence_confidence(PredictiveReasonCode::ActionFailedOnce),
            );
        }
        (None, _) => {}
    }

    if history.failure_count >= scorecard.repeated_failure_count
        && observed.recovery_signal == RecoverySignal::LikelyRecovery
    {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            scorecard.weight("recovery_pressure"),
            &mut evidence,
            PredictiveReasonCode::RecoveryPressure,
            scorecard.evidence_confidence(PredictiveReasonCode::RecoveryPressure),
        );
    }
    if observed.capability_constraints.expected_redo_penalty == RequirementLevel::High
        && history.failure_count >= scorecard.repeated_failure_count
    {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            scorecard.weight("redo_penalty"),
            &mut evidence,
            PredictiveReasonCode::RedoPenaltyHigh,
            scorecard.evidence_confidence(PredictiveReasonCode::RedoPenaltyHigh),
        );
    }
    if observed.capability_constraints.context_pressure == RequirementLevel::High {
        add_score(
            &mut scores,
            NextStepRole::Orchestrate,
            scorecard.weight("context_pressure"),
            &mut evidence,
            PredictiveReasonCode::ContextPressureHigh,
            scorecard.evidence_confidence(PredictiveReasonCode::ContextPressureHigh),
        );
    }
    if observed.capability_constraints.output_precision == RequirementLevel::High
        && history.last_action == Some(ObservedAction::Mutate)
    {
        add_score(
            &mut scores,
            NextStepRole::Verify,
            scorecard.weight("output_precision"),
            &mut evidence,
            PredictiveReasonCode::OutputPrecisionHigh,
            scorecard.evidence_confidence(PredictiveReasonCode::OutputPrecisionHigh),
        );
    }
    if observed.tool_density == ToolDensity::High
        && history.last_action == Some(ObservedAction::Read)
    {
        add_score(
            &mut scores,
            NextStepRole::Implement,
            scorecard.weight("tool_context"),
            &mut evidence,
            PredictiveReasonCode::ToolContextAvailable,
            scorecard.evidence_confidence(PredictiveReasonCode::ToolContextAvailable),
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
            scorecard.weight("trajectory_pressure"),
            &mut evidence,
            PredictiveReasonCode::TrajectoryPressure,
            scorecard.evidence_confidence(PredictiveReasonCode::TrajectoryPressure),
        );
    }

    let coverage = u8::from(instruction.has_signal())
        + u8::from(history.has_trajectory)
        + u8::from(observed.confidence > 0.0);
    let (role, top_score, runner_up) = choose_role(scores);
    let margin = top_score.saturating_sub(runner_up);
    if top_score < scorecard.minimum_top_score
        || margin < scorecard.minimum_margin
        || coverage < scorecard.minimum_coverage
    {
        push_evidence(
            &mut evidence,
            PredictiveReasonCode::ScoreMarginLow,
            scorecard.weight("low_margin"),
            scorecard.evidence_confidence(PredictiveReasonCode::ScoreMarginLow),
        );
        return unknown_prediction(
            observed_projection,
            history.completeness,
            evidence,
            task_family,
            task_family_confidence,
            task_family_evidence,
        );
    }

    let next_action_class = action_for_role(role, instruction.narrow_read);
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
    let route_risk = if history.failure_count >= scorecard.repeated_failure_count {
        RouteRisk::Guarded
    } else {
        observed_projection.risk
    };
    PredictiveRouteIR {
        schema_version: 2,
        observed: observed_projection,
        next_step_role: role,
        next_action_class,
        task_complexity,
        progress_state,
        history_completeness: history.completeness,
        route_risk,
        confidence: scorecard.confidence_for_margin(margin),
        evidence,
        predictor_contract_digest: compiled_scorecard_digest().to_owned(),
        confidence_kind: PREDICTOR_CONFIDENCE_KIND.to_owned(),
        task_family,
        task_family_confidence,
        task_family_evidence,
    }
}

fn instruction_features(
    prompt: &Prompt,
    normalized_plain_text_history: bool,
) -> InstructionFeatures {
    let behavior = compiled_predictor_behavior();
    let text = latest_causal_instruction_text(prompt, normalized_plain_text_history);
    let broad = contains_any(&text, behavior_terms(&behavior.instruction_terms, "broad"));
    let mutate = contains_any(&text, behavior_terms(&behavior.instruction_terms, "mutate"));
    let verify = contains_any(&text, behavior_terms(&behavior.instruction_terms, "verify"));
    let poll = contains_any(&text, behavior_terms(&behavior.instruction_terms, "poll"));
    let read_requested = contains_any(&text, behavior_terms(&behavior.instruction_terms, "read"));
    let finalize = !mutate
        && !verify
        && !read_requested
        && contains_any(
            &text,
            behavior_terms(&behavior.instruction_terms, "finalize"),
        );
    let concrete = has_concrete_evidence(&text);
    let narrow_read = read_requested && concrete && !broad && !mutate && !verify && !poll;
    let contradictory = mutate
        && contains_any(
            &text,
            behavior_terms(&behavior.instruction_terms, "contradiction"),
        );

    InstructionFeatures {
        broad,
        mutate,
        verify,
        poll,
        narrow_read,
        finalize,
        concrete,
        contradictory,
    }
}

fn latest_causal_instruction_text(prompt: &Prompt, normalized_plain_text_history: bool) -> String {
    let mut latest_user = None;
    let mut latest_system = None;
    let mut awaiting_plain_text_action_result = false;
    for message in &prompt.messages {
        match message.role {
            Role::Assistant if normalized_plain_text_history && is_assistant_action(message) => {
                awaiting_plain_text_action_result = true;
            }
            Role::User if awaiting_plain_text_action_result => {
                awaiting_plain_text_action_result = false;
            }
            Role::User => {
                if let Some(text) = message_text(message) {
                    latest_user = Some(text);
                }
            }
            Role::System => {
                if let Some(text) = message_text(message) {
                    latest_system = Some(text);
                }
            }
            _ => {}
        }
    }
    latest_user
        .or(latest_system)
        .or_else(|| prompt.system.clone())
        .unwrap_or_default()
        .to_ascii_lowercase()
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
    contains_any(text, &compiled_predictor_behavior().concrete_terms)
}

fn contains_any<T: AsRef<str>>(text: &str, terms: &[T]) -> bool {
    terms.iter().any(|term| text.contains(term.as_ref()))
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

pub(crate) fn authenticated_visible_causal_prefix(
    prompt: &Prompt,
    expected: &bitrouter_sdk::language_model::protocol::responses::CausalPrefixCommitment,
) -> Option<usize> {
    if !has_complete_visible_causal_history(prompt) {
        return None;
    }
    bitrouter_sdk::language_model::protocol::responses::find_unique_causal_prefix(
        &prompt.messages,
        expected,
    )
}

fn bounded_signal_count(count: u8) -> u8 {
    count.saturating_add(1).min(MAX_HISTORY_SIGNAL_COUNT)
}

fn classify_action(name: &str, arguments: &str, observed: &WorkflowStateIR) -> ObservedAction {
    let behavior = compiled_predictor_behavior();
    let name = name.to_ascii_lowercase();
    if contains_any(&name, behavior_terms(&behavior.tool_name_terms, "mutate")) {
        return ObservedAction::Mutate;
    }
    if contains_any(&name, behavior_terms(&behavior.tool_name_terms, "read")) {
        return ObservedAction::Read;
    }
    if contains_any(&name, behavior_terms(&behavior.tool_name_terms, "test")) {
        return ObservedAction::Test;
    }
    if contains_any(&name, behavior_terms(&behavior.tool_name_terms, "command"))
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
    contains_any(command, &compiled_predictor_behavior().command_test_terms)
}

fn command_is_read(command: &str) -> bool {
    let command = command.trim_start();
    compiled_predictor_behavior()
        .command_read_prefixes
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
    let roles = &compiled_predictor_behavior().role_tie_order;
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

fn action_for_role(role: NextStepRole, narrow_read: bool) -> NextActionClass {
    let behavior = compiled_predictor_behavior();
    if role == NextStepRole::Mechanical && narrow_read {
        return behavior.narrow_read_action;
    }
    behavior
        .role_actions
        .get(role.key())
        .copied()
        .unwrap_or(NextActionClass::Unknown)
}

fn push_evidence(
    evidence: &mut Vec<PredictiveEvidence>,
    code: PredictiveReasonCode,
    weight: i16,
    confidence: f32,
) {
    if evidence.len() < compiled_scorecard_v1().maximum_evidence
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
    task_family: TaskFamily,
    task_family_confidence: f32,
    task_family_evidence: Vec<PredictiveEvidence>,
) -> PredictiveRouteIR {
    PredictiveRouteIR {
        schema_version: 2,
        route_risk: observed.risk,
        observed,
        next_step_role: NextStepRole::Unknown,
        next_action_class: NextActionClass::Unknown,
        task_complexity: TaskComplexity::Ambiguous,
        progress_state: ProgressState::Unknown,
        history_completeness,
        confidence: compiled_scorecard_v1().unknown_confidence(),
        evidence,
        predictor_contract_digest: compiled_scorecard_digest().to_owned(),
        confidence_kind: PREDICTOR_CONFIDENCE_KIND.to_owned(),
        task_family,
        task_family_confidence,
        task_family_evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role, ToolResultOutput,
    };
    use sha2::{Digest, Sha256};

    use crate::workflow_state::extractors::generic::GenericPromptExtractor;
    use crate::workflow_state::extractors::{
        ExtractorInput, WorkflowStateExtractor, extract_workflow_state,
    };

    fn predictor_behavior_digest(behavior: &PredictorBehaviorV1) -> anyhow::Result<String> {
        Ok(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(serde_json::to_vec(behavior)?))
        ))
    }
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
        let projection = PredictiveRouteProjection::new(
            TaskFamily::CodeGeneration,
            NextStepRole::Implement,
            RouteRisk::Normal,
        );

        assert_eq!(
            projection.key(),
            "agent_route/v1|code:generation|implement|normal"
        );
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(CanonicalPolicyProjection::parse_key("agent_trace/v2|edit|normal").is_some());
        assert!(CanonicalPolicyProjection::parse_key(&projection.key()).is_some());
        assert!(CanonicalPolicyProjection::parse_key("agent_route/v1|implement|normal").is_none());
        assert!(CanonicalPolicyProjection::parse_key(
            "agent_route/v2|code:review|verify|normal"
        )
        .is_none());
    }

    #[test]
    fn predictive_projection_has_one_v1_task_aware_shape() {
        let projection = PredictiveRouteProjection::new(
            TaskFamily::CodeReview,
            NextStepRole::Verify,
            RouteRisk::Normal,
        );

        assert_eq!(
            projection.key(),
            "agent_route/v1|code:review|verify|normal"
        );
        assert_eq!(
            projection.unknown_baseline().key(),
            "agent_route/v1|unknown|verify|normal"
        );
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(PredictiveRouteProjection::parse_key("agent_route/v1|verify|normal").is_none());
        assert!(PredictiveRouteProjection::parse_key(
            "agent_route/v2|code:review|verify|normal"
        )
        .is_none());
    }

    #[test]
    fn predictive_projection_round_trips_exactly() {
        let projection = PredictiveRouteProjection::new(
            TaskFamily::CodeReview,
            NextStepRole::Verify,
            RouteRisk::Normal,
        );

        assert_eq!(projection.schema_version(), 1);
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(
            serde_json::from_str::<PredictiveRouteProjection>(
                r#"{"task_family":"code_review","next_step_role":"verify","risk":"normal","compatibility":true}"#
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
        assert!(matches!(
            serde_json::to_string(&TaskFamily::CodeDebugging),
            Ok(value) if value == "\"code_debugging\""
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
        let projection = PredictiveRouteProjection::new(
            TaskFamily::CodeDebugging,
            NextStepRole::Implement,
            RouteRisk::Normal,
        );
        let key = projection.key();

        assert_eq!(key, "agent_route/v1|code:debugging|implement|normal");
        for source_identity in ["codex", "claude_code", "hermes", "smithers", "openclaw"] {
            assert!(!key.contains(source_identity), "{source_identity}");
        }
    }

    #[test]
    fn predicts_roles_from_http_native_history() -> anyhow::Result<()> {
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
            let (ir, prompt) = fixture_input(fixture_text)
                .ok_or_else(|| anyhow::anyhow!("invalid prediction fixture: {name}"))?;
            let prediction = predict_next_step(&ir, &prompt);

            assert_eq!(prediction.next_step_role, expected_role, "{name}");
            assert_eq!(prediction.next_action_class, expected_action, "{name}");
            assert_eq!(prediction.route_risk, expected_risk, "{name}");
        }
        Ok(())
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
    fn authenticated_parent_requires_one_exact_last_assistant_turn() -> anyhow::Result<()> {
        use bitrouter_sdk::language_model::protocol::responses::{
            assistant_turn_commitment, extend_causal_prefix,
        };

        let opening = Message::text(Role::User, "opening");
        let parent = Message::text(Role::Assistant, "the exact parent response");
        let delivered = assistant_turn_commitment(&parent.content)
            .ok_or_else(|| anyhow::anyhow!("parent commitment missing"))?;
        let commitment = extend_causal_prefix(None, std::slice::from_ref(&opening), &delivered)
            .ok_or_else(|| anyhow::anyhow!("causal prefix missing"))?;
        let exact = prompt(vec![
            opening.clone(),
            parent.clone(),
            Message::text(Role::User, "follow up"),
        ]);
        let unrelated = prompt(vec![
            Message::text(Role::User, "opening"),
            Message::text(Role::Assistant, "an unrelated response"),
            Message::text(Role::User, "follow up"),
        ]);
        let tampered = prompt(vec![
            Message::text(Role::User, "opening"),
            Message::text(Role::Assistant, "the exact parent response!"),
            Message::text(Role::User, "follow up"),
        ]);
        let ambiguous = prompt(vec![
            Message::text(Role::User, "opening"),
            parent.clone(),
            Message::text(Role::User, "middle"),
            parent,
            Message::text(Role::User, "follow up"),
        ]);

        assert_eq!(
            authenticated_visible_causal_prefix(&exact, &commitment),
            Some(2)
        );
        for rejected in [unrelated, tampered, ambiguous] {
            assert_eq!(
                authenticated_visible_causal_prefix(&rejected, &commitment),
                None
            );
        }
        Ok(())
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
                named_prediction.task_family,
                named_prediction.next_step_role,
                named_prediction.route_risk
            ),
            PredictiveRouteProjection::new(
                renamed_prediction.task_family,
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
    fn predictor_preserves_prompt_tools_and_excludes_private_evidence() -> anyhow::Result<()> {
        let (ir, prompt) = fixture_input(POST_EDIT_VERIFY_FIXTURE)
            .ok_or_else(|| anyhow::anyhow!("invalid post-edit fixture"))?;
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
        Ok(())
    }

    #[test]
    fn compiled_predictor_contract_is_stable_and_heuristic() -> anyhow::Result<()> {
        let contract = compiled_predictor_contract();
        let recomputed_digest = predictor_behavior_digest(compiled_predictor_behavior())?;

        assert_eq!(contract.algorithm, "deterministic_scorecard");
        assert_eq!(contract.version, 1);
        assert_eq!(contract.confidence_kind, "heuristic_margin");
        assert_eq!(contract.calibration_digest, None);
        assert_eq!(contract.config_digest, compiled_scorecard_digest());
        assert_eq!(contract.config_digest, recomputed_digest);
        assert!(contract.config_digest.starts_with("sha256:"));
        assert_eq!(contract.config_digest.len(), 71);
        Ok(())
    }

    #[test]
    fn predictor_contract_digest_covers_every_task_classifier_component() -> anyhow::Result<()> {
        let mut changed_lexicon = compiled_predictor_behavior().clone();
        changed_lexicon
            .instruction_terms
            .get_mut("broad")
            .ok_or_else(|| anyhow::anyhow!("compiled broad lexicon is missing"))?
            .push("new broad signal".into());
        assert_ne!(
            predictor_behavior_digest(&changed_lexicon)?,
            compiled_scorecard_digest()
        );

        let mut changed_mapping = compiled_predictor_behavior().clone();
        changed_mapping
            .role_actions
            .insert("mechanical".into(), NextActionClass::ReasonOrPlan);
        assert_ne!(
            predictor_behavior_digest(&changed_mapping)?,
            compiled_scorecard_digest()
        );

        let mut task_changes = Vec::new();
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_terms
            .get_mut("code:debugging")
            .ok_or_else(|| anyhow::anyhow!("debugging terms missing"))?
            .push("new debug signal".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_modifier_families
            .get_mut("specific")
            .ok_or_else(|| anyhow::anyhow!("specific modifier group missing"))?
            .pop();
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_intent_terms
            .get_mut("debugging")
            .ok_or_else(|| anyhow::anyhow!("debugging intent terms missing"))?
            .push("new intent".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_failure_terms
            .get_mut("debugging")
            .ok_or_else(|| anyhow::anyhow!("debugging failure terms missing"))?
            .push("new failure".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_precedence_terms
            .get_mut("review")
            .ok_or_else(|| anyhow::anyhow!("review precedence terms missing"))?
            .push("new precedence".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_anchor_terms
            .get_mut("agent:workflow_execution")
            .ok_or_else(|| anyhow::anyhow!("workflow anchor terms missing"))?
            .push("new anchor".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed
            .task_family_code_subject_terms
            .push("new subject".into());
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed.task_family_scorecard.review_bonus += 1;
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed.task_family_tie_order.swap(0, 1);
        task_changes.push(changed);
        let mut changed = compiled_predictor_behavior().clone();
        changed.task_classifier_algorithm_version += 1;
        task_changes.push(changed);

        for changed in task_changes {
            assert_ne!(
                predictor_behavior_digest(&changed)?,
                compiled_scorecard_digest()
            );
        }
        Ok(())
    }

    #[test]
    fn predicts_only_specific_narrow_reads_as_mechanical_inspection() {
        let cases = [
            (
                "Read src/parser.rs and report the current error enum.",
                NextStepRole::Mechanical,
                NextActionClass::InspectOrRead,
            ),
            (
                "Inspect the repository and understand the architecture.",
                NextStepRole::Orchestrate,
                NextActionClass::ReasonOrPlan,
            ),
        ];

        for (instruction, expected_role, expected_action) in cases {
            let prompt = prompt(vec![Message::text(Role::User, instruction)]);
            let prediction = predict_next_step(&observed(&prompt), &prompt);
            assert_eq!(prediction.next_step_role, expected_role, "{instruction}");
            assert_eq!(
                prediction.next_action_class, expected_action,
                "{instruction}"
            );
        }
    }

    #[test]
    fn mutation_constraints_are_not_misread_as_instruction_contradictions() {
        let valid = prompt(vec![Message::text(
            Role::User,
            "Fix src/parser.rs without changing the public API or behavior.",
        )]);
        let contradictory = prompt(vec![Message::text(
            Role::User,
            "Fix src/parser.rs, but make no changes and only summarize it.",
        )]);

        let valid_prediction = predict_next_step(&observed(&valid), &valid);
        let contradictory_prediction = predict_next_step(&observed(&contradictory), &contradictory);

        assert_eq!(valid_prediction.next_step_role, NextStepRole::Implement);
        assert_eq!(valid_prediction.next_action_class, NextActionClass::Mutate);
        assert_eq!(
            contradictory_prediction.next_step_role,
            NextStepRole::Unknown
        );
        assert!(
            contradictory_prediction
                .evidence
                .iter()
                .any(|item| item.code == "instruction_contradiction")
        );
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

    #[test]
    fn predictive_projection_rejects_noncanonical_family_and_segments() {
        let projection = PredictiveRouteProjection::new(
            TaskFamily::CodeDebugging,
            NextStepRole::Implement,
            RouteRisk::Guarded,
        );

        assert_eq!(
            projection.key(),
            "agent_route/v1|code:debugging|implement|guarded"
        );
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(
            PredictiveRouteProjection::parse_key(
                "agent_route/v1|debugging|implement|guarded"
            )
            .is_none()
        );
        assert!(
            PredictiveRouteProjection::parse_key(
                "agent_route/v1|code:debugging|implement|guarded|extra"
            )
            .is_none()
        );
    }

    #[test]
    fn task_family_classifies_canonical_prompt_families() {
        let cases = [
            (
                "code generation",
                "Implement a new module and refactor the parser API.",
                TaskFamily::CodeGeneration,
            ),
            (
                "debugging outranks sql",
                "Fix the panic in the SQL migration runner after this regression failed.",
                TaskFamily::CodeDebugging,
            ),
            (
                "review outranks frontend",
                "Review this React pull request for security bugs and audit the diff.",
                TaskFamily::CodeReview,
            ),
            (
                "sql database",
                "Write a SQL schema migration and optimize the database query.",
                TaskFamily::CodeSqlDatabase,
            ),
            (
                "frontend ui",
                "Build a React component with CSS and fix the DOM layout.",
                TaskFamily::CodeFrontendUi,
            ),
            (
                "devops config",
                "Update the CI deployment configuration for the Kubernetes service.",
                TaskFamily::CodeDevopsConfig,
            ),
            (
                "repository analysis",
                "Analyze the repository dependencies and trace the codebase call graph.",
                TaskFamily::CodeRepositoryAnalysis,
            ),
            (
                "multi step planning",
                "Plan a multi-step agent handoff and decompose the architecture work.",
                TaskFamily::AgentMultiStepPlanning,
            ),
            (
                "workflow execution",
                "Orchestrate the agent workflow pipeline and execute the handoff.",
                TaskFamily::AgentWorkflowExecution,
            ),
            (
                "web research",
                "Browse the web for current sources and research the latest release.",
                TaskFamily::AgentWebResearch,
            ),
            (
                "memory operations",
                "Extract durable facts into memory and synthesize the saved context.",
                TaskFamily::AgentMemoryOperations,
            ),
            (
                "general agent",
                "Coordinate the agent task and manage its assistant work.",
                TaskFamily::AgentGeneral,
            ),
            (
                "action is not a family",
                "Run the shell command and report its output.",
                TaskFamily::Unknown,
            ),
        ];

        for (name, instruction, expected_family) in cases {
            let prompt = prompt(vec![Message::text(Role::User, instruction)]);
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.task_family, expected_family, "{name}");
            assert!(
                prediction.task_family_evidence.len() <= MAX_PREDICTIVE_EVIDENCE,
                "{name}"
            );
        }
    }

    #[test]
    fn task_family_review_intent_precedes_debugging_subject_terms() {
        let cases = [
            ("Review the bug fix.", TaskFamily::CodeReview),
            ("Audit the regression patch.", TaskFamily::CodeReview),
            ("Fix the bug.", TaskFamily::CodeDebugging),
            ("Repair a regression.", TaskFamily::CodeDebugging),
            ("Review the schedule.", TaskFamily::Unknown),
        ];

        for (instruction, expected) in cases {
            let prompt = prompt(vec![Message::text(Role::User, instruction)]);
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.task_family, expected, "{instruction}");
        }
    }

    #[test]
    fn normalized_history_skips_only_actual_action_results() {
        let prompt = prompt(vec![
            Message::text(Role::User, "Review the parser patch."),
            Message::text(Role::Assistant, "Which branch should I inspect?"),
            Message::text(Role::User, "Fix the regression in src/parser.rs."),
            Message::text(
                Role::Assistant,
                r#"{"commands":[{"keystrokes":"cargo test"}],"task_complete":false}"#,
            ),
            Message::text(Role::User, "All parser tests passed."),
        ]);
        let mut ir = observed(&prompt);
        ir.normalized_action_history = Some(crate::workflow_state::ir::NormalizedActionHistory {
            last_action: Some(NormalizedActionKind::Test),
            last_failed: false,
            failure_count: 0,
            mutation_count: 0,
            complete: true,
        });

        let prediction = predict_next_step(&ir, &prompt);

        assert_eq!(prediction.task_family, TaskFamily::CodeDebugging);
    }

    #[test]
    fn task_family_workflow_requires_an_orchestration_anchor() {
        let cases = [
            (
                "Execute the shell pipeline and report its output.",
                TaskFamily::Unknown,
            ),
            (
                "Execute the file pipeline and report its output.",
                TaskFamily::Unknown,
            ),
            (
                "Dispatch the tool pipeline and report its output.",
                TaskFamily::Unknown,
            ),
            (
                "Orchestrate the agent pipeline.",
                TaskFamily::AgentWorkflowExecution,
            ),
            (
                "Hand off the workflow to another agent.",
                TaskFamily::AgentWorkflowExecution,
            ),
        ];

        for (instruction, expected) in cases {
            let prompt = prompt(vec![Message::text(Role::User, instruction)]);
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.task_family, expected, "{instruction}");
        }
    }

    #[test]
    fn task_family_excludes_harness_and_private_identity_metadata() {
        let base_prompt = prompt(vec![Message::text(
            Role::User,
            "Fix generated-task-a in src/solver.rs after the parser panic failed.",
        )]);
        let renamed_prompt = prompt(vec![Message::text(
            Role::User,
            "Fix generated-task-b in src/solver.rs after the parser panic failed.",
        )]);
        let mut private_ir = observed(&base_prompt);
        private_ir.harness_id = HarnessId::Codex;
        private_ir.active_workflow = Some("private-release-workflow".to_owned());
        private_ir.subagent_role = Some("generated-reviewer".to_owned());
        let mut adapter_ir = observed(&renamed_prompt);
        adapter_ir.harness_id = HarnessId::Terminus2;
        adapter_ir.active_workflow = Some("another-private-workflow".to_owned());
        adapter_ir.subagent_role = Some("generated-implementer".to_owned());

        let private_prediction = predict_next_step(&private_ir, &base_prompt);
        let adapter_prediction = predict_next_step(&adapter_ir, &renamed_prompt);

        assert_eq!(private_prediction.task_family, TaskFamily::CodeDebugging);
        assert_eq!(
            private_prediction.task_family,
            adapter_prediction.task_family
        );
        assert_eq!(
            private_prediction.task_family_evidence,
            adapter_prediction.task_family_evidence
        );
    }

    #[test]
    fn task_family_does_not_match_short_terms_inside_action_words() {
        let cases = [
            (
                "dom and ui inside random and build",
                "Run the random build command and report its output.",
            ),
            (
                "ci fix and error inside civic prefix and terror",
                "Run the civic prefix terror command and report its output.",
            ),
        ];

        for (name, instruction) in cases {
            let prompt = prompt(vec![Message::text(Role::User, instruction)]);
            let prediction = predict_next_step(&observed(&prompt), &prompt);

            assert_eq!(prediction.task_family, TaskFamily::Unknown, "{name}");
            assert!(prediction.task_family_evidence.is_empty(), "{name}");
        }
    }
}
