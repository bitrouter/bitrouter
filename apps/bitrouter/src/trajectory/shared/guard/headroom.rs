//! Admission-only serialization bounds for the required operational outbox.
//! Sizing values never become facts: this module returns only a byte count.
//!
//! Keep this accounting aligned with trajectory/evaluation.rs. In particular,
//! every pending settlement contributes routing receipts, while only the
//! currently evaluated settlement contributes prediction attributes.

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

use super::super::{MAX_GUARDED_SETTLEMENT_BYTES, MAX_OUTBOX_BYTES};

pub(super) fn reserve(events: &[TrajectoryEvent], unsettled: u64) -> Result<()> {
    // The complete outbox contains its evaluation as a JSON object, not an
    // escaped string. Its 1 MiB limit therefore also covers the current shared
    // evaluation sink's 1 MiB limit (storage/evaluation/shared.rs).
    ensure!(
        upper_bound(events, unsettled)? <= MAX_OUTBOX_BYTES,
        "shared trajectory lacks required evaluation outbox headroom"
    );
    Ok(())
}

pub(super) fn upper_bound(events: &[TrajectoryEvent], unsettled: u64) -> Result<usize> {
    ensure!(
        unsettled > 0,
        "shared output reserve requires unsettled work"
    );
    let payload = sizing_payload(events)?;
    let known_and_fixed = serde_json::to_vec(&payload)?.len();

    // A valid event contains <=2 routing receipts, each with an ID <=512
    // bytes. General JSON string escaping costs <=6 bytes per input byte,
    // plus quotes and the separator when appended to evidence_refs.
    // https://www.rfc-editor.org/rfc/rfc8259#section-7
    let receipt_references = 2usize * (512 * 6 + 2 + 1);
    let per_pending = MAX_GUARDED_SETTLEMENT_BYTES
        .checked_add(receipt_references)
        .context("shared output reserve overflow")?;
    let future = usize::try_from(unsettled)?
        .checked_mul(per_pending)
        .context("shared output reserve overflow")?;

    // Future receipts are copied once. The current prediction item projects
    // disjoint categorical/structural fields from that same event: its keys
    // lose the routing. prefix, strings retain their JSON encoding, and at
    // most 64 u64 values gain two quotes. Their source bytes plus receipt
    // bytes fit in the one full-event allowance; do not charge them twice.
    // The empty prediction item and reference are already in sizing_payload.
    // Two extra separators cover joining the copied map/array fragments.
    known_and_fixed
        .checked_add(future)
        .and_then(|bytes| bytes.checked_add(2 * 64 + 2))
        .context("shared output reserve overflow")
}

fn sizing_payload(events: &[TrajectoryEvent]) -> Result<OutboxPayload> {
    let episode = &events
        .first()
        .context("shared output history missing")?
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
                    "shared guarded route lost replayable evaluation identity"
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
        "shared output has no replayable decision"
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
    // Shared projection normalizes its timestamp to UTC/AutoSi. Nanos at the
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
