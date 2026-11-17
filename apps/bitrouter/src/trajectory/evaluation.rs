use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::storage::evaluation::{canonical_digest, evidence_digest, validate_result_for_subject};
use bitrouter_ai::eval::{
    EVAL_RESULT_SCHEMA_VERSION, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, EvidenceItem, MetricUnit, MetricValue,
};

use super::health::reduce;
use super::types::{HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind};

pub(crate) const TRAJECTORY_EVAL_TOPIC: &str = "eval.trajectory-operational.v1";
pub(crate) const TRAJECTORY_EVALUATOR_ID: &str = "bitrouter.trajectory-operational";
pub(crate) const TRAJECTORY_EVALUATOR_AUTHORITY_ID: &str = "bitrouter.builtin";
pub(crate) const TRAJECTORY_EVALUATOR_VERSION: &str = "1";
const TRAJECTORY_EVALUATOR_CONFIG: &str = "generic-inconclusive-operational-v1";

pub(crate) fn trajectory_evaluator_config_digest() -> Result<String> {
    canonical_digest(&(TRAJECTORY_EVALUATOR_ID, TRAJECTORY_EVALUATOR_CONFIG))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrajectoryEvaluationEnvelope {
    pub subject: EvalSubject,
    pub result: EvaluationResult,
}

pub(crate) fn validate_evaluation_envelope(envelope: &TrajectoryEvaluationEnvelope) -> Result<()> {
    validate_result_for_subject(&envelope.result, &envelope.subject)?;
    if envelope.subject.scope != EvalScope::Episode
        || envelope.result.evaluator.authority_id != TRAJECTORY_EVALUATOR_AUTHORITY_ID
        || envelope.result.evaluator.evaluator_id != TRAJECTORY_EVALUATOR_ID
        || envelope.result.evaluator.kind != EvaluatorKind::Generic
        || envelope.result.evaluator.version != TRAJECTORY_EVALUATOR_VERSION
        || envelope.result.evaluator.config_digest != trajectory_evaluator_config_digest()?
        || envelope.result.verdict != EvalVerdict::Inconclusive
        || !envelope.result.hard_violations.is_empty()
        || envelope.result.confidence_ppm.is_some()
        || envelope.result.metrics.contains_key("quality.pass")
        || envelope.subject.requested_dimensions
            != envelope.result.metrics.keys().cloned().collect()
    {
        anyhow::bail!("trajectory operational evaluation envelope violates its fixed contract")
    }
    Ok(())
}

#[cfg(any(feature = "core", feature = "multi-instance"))]
pub(crate) fn validate_builtin_trajectory_result(result: &EvaluationResult) -> Result<()> {
    use crate::trajectory::evaluation::{
        TRAJECTORY_EVALUATOR_AUTHORITY_ID, TRAJECTORY_EVALUATOR_ID, TRAJECTORY_EVALUATOR_VERSION,
        trajectory_evaluator_config_digest,
    };

    if result.evaluator.authority_id != TRAJECTORY_EVALUATOR_AUTHORITY_ID
        || result.evaluator.evaluator_id != TRAJECTORY_EVALUATOR_ID
        || result.evaluator.kind != EvaluatorKind::Generic
        || result.evaluator.version != TRAJECTORY_EVALUATOR_VERSION
        || result.evaluator.config_digest != trajectory_evaluator_config_digest()?
        || result.verdict != EvalVerdict::Inconclusive
        || !result.hard_violations.is_empty()
        || result.confidence_ppm.is_some()
    {
        anyhow::bail!("trusted trajectory principal requires the built-in operational evaluator")
    }
    if result.metrics.keys().any(|metric| {
        !metric.starts_with("trajectory.")
            && !matches!(metric.as_str(), "cost.usd_micros" | "latency.ms")
    }) {
        anyhow::bail!("trusted trajectory principal submitted a metric outside its scope")
    }
    Ok(())
}

pub(crate) fn build_operational_evaluation(
    events: &[TrajectoryEvent],
) -> Result<TrajectoryEvaluationEnvelope> {
    let settlement = events
        .last()
        .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
        .ok_or_else(|| anyhow::anyhow!("trajectory evaluation requires a terminal settlement"))?;
    let snapshot = reduce(events, &BTreeSet::new())?;
    // Episode observations retain operational facts and routing receipts. Policy
    // decisions belong to the independent typed request/shared Eval producers.
    let metrics = operational_metrics(&snapshot.health, settlement)?;
    let requested_dimensions = metrics.keys().cloned().collect::<BTreeSet<_>>();
    let event_digests = events
        .iter()
        .map(|event| event.content_digest.as_str())
        .collect::<Vec<_>>();
    let event_evidence = EvidenceItem {
        evidence_id: "episode-events".to_owned(),
        kind: "trajectory.event_digests".to_owned(),
        digest: canonical_digest(&event_digests)?,
        redacted: true,
        attributes: BTreeMap::from([
            ("event_count".to_owned(), events.len().to_string()),
            (
                "through_sequence".to_owned(),
                snapshot.through_sequence.to_string(),
            ),
        ]),
    };
    let snapshot_evidence = EvidenceItem {
        evidence_id: "trajectory-snapshot".to_owned(),
        kind: "trajectory.snapshot".to_owned(),
        digest: snapshot.evidence_digest.clone(),
        redacted: true,
        attributes: BTreeMap::from([
            (
                "history_completeness".to_owned(),
                completeness_name(snapshot.health.completeness).to_owned(),
            ),
            (
                "request_count".to_owned(),
                snapshot.health.request_count.to_string(),
            ),
            (
                "settled_request_count".to_owned(),
                snapshot.health.settled_request_count.to_string(),
            ),
        ]),
    };
    let mut evidence = vec![event_evidence, snapshot_evidence];
    if let Some(prediction_evidence) = prediction_observation_evidence(settlement)? {
        evidence.push(prediction_evidence);
    }
    let mut routing = BTreeMap::new();
    for item in events
        .iter()
        .flat_map(|event| &event.evidence.routing_evidence)
    {
        if let Some(previous) = routing.insert(item.evidence_id.clone(), item.clone())
            && previous != *item
        {
            anyhow::bail!("routing receipt identity has conflicting evidence");
        }
    }
    evidence.extend(routing.into_values());
    let evidence_digest = evidence_digest(&evidence)?;
    let evidence_refs = evidence
        .iter()
        .map(|item| item.evidence_id.clone())
        .collect();
    let eval_id = format!(
        "trajectory:{}:{}",
        snapshot.episode_id, snapshot.through_sequence
    );
    let subject = EvalSubject {
        schema_version: bitrouter_ai::eval::EVAL_SUBJECT_SCHEMA_VERSION,
        identity: bitrouter_ai::eval::EvalSubjectIdentity::GatewayGeneric {},
        eval_id: eval_id.clone(),
        scope: EvalScope::Episode,
        subject_id: snapshot.episode_id.clone(),
        cohort: None,
        holdout: false,
        decisions: Vec::new(),
        requested_dimensions,
        evidence,
        evidence_digest: evidence_digest.clone(),
        observed_at: settlement.captured_at.clone(),
    };
    let result = EvaluationResult {
        schema_version: EVAL_RESULT_SCHEMA_VERSION,
        eval_id,
        evidence_digest,
        evaluator: EvaluatorIdentity {
            authority_id: TRAJECTORY_EVALUATOR_AUTHORITY_ID.to_owned(),
            evaluator_id: TRAJECTORY_EVALUATOR_ID.to_owned(),
            kind: EvaluatorKind::Generic,
            version: TRAJECTORY_EVALUATOR_VERSION.to_owned(),
            config_digest: trajectory_evaluator_config_digest()?,
        },
        verdict: EvalVerdict::Inconclusive,
        decision_credit: BTreeMap::new(),
        metrics,
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs,
        idempotency_key: format!(
            "trajectory-operational:{}:{}",
            snapshot.episode_id, snapshot.through_sequence
        ),
        submitted_at: settlement.captured_at.clone(),
    };
    let envelope = TrajectoryEvaluationEnvelope { subject, result };
    validate_evaluation_envelope(&envelope)?;
    Ok(envelope)
}

fn prediction_observation_evidence(settlement: &TrajectoryEvent) -> Result<Option<EvidenceItem>> {
    let mut attributes = BTreeMap::new();
    for (event_key, attribute_key) in [
        ("routing.predicted_role", "predicted_role"),
        ("routing.predicted_task_family", "predicted_task_family"),
        (
            "routing.task_family_reason_codes",
            "task_family_reason_codes",
        ),
        ("routing.predicted_action", "predicted_action"),
        (
            "routing.predictor_contract_digest",
            "predictor_contract_digest",
        ),
        (
            "routing.prediction_confidence_kind",
            "prediction_confidence_kind",
        ),
        ("routing.observed_action", "observed_action"),
        ("routing.observation_reason_code", "observation_reason_code"),
        ("routing.action_match", "action_match"),
        (
            "routing.continuation_proposed_tier",
            "continuation_proposed_tier",
        ),
        (
            "routing.continuation_proposed_model",
            "continuation_proposed_model",
        ),
        ("routing.continuation_adjustment", "continuation_adjustment"),
    ] {
        if let Some(value) = settlement.evidence.categorical.get(event_key) {
            attributes.insert(attribute_key.to_owned(), value.clone());
        }
    }
    if let Some(confidence) = settlement
        .evidence
        .structural
        .get("routing.prediction_confidence_ppm")
    {
        attributes.insert(
            "prediction_confidence_ppm".to_owned(),
            confidence.to_string(),
        );
    }
    if let Some(confidence) = settlement
        .evidence
        .structural
        .get("routing.task_family_confidence_ppm")
    {
        attributes.insert(
            "task_family_confidence_ppm".to_owned(),
            confidence.to_string(),
        );
    }
    if let Some(confidence) = settlement
        .evidence
        .structural
        .get("routing.observation_confidence_ppm")
    {
        attributes.insert(
            "observation_confidence_ppm".to_owned(),
            confidence.to_string(),
        );
    }
    if attributes.is_empty() {
        return Ok(None);
    }
    Ok(Some(EvidenceItem {
        evidence_id: "routing-prediction-observation".to_owned(),
        kind: "routing.prediction_observation".to_owned(),
        digest: canonical_digest(&attributes)?,
        redacted: true,
        attributes,
    }))
}

fn operational_metrics(
    health: &super::types::TrajectoryHealth,
    settlement: &TrajectoryEvent,
) -> Result<BTreeMap<String, MetricValue>> {
    let mut metrics = BTreeMap::new();
    insert_u64(
        &mut metrics,
        "trajectory.request_count",
        health.request_count,
        MetricUnit::Count,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.settled_request_count",
        health.settled_request_count,
        MetricUnit::Count,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.elapsed_ms",
        health.elapsed_ms,
        MetricUnit::Milliseconds,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.same_projection_streak",
        health.same_projection_streak,
        MetricUnit::Count,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.same_selected_tier_streak",
        health.same_selected_tier_streak,
        MetricUnit::Count,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.unprotected_streak",
        health.consecutive_unprotected_requests,
        MetricUnit::Count,
    )?;
    insert_u64(
        &mut metrics,
        "trajectory.recovery_count",
        health.recovery_count,
        MetricUnit::Count,
    )?;
    if let Some(value) = health.context_growth_ppm {
        insert_u64(
            &mut metrics,
            "trajectory.context_growth_ppm",
            value,
            MetricUnit::ScalarMicros,
        )?;
    }
    if let Some(value) = health.total_tokens {
        insert_u64(
            &mut metrics,
            "trajectory.total_tokens",
            value,
            MetricUnit::Count,
        )?;
    }
    if let Some(value) = health.settled_cost_micro_usd {
        insert_u64(
            &mut metrics,
            "trajectory.cost.usd_micros",
            value,
            MetricUnit::MicroUsd,
        )?;
    }
    metrics.insert(
        "trajectory.history_complete".to_owned(),
        MetricValue::new(
            i64::from(health.completeness == HistoryCompleteness::Complete),
            MetricUnit::Boolean,
        ),
    );
    if let Some(value) = settlement
        .evidence
        .structural
        .get("settlement.cost_micro_usd")
        .copied()
    {
        insert_u64(&mut metrics, "cost.usd_micros", value, MetricUnit::MicroUsd)?;
    }
    if let Some(value) = settlement
        .evidence
        .structural
        .get("settlement.duration_ms")
        .copied()
    {
        insert_u64(&mut metrics, "latency.ms", value, MetricUnit::Milliseconds)?;
    }
    Ok(metrics)
}

fn insert_u64(
    metrics: &mut BTreeMap<String, MetricValue>,
    id: &str,
    value: u64,
    unit: MetricUnit,
) -> Result<()> {
    metrics.insert(
        id.to_owned(),
        MetricValue::new(
            i64::try_from(value).context("operational metric exceeds i64")?,
            unit,
        ),
    );
    Ok(())
}

fn completeness_name(value: HistoryCompleteness) -> &'static str {
    match value {
        HistoryCompleteness::Complete => "complete",
        HistoryCompleteness::Incomplete => "incomplete",
        HistoryCompleteness::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{build_operational_evaluation, operational_metrics};
    use crate::trajectory::types::{
        HistoryCompleteness, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind,
        TrajectoryEvidence, TrajectoryHealth,
    };
    use bitrouter_ai::eval::{EvalScope, EvalVerdict, EvidenceItem, MetricUnit};
    use serde_json::json;

    #[test]
    fn episode_evaluation_is_operational_inconclusive_and_replayable() -> anyhow::Result<()> {
        let events = complete_events(Some(15), Some(70))?;
        let envelope = build_operational_evaluation(&events)?;

        assert_eq!(envelope.subject.scope, EvalScope::Episode);
        assert_eq!(envelope.subject.subject_id, "episode-1");
        assert_eq!(envelope.subject.eval_id, "trajectory:episode-1:2");
        assert!(matches!(
            envelope.subject.identity,
            bitrouter_ai::eval::EvalSubjectIdentity::GatewayGeneric {}
        ));
        assert!(envelope.subject.decisions.is_empty());
        assert!(envelope.result.decision_credit.is_empty());
        assert_eq!(envelope.result.metrics["cost.usd_micros"].value, 70);
        assert_eq!(envelope.result.metrics["latency.ms"].value, 100);
        assert_eq!(envelope.result.metrics["trajectory.total_tokens"].value, 15);
        assert_eq!(
            envelope.subject.requested_dimensions,
            envelope.result.metrics.keys().cloned().collect()
        );
        assert_eq!(envelope.result.verdict, EvalVerdict::Inconclusive);
        assert_eq!(
            envelope.result.evaluator.evaluator_id,
            "bitrouter.trajectory-operational"
        );
        assert!(envelope.result.hard_violations.is_empty());
        assert!(!envelope.result.metrics.contains_key("quality.pass"));
        assert_eq!(envelope.subject.observed_at, "2026-08-01T00:00:01Z");
        assert_eq!(envelope.result.submitted_at, envelope.subject.observed_at);
        Ok(())
    }

    #[test]
    fn episode_evaluation_requests_only_available_unknown_safe_metrics() -> anyhow::Result<()> {
        let events = complete_events(None, None)?;
        let envelope = build_operational_evaluation(&events)?;

        assert!(
            !envelope
                .result
                .metrics
                .contains_key("trajectory.total_tokens")
        );
        assert!(
            !envelope
                .result
                .metrics
                .contains_key("trajectory.cost.usd_micros")
        );
        assert!(!envelope.result.metrics.contains_key("cost.usd_micros"));
        assert_eq!(
            envelope.subject.requested_dimensions,
            envelope.result.metrics.keys().cloned().collect()
        );
        Ok(())
    }

    #[test]
    fn tracked_eval_evidence_keeps_continuation_proposal_and_adjustment() -> anyhow::Result<()> {
        let mut events = complete_events(Some(15), Some(70))?;
        let settlement = events
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("settlement missing"))?;
        settlement.evidence.categorical.extend(BTreeMap::from([
            (
                "routing.continuation_proposed_tier".to_owned(),
                "economy".to_owned(),
            ),
            (
                "routing.continuation_proposed_model".to_owned(),
                "provider:economy".to_owned(),
            ),
            (
                "routing.continuation_adjustment".to_owned(),
                "pin".to_owned(),
            ),
        ]));
        resign(settlement)?;

        let envelope = build_operational_evaluation(&events)?;
        let evidence = envelope
            .subject
            .evidence
            .iter()
            .find(|item| item.evidence_id == "routing-prediction-observation")
            .ok_or_else(|| anyhow::anyhow!("prediction evidence missing"))?;
        assert_eq!(
            evidence.attributes.get("continuation_proposed_tier"),
            Some(&"economy".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_proposed_model"),
            Some(&"provider:economy".to_owned())
        );
        assert_eq!(
            evidence.attributes.get("continuation_adjustment"),
            Some(&"pin".to_owned())
        );
        Ok(())
    }

    #[test]
    fn context_growth_above_one_hundred_percent_uses_unbounded_ratio_micros() -> anyhow::Result<()>
    {
        let events = complete_events(None, None)?;
        let metrics = operational_metrics(
            &TrajectoryHealth {
                completeness: HistoryCompleteness::Complete,
                request_count: 2,
                settled_request_count: 1,
                unsettled_request_count: 1,
                elapsed_ms: 10,
                latest_projection: None,
                same_projection_streak: 1,
                same_selected_tier_streak: 1,
                consecutive_unprotected_requests: 1,
                recovery_count: 0,
                requests_since_recovery: None,
                context_growth_ppm: Some(1_500_000),
                total_tokens: None,
                settled_cost_micro_usd: None,
            },
            &events[1],
        )?;
        let growth = metrics
            .get("trajectory.context_growth_ppm")
            .ok_or_else(|| anyhow::anyhow!("growth metric missing"))?;
        assert_eq!(growth.value, 1_500_000);
        assert_eq!(growth.unit, MetricUnit::ScalarMicros);
        Ok(())
    }

    #[test]
    fn interleaved_settlements_keep_current_metrics_without_policy_credit() -> anyhow::Result<()> {
        let events = vec![
            request_start(1, "start-a", "request-a", 20)?,
            request_start(2, "start-b", "request-b", 40)?,
            request_settlement(3, "settlement-a", "request-a", 100, 15, 70)?,
            request_settlement(4, "settlement-b", "request-b", 200, 20, 90)?,
        ];
        let settled_a = build_operational_evaluation(&events[..3])?;
        let settled_b = build_operational_evaluation(&events)?;
        for envelope in [&settled_a, &settled_b] {
            assert!(envelope.subject.decisions.is_empty());
            assert!(envelope.result.decision_credit.is_empty());
        }
        assert_eq!(settled_a.result.metrics["cost.usd_micros"].value, 70);
        assert_eq!(settled_a.result.metrics["latency.ms"].value, 100);
        assert_eq!(
            settled_a.result.metrics["trajectory.settled_request_count"].value,
            1
        );
        assert_eq!(settled_b.result.metrics["cost.usd_micros"].value, 90);
        assert_eq!(settled_b.result.metrics["latency.ms"].value, 200);
        assert_eq!(
            settled_b.result.metrics["trajectory.cost.usd_micros"].value,
            160
        );
        assert_eq!(
            settled_b.result.metrics["trajectory.total_tokens"].value,
            35
        );
        Ok(())
    }

    #[test]
    fn native_routing_receipts_remain_evidence_and_reject_conflicts() -> anyhow::Result<()> {
        let receipt = EvidenceItem {
            evidence_id: "native-plan".into(),
            kind: "routing.plan".into(),
            digest: crate::storage::evaluation::canonical_digest(&"native-plan")?,
            redacted: true,
            attributes: BTreeMap::from([("model".into(), "provider:model".into())]),
        };
        let mut events = complete_events(Some(15), Some(70))?;
        events[1].evidence.routing_evidence.push(receipt.clone());
        resign(&mut events[1])?;
        let envelope = build_operational_evaluation(&events)?;
        assert_eq!(
            envelope
                .subject
                .evidence
                .iter()
                .find(|item| item.kind == "routing.plan"),
            Some(&receipt)
        );
        assert!(envelope.subject.decisions.is_empty());
        assert!(envelope.result.decision_credit.is_empty());
        let mut invalid = events.clone();
        invalid[1].evidence.routing_evidence[0].redacted = false;
        resign(&mut invalid[1])?;
        assert!(build_operational_evaluation(&invalid).is_err());
        events.push(request_start(3, "start-2", "request-2", 40)?);
        let mut conflicting = request_settlement(4, "settled-2", "request-2", 200, 20, 90)?;
        let mut changed = receipt;
        changed.digest = crate::storage::evaluation::canonical_digest(&"changed-plan")?;
        conflicting.evidence.routing_evidence.push(changed);
        resign(&mut conflicting)?;
        events.push(conflicting);
        assert!(build_operational_evaluation(&events).is_err());
        Ok(())
    }

    #[test]
    fn raw_old_guard_intent_never_becomes_policy_decision_or_credit() -> anyhow::Result<()> {
        let start = request_start(1, "start-old", "request-old", 20)?;
        let snapshot =
            crate::trajectory::health::reduce(std::slice::from_ref(&start), &BTreeSet::new())?;
        // Opaque historical journal input, never a typed current Policy producer.
        let mut route: TrajectoryEvent = serde_json::from_value(json!({
            "schema_version":TRAJECTORY_SCHEMA_VERSION, "event_id":"old-route",
            "owner_user_id":"owner-1", "episode_id":"episode-1", "request_id":"request-old",
            "sequence":2, "kind":"route_intent_recorded", "captured_at":"2026-08-01T00:00:01Z",
            "content_digest":"", "evidence":{
                "structural":{"route.applied_clause_count":0,"route.selected_is_protected":0,
                    "route.experiment_challenger_propensity_ppm":100000},
                "categorical":{
                    "route.eval_schema":"trajectory.v1", "route.policy":"retired-policy",
                    "route.request_key":"semantic_route/v1|unknown|orchestrate|normal",
                    "route.route_projection":"semantic_route/v1|unknown|orchestrate|normal",
                    "route.baseline_tier":"reference", "route.preset":"auto:cost",
                    "route.projection":"agent_trace/v2|planning|normal", "route.workflow_state":"planning",
                    "route.selected_tier":"economy", "route.experiment_id":format!("sha256:{}", "a".repeat(64)),
                    "route.experiment_arm":"challenger", "route.experiment_assignment_unit":"task",
                    "route.clause_00.id":"progress_guard.active_hold", "route.clause_00.disposition":"skipped",
                    "route.clause_01.id":"progress_guard.incomplete_history", "route.clause_01.disposition":"skipped",
                    "route.clause_02.id":"progress_guard.max_consecutive_unprotected", "route.clause_02.disposition":"skipped",
                    "route.clause_03.id":"progress_guard.max_same_projection_unprotected", "route.clause_03.disposition":"skipped",
                    "route.clause_04.id":"progress_guard.max_recovery_count", "route.clause_04.disposition":"skipped",
                    "route.clause_05.id":"progress_guard.max_episode_requests", "route.clause_05.disposition":"skipped",
                    "route.clause_06.id":"progress_guard.max_episode_elapsed_ms", "route.clause_06.disposition":"skipped",
                    "route.clause_07.id":"progress_guard.max_episode_cost_micro_usd", "route.clause_07.disposition":"skipped",
                    "route.clause_08.id":"tool_safety.floor", "route.clause_08.disposition":"skipped"
                },
                "digests":{"route.policy_lock":format!("sha256:{}", "b".repeat(64)),
                    "route.health_snapshot":snapshot.evidence_digest,
                    "route.experiment_assignment_id":format!("sha256:{}", "c".repeat(64))}
            }
        }))?;
        resign(&mut route)?;
        let events = [
            start,
            route,
            request_settlement(3, "settled-old", "request-old", 100, 15, 70)?,
        ];
        let envelope = build_operational_evaluation(&events)?;
        assert!(envelope.subject.decisions.is_empty());
        assert!(envelope.result.decision_credit.is_empty());
        assert_eq!(envelope.result.metrics["cost.usd_micros"].value, 70);
        assert_eq!(envelope.result.metrics["latency.ms"].value, 100);
        assert_eq!(
            envelope.subject.evidence[0]
                .attributes
                .get("event_count")
                .map(String::as_str),
            Some("3")
        );
        Ok(())
    }

    fn request_start(
        sequence: u64,
        event_id: &str,
        request_id: &str,
        canonical_input_bytes: u64,
    ) -> anyhow::Result<TrajectoryEvent> {
        event_for_request(
            sequence,
            event_id,
            request_id,
            TrajectoryEventKind::RequestStarted,
            BTreeMap::from([(
                "request.canonical_input_bytes".to_owned(),
                canonical_input_bytes,
            )]),
            BTreeMap::from([
                ("history.completeness".to_owned(), "complete".to_owned()),
                ("correlation.source".to_owned(), "explicit_root".to_owned()),
            ]),
            BTreeMap::new(),
        )
    }

    fn request_settlement(
        sequence: u64,
        event_id: &str,
        request_id: &str,
        duration_ms: u64,
        total_tokens: u64,
        cost_micro_usd: u64,
    ) -> anyhow::Result<TrajectoryEvent> {
        event_for_request(
            sequence,
            event_id,
            request_id,
            TrajectoryEventKind::RequestSettled,
            BTreeMap::from([
                ("settlement.duration_ms".to_owned(), duration_ms),
                ("settlement.total_tokens".to_owned(), total_tokens),
                ("settlement.cost_micro_usd".to_owned(), cost_micro_usd),
            ]),
            BTreeMap::from([
                ("settlement.outcome".to_owned(), "settled".to_owned()),
                ("settlement.provider".to_owned(), "provider".to_owned()),
                ("settlement.model".to_owned(), "model".to_owned()),
            ]),
            BTreeMap::new(),
        )
    }

    fn complete_events(
        total_tokens: Option<u64>,
        cost_micro_usd: Option<u64>,
    ) -> anyhow::Result<Vec<TrajectoryEvent>> {
        let start = request_start(1, "start-1", "request-1", 20)?;
        let mut structural = BTreeMap::from([("settlement.duration_ms".to_owned(), 100)]);
        if let Some(total_tokens) = total_tokens {
            structural.insert("settlement.total_tokens".to_owned(), total_tokens);
        }
        if let Some(cost_micro_usd) = cost_micro_usd {
            structural.insert("settlement.cost_micro_usd".to_owned(), cost_micro_usd);
        }
        let settlement = event(
            2,
            "settlement-1",
            TrajectoryEventKind::RequestSettled,
            structural,
            BTreeMap::from([
                ("settlement.outcome".to_owned(), "settled".to_owned()),
                ("settlement.provider".to_owned(), "provider".to_owned()),
                ("settlement.model".to_owned(), "model".to_owned()),
            ]),
            BTreeMap::new(),
        )?;
        Ok(vec![start, settlement])
    }

    fn event(
        sequence: u64,
        event_id: &str,
        kind: TrajectoryEventKind,
        structural: BTreeMap<String, u64>,
        categorical: BTreeMap<String, String>,
        digests: BTreeMap<String, String>,
    ) -> anyhow::Result<TrajectoryEvent> {
        event_for_request(
            sequence,
            event_id,
            "request-1",
            kind,
            structural,
            categorical,
            digests,
        )
    }

    fn event_for_request(
        sequence: u64,
        event_id: &str,
        request_id: &str,
        kind: TrajectoryEventKind,
        structural: BTreeMap<String, u64>,
        categorical: BTreeMap<String, String>,
        digests: BTreeMap<String, String>,
    ) -> anyhow::Result<TrajectoryEvent> {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: event_id.to_owned(),
            owner_user_id: "owner-1".to_owned(),
            episode_id: "episode-1".to_owned(),
            request_id: Some(request_id.to_owned()),
            sequence,
            kind,
            evidence: TrajectoryEvidence {
                routing_evidence: Vec::new(),
                structural,
                categorical,
                digests,
                route_measurement: None,
            },
            captured_at: format!("2026-08-01T00:00:{:02}Z", sequence - 1),
            content_digest: String::new(),
        };
        resign(&mut event)?;
        Ok(event)
    }

    fn resign(event: &mut TrajectoryEvent) -> anyhow::Result<()> {
        event.content_digest.clear();
        event.content_digest = event.semantic_digest()?;
        Ok(())
    }
}
