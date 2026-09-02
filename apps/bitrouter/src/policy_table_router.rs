//! Config-driven per-request model routing — the `policy_table:` section.
//!
//! [`PolicyTableRouter`] is an ingress [`PromptTransform`] that picks the model
//! for each request from a static, operator-owned policy table instead of
//! taking the caller's requested model at face value. It is deterministic and
//! does no inference: it derives a coarse *fingerprint* of the request from the
//! canonical [`Prompt`] (the agent-loop step), looks the fingerprint up to get a
//! *tier*, maps the tier to a model id, and rewrites `prompt.model`.
//!
//! Two design points carry over from the sibling [`crate::claude_code`] router:
//!
//! - It lives in the app layer (not the SDK), because the decision needs the
//!   parsed [`Prompt`], which only exists above the SDK ingress seam, and its
//!   config is wired in [`crate::assemble`].
//! - It is idempotent and self-no-ops on anything it does not own. An explicit
//!   `provider:model` route — including the `claude-code:` subscription route
//!   the Claude Code router emits just before this one — always wins: such a
//!   request is left untouched. So is a request driven by a server-tool flow
//!   (it carries a bitrouter server-tool declaration, e.g. the `bitrouter/fusion`
//!   alias's injected tool), a request already on its tier's model, and one
//!   whose fingerprint resolves to no tier.
//!
//! The policy table is purely declarative and never mutated at runtime; it is
//! the kind of thing an operator keeps under version control.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use bitrouter_sdk::config::{PolicyKeyStrategy, PolicyModelTarget, PolicyTableConfig};
use bitrouter_sdk::language_model::types::{Content, Prompt, ReasoningEffort, Role, Tool};
use bitrouter_sdk::{HeaderMap, PromptTransform};

use crate::continuation::ContinuationAdjustment;
use crate::eval::settlement::{
    EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore, bounded_continuation_label,
};
use crate::eval::types::{EvalExperimentRef, ExperimentArm};
use crate::optimization::exploration::RouteExploration;
use crate::trajectory::guard::ProgressGuardPolicy;
use crate::trajectory::types::HistoryCompleteness;
use crate::workflow_state::decision::{PolicyDecisionJsonlRecorder, PolicyDecisionRecord};
use crate::workflow_state::ir::{HarnessId, WorkflowIdentity};
use crate::workflow_state::online::OnlineWorkflowState;
use crate::workflow_state::predictive::{
    NextActionClass, NextStepRole, PredictiveEvidence, is_predictive_reason_code,
    is_task_family_reason_code,
};
use crate::workflow_state::session::WorkflowIdentityTracker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecisionReason {
    StaticTable,
    ToolGuardrail,
    ProgressGuard,
    ProgressGuardToolGuardrail,
    ContinuationPin,
    NoMatch,
}

impl PolicyDecisionReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::StaticTable => "static_table",
            Self::ToolGuardrail => "tool_guardrail",
            Self::ProgressGuard => "progress_guard",
            Self::ProgressGuardToolGuardrail => "progress_guard_tool_guardrail",
            Self::ContinuationPin => "continuation_pin",
            Self::NoMatch => "no_match",
        }
    }
}

impl fmt::Display for PolicyDecisionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub key_strategy: PolicyKeyStrategy,
    pub request_key: String,
    pub route_projection: String,
    pub observed_route_projection: String,
    pub legacy_fingerprint: String,
    pub workflow_state_kind: String,
    pub harness_id: HarnessId,
    pub workflow_identity: WorkflowIdentity,
    pub input_effort: Option<ReasoningEffort>,
    pub static_tier: Option<String>,
    pub static_model: Option<String>,
    pub static_effort: Option<ReasoningEffort>,
    pub selected_tier: Option<String>,
    pub selected_model: Option<String>,
    pub selected_effort: Option<ReasoningEffort>,
    pub continuation_proposed_tier: Option<String>,
    pub continuation_proposed_model: Option<String>,
    pub continuation_proposed_effort: Option<ReasoningEffort>,
    pub continuation_adjustment: Option<String>,
    pub predicted_role: Option<String>,
    pub predicted_task_family: Option<String>,
    pub predicted_action: Option<String>,
    pub prediction_confidence_ppm: Option<u32>,
    pub task_family_confidence_ppm: Option<u32>,
    pub predictor_contract_digest: Option<String>,
    pub prediction_confidence_kind: Option<String>,
    pub prediction_reason_codes: Vec<String>,
    pub task_family_reason_codes: Vec<String>,
    pub reason: PolicyDecisionReason,
    pub pinned: bool,
    pub request_qualified: bool,
    pub semantic_successes: u32,
    pub semantic_success_threshold: u32,
    pub locked: bool,
    pub trialed: bool,
    pub trajectory_episode_id: Option<String>,
    pub trajectory_sequence: Option<u64>,
    pub trajectory_completeness: Option<HistoryCompleteness>,
    pub trajectory_health_digest: Option<String>,
    pub progress_candidate_tier: Option<String>,
    pub progress_clause_ids: Vec<String>,
    pub experiment: Option<EvalExperimentRef>,
}

impl PolicyDecision {
    /// Canonical decision records call this value `trace_state`; retain the
    /// established Rust field while exposing the canonical terminology.
    pub fn trace_state(&self) -> &str {
        &self.workflow_state_kind
    }

    /// Canonical decision records call this value `trace_identity`; retain the
    /// established Rust field while exposing the canonical terminology.
    pub fn trace_identity(&self) -> &WorkflowIdentity {
        &self.workflow_identity
    }
}

/// The resolved, immutable policy spec: fingerprint→tier→model plus tool-use
/// guardrails. Learned-state databases never participate in this hot path.
pub struct PolicyTable {
    /// Tier name → model id the request is rewritten to.
    tiers: HashMap<String, PolicyModelTarget>,
    /// Request fingerprint → tier name.
    fingerprints: HashMap<String, String>,
    /// Tier for a fingerprint absent from `fingerprints`.
    default_tier: Option<String>,
    /// Guardrail target tier for tool-carrying requests whose chosen tier is not
    /// tool-safe.
    tool_use_tier: Option<String>,
    /// Tiers that handle tool calls reliably.
    tool_safe_tiers: Vec<String>,
}

impl PolicyTable {
    /// Build the shared spec from config, or `None` when the section is inert
    /// (no tiers defined).
    pub fn from_config(config: &PolicyTableConfig) -> Option<Arc<Self>> {
        if config.tiers.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            tiers: config.tiers.clone(),
            fingerprints: config.fingerprints.clone(),
            default_tier: config.default_tier.clone(),
            tool_use_tier: config.tool_use_tier.clone(),
            tool_safe_tiers: config.tool_safe_tiers.clone(),
        }))
    }

    /// A spec that routes nothing.
    ///
    /// What a reload installs when the fresh config defines no tiers. The
    /// transform is baked into the built `App` and cannot be unregistered, so
    /// "no policy table" has to be expressible as a table: every lookup misses,
    /// so `route_prompt` leaves the caller's model alone.
    pub fn inert() -> Arc<Self> {
        Arc::new(Self {
            tiers: HashMap::new(),
            fingerprints: HashMap::new(),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
        })
    }

    /// Resolve the exact task-aware predictive key, its unknown-family
    /// baseline, then the default. The returned key is the key that actually
    /// selected the tier, except that defaults retain the primary key.
    fn tier_for_workflow<'table, 'key>(
        &'table self,
        predictive_primary: &'key str,
        unknown_baseline: &'key str,
    ) -> Option<(&'table str, &'key str)> {
        if let Some(tier) = self.fingerprints.get(predictive_primary) {
            return Some((tier.as_str(), predictive_primary));
        }
        if let Some(tier) = self.fingerprints.get(unknown_baseline) {
            return Some((tier.as_str(), unknown_baseline));
        }
        self.default_tier
            .as_deref()
            .map(|tier| (tier, predictive_primary))
    }

    fn guardrail_with_status<'a>(&'a self, tier: &'a str, prompt: &Prompt) -> (&'a str, bool) {
        if !prompt.tools.is_empty()
            && !self.tool_safe_tiers.iter().any(|t| t == tier)
            && let Some(floor) = self.tool_use_tier.as_deref()
        {
            return (floor, floor != tier);
        }
        (tier, false)
    }

    /// The model id a tier routes to.
    pub(crate) fn model_of_tier(&self, tier: &str) -> Option<&str> {
        self.target_of_tier(tier).map(PolicyModelTarget::model)
    }

    /// The policy-owned effort override for a tier, when the target is
    /// structured. Scalar targets intentionally preserve caller effort.
    pub(crate) fn effort_of_tier(&self, tier: &str) -> Option<ReasoningEffort> {
        self.target_of_tier(tier)
            .and_then(PolicyModelTarget::effort)
    }

    /// The complete model/effort target selected by a tier.
    pub(crate) fn target_of_tier(&self, tier: &str) -> Option<&PolicyModelTarget> {
        self.tiers.get(tier)
    }

    fn stable_tier_of_target(&self, model: &str, effort: Option<ReasoningEffort>) -> Option<&str> {
        self.tiers
            .iter()
            .filter_map(|(tier, candidate)| {
                (candidate.model() == model && candidate.effort() == effort)
                    .then_some(tier.as_str())
            })
            .min()
            .or_else(|| {
                self.tiers
                    .iter()
                    .filter_map(|(tier, candidate)| {
                        (candidate.model() == model && candidate.effort().is_none())
                            .then_some(tier.as_str())
                    })
                    .min()
            })
    }

    /// A coarse fingerprint of the agent-loop step, derived purely from the
    /// prompt body (so it is stable regardless of the inbound protocol). It
    /// classifies the request by the model's *most recent* turn:
    ///
    /// - `after_<tool>` — the model's last turn called `<tool>` (the request is
    ///   most likely the follow-up that feeds the tool result back). This is the
    ///   common in-loop step.
    /// - `midstream` — the model's last turn was a plain reply with no tool call
    ///   (e.g. it answered, then the user sent a fresh instruction). Keying on
    ///   the *most recent* turn — rather than the last tool call anywhere in the
    ///   history — is what keeps a request that has moved past a tool turn from
    ///   being misread as the `after_<tool>` step.
    /// - `opening` — the model has taken no turn yet (the first request).
    ///
    /// When a turn makes several tool calls at once, the last call in the turn
    /// names the step. The fingerprint reads [`Content::ToolCall`] (whose name
    /// is always present) rather than a [`Content::ToolResult`] (whose name is
    /// wire-dependent and often absent).
    pub fn fingerprint(prompt: &Prompt) -> String {
        // Walk back to the model's most recent turn and classify by it.
        for message in prompt.messages.iter().rev() {
            if message.role != Role::Assistant {
                continue;
            }
            let last_call = message
                .content
                .iter()
                .rev()
                .find_map(|content| match content {
                    Content::ToolCall { name, .. } => Some(name.as_str()),
                    _ => None,
                });
            return match last_call {
                Some(name) => format!("after_{name}"),
                None => "midstream".to_string(),
            };
        }
        "opening".to_string()
    }
}

/// Ingress [`PromptTransform`] that rewrites `prompt.model` per a [`PolicyTable`]
/// keyed on a per-request fingerprint, with a hard tool-use guardrail.
///
/// Build it from [`PolicyTableConfig`] via [`PolicyTableRouter::from_config`]
/// (`None` when no tiers are defined) or [`PolicyTableRouter::new`].
pub struct PolicyTableRouter {
    /// Swappable so `bitrouter reload` can install a fresh spec into the
    /// *live* transform. The router itself is baked into the built `App` and
    /// cannot be re-registered, so the reloadable unit has to be the table
    /// inside it rather than the router around it.
    ///
    /// A plain `RwLock` rather than a channel or an async lock: reads happen
    /// on the per-request hot path from [`PromptTransform::apply`], which is
    /// synchronous, and writes happen once per reload.
    table: std::sync::RwLock<Arc<PolicyTable>>,
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    state_namespace: Option<String>,
    identity_tracker: WorkflowIdentityTracker,
    eval_observer: Option<EvalDecisionObserver>,
    progress_guard: Option<ProgressGuardPolicy>,
    exploration: Option<RouteExploration>,
}

#[derive(Clone)]
struct EvalDecisionObserver {
    pending: Option<PendingEvalDecisionStore>,
    policy: String,
    policy_digest: String,
    route_baselines: HashMap<String, String>,
    default_baseline: Option<String>,
}

impl PolicyTableRouter {
    /// Build a static router from the `policy_table:` config, or `None` when the
    /// section is inert (no tiers defined) — mirroring
    /// `FusionAliasConfig::from_settings`, so an unconfigured deployment wires no
    /// transform. No adequacy ledger is attached.
    pub fn from_config(config: &PolicyTableConfig) -> Option<Self> {
        PolicyTable::from_config(config).map(|table| Self {
            table: std::sync::RwLock::new(table),
            decision_recorder: None,
            state_namespace: None,
            identity_tracker: WorkflowIdentityTracker::default(),
            eval_observer: None,
            progress_guard: None,
            exploration: None,
        })
    }

    /// The spec in force right now. Clones the `Arc` rather than handing out a
    /// borrow so a concurrent [`Self::replace_table`] cannot invalidate a
    /// decision already in progress: a request that started under the old
    /// table finishes under it.
    ///
    /// A poisoned lock yields the value anyway. The alternative is a panic on
    /// the request path over a writer that panicked while swapping a
    /// declarative table — the stale spec is strictly the better failure.
    fn table(&self) -> Arc<PolicyTable> {
        match self.table.read() {
            Ok(table) => Arc::clone(&table),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Install a freshly built spec into this live router (`bitrouter reload`).
    ///
    /// Returns whether the swap happened; a poisoned lock is reported rather
    /// than silently dropping the operator's new table.
    pub(crate) fn replace_table(&self, table: Arc<PolicyTable>) -> bool {
        match self.table.write() {
            Ok(mut current) => {
                *current = table;
                true
            }
            Err(_) => false,
        }
    }

    /// Build a router over an immutable policy table.
    pub fn new(table: Arc<PolicyTable>) -> Self {
        Self {
            table: std::sync::RwLock::new(table),
            decision_recorder: None,
            state_namespace: None,
            identity_tracker: WorkflowIdentityTracker::default(),
            eval_observer: None,
            progress_guard: None,
            exploration: None,
        }
    }

    pub fn with_decision_recorder(mut self, recorder: PolicyDecisionJsonlRecorder) -> Self {
        self.decision_recorder = Some(Arc::new(recorder));
        self
    }

    /// Namespace diagnostic decision records for a named policy.
    pub(crate) fn with_state_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.state_namespace = Some(namespace.into());
        self
    }

    pub(crate) fn with_shared_decision_recorder(
        mut self,
        recorder: Arc<PolicyDecisionJsonlRecorder>,
    ) -> Self {
        self.decision_recorder = Some(recorder);
        self
    }

    pub(crate) fn with_eval_observer(
        mut self,
        pending: PendingEvalDecisionStore,
        policy: impl Into<String>,
        policy_digest: impl Into<String>,
        route_baselines: HashMap<String, String>,
        default_baseline: Option<String>,
    ) -> Self {
        self.eval_observer = Some(EvalDecisionObserver {
            pending: Some(pending),
            policy: policy.into(),
            policy_digest: policy_digest.into(),
            route_baselines,
            default_baseline,
        });
        self
    }

    pub(crate) fn with_progress_guard(mut self, guard: Option<ProgressGuardPolicy>) -> Self {
        self.progress_guard = guard;
        self
    }

    pub(crate) fn with_exploration(mut self, exploration: Option<RouteExploration>) -> Self {
        self.exploration = exploration;
        self
    }

    pub(crate) fn progress_guard(&self) -> Option<&ProgressGuardPolicy> {
        self.progress_guard.as_ref()
    }

    pub(crate) fn eval_policy_name(&self) -> Option<&str> {
        self.eval_observer
            .as_ref()
            .map(|observer| observer.policy.as_str())
    }

    pub(crate) fn eval_baseline_tier(&self, decision: &PolicyDecision) -> Option<String> {
        if decision.request_key != decision.route_projection {
            return decision.static_tier.clone();
        }
        self.eval_observer
            .as_ref()
            .and_then(|observer| {
                observer
                    .route_baselines
                    .get(&decision.route_projection)
                    .or_else(|| observer.route_baselines.get(&decision.request_key))
                    .cloned()
                    .or_else(|| observer.default_baseline.clone())
            })
            .or_else(|| decision.static_tier.clone())
    }

    /// Returns an owned value rather than a borrow: the table it reads is
    /// swappable, so nothing inside it can outlive the call.
    pub(crate) fn tool_use_tier(&self) -> Option<String> {
        self.table().tool_use_tier.clone()
    }

    pub(crate) fn tool_safe_tiers(&self) -> std::collections::BTreeSet<String> {
        self.table().tool_safe_tiers.iter().cloned().collect()
    }

    pub(crate) fn effective_tier_efforts(
        &self,
        inherited: Option<ReasoningEffort>,
    ) -> std::collections::BTreeMap<String, ReasoningEffort> {
        self.table()
            .tiers
            .iter()
            .filter_map(|(tier, target)| {
                target
                    .effort()
                    .or(inherited)
                    .map(|effort| (tier.clone(), effort))
            })
            .collect()
    }

    pub(crate) fn effort_of_tier(&self, tier: &str) -> Option<ReasoningEffort> {
        self.table().effort_of_tier(tier)
    }

    fn ledger_key(&self, request_key: &str) -> String {
        self.state_namespace.as_ref().map_or_else(
            || request_key.to_string(),
            |ns| format!("{ns}\0{request_key}"),
        )
    }

    /// Apply the policy table to a prompt, returning whether the model was
    /// rewritten. A no-op (returns `false`) when the model is already explicitly
    /// routed, when the request carries a bitrouter server-tool declaration,
    /// when the fingerprint resolves to no tier, or when the prompt is already on
    /// the resolved tier's model.
    pub fn apply(&self, prompt: &mut Prompt) -> bool {
        self.route_prompt(prompt, &HeaderMap::new())
    }

    pub fn decision_for(&self, prompt: &Prompt, headers: &HeaderMap) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, true, true, true)
    }

    fn decision_for_inner(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        respect_explicit_route: bool,
        use_shared_identity_tracker: bool,
        apply_tool_floor: bool,
    ) -> PolicyDecision {
        let online = if use_shared_identity_tracker {
            OnlineWorkflowState::from_headers_with_tracker(headers, prompt, &self.identity_tracker)
        } else {
            OnlineWorkflowState::for_named_policy(headers, prompt)
        };
        let legacy_fingerprint = online.legacy_fingerprint().to_string();
        let primary_request_key = online.routing_key().to_string();
        let baseline_request_key = online.baseline_routing_key();
        let observed_route_projection = online.observed_routing_key().to_string();
        let mut decision = PolicyDecision {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            request_key: primary_request_key.clone(),
            route_projection: primary_request_key.clone(),
            observed_route_projection: observed_route_projection.clone(),
            legacy_fingerprint,
            workflow_state_kind: online.ir.state_kind.to_string(),
            harness_id: online.ir.harness_id.clone(),
            workflow_identity: online.ir.identity.clone(),
            input_effort: prompt.params.reasoning_effort,
            static_tier: None,
            static_model: None,
            static_effort: None,
            selected_tier: None,
            selected_model: None,
            selected_effort: None,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some(
                prediction_role_name(online.predictive.next_step_role).to_string(),
            ),
            predicted_task_family: Some(online.predictive.task_family.key().to_string()),
            predicted_action: Some(
                prediction_action_name(online.predictive.next_action_class).to_string(),
            ),
            prediction_confidence_ppm: Some(prediction_confidence_ppm(
                online.predictive.confidence,
            )),
            task_family_confidence_ppm: Some(prediction_confidence_ppm(
                online.predictive.task_family_confidence,
            )),
            predictor_contract_digest: Some(online.predictive.predictor_contract_digest.clone()),
            prediction_confidence_kind: Some(online.predictive.confidence_kind.clone()),
            prediction_reason_codes: prediction_reason_codes(&online.predictive.evidence),
            task_family_reason_codes: task_family_reason_codes(
                &online.predictive.task_family_evidence,
            ),
            reason: PolicyDecisionReason::NoMatch,
            pinned: false,
            request_qualified: false,
            semantic_successes: 0,
            semantic_success_threshold: 0,
            locked: false,
            trialed: false,
            trajectory_episode_id: None,
            trajectory_sequence: None,
            trajectory_completeness: None,
            trajectory_health_digest: None,
            progress_candidate_tier: None,
            progress_clause_ids: Vec::new(),
            experiment: None,
        };

        if (respect_explicit_route && is_explicitly_routed(&prompt.model))
            || carries_bitrouter_server_tool(prompt)
        {
            return decision;
        }

        // One snapshot for the whole decision: a reload landing mid-decision
        // must not let the tier and the model it maps to come from different
        // tables.
        let table = self.table();
        let Some((raw_static_tier, matched_request_key)) =
            table.tier_for_workflow(&primary_request_key, baseline_request_key)
        else {
            return decision;
        };
        decision.request_key = matched_request_key.to_string();
        decision.static_tier = Some(raw_static_tier.to_string());
        decision.static_model = table
            .model_of_tier(raw_static_tier)
            .map(ToString::to_string);
        decision.static_effort = table
            .effort_of_tier(raw_static_tier)
            .or(decision.input_effort);
        let (assigned_tier, experiment) = match self
            .exploration
            .as_ref()
            .filter(|exploration| exploration.target_request_key == decision.route_projection)
        {
            Some(exploration) => match exploration
                .assignment(&decision.workflow_identity)
                .ok()
                .flatten()
            {
                Some(assignment) => {
                    let tier = match assignment.arm {
                        ExperimentArm::Control => exploration.champion_tier.as_str(),
                        ExperimentArm::Challenger => exploration.challenger_tier.as_str(),
                    };
                    (tier, Some(assignment))
                }
                None => (exploration.champion_tier.as_str(), None),
            },
            None => (raw_static_tier, None),
        };
        decision.experiment = experiment;
        let (selected_tier, static_clamped) = if apply_tool_floor {
            table.guardrail_with_status(assigned_tier, prompt)
        } else {
            (assigned_tier, false)
        };
        decision.reason = if static_clamped {
            PolicyDecisionReason::ToolGuardrail
        } else {
            PolicyDecisionReason::StaticTable
        };

        decision.selected_tier = Some(selected_tier.to_string());
        decision.selected_model = table.model_of_tier(selected_tier).map(ToString::to_string);
        decision.selected_effort = table
            .effort_of_tier(selected_tier)
            .or(decision.input_effort);
        if decision.selected_model.is_none() {
            decision.reason = PolicyDecisionReason::NoMatch;
        }
        decision
    }

    fn route_prompt(&self, prompt: &mut Prompt, headers: &HeaderMap) -> bool {
        let input_model = prompt.model.clone();
        let input_effort = prompt.params.reasoning_effort;
        let decision = self.decision_for(prompt, headers);
        let selected =
            self.record_decision(input_model, input_effort, decision, headers, None, None);
        let Some(target) = selected else {
            return false;
        };
        let mut changed = prompt.model != target.model();
        prompt.model = target.model().to_string();
        if let Some(effort) = target.effort() {
            changed |= prompt.params.reasoning_effort != Some(effort);
            prompt.params.reasoning_effort = Some(effort);
            prompt.params.reasoning_effort_source =
                bitrouter_sdk::language_model::types::ReasoningEffortSource::Policy;
        }
        changed
    }

    /// Select a model for a preset that explicitly owns this policy. Unlike the
    /// legacy global transform, a provider-qualified preset base does not opt
    /// out: the preset binding itself is the caller's explicit routing intent.
    pub(crate) fn decision_for_bound_policy(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, false, false, true)
    }

    pub(crate) fn candidate_for_guarded_policy(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, false, false, false)
    }

    pub(crate) fn apply_guarded_route(
        &self,
        decision: &mut PolicyDecision,
        selected_tier: Option<&str>,
        guard_applied: bool,
        tool_floor_applied: bool,
    ) {
        decision.selected_tier = selected_tier.map(ToOwned::to_owned);
        let table = self.table();
        decision.selected_model = selected_tier
            .and_then(|tier| table.model_of_tier(tier))
            .map(ToOwned::to_owned);
        decision.selected_effort = selected_tier
            .and_then(|tier| table.effort_of_tier(tier))
            .or(decision.input_effort);
        decision.reason = match (guard_applied, tool_floor_applied) {
            (true, true) => PolicyDecisionReason::ProgressGuardToolGuardrail,
            (true, false) => PolicyDecisionReason::ProgressGuard,
            (false, true) => PolicyDecisionReason::ToolGuardrail,
            (false, false) if decision.selected_model.is_some() => {
                PolicyDecisionReason::StaticTable
            }
            (false, false) => PolicyDecisionReason::NoMatch,
        };
    }

    pub(crate) fn apply_continuation_adjustment(
        &self,
        decision: &mut PolicyDecision,
        adjustment: &ContinuationAdjustment,
    ) -> bitrouter_sdk::Result<()> {
        decision.continuation_proposed_tier = decision.selected_tier.clone();
        decision.continuation_proposed_model = decision.selected_model.clone();
        decision.continuation_proposed_effort = decision.selected_effort;
        match adjustment {
            ContinuationAdjustment::Pin {
                effective_model,
                effective_effort,
                effort_authoritative,
            } => {
                let pinned_effort = effort_authoritative
                    .then_some(*effective_effort)
                    .unwrap_or(decision.input_effort);
                // Through the snapshot, like every other lookup: a reload
                // landing here must not answer from a table the rest of this
                // decision never saw.
                let table = self.table();
                let selected_tier = table
                    .stable_tier_of_target(effective_model, pinned_effort)
                    .ok_or_else(|| {
                        bitrouter_sdk::BitrouterError::bad_request(
                            "provider continuation model is unavailable in the active policy",
                        )
                    })?;
                decision.selected_tier = Some(selected_tier.to_owned());
                decision.selected_model = Some(effective_model.clone());
                decision.selected_effort = pinned_effort;
                decision.continuation_adjustment = Some("pin".to_owned());
                decision.reason = PolicyDecisionReason::ContinuationPin;
                decision.pinned = true;
            }
            ContinuationAdjustment::Detach => {
                decision.continuation_adjustment = Some("detach".to_owned());
            }
            ContinuationAdjustment::RejectLegacy => {
                decision.continuation_adjustment = Some("reject_legacy".to_owned());
            }
        }
        Ok(())
    }

    pub(crate) fn record_bound_policy_decision(
        &self,
        request_id: &str,
        invocation: &EvalInvocation,
        input_model: String,
        input_effort: Option<ReasoningEffort>,
        decision: PolicyDecision,
        headers: &HeaderMap,
    ) -> Option<PolicyModelTarget> {
        self.record_decision(
            input_model,
            input_effort,
            decision,
            headers,
            Some(request_id),
            Some(invocation),
        )
    }

    fn record_decision(
        &self,
        input_model: String,
        input_effort: Option<ReasoningEffort>,
        decision: PolicyDecision,
        headers: &HeaderMap,
        request_id_override: Option<&str>,
        invocation: Option<&EvalInvocation>,
    ) -> Option<PolicyModelTarget> {
        let baseline_tier = self.eval_baseline_tier(&decision);
        let baseline_effort = baseline_tier
            .as_deref()
            .and_then(|tier| self.table().effort_of_tier(tier))
            .or(input_effort);
        let ingress_request_id = headers
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let request_id = request_id_override.or(ingress_request_id);
        let request_id_for_log = request_id.unwrap_or("-");
        tracing::info!(
            request_id = request_id_for_log,
            key_strategy = ?decision.key_strategy,
            request_key = %decision.request_key,
            legacy_fingerprint = %decision.legacy_fingerprint,
            trace_state = %decision.workflow_state_kind,
            trace_parent_session = ?decision.workflow_identity.parent_session_id,
            trace_agent_role = decision.workflow_identity.role.as_str(),
            trace_context_epoch = decision.workflow_identity.context_epoch,
            trace_session_fingerprint = %decision.workflow_identity.fingerprint,
            predicted_task_family = ?decision.predicted_task_family,
            task_family_confidence_ppm = ?decision.task_family_confidence_ppm,
            task_family_reason_codes = ?decision.task_family_reason_codes,
            input_effort = ?decision.input_effort,
            static_tier = ?decision.static_tier,
            static_model = ?decision.static_model,
            static_effort = ?decision.static_effort,
            selected_tier = ?decision.selected_tier,
            selected_model = ?decision.selected_model,
            selected_effort = ?decision.selected_effort,
            continuation_proposed_tier = ?decision.continuation_proposed_tier,
            continuation_proposed_model = ?decision.continuation_proposed_model,
            continuation_proposed_effort = ?decision.continuation_proposed_effort,
            continuation_adjustment = ?decision.continuation_adjustment,
            trajectory_episode_id = ?decision.trajectory_episode_id,
            trajectory_sequence = ?decision.trajectory_sequence,
            trajectory_completeness = ?decision.trajectory_completeness,
            trajectory_health_digest = ?decision.trajectory_health_digest,
            progress_candidate_tier = ?decision.progress_candidate_tier,
            progress_clause_ids = ?decision.progress_clause_ids,
            reason = %decision.reason,
            pinned = decision.pinned,
            request_qualified = decision.request_qualified,
            semantic_successes = decision.semantic_successes,
            semantic_success_threshold = decision.semantic_success_threshold,
            locked = decision.locked,
            trialed = decision.trialed,
            "policy routing decision"
        );
        if let (Some(observer), Some(invocation), Some(request_id), Some(selected_tier)) = (
            &self.eval_observer,
            invocation,
            request_id,
            decision.selected_tier.as_deref(),
        ) && let Some(pending) = &observer.pending
        {
            pending.insert(
                invocation,
                PendingEvalDecision {
                    request_id: request_id.to_string(),
                    decision_id: format!("{request_id}:{}", observer.policy),
                    policy: observer.policy.clone(),
                    policy_digest: observer.policy_digest.clone(),
                    route_projection: decision.route_projection.clone(),
                    request_key: decision.request_key.clone(),
                    selected_tier: selected_tier.to_string(),
                    selected_effort: decision.selected_effort,
                    baseline_tier: baseline_tier.clone(),
                    baseline_effort,
                    experiment: decision.experiment.clone(),
                    preset: Some(observer.policy.clone()),
                    holdout: false,
                    continuation_proposed_tier: bounded_continuation_label(
                        decision.continuation_proposed_tier.as_deref(),
                        128,
                    ),
                    continuation_proposed_model: bounded_continuation_label(
                        decision.continuation_proposed_model.as_deref(),
                        512,
                    ),
                    continuation_proposed_effort: decision.continuation_proposed_effort,
                    continuation_adjustment: bounded_continuation_label(
                        decision.continuation_adjustment.as_deref(),
                        32,
                    ),
                    predicted_role: decision.predicted_role.clone(),
                    predicted_task_family: decision.predicted_task_family.clone(),
                    predicted_action: decision.predicted_action.clone(),
                    prediction_confidence_ppm: decision.prediction_confidence_ppm,
                    task_family_confidence_ppm: decision.task_family_confidence_ppm,
                    task_family_reason_codes: decision.task_family_reason_codes.clone(),
                    predictor_contract_digest: decision.predictor_contract_digest.clone(),
                    prediction_confidence_kind: decision.prediction_confidence_kind.clone(),
                    observation: None,
                    observed_at: chrono::Utc::now().to_rfc3339(),
                },
            );
        }
        if let Some(recorder) = &self.decision_recorder {
            let record = PolicyDecisionRecord {
                captured_at: None,
                request_id: request_id.map(ToString::to_string),
                ingress_request_id_sha256: ingress_request_id
                    .map(crate::workflow_state::decision::ingress_request_id_sha256),
                input_model,
                input_effort,
                key_strategy: key_strategy_name().to_string(),
                route_projection: Some(decision.route_projection.clone()),
                request_key: decision.request_key.clone(),
                ledger_key: self
                    .state_namespace
                    .as_ref()
                    .map(|_| self.ledger_key(&decision.request_key)),
                policy: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                policy_digest: self
                    .eval_observer
                    .as_ref()
                    .map(|item| item.policy_digest.clone()),
                preset_variant: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                baseline_tier,
                baseline_effort,
                legacy_fingerprint: decision.legacy_fingerprint.clone(),
                workflow_state: decision.workflow_state_kind.clone(),
                workflow_identity: decision.workflow_identity.clone(),
                static_tier: decision.static_tier.clone(),
                static_model: decision.static_model.clone(),
                static_effort: decision.static_effort,
                selected_tier: decision.selected_tier.clone(),
                selected_model: decision.selected_model.clone(),
                selected_effort: decision.selected_effort,
                continuation_proposed_tier: decision.continuation_proposed_tier.clone(),
                continuation_proposed_model: decision.continuation_proposed_model.clone(),
                continuation_proposed_effort: decision.continuation_proposed_effort,
                continuation_adjustment: decision.continuation_adjustment.clone(),
                predicted_role: decision.predicted_role.clone(),
                predicted_task_family: decision.predicted_task_family.clone(),
                predicted_action: decision.predicted_action.clone(),
                prediction_confidence_ppm: decision.prediction_confidence_ppm,
                task_family_confidence_ppm: decision.task_family_confidence_ppm,
                predictor_contract_digest: decision.predictor_contract_digest.clone(),
                prediction_confidence_kind: decision.prediction_confidence_kind.clone(),
                prediction_reason_codes: decision.prediction_reason_codes.clone(),
                task_family_reason_codes: decision.task_family_reason_codes.clone(),
                observed_route_projection: Some(decision.observed_route_projection.clone()),
                trajectory_episode_id: decision.trajectory_episode_id.clone(),
                trajectory_sequence: decision.trajectory_sequence,
                trajectory_completeness: decision
                    .trajectory_completeness
                    .map(|value| match value {
                        HistoryCompleteness::Complete => "complete",
                        HistoryCompleteness::Incomplete => "incomplete",
                        HistoryCompleteness::Unknown => "unknown",
                    })
                    .map(ToOwned::to_owned),
                trajectory_health_digest: decision.trajectory_health_digest.clone(),
                candidate_tier: decision.progress_candidate_tier.clone(),
                progress_clause_ids: decision.progress_clause_ids.clone(),
                reason: decision.reason.to_string(),
                pinned: decision.pinned,
                request_qualified: decision.request_qualified,
                semantic_successes: decision.semantic_successes,
                semantic_success_threshold: decision.semantic_success_threshold,
                locked: decision.locked,
                trialed: decision.trialed,
            }
            .captured_now();
            if let Err(error) = recorder.record(&record) {
                tracing::warn!(%error, "policy decision recorder failed");
            }
        }
        let selected_model = decision.selected_model.as_deref()?;
        let selected_tier = decision.selected_tier.as_deref()?;
        let configured = self
            .table()
            .target_of_tier(selected_tier)
            .filter(|target| target.model() == selected_model)
            .cloned()?;
        if decision.continuation_adjustment.as_deref() == Some("pin")
            && configured.effort().is_none()
            && let Some(effort) = decision.selected_effort
        {
            return Some(PolicyModelTarget::ModelEffort {
                model: selected_model.to_owned(),
                effort,
            });
        }
        Some(configured)
    }
}

fn key_strategy_name() -> &'static str {
    "agent_trace"
}

const MAX_PREDICTION_REASON_CODES: usize = 8;
const PREDICTION_CONFIDENCE_PPM_MAX: u32 = 1_000_000;

fn prediction_role_name(role: NextStepRole) -> &'static str {
    match role {
        NextStepRole::Orchestrate => "orchestrate",
        NextStepRole::Implement => "implement",
        NextStepRole::Mechanical => "mechanical",
        NextStepRole::Verify => "verify",
        NextStepRole::Finalize => "finalize",
        NextStepRole::Unknown => "unknown",
    }
}

fn prediction_action_name(action: NextActionClass) -> &'static str {
    match action {
        NextActionClass::ReasonOrPlan => "reason_or_plan",
        NextActionClass::InspectOrRead => "inspect_or_read",
        NextActionClass::Mutate => "mutate",
        NextActionClass::ExecuteOrTest => "execute_or_test",
        NextActionClass::WaitOrPoll => "wait_or_poll",
        NextActionClass::AnswerOrSummarize => "answer_or_summarize",
        NextActionClass::Unknown => "unknown",
    }
}

fn prediction_confidence_ppm(confidence: f32) -> u32 {
    let scaled = f64::from(confidence) * f64::from(PREDICTION_CONFIDENCE_PPM_MAX);
    if scaled.is_nan() || scaled <= 0.0 {
        return 0;
    }
    if scaled >= f64::from(PREDICTION_CONFIDENCE_PPM_MAX) {
        return PREDICTION_CONFIDENCE_PPM_MAX;
    }
    let rounded = scaled.round() as u64;
    u32::try_from(rounded).unwrap_or(PREDICTION_CONFIDENCE_PPM_MAX)
}

fn prediction_reason_codes(evidence: &[PredictiveEvidence]) -> Vec<String> {
    evidence
        .iter()
        .map(|item| item.code.as_str())
        .filter(|code| is_predictive_reason_code(code))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_PREDICTION_REASON_CODES)
        .map(ToString::to_string)
        .collect()
}

fn task_family_reason_codes(evidence: &[PredictiveEvidence]) -> Vec<String> {
    evidence
        .iter()
        .map(|item| item.code.as_str())
        .filter(|code| is_task_family_reason_code(code))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_PREDICTION_REASON_CODES)
        .map(ToString::to_string)
        .collect()
}

impl PromptTransform for PolicyTableRouter {
    fn apply(&self, prompt: &mut Prompt) {
        PolicyTableRouter::apply(self, prompt);
    }

    fn apply_with_headers(&self, prompt: &mut Prompt, headers: &HeaderMap) {
        self.route_prompt(prompt, headers);
    }
}

/// Whether `model` already names an explicit upstream route or preset. A
/// `provider:model` id triggers Strategy 1; `@preset` and its public
/// `bitrouter/<slug>` spelling must both survive until Stage 0 can resolve
/// their prompt defaults, provider preferences, and named policy. The reserved
/// namespace carries neither an `@` nor a colon, so it needs naming here — a
/// `bitrouter/auto` request left to this table would be fingerprinted and
/// rewritten by the legacy inline policy table instead of reaching the signed
/// policy its slug addresses.
fn is_explicitly_routed(model: &str) -> bool {
    model.starts_with('@')
        || model.contains(':')
        || bitrouter_sdk::config::presets::is_reserved(model)
}

/// Whether the request carries a bitrouter server-tool declaration — a
/// provider-defined tool in the bitrouter namespace, e.g. the `fusion` tool the
/// `bitrouter/fusion` alias injects (id `bitrouter.fusion`) or a caller's
/// `{"type":"bitrouter:advisor"}`. Such a request is driven by a server-tool
/// flow that already chose its outer model, so the policy table leaves it alone.
fn carries_bitrouter_server_tool(prompt: &Prompt) -> bool {
    prompt.tools.iter().any(|tool| match tool {
        Tool::ProviderDefined { id, name, .. } => {
            id.starts_with("bitrouter.") || is_bitrouter_namespaced(name)
        }
        Tool::Function { .. } => false,
    })
}

/// Whether `name` carries the explicit `bitrouter:` / `bitrouter.` namespace —
/// the documented `{"type":"bitrouter:<tool>"}` server-tool declaration form, as
/// opposed to a bare or foreign-namespaced tool a provider defines itself.
fn is_bitrouter_namespaced(name: &str) -> bool {
    name.split_once([':', '.'])
        .is_some_and(|(namespace, _)| namespace == "bitrouter")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
    use crate::eval::settlement::EvalSettlementRecorder;
    use crate::eval::store::EvalStore;
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalVerdict, EvaluationResult, EvaluatorIdentity, EvaluatorKind,
    };
    use crate::metering::PricingTable;
    use crate::optimization::exploration::{OptimizationGate, RouteExploration};
    use crate::policy_compile::{CompileInput, LegacyAdequacySnapshot, compile_candidate};
    use crate::policy_lock::{PolicyLock, semantic_digest};
    use crate::trajectory::canonical::CorrelationKey;
    use crate::workflow_state::decision::PolicyDecisionJsonlRecorder;
    use crate::workflow_state::ir::{AgentRole, HarnessId, ProtocolKind};
    use crate::workflow_state::online::OnlineWorkflowState;
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::config::PolicyKeyStrategy;
    use bitrouter_sdk::event::EventBus;
    use bitrouter_sdk::language_model::types::{
        GenerationParams, Message, ProviderMetadata, Tool, ToolResultOutput,
    };
    use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, UsageOrigin};
    use http::HeaderValue;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A policy table with a cheap and a flagship tier: `opening` and tool-heavy
    /// steps stay flagship, a read step goes cheap, and only flagship is
    /// tool-safe.
    fn config() -> PolicyTableConfig {
        PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                ("cheap".to_string(), PolicyModelTarget::from("vendor/cheap")),
                (
                    "flagship".to_string(),
                    PolicyModelTarget::from("vendor/flagship"),
                ),
            ]),
            fingerprints: HashMap::from([
                (
                    "agent_route/v1|unknown|orchestrate|normal".to_string(),
                    "flagship".to_string(),
                ),
                (
                    "agent_route/v1|unknown|implement|normal".to_string(),
                    "cheap".to_string(),
                ),
            ]),
            default_tier: Some("flagship".to_string()),
            tool_use_tier: Some("flagship".to_string()),
            tool_safe_tiers: vec!["flagship".to_string()],
            adequacy: Default::default(),
        }
    }

    fn comparator_config() -> PolicyTableConfig {
        PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                (
                    "economy".to_string(),
                    PolicyModelTarget::from("vendor/economy"),
                ),
                (
                    "strong".to_string(),
                    PolicyModelTarget::from("vendor/strong"),
                ),
            ]),
            fingerprints: HashMap::from([
                (
                    "agent_route/v1|unknown|orchestrate|normal".to_string(),
                    "strong".to_string(),
                ),
                (
                    "agent_route/v1|unknown|implement|normal".to_string(),
                    "economy".to_string(),
                ),
            ]),
            default_tier: Some("strong".to_string()),
            tool_use_tier: Some("strong".to_string()),
            tool_safe_tiers: vec!["strong".to_string()],
            adequacy: Default::default(),
        }
    }

    fn router() -> PolicyTableRouter {
        PolicyTableRouter::from_config(&config()).expect("tiers are configured")
    }

    fn claude_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,tools-2024-05-16"),
        );
        headers
    }

    fn prompt(model: &str) -> Prompt {
        Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    /// The reserved namespace is an addressing form the signed policy owns, so
    /// the legacy inline table must leave it alone exactly as it leaves `@auto`
    /// alone. `bitrouter/auto` carries neither an `@` nor a colon, so this is
    /// the one classification that does not fall out of the old shape checks.
    #[test]
    fn reserved_namespace_counts_as_an_explicit_route() {
        assert!(is_explicitly_routed("bitrouter/auto"));
        assert!(is_explicitly_routed("bitrouter/auto:cost"));
        assert!(is_explicitly_routed("@auto"));
        assert!(is_explicitly_routed("openai:gpt-5.5"));
        // A vendor-qualified catalog model is still an ordinary bare model the
        // table may route.
        assert!(!is_explicitly_routed("anthropic/claude-opus-5"));
    }

    /// An assistant message whose only content is a call to `tool`.
    fn assistant_calls(tool: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: format!("call_{tool}"),
                name: tool.to_string(),
                arguments: "{}".to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    /// An assistant message that is a plain text reply (no tool call) — a
    /// completed model turn.
    fn assistant_text(text: &str) -> Message {
        Message::text(Role::Assistant, text)
    }

    /// An assistant message that calls several tools in one turn, in order.
    fn assistant_calls_multi(tools: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: tools
                .iter()
                .map(|tool| Content::ToolCall {
                    id: format!("call_{tool}"),
                    name: tool.to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                })
                .collect(),
        }
    }

    /// A minimal function tool, so a request "carries tools".
    fn a_tool() -> Tool {
        Tool::Function {
            name: "read_file".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    /// The provider-defined declaration the `bitrouter/fusion` alias injects.
    fn fusion_declaration() -> Tool {
        Tool::ProviderDefined {
            id: "bitrouter.fusion".to_string(),
            name: "fusion".to_string(),
            args: serde_json::json!({}),
            provider_metadata: ProviderMetadata::new(),
        }
    }

    /// Drive the router over a constructed prompt and return the routed model.
    fn route(model: &str, messages: Vec<Message>, tools: Vec<Tool>) -> String {
        let mut p = prompt(model);
        p.messages = messages;
        p.tools = tools;
        router().apply(&mut p);
        p.model
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bitrouter-policy-table-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn opening_request_routes_to_its_tier() {
        // The explicit fix instruction predicts implementation → cheap.
        assert_eq!(
            route("inbound", vec![user("fix the bug")], vec![]),
            "vendor/cheap"
        );
    }

    #[test]
    fn guarded_decision_log_uses_only_opaque_trajectory_request_identity() -> anyhow::Result<()> {
        let raw_request_id = "SECRET-task-labeled-request-header-SENTINEL";
        let opaque_request_id =
            CorrelationKey::from_bytes([35; 32])?.request_identity("owner-a", raw_request_id)?;
        let router = router();
        let mut request_prompt = prompt("inbound");
        request_prompt.messages = vec![user("generic request")];
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_str(raw_request_id)?,
        );
        let decision = router.candidate_for_guarded_policy(&request_prompt, &headers);
        let invocation = EvalInvocation::new("owner-a");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || CapturedLogWriter(sink.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            router.record_bound_policy_decision(
                &opaque_request_id,
                &invocation,
                "inbound".into(),
                None,
                decision,
                &headers,
            );
        });
        let rendered = String::from_utf8(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        assert!(rendered.contains(&opaque_request_id));
        assert!(!rendered.contains(raw_request_id));
        assert!(!rendered.contains("SECRET"));
        assert!(!rendered.contains("task-labeled"));
        Ok(())
    }

    #[test]
    fn after_tool_step_routes_to_its_tier() {
        // An incomplete tool call has unknown predictive role and therefore
        // uses the policy default rather than observed-state compatibility.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("read_file")],
                vec![],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn equivalent_tool_followups_share_the_trace_projection_route() {
        // Raw tool names do not participate in predictive task routing.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("grep")],
                vec![],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn tool_use_guardrail_clamps_a_non_tool_safe_tier() {
        // `after_read_file` would route to cheap, but the request carries tools
        // and cheap is not tool-safe → clamped up to the tool_use_tier
        // (flagship). The guardrail is the key safety property.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("read_file")],
                vec![a_tool()],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn exploration_assignment_precedes_tool_clamping() -> anyhow::Result<()> {
        let mut config = comparator_config();
        config.fingerprints.insert(
            "agent_route/v1|unknown|unknown|normal".into(),
            "strong".into(),
        );
        let exploration = RouteExploration {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            target_request_key: "agent_route/v1|unknown|unknown|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 1_000_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: None,
            },
        };
        let table = PolicyTable::from_config(&config)
            .ok_or_else(|| anyhow::anyhow!("comparison policy must contain tiers"))?;
        let router = PolicyTableRouter::new(table).with_exploration(Some(exploration));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-benchmark-run-id",
            HeaderValue::from_static("run-1"),
        );
        headers.insert("x-bitrouter-trial-id", HeaderValue::from_static("trial-1"));

        let non_target = router.decision_for(&prompt("inbound"), &HeaderMap::new());
        assert_eq!(non_target.selected_tier.as_deref(), Some("strong"));

        let mut with_tool = prompt("inbound");
        with_tool.tools = vec![a_tool()];
        let clamped = router.decision_for(&with_tool, &headers);
        assert_eq!(clamped.selected_tier.as_deref(), Some("strong"));
        assert_eq!(
            clamped.experiment.as_ref().map(|experiment| experiment.arm),
            Some(ExperimentArm::Challenger)
        );
        Ok(())
    }

    #[test]
    fn exploration_targets_exact_projection_when_static_route_uses_unknown_fallback()
    -> anyhow::Result<()> {
        let config = comparator_config();
        let exploration = RouteExploration {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            target_request_key: "agent_route/v1|code:debugging|implement|normal".into(),
            champion_tier: "economy".into(),
            challenger_tier: "strong".into(),
            challenger_exposure_ppm: 1_000_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: None,
            },
        };
        let table = PolicyTable::from_config(&config)
            .ok_or_else(|| anyhow::anyhow!("comparison policy must contain tiers"))?;
        let router = PolicyTableRouter::new(table).with_exploration(Some(exploration));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-benchmark-run-id",
            HeaderValue::from_static("run-1"),
        );
        headers.insert("x-bitrouter-trial-id", HeaderValue::from_static("trial-1"));
        let mut routed = prompt("inbound");
        routed.messages = completed_read_step();

        let decision = router.decision_for(&routed, &headers);

        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:debugging|implement|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("strong"));
        assert_eq!(
            decision
                .experiment
                .as_ref()
                .map(|experiment| experiment.arm),
            Some(ExperimentArm::Challenger)
        );
        Ok(())
    }

    #[test]
    fn exploration_without_stable_identity_uses_signed_champion_control() -> anyhow::Result<()> {
        let mut config = comparator_config();
        config.fingerprints.insert(
            "agent_route/v1|unknown|unknown|normal".into(),
            "economy".into(),
        );
        let exploration = RouteExploration {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            target_request_key: "agent_route/v1|unknown|unknown|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 1_000_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: None,
            },
        };
        let table = PolicyTable::from_config(&config)
            .ok_or_else(|| anyhow::anyhow!("comparison policy must contain tiers"))?;
        let router = PolicyTableRouter::new(table).with_exploration(Some(exploration));

        let decision = router.decision_for(&prompt("inbound"), &HeaderMap::new());
        assert_eq!(decision.selected_tier.as_deref(), Some("strong"));
        assert_eq!(decision.experiment, None);
        Ok(())
    }

    #[test]
    fn progress_guard_clamp_preserves_the_assigned_challenger_arm() -> anyhow::Result<()> {
        let mut config = comparator_config();
        config.fingerprints.insert(
            "agent_route/v1|unknown|unknown|normal".into(),
            "strong".into(),
        );
        let table = PolicyTable::from_config(&config)
            .ok_or_else(|| anyhow::anyhow!("comparison policy must contain tiers"))?;
        let router = PolicyTableRouter::new(table).with_exploration(Some(RouteExploration {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            target_request_key: "agent_route/v1|unknown|unknown|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 1_000_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: None,
            },
        }));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-benchmark-run-id",
            HeaderValue::from_static("run-1"),
        );
        headers.insert("x-bitrouter-trial-id", HeaderValue::from_static("trial-1"));
        let mut decision = router.candidate_for_guarded_policy(&prompt("inbound"), &headers);

        router.apply_guarded_route(&mut decision, Some("strong"), true, false);

        assert_eq!(decision.selected_tier.as_deref(), Some("strong"));
        assert_eq!(
            decision
                .experiment
                .as_ref()
                .map(|experiment| experiment.arm),
            Some(ExperimentArm::Challenger)
        );
        Ok(())
    }

    #[test]
    fn explicit_provider_route_is_left_untouched() {
        // A `provider:model` pin (and the `claude-code:` subscription route) is
        // the caller's deliberate choice and is never re-tiered.
        assert_eq!(
            route("vendor:exact-model", vec![user("hi")], vec![]),
            "vendor:exact-model"
        );
        assert_eq!(
            route("claude-code:claude-opus-4-8", vec![user("hi")], vec![]),
            "claude-code:claude-opus-4-8"
        );
    }

    #[test]
    fn preset_routes_are_left_for_stage_zero_resolution() {
        assert_eq!(route("@coding", vec![user("hi")], vec![]), "@coding");
        assert_eq!(
            route("@coding:free", vec![user("hi")], vec![]),
            "@coding:free"
        );
    }

    #[test]
    fn idempotent_on_second_application() {
        // Applying twice must not double-route: the second pass is already on
        // the tier's model and no-ops.
        let mut p = prompt("inbound");
        p.messages = vec![user("fix the bug")];
        assert!(router().apply(&mut p), "first pass routes");
        assert_eq!(p.model, "vendor/cheap");
        assert!(!router().apply(&mut p), "second pass is a no-op");
        assert_eq!(p.model, "vendor/cheap");
    }

    #[test]
    fn unmapped_fingerprint_without_default_is_a_noop() {
        // No default_tier and an unmapped fingerprint → the caller's model is
        // left as-is.
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("cheap".to_string(), PolicyModelTarget::from("vendor/cheap"))]),
            fingerprints: HashMap::from([(
                "agent_trace/v1|opening|normal".to_string(),
                "cheap".to_string(),
            )]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi"), assistant_calls("grep")];
        assert!(!r.apply(&mut p));
        assert_eq!(p.model, "inbound");
    }

    #[test]
    fn route_prompt_writes_policy_decision_jsonl_when_recorder_is_configured() {
        let path = temp_path("decisions.jsonl");
        let table = PolicyTable::from_config(&config()).expect("configured");
        let recorder = PolicyDecisionJsonlRecorder::new(path.clone()).unwrap();
        let r = PolicyTableRouter::new(table)
            .with_state_namespace("coding")
            .with_decision_recorder(recorder);

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("req-001"),
        );
        headers.insert(
            "x-bitrouter-harness",
            HeaderValue::from_static("terminus_2"),
        );
        headers.insert("x-session-id", HeaderValue::from_static("parent-001"));
        headers.insert(
            "x-bitrouter-benchmark-run-id",
            HeaderValue::from_static("short13-run"),
        );
        headers.insert("x-bitrouter-trial-id", HeaderValue::from_static("trial-01"));
        let mut p = prompt("inbound");
        p.system = Some(
            "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON commands with task_complete.".to_string(),
        );
        p.messages = completed_read_step();

        assert!(r.route_prompt(&mut p, &headers));
        assert_eq!(p.model, "vendor/cheap");

        let records =
            crate::workflow_state::decision::PolicyDecisionRecord::load_jsonl(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request_id.as_deref(), Some("req-001"));
        assert_eq!(records[0].input_model, "inbound");
        assert_eq!(
            records[0].ledger_key.as_deref(),
            Some("coding\0agent_route/v1|unknown|implement|normal")
        );
        assert_eq!(records[0].static_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(records[0].selected_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(records[0].predicted_role.as_deref(), Some("implement"));
        assert_eq!(records[0].predicted_action.as_deref(), Some("mutate"));
        assert_eq!(records[0].prediction_confidence_ppm, Some(900_000));
        assert_eq!(
            records[0].predictor_contract_digest.as_deref(),
            Some("sha256:7039bc16f3ac2e306d7855a193aee8bb4cd4395a92a58a09768d60d628f70f37")
        );
        assert_eq!(
            records[0].prediction_confidence_kind.as_deref(),
            Some("heuristic_margin")
        );
        assert_eq!(
            records[0].prediction_reason_codes,
            vec!["mutation_requested", "read_result_available"]
        );
        assert_eq!(
            records[0].observed_route_projection.as_deref(),
            Some("agent_trace/v2|tool_followup|normal")
        );
        assert_eq!(records[0].reason, "static_table");
        assert_eq!(records[0].workflow_identity.role, AgentRole::Main);
        assert_eq!(
            records[0].workflow_identity.parent_session_id.as_deref(),
            Some("parent-001")
        );
        assert_eq!(
            records[0].workflow_identity.benchmark_run_id.as_deref(),
            Some("short13-run")
        );
        assert_eq!(
            records[0].workflow_identity.trial_id.as_deref(),
            Some("trial-01")
        );
        assert!(
            records[0]
                .workflow_identity
                .fingerprint
                .starts_with("sha256:")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prediction_confidence_ppm_clamps_a_literal_edge_case_table() {
        let cases = [
            (f32::NAN, 0),
            (f32::INFINITY, 1_000_000),
            (f32::NEG_INFINITY, 0),
            (-0.25, 0),
            (0.0, 0),
            (0.9, 900_000),
            (1.1, 1_000_000),
        ];

        for (confidence, expected) in cases {
            assert_eq!(prediction_confidence_ppm(confidence), expected);
        }
    }

    #[test]
    fn prediction_reason_codes_allow_only_predictor_categories_in_sorted_capped_order() {
        let evidence = [
            "test_succeeded",
            "action_failed_once",
            "customer_secret",
            "read_result_available",
            "opening_broad_goal",
            "mutation_requested",
            "score_margin_low",
            "verification_requested",
            "narrow_poll_requested",
            "concrete_mutation_requested",
            "mutation_requested",
            "",
        ]
        .into_iter()
        .map(|code| PredictiveEvidence {
            code: code.to_string(),
            weight: 1,
            confidence: 0.9,
        })
        .collect::<Vec<_>>();

        assert_eq!(
            prediction_reason_codes(&evidence),
            vec![
                "action_failed_once",
                "concrete_mutation_requested",
                "mutation_requested",
                "narrow_poll_requested",
                "opening_broad_goal",
                "read_result_available",
                "score_margin_low",
                "test_succeeded",
            ]
        );
    }

    #[test]
    fn task_family_reason_codes_allow_only_task_categories_in_sorted_capped_order() {
        let evidence = [
            "task_code_review",
            "task_agent_general",
            "customer_secret",
            "task_code_debugging",
            "task_agent_web_research",
            "task_code_generation",
            "task_agent_memory_operations",
            "task_code_sql_database",
            "task_code_frontend_ui",
            "task_code_devops_config",
            "task_code_repository_analysis",
            "task_agent_multi_step_planning",
            "task_agent_workflow_execution",
            "task_unknown",
            "task_code_review",
        ]
        .into_iter()
        .map(|code| PredictiveEvidence {
            code: code.to_string(),
            weight: 1,
            confidence: 0.9,
        })
        .collect::<Vec<_>>();

        assert_eq!(
            task_family_reason_codes(&evidence),
            vec![
                "task_agent_general",
                "task_agent_memory_operations",
                "task_agent_multi_step_planning",
                "task_agent_web_research",
                "task_agent_workflow_execution",
                "task_code_debugging",
                "task_code_devops_config",
                "task_code_frontend_ui",
            ]
        );
    }

    #[test]
    fn explicit_economy_route_uses_strong_baseline_for_pending_eval_decision() {
        let mut config = comparator_config();
        config.fingerprints.insert(
            "agent_route/v1|code:debugging|implement|normal".to_string(),
            "economy".to_string(),
        );
        let table = PolicyTable::from_config(&config).expect("configured");
        let pending = crate::eval::settlement::PendingEvalDecisionStore::default();
        let router = PolicyTableRouter::new(table).with_eval_observer(
            pending.clone(),
            "auto:cost",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            HashMap::new(),
            Some("strong".to_string()),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("request-1"),
        );
        let mut routed = prompt("inbound");
        routed.messages = completed_read_step();
        let invocation = EvalInvocation::new("local");
        let decision = router.decision_for_bound_policy(&routed, &headers);

        let selected = router.record_bound_policy_decision(
            "request-1",
            &invocation,
            routed.model.clone(),
            routed.params.reasoning_effort,
            decision,
            &headers,
        );
        assert_eq!(
            selected.as_ref().map(PolicyModelTarget::model),
            Some("vendor/economy")
        );

        let decision = pending
            .get(&invocation, "local")
            .expect("pending eval decision");
        assert_eq!(decision.policy, "auto:cost");
        assert_eq!(decision.selected_tier, "economy");
        assert_eq!(decision.baseline_tier.as_deref(), Some("strong"));
        assert_eq!(
            decision.request_key,
            "agent_route/v1|code:debugging|implement|normal"
        );
    }

    #[test]
    fn certificate_baseline_overrides_policy_default_in_eval_outputs() {
        let mut table_config = comparator_config();
        table_config.fingerprints.insert(
            "agent_route/v1|code:debugging|implement|normal".to_string(),
            "economy".to_string(),
        );
        table_config.tiers.insert(
            "reference".to_string(),
            PolicyModelTarget::from("vendor/reference"),
        );
        let table = PolicyTable::from_config(&table_config).expect("configured");
        let pending = crate::eval::settlement::PendingEvalDecisionStore::default();
        let path = temp_path("certificate-baseline-decisions.jsonl");
        let recorder = PolicyDecisionJsonlRecorder::new(path.clone()).expect("recorder");
        let router = PolicyTableRouter::new(table)
            .with_decision_recorder(recorder)
            .with_eval_observer(
                pending.clone(),
                "auto:cost",
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                HashMap::from([(
                    "agent_route/v1|code:debugging|implement|normal".to_string(),
                    "reference".to_string(),
                )]),
                Some("strong".to_string()),
            );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("request-2"),
        );
        let mut routed = prompt("inbound");
        routed.messages = completed_read_step();
        let invocation = EvalInvocation::new("local");
        let decision = router.decision_for_bound_policy(&routed, &headers);

        let selected = router.record_bound_policy_decision(
            "trajectory-request-opaque",
            &invocation,
            routed.model.clone(),
            routed.params.reasoning_effort,
            decision,
            &headers,
        );
        assert_eq!(
            selected.as_ref().map(PolicyModelTarget::model),
            Some("vendor/economy")
        );

        let pending_decision = pending
            .get(&invocation, "local")
            .expect("pending eval decision");
        assert_eq!(pending_decision.selected_tier, "economy");
        assert_eq!(pending_decision.baseline_tier.as_deref(), Some("reference"));
        let records = PolicyDecisionRecord::load_jsonl(&path).expect("decision record");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].request_id.as_deref(),
            Some("trajectory-request-opaque")
        );
        assert_eq!(
            records[0].ingress_request_id_sha256.as_deref(),
            Some(crate::workflow_state::decision::ingress_request_id_sha256("request-2").as_str())
        );
        assert!(
            !serde_json::to_string(&records[0])
                .expect("decision record serializes")
                .contains("request-2")
        );
        assert_eq!(records[0].static_tier.as_deref(), Some("economy"));
        assert_eq!(records[0].selected_tier.as_deref(), Some("economy"));
        assert_eq!(records[0].baseline_tier.as_deref(), Some("reference"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn from_config_is_none_when_no_tiers() {
        assert!(PolicyTableRouter::from_config(&PolicyTableConfig::default()).is_none());
    }

    #[test]
    fn latest_user_turn_routes_by_predictive_role_not_stale_tool_state() {
        // The decisive new user instruction routes to implementation instead
        // of consulting the stale observed read-tool projection.
        let routed = route(
            "inbound",
            vec![
                user("fix the bug"),
                assistant_calls("read_file"),
                assistant_text("here is what I found"),
                user("Implement a new parser module now."),
            ],
            vec![],
        );
        assert_eq!(routed, "vendor/cheap");
    }

    #[test]
    fn parallel_tool_calls_use_the_last_call_in_the_turn() {
        // An incomplete multi-tool turn has no confident predictive role and
        // therefore uses the policy default regardless of raw tool names.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix"), assistant_calls_multi(&["grep", "read_file"])],
                vec![],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn colon_form_tier_target_is_idempotent() {
        // A tier that resolves to a `provider:model` (colon) id: the first pass
        // routes to it, and the second pass skips it as an explicit route.
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([(
                "flagship".to_string(),
                PolicyModelTarget::from("vendor:exact"),
            )]),
            fingerprints: HashMap::from([(
                "agent_route/v1|unknown|unknown|normal".to_string(),
                "flagship".to_string(),
            )]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi")];
        assert!(r.apply(&mut p), "first pass routes");
        assert_eq!(p.model, "vendor:exact");
        assert!(!r.apply(&mut p), "second pass skips the explicit route");
        assert_eq!(p.model, "vendor:exact");
    }

    #[test]
    fn same_model_compound_target_overrides_caller_effort() -> anyhow::Result<()> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([(
                "strong".to_string(),
                PolicyModelTarget::ModelEffort {
                    model: "vendor/same".to_string(),
                    effort: ReasoningEffort::High,
                },
            )]),
            default_tier: Some("strong".to_string()),
            ..Default::default()
        };
        let router = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let mut prompt = prompt("vendor/same");
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);

        assert!(router.apply(&mut prompt), "effort-only route is a mutation");
        assert_eq!(prompt.model, "vendor/same");
        assert_eq!(prompt.params.reasoning_effort, Some(ReasoningEffort::High));
        Ok(())
    }

    #[test]
    fn scalar_target_records_caller_effort_as_the_effective_static_treatment() -> anyhow::Result<()>
    {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("strong".to_string(), PolicyModelTarget::from("vendor/same"))]),
            default_tier: Some("strong".to_string()),
            ..Default::default()
        };
        let router = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let mut prompt = prompt("@auto");
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);

        let decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new());

        assert_eq!(decision.input_effort, Some(ReasoningEffort::Low));
        assert_eq!(decision.static_effort, Some(ReasoningEffort::Low));
        assert_eq!(decision.selected_effort, Some(ReasoningEffort::Low));
        Ok(())
    }

    #[test]
    fn hidden_continuation_pins_same_model_compound_target() -> anyhow::Result<()> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([
                (
                    "low".to_string(),
                    PolicyModelTarget::ModelEffort {
                        model: "vendor/same".to_string(),
                        effort: ReasoningEffort::Low,
                    },
                ),
                (
                    "high".to_string(),
                    PolicyModelTarget::ModelEffort {
                        model: "vendor/same".to_string(),
                        effort: ReasoningEffort::High,
                    },
                ),
            ]),
            default_tier: Some("low".to_string()),
            ..Default::default()
        };
        let router = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let prompt = prompt("@auto");
        let mut decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new());

        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/same".to_owned(),
                effective_effort: Some(ReasoningEffort::High),
                effort_authoritative: true,
            },
        )?;

        assert_eq!(decision.continuation_proposed_tier.as_deref(), Some("low"));
        assert_eq!(
            decision.continuation_proposed_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("high"));
        assert_eq!(decision.selected_effort, Some(ReasoningEffort::High));
        Ok(())
    }

    #[test]
    fn hidden_continuation_effort_is_applied_through_a_scalar_compatibility_tier()
    -> anyhow::Result<()> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("compat".to_string(), PolicyModelTarget::from("vendor/same"))]),
            default_tier: Some("compat".to_string()),
            ..Default::default()
        };
        let router = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let mut prompt = prompt("@auto");
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);
        let mut decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new());
        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/same".to_owned(),
                effective_effort: Some(ReasoningEffort::High),
                effort_authoritative: true,
            },
        )?;

        let target = router
            .record_decision(
                "@auto".to_owned(),
                Some(ReasoningEffort::Low),
                decision,
                &HeaderMap::new(),
                None,
                None,
            )
            .ok_or_else(|| anyhow::anyhow!("continuation target missing"))?;
        assert_eq!(target.model(), "vendor/same");
        assert_eq!(target.effort(), Some(ReasoningEffort::High));
        Ok(())
    }

    #[test]
    fn legacy_continuation_does_not_reinterpret_missing_effort_as_provider_default()
    -> anyhow::Result<()> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("compat".to_string(), PolicyModelTarget::from("vendor/same"))]),
            default_tier: Some("compat".to_string()),
            ..Default::default()
        };
        let router = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let mut prompt = prompt("@auto");
        prompt.params.reasoning_effort = Some(ReasoningEffort::High);
        let mut decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new());

        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/same".to_owned(),
                effective_effort: None,
                effort_authoritative: false,
            },
        )?;

        assert_eq!(decision.selected_effort, Some(ReasoningEffort::High));
        Ok(())
    }

    #[test]
    fn disabled_guardrail_lets_a_tool_request_route_cheap() {
        // With no `tool_use_tier`, the guardrail is off: a tool-carrying request
        // routes by fingerprint like any other (here `after_read_file` → cheap).
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([
                ("cheap".to_string(), PolicyModelTarget::from("vendor/cheap")),
                (
                    "flagship".to_string(),
                    PolicyModelTarget::from("vendor/flagship"),
                ),
            ]),
            fingerprints: HashMap::from([(
                "agent_route/v1|unknown|implement|normal".to_string(),
                "cheap".to_string(),
            )]),
            default_tier: Some("flagship".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = completed_read_step();
        p.tools = vec![a_tool()];
        assert!(r.apply(&mut p));
        assert_eq!(p.model, "vendor/cheap");
    }

    #[test]
    fn fusion_declaration_is_left_untouched() {
        // A request carrying the fusion alias's injected declaration is owned by
        // the fusion flow; the policy table must not re-tier its outer model,
        // even though the model is colonless and the request carries tools.
        assert_eq!(
            route(
                "vendor/fusion-outer",
                vec![user("compare these")],
                vec![fusion_declaration()]
            ),
            "vendor/fusion-outer"
        );
    }

    // ---- canonical agent-trace routing ----

    /// A read step prompt — fingerprints to `after_read_file` (→ cheap statically).
    fn read_step() -> Vec<Message> {
        vec![user("fix the bug"), assistant_calls("read_file")]
    }

    fn completed_read_step() -> Vec<Message> {
        vec![
            user("fix the bug"),
            assistant_calls("read_file"),
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: "call_read_file".to_string(),
                    tool_name: None,
                    output: ToolResultOutput::Text {
                        value: "source contents".to_string(),
                    },
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                }],
            },
        ]
    }

    fn completed_task_review_mutation_step() -> Vec<Message> {
        vec![
            user(
                "Review this pull request for security bugs, audit the diff, and verify the test suite.",
            ),
            assistant_calls("write_file"),
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: "call_write_file".to_string(),
                    tool_name: None,
                    output: ToolResultOutput::Text {
                        value: "updated source contents".to_string(),
                    },
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                }],
            },
        ]
    }

    fn route_with_headers(
        router: &PolicyTableRouter,
        messages: Vec<Message>,
        headers: &HeaderMap,
    ) -> String {
        let mut p = prompt("inbound");
        p.messages = messages;
        router.apply_with_headers(&mut p, headers);
        p.model
    }

    #[test]
    fn workflow_state_key_strategy_uses_ir_key_for_lookup() {
        let mut cfg = config();
        cfg.key_strategy = PolicyKeyStrategy::AgentTrace;
        cfg.fingerprints.clear();
        cfg.default_tier = Some("flagship".to_string());

        let mut probe = prompt("inbound");
        probe.messages = vec![user("fix"), assistant_calls("Bash")];
        let headers = claude_headers();
        let key = OnlineWorkflowState::from_prompt(
            &headers,
            &probe,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        )
        .routing_key()
        .to_string();
        cfg.fingerprints.insert(key, "cheap".to_string());

        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        assert_eq!(
            route_with_headers(
                &router,
                vec![user("fix"), assistant_calls("Bash")],
                &headers
            ),
            "vendor/cheap"
        );
    }

    #[test]
    fn unknown_family_baseline_routes_unlisted_task_cells() {
        let router = router();
        let mut prompt = prompt("inbound");
        prompt.messages = completed_read_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
    }

    #[test]
    fn unknown_family_baseline_wins_before_observed_telemetry() {
        let mut cfg = config();
        cfg.fingerprints.insert(
            "agent_trace/v2|tool_followup|normal".to_string(),
            "flagship".to_string(),
        );
        cfg.fingerprints.insert(
            "agent_route/v1|unknown|implement|normal".to_string(),
            "cheap".to_string(),
        );
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = completed_read_step();

        let online = OnlineWorkflowState::for_named_policy(&HeaderMap::new(), &prompt);
        assert_eq!(
            online.baseline_routing_key(),
            "agent_route/v1|unknown|implement|normal"
        );

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:debugging|implement|normal"
        );
        assert_eq!(
            decision.observed_route_projection,
            "agent_trace/v2|tool_followup|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
    }

    #[test]
    fn observed_route_does_not_participate_in_named_auto_policy_lookup() {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                (
                    "balanced".to_string(),
                    PolicyModelTarget::from("vendor/balanced"),
                ),
                (
                    "strong".to_string(),
                    PolicyModelTarget::from("vendor/strong"),
                ),
            ]),
            fingerprints: HashMap::from([(
                "agent_trace/v2|tool_followup|normal".to_string(),
                "strong".to_string(),
            )]),
            default_tier: Some("balanced".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = completed_task_review_mutation_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:review|verify|normal"
        );
        assert_eq!(decision.request_key, decision.route_projection);
        assert_eq!(decision.selected_tier.as_deref(), Some("balanced"));
    }

    #[test]
    fn exact_task_aware_v1_override_wins_before_unknown_baseline() {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                (
                    "economy".to_string(),
                    PolicyModelTarget::from("vendor/economy"),
                ),
                (
                    "strong".to_string(),
                    PolicyModelTarget::from("vendor/strong"),
                ),
            ]),
            fingerprints: HashMap::from([
                (
                    "agent_route/v1|code:review|verify|normal".to_string(),
                    "strong".to_string(),
                ),
                (
                    "agent_route/v1|unknown|verify|normal".to_string(),
                    "economy".to_string(),
                ),
                (
                    "agent_trace/v2|tool_followup|normal".to_string(),
                    "economy".to_string(),
                ),
            ]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = completed_task_review_mutation_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:review|verify|normal"
        );
        assert_eq!(
            decision.request_key,
            "agent_route/v1|code:review|verify|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("strong"));
    }

    #[test]
    fn unknown_family_baseline_wins_before_observed_state() {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                (
                    "economy".to_string(),
                    PolicyModelTarget::from("vendor/economy"),
                ),
                (
                    "strong".to_string(),
                    PolicyModelTarget::from("vendor/strong"),
                ),
            ]),
            fingerprints: HashMap::from([
                (
                    "agent_route/v1|unknown|verify|normal".to_string(),
                    "economy".to_string(),
                ),
                (
                    "agent_trace/v2|tool_followup|normal".to_string(),
                    "strong".to_string(),
                ),
            ]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = completed_task_review_mutation_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:review|verify|normal"
        );
        assert_eq!(decision.request_key, "agent_route/v1|unknown|verify|normal");
        assert_eq!(decision.selected_tier.as_deref(), Some("economy"));
    }

    #[tokio::test]
    async fn unified_v1_settlement_attributes_exact_route_and_unknown_baseline()
    -> anyhow::Result<()> {
        let template = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router/policy-lock.yaml");
        let mut active: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(template)?)?;
        active
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("template auto policy missing"))?
            .adequacy
            .min_semantic_successes_for_lock = 1;
        let policy_digest = semantic_digest(&active)?;
        let policy = active
            .policies
            .get("auto")
            .ok_or_else(|| anyhow::anyhow!("template auto policy missing"))?;
        let table = PolicyTable::from_config(
            &policy.as_table_config(bitrouter_sdk::config::PolicyRuntimeMode::Frozen),
        )
        .ok_or_else(|| anyhow::anyhow!("template policy table missing"))?;
        let route_baselines = active.certificates["auto"]
            .iter()
            .filter_map(|(request_key, certificate)| {
                certificate
                    .baseline_tier
                    .as_ref()
                    .map(|baseline| (request_key.clone(), baseline.clone()))
            })
            .collect();
        let pending = PendingEvalDecisionStore::default();
        let router = PolicyTableRouter::new(table).with_eval_observer(
            pending.clone(),
            "auto",
            policy_digest.clone(),
            route_baselines,
            policy.default_tier.clone(),
        );
        let mut prompt = prompt("@auto");
        prompt.messages = vec![user(
            "Implement a new module and refactor the parser API in src/parser.rs.",
        )];
        let mut decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new());
        assert_eq!(
            decision.route_projection,
            "agent_route/v1|code:generation|implement|normal"
        );
        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("balanced"));
        let strong_model = policy
            .tiers
            .get("strong")
            .ok_or_else(|| anyhow::anyhow!("template strong tier missing"))?
            .model()
            .to_owned();
        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: strong_model,
                effective_effort: None,
                effort_authoritative: true,
            },
        )?;
        let invocation = EvalInvocation::new("local");
        router.record_bound_policy_decision(
            "request-unified-v1",
            &invocation,
            prompt.model,
            prompt.params.reasoning_effort,
            decision,
            &HeaderMap::new(),
        );

        let correlated = pending
            .peek(&invocation, "local")
            .ok_or_else(|| anyhow::anyhow!("pending unified-v1 decision missing"))?;
        assert_eq!(
            correlated.route_projection,
            "agent_route/v1|code:generation|implement|normal"
        );
        assert_eq!(
            correlated.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(correlated.baseline_tier.as_deref(), Some("balanced"));
        assert_eq!(
            correlated.task_family_reason_codes,
            vec!["task_code_generation"]
        );

        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let recorder =
            EvalSettlementRecorder::new(store.clone(), pending, Arc::new(PricingTable::new()));
        let mut settlement = SettlementContext {
            request_id: "request-unified-v1".into(),
            caller: CallerContext::local(),
            target: None,
            model_id: "gpt-5.6-sol".into(),
            reasoning_effort: None,
            provider_id: "openai-codex".into(),
            account_label: None,
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_origin: UsageOrigin::ProviderReported,
            raw_usage: None,
            web_search_count: 0,
            media_input_count: 0,
            media_output_count: 0,
            server_tool_calls: Vec::new(),
            streamed: false,
            request_duration_ms: 100,
            upstream_duration_ms: Some(90),
            ttft_ms: None,
            generation_duration_ms: None,
            first_token_kind: None,
            finish_reason: None,
            error: None,
            events: EventBus::default(),
        };
        settlement.emit(invocation);
        recorder.record(&mut settlement).await?;
        let subject = store
            .subject("request:request-unified-v1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("settled unified-v1 subject missing"))?;
        let settled = subject
            .decisions
            .first()
            .ok_or_else(|| anyhow::anyhow!("settled unified-v1 decision missing"))?;
        assert_eq!(
            settled.route_projection,
            "agent_route/v1|code:generation|implement|normal"
        );
        assert_eq!(
            settled.request_key,
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(settled.baseline_tier.as_deref(), Some("balanced"));
        assert_eq!(
            subject
                .evidence
                .first()
                .ok_or_else(|| anyhow::anyhow!("settlement evidence missing"))?
                .attributes
                .get("task_family_reason_codes")
                .map(String::as_str),
            Some("task_code_generation")
        );

        let result = EvaluationResult {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: subject.eval_id.clone(),
            evidence_digest: subject.evidence_digest.clone(),
            evaluator: EvaluatorIdentity {
                authority_id: "task-native".into(),
                evaluator_id: "unified-v1-regression".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest: policy_digest,
            },
            verdict: EvalVerdict::Pass,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "unified-v1-regression".into(),
            submitted_at: "2026-08-15T00:00:01Z".into(),
        };
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            frozen_at: "2026-08-15T00:00:02Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id: "unified-v1-result".into(),
                content_digest:
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                subject,
                result,
            }],
        };
        let routes = eval.route_evidence()?;
        let attributed = routes
            .get(&(
                "auto".to_owned(),
                "agent_route/v1|code:generation|implement|normal".to_owned(),
            ))
            .ok_or_else(|| anyhow::anyhow!("unified-v1 compiler attribution missing"))?;
        assert_eq!(attributed.baseline_tier.as_deref(), Some("balanced"));
        assert_eq!(
            attributed.matched_request_keys,
            BTreeSet::from(["agent_route/v1|unknown|implement|normal".to_owned()])
        );
        let compiled = compile_candidate(CompileInput {
            current: &active,
            parent_digest: None,
            legacy: &LegacyAdequacySnapshot {
                snapshot_time_unix_ms: 0,
                pins: Vec::new(),
                exploration: Vec::new(),
                semantic_successes: Vec::new(),
                reliability_events: Vec::new(),
            },
            eval: Some(&eval),
            proposed_progress_guards: None,
        })?
        .document;
        let certificate = compiled
            .certificate("auto", "agent_route/v1|code:generation|implement|normal")
            .ok_or_else(|| anyhow::anyhow!("compiled unified-v1 certificate missing"))?;
        assert_eq!(certificate.baseline_tier.as_deref(), Some("balanced"));
        Ok(())
    }

    #[test]
    fn task_aware_policy_unknown_family_uses_unified_v1_key() {
        let mut cfg = config();
        cfg.fingerprints.clear();
        cfg.default_tier = None;
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = vec![user("Run the shell command and report its output.")];

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.route_projection,
            "agent_route/v1|unknown|finalize|normal"
        );
    }

    #[test]
    fn unknown_family_uses_an_explicit_v1_route_before_default() -> anyhow::Result<()> {
        let mut cfg = config();
        cfg.fingerprints.clear();
        cfg.fingerprints.insert(
            "agent_route/v1|unknown|finalize|normal".into(),
            "economy".into(),
        );
        cfg.default_tier = Some("strong".into());
        let routed = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("configured router missing"))?;
        let mut prompt = prompt("inbound");
        prompt.messages = vec![user("Run the shell command and report its output.")];

        let explicit = routed.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            explicit.route_projection,
            "agent_route/v1|unknown|finalize|normal"
        );
        assert_eq!(
            explicit.request_key,
            "agent_route/v1|unknown|finalize|normal"
        );
        assert_eq!(explicit.selected_tier.as_deref(), Some("economy"));

        cfg.fingerprints.clear();
        let defaulted = PolicyTableRouter::from_config(&cfg)
            .ok_or_else(|| anyhow::anyhow!("default router missing"))?;
        let fallback = defaulted.decision_for(&prompt, &HeaderMap::new());
        assert_eq!(fallback.selected_tier.as_deref(), Some("strong"));
        Ok(())
    }

    #[test]
    fn task_aware_policy_records_bounded_task_observability() -> anyhow::Result<()> {
        let path = temp_path("task-aware-decisions.jsonl");
        let table = PolicyTable::from_config(&config())
            .ok_or_else(|| anyhow::anyhow!("configured table missing"))?;
        let recorder = PolicyDecisionJsonlRecorder::new(path.clone())?;
        let router = PolicyTableRouter::new(table).with_decision_recorder(recorder);
        let mut prompt = prompt("inbound");
        prompt.messages = completed_task_review_mutation_step();

        assert!(router.route_prompt(&mut prompt, &HeaderMap::new()));
        let records = PolicyDecisionRecord::load_jsonl(&path)?;
        let record = records
            .first()
            .ok_or_else(|| anyhow::anyhow!("task-aware decision record missing"))?;
        let value = serde_json::to_value(record)?;

        assert_eq!(value["predicted_task_family"], "code:review");
        assert_eq!(value["task_family_confidence_ppm"], 800_000);
        assert_eq!(
            value["task_family_reason_codes"],
            serde_json::json!(["task_code_review"])
        );

        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn broad_opening_routes_on_predictive_orchestrate_role() {
        let mut cfg = config();
        cfg.fingerprints.clear();
        cfg.fingerprints.insert(
            "agent_route/v1|agent:multi_step_planning|orchestrate|normal".to_string(),
            "cheap".to_string(),
        );
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = vec![user("Design the architecture and plan the implementation")];

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.request_key,
            "agent_route/v1|agent:multi_step_planning|orchestrate|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
    }

    #[test]
    fn observed_policy_route_is_ignored_in_favor_of_default() {
        let mut cfg = config();
        cfg.fingerprints.insert(
            "agent_trace/v2|tool_followup|normal".to_string(),
            "flagship".to_string(),
        );
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = read_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|unknown|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    }

    #[test]
    fn default_policy_route_records_the_predictive_learning_key() {
        let mut cfg = config();
        cfg.fingerprints.clear();
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let prompt = prompt("inbound");

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(
            decision.request_key,
            "agent_route/v1|unknown|unknown|normal"
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    }

    #[test]
    fn decision_reason_static_table() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = completed_read_step();

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
        assert_eq!(decision.static_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_model.as_deref(), Some("vendor/cheap"));
    }

    #[test]
    fn continuation_pin_records_the_predictive_proposal_and_serving_adjustment() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = completed_read_step();
        let mut decision = router.decision_for(&p, &HeaderMap::new());

        let applied = router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/flagship".to_owned(),
                effective_effort: None,
                effort_authoritative: true,
            },
        );

        assert!(applied.is_ok());
        assert_eq!(
            decision.continuation_proposed_tier.as_deref(),
            Some("cheap")
        );
        assert_eq!(
            decision.continuation_proposed_model.as_deref(),
            Some("vendor/cheap")
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
        assert_eq!(decision.selected_model.as_deref(), Some("vendor/flagship"));
        assert_eq!(decision.continuation_adjustment.as_deref(), Some("pin"));
        assert_eq!(decision.reason, PolicyDecisionReason::ContinuationPin);
        assert!(decision.pinned);
    }

    #[test]
    fn continuation_detach_records_adjustment_without_rewriting_prediction() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = read_step();
        let mut decision = router.decision_for(&p, &HeaderMap::new());

        let applied =
            router.apply_continuation_adjustment(&mut decision, &ContinuationAdjustment::Detach);

        assert!(applied.is_ok());
        assert_eq!(
            decision.continuation_proposed_model,
            decision.selected_model
        );
        assert_eq!(decision.continuation_adjustment.as_deref(), Some("detach"));
        assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
        assert!(!decision.pinned);
    }

    #[test]
    fn legacy_rejection_preserves_proposal_in_pending_eval_audit() {
        let table = PolicyTable::from_config(&config()).expect("configured");
        let pending = crate::eval::settlement::PendingEvalDecisionStore::default();
        let router = PolicyTableRouter::new(table).with_eval_observer(
            pending.clone(),
            "auto:cost",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            HashMap::new(),
            Some("flagship".to_owned()),
        );
        let mut p = prompt("@auto");
        p.messages = completed_read_step();
        let mut decision = router.decision_for_bound_policy(&p, &HeaderMap::new());
        router
            .apply_continuation_adjustment(&mut decision, &ContinuationAdjustment::RejectLegacy)
            .expect("legacy rejection is auditable");
        let invocation = EvalInvocation::new("local");

        router.record_bound_policy_decision(
            "request-legacy",
            &invocation,
            p.model,
            p.params.reasoning_effort,
            decision,
            &HeaderMap::new(),
        );

        let pending = pending
            .get(&invocation, "local")
            .expect("pending eval decision");
        assert_eq!(pending.continuation_proposed_tier.as_deref(), Some("cheap"));
        assert_eq!(
            pending.continuation_proposed_model.as_deref(),
            Some("vendor/cheap")
        );
        assert_eq!(
            pending.continuation_adjustment.as_deref(),
            Some("reject_legacy")
        );
        assert_eq!(pending.selected_tier, "cheap");
    }

    #[test]
    fn continuation_adjustment_and_proposal_are_written_to_jsonl() -> anyhow::Result<()> {
        let path = temp_path("continuation-adjustment-decisions.jsonl");
        let table = PolicyTable::from_config(&config())
            .ok_or_else(|| anyhow::anyhow!("policy table missing"))?;
        let recorder = PolicyDecisionJsonlRecorder::new(path.clone())?;
        let router = PolicyTableRouter::new(table).with_decision_recorder(recorder);
        let mut p = prompt("@auto");
        p.messages = completed_read_step();
        let mut decision = router.decision_for_bound_policy(&p, &HeaderMap::new());
        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/flagship".to_owned(),
                effective_effort: None,
                effort_authoritative: true,
            },
        )?;

        router.record_bound_policy_decision(
            "request-continuation",
            &EvalInvocation::new("owner"),
            "@auto".to_owned(),
            None,
            decision,
            &HeaderMap::new(),
        );

        let records = PolicyDecisionRecord::load_jsonl(&path)?;
        let record = records
            .first()
            .ok_or_else(|| anyhow::anyhow!("continuation decision missing"))?;
        assert_eq!(record.continuation_proposed_tier.as_deref(), Some("cheap"));
        assert_eq!(
            record.continuation_proposed_model.as_deref(),
            Some("vendor/cheap")
        );
        assert_eq!(record.continuation_adjustment.as_deref(), Some("pin"));
        assert_eq!(record.selected_model.as_deref(), Some("vendor/flagship"));
        assert_eq!(record.reason, "continuation_pin");
        assert!(record.pinned);
        Ok(())
    }

    #[test]
    fn continuation_pin_fails_closed_when_the_active_lock_removed_its_model() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = read_step();
        let mut decision = router.decision_for(&p, &HeaderMap::new());
        let original_selected = decision.selected_model.clone();

        let error = router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "retired-provider:retired-model".to_owned(),
                effective_effort: None,
                effort_authoritative: true,
            },
        );

        assert!(matches!(
            error,
            Err(bitrouter_sdk::BitrouterError::BadRequest { ref message })
                if message.contains("unavailable in the active policy")
        ));
        assert_eq!(decision.selected_model, original_selected);
    }

    #[test]
    fn decision_reason_tool_guardrail() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = completed_read_step();
        p.tools = vec![a_tool()];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::ToolGuardrail);
        assert_eq!(decision.static_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    }

    #[test]
    fn decision_reason_records_progress_guard_before_tool_floor() {
        let router = router();
        let mut decision =
            router.candidate_for_guarded_policy(&prompt("inbound"), &HeaderMap::new());

        router.apply_guarded_route(&mut decision, Some("flagship"), true, true);

        assert_eq!(
            decision.reason,
            PolicyDecisionReason::ProgressGuardToolGuardrail
        );
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
        assert_eq!(decision.selected_model.as_deref(), Some("vendor/flagship"));
    }

    #[test]
    fn decision_reason_no_match() {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("cheap".to_string(), PolicyModelTarget::from("vendor/cheap"))]),
            fingerprints: HashMap::from([(
                "agent_route/v1|code:review|verify|normal".to_string(),
                "cheap".to_string(),
            )]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi"), assistant_calls("grep")];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::NoMatch);
        assert_eq!(decision.selected_tier, None);
        assert_eq!(decision.selected_model, None);
    }

    #[test]
    fn policy_table_decision_never_exposes_learned_override_reasons() {
        let router = PolicyTableRouter::from_config(&config()).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = read_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
        assert!(!decision.pinned);
        assert!(!decision.request_qualified);
        assert_eq!(decision.semantic_successes, 0);
        assert_eq!(decision.semantic_success_threshold, 0);
        assert!(!decision.locked);
        assert!(!decision.trialed);
    }

    // ---- fingerprint parity through the real Chat Completions wire ----
    //
    // These exercise the full ingress path a harness drives — an OpenAI Chat
    // Completions request body parsed by the daemon's inbound adapter into the
    // canonical `Prompt`, then fingerprinted — and assert the agent-loop step
    // label, so the native router keys requests the same way an external
    // fingerprinting proxy would.

    fn fingerprint_of(body: serde_json::Value) -> String {
        use bitrouter_sdk::language_model::inbound_adapter_for;
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let adapter =
            inbound_adapter_for(&ApiProtocol::ChatCompletions).expect("chat completions adapter");
        let prompt = adapter.parse_request(body).expect("parse request");
        PolicyTable::fingerprint(&prompt)
    }

    #[test]
    fn opening_request_fingerprints_through_the_wire() {
        // System + first user turn, no assistant yet → the opening step.
        assert_eq!(
            fingerprint_of(serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "You are an agent."},
                    {"role": "user", "content": "fix the bug"}
                ]
            })),
            "opening"
        );
    }

    #[test]
    fn after_tool_steps_fingerprint_through_the_wire() {
        // The common in-loop step: the model called <tool>, its result returns
        // as an OpenAI `{role:"tool", tool_call_id, ...}` message (which carries
        // no tool name on the wire). The fingerprint recovers the step from the
        // assistant's tool call, matching `after_<tool>` for every loop tool.
        let after = |tool: &str| {
            serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "fix the bug"},
                    {"role": "assistant", "content": serde_json::Value::Null,
                     "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": tool, "arguments": "{}"}}
                     ]},
                    {"role": "tool", "tool_call_id": "c1", "content": "<result>"}
                ]
            })
        };
        for tool in ["terminal", "patch", "read_file"] {
            assert_eq!(
                fingerprint_of(after(tool)),
                format!("after_{tool}"),
                "the wire parse + fingerprint must label this the after_{tool} step"
            );
        }
    }

    #[test]
    fn trailing_user_turn_is_keyed_by_the_last_model_turn() {
        // A documented divergence from a simpler last-message scheme: a fresh
        // user instruction after a plain model reply is keyed by the model's most
        // recent turn (`midstream`), not by the trailing user message. Neither
        // `midstream` nor a user-followup label is in the demo's converged policy,
        // so this does not affect that workload's routing.
        assert_eq!(
            fingerprint_of(serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "done"},
                    {"role": "user", "content": "now do Y"}
                ]
            })),
            "midstream"
        );
    }
}
