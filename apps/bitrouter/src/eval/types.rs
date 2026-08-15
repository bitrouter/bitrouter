//! Versioned wire contracts for generic evaluation subjects and results.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bitrouter_sdk::language_model::types::ReasoningEffort;

pub const EVAL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalScope {
    Request,
    Episode,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalSubjectStatus {
    Pending,
    Evaluated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalDecisionRef {
    pub decision_id: String,
    pub policy: String,
    pub route_projection: String,
    pub request_key: String,
    pub selected_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<ReasoningEffort>,
    pub baseline_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_effort: Option<ReasoningEffort>,
    pub policy_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceItem {
    pub evidence_id: String,
    pub kind: String,
    pub digest: String,
    pub redacted: bool,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalSubject {
    pub schema_version: u32,
    pub eval_id: String,
    pub scope: EvalScope,
    pub subject_id: String,
    pub policy_digest: String,
    pub preset: Option<String>,
    pub cohort: Option<String>,
    pub holdout: bool,
    #[serde(default)]
    pub decisions: Vec<EvalDecisionRef>,
    #[serde(default)]
    pub requested_dimensions: BTreeSet<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
    pub evidence_digest: String,
    pub observed_at: String,
}

impl EvalSubject {
    pub fn semantic_digest(&self) -> Result<String> {
        validate_subject(self)?;
        canonical_digest(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorKind {
    TaskNative,
    Human,
    Enterprise,
    Agentic,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatorIdentity {
    pub authority_id: String,
    pub evaluator_id: String,
    pub kind: EvaluatorKind,
    pub version: String,
    pub config_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalVerdict {
    Pass,
    Fail,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Boolean,
    Ppm,
    MicroUsd,
    Milliseconds,
    Count,
    ScalarMicros,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricValue {
    pub value: i64,
    pub unit: MetricUnit,
}

impl MetricValue {
    pub fn new(value: i64, unit: MetricUnit) -> Self {
        Self { value, unit }
    }

    pub fn validate(&self) -> Result<()> {
        match self.unit {
            MetricUnit::Boolean if !matches!(self.value, 0 | 1) => {
                anyhow::bail!("boolean metric must be 0 or 1")
            }
            MetricUnit::Ppm if !(0..=1_000_000).contains(&self.value) => {
                anyhow::bail!("ppm metric must be between 0 and 1000000")
            }
            MetricUnit::MicroUsd | MetricUnit::Milliseconds | MetricUnit::Count
                if self.value < 0 =>
            {
                anyhow::bail!("unsigned metric unit cannot be negative")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionCredit {
    pub weight_ppm: i64,
    #[serde(default)]
    pub metric_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    pub schema_version: u32,
    pub eval_id: String,
    pub evidence_digest: String,
    pub evaluator: EvaluatorIdentity,
    pub verdict: EvalVerdict,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricValue>,
    #[serde(default)]
    pub hard_violations: Vec<String>,
    pub confidence_ppm: Option<i64>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    #[serde(default)]
    pub decision_credit: BTreeMap<String, DecisionCredit>,
    pub idempotency_key: String,
    pub submitted_at: String,
}

impl EvaluationResult {
    pub fn semantic_digest(&self) -> Result<String> {
        validate_result(self)?;
        canonical_digest(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Admitted,
    Rejected,
    HeldOut,
    Disputed,
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

fn validate_digest(value: &str, field: &str) -> Result<()> {
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
        assert_eq!(left.semantic_digest()?, right.semantic_digest()?);
        Ok(())
    }

    #[test]
    fn decision_without_primary_route_projection_is_rejected() {
        let incomplete = serde_json::json!({
            "decision_id": "decision-1",
            "policy": "auto",
            "request_key": "agent_route/v1|unknown|implement|normal",
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
            "route_projection": "agent_route/v1|code:generation|implement|normal",
            "request_key": "agent_route/v1|unknown|implement|normal",
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
            request_key: "agent_route/v1|unknown|implement|normal".into(),
            selected_tier: "balanced".into(),
            selected_effort: None,
            baseline_tier: Some("balanced".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
        });
        assert!(validate_subject(&subject).is_err());
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
