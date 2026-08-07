use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::eval::types::{
    DecisionCredit, EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict,
    EvaluationResult, EvaluatorIdentity, EvaluatorKind, EvidenceItem, MetricUnit, MetricValue,
    canonical_digest, evidence_digest, validate_result_for_subject,
};

use super::health::reduce;
use super::types::{HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind};

pub(crate) const TRAJECTORY_EVAL_TOPIC: &str = "eval.trajectory-operational.v1";
const ROUTE_EVAL_SCHEMA: &str = "trajectory.v1";
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

pub(crate) fn build_operational_evaluation(
    events: &[TrajectoryEvent],
) -> Result<TrajectoryEvaluationEnvelope> {
    let settlement = events
        .last()
        .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
        .ok_or_else(|| anyhow::anyhow!("trajectory evaluation requires a terminal settlement"))?;
    let snapshot = reduce(events, &BTreeSet::new())?;
    let mut decoded = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
    {
        match decode_eval_decision(event)? {
            Some(decision) => decoded.push((event, decision)),
            None if event.evidence.digests.contains_key("route.policy_lock") => {
                anyhow::bail!(
                    "guarded route '{}' predates replayable evaluation identity",
                    event.event_id
                )
            }
            None => {}
        }
    }
    let (current_route, current_decision) = decoded
        .iter()
        .rev()
        .find(|(event, _)| event.request_id == settlement.request_id)
        .ok_or_else(|| anyhow::anyhow!("settlement has no matching replayable route intent"))?;

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
    let evidence_digest = evidence_digest(&evidence)?;
    let evidence_refs = evidence
        .iter()
        .map(|item| item.evidence_id.clone())
        .collect();
    let decisions = decoded
        .iter()
        .map(|(_, decision)| decision.clone())
        .collect::<Vec<_>>();
    let eval_id = format!(
        "trajectory:{}:{}",
        snapshot.episode_id, snapshot.through_sequence
    );
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: eval_id.clone(),
        scope: EvalScope::Episode,
        subject_id: snapshot.episode_id.clone(),
        policy_digest: current_decision.policy_digest.clone(),
        preset: current_route
            .evidence
            .categorical
            .get("route.preset")
            .cloned(),
        cohort: None,
        holdout: false,
        decisions,
        requested_dimensions,
        evidence,
        evidence_digest: evidence_digest.clone(),
        observed_at: settlement.captured_at.clone(),
    };
    let result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
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
        decision_credit: decision_credit(&decoded, settlement, &metrics),
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
        ("routing.predicted_action", "predicted_action"),
        ("routing.observed_action", "observed_action"),
        ("routing.action_match", "action_match"),
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

fn decode_eval_decision(event: &TrajectoryEvent) -> Result<Option<EvalDecisionRef>> {
    let Some(schema) = event.evidence.categorical.get("route.eval_schema") else {
        return Ok(None);
    };
    if schema != ROUTE_EVAL_SCHEMA {
        anyhow::bail!("route intent has unsupported route.eval_schema '{schema}'")
    }
    let required = |key: &str| {
        event
            .evidence
            .categorical
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("trajectory evaluation route is missing {key}"))
    };
    let policy_digest = event
        .evidence
        .digests
        .get("route.policy_lock")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("trajectory evaluation route is missing policy digest"))?;
    Ok(Some(EvalDecisionRef {
        decision_id: event.event_id.clone(),
        policy: required("route.policy")?,
        request_key: required("route.request_key")?,
        selected_tier: required("route.selected_tier")?,
        baseline_tier: event
            .evidence
            .categorical
            .get("route.baseline_tier")
            .cloned(),
        policy_digest,
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

fn decision_credit(
    decoded: &[(&TrajectoryEvent, EvalDecisionRef)],
    current_settlement: &TrajectoryEvent,
    metrics: &BTreeMap<String, MetricValue>,
) -> BTreeMap<String, DecisionCredit> {
    let Some((_, decision)) = decoded
        .iter()
        .rev()
        .find(|(event, _)| event.request_id == current_settlement.request_id)
    else {
        return BTreeMap::new();
    };
    let metric_ids = ["cost.usd_micros", "latency.ms"]
        .into_iter()
        .filter(|metric| metrics.contains_key(*metric))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if metric_ids.is_empty() {
        return BTreeMap::new();
    }
    BTreeMap::from([(
        decision.decision_id.clone(),
        DecisionCredit {
            weight_ppm: 1_000_000,
            metric_ids,
        },
    )])
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

    use super::{
        build_operational_evaluation, decision_credit, decode_eval_decision, operational_metrics,
    };
    use crate::eval::types::{EvalScope, EvalVerdict, MetricUnit};
    use crate::trajectory::health::reduce;
    use crate::trajectory::types::{
        HistoryCompleteness, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind,
        TrajectoryEvidence, TrajectoryHealth,
    };

    const POLICY_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn episode_evaluation_is_operational_inconclusive_and_replayable() -> anyhow::Result<()> {
        let events = complete_events(Some(15), Some(70))?;
        let envelope = build_operational_evaluation(&events)?;

        assert_eq!(envelope.subject.scope, EvalScope::Episode);
        assert_eq!(envelope.subject.subject_id, "episode-1");
        assert_eq!(envelope.subject.eval_id, "trajectory:episode-1:3");
        assert_eq!(envelope.subject.policy_digest, POLICY_DIGEST);
        assert_eq!(envelope.subject.preset.as_deref(), Some("auto:cost"));
        assert_eq!(envelope.subject.decisions.len(), 1);
        assert_eq!(envelope.subject.decisions[0].decision_id, "route-1");
        assert_eq!(envelope.subject.decisions[0].policy, "auto:cost");
        assert_eq!(
            envelope.subject.decisions[0].request_key,
            "agent_trace/v2|planning|normal"
        );
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
        assert_eq!(envelope.subject.observed_at, "2026-08-01T00:00:02Z");
        assert_eq!(envelope.result.submitted_at, envelope.subject.observed_at);
        assert!(envelope.result.decision_credit.keys().all(|id| {
            envelope
                .subject
                .decisions
                .iter()
                .any(|item| &item.decision_id == id)
        }));
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
    fn evaluation_identity_marker_never_guesses_missing_policy() -> anyhow::Result<()> {
        let mut events = complete_events(Some(15), Some(70))?;
        events[1].evidence.categorical.remove("route.policy");
        resign(&mut events[1])?;
        let error = build_operational_evaluation(&events).expect_err("missing policy must fail");
        assert!(error.to_string().contains("route.policy"));
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
            &events[2],
        )?;
        let growth = metrics
            .get("trajectory.context_growth_ppm")
            .ok_or_else(|| anyhow::anyhow!("growth metric missing"))?;
        assert_eq!(growth.value, 1_500_000);
        assert_eq!(growth.unit, MetricUnit::ScalarMicros);
        Ok(())
    }

    #[test]
    fn decision_credit_only_names_the_current_route_after_interleaving_and_recovery_reset()
    -> anyhow::Result<()> {
        let events = complete_events(Some(15), Some(70))?;
        let mut first = events[1].clone();
        first.request_id = Some("request-1".into());
        let mut interleaved = first.clone();
        interleaved.event_id = "route-2-unsettled".into();
        interleaved.request_id = Some("request-2".into());
        interleaved
            .evidence
            .categorical
            .insert("route.workflow_state".into(), "recovery".into());
        resign(&mut interleaved)?;
        let mut current = first.clone();
        current.event_id = "route-3-current".into();
        current.request_id = Some("request-4-unsettled".into());
        current
            .evidence
            .categorical
            .insert("route.workflow_state".into(), "editing".into());
        resign(&mut current)?;
        let mut settlement = events[2].clone();
        settlement.request_id = Some("request-3".into());
        resign(&mut settlement)?;
        first.request_id = Some("request-3".into());
        resign(&mut first)?;
        let decoded = [&first, &interleaved, &current]
            .into_iter()
            .map(|event| {
                decode_eval_decision(event)?
                    .map(|decision| (event, decision))
                    .ok_or_else(|| anyhow::anyhow!("typed route did not decode"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let metrics = operational_metrics(
            &TrajectoryHealth {
                completeness: HistoryCompleteness::Complete,
                request_count: 3,
                settled_request_count: 2,
                unsettled_request_count: 1,
                elapsed_ms: 10,
                latest_projection: None,
                same_projection_streak: 1,
                same_selected_tier_streak: 1,
                consecutive_unprotected_requests: 1,
                recovery_count: 1,
                requests_since_recovery: Some(1),
                context_growth_ppm: None,
                total_tokens: Some(15),
                settled_cost_micro_usd: Some(70),
            },
            &settlement,
        )?;

        let credits = decision_credit(&decoded, &settlement, &metrics);
        assert_eq!(credits.keys().cloned().collect::<Vec<_>>(), ["route-1"]);
        assert!(
            !credits["route-1"]
                .metric_ids
                .contains("trajectory.recovery_count")
        );
        assert!(
            !credits["route-1"]
                .metric_ids
                .contains("trajectory.total_tokens")
        );
        assert!(
            !credits["route-1"]
                .metric_ids
                .contains("trajectory.cost.usd_micros")
        );
        Ok(())
    }

    #[test]
    fn interleaved_settlements_credit_only_their_request_local_metrics() -> anyhow::Result<()> {
        let events = interleaved_events()?;

        let settled_a = build_operational_evaluation(&events[..5])?;
        assert_eq!(
            settled_a.result.decision_credit.keys().collect::<Vec<_>>(),
            ["route-a"]
        );
        assert_eq!(
            settled_a.result.decision_credit["route-a"].metric_ids,
            BTreeSet::from(["cost.usd_micros".into(), "latency.ms".into()])
        );
        assert_eq!(settled_a.result.metrics["cost.usd_micros"].value, 70);
        assert_eq!(settled_a.result.metrics["latency.ms"].value, 100);

        let settled_b = build_operational_evaluation(&events)?;
        assert_eq!(
            settled_b.result.decision_credit.keys().collect::<Vec<_>>(),
            ["route-b"]
        );
        assert_eq!(
            settled_b.result.decision_credit["route-b"].metric_ids,
            BTreeSet::from(["cost.usd_micros".into(), "latency.ms".into()])
        );
        assert_eq!(settled_b.result.metrics["cost.usd_micros"].value, 90);
        assert_eq!(settled_b.result.metrics["latency.ms"].value, 200);
        Ok(())
    }

    fn interleaved_events() -> anyhow::Result<Vec<TrajectoryEvent>> {
        let start_a = request_start(1, "start-a", "request-a", 20)?;
        let route_a = typed_route(
            std::slice::from_ref(&start_a),
            2,
            "route-a",
            "request-a",
            "agent_trace/v2|planning|normal",
            "planning",
            ("economy", false),
        )?;
        let start_b = request_start(3, "start-b", "request-b", 40)?;
        let prefix_b = vec![start_a.clone(), route_a.clone(), start_b.clone()];
        let route_b = typed_route(
            &prefix_b,
            4,
            "route-b",
            "request-b",
            "agent_trace/v2|recovery|guarded",
            "recovery",
            ("strong", true),
        )?;
        let settlement_a = request_settlement(5, "settlement-a", "request-a", 100, 15, 70)?;
        let settlement_b = request_settlement(6, "settlement-b", "request-b", 200, 20, 90)?;
        Ok(vec![
            start_a,
            route_a,
            start_b,
            route_b,
            settlement_a,
            settlement_b,
        ])
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

    fn typed_route(
        prefix: &[TrajectoryEvent],
        sequence: u64,
        event_id: &str,
        request_id: &str,
        projection: &str,
        workflow_state: &str,
        selection: (&str, bool),
    ) -> anyhow::Result<TrajectoryEvent> {
        let (selected_tier, selected_is_protected) = selection;
        let snapshot = reduce(prefix, &BTreeSet::new())?;
        let mut route = event_for_request(
            sequence,
            event_id,
            request_id,
            TrajectoryEventKind::RouteIntentRecorded,
            BTreeMap::from([
                ("route.applied_clause_count".to_owned(), 0),
                (
                    "route.selected_is_protected".to_owned(),
                    u64::from(selected_is_protected),
                ),
            ]),
            BTreeMap::from([
                ("route.eval_schema".to_owned(), "trajectory.v1".to_owned()),
                ("route.policy".to_owned(), "auto:cost".to_owned()),
                ("route.request_key".to_owned(), projection.to_owned()),
                ("route.baseline_tier".to_owned(), "reference".to_owned()),
                ("route.preset".to_owned(), "auto:cost".to_owned()),
                ("route.projection".to_owned(), projection.to_owned()),
                ("route.workflow_state".to_owned(), workflow_state.to_owned()),
                ("route.selected_tier".to_owned(), selected_tier.to_owned()),
            ]),
            BTreeMap::from([
                ("route.policy_lock".to_owned(), POLICY_DIGEST.to_owned()),
                ("route.health_snapshot".to_owned(), snapshot.evidence_digest),
            ]),
        )?;
        for (index, clause_id) in [
            "progress_guard.active_hold",
            "progress_guard.incomplete_history",
            "progress_guard.max_consecutive_unprotected",
            "progress_guard.max_same_projection_unprotected",
            "progress_guard.max_recovery_count",
            "progress_guard.max_episode_requests",
            "progress_guard.max_episode_elapsed_ms",
            "progress_guard.max_episode_cost_micro_usd",
            "tool_safety.floor",
        ]
        .into_iter()
        .enumerate()
        {
            let clause = format!("route.clause_{index:02}");
            route
                .evidence
                .categorical
                .insert(format!("{clause}.id"), clause_id.to_owned());
            route
                .evidence
                .categorical
                .insert(format!("{clause}.disposition"), "skipped".to_owned());
        }
        resign(&mut route)?;
        Ok(route)
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
        let start = event(
            1,
            "start-1",
            TrajectoryEventKind::RequestStarted,
            BTreeMap::from([("request.canonical_input_bytes".to_owned(), 20)]),
            BTreeMap::from([
                ("history.completeness".to_owned(), "complete".to_owned()),
                ("correlation.source".to_owned(), "explicit_root".to_owned()),
            ]),
            BTreeMap::new(),
        )?;
        let snapshot = reduce(std::slice::from_ref(&start), &BTreeSet::new())?;
        let mut route = event(
            2,
            "route-1",
            TrajectoryEventKind::RouteIntentRecorded,
            BTreeMap::from([
                ("route.applied_clause_count".to_owned(), 0),
                ("route.selected_is_protected".to_owned(), 0),
            ]),
            BTreeMap::from([
                ("route.eval_schema".to_owned(), "trajectory.v1".to_owned()),
                ("route.policy".to_owned(), "auto:cost".to_owned()),
                (
                    "route.request_key".to_owned(),
                    "agent_trace/v2|planning|normal".to_owned(),
                ),
                ("route.baseline_tier".to_owned(), "reference".to_owned()),
                ("route.preset".to_owned(), "auto:cost".to_owned()),
                (
                    "route.projection".to_owned(),
                    "agent_trace/v2|planning|normal".to_owned(),
                ),
                ("route.workflow_state".to_owned(), "planning".to_owned()),
                ("route.selected_tier".to_owned(), "economy".to_owned()),
            ]),
            BTreeMap::from([
                ("route.policy_lock".to_owned(), POLICY_DIGEST.to_owned()),
                ("route.health_snapshot".to_owned(), snapshot.evidence_digest),
            ]),
        )?;
        for (index, clause_id) in [
            "progress_guard.active_hold",
            "progress_guard.incomplete_history",
            "progress_guard.max_consecutive_unprotected",
            "progress_guard.max_same_projection_unprotected",
            "progress_guard.max_recovery_count",
            "progress_guard.max_episode_requests",
            "progress_guard.max_episode_elapsed_ms",
            "progress_guard.max_episode_cost_micro_usd",
            "tool_safety.floor",
        ]
        .into_iter()
        .enumerate()
        {
            let prefix = format!("route.clause_{index:02}");
            route
                .evidence
                .categorical
                .insert(format!("{prefix}.id"), clause_id.to_owned());
            route
                .evidence
                .categorical
                .insert(format!("{prefix}.disposition"), "skipped".to_owned());
        }
        resign(&mut route)?;
        let mut structural = BTreeMap::from([("settlement.duration_ms".to_owned(), 100)]);
        if let Some(total_tokens) = total_tokens {
            structural.insert("settlement.total_tokens".to_owned(), total_tokens);
        }
        if let Some(cost_micro_usd) = cost_micro_usd {
            structural.insert("settlement.cost_micro_usd".to_owned(), cost_micro_usd);
        }
        let settlement = event(
            3,
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
        Ok(vec![start, route, settlement])
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
                structural,
                categorical,
                digests,
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
