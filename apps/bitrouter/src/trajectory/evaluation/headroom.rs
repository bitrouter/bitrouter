//! Serialization-only upper-bound shape; these values never become evidence.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{
    DecisionCredit, EVAL_SCHEMA_VERSION, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, EvidenceItem, MetricUnit, MetricValue,
};
use chrono::{DateTime, SecondsFormat, Utc};

use crate::trajectory::evaluation::{
    TRAJECTORY_EVALUATOR_AUTHORITY_ID, TRAJECTORY_EVALUATOR_ID, TRAJECTORY_EVALUATOR_VERSION,
    TrajectoryEvaluationEnvelope, decode_eval_decision, trajectory_evaluator_config_digest,
};
use crate::trajectory::types::{OutboxPayload, TrajectoryEvent, TrajectoryEventKind};

pub(crate) fn sizing_payload(events: &[TrajectoryEvent]) -> Result<OutboxPayload> {
    let episode = &events
        .first()
        .context("trajectory output history missing")?
        .episode_id;
    let digest = format!("sha256:{}", "0".repeat(64));
    let mut decisions = Vec::new();
    let mut routing = BTreeMap::new();
    let mut preset: Option<String> = None;
    let mut preset_bytes = serde_json::to_vec(&preset)?.len();
    let mut credit_id = String::new();
    let mut credit_bytes = 0usize;
    let mut policy_digest = digest.clone();
    let mut policy_bytes = serde_json::to_vec(&policy_digest)?.len();
    for event in events {
        if event.kind == TrajectoryEventKind::RouteIntentRecorded {
            if let Some(decision) = decode_eval_decision(event)? {
                let bytes = serde_json::to_vec(&decision.policy_digest)?.len();
                if bytes > policy_bytes {
                    policy_bytes = bytes;
                    policy_digest.clone_from(&decision.policy_digest);
                }
                let bytes = serde_json::to_vec(&decision.decision_id)?.len();
                if bytes > credit_bytes {
                    credit_bytes = bytes;
                    credit_id.clone_from(&decision.decision_id);
                }
                decisions.push(decision);
                let candidate = event.evidence.categorical.get("route.preset").cloned();
                let bytes = serde_json::to_vec(&candidate)?.len();
                if bytes > preset_bytes {
                    preset_bytes = bytes;
                    preset = candidate;
                }
            } else {
                ensure!(
                    !event.evidence.digests.contains_key("route.policy_lock"),
                    "guarded route lost replayable evaluation identity"
                );
            }
        }
        for item in &event.evidence.routing_evidence {
            if let Some(previous) = routing.insert(item.evidence_id.clone(), item.clone()) {
                ensure!(
                    previous == *item,
                    "routing receipt identity has conflicting evidence"
                );
            }
        }
    }
    ensure!(
        !decisions.is_empty(),
        "trajectory output has no replayable decision"
    );
    let number = u64::MAX.to_string();
    let item = |id: &str, kind: &str, attributes| EvidenceItem {
        evidence_id: id.into(),
        kind: kind.into(),
        digest: digest.clone(),
        redacted: true,
        attributes,
    };
    let mut evidence = vec![
        item(
            "episode-events",
            "trajectory.event_digests",
            BTreeMap::from([
                ("event_count".into(), number.clone()),
                ("through_sequence".into(), number.clone()),
            ]),
        ),
        item(
            "trajectory-snapshot",
            "trajectory.snapshot",
            BTreeMap::from([
                ("history_completeness".into(), "incomplete".into()),
                ("request_count".into(), number.clone()),
                ("settled_request_count".into(), number.clone()),
            ]),
        ),
        item(
            "routing-prediction-observation",
            "routing.prediction_observation",
            BTreeMap::new(),
        ),
    ];
    evidence.extend(routing.into_values());
    let evidence_refs = evidence
        .iter()
        .map(|item| item.evidence_id.clone())
        .collect();

    // This is the complete finite set from operational_metrics; maximum
    // nonnegative i64 widths cover every successfully validated metric value.
    // No usage, cost or timing is being inferred or emitted by this planner.
    let metrics = BTreeMap::from([
        metric("trajectory.request_count", MetricUnit::Count),
        metric("trajectory.settled_request_count", MetricUnit::Count),
        metric("trajectory.elapsed_ms", MetricUnit::Milliseconds),
        metric("trajectory.same_projection_streak", MetricUnit::Count),
        metric("trajectory.same_selected_tier_streak", MetricUnit::Count),
        metric("trajectory.unprotected_streak", MetricUnit::Count),
        metric("trajectory.recovery_count", MetricUnit::Count),
        metric("trajectory.context_growth_ppm", MetricUnit::ScalarMicros),
        metric("trajectory.total_tokens", MetricUnit::Count),
        metric("trajectory.cost.usd_micros", MetricUnit::MicroUsd),
        (
            "trajectory.history_complete".into(),
            MetricValue::new(1, MetricUnit::Boolean),
        ),
        metric("cost.usd_micros", MetricUnit::MicroUsd),
        metric("latency.ms", MetricUnit::Milliseconds),
    ]);
    // The operational projection normalizes its timestamp to UTC/AutoSi. Nanos at the
    // two supported year extrema bounds every such normalized ASCII string.
    // Raw arbitrary-length RFC3339 input is NOT the producer contract here.
    let timestamp_bytes = [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC]
        .iter()
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Nanos, true).len())
        .max()
        .context("shared timestamp sizing missing")?;
    let timestamp = "0".repeat(timestamp_bytes);
    let eval_id = format!("trajectory:{episode}:{}", u64::MAX);
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: eval_id.clone(),
        scope: EvalScope::Episode,
        subject_id: episode.clone(),
        policy_digest,
        preset,
        cohort: None,
        holdout: false,
        decisions,
        requested_dimensions: metrics.keys().cloned().collect(),
        evidence,
        evidence_digest: digest.clone(),
        observed_at: timestamp.clone(),
    };
    let result = EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id,
        evidence_digest: digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: TRAJECTORY_EVALUATOR_AUTHORITY_ID.into(),
            evaluator_id: TRAJECTORY_EVALUATOR_ID.into(),
            kind: EvaluatorKind::Generic,
            version: TRAJECTORY_EVALUATOR_VERSION.into(),
            config_digest: trajectory_evaluator_config_digest()?,
        },
        verdict: EvalVerdict::Inconclusive,
        metrics,
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs,
        decision_credit: BTreeMap::from([(
            credit_id,
            DecisionCredit {
                weight_ppm: 1_000_000,
                metric_ids: BTreeSet::from(["cost.usd_micros".into(), "latency.ms".into()]),
            },
        )]),
        idempotency_key: format!("trajectory-operational:{episode}:{}", u64::MAX),
        submitted_at: timestamp,
    };
    Ok(OutboxPayload {
        structural: BTreeMap::from([("trajectory.through_sequence".into(), u64::MAX)]),
        digests: BTreeMap::from([("trajectory.settlement_event".into(), digest)]),
        evaluation: Some(Box::new(TrajectoryEvaluationEnvelope { subject, result })),
    })
}

fn metric(id: &str, unit: MetricUnit) -> (String, MetricValue) {
    (id.into(), MetricValue::new(i64::MAX, unit))
}
