//! Request settlement bridge for generic evaluation subjects.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bitrouter_sdk::Result as BitrouterResult;
use bitrouter_sdk::event::PipelineEvent;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, Usage};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use uuid::Uuid;

use super::store::EvalStore;
use super::types::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvidenceItem, canonical_digest,
    evidence_digest,
};
use crate::metering::{PricingTable, calculate_charge_micro_usd};
use crate::workflow_state::predictive::TaskFamily;
use crate::workflow_state::predictive::is_task_family_reason_code;
use crate::workflow_state::response_observer::{ObservedActionClass, PredictionObservation};

/// Opaque, process-local identity for one pipeline invocation that produced an
/// evaluable policy decision. Its token and owner never enter serialized event
/// output or durable evidence.
#[derive(Clone)]
pub struct EvalInvocation {
    token: Uuid,
    owner_user_id: String,
}

impl EvalInvocation {
    pub fn new(owner_user_id: impl Into<String>) -> Self {
        Self {
            token: Uuid::new_v4(),
            owner_user_id: owner_user_id.into(),
        }
    }

    pub(crate) fn token(&self) -> Uuid {
        self.token
    }

    pub(crate) fn owner_user_id(&self) -> &str {
        &self.owner_user_id
    }
}

impl std::fmt::Debug for EvalInvocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EvalInvocation(REDACTED)")
    }
}

impl Serialize for EvalInvocation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("EvalInvocation", 1)?;
        state.serialize_field("token", "redacted")?;
        state.end()
    }
}

impl PipelineEvent for EvalInvocation {
    fn event_name(&self) -> &'static str {
        "eval.invocation"
    }
}

/// A policy decision waiting for the always-run settlement stage to attach
/// request outcome evidence. This is process-local correlation state only; it
/// never participates in routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEvalDecision {
    pub request_id: String,
    pub decision_id: String,
    pub policy: String,
    pub policy_digest: String,
    pub route_projection: String,
    pub request_key: String,
    pub selected_tier: String,
    pub selected_effort: Option<ReasoningEffort>,
    pub baseline_tier: Option<String>,
    pub baseline_effort: Option<ReasoningEffort>,
    pub preset: Option<String>,
    pub holdout: bool,
    pub continuation_proposed_tier: Option<String>,
    pub continuation_proposed_model: Option<String>,
    pub continuation_proposed_effort: Option<ReasoningEffort>,
    pub continuation_adjustment: Option<String>,
    pub predicted_role: Option<String>,
    pub predicted_task_family: Option<String>,
    pub predicted_action: Option<String>,
    pub prediction_confidence_ppm: Option<u32>,
    pub task_family_confidence_ppm: Option<u32>,
    pub task_family_reason_codes: Vec<String>,
    pub predictor_contract_digest: Option<String>,
    pub prediction_confidence_kind: Option<String>,
    pub observation: Option<PredictionObservation>,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PredictionObservationSnapshot {
    continuation_proposed_tier: Option<String>,
    continuation_proposed_model: Option<String>,
    continuation_proposed_effort: Option<ReasoningEffort>,
    continuation_adjustment: Option<String>,
    predicted_role: Option<String>,
    predicted_task_family: Option<String>,
    predicted_action: Option<String>,
    prediction_confidence_ppm: Option<u32>,
    task_family_confidence_ppm: Option<u32>,
    task_family_reason_codes: Vec<String>,
    predictor_contract_digest: Option<String>,
    prediction_confidence_kind: Option<String>,
    observed_action: Option<ObservedActionClass>,
    observation_confidence_ppm: Option<u32>,
    observation_reason_code: Option<String>,
}

impl PendingEvalDecision {
    pub(crate) fn observation_snapshot(&self) -> PredictionObservationSnapshot {
        PredictionObservationSnapshot {
            continuation_proposed_tier: bounded_continuation_label(
                self.continuation_proposed_tier.as_deref(),
                128,
            ),
            continuation_proposed_model: bounded_continuation_label(
                self.continuation_proposed_model.as_deref(),
                512,
            ),
            continuation_proposed_effort: self.continuation_proposed_effort,
            continuation_adjustment: bounded_continuation_label(
                self.continuation_adjustment.as_deref(),
                32,
            ),
            predicted_role: self.predicted_role.as_deref().map(normalize_predicted_role),
            predicted_task_family: self
                .predicted_task_family
                .as_deref()
                .map(normalize_predicted_task_family),
            predicted_action: self
                .predicted_action
                .as_deref()
                .map(normalize_predicted_action),
            prediction_confidence_ppm: self
                .prediction_confidence_ppm
                .map(|confidence| confidence.min(1_000_000)),
            task_family_confidence_ppm: self
                .task_family_confidence_ppm
                .map(|confidence| confidence.min(1_000_000)),
            task_family_reason_codes: normalized_task_family_reason_codes(
                &self.task_family_reason_codes,
            ),
            predictor_contract_digest: bounded_continuation_label(
                self.predictor_contract_digest.as_deref(),
                71,
            ),
            prediction_confidence_kind: bounded_continuation_label(
                self.prediction_confidence_kind.as_deref(),
                32,
            ),
            observed_action: self
                .observation
                .map(|observation| observation.observed_action),
            observation_confidence_ppm: self
                .observation
                .map(|observation| observation.confidence_ppm().min(1_000_000)),
            observation_reason_code: self
                .observation
                .map(|observation| observation.reason_code().to_owned()),
        }
    }
}

impl PredictionObservationSnapshot {
    pub(crate) fn attributes(&self) -> BTreeMap<String, String> {
        let mut attributes = BTreeMap::new();
        if let Some(value) = &self.continuation_proposed_tier {
            attributes.insert("continuation_proposed_tier".into(), value.clone());
        }
        if let Some(value) = &self.continuation_proposed_model {
            attributes.insert("continuation_proposed_model".into(), value.clone());
        }
        if let Some(value) = self.continuation_proposed_effort {
            attributes.insert("continuation_proposed_effort".into(), value.to_string());
        }
        if let Some(value) = &self.continuation_adjustment {
            attributes.insert("continuation_adjustment".into(), value.clone());
        }
        if let Some(role) = &self.predicted_role {
            attributes.insert("predicted_role".into(), role.clone());
        }
        if let Some(task_family) = &self.predicted_task_family {
            attributes.insert("predicted_task_family".into(), task_family.clone());
        }
        if let Some(action) = &self.predicted_action {
            attributes.insert("predicted_action".into(), action.clone());
        }
        if let Some(confidence) = self.prediction_confidence_ppm {
            attributes.insert("prediction_confidence_ppm".into(), confidence.to_string());
        }
        if let Some(confidence) = self.task_family_confidence_ppm {
            attributes.insert("task_family_confidence_ppm".into(), confidence.to_string());
        }
        if !self.task_family_reason_codes.is_empty() {
            attributes.insert(
                "task_family_reason_codes".into(),
                self.task_family_reason_codes.join(","),
            );
        }
        if let Some(digest) = &self.predictor_contract_digest {
            attributes.insert("predictor_contract_digest".into(), digest.clone());
        }
        if let Some(kind) = &self.prediction_confidence_kind {
            attributes.insert("prediction_confidence_kind".into(), kind.clone());
        }
        if let Some(action) = self.observed_action {
            attributes.insert("observed_action".into(), action.as_str().into());
        }
        if let Some(confidence) = self.observation_confidence_ppm {
            attributes.insert("observation_confidence_ppm".into(), confidence.to_string());
        }
        if let Some(reason) = &self.observation_reason_code {
            attributes.insert("observation_reason_code".into(), reason.clone());
        }
        if let (Some(predicted), Some(observed)) = (
            self.predicted_action
                .as_deref()
                .and_then(ObservedActionClass::parse)
                .filter(|action| action.is_known()),
            self.observed_action.filter(|action| action.is_known()),
        ) {
            attributes.insert("action_match".into(), (predicted == observed).to_string());
        }
        attributes
    }

    pub(crate) fn write_namespaced(
        &self,
        structural: &mut BTreeMap<String, u64>,
        categorical: &mut BTreeMap<String, String>,
    ) {
        if let Some(value) = &self.continuation_proposed_tier {
            categorical.insert("routing.continuation_proposed_tier".into(), value.clone());
        }
        if let Some(value) = &self.continuation_proposed_model {
            categorical.insert("routing.continuation_proposed_model".into(), value.clone());
        }
        if let Some(value) = self.continuation_proposed_effort {
            categorical.insert(
                "routing.continuation_proposed_effort".into(),
                value.to_string(),
            );
        }
        if let Some(value) = &self.continuation_adjustment {
            categorical.insert("routing.continuation_adjustment".into(), value.clone());
        }
        if let Some(role) = &self.predicted_role {
            categorical.insert("routing.predicted_role".into(), role.clone());
        }
        if let Some(task_family) = &self.predicted_task_family {
            categorical.insert("routing.predicted_task_family".into(), task_family.clone());
        }
        if let Some(action) = &self.predicted_action {
            categorical.insert("routing.predicted_action".into(), action.clone());
        }
        if let Some(confidence) = self.prediction_confidence_ppm {
            structural.insert(
                "routing.prediction_confidence_ppm".into(),
                u64::from(confidence),
            );
        }
        if let Some(confidence) = self.task_family_confidence_ppm {
            structural.insert(
                "routing.task_family_confidence_ppm".into(),
                u64::from(confidence),
            );
        }
        if !self.task_family_reason_codes.is_empty() {
            categorical.insert(
                "routing.task_family_reason_codes".into(),
                self.task_family_reason_codes.join(","),
            );
        }
        if let Some(digest) = &self.predictor_contract_digest {
            categorical.insert("routing.predictor_contract_digest".into(), digest.clone());
        }
        if let Some(kind) = &self.prediction_confidence_kind {
            categorical.insert("routing.prediction_confidence_kind".into(), kind.clone());
        }
        if let Some(action) = self.observed_action {
            categorical.insert("routing.observed_action".into(), action.as_str().into());
        }
        if let Some(confidence) = self.observation_confidence_ppm {
            structural.insert(
                "routing.observation_confidence_ppm".into(),
                u64::from(confidence),
            );
        }
        if let Some(reason) = &self.observation_reason_code {
            categorical.insert("routing.observation_reason_code".into(), reason.clone());
        }
        if let Some(action_match) = self.attributes().get("action_match") {
            categorical.insert("routing.action_match".into(), action_match.clone());
        }
    }
}

pub(crate) fn bounded_continuation_label(value: Option<&str>, max_bytes: usize) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
        })
        .map(ToOwned::to_owned)
}

fn normalize_predicted_role(value: &str) -> String {
    match value {
        "orchestrate" | "implement" | "mechanical" | "verify" | "finalize" | "unknown" => {
            value.to_owned()
        }
        _ => "unknown".to_owned(),
    }
}

fn normalize_predicted_task_family(value: &str) -> String {
    TaskFamily::parse_key(value)
        .unwrap_or(TaskFamily::Unknown)
        .key()
        .to_owned()
}

fn normalize_predicted_action(value: &str) -> String {
    ObservedActionClass::parse(value)
        .unwrap_or(ObservedActionClass::Unknown)
        .as_str()
        .to_owned()
}

fn normalized_task_family_reason_codes(values: &[String]) -> Vec<String> {
    const MAX_REASON_CODES: usize = 8;
    const MAX_CATEGORICAL_BYTES: usize = 128;
    let mut normalized = Vec::new();
    let mut categorical_bytes = 0_usize;
    for code in values
        .iter()
        .map(String::as_str)
        .filter(|value| is_task_family_reason_code(value))
        .collect::<BTreeSet<_>>()
    {
        if normalized.len() == MAX_REASON_CODES {
            break;
        }
        let appended_bytes = code.len() + usize::from(!normalized.is_empty());
        if categorical_bytes.saturating_add(appended_bytes) > MAX_CATEGORICAL_BYTES {
            continue;
        }
        categorical_bytes += appended_bytes;
        normalized.push(code.to_owned());
    }
    normalized
}

/// Bounded-lifetime request correlation used between model selection and
/// settlement. Restarting BitRouter may lose in-flight observations, but can
/// never alter the active routing policy.
const DEFAULT_PENDING_EVAL_CAPACITY: usize = 4_096;
const DEFAULT_PENDING_EVAL_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct PendingEvalDecisionStore {
    entries: Arc<Mutex<BTreeMap<Uuid, PendingEvalEntry>>>,
    capacity: usize,
    ttl: std::time::Duration,
}

struct PendingEvalEntry {
    owner_user_id: String,
    decision: PendingEvalDecision,
    inserted_at: std::time::Instant,
}

impl Default for PendingEvalDecisionStore {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            capacity: DEFAULT_PENDING_EVAL_CAPACITY,
            ttl: DEFAULT_PENDING_EVAL_TTL,
        }
    }
}

impl PendingEvalDecisionStore {
    pub fn insert(&self, invocation: &EvalInvocation, decision: PendingEvalDecision) {
        let now = std::time::Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune_expired(&mut entries, now);
        let token = invocation.token();
        if entries.len() >= self.capacity
            && !entries.contains_key(&token)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(token, entry)| (entry.inserted_at, **token))
                .map(|(token, _)| *token)
        {
            entries.remove(&oldest);
        }
        entries.insert(
            token,
            PendingEvalEntry {
                owner_user_id: invocation.owner_user_id().to_owned(),
                decision,
                inserted_at: now,
            },
        );
    }

    pub fn peek(
        &self,
        invocation: &EvalInvocation,
        owner_user_id: &str,
    ) -> Option<PendingEvalDecision> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune_expired(&mut entries, std::time::Instant::now());
        entries
            .get(&invocation.token())
            .filter(|entry| entry.owner_user_id == owner_user_id)
            .map(|entry| entry.decision.clone())
    }

    pub fn observe(
        &self,
        invocation: &EvalInvocation,
        owner_user_id: &str,
        observation: PredictionObservation,
    ) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune_expired(&mut entries, std::time::Instant::now());
        let Some(entry) = entries
            .get_mut(&invocation.token())
            .filter(|entry| entry.owner_user_id == owner_user_id)
        else {
            return false;
        };
        entry.decision.observation = Some(match entry.decision.observation {
            Some(existing) => existing.merge(observation),
            None => observation,
        });
        true
    }

    pub fn take(
        &self,
        invocation: &EvalInvocation,
        owner_user_id: &str,
    ) -> Option<PendingEvalDecision> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune_expired(&mut entries, std::time::Instant::now());
        if entries
            .get(&invocation.token())
            .is_none_or(|entry| entry.owner_user_id != owner_user_id)
        {
            return None;
        }
        entries
            .remove(&invocation.token())
            .map(|entry| entry.decision)
    }

    pub fn remove(&self, invocation: &EvalInvocation, owner_user_id: &str) -> bool {
        self.take(invocation, owner_user_id).is_some()
    }

    fn prune_expired(
        &self,
        entries: &mut BTreeMap<Uuid, PendingEvalEntry>,
        now: std::time::Instant,
    ) {
        entries.retain(|_, entry| now.duration_since(entry.inserted_at) < self.ttl);
    }

    #[cfg(test)]
    fn with_limits_for_test(capacity: usize, ttl: std::time::Duration) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            capacity: capacity.max(1),
            ttl,
        }
    }

    #[cfg(test)]
    fn len_for_test(&self) -> usize {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune_expired(&mut entries, std::time::Instant::now());
        entries.len()
    }

    #[cfg(test)]
    pub(crate) fn get(
        &self,
        invocation: &EvalInvocation,
        owner_user_id: &str,
    ) -> Option<PendingEvalDecision> {
        self.peek(invocation, owner_user_id)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len_for_test() == 0
    }
}

/// Converts a settled routed request into a generic, redacted eval subject.
/// It deliberately emits no score: evaluator-specific work happens outside
/// the request path and returns through the exchange API.
pub struct EvalSettlementRecorder {
    store: EvalStore,
    pending: PendingEvalDecisionStore,
    pricing: Arc<PricingTable>,
    trajectory: Option<crate::trajectory::settlement::TrajectorySettlementRecorder>,
}

impl EvalSettlementRecorder {
    pub fn new(
        store: EvalStore,
        pending: PendingEvalDecisionStore,
        pricing: Arc<PricingTable>,
    ) -> Self {
        Self {
            store,
            pending,
            pricing,
            trajectory: None,
        }
    }

    pub(crate) fn with_trajectory(
        mut self,
        trajectory: crate::trajectory::settlement::TrajectorySettlementRecorder,
    ) -> Self {
        self.trajectory = Some(trajectory);
        self
    }

    fn subject(
        &self,
        decision: &PendingEvalDecision,
        context: &SettlementContext,
    ) -> anyhow::Result<EvalSubject> {
        if decision.selected_effort != context.reasoning_effort {
            anyhow::bail!("settled reasoning effort does not match the recorded policy treatment");
        }
        let mut attributes = BTreeMap::from([
            ("provider".to_string(), context.provider_id.clone()),
            ("model".to_string(), context.model_id.clone()),
            (
                "prompt_tokens".to_string(),
                context.prompt_tokens.to_string(),
            ),
            (
                "completion_tokens".to_string(),
                context.completion_tokens.to_string(),
            ),
            (
                "reasoning_tokens".to_string(),
                context.reasoning_tokens.to_string(),
            ),
            (
                "request_duration_ms".to_string(),
                context.request_duration_ms.to_string(),
            ),
            ("success".to_string(), context.error.is_none().to_string()),
        ]);
        if let Some(error) = &context.error {
            attributes.insert("error_code".into(), error.error_code().into());
        }
        if let Some(effort) = context.reasoning_effort {
            attributes.insert("reasoning_effort".into(), effort.to_string());
        }
        attributes.extend(decision.observation_snapshot().attributes());
        let usage = Usage {
            prompt_tokens: context.prompt_tokens,
            completion_tokens: context.completion_tokens,
            reasoning_tokens: context.reasoning_tokens,
            cache_read_tokens: context.cache_read_tokens,
            cache_write_tokens: context.cache_write_tokens,
            web_search_count: context.web_search_count,
            origin: context.usage_origin,
            raw: None,
        };
        if let Some(pricing) = self
            .pricing
            .resolve(&context.provider_id, &context.model_id)
            && let Some(cost) = calculate_charge_micro_usd(&usage, &pricing)
        {
            attributes.insert("cost_micro_usd".into(), cost.to_string());
        }
        let evidence = vec![EvidenceItem {
            evidence_id: "request-outcome".into(),
            kind: "request.outcome".into(),
            digest: canonical_digest(&attributes)?,
            redacted: true,
            attributes,
        }];
        let evidence_digest = evidence_digest(&evidence)?;
        Ok(EvalSubject {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: format!("request:{}", context.request_id),
            scope: EvalScope::Request,
            subject_id: context.request_id.clone(),
            policy_digest: decision.policy_digest.clone(),
            preset: decision.preset.clone(),
            cohort: None,
            holdout: decision.holdout,
            decisions: vec![EvalDecisionRef {
                decision_id: decision.decision_id.clone(),
                policy: decision.policy.clone(),
                route_projection: decision.route_projection.clone(),
                request_key: decision.request_key.clone(),
                selected_tier: decision.selected_tier.clone(),
                selected_effort: decision.selected_effort,
                baseline_tier: decision.baseline_tier.clone(),
                baseline_effort: decision.baseline_effort,
                policy_digest: decision.policy_digest.clone(),
            }],
            requested_dimensions: BTreeSet::from([
                "cost.usd_micros".into(),
                "latency.ms".into(),
                "quality.pass".into(),
            ]),
            evidence,
            evidence_digest,
            observed_at: decision.observed_at.clone(),
        })
    }
}

#[async_trait]
impl SettlementRecorder for EvalSettlementRecorder {
    async fn record(&self, context: &mut SettlementContext) -> BitrouterResult<()> {
        let invocation = context.get_event::<EvalInvocation>().cloned();
        let pending = invocation
            .as_ref()
            .and_then(|invocation| self.pending.peek(invocation, context.caller.user_id()));
        if let Some(trajectory) = &self.trajectory {
            let snapshot = pending
                .as_ref()
                .map(PendingEvalDecision::observation_snapshot);
            let disposition = trajectory
                .record_if_tracked(context, snapshot.as_ref())
                .await
                .map_err(|error| {
                    bitrouter_sdk::BitrouterError::internal(format!(
                        "persisting trajectory settlement: {error}"
                    ))
                })?;
            match disposition {
                crate::trajectory::settlement::TrajectorySettlementDisposition::Untracked => {}
                crate::trajectory::settlement::TrajectorySettlementDisposition::AwaitingAuthoritativeMetering => {
                    return Ok(());
                }
                crate::trajectory::settlement::TrajectorySettlementDisposition::Persisted
                | crate::trajectory::settlement::TrajectorySettlementDisposition::AlreadyTerminal => {
                    if let Some(invocation) = &invocation {
                        self.pending
                            .remove(invocation, context.caller.user_id());
                    }
                    return Ok(());
                }
            }
        }
        let Some(decision) = pending else {
            return Ok(());
        };
        let subject = self.subject(&decision, context).map_err(|error| {
            bitrouter_sdk::BitrouterError::internal(format!(
                "building request eval subject: {error}"
            ))
        })?;
        self.store
            .insert_subject_owned(&subject, context.caller.user_id())
            .await
            .map_err(|error| {
                bitrouter_sdk::BitrouterError::internal(format!(
                    "persisting request eval subject: {error}"
                ))
            })?;
        if let Some(invocation) = &invocation {
            self.pending.take(invocation, context.caller.user_id());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::event::EventBus;
    use bitrouter_sdk::language_model::types::ReasoningEffort;
    use bitrouter_sdk::language_model::{SettlementContext, SettlementRecorder, UsageOrigin};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::{
        EvalInvocation, EvalSettlementRecorder, PendingEvalDecision, PendingEvalDecisionStore,
        normalized_task_family_reason_codes,
    };
    use crate::eval::store::EvalStore;
    use crate::metering::PricingTable;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn task_family_reason_codes_fit_the_categorical_bound() {
        let values = [
            "task_code_generation",
            "task_code_debugging",
            "task_code_review",
            "task_code_sql_database",
            "task_code_frontend_ui",
            "task_code_devops_config",
            "task_code_repository_analysis",
            "task_agent_multi_step_planning",
            "task_agent_workflow_execution",
            "task_agent_web_research",
            "task_agent_memory_operations",
            "task_agent_general",
            "task_unknown",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let normalized = normalized_task_family_reason_codes(&values);

        assert!(normalized.len() <= 8);
        assert!(normalized.join(",").len() <= 128);
        assert!(
            normalized
                .iter()
                .all(|code| crate::workflow_state::predictive::is_task_family_reason_code(code))
        );
    }

    #[tokio::test]
    async fn settlement_creates_a_redacted_request_subject() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("local");
        pending.insert(
            &invocation,
            PendingEvalDecision {
                request_id: "request-1".into(),
                decision_id: "decision-1".into(),
                policy: "auto:cost".into(),
                policy_digest: DIGEST.into(),
                route_projection: "opening".into(),
                request_key: "opening".into(),
                selected_tier: "economy".into(),
                selected_effort: Some(ReasoningEffort::Low),
                baseline_tier: Some("strong".into()),
                baseline_effort: Some(ReasoningEffort::High),
                preset: Some("auto:cost".into()),
                holdout: false,
                continuation_proposed_tier: Some("balanced".into()),
                continuation_proposed_model: Some("balanced:balanced-model".into()),
                continuation_proposed_effort: Some(ReasoningEffort::Medium),
                continuation_adjustment: Some("pin".into()),
                predicted_role: None,
                predicted_task_family: Some("code:review".into()),
                predicted_action: None,
                prediction_confidence_ppm: None,
                task_family_confidence_ppm: Some(1_500_000),
                task_family_reason_codes: vec![
                    "task_code_review".into(),
                    "customer_secret".into(),
                    "task_code_review".into(),
                ],
                predictor_contract_digest: None,
                prediction_confidence_kind: None,
                observation: None,
                observed_at: "2026-08-08T00:00:00Z".into(),
            },
        );
        let recorder = EvalSettlementRecorder::new(
            store.clone(),
            pending.clone(),
            Arc::new(PricingTable::new()),
        );
        let mut context = settlement_context_with(&invocation);
        context.reasoning_effort = Some(ReasoningEffort::Low);

        recorder.record(&mut context).await?;

        let subject = store
            .subject("request:request-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("request subject missing"))?;
        assert_eq!(subject.policy_digest, DIGEST);
        assert_eq!(subject.decisions.len(), 1);
        assert_eq!(
            subject.decisions[0].selected_effort,
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            subject.decisions[0].baseline_effort,
            Some(ReasoningEffort::High)
        );
        assert!(subject.evidence.iter().all(|item| item.redacted));
        let evidence = subject
            .evidence
            .first()
            .ok_or_else(|| anyhow::anyhow!("request outcome evidence missing"))?;
        assert_eq!(
            evidence.attributes.get("continuation_proposed_tier"),
            Some(&"balanced".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_proposed_model"),
            Some(&"balanced:balanced-model".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_proposed_effort"),
            Some(&"medium".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_adjustment"),
            Some(&"pin".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("predicted_task_family"),
            Some(&"code:review".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("task_family_confidence_ppm"),
            Some(&"1000000".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("task_family_reason_codes"),
            Some(&"task_code_review".to_owned())
        );
        assert_eq!(
            subject.requested_dimensions,
            BTreeSet::from([
                "cost.usd_micros".to_string(),
                "latency.ms".to_string(),
                "quality.pass".to_string(),
            ])
        );
        assert!(pending.peek(&invocation, "local").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn settlement_storage_failure_preserves_pending_decision_for_retry() -> anyhow::Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db.clone());
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("local");
        pending.insert(
            &invocation,
            PendingEvalDecision {
                request_id: "request-1".into(),
                decision_id: "decision-1".into(),
                policy: "auto:cost".into(),
                policy_digest: DIGEST.into(),
                route_projection: "opening".into(),
                request_key: "opening".into(),
                selected_tier: "economy".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                preset: Some("auto:cost".into()),
                holdout: false,
                continuation_proposed_tier: None,
                continuation_proposed_model: None,
                continuation_proposed_effort: None,
                continuation_adjustment: None,
                predicted_role: Some("implement".into()),
                predicted_task_family: Some("code:review".into()),
                predicted_action: Some("mutate".into()),
                prediction_confidence_ppm: Some(900_000),
                task_family_confidence_ppm: Some(800_000),
                task_family_reason_codes: Vec::new(),
                predictor_contract_digest: Some(
                    "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec"
                        .into(),
                ),
                prediction_confidence_kind: Some("heuristic_margin".into()),
                observation: Some(
                    crate::workflow_state::response_observer::PredictionObservation::new(
                        crate::workflow_state::response_observer::ObservedActionClass::Mutate,
                    ),
                ),
                observed_at: "2026-08-08T00:00:00Z".into(),
            },
        );
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TRIGGER fail_eval_subject BEFORE INSERT ON eval_subjects \
             BEGIN SELECT RAISE(FAIL, 'injected eval failure'); END"
                .to_owned(),
        ))
        .await?;
        let recorder = EvalSettlementRecorder::new(
            store.clone(),
            pending.clone(),
            Arc::new(PricingTable::new()),
        );
        let mut context = settlement_context_with(&invocation);

        assert!(recorder.record(&mut context).await.is_err());
        assert!(pending.peek(&invocation, "local").is_some());
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER fail_eval_subject".to_owned(),
        ))
        .await?;

        recorder.record(&mut context).await?;

        assert!(pending.peek(&invocation, "local").is_none());
        assert!(store.subject("request:request-1").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn request_subject_digest_is_stable_across_storage_retries() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let recorder = EvalSettlementRecorder::new(
            EvalStore::new(db),
            PendingEvalDecisionStore::default(),
            Arc::new(PricingTable::new()),
        );
        let decision = PendingEvalDecision {
            request_id: "request-1".into(),
            decision_id: "decision-1".into(),
            policy: "auto:cost".into(),
            policy_digest: DIGEST.into(),
            route_projection: "opening".into(),
            request_key: "opening".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            preset: Some("auto:cost".into()),
            holdout: false,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some("implement".into()),
            predicted_task_family: Some("code:review".into()),
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(900_000),
            task_family_confidence_ppm: Some(800_000),
            task_family_reason_codes: Vec::new(),
            predictor_contract_digest: Some(
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            ),
            prediction_confidence_kind: Some("heuristic_margin".into()),
            observation: None,
            observed_at: "2026-08-08T00:00:00Z".into(),
        };
        let first = recorder.subject(&decision, &settlement_context())?;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let second = recorder.subject(&decision, &settlement_context())?;

        assert_eq!(first.semantic_digest()?, second.semantic_digest()?);
        Ok(())
    }

    #[test]
    fn pending_eval_store_prunes_expired_entries_and_caps_capacity() {
        let store =
            PendingEvalDecisionStore::with_limits_for_test(2, std::time::Duration::from_secs(60));
        let first = EvalInvocation::new("local");
        let second = EvalInvocation::new("local");
        let third = EvalInvocation::new("local");
        for (invocation, request_id) in [(&first, "first"), (&second, "second"), (&third, "third")]
        {
            let mut decision = test_decision(request_id);
            decision.request_id = request_id.to_owned();
            store.insert(invocation, decision);
        }

        assert!(store.peek(&first, "local").is_none());
        assert!(store.peek(&second, "local").is_some());
        assert!(store.peek(&third, "local").is_some());
        assert_eq!(store.len_for_test(), 2);

        let expiring = PendingEvalDecisionStore::with_limits_for_test(1, std::time::Duration::ZERO);
        let invocation = EvalInvocation::new("local");
        expiring.insert(&invocation, test_decision("expired"));
        assert!(expiring.peek(&invocation, "local").is_none());
        assert_eq!(expiring.len_for_test(), 0);
    }

    fn test_decision(request_id: &str) -> PendingEvalDecision {
        PendingEvalDecision {
            request_id: request_id.to_owned(),
            decision_id: format!("decision-{request_id}"),
            policy: "auto:cost".into(),
            policy_digest: DIGEST.into(),
            route_projection: "opening".into(),
            request_key: "opening".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            preset: Some("auto:cost".into()),
            holdout: false,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some("implement".into()),
            predicted_task_family: Some("code:review".into()),
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(900_000),
            task_family_confidence_ppm: Some(800_000),
            task_family_reason_codes: Vec::new(),
            predictor_contract_digest: Some(
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            ),
            prediction_confidence_kind: Some("heuristic_margin".into()),
            observation: None,
            observed_at: "2026-08-08T00:00:00Z".into(),
        }
    }

    fn settlement_context() -> SettlementContext {
        SettlementContext {
            request_id: "request-1".into(),
            caller: CallerContext::local(),
            target: None,
            model_id: "model".into(),
            reasoning_effort: None,
            provider_id: "provider".into(),
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
            request_duration_ms: 123,
            upstream_duration_ms: Some(100),
            ttft_ms: Some(10),
            generation_duration_ms: Some(90),
            first_token_kind: None,
            finish_reason: None,
            error: None,
            events: EventBus::default(),
        }
    }

    fn settlement_context_with(invocation: &EvalInvocation) -> SettlementContext {
        let mut context = settlement_context();
        context.emit(invocation.clone());
        context
    }
}
