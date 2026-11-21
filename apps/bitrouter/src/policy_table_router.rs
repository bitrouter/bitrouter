//! Frozen model-selection tables over source-independent semantic classifications.
//! One immutable table snapshot owns selection, continuation guards and Eval evidence.

use crate::continuation::ContinuationAdjustment;
use crate::gateway::observability::decision::{
    EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore, bounded_continuation_label,
};
use crate::gateway::routing::{
    assessment::{self, Assessment},
    workflow::decision::{PolicyDecisionJsonlRecorder, PolicyDecisionRecord},
};
use bitrouter_ai::{
    eval::{RouteActionCandidate, RouteDecisionMeasurement},
    policy::{NextStep, Risk, RouteClass, Target, TaskFamily},
    types::{Prompt, ReasoningEffort, Tool},
};
use http::HeaderMap;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecisionReason {
    StaticTable,
    ToolGuardrail,
    ContinuationPin,
    NoMatch,
}

impl PolicyDecisionReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::StaticTable => "static_table",
            Self::ToolGuardrail => "tool_guardrail",
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
    policy_snapshot: PolicySnapshot,
    pub request_key: String,
    pub route_projection: String,
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
    pub prediction_confidence_ppm: Option<u32>,
    pub task_family_confidence_ppm: Option<u32>,
    pub predictor_contract_digest: Option<String>,
    pub prediction_confidence_kind: Option<String>,
    pub reason: PolicyDecisionReason,
    pub route_measurement: Option<RouteDecisionMeasurement>,
}

#[derive(Clone)]
struct PolicySnapshot(Arc<PolicyTable>);

impl std::fmt::Debug for PolicySnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PolicySnapshot(<opaque>)")
    }
}

impl std::ops::Deref for PolicySnapshot {
    type Target = PolicyTable;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The resolved, immutable policy spec: route class → tier → model plus tool-use
/// guardrails. Learned-state databases never participate in this hot path.
pub struct PolicyTable {
    /// Tier name → model id the request is rewritten to.
    tiers: HashMap<String, Target>,
    /// Route class → tier name.
    routes: HashMap<String, String>,
    /// Tier for a class absent from `routes`.
    default_tier: Option<String>,
    /// Guardrail target tier for tool-carrying requests whose chosen tier is not
    /// tool-safe.
    tool_use_tier: Option<String>,
    /// Tiers that handle tool calls reliably.
    tool_safe_tiers: Vec<String>,
}

impl PolicyTable {
    /// Build a serving table from validated model-selection indexes.
    pub(crate) fn from_model_selection(
        selection: &crate::model_policy::CompiledSelection,
    ) -> Arc<Self> {
        Arc::new(Self {
            tiers: selection.tiers.clone().into_iter().collect(),
            routes: selection
                .routes
                .iter()
                .map(|(key, tier)| (key.to_string(), tier.clone()))
                .collect(),
            default_tier: Some(selection.default_tier.clone()),
            tool_use_tier: selection.tool_use_tier.clone(),
            tool_safe_tiers: selection.tool_safe_tiers.clone(),
        })
    }

    /// An empty table for validating selection after a policy is removed.
    #[cfg(test)]
    pub fn inert() -> Arc<Self> {
        Arc::new(Self {
            tiers: HashMap::new(),
            routes: HashMap::new(),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
        })
    }

    /// Resolve the exact semantic route class, its unknown-family
    /// baseline, then the default. The returned key is the key that actually
    /// selected the tier, except that defaults retain the primary key.
    fn tier_for_class<'table, 'key>(
        &'table self,
        predictive_primary: &'key str,
        unknown_baseline: &'key str,
    ) -> Option<(&'table str, &'key str)> {
        if let Some(tier) = self.routes.get(predictive_primary) {
            return Some((tier.as_str(), predictive_primary));
        }
        if let Some(tier) = self.routes.get(unknown_baseline) {
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
        self.target_of_tier(tier).map(Target::model)
    }

    /// The policy-owned effort override for a tier. An absent override
    /// preserves caller effort.
    pub(crate) fn effort_of_tier(&self, tier: &str) -> Option<ReasoningEffort> {
        self.target_of_tier(tier).and_then(Target::effort)
    }

    /// The complete model/effort target selected by a tier.
    pub(crate) fn target_of_tier(&self, tier: &str) -> Option<&Target> {
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

    fn route_measurement(
        &self,
        logging_action_tier: &str,
        input_effort: Option<ReasoningEffort>,
    ) -> anyhow::Result<RouteDecisionMeasurement> {
        let candidates = self
            .tiers
            .iter()
            .map(|(tier, target)| {
                let logging_probability_ppm = if tier == logging_action_tier {
                    1_000_000
                } else {
                    0
                };
                RouteActionCandidate {
                    tier: tier.clone(),
                    model: target.model().to_owned(),
                    effort: target.effort().or(input_effort),
                    logging_probability_ppm,
                }
            })
            .collect();
        let target = self.target_of_tier(logging_action_tier).ok_or_else(|| {
            anyhow::anyhow!("logging action tier is absent from the policy snapshot")
        })?;
        crate::storage::evaluation::route_decision_measurement(
            logging_action_tier,
            target.model(),
            target.effort().or(input_effort),
            candidates,
        )
    }
}

/// Frozen model-selection table with deterministic tool and continuation guards.
pub struct PolicyTableRouter {
    /// Swappable so `bitrouter reload` can install a fresh spec into the
    /// *live* transform. The router itself is baked into the built `App` and
    /// cannot be re-registered, so the reloadable unit has to be the table
    /// inside it rather than the router around it.
    ///
    /// A plain `RwLock` rather than a channel or an async lock: reads happen
    /// on the per-request hot path during policy selection, which is
    /// synchronous, and writes happen once per reload.
    table: std::sync::RwLock<Arc<PolicyTable>>,
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    eval_observer: Option<EvalDecisionObserver>,
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
    /// The spec in force right now. Clones the `Arc` rather than handing out a
    /// borrow so a concurrent table replacement cannot invalidate a
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
    #[cfg(test)]
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
            eval_observer: None,
        }
    }

    #[cfg(test)]
    pub fn with_decision_recorder(mut self, recorder: PolicyDecisionJsonlRecorder) -> Self {
        self.decision_recorder = Some(Arc::new(recorder));
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

    #[cfg(feature = "multi-instance")]
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

    /// Apply the policy table to a prompt, returning whether the model was
    /// rewritten. A no-op (returns `false`) when the model is already explicitly
    /// routed, when the request carries a bitrouter server-tool declaration,
    /// when the class resolves to no tier, or when the prompt is already on
    /// the resolved tier's model.
    #[cfg(test)]
    pub fn apply(&self, prompt: &mut Prompt, assessment: &Assessment) -> bool {
        self.route_prompt(prompt, &HeaderMap::new(), assessment)
    }

    fn decision_for_inner(
        &self,
        prompt: &Prompt,
        _headers: &HeaderMap,
        respect_explicit_route: bool,
        apply_tool_floor: bool,
        assessment: Option<&Assessment>,
    ) -> PolicyDecision {
        // Keep this exact immutable table alive through guards, recording, and
        // final target materialization, even if a concurrent reload swaps the
        // router's active table.
        let table = self.table();
        let class = assessment.map_or(
            RouteClass::new(TaskFamily::Unknown, NextStep::Unknown, Risk::Guarded),
            |value| value.route_class,
        );
        let primary_request_key = class.to_string();
        let baseline_request_key = class.unknown_baseline().to_string();
        let confidence = |id: &str| {
            assessment
                .and_then(|value| value.judgments.get(id))
                .map(|value| prediction_confidence_ppm(value.confidence))
        };
        let mut decision = PolicyDecision {
            policy_snapshot: PolicySnapshot(Arc::clone(&table)),
            request_key: primary_request_key.clone(),
            route_projection: primary_request_key.clone(),
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
            predicted_role: Some(class.next_step_role.key().to_owned()),
            predicted_task_family: Some(class.task_family.key().to_owned()),
            prediction_confidence_ppm: confidence(assessment::ROLE),
            task_family_confidence_ppm: confidence(assessment::TASK),
            predictor_contract_digest: assessment.map(|value| value.contract_digest.clone()),
            prediction_confidence_kind: assessment.map(|_| "provider_probability".to_owned()),
            reason: PolicyDecisionReason::NoMatch,
            route_measurement: None,
        };

        if (respect_explicit_route && is_explicitly_routed(&prompt.model))
            || carries_bitrouter_server_tool(prompt)
        {
            return decision;
        }

        // One snapshot for the whole decision: a reload landing mid-decision
        // must not let the tier and the model it maps to come from different
        // tables.
        let Some((raw_static_tier, matched_request_key)) =
            table.tier_for_class(&primary_request_key, &baseline_request_key)
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
        decision.route_measurement =
            match table.route_measurement(raw_static_tier, decision.input_effort) {
                Ok(measurement) => Some(measurement),
                Err(error) => {
                    tracing::warn!(%error, "route measurement evidence unavailable");
                    None
                }
            };
        let (selected_tier, static_clamped) = if apply_tool_floor {
            table.guardrail_with_status(raw_static_tier, prompt)
        } else {
            (raw_static_tier, false)
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

    #[cfg(test)]
    fn route_prompt(
        &self,
        prompt: &mut Prompt,
        headers: &HeaderMap,
        assessment: &Assessment,
    ) -> bool {
        let input_model = prompt.model.clone();
        let input_effort = prompt.params.reasoning_effort;
        let decision = self.decision_for_inner(prompt, headers, true, true, Some(assessment));
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
                bitrouter_ai::types::ReasoningEffortSource::Policy;
        }
        changed
    }

    /// Select for a named policy even when its base model is provider-qualified.
    #[cfg(test)]
    pub(crate) fn decision_for_bound_policy(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        assessment: &Assessment,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, false, true, Some(assessment))
    }

    #[cfg(test)]
    pub(crate) fn candidate_for_guarded_policy(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        assessment: &Assessment,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, false, false, Some(assessment))
    }

    pub(crate) fn decision_for_routing(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        assessment: Option<&Assessment>,
        guarded: bool,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, false, !guarded, assessment)
    }

    pub(crate) fn apply_continuation_adjustment(
        &self,
        decision: &mut PolicyDecision,
        adjustment: &ContinuationAdjustment,
    ) -> crate::gateway::error::Result<()> {
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
                let table = &decision.policy_snapshot;
                let selected_tier = table
                    .stable_tier_of_target(effective_model, pinned_effort)
                    .ok_or_else(|| {
                        crate::gateway::error::BitrouterError::bad_request(
                            "provider continuation model is unavailable in the active policy",
                        )
                    })?;
                decision.selected_tier = Some(selected_tier.to_owned());
                decision.selected_model = Some(effective_model.clone());
                decision.selected_effort = pinned_effort;
                decision.continuation_adjustment = Some("pin".to_owned());
                decision.reason = PolicyDecisionReason::ContinuationPin;
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
        // Persist the owner-opaque identity while committing to the canonical ingress id.
        request_ids: (&str, &str),
        invocation: &EvalInvocation,
        input_model: String,
        input_effort: Option<ReasoningEffort>,
        decision: PolicyDecision,
        headers: &HeaderMap,
    ) -> (Option<Target>, Option<PendingEvalDecision>) {
        self.record_decision_capture(
            input_model,
            input_effort,
            decision,
            headers,
            Some(request_ids),
            Some(invocation),
        )
    }

    #[cfg(test)]
    fn record_decision(
        &self,
        input_model: String,
        input_effort: Option<ReasoningEffort>,
        decision: PolicyDecision,
        headers: &HeaderMap,
        request_id_override: Option<(&str, &str)>,
        invocation: Option<&EvalInvocation>,
    ) -> Option<Target> {
        self.record_decision_capture(
            input_model,
            input_effort,
            decision,
            headers,
            request_id_override,
            invocation,
        )
        .0
    }

    fn record_decision_capture(
        &self,
        input_model: String,
        input_effort: Option<ReasoningEffort>,
        decision: PolicyDecision,
        headers: &HeaderMap,
        request_id_override: Option<(&str, &str)>,
        invocation: Option<&EvalInvocation>,
    ) -> (Option<Target>, Option<PendingEvalDecision>) {
        let baseline_tier = self.eval_baseline_tier(&decision);
        let baseline_effort = baseline_tier
            .as_deref()
            .and_then(|tier| decision.policy_snapshot.effort_of_tier(tier))
            .or(input_effort);
        let ingress_request_id = request_id_override.map(|(_, ingress)| ingress).or_else(|| {
            headers
                .get("x-bitrouter-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
        let request_id = request_id_override
            .map(|(stored, _)| stored)
            .or(ingress_request_id);
        let request_id_for_log = request_id.unwrap_or("-");
        tracing::info!(
            request_id = request_id_for_log,
            request_key = %decision.request_key,
            predicted_task_family = ?decision.predicted_task_family,
            task_family_confidence_ppm = ?decision.task_family_confidence_ppm,
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
            reason = %decision.reason,
            "policy routing decision"
        );
        let pending_decision = if let (Some(observer), Some(request_id), Some(selected_tier)) = (
            &self.eval_observer,
            request_id,
            decision.selected_tier.as_deref(),
        ) {
            Some(PendingEvalDecision {
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
                route_measurement: decision.route_measurement.clone(),
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
                prediction_confidence_ppm: decision.prediction_confidence_ppm,
                task_family_confidence_ppm: decision.task_family_confidence_ppm,
                predictor_contract_digest: decision.predictor_contract_digest.clone(),
                prediction_confidence_kind: decision.prediction_confidence_kind.clone(),
                observation: None,
                observed_at: chrono::Utc::now().to_rfc3339(),
            })
        } else {
            None
        };
        if let (Some(pending), Some(invocation), Some(captured)) = (
            self.eval_observer
                .as_ref()
                .and_then(|observer| observer.pending.as_ref()),
            invocation,
            pending_decision.as_ref(),
        ) {
            pending.insert(invocation, captured.clone());
        }
        if let Some(recorder) = &self.decision_recorder {
            let record = PolicyDecisionRecord {
                captured_at: None,
                request_id: request_id.map(ToString::to_string),
                ingress_request_id_sha256: ingress_request_id
                    .map(crate::gateway::routing::workflow::decision::ingress_request_id_sha256),
                input_model,
                input_effort,
                route_projection: Some(decision.route_projection.clone()),
                request_key: decision.request_key.clone(),
                policy: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                policy_digest: self
                    .eval_observer
                    .as_ref()
                    .map(|item| item.policy_digest.clone()),
                preset_variant: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                baseline_tier,
                baseline_effort,
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
                prediction_confidence_ppm: decision.prediction_confidence_ppm,
                task_family_confidence_ppm: decision.task_family_confidence_ppm,
                predictor_contract_digest: decision.predictor_contract_digest.clone(),
                prediction_confidence_kind: decision.prediction_confidence_kind.clone(),
                route_measurement: decision.route_measurement.clone(),
                reason: decision.reason.to_string(),
            }
            .captured_now();
            if let Err(error) = recorder.record(&record) {
                tracing::warn!(%error, "policy decision recorder failed");
            }
        }
        let selected = (|| {
            let selected_model = decision.selected_model.as_deref()?;
            let selected_tier = decision.selected_tier.as_deref()?;
            let configured = decision
                .policy_snapshot
                .target_of_tier(selected_tier)
                .filter(|target| target.model() == selected_model)
                .cloned()?;
            if decision.continuation_adjustment.as_deref() == Some("pin")
                && configured.effort().is_none()
                && let Some(effort) = decision.selected_effort
            {
                return Some(Target {
                    model: selected_model.to_owned(),
                    effort: Some(effort),
                });
            }
            Some(configured)
        })();
        (selected, pending_decision)
    }
}

const PREDICTION_CONFIDENCE_PPM_MAX: u32 = 1_000_000;

fn prediction_confidence_ppm(confidence: f64) -> u32 {
    let scaled = confidence * f64::from(PREDICTION_CONFIDENCE_PPM_MAX);
    if scaled.is_nan() || scaled <= 0.0 {
        return 0;
    }
    if scaled >= f64::from(PREDICTION_CONFIDENCE_PPM_MAX) {
        return PREDICTION_CONFIDENCE_PPM_MAX;
    }
    let rounded = scaled.round() as u64;
    u32::try_from(rounded).unwrap_or(PREDICTION_CONFIDENCE_PPM_MAX)
}
/// Explicit provider routes and named routers retain their own binding semantics.
fn is_explicitly_routed(model: &str) -> bool {
    model.starts_with('@') || model.contains(':') || crate::config::resolution::is_reserved(model)
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
#[path = "policy_table_router/tests.rs"]
mod tests;
