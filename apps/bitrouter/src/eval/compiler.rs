//! Deterministic projection of one frozen eval snapshot into route evidence.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;

use super::store::{EvalSnapshot, EvalStore};
use super::types::{
    DecisionCredit, EvalSubject, EvalVerdict, EvaluationResult, EvaluatorKind, MetricUnit,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvalEvidenceRecord {
    pub result_id: String,
    pub content_digest: String,
    pub subject: EvalSubject,
    pub result: EvaluationResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvalEvidenceSnapshot {
    pub evidence_root: String,
    pub frozen_at: String,
    pub records: Vec<EvalEvidenceRecord>,
}

impl EvalEvidenceSnapshot {
    pub async fn load(store: &EvalStore, evidence_root: &str) -> Result<Self> {
        let snapshot = store
            .snapshot_by_root(evidence_root)
            .await?
            .ok_or_else(|| anyhow::anyhow!("eval snapshot '{evidence_root}' not found"))?;
        Self::from_manifest(store, snapshot).await
    }

    async fn from_manifest(store: &EvalStore, snapshot: EvalSnapshot) -> Result<Self> {
        let mut records = Vec::with_capacity(snapshot.entries.len());
        for entry in snapshot.entries {
            let stored = store.result(&entry.result_id).await?.ok_or_else(|| {
                anyhow::anyhow!("snapshot result '{}' is missing", entry.result_id)
            })?;
            if stored.content_digest != entry.content_digest {
                anyhow::bail!("snapshot result '{}' changed content", entry.result_id);
            }
            let subject = store.subject(&entry.eval_id).await?.ok_or_else(|| {
                anyhow::anyhow!("snapshot subject '{}' is missing", entry.eval_id)
            })?;
            let subject_digest = subject.semantic_digest()?;
            if subject_digest != entry.subject_content_digest {
                anyhow::bail!("snapshot subject '{}' changed content", entry.eval_id);
            }
            super::types::validate_result_for_subject(&stored.result, &subject)?;
            records.push(EvalEvidenceRecord {
                result_id: entry.result_id,
                content_digest: entry.content_digest,
                subject,
                result: stored.result,
            });
        }
        records.sort_by(|left, right| left.result_id.cmp(&right.result_id));
        Ok(Self {
            evidence_root: snapshot.evidence_root,
            frozen_at: snapshot.frozen_at,
            records,
        })
    }

    pub fn route_evidence(&self) -> Result<BTreeMap<(String, String), RouteEvalEvidence>> {
        let mut routes = BTreeMap::<(String, String), RouteEvalEvidence>::new();
        for record in &self.records {
            for decision in &record.subject.decisions {
                let Some(credit) = credit_for_decision(
                    &record.result,
                    &decision.decision_id,
                    record.subject.decisions.len(),
                ) else {
                    continue;
                };
                let quality_credited = credit.includes("quality.pass");
                let cost_credited = credit.includes("cost.usd_micros");
                let latency_credited = credit.includes("latency.ms");
                let credited_violations = record
                    .result
                    .hard_violations
                    .iter()
                    .filter(|violation| credit.includes(violation))
                    .count();
                if !quality_credited
                    && !cost_credited
                    && !latency_credited
                    && credited_violations == 0
                {
                    continue;
                }
                let route_projection = &decision.route_projection;
                let route = routes
                    .entry((decision.policy.clone(), route_projection.clone()))
                    .or_default();
                route
                    .matched_request_keys
                    .insert(decision.request_key.clone());
                if let Some(baseline) = decision.baseline_tier.as_ref() {
                    match &route.baseline_tier {
                        Some(existing) if existing != baseline => {
                            anyhow::bail!(
                                "eval route '{}:{}' names conflicting baselines",
                                decision.policy,
                                route_projection
                            );
                        }
                        None => route.baseline_tier = Some(baseline.clone()),
                        _ => {}
                    }
                }
                route.sources.insert(record.result.evaluator.kind);
                route
                    .evaluator_config_digests
                    .insert(record.result.evaluator.config_digest.clone());
                route.evidence_records.push((
                    record.result_id.clone(),
                    record.content_digest.clone(),
                    record.subject.evidence_digest.clone(),
                ));
                let tier = route
                    .tiers
                    .entry(decision.selected_tier.clone())
                    .or_default();
                if quality_credited {
                    tier.eligible_episodes = tier.eligible_episodes.saturating_add(1);
                    tier.independent_tasks
                        .insert(record.subject.subject_id.clone());
                    tier.total_weight_ppm = tier.total_weight_ppm.saturating_add(credit.weight_ppm);
                    match record.result.verdict {
                        EvalVerdict::Pass => {
                            tier.pass_weight_ppm =
                                tier.pass_weight_ppm.saturating_add(credit.weight_ppm)
                        }
                        EvalVerdict::Fail => {
                            tier.fail_weight_ppm =
                                tier.fail_weight_ppm.saturating_add(credit.weight_ppm)
                        }
                        EvalVerdict::Inconclusive => {}
                    }
                }
                tier.critical_violations = tier
                    .critical_violations
                    .saturating_add(u32::try_from(credited_violations).unwrap_or(u32::MAX));
                if cost_credited {
                    add_metric(
                        &mut tier.cost_micro_usd,
                        record.result.metrics.get("cost.usd_micros"),
                        MetricUnit::MicroUsd,
                        credit.weight_ppm,
                    )?;
                }
                if latency_credited {
                    add_metric(
                        &mut tier.latency_ms,
                        record.result.metrics.get("latency.ms"),
                        MetricUnit::Milliseconds,
                        credit.weight_ppm,
                    )?;
                }
            }
        }
        for route in routes.values_mut() {
            route.evidence_records.sort();
            route.evidence_records.dedup();
        }
        Ok(routes)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteEvalEvidence {
    pub baseline_tier: Option<String>,
    pub matched_request_keys: BTreeSet<String>,
    pub tiers: BTreeMap<String, TierEvalEvidence>,
    pub sources: BTreeSet<EvaluatorKind>,
    pub evaluator_config_digests: BTreeSet<String>,
    pub evidence_records: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierEvalEvidence {
    pub eligible_episodes: u32,
    pub independent_tasks: BTreeSet<String>,
    pub total_weight_ppm: i64,
    pub pass_weight_ppm: i64,
    pub fail_weight_ppm: i64,
    pub critical_violations: u32,
    pub cost_micro_usd: MetricAggregate,
    pub latency_ms: MetricAggregate,
}

impl TierEvalEvidence {
    pub fn pass_rate_ppm(&self) -> i64 {
        let conclusive = self.pass_weight_ppm.saturating_add(self.fail_weight_ppm);
        if conclusive == 0 {
            return 0;
        }
        self.pass_weight_ppm
            .saturating_mul(1_000_000)
            .checked_div(conclusive)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricAggregate {
    weighted_total: i128,
    total_weight_ppm: i64,
}

impl MetricAggregate {
    pub fn mean(&self) -> Option<i64> {
        if self.total_weight_ppm == 0 {
            return None;
        }
        i64::try_from(self.weighted_total / i128::from(self.total_weight_ppm)).ok()
    }
}

struct AttributedCredit<'a> {
    weight_ppm: i64,
    metric_ids: Option<&'a BTreeSet<String>>,
}

impl AttributedCredit<'_> {
    fn includes(&self, metric_id: &str) -> bool {
        self.metric_ids
            .is_none_or(|metric_ids| metric_ids.is_empty() || metric_ids.contains(metric_id))
    }
}

fn credit_for_decision<'a>(
    result: &'a EvaluationResult,
    decision_id: &str,
    decision_count: usize,
) -> Option<AttributedCredit<'a>> {
    if result.decision_credit.is_empty() && decision_count == 1 {
        return Some(AttributedCredit {
            weight_ppm: 1_000_000,
            metric_ids: None,
        });
    }
    result
        .decision_credit
        .get(decision_id)
        .filter(|credit| credit.weight_ppm > 0)
        .map(
            |DecisionCredit {
                 weight_ppm,
                 metric_ids,
             }| AttributedCredit {
                weight_ppm: *weight_ppm,
                metric_ids: Some(metric_ids),
            },
        )
}

fn add_metric(
    aggregate: &mut MetricAggregate,
    metric: Option<&super::types::MetricValue>,
    expected_unit: MetricUnit,
    weight_ppm: i64,
) -> Result<()> {
    let Some(metric) = metric else {
        return Ok(());
    };
    if metric.unit != expected_unit {
        anyhow::bail!("evaluation metric has an incompatible unit");
    }
    let weighted = i128::from(metric.value)
        .checked_mul(i128::from(weight_ppm))
        .context("evaluation metric weighted value overflow")?;
    aggregate.weighted_total = aggregate
        .weighted_total
        .checked_add(weighted)
        .context("evaluation metric aggregate overflow")?;
    aggregate.total_weight_ppm = aggregate.total_weight_ppm.saturating_add(weight_ppm);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bitrouter_sdk::config::EvalConfig;

    use super::*;
    use crate::eval::EvalService;
    use crate::eval::admission::SubmissionPrincipal;
    use crate::eval::store::EvalStore;
    use crate::eval::types::*;

    #[tokio::test]
    async fn single_decision_result_gets_implicit_full_credit() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let service = EvalService::new(store.clone(), EvalConfig::default());
        let subject = subject()?;
        store.insert_subject(&subject).await?;
        service
            .submit(result(&subject), SubmissionPrincipal::LocalOperator)
            .await?;
        let frozen = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;

        let snapshot = EvalEvidenceSnapshot::load(&store, &frozen.evidence_root).await?;
        let routes = snapshot.route_evidence()?;
        let tier = &routes[&(
            "auto".into(),
            "agent_route/v1|unknown|implement|normal".into(),
        )]
            .tiers["economy"];
        assert_eq!(tier.pass_rate_ppm(), 1_000_000);
        assert_eq!(tier.independent_tasks.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn explicit_zero_credit_withholds_single_decision_attribution() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let service = EvalService::new(store.clone(), EvalConfig::default());
        let subject = subject()?;
        store.insert_subject(&subject).await?;
        let mut result = result(&subject);
        result.verdict = EvalVerdict::Inconclusive;
        result.decision_credit.insert(
            "decision-a".into(),
            DecisionCredit {
                weight_ppm: 0,
                metric_ids: BTreeSet::from(["quality.pass".into()]),
            },
        );
        service
            .submit(result, SubmissionPrincipal::LocalOperator)
            .await?;
        let frozen = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;

        let routes = EvalEvidenceSnapshot::load(&store, &frozen.evidence_root)
            .await?
            .route_evidence()?;
        assert!(routes.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn explicit_credit_never_broadcasts_metrics_between_decisions() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = EvalStore::new(db);
        let service = EvalService::new(store.clone(), EvalConfig::default());
        let mut subject = subject()?;
        subject.decisions[0].route_projection =
            "agent_route/v1|code:generation|implement|normal".into();
        subject.decisions.push(EvalDecisionRef {
            decision_id: "decision-b".into(),
            policy: "auto".into(),
            route_projection: "agent_route/v1|unknown|verify|normal".into(),
            request_key: "agent_route/v1|unknown|verify|normal".into(),
            selected_tier: "strong".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            policy_digest: subject.policy_digest.clone(),
        });
        subject
            .requested_dimensions
            .insert("cost.usd_micros".into());
        store.insert_subject(&subject).await?;
        let mut result = result(&subject);
        result.metrics.insert(
            "cost.usd_micros".into(),
            MetricValue::new(420, MetricUnit::MicroUsd),
        );
        result.decision_credit.insert(
            "decision-a".into(),
            DecisionCredit {
                weight_ppm: 1_000_000,
                metric_ids: BTreeSet::from(["quality.pass".into()]),
            },
        );
        result.decision_credit.insert(
            "decision-b".into(),
            DecisionCredit {
                weight_ppm: 1_000_000,
                metric_ids: BTreeSet::from(["cost.usd_micros".into()]),
            },
        );
        service
            .submit(result, SubmissionPrincipal::LocalOperator)
            .await?;
        let frozen = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;

        let routes = EvalEvidenceSnapshot::load(&store, &frozen.evidence_root)
            .await?
            .route_evidence()?;
        let quality = &routes[&(
            "auto".into(),
            "agent_route/v1|code:generation|implement|normal".into(),
        )]
            .tiers["economy"];
        assert_eq!(quality.pass_rate_ppm(), 1_000_000);
        assert_eq!(quality.cost_micro_usd.mean(), None);
        assert_eq!(
            routes[&(
                "auto".into(),
                "agent_route/v1|code:generation|implement|normal".into(),
            )]
                .matched_request_keys,
            BTreeSet::from(["agent_route/v1|unknown|implement|normal".into()])
        );
        let cost = &routes[&("auto".into(), "agent_route/v1|unknown|verify|normal".into())].tiers["strong"];
        assert_eq!(cost.pass_rate_ppm(), 0);
        assert_eq!(cost.eligible_episodes, 0);
        assert_eq!(cost.cost_micro_usd.mean(), Some(420));
        Ok(())
    }

    fn subject() -> anyhow::Result<EvalSubject> {
        let evidence = Vec::new();
        Ok(EvalSubject {
            schema_version: 1,
            eval_id: "eval-compile".into(),
            scope: EvalScope::Task,
            subject_id: "task-a".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id: "decision-a".into(),
                policy: "auto".into(),
                route_projection: "agent_route/v1|unknown|implement|normal".into(),
                request_key: "agent_route/v1|unknown|implement|normal".into(),
                selected_tier: "economy".into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                policy_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "2026-07-30T00:00:00Z".into(),
        })
    }

    fn result(subject: &EvalSubject) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            eval_id: subject.eval_id.clone(),
            evidence_digest: subject.evidence_digest.clone(),
            evaluator: EvaluatorIdentity {
                authority_id: "local".into(),
                evaluator_id: "task-runner".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest:
                    "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            verdict: EvalVerdict::Pass,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "result-compile".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        }
    }
}
