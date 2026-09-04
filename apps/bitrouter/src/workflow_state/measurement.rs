//! Redacted, content-blind controls for routing measurement.

use std::collections::BTreeMap;

use bitrouter_sdk::language_model::types::ReasoningEffort;
use serde::{Deserialize, Serialize};

use crate::eval::types::{
    RouteActionCandidate, RouteDecisionMeasurement, canonical_digest,
    validate_route_measurement_for_experiment,
};

use super::decision::{PolicyDecisionRecord, ingress_request_id_sha256};

pub const ROUTING_BASELINE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingBaselineReport {
    pub schema_version: u32,
    pub dataset_digest: String,
    pub total_decision_count: usize,
    pub eligible_decision_count: usize,
    pub excluded_by_reason: BTreeMap<String, usize>,
    pub groups: Vec<RoutingBaselineGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingBaselineGroup {
    pub candidate_set_digest: String,
    pub decision_count: usize,
    pub observed_selected_tier_counts: BTreeMap<String, usize>,
    pub always_tier: Vec<RoutingBaseline>,
    pub share_matched: RoutingBaseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingBaselineKind {
    AlwaysTier,
    ShareMatched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingBaseline {
    pub baseline_id: String,
    pub kind: RoutingBaselineKind,
    pub assignments: Vec<RoutingBaselineAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingBaselineAssignment {
    pub decision_id_digest: String,
    pub tier: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
}

#[derive(Clone, Serialize)]
struct EligibleDecision {
    decision_id_digest: String,
    selected_tier: String,
    measurement: RouteDecisionMeasurement,
}

impl RoutingBaselineReport {
    pub fn from_records(records: &[PolicyDecisionRecord]) -> anyhow::Result<Self> {
        let identity_counts = records.iter().filter_map(decision_id_digest).fold(
            BTreeMap::<String, usize>::new(),
            |mut counts, digest| {
                *counts.entry(digest).or_default() += 1;
                counts
            },
        );
        let mut excluded_by_reason = BTreeMap::new();
        let mut eligible = Vec::new();
        for record in records {
            let Some(decision_id_digest) = decision_id_digest(record) else {
                increment(&mut excluded_by_reason, "missing_decision_identity");
                continue;
            };
            if identity_counts.get(&decision_id_digest).copied() != Some(1) {
                increment(&mut excluded_by_reason, "duplicate_decision_identity");
                continue;
            }
            let Some(measurement) = &record.route_measurement else {
                increment(&mut excluded_by_reason, "missing_route_measurement");
                continue;
            };
            if validate_route_measurement_for_experiment(measurement, record.experiment.as_ref())
                .is_err()
            {
                increment(&mut excluded_by_reason, "invalid_route_measurement");
                continue;
            }
            let Some(selected_tier) = &record.selected_tier else {
                increment(&mut excluded_by_reason, "missing_selected_tier");
                continue;
            };
            let targets_by_tier = measurement
                .candidates
                .iter()
                .map(|candidate| (candidate.tier.as_str(), candidate.model.as_str()))
                .collect::<BTreeMap<_, _>>();
            if targets_by_tier.len() != measurement.candidates.len()
                || targets_by_tier.get(selected_tier.as_str()).copied()
                    != record.selected_model.as_deref()
            {
                increment(&mut excluded_by_reason, "selected_target_mismatch");
                continue;
            }
            eligible.push(EligibleDecision {
                decision_id_digest,
                selected_tier: selected_tier.clone(),
                measurement: measurement.clone(),
            });
        }
        eligible.sort_by(|left, right| left.decision_id_digest.cmp(&right.decision_id_digest));
        let dataset_digest =
            canonical_digest(&("bitrouter.routing-baseline-dataset.v1", &eligible))?;
        let mut grouped = BTreeMap::<String, Vec<EligibleDecision>>::new();
        for decision in eligible {
            grouped
                .entry(decision.measurement.candidate_set_digest.clone())
                .or_default()
                .push(decision);
        }
        let mut groups = Vec::with_capacity(grouped.len());
        for (candidate_set_digest, decisions) in grouped {
            groups.push(build_group(
                &dataset_digest,
                candidate_set_digest,
                decisions,
            )?);
        }
        let eligible_decision_count = groups.iter().map(|group| group.decision_count).sum();
        Ok(Self {
            schema_version: ROUTING_BASELINE_SCHEMA_VERSION,
            dataset_digest,
            total_decision_count: records.len(),
            eligible_decision_count,
            excluded_by_reason,
            groups,
        })
    }
}

fn build_group(
    dataset_digest: &str,
    candidate_set_digest: String,
    decisions: Vec<EligibleDecision>,
) -> anyhow::Result<RoutingBaselineGroup> {
    let assignment_population_digest = canonical_digest(&(
        "bitrouter.routing-share-population.v1",
        candidate_set_digest.as_str(),
        decisions
            .iter()
            .map(|decision| decision.decision_id_digest.as_str())
            .collect::<Vec<_>>(),
    ))?;
    let targets = decisions
        .first()
        .map(|decision| decision.measurement.candidates.clone())
        .unwrap_or_default();
    let observed_selected_tier_counts =
        decisions
            .iter()
            .fold(BTreeMap::<String, usize>::new(), |mut counts, decision| {
                *counts.entry(decision.selected_tier.clone()).or_default() += 1;
                counts
            });
    let mut always_tier = Vec::with_capacity(targets.len());
    for target in &targets {
        let assignments = decisions
            .iter()
            .map(|decision| assignment(&decision.decision_id_digest, target))
            .collect();
        always_tier.push(RoutingBaseline {
            baseline_id: canonical_digest(&(
                "bitrouter.routing-baseline.v1",
                RoutingBaselineKind::AlwaysTier,
                dataset_digest,
                candidate_set_digest.as_str(),
                target,
            ))?,
            kind: RoutingBaselineKind::AlwaysTier,
            assignments,
        });
    }

    let targets_by_tier = targets
        .iter()
        .map(|target| (target.tier.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let mut quota = Vec::with_capacity(decisions.len());
    for (tier, count) in &observed_selected_tier_counts {
        if let Some(target) = targets_by_tier.get(tier.as_str()) {
            quota.extend(std::iter::repeat_n(*target, *count));
        }
    }
    let mut shuffled = decisions
        .iter()
        .map(|decision| {
            Ok((
                canonical_digest(&(
                    "bitrouter.routing-share-order.v1",
                    assignment_population_digest.as_str(),
                    candidate_set_digest.as_str(),
                    decision.decision_id_digest.as_str(),
                ))?,
                decision,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    shuffled.sort_by(|left, right| left.0.cmp(&right.0));
    let mut assignments = shuffled
        .into_iter()
        .zip(quota)
        .map(|((_, decision), target)| assignment(&decision.decision_id_digest, target))
        .collect::<Vec<_>>();
    assignments.sort_by(|left, right| left.decision_id_digest.cmp(&right.decision_id_digest));
    let share_matched = RoutingBaseline {
        baseline_id: canonical_digest(&(
            "bitrouter.routing-baseline.v1",
            RoutingBaselineKind::ShareMatched,
            dataset_digest,
            candidate_set_digest.as_str(),
            &observed_selected_tier_counts,
        ))?,
        kind: RoutingBaselineKind::ShareMatched,
        assignments,
    };
    Ok(RoutingBaselineGroup {
        candidate_set_digest,
        decision_count: decisions.len(),
        observed_selected_tier_counts,
        always_tier,
        share_matched,
    })
}

fn assignment(
    decision_id_digest: &str,
    target: &RouteActionCandidate,
) -> RoutingBaselineAssignment {
    RoutingBaselineAssignment {
        decision_id_digest: decision_id_digest.to_owned(),
        tier: target.tier.clone(),
        model: target.model.clone(),
        effort: target.effort,
    }
}

fn decision_id_digest(record: &PolicyDecisionRecord) -> Option<String> {
    record
        .ingress_request_id_sha256
        .as_deref()
        .filter(|digest| is_sha256_digest(digest))
        .map(ToOwned::to_owned)
        .or_else(|| {
            record
                .request_id
                .as_deref()
                .filter(|request_id| !request_id.is_empty())
                .map(ingress_request_id_sha256)
        })
}

fn is_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn increment(counts: &mut BTreeMap<String, usize>, reason: &str) {
    *counts.entry(reason.to_owned()).or_default() += 1;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn record(
        request_id: Option<&str>,
        selected_tier: &str,
        model_suffix: &str,
    ) -> PolicyDecisionRecord {
        let candidates = vec![
            RouteActionCandidate {
                tier: "economy".into(),
                model: format!("vendor/economy{model_suffix}"),
                effort: None,
                logging_probability_ppm: if selected_tier == "economy" {
                    1_000_000
                } else {
                    0
                },
            },
            RouteActionCandidate {
                tier: "strong".into(),
                model: format!("vendor/strong{model_suffix}"),
                effort: None,
                logging_probability_ppm: if selected_tier == "strong" {
                    1_000_000
                } else {
                    0
                },
            },
        ];
        let measurement = RouteDecisionMeasurement::new(
            selected_tier,
            format!("vendor/{selected_tier}{model_suffix}"),
            None,
            candidates,
        )
        .unwrap();
        serde_json::from_value(serde_json::json!({
            "request_id": request_id,
            "input_model": "inbound",
            "key_strategy": "agent_trace",
            "request_key": "agent_route/v1|unknown|implement|normal",
            "legacy_fingerprint": "opening",
            "trace_state": "opening",
            "selected_tier": selected_tier,
            "selected_model": format!("vendor/{selected_tier}{model_suffix}"),
            "route_measurement": measurement,
            "reason": "static_table",
            "pinned": false,
            "locked": false,
            "trialed": false
        }))
        .unwrap()
    }

    #[test]
    fn share_matched_is_exact_deterministic_and_redacted() {
        let records = vec![
            record(Some("secret-a"), "economy", ""),
            record(Some("secret-b"), "economy", ""),
            record(Some("secret-c"), "strong", ""),
        ];
        let report = RoutingBaselineReport::from_records(&records).unwrap();
        let reversed =
            RoutingBaselineReport::from_records(&records.iter().rev().cloned().collect::<Vec<_>>())
                .unwrap();
        assert_eq!(report, reversed);
        assert_eq!(report.groups.len(), 1);
        let group = &report.groups[0];
        let counts = group.share_matched.assignments.iter().fold(
            BTreeMap::<String, usize>::new(),
            |mut counts, item| {
                *counts.entry(item.tier.clone()).or_default() += 1;
                counts
            },
        );
        assert_eq!(counts, group.observed_selected_tier_counts);
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("secret-a"));
        assert!(!encoded.contains("secret-b"));
        assert!(!encoded.contains("secret-c"));
    }

    #[test]
    fn groups_isolate_candidate_sets_and_legacy_records_are_reported() {
        let records = vec![
            record(Some("request-a"), "economy", ""),
            record(Some("request-b"), "strong", "-v2"),
            record(None, "strong", ""),
        ];
        let report = RoutingBaselineReport::from_records(&records).unwrap();
        assert_eq!(report.total_decision_count, 3);
        assert_eq!(report.eligible_decision_count, 2);
        assert_eq!(report.groups.len(), 2);
        assert_eq!(
            report.excluded_by_reason.get("missing_decision_identity"),
            Some(&1)
        );
        for group in &report.groups {
            let model_versions = group
                .always_tier
                .iter()
                .flat_map(|baseline| baseline.assignments.iter())
                .map(|assignment| assignment.model.ends_with("-v2"))
                .collect::<BTreeSet<_>>();
            assert_eq!(group.decision_count, 1);
            assert_eq!(model_versions.len(), 1);
        }
    }

    #[test]
    fn malformed_measurement_and_unredacted_identity_are_excluded() {
        let mut malformed = record(Some("request-a"), "economy", "");
        malformed
            .route_measurement
            .as_mut()
            .unwrap()
            .candidate_set_digest = "sha256:forged".into();
        let mut raw_identity = record(None, "strong", "");
        raw_identity.ingress_request_id_sha256 = Some("secret-request-id".into());
        let mut mismatched_target = record(Some("request-c"), "economy", "");
        mismatched_target.selected_model = Some("vendor/strong".into());

        let report =
            RoutingBaselineReport::from_records(&[malformed, raw_identity, mismatched_target])
                .unwrap();
        assert_eq!(report.eligible_decision_count, 0);
        assert_eq!(
            report.excluded_by_reason.get("invalid_route_measurement"),
            Some(&1)
        );
        assert_eq!(
            report.excluded_by_reason.get("missing_decision_identity"),
            Some(&1)
        );
        assert_eq!(
            report.excluded_by_reason.get("selected_target_mismatch"),
            Some(&1)
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("secret-request-id")
        );
    }

    #[test]
    fn uppercase_commitments_cannot_bypass_duplicate_detection() {
        let mut uppercase = record(None, "economy", "");
        uppercase.ingress_request_id_sha256 = Some(
            ingress_request_id_sha256("request-a")[..7].to_owned()
                + &ingress_request_id_sha256("request-a")[7..].to_ascii_uppercase(),
        );
        let report = RoutingBaselineReport::from_records(&[
            record(Some("request-a"), "economy", ""),
            uppercase,
        ])
        .unwrap();
        assert_eq!(report.eligible_decision_count, 1);
        assert_eq!(
            report.excluded_by_reason.get("missing_decision_identity"),
            Some(&1)
        );
    }

    #[test]
    fn share_matched_assignment_ignores_per_identity_observed_labels() {
        let original = vec![
            record(Some("request-a"), "economy", ""),
            record(Some("request-b"), "economy", ""),
            record(Some("request-c"), "strong", ""),
            record(Some("request-d"), "strong", ""),
        ];
        let mut permuted = original.clone();
        permuted[0].selected_tier = Some("strong".into());
        permuted[0].selected_model = Some("vendor/strong".into());
        permuted[2].selected_tier = Some("economy".into());
        permuted[2].selected_model = Some("vendor/economy".into());

        let original_report = RoutingBaselineReport::from_records(&original).unwrap();
        let permuted_report = RoutingBaselineReport::from_records(&permuted).unwrap();
        assert_ne!(
            original_report.dataset_digest,
            permuted_report.dataset_digest
        );
        assert_eq!(
            original_report.groups[0].share_matched.assignments,
            permuted_report.groups[0].share_matched.assignments
        );
    }
}
