//! Deterministic compilation of observed evidence into policy lock artifacts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::adequacy::reliability::ReliabilityEvent;
use crate::adequacy::store::{
    AdequacyStore, LegacyPin, PersistedExplorationState, PersistedReliabilityEvent,
    PersistedSemanticSuccess,
};
use crate::eval::compiler::{EvalEvidenceSnapshot, RouteEvalEvidence, TierEvalEvidence};
use crate::eval::types::EvaluatorKind;
use crate::policy_lock::{
    CertificateSource, CompilerIdentity, EconomicsSummary, LatencySummary, LegacyAdequacySummary,
    LegacyMigration, POLICY_COMPILER_ID, POLICY_COMPILER_VERSION, POLICY_LOCKFILE_VERSION,
    PolicyArtifact, PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict,
    QualitySummary, RouteOwner, semantic_digest, validate_document,
};
use crate::workflow_state::ir::{RouteProjection, WorkflowStateKind};

/// A point-in-time, ordered view of every pre-v2 learned-state table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyAdequacySnapshot {
    pub snapshot_time_unix_ms: i64,
    pub pins: Vec<LegacyPin>,
    pub exploration: Vec<PersistedExplorationState>,
    pub semantic_successes: Vec<PersistedSemanticSuccess>,
    pub reliability_events: Vec<PersistedReliabilityEvent>,
}

impl LegacyAdequacySnapshot {
    pub async fn load(store: &AdequacyStore, snapshot_time_unix_ms: i64) -> Result<Self> {
        if snapshot_time_unix_ms < 0 {
            anyhow::bail!("legacy snapshot time cannot be negative");
        }
        let mut pins = store.load_pins().await?;
        let mut exploration = store.load_exploration_all().await?;
        let mut semantic_successes = store.load_semantic_successes().await?;
        let mut reliability_events = store.load_reliability_events().await?;
        pins.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        exploration.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        semantic_successes.sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
        reliability_events
            .sort_by(|left, right| left.event.request_id.cmp(&right.event.request_id));
        Ok(Self {
            snapshot_time_unix_ms,
            pins,
            exploration,
            semantic_successes,
            reliability_events,
        })
    }

    pub fn semantic_digest(&self) -> Result<String> {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            pins: &'a [LegacyPin],
            exploration: &'a [PersistedExplorationState],
            semantic_successes: &'a [PersistedSemanticSuccess],
            reliability_events: Vec<&'a ReliabilityEvent>,
        }

        let canonical = serde_json::to_vec(&DigestInput {
            pins: &self.pins,
            exploration: &self.exploration,
            semantic_successes: &self.semantic_successes,
            reliability_events: self
                .reliability_events
                .iter()
                .map(|row| &row.event)
                .collect(),
        })
        .context("serializing legacy adequacy snapshot")?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
            && self.exploration.is_empty()
            && self.semantic_successes.is_empty()
            && self.reliability_events.is_empty()
    }
}

pub struct CompileInput<'a> {
    pub current: &'a PolicyLock,
    pub parent_digest: Option<&'a str>,
    pub legacy: &'a LegacyAdequacySnapshot,
    pub eval: Option<&'a EvalEvidenceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompileChange {
    pub policy: String,
    pub request_key: String,
    pub previous_tier: Option<String>,
    pub selected_tier: String,
    pub verdict: PromotionVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompileConflict {
    pub policy: String,
    pub request_key: String,
    pub operator_tier: String,
    pub recommended_tier: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct CompileResult {
    pub document: PolicyLock,
    pub changes: Vec<CompileChange>,
    pub conflicts: Vec<CompileConflict>,
}

#[derive(Serialize)]
struct CompilerConfigDigest {
    id: &'static str,
    version: u32,
    precedence: &'static str,
    minimum_quality_ppm: i64,
}

#[derive(Default)]
struct RouteEvidence<'a> {
    pin: Option<&'a LegacyPin>,
    exploration: Option<&'a PersistedExplorationState>,
    semantic_tasks: BTreeSet<&'a str>,
}

/// Compile a deterministic v2 candidate without mutating the active lock.
pub fn compile_candidate(input: CompileInput<'_>) -> Result<CompileResult> {
    validate_document(input.current)?;
    let legacy_evidence_root = input.legacy.semantic_digest()?;
    let evidence_root = match input.eval {
        Some(eval) => canonical_digest(&(
            "policy-evidence-v2",
            legacy_evidence_root.as_str(),
            eval.evidence_root.as_str(),
        ))?,
        None => legacy_evidence_root.clone(),
    };
    let eval_routes = match input.eval {
        Some(eval) => eval.route_evidence()?,
        None => BTreeMap::new(),
    };
    let compiler_config_digest = canonical_digest(&CompilerConfigDigest {
        id: POLICY_COMPILER_ID,
        version: POLICY_COMPILER_VERSION,
        precedence: "guardrail>operator>eval_negative>eval_positive>legacy_negative>legacy_positive>inherited",
        minimum_quality_ppm: 900_000,
    })?;
    let parent_digest = match input.parent_digest {
        Some(digest) => digest.to_string(),
        None => semantic_digest(input.current)?,
    };
    let mut document = PolicyLock {
        lockfile_version: POLICY_LOCKFILE_VERSION,
        artifact: Some(PolicyArtifact {
            parent_digest: Some(parent_digest),
            evidence_root: evidence_root.clone(),
            eval_snapshot_root: input.eval.map(|eval| eval.evidence_root.clone()),
            source_snapshot_time_unix_ms: source_snapshot_time(&input)?,
            migration: Some(LegacyMigration {
                legacy_adequacy_digest: legacy_evidence_root,
            }),
            compiler: CompilerIdentity {
                id: POLICY_COMPILER_ID.to_string(),
                version: POLICY_COMPILER_VERSION,
                config_digest: compiler_config_digest.clone(),
            },
        }),
        policies: input.current.policies.clone(),
        certificates: BTreeMap::new(),
    };
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();

    for (policy_name, policy) in &input.current.policies {
        let evidence = route_evidence(policy_name, input.legacy);
        let policy_eval = eval_routes
            .iter()
            .filter_map(|((eval_policy, request_key), route)| {
                (eval_policy == policy_name).then_some((request_key.clone(), route))
            })
            .collect::<BTreeMap<_, _>>();
        let mut request_keys = policy.routes.keys().cloned().collect::<BTreeSet<_>>();
        request_keys.extend(evidence.keys().cloned());
        request_keys.extend(policy_eval.keys().cloned());
        let mut compiled_routes = BTreeMap::new();
        let mut certificates = BTreeMap::new();

        for request_key in request_keys {
            let prior_tier = policy.routes.get(&request_key).cloned();
            let route_evidence = evidence.get(&request_key);
            let prior_certificate = input.current.certificate(policy_name, &request_key);
            let owner = route_owner(
                input.current,
                policy,
                prior_tier.as_deref(),
                prior_certificate,
                route_evidence,
            );
            let active_pin = route_evidence.and_then(|item| item.pin).is_some_and(|pin| {
                pin_is_active(
                    pin,
                    policy.adequacy.pin_cooldown_secs,
                    input.legacy.snapshot_time_unix_ms,
                )
            });
            let semantic_successes = match route_evidence.map(|item| item.semantic_tasks.len()) {
                Some(count) => u32::try_from(count).unwrap_or(u32::MAX),
                None => 0,
            };
            let positive = route_evidence
                .and_then(|item| item.exploration)
                .is_some_and(|row| {
                    row.locked
                        && positive_route_is_allowed(policy, request_key.as_str())
                        && semantic_successes >= semantic_threshold(policy, request_key.as_str())
                });
            let escalation_tier = policy
                .adequacy
                .escalation_tier
                .as_deref()
                .or(policy.default_tier.as_deref());
            let explore_tier = policy.adequacy.explore_tier.as_deref();
            let eval_evidence = policy_eval.get(&request_key).copied();
            let eval_recommendation = eval_evidence.and_then(|route| {
                eval_recommendation(policy, &request_key, prior_tier.as_deref(), route)
            });
            let legacy_recommendation = if active_pin {
                escalation_tier.map(|tier| (tier.to_string(), PromotionVerdict::Demote))
            } else if positive {
                explore_tier.map(|tier| (tier.to_string(), PromotionVerdict::Promote))
            } else {
                None
            };
            let recommendation = eval_recommendation
                .map(|(tier, verdict)| {
                    (
                        tier,
                        verdict,
                        eval_evidence
                            .map(eval_certificate_source)
                            .unwrap_or(CertificateSource::Mixed),
                        false,
                        true,
                    )
                })
                .or_else(|| {
                    legacy_recommendation.map(|(tier, verdict)| {
                        (
                            tier,
                            verdict,
                            CertificateSource::LegacyAdequacyV1,
                            true,
                            false,
                        )
                    })
                });

            let (selected_tier, selected_owner, source, verdict, uses_legacy, uses_eval) =
                match (prior_tier.as_deref(), owner, recommendation) {
                    (Some(prior), RouteOwner::Operator, Some((recommended, _, _, _, _)))
                        if prior != recommended =>
                    {
                        conflicts.push(CompileConflict {
                            policy: policy_name.clone(),
                            request_key: request_key.clone(),
                            operator_tier: prior.to_string(),
                            recommended_tier: recommended,
                            reason: "admitted evidence conflicts with an operator-owned route"
                                .into(),
                        });
                        (
                            prior.to_string(),
                            RouteOwner::Operator,
                            CertificateSource::Operator,
                            PromotionVerdict::Blocked,
                            false,
                            false,
                        )
                    }
                    (Some(prior), RouteOwner::Operator, _) => (
                        prior.to_string(),
                        RouteOwner::Operator,
                        CertificateSource::Operator,
                        PromotionVerdict::Retain,
                        false,
                        false,
                    ),
                    (
                        _,
                        RouteOwner::Compiler,
                        Some((recommended, verdict, source, legacy, eval)),
                    ) => (
                        recommended,
                        RouteOwner::Compiler,
                        source,
                        verdict,
                        legacy,
                        eval,
                    ),
                    (Some(prior), RouteOwner::Compiler, None) => {
                        let source = prior_certificate
                            .map(|certificate| certificate.source)
                            .unwrap_or(CertificateSource::LegacyAdequacyV1);
                        (
                            prior.to_string(),
                            RouteOwner::Compiler,
                            source,
                            PromotionVerdict::Retain,
                            prior_certificate.is_none(),
                            false,
                        )
                    }
                    (None, _, Some((recommended, verdict, source, legacy, eval))) => (
                        recommended,
                        RouteOwner::Compiler,
                        source,
                        verdict,
                        legacy,
                        eval,
                    ),
                    (None, _, None) => continue,
                };

            if prior_tier.as_deref() != Some(selected_tier.as_str()) {
                changes.push(CompileChange {
                    policy: policy_name.clone(),
                    request_key: request_key.clone(),
                    previous_tier: prior_tier.clone(),
                    selected_tier: selected_tier.clone(),
                    verdict,
                });
            }
            compiled_routes.insert(request_key.clone(), selected_tier.clone());
            if !uses_legacy
                && !uses_eval
                && selected_owner == RouteOwner::Compiler
                && let Some(prior) = prior_certificate
            {
                let mut certificate = prior.clone();
                certificate.selected_tier = selected_tier;
                certificate.verdict = verdict;
                certificate.compiler_config_digest = compiler_config_digest.clone();
                certificates.insert(request_key, certificate);
                continue;
            }
            let evidence_digest = if uses_eval {
                eval_route_evidence_digest(policy_name, &request_key, eval_evidence)?
            } else if uses_legacy {
                route_evidence_digest(policy_name, &request_key, route_evidence)?
            } else {
                match prior_certificate {
                    Some(certificate) => certificate.evidence_digest.clone(),
                    None => {
                        canonical_digest(&("operator", policy_name, &request_key, &selected_tier))?
                    }
                }
            };
            let legacy = uses_legacy.then(|| LegacyAdequacySummary {
                observed: route_evidence
                    .and_then(|item| item.exploration)
                    .map(|row| row.observed)
                    .unwrap_or_default(),
                adequate_trials: route_evidence
                    .and_then(|item| item.exploration)
                    .map(|row| row.adequate_trials)
                    .unwrap_or_default(),
                semantic_successes,
                pinned: active_pin,
            });
            let eval_tier = uses_eval
                .then(|| eval_evidence.and_then(|route| route.tiers.get(&selected_tier)))
                .flatten();
            let baseline_tier = eval_evidence
                .and_then(|route| route.baseline_tier.clone())
                .or_else(|| policy.default_tier.clone());
            let baseline_eval = eval_evidence.and_then(|route| {
                baseline_tier
                    .as_deref()
                    .and_then(|tier| route.tiers.get(tier))
            });
            certificates.insert(
                request_key,
                PolicyCertificate {
                    owner: selected_owner,
                    selected_tier,
                    baseline_tier,
                    source,
                    eligible_episodes: eval_tier.map_or_else(
                        || {
                            legacy
                                .as_ref()
                                .map(|summary| summary.adequate_trials)
                                .unwrap_or_default()
                        },
                        |tier| tier.eligible_episodes,
                    ),
                    independent_tasks: eval_tier.map_or_else(
                        || {
                            legacy
                                .as_ref()
                                .map(|_| semantic_successes)
                                .unwrap_or_default()
                        },
                        |tier| u32::try_from(tier.independent_tasks.len()).unwrap_or(u32::MAX),
                    ),
                    quality: eval_tier.map(|tier| quality_summary(tier, baseline_eval)),
                    economics: metric_delta_summary(eval_tier, baseline_eval, |tier| {
                        tier.cost_micro_usd.mean()
                    })
                    .map(|normalized_cost_delta_ppm| EconomicsSummary {
                        normalized_cost_delta_ppm,
                    }),
                    latency: metric_delta_summary(eval_tier, baseline_eval, |tier| {
                        tier.latency_ms.mean()
                    })
                    .map(|normalized_latency_delta_ppm| LatencySummary {
                        normalized_latency_delta_ppm,
                    }),
                    critical_violations: eval_tier.map_or_else(
                        || u32::from(uses_legacy && active_pin),
                        |tier| tier.critical_violations,
                    ),
                    verdict,
                    evaluator_config_digest: uses_eval
                        .then(|| {
                            eval_evidence
                                .map(|route| canonical_digest(&route.evaluator_config_digests))
                        })
                        .flatten()
                        .transpose()?,
                    compiler_config_digest: compiler_config_digest.clone(),
                    evidence_digest,
                    legacy,
                },
            );
        }

        if let Some(compiled_policy) = document.policies.get_mut(policy_name) {
            compiled_policy.routes = compiled_routes;
        }
        if !certificates.is_empty() {
            document
                .certificates
                .insert(policy_name.clone(), certificates);
        }
    }

    validate_document(&document)?;
    Ok(CompileResult {
        document,
        changes,
        conflicts,
    })
}

fn source_snapshot_time(input: &CompileInput<'_>) -> Result<i64> {
    let eval_time = match input.eval {
        Some(eval) => chrono::DateTime::parse_from_rfc3339(&eval.frozen_at)
            .context("eval snapshot frozen_at must be RFC3339")?
            .timestamp_millis(),
        None => 0,
    };
    Ok(input.legacy.snapshot_time_unix_ms.max(eval_time))
}

fn eval_recommendation(
    policy: &PolicyDefinition,
    request_key: &str,
    prior_tier: Option<&str>,
    route: &RouteEvalEvidence,
) -> Option<(String, PromotionVerdict)> {
    let baseline = route
        .baseline_tier
        .as_deref()
        .or(policy.default_tier.as_deref());
    if let (Some(prior), Some(baseline)) = (prior_tier, baseline)
        && prior != baseline
        && let Some(active) = route.tiers.get(prior)
        && (active.critical_violations > 0
            || (active.fail_weight_ppm > 0 && active.pass_rate_ppm() < 900_000))
    {
        return Some((baseline.to_string(), PromotionVerdict::Demote));
    }
    if !positive_route_is_allowed(policy, request_key) {
        return None;
    }
    let baseline_pass_rate = baseline
        .and_then(|tier| route.tiers.get(tier))
        .map(TierEvalEvidence::pass_rate_ppm);
    let minimum_tasks = semantic_threshold(policy, request_key).max(1);
    route
        .tiers
        .iter()
        .filter(|(tier, evidence)| {
            Some(tier.as_str()) != baseline
                && policy.tiers.contains_key(tier.as_str())
                && u32::try_from(evidence.independent_tasks.len()).unwrap_or(u32::MAX)
                    >= minimum_tasks
                && evidence.critical_violations == 0
                && evidence.pass_rate_ppm() >= 900_000
                && baseline_pass_rate
                    .is_none_or(|baseline_rate| evidence.pass_rate_ppm() >= baseline_rate)
        })
        .max_by(|(left_tier, left), (right_tier, right)| {
            left.independent_tasks
                .len()
                .cmp(&right.independent_tasks.len())
                .then_with(|| left.pass_rate_ppm().cmp(&right.pass_rate_ppm()))
                .then_with(|| right_tier.cmp(left_tier))
        })
        .map(|(tier, _)| (tier.clone(), PromotionVerdict::Promote))
}

fn eval_certificate_source(route: &RouteEvalEvidence) -> CertificateSource {
    if route.sources.len() != 1 {
        return CertificateSource::Mixed;
    }
    match route.sources.iter().next() {
        Some(EvaluatorKind::TaskNative) => CertificateSource::TaskNative,
        Some(EvaluatorKind::Human) => CertificateSource::Human,
        Some(EvaluatorKind::Enterprise) => CertificateSource::Enterprise,
        Some(EvaluatorKind::Agentic) => CertificateSource::Agentic,
        Some(EvaluatorKind::Generic) | None => CertificateSource::Mixed,
    }
}

fn quality_summary(
    candidate: &TierEvalEvidence,
    baseline: Option<&TierEvalEvidence>,
) -> QualitySummary {
    let candidate_pass_rate_ppm = candidate.pass_rate_ppm();
    let baseline_pass_rate_ppm = baseline
        .map(TierEvalEvidence::pass_rate_ppm)
        .unwrap_or_default();
    QualitySummary {
        baseline_pass_rate_ppm,
        candidate_pass_rate_ppm,
        delta_ppm: candidate_pass_rate_ppm.saturating_sub(baseline_pass_rate_ppm),
        lower_bound_ppm: candidate_pass_rate_ppm,
    }
}

fn metric_delta_summary(
    candidate: Option<&TierEvalEvidence>,
    baseline: Option<&TierEvalEvidence>,
    value: impl Fn(&TierEvalEvidence) -> Option<i64>,
) -> Option<i64> {
    let candidate = candidate.and_then(&value)?;
    let baseline = baseline.and_then(value)?;
    if baseline == 0 {
        return None;
    }
    Some(candidate.saturating_sub(baseline).saturating_mul(1_000_000) / baseline)
}

fn eval_route_evidence_digest(
    policy_name: &str,
    request_key: &str,
    evidence: Option<&RouteEvalEvidence>,
) -> Result<String> {
    canonical_digest(&(
        policy_name,
        request_key,
        evidence.map(|route| &route.evidence_records),
    ))
}

fn route_evidence<'a>(
    policy_name: &str,
    legacy: &'a LegacyAdequacySnapshot,
) -> BTreeMap<String, RouteEvidence<'a>> {
    let mut evidence = BTreeMap::<String, RouteEvidence<'a>>::new();
    for pin in &legacy.pins {
        if let Some(request_key) = legacy_request_key(policy_name, &pin.fingerprint) {
            evidence.entry(request_key).or_default().pin = Some(pin);
        }
    }
    for row in &legacy.exploration {
        if let Some(request_key) = legacy_request_key(policy_name, &row.fingerprint) {
            evidence.entry(request_key).or_default().exploration = Some(row);
        }
    }
    for row in &legacy.semantic_successes {
        if let Some(request_key) = legacy_request_key(policy_name, &row.fingerprint) {
            evidence
                .entry(request_key)
                .or_default()
                .semantic_tasks
                .insert(&row.task_id);
        }
    }
    evidence
}

fn legacy_request_key(policy_name: &str, fingerprint: &str) -> Option<String> {
    let (namespace, request_key) = fingerprint.split_once('\0')?;
    (namespace == policy_name && RouteProjection::parse_key(request_key).is_some())
        .then(|| request_key.to_string())
}

fn route_owner(
    current: &PolicyLock,
    policy: &PolicyDefinition,
    prior_tier: Option<&str>,
    prior_certificate: Option<&PolicyCertificate>,
    evidence: Option<&RouteEvidence<'_>>,
) -> RouteOwner {
    if current.is_v2() {
        return prior_certificate
            .map(|certificate| certificate.owner)
            .unwrap_or(RouteOwner::Compiler);
    }
    let inferred_compiler = prior_tier.is_some_and(|tier| {
        policy.adequacy.explore_tier.as_deref() == Some(tier)
            && evidence
                .and_then(|item| item.exploration)
                .is_some_and(|row| row.locked)
    });
    if inferred_compiler {
        RouteOwner::Compiler
    } else {
        RouteOwner::Operator
    }
}

fn pin_is_active(pin: &LegacyPin, cooldown_secs: u64, snapshot_time_unix_ms: i64) -> bool {
    if cooldown_secs == 0 {
        return true;
    }
    let snapshot_secs = snapshot_time_unix_ms / 1_000;
    let cooldown = i64::try_from(cooldown_secs).unwrap_or(i64::MAX);
    pin.pinned_at_unix.saturating_add(cooldown) > snapshot_secs
}

fn semantic_threshold(policy: &PolicyDefinition, request_key: &str) -> u32 {
    let opening = RouteProjection::parse_key(request_key)
        .is_some_and(|projection| projection.state_kind == WorkflowStateKind::Opening);
    if opening {
        policy
            .adequacy
            .min_semantic_successes_for_lock
            .max(policy.adequacy.min_semantic_successes_for_opening)
    } else {
        policy.adequacy.min_semantic_successes_for_lock
    }
}

fn positive_route_is_allowed(policy: &PolicyDefinition, request_key: &str) -> bool {
    RouteProjection::parse_key(request_key).is_some_and(|projection| {
        projection.state_kind != WorkflowStateKind::Opening || policy.adequacy.explore_opening
    })
}

fn route_evidence_digest(
    policy_name: &str,
    request_key: &str,
    evidence: Option<&RouteEvidence<'_>>,
) -> Result<String> {
    #[derive(Serialize)]
    struct DigestInput<'a> {
        policy: &'a str,
        request_key: &'a str,
        pin: Option<&'a LegacyPin>,
        exploration: Option<&'a PersistedExplorationState>,
        semantic_tasks: Vec<&'a str>,
    }
    canonical_digest(&DigestInput {
        policy: policy_name,
        request_key,
        pin: evidence.and_then(|item| item.pin),
        exploration: evidence.and_then(|item| item.exploration),
        semantic_tasks: evidence
            .map(|item| item.semantic_tasks.iter().copied().collect())
            .unwrap_or_default(),
    })
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing canonical compiler input")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bitrouter_sdk::config::AdequacyConfig;

    use crate::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
    use crate::adequacy::store::AdequacyStore;
    use crate::adequacy::store::{LegacyPin, PersistedExplorationState, PersistedSemanticSuccess};
    use crate::db;
    use crate::eval::compiler::{EvalEvidenceRecord, EvalEvidenceSnapshot};
    use crate::eval::types::{
        EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict, EvaluationResult, EvaluatorIdentity,
        EvaluatorKind, evidence_digest,
    };
    use crate::policy_lock::{
        CertificateSource, PolicyDefinition, PolicyLock, PromotionVerdict, RouteOwner,
        deterministic_yaml,
    };

    const EDIT_KEY: &str = "agent_trace/v1|edit|normal";

    fn policy(route: Option<&str>) -> PolicyDefinition {
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "vendor:economy".into()),
                ("strong".into(), "vendor:strong".into()),
            ]),
            routes: route
                .map(|tier| BTreeMap::from([(EDIT_KEY.to_string(), tier.to_string())]))
                .unwrap_or_default(),
            default_tier: Some("strong".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            adequacy: AdequacyConfig {
                escalation_tier: Some("strong".into()),
                explore_tier: Some("economy".into()),
                min_semantic_successes_for_lock: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn v1(route: Option<&str>) -> PolicyLock {
        PolicyLock {
            lockfile_version: 1,
            artifact: None,
            policies: BTreeMap::from([("auto".into(), policy(route))]),
            certificates: BTreeMap::new(),
        }
    }

    fn snapshot(positive: bool, pinned_at_unix: Option<i64>) -> super::LegacyAdequacySnapshot {
        let fingerprint = format!("auto\0{EDIT_KEY}");
        super::LegacyAdequacySnapshot {
            snapshot_time_unix_ms: 1_700_000_100_000,
            pins: pinned_at_unix
                .map(|timestamp| {
                    vec![LegacyPin {
                        fingerprint: fingerprint.clone(),
                        pinned_at_unix: timestamp,
                    }]
                })
                .unwrap_or_default(),
            exploration: if positive {
                vec![PersistedExplorationState {
                    fingerprint: fingerprint.clone(),
                    observed: 8,
                    adequate_trials: 4,
                    locked: true,
                }]
            } else {
                Vec::new()
            },
            semantic_successes: if positive {
                vec![PersistedSemanticSuccess {
                    evidence_id: format!("{fingerprint}\ntask-a"),
                    fingerprint,
                    task_id: "task-a".into(),
                }]
            } else {
                Vec::new()
            },
            reliability_events: Vec::new(),
        }
    }

    async fn populated_store(order: [&str; 2]) -> anyhow::Result<AdequacyStore> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let store = AdequacyStore::new(db);
        for key in order {
            let fingerprint = format!("auto\0agent_trace/v1|{key}|normal");
            store.upsert_pin(&fingerprint, 1_700_000_000).await?;
            store.upsert_exploration(&fingerprint, 8, 4, true).await?;
            store
                .record_semantic_success(&fingerprint, &format!("task-{key}"))
                .await?;
            store
                .append_reliability_event(&ReliabilityEvent {
                    request_id: format!("request-{key}"),
                    route_key: fingerprint,
                    endpoint_key: ReliabilityKey {
                        provider: "provider".into(),
                        model: format!("model-{key}"),
                        credential_class: "shared".into(),
                        endpoint_scope: "default".into(),
                        protocol: "responses".into(),
                    },
                    observation: ReliabilityObservation::Success,
                    half_open_probe: false,
                    observed_at_unix: 1_700_000_001,
                })
                .await?;
        }
        Ok(store)
    }

    #[tokio::test]
    async fn legacy_snapshot_digest_is_order_independent_and_complete() -> anyhow::Result<()> {
        let first = populated_store(["edit", "test"]).await?;
        let second = populated_store(["test", "edit"]).await?;

        let left = super::LegacyAdequacySnapshot::load(&first, 1_785_369_600_000).await?;
        let right = super::LegacyAdequacySnapshot::load(&second, 1_785_369_600_000).await?;

        assert_eq!(left.semantic_digest()?, right.semantic_digest()?);
        assert!(!left.is_empty());
        let mut incomplete = left.clone();
        incomplete.reliability_events.clear();
        assert_ne!(left.semantic_digest()?, incomplete.semantic_digest()?);
        Ok(())
    }

    #[test]
    fn active_pin_replaces_a_compiler_owned_economy_route() -> anyhow::Result<()> {
        let learned = super::compile_candidate(super::CompileInput {
            current: &v1(None),
            parent_digest: None,
            legacy: &snapshot(true, None),
            eval: None,
        })?;
        let corrected = super::compile_candidate(super::CompileInput {
            current: &learned.document,
            parent_digest: None,
            legacy: &snapshot(true, Some(1_700_000_000)),
            eval: None,
        })?;

        assert_eq!(
            corrected.document.policies["auto"].routes[EDIT_KEY],
            "strong"
        );
        let certificate = &corrected.document.certificates["auto"][EDIT_KEY];
        assert_eq!(certificate.source, CertificateSource::LegacyAdequacyV1);
        assert_eq!(certificate.verdict, PromotionVerdict::Demote);
        assert!(certificate.legacy.as_ref().is_some_and(|item| item.pinned));
        Ok(())
    }

    #[test]
    fn negative_evidence_conflicting_with_operator_route_blocks_publication() -> anyhow::Result<()>
    {
        let result = super::compile_candidate(super::CompileInput {
            current: &v1(Some("economy")),
            parent_digest: None,
            legacy: &snapshot(false, Some(1_700_000_000)),
            eval: None,
        })?;

        assert_eq!(result.document.policies["auto"].routes[EDIT_KEY], "economy");
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(
            result.document.certificates["auto"][EDIT_KEY].source,
            CertificateSource::Operator
        );
        Ok(())
    }

    #[test]
    fn admitted_negative_evidence_demotes_pretrained_economy_route() -> anyhow::Result<()> {
        let template_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("templates/auto-router/policy-lock.yaml");
        let template_raw = std::fs::read_to_string(template_path)?;
        let current: PolicyLock = serde_saphyr::from_str(&template_raw)?;
        let evidence = Vec::new();
        let evidence_digest = evidence_digest(&evidence)?;
        let subject = EvalSubject {
            schema_version: 1,
            eval_id: "eval-pretrained-demotion".into(),
            scope: EvalScope::Task,
            subject_id: "task-pretrained-demotion".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: vec![EvalDecisionRef {
                decision_id: "decision-pretrained-demotion".into(),
                policy: "auto".into(),
                request_key: EDIT_KEY.into(),
                selected_tier: "economy".into(),
                baseline_tier: Some("strong".into()),
                policy_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence,
            evidence_digest: evidence_digest.clone(),
            observed_at: "2026-07-30T00:00:00Z".into(),
        };
        let result = EvaluationResult {
            schema_version: 1,
            eval_id: subject.eval_id.clone(),
            evidence_digest,
            evaluator: EvaluatorIdentity {
                authority_id: "task-native".into(),
                evaluator_id: "suite".into(),
                kind: EvaluatorKind::TaskNative,
                version: "1".into(),
                config_digest:
                    "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            verdict: EvalVerdict::Fail,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: Some(1_000_000),
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "result-pretrained-demotion".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        };
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            frozen_at: "2026-07-30T00:02:00Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                content_digest:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                subject,
                result,
            }],
        };

        let compiled = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &snapshot(false, None),
            eval: Some(&eval),
        })?;

        assert_eq!(
            compiled.document.policies["auto"].routes[EDIT_KEY],
            "strong"
        );
        assert!(compiled.conflicts.is_empty());
        let certificate = &compiled.document.certificates["auto"][EDIT_KEY];
        assert_eq!(certificate.owner, RouteOwner::Compiler);
        assert_eq!(certificate.source, CertificateSource::TaskNative);
        assert_eq!(certificate.verdict, PromotionVerdict::Demote);
        Ok(())
    }

    #[test]
    fn compiler_is_deterministic_and_reliability_never_changes_routes() -> anyhow::Result<()> {
        let current = v1(None);
        let evidence = snapshot(false, None);
        let left = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &evidence,
            eval: None,
        })?;
        let right = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &evidence,
            eval: None,
        })?;

        assert_eq!(left.document, right.document);
        assert_eq!(
            deterministic_yaml(&left.document)?,
            deterministic_yaml(&right.document)?
        );
        assert!(left.document.policies["auto"].routes.is_empty());
        Ok(())
    }

    #[test]
    fn opening_guardrail_blocks_positive_legacy_promotion() -> anyhow::Result<()> {
        let opening_key = "agent_trace/v1|opening|normal";
        let fingerprint = format!("auto\0{opening_key}");
        let mut current = v1(None);
        current
            .policies
            .get_mut("auto")
            .ok_or_else(|| anyhow::anyhow!("test fixture is missing policy auto"))?
            .adequacy
            .min_semantic_successes_for_opening = 1;
        let evidence = super::LegacyAdequacySnapshot {
            snapshot_time_unix_ms: 1_700_000_100_000,
            pins: Vec::new(),
            exploration: vec![PersistedExplorationState {
                fingerprint: fingerprint.clone(),
                observed: 8,
                adequate_trials: 4,
                locked: true,
            }],
            semantic_successes: vec![PersistedSemanticSuccess {
                evidence_id: format!("{fingerprint}\ntask-a"),
                fingerprint,
                task_id: "task-a".into(),
            }],
            reliability_events: Vec::new(),
        };

        let result = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &evidence,
            eval: None,
        })?;

        assert!(
            !result.document.policies["auto"]
                .routes
                .contains_key(opening_key)
        );
        Ok(())
    }

    #[test]
    fn admitted_generic_eval_promotes_a_qualified_candidate() -> anyhow::Result<()> {
        let evidence = Vec::new();
        let evidence_digest = evidence_digest(&evidence)?;
        let subject = EvalSubject {
            schema_version: 1,
            eval_id: "eval-policy".into(),
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
                request_key: EDIT_KEY.into(),
                selected_tier: "economy".into(),
                baseline_tier: Some("strong".into()),
                policy_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            }],
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence,
            evidence_digest: evidence_digest.clone(),
            observed_at: "2026-07-30T00:00:00Z".into(),
        };
        let result = EvaluationResult {
            schema_version: 1,
            eval_id: subject.eval_id.clone(),
            evidence_digest,
            evaluator: EvaluatorIdentity {
                authority_id: "task-native".into(),
                evaluator_id: "suite".into(),
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
            idempotency_key: "result-policy".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        };
        let eval = EvalEvidenceSnapshot {
            evidence_root:
                "sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            frozen_at: "2026-07-30T00:02:00Z".into(),
            records: vec![EvalEvidenceRecord {
                result_id:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                content_digest:
                    "sha256:3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                subject,
                result,
            }],
        };

        let compiled = super::compile_candidate(super::CompileInput {
            current: &v1(None),
            parent_digest: None,
            legacy: &snapshot(false, None),
            eval: Some(&eval),
        })?;

        assert_eq!(
            compiled.document.policies["auto"].routes[EDIT_KEY],
            "economy"
        );
        let certificate = &compiled.document.certificates["auto"][EDIT_KEY];
        assert_eq!(certificate.source, CertificateSource::TaskNative);
        assert_eq!(certificate.verdict, PromotionVerdict::Promote);
        assert_eq!(
            certificate
                .quality
                .as_ref()
                .map(|q| q.candidate_pass_rate_ppm),
            Some(1_000_000)
        );
        Ok(())
    }
}
