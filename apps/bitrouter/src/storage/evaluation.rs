//! Shared validation and semantic digests for persisted evaluation evidence.
//! No evaluator service, admission authority or storage connection is constructed here.

#[cfg(feature = "multi-instance")]
pub(crate) mod shared;

use bitrouter_ai::eval::{
    EVAL_SCHEMA_VERSION, EvalExperimentRef, EvalSubject, EvaluationResult, EvidenceItem,
    ExperimentArm, ROUTE_MEASUREMENT_SCHEMA_VERSION, RouteActionCandidate,
    RouteDecisionMeasurement,
};
use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use bitrouter_ai::types::ReasoningEffort;

pub(crate) fn route_decision_measurement(
    logging_action_tier: impl Into<String>,
    logging_action_model: impl Into<String>,
    logging_action_effort: Option<ReasoningEffort>,
    mut candidates: Vec<RouteActionCandidate>,
) -> Result<RouteDecisionMeasurement> {
    candidates.sort_by(|left, right| candidate_key(left).cmp(&candidate_key(right)));
    let logging_action_tier = logging_action_tier.into();
    let logging_action_model = logging_action_model.into();
    let logging_action_probability_ppm = candidates
        .iter()
        .find(|candidate| {
            candidate.tier == logging_action_tier
                && candidate.model == logging_action_model
                && candidate.effort == logging_action_effort
        })
        .map_or(0, |candidate| candidate.logging_probability_ppm);
    let candidate_set_digest = route_candidate_set_digest(&candidates)?;
    let measurement = RouteDecisionMeasurement {
        schema_version: ROUTE_MEASUREMENT_SCHEMA_VERSION,
        logging_action_tier,
        logging_action_model,
        logging_action_effort,
        logging_action_probability_ppm,
        candidate_set_digest,
        candidates,
    };
    validate_route_measurement(&measurement)?;
    Ok(measurement)
}

#[cfg(any(feature = "core", feature = "multi-instance", test))]
pub(crate) fn subject_digest(value: &EvalSubject) -> Result<String> {
    validate_subject(value)?;
    canonical_digest(value)
}

#[cfg(any(feature = "core", feature = "multi-instance"))]
pub(crate) fn result_digest(value: &EvaluationResult) -> Result<String> {
    validate_result(value)?;
    canonical_digest(value)
}

pub fn validate_subject(subject: &EvalSubject) -> Result<()> {
    if subject.schema_version != EVAL_SCHEMA_VERSION {
        anyhow::bail!("unsupported eval subject schema version")
    }
    validate_identifier(&subject.eval_id, "eval_id")?;
    validate_identifier(&subject.subject_id, "subject_id")?;
    validate_digest(&subject.policy_digest, "policy_digest")?;
    chrono::DateTime::parse_from_rfc3339(&subject.observed_at)
        .context("observed_at must be RFC3339")?;
    let mut decision_ids = BTreeSet::new();
    for decision in &subject.decisions {
        validate_identifier(&decision.decision_id, "decision_id")?;
        if !decision_ids.insert(decision.decision_id.as_str()) {
            anyhow::bail!("duplicate eval decision id '{}';", decision.decision_id)
        }
        validate_identifier(&decision.policy, "decision.policy")?;
        validate_identifier(&decision.route_projection, "decision.route_projection")?;
        validate_identifier(&decision.request_key, "decision.request_key")?;
        validate_identifier(&decision.selected_tier, "decision.selected_tier")?;
        validate_digest(&decision.policy_digest, "decision.policy_digest")?;
        if let Some(experiment) = &decision.experiment {
            validate_digest(
                &experiment.experiment_id,
                "decision.experiment.experiment_id",
            )?;
            validate_digest(
                &experiment.assignment_id_digest,
                "decision.experiment.assignment_id_digest",
            )?;
            if !(1..=1_000_000).contains(&experiment.challenger_propensity_ppm) {
                anyhow::bail!(
                    "decision.experiment.challenger_propensity_ppm must be between 1 and 1000000"
                )
            }
        }
        if let Some(measurement) = &decision.route_measurement {
            validate_route_measurement_for_experiment(measurement, decision.experiment.as_ref())?;
        }
    }
    for dimension in &subject.requested_dimensions {
        validate_metric_id(dimension)?;
    }
    let mut evidence_ids = BTreeSet::new();
    for item in &subject.evidence {
        validate_identifier(&item.evidence_id, "evidence_id")?;
        if !evidence_ids.insert(item.evidence_id.as_str()) {
            anyhow::bail!("duplicate evidence id '{}';", item.evidence_id)
        }
        validate_metric_id(&item.kind)?;
        validate_digest(&item.digest, "evidence.digest")?;
        if !item.redacted {
            anyhow::bail!("evidence '{}' is not explicitly redacted", item.evidence_id)
        }
        if item
            .attributes
            .iter()
            .any(|(key, value)| attribute_looks_sensitive(key, value))
        {
            anyhow::bail!(
                "evidence '{}' contains credential-shaped attribute material",
                item.evidence_id
            )
        }
    }
    validate_digest(&subject.evidence_digest, "evidence_digest")?;
    let actual = evidence_digest(&subject.evidence)?;
    if actual != subject.evidence_digest {
        anyhow::bail!("subject evidence_digest does not match evidence items")
    }
    Ok(())
}

pub(crate) fn validate_route_measurement(measurement: &RouteDecisionMeasurement) -> Result<()> {
    if measurement.schema_version != ROUTE_MEASUREMENT_SCHEMA_VERSION {
        anyhow::bail!("unsupported route measurement schema version")
    }
    if measurement.candidates.is_empty() || measurement.candidates.len() > 64 {
        anyhow::bail!("route measurement must contain between 1 and 64 candidates")
    }
    validate_identifier(
        &measurement.logging_action_tier,
        "route_measurement.logging_action_tier",
    )?;
    validate_model_id(
        &measurement.logging_action_model,
        "route_measurement.logging_action_model",
    )?;
    validate_digest(
        &measurement.candidate_set_digest,
        "route_measurement.candidate_set_digest",
    )?;

    let mut previous = None;
    let mut probability_sum = 0_u64;
    let mut action_matches = 0_u8;
    for candidate in &measurement.candidates {
        validate_identifier(&candidate.tier, "route_measurement.candidate.tier")?;
        validate_model_id(&candidate.model, "route_measurement.candidate.model")?;
        if candidate.logging_probability_ppm > 1_000_000 {
            anyhow::bail!("route candidate logging probability exceeds one million ppm")
        }
        probability_sum =
            probability_sum.saturating_add(u64::from(candidate.logging_probability_ppm));
        let key = candidate_key(candidate);
        if previous.as_ref().is_some_and(|previous| previous >= &key) {
            anyhow::bail!("route measurement candidates are not unique canonical order")
        }
        previous = Some(key);
        if candidate.tier == measurement.logging_action_tier
            && candidate.model == measurement.logging_action_model
            && candidate.effort == measurement.logging_action_effort
        {
            action_matches = action_matches.saturating_add(1);
            if candidate.logging_probability_ppm != measurement.logging_action_probability_ppm {
                anyhow::bail!("route measurement action probability does not match candidate")
            }
        }
    }
    if probability_sum != 1_000_000 {
        anyhow::bail!("route measurement probabilities must sum to one million ppm")
    }
    if action_matches != 1 || measurement.logging_action_probability_ppm == 0 {
        anyhow::bail!("route measurement action must match one positive-probability candidate")
    }
    if route_candidate_set_digest(&measurement.candidates)? != measurement.candidate_set_digest {
        anyhow::bail!("route measurement candidate set digest does not match candidates")
    }
    Ok(())
}

pub(crate) fn validate_route_measurement_for_experiment(
    measurement: &RouteDecisionMeasurement,
    experiment: Option<&EvalExperimentRef>,
) -> Result<()> {
    validate_route_measurement(measurement)?;
    let mut actual_positive = measurement
        .candidates
        .iter()
        .map(|candidate| candidate.logging_probability_ppm)
        .filter(|probability| *probability > 0)
        .collect::<Vec<_>>();
    actual_positive.sort_unstable();
    let Some(experiment) = experiment else {
        if actual_positive != [1_000_000] {
            anyhow::bail!("route measurement without an experiment must be deterministic")
        }
        return Ok(());
    };

    let challenger = experiment.challenger_propensity_ppm;
    let champion = 1_000_000_u32
        .checked_sub(challenger)
        .ok_or_else(|| anyhow::anyhow!("experiment challenger propensity exceeds one million"))?;
    let expected_action = match experiment.arm {
        ExperimentArm::Control => champion,
        ExperimentArm::Challenger => challenger,
    };
    if measurement.logging_action_probability_ppm != expected_action {
        anyhow::bail!("route measurement action probability contradicts experiment arm")
    }
    let mut expected_positive = [champion, challenger]
        .into_iter()
        .filter(|probability| *probability > 0)
        .collect::<Vec<_>>();
    expected_positive.sort_unstable();
    if actual_positive != expected_positive {
        anyhow::bail!("route measurement distribution contradicts two-arm experiment")
    }
    Ok(())
}

fn candidate_key(
    candidate: &RouteActionCandidate,
) -> (&str, &str, &str, bitrouter_ai::routing::ContextStrategy) {
    (
        candidate.tier.as_str(),
        candidate.model.as_str(),
        candidate.effort.map_or("inherit", ReasoningEffort::as_str),
        candidate.context,
    )
}

fn route_candidate_set_digest(candidates: &[RouteActionCandidate]) -> Result<String> {
    let targets = candidates.iter().map(candidate_key).collect::<Vec<_>>();
    canonical_digest(&("bitrouter.route-candidate-set.v2", targets))
}

fn validate_model_id(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be a non-empty bounded model identifier")
    }
    Ok(())
}

pub fn validate_result(result: &EvaluationResult) -> Result<()> {
    if result.schema_version != EVAL_SCHEMA_VERSION {
        anyhow::bail!("unsupported evaluation result schema version")
    }
    validate_identifier(&result.eval_id, "eval_id")?;
    validate_digest(&result.evidence_digest, "evidence_digest")?;
    validate_identifier(&result.evaluator.authority_id, "authority_id")?;
    validate_identifier(&result.evaluator.evaluator_id, "evaluator_id")?;
    validate_identifier(&result.evaluator.version, "evaluator.version")?;
    validate_digest(&result.evaluator.config_digest, "evaluator.config_digest")?;
    validate_identifier(&result.idempotency_key, "idempotency_key")?;
    chrono::DateTime::parse_from_rfc3339(&result.submitted_at)
        .context("submitted_at must be RFC3339")?;
    if result
        .confidence_ppm
        .is_some_and(|value| !(0..=1_000_000).contains(&value))
    {
        anyhow::bail!("confidence_ppm must be between 0 and 1000000")
    }
    for (metric_id, value) in &result.metrics {
        validate_metric_id(metric_id)?;
        value.validate()?;
    }
    let mut evidence_refs = BTreeSet::new();
    for evidence_ref in &result.evidence_refs {
        validate_identifier(evidence_ref, "evidence_ref")?;
        if !evidence_refs.insert(evidence_ref.as_str()) {
            anyhow::bail!("duplicate evidence reference '{evidence_ref}'")
        }
    }
    let mut hard_violations = BTreeSet::new();
    for violation in &result.hard_violations {
        validate_metric_id(violation)?;
        if !hard_violations.insert(violation.as_str()) {
            anyhow::bail!("duplicate hard violation '{violation}'")
        }
    }
    for (decision_id, credit) in &result.decision_credit {
        validate_identifier(decision_id, "decision_credit id")?;
        if !(0..=1_000_000).contains(&credit.weight_ppm) {
            anyhow::bail!("decision credit weight must be between 0 and 1000000")
        }
        for metric_id in &credit.metric_ids {
            validate_metric_id(metric_id)?;
            if metric_id != "quality.pass"
                && !result.metrics.contains_key(metric_id)
                && !result.hard_violations.contains(metric_id)
            {
                anyhow::bail!("decision credit references absent metric '{metric_id}'")
            }
        }
    }
    Ok(())
}

pub fn validate_result_for_subject(result: &EvaluationResult, subject: &EvalSubject) -> Result<()> {
    validate_result(result)?;
    validate_subject(subject)?;
    if result.eval_id != subject.eval_id || result.evidence_digest != subject.evidence_digest {
        anyhow::bail!("evaluation result does not match its subject evidence")
    }
    let decisions = subject
        .decisions
        .iter()
        .map(|decision| decision.decision_id.as_str())
        .collect::<BTreeSet<_>>();
    if result
        .decision_credit
        .keys()
        .any(|decision_id| !decisions.contains(decision_id.as_str()))
    {
        anyhow::bail!("evaluation result credits an unknown decision")
    }
    if result
        .metrics
        .keys()
        .any(|metric_id| !subject.requested_dimensions.contains(metric_id))
    {
        anyhow::bail!("evaluation result contains an unrequested metric")
    }
    let evidence_ids = subject
        .evidence
        .iter()
        .map(|item| item.evidence_id.as_str())
        .collect::<BTreeSet<_>>();
    if result
        .evidence_refs
        .iter()
        .any(|evidence_ref| !evidence_ids.contains(evidence_ref.as_str()))
    {
        anyhow::bail!("evaluation result references absent subject evidence")
    }
    Ok(())
}

pub fn evidence_digest(evidence: &[EvidenceItem]) -> Result<String> {
    let mut ordered = evidence.to_vec();
    ordered.sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
    canonical_digest(&ordered)
}

pub fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing canonical eval value")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be a non-empty bounded identifier")
    }
    Ok(())
}

fn validate_metric_id(value: &str) -> Result<()> {
    if !value.contains('.')
        || value.starts_with('.')
        || value.ends_with('.')
        || value.chars().any(|character| {
            !(character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-'))
        })
    {
        anyhow::bail!("metric/evidence kind '{value}' must be a lowercase namespaced id")
    }
    Ok(())
}

pub(crate) fn validate_digest(value: &str, field: &str) -> Result<()> {
    let Some(hex_digest) = value.strip_prefix("sha256:") else {
        anyhow::bail!("{field} must be a sha256 digest")
    };
    if hex_digest.len() != 64
        || !hex_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{field} must contain 64 lowercase hexadecimal digits")
    }
    Ok(())
}

fn attribute_looks_sensitive(key: &str, value: &str) -> bool {
    let normalized_key = key.to_ascii_lowercase().replace('-', "_");
    let sensitive_key = matches!(
        normalized_key.as_str(),
        "authorization"
            | "proxy_authorization"
            | "x_api_key"
            | "api_key"
            | "access_token"
            | "refresh_token"
            | "cookie"
            | "set_cookie"
            | "secret"
    ) || normalized_key.ends_with("_secret")
        || normalized_key.ends_with("_api_key")
        || normalized_key.ends_with("_access_token")
        || normalized_key.ends_with("_refresh_token");
    let normalized_value = value.to_ascii_lowercase();
    sensitive_key
        || normalized_value.starts_with("bearer ")
        || normalized_value.contains("brvk_")
        || normalized_value.starts_with("sk-")
        || normalized_value.contains("-----begin private key-----")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::eval::{
        EvalDecisionRef, EvalScope, EvalVerdict, EvaluatorIdentity, EvaluatorKind,
        ExperimentAssignmentUnit, MetricUnit, MetricValue,
    };
    use std::collections::BTreeMap;

    #[test]
    fn metric_units_enforce_integer_domains() {
        assert!(MetricValue::new(1, MetricUnit::Boolean).validate().is_ok());
        assert!(MetricValue::new(2, MetricUnit::Boolean).validate().is_err());
        assert!(
            MetricValue::new(1_000_001, MetricUnit::Ppm)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn exported_subject_requires_redacted_evidence() {
        let mut subject = subject_fixture();
        subject.evidence[0].redacted = false;
        assert!(validate_subject(&subject).is_err());
    }

    #[test]
    fn redacted_flag_cannot_hide_obvious_secret_material() {
        let mut subject = subject_fixture();
        subject.evidence[0]
            .attributes
            .insert("authorization".into(), "Bearer brvk_super-secret".into());
        subject.evidence_digest = evidence_digest(&subject.evidence).unwrap_or_default();

        assert!(validate_subject(&subject).is_err());
    }

    #[test]
    fn every_evaluator_kind_uses_the_same_versioned_result_contract() -> anyhow::Result<()> {
        for kind in [
            EvaluatorKind::TaskNative,
            EvaluatorKind::Human,
            EvaluatorKind::Enterprise,
            EvaluatorKind::Agentic,
            EvaluatorKind::Generic,
        ] {
            let subject = subject_fixture();
            let result = EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: subject.eval_id.clone(),
                evidence_digest: subject.evidence_digest.clone(),
                evaluator: EvaluatorIdentity {
                    authority_id: "fixture-authority".into(),
                    evaluator_id: "fixture-evaluator".into(),
                    kind,
                    version: "1".into(),
                    config_digest: subject.policy_digest.clone(),
                },
                verdict: EvalVerdict::Pass,
                metrics: BTreeMap::from([(
                    "quality.pass".into(),
                    MetricValue::new(1, MetricUnit::Boolean),
                )]),
                hard_violations: Vec::new(),
                confidence_ppm: Some(1_000_000),
                evidence_refs: vec!["usage".into()],
                decision_credit: BTreeMap::new(),
                idempotency_key: format!("fixture-{kind:?}"),
                submitted_at: "2026-07-30T00:01:00Z".into(),
            };
            let encoded = serde_json::to_vec(&result)?;
            let decoded: EvaluationResult = serde_json::from_slice(&encoded)?;
            assert_eq!(decoded, result);
            validate_result_for_subject(&decoded, &subject)?;
        }
        Ok(())
    }

    #[test]
    fn subject_digest_is_stable_for_ordered_maps() -> anyhow::Result<()> {
        let left = subject_fixture();
        let mut right = subject_fixture();
        right.requested_dimensions = ["cost.usd_micros", "quality.pass"]
            .into_iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            crate::storage::evaluation::subject_digest(&left)?,
            crate::storage::evaluation::subject_digest(&right)?
        );
        Ok(())
    }

    #[test]
    fn decision_without_primary_route_projection_is_rejected() {
        let incomplete = serde_json::json!({
            "decision_id": "decision-1",
            "policy": "auto",
            "request_key": "semantic_route/v1|unknown|implement|normal",
            "selected_tier": "economy",
            "baseline_tier": "strong",
            "policy_digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        });
        assert!(serde_json::from_value::<EvalDecisionRef>(incomplete).is_err());
    }

    #[test]
    fn retired_predictive_v1_fallback_field_is_rejected() {
        let mut retired = serde_json::json!({
            "decision_id": "decision-1",
            "policy": "auto",
            "route_projection": "semantic_route/v1|code:generation|implement|normal",
            "request_key": "semantic_route/v1|unknown|implement|normal",
            "selected_tier": "balanced",
            "baseline_tier": "balanced",
            "policy_digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        });
        retired
            .as_object_mut()
            .expect("decision fixture is an object")
            .insert(
                ["predictive_v1", "fallback_tier"].join("_"),
                serde_json::json!("balanced"),
            );

        assert!(serde_json::from_value::<EvalDecisionRef>(retired).is_err());
    }

    #[test]
    fn primary_projection_is_bounded() {
        let mut subject = subject_fixture();
        subject.decisions.push(EvalDecisionRef {
            decision_id: "decision-1".into(),
            policy: "auto".into(),
            route_projection: "x".repeat(513),
            request_key: "semantic_route/v1|unknown|implement|normal".into(),
            selected_tier: "balanced".into(),
            selected_effort: None,
            baseline_tier: Some("balanced".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
            experiment: None,
            route_measurement: None,
        });
        assert!(validate_subject(&subject).is_err());
    }

    #[test]
    fn subject_rejects_duplicate_decision_ids() {
        let mut subject = subject_fixture();
        let decision = EvalDecisionRef {
            decision_id: "decision-duplicate".into(),
            policy: "auto".into(),
            route_projection: "semantic_route/v1|unknown|implement|normal".into(),
            request_key: "semantic_route/v1|unknown|implement|normal".into(),
            selected_tier: "balanced".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
            experiment: None,
            route_measurement: None,
        };
        subject.decisions = vec![decision.clone(), decision];

        let error = validate_subject(&subject).expect_err("duplicate decisions must fail closed");

        assert!(error.to_string().contains("duplicate eval decision id"));
    }

    #[test]
    fn decision_experiment_metadata_is_backward_compatible_and_canonical() -> anyhow::Result<()> {
        let legacy = serde_json::json!({
            "decision_id": "decision-1",
            "policy": "auto",
            "route_projection": "semantic_route/v1|code:generation|implement|normal",
            "request_key": "semantic_route/v1|unknown|implement|normal",
            "selected_tier": "strong",
            "baseline_tier": "strong",
            "policy_digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        });
        let old: EvalDecisionRef = serde_json::from_value(legacy.clone())?;
        assert_eq!(old.experiment, None);
        assert_eq!(serde_json::to_value(old)?, legacy);

        let experiment = EvalExperimentRef {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            arm: ExperimentArm::Challenger,
            assignment_unit: ExperimentAssignmentUnit::Task,
            assignment_id_digest:
                "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
            challenger_propensity_ppm: 100_000,
        };
        let mut decision: EvalDecisionRef = serde_json::from_value(legacy)?;
        decision.experiment = Some(experiment);
        assert_eq!(
            serde_json::to_value(decision)?["experiment"],
            serde_json::json!({
                "experiment_id": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "arm": "challenger",
                "assignment_unit": "task",
                "assignment_id_digest": "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                "challenger_propensity_ppm": 100000
            })
        );
        Ok(())
    }

    #[test]
    fn route_measurement_is_backward_compatible_and_canonical() -> anyhow::Result<()> {
        let legacy = serde_json::json!({
            "decision_id": "decision-1",
            "policy": "auto",
            "route_projection": "semantic_route/v1|code:generation|implement|normal",
            "request_key": "semantic_route/v1|unknown|implement|normal",
            "selected_tier": "balanced",
            "baseline_tier": "balanced",
            "policy_digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        });
        let old: EvalDecisionRef = serde_json::from_value(legacy.clone())?;
        assert_eq!(old.route_measurement, None);
        assert_eq!(serde_json::to_value(old)?, legacy);

        let candidates = vec![
            RouteActionCandidate {
                context: Default::default(),
                tier: "balanced".into(),
                model: "bitrouter:balanced".into(),
                effort: None,
                logging_probability_ppm: 900_000,
            },
            RouteActionCandidate {
                context: Default::default(),
                tier: "economy".into(),
                model: "bitrouter:economy".into(),
                effort: Some(ReasoningEffort::Low),
                logging_probability_ppm: 100_000,
            },
        ];
        let measurement = crate::storage::evaluation::route_decision_measurement(
            "economy",
            "bitrouter:economy",
            Some(ReasoningEffort::Low),
            candidates,
        )?;
        let mut decision: EvalDecisionRef = serde_json::from_value(legacy)?;
        decision.experiment = Some(EvalExperimentRef {
            experiment_id:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            arm: ExperimentArm::Challenger,
            assignment_unit: ExperimentAssignmentUnit::Task,
            assignment_id_digest:
                "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
            challenger_propensity_ppm: 100_000,
        });
        decision.route_measurement = Some(measurement.clone());
        let encoded = serde_json::to_value(&decision)?;
        assert_eq!(encoded["route_measurement"]["schema_version"], 2);
        assert_eq!(
            encoded["route_measurement"]["logging_action_probability_ppm"],
            100_000
        );
        assert_eq!(
            encoded["route_measurement"]["candidate_set_digest"],
            measurement.candidate_set_digest
        );

        let mut subject = subject_fixture();
        subject.decisions = vec![decision];
        validate_subject(&subject)?;
        Ok(())
    }

    #[test]
    fn route_measurement_must_match_experiment_arm_and_distribution() -> anyhow::Result<()> {
        let mut subject = subject_fixture();
        let base = EvalDecisionRef {
            decision_id: "decision-1".into(),
            policy: "auto".into(),
            route_projection: "semantic_route/v1|code:generation|implement|normal".into(),
            request_key: "semantic_route/v1|unknown|implement|normal".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
            experiment: Some(EvalExperimentRef {
                experiment_id: subject.policy_digest.clone(),
                arm: ExperimentArm::Control,
                assignment_unit: ExperimentAssignmentUnit::Task,
                assignment_id_digest: subject.policy_digest.clone(),
                challenger_propensity_ppm: 100_000,
            }),
            route_measurement: Some(crate::storage::evaluation::route_decision_measurement(
                "economy",
                "bitrouter:economy",
                None,
                vec![
                    RouteActionCandidate {
                        context: Default::default(),
                        tier: "economy".into(),
                        model: "bitrouter:economy".into(),
                        effort: None,
                        logging_probability_ppm: 100_000,
                    },
                    RouteActionCandidate {
                        context: Default::default(),
                        tier: "strong".into(),
                        model: "bitrouter:strong".into(),
                        effort: None,
                        logging_probability_ppm: 900_000,
                    },
                ],
            )?),
        };
        subject.decisions = vec![base.clone()];
        assert!(validate_subject(&subject).is_err());

        let mut split = base;
        split.experiment.as_mut().unwrap().arm = ExperimentArm::Challenger;
        split.route_measurement = Some(crate::storage::evaluation::route_decision_measurement(
            "economy",
            "bitrouter:economy",
            None,
            vec![
                RouteActionCandidate {
                    context: Default::default(),
                    tier: "economy".into(),
                    model: "bitrouter:economy".into(),
                    effort: None,
                    logging_probability_ppm: 100_000,
                },
                RouteActionCandidate {
                    context: Default::default(),
                    tier: "standard".into(),
                    model: "bitrouter:standard".into(),
                    effort: None,
                    logging_probability_ppm: 200_000,
                },
                RouteActionCandidate {
                    context: Default::default(),
                    tier: "strong".into(),
                    model: "bitrouter:strong".into(),
                    effort: None,
                    logging_probability_ppm: 700_000,
                },
            ],
        )?);
        subject.decisions = vec![split];
        assert!(validate_subject(&subject).is_err());
        Ok(())
    }

    #[test]
    fn route_measurement_validation_rejects_forged_probability_evidence() -> anyhow::Result<()> {
        let valid = crate::storage::evaluation::route_decision_measurement(
            "balanced",
            "bitrouter:balanced",
            None,
            vec![
                RouteActionCandidate {
                    context: Default::default(),
                    tier: "balanced".into(),
                    model: "bitrouter:balanced".into(),
                    effort: None,
                    logging_probability_ppm: 750_000,
                },
                RouteActionCandidate {
                    context: Default::default(),
                    tier: "economy".into(),
                    model: "bitrouter:economy".into(),
                    effort: None,
                    logging_probability_ppm: 250_000,
                },
            ],
        )?;
        let invalid = [
            {
                let mut value = valid.clone();
                value.candidates[0].logging_probability_ppm = 750_001;
                value
            },
            {
                let mut value = valid.clone();
                value.candidates.swap(0, 1);
                value
            },
            {
                let mut value = valid.clone();
                value.candidates.push(value.candidates[0].clone());
                value
            },
            {
                let mut value = valid.clone();
                value.logging_action_probability_ppm = 1;
                value
            },
            {
                let mut value = valid.clone();
                value.candidate_set_digest = "sha256:invalid".into();
                value
            },
        ];

        for route_measurement in invalid {
            assert!(validate_route_measurement(&route_measurement).is_err());
        }
        Ok(())
    }

    #[test]
    fn result_cannot_reference_missing_subject_evidence() -> anyhow::Result<()> {
        let subject = subject_fixture();
        let result = EvaluationResult {
            schema_version: EVAL_SCHEMA_VERSION,
            eval_id: subject.eval_id.clone(),
            evidence_digest: subject.evidence_digest.clone(),
            evaluator: EvaluatorIdentity {
                authority_id: "local".into(),
                evaluator_id: "task-runner".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest: subject.policy_digest.clone(),
            },
            verdict: EvalVerdict::Pass,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: vec!["missing-evidence".into()],
            decision_credit: BTreeMap::new(),
            idempotency_key: "missing-evidence-result".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        };

        assert!(validate_result_for_subject(&result, &subject).is_err());
        Ok(())
    }

    fn subject_fixture() -> EvalSubject {
        let evidence = vec![EvidenceItem {
            evidence_id: "usage".into(),
            kind: "request.usage".into(),
            digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            redacted: true,
            attributes: Default::default(),
        }];
        EvalSubject {
            schema_version: 1,
            eval_id: "eval-1".into(),
            scope: EvalScope::Request,
            subject_id: "request-1".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: Vec::new(),
            requested_dimensions: ["quality.pass", "cost.usd_micros"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            evidence_digest: canonical_digest(&evidence).unwrap_or_default(),
            evidence,
            observed_at: "2026-07-30T00:00:00Z".into(),
        }
    }
}
