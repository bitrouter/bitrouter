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
use crate::policy_lock::{
    CertificateSource, CompilerIdentity, LegacyAdequacySummary, LegacyMigration,
    POLICY_COMPILER_ID, POLICY_COMPILER_VERSION, POLICY_LOCKFILE_VERSION, PolicyArtifact,
    PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict, RouteOwner, semantic_digest,
    validate_document,
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
            snapshot_time_unix_ms: i64,
            pins: &'a [LegacyPin],
            exploration: &'a [PersistedExplorationState],
            semantic_successes: &'a [PersistedSemanticSuccess],
            reliability_events: Vec<&'a ReliabilityEvent>,
        }

        let canonical = serde_json::to_vec(&DigestInput {
            snapshot_time_unix_ms: self.snapshot_time_unix_ms,
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
    let evidence_root = input.legacy.semantic_digest()?;
    let compiler_config_digest = canonical_digest(&CompilerConfigDigest {
        id: POLICY_COMPILER_ID,
        version: POLICY_COMPILER_VERSION,
        precedence: "guardrail>operator>negative>positive>inherited",
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
            source_snapshot_time_unix_ms: input.legacy.snapshot_time_unix_ms,
            migration: Some(LegacyMigration {
                legacy_adequacy_digest: evidence_root,
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
        let mut request_keys = policy.routes.keys().cloned().collect::<BTreeSet<_>>();
        request_keys.extend(evidence.keys().cloned());
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
            let recommendation = if active_pin {
                escalation_tier.map(|tier| (tier, PromotionVerdict::Demote))
            } else if positive {
                explore_tier.map(|tier| (tier, PromotionVerdict::Promote))
            } else {
                None
            };

            let (selected_tier, selected_owner, source, verdict, uses_legacy) =
                match (prior_tier.as_deref(), owner, recommendation) {
                    (Some(prior), RouteOwner::Operator, Some((recommended, _)))
                        if active_pin && prior != recommended =>
                    {
                        conflicts.push(CompileConflict {
                            policy: policy_name.clone(),
                            request_key: request_key.clone(),
                            operator_tier: prior.to_string(),
                            recommended_tier: recommended.to_string(),
                            reason:
                                "active negative evidence conflicts with an operator-owned route"
                                    .into(),
                        });
                        (
                            prior.to_string(),
                            RouteOwner::Operator,
                            CertificateSource::Operator,
                            PromotionVerdict::Blocked,
                            false,
                        )
                    }
                    (Some(prior), RouteOwner::Operator, _) => (
                        prior.to_string(),
                        RouteOwner::Operator,
                        CertificateSource::Operator,
                        PromotionVerdict::Retain,
                        false,
                    ),
                    (_, RouteOwner::Compiler, Some((recommended, verdict))) => (
                        recommended.to_string(),
                        RouteOwner::Compiler,
                        CertificateSource::LegacyAdequacyV1,
                        verdict,
                        true,
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
                        )
                    }
                    (None, _, Some((recommended, verdict))) => (
                        recommended.to_string(),
                        RouteOwner::Compiler,
                        CertificateSource::LegacyAdequacyV1,
                        verdict,
                        true,
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
            let evidence_digest = if uses_legacy {
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
            certificates.insert(
                request_key,
                PolicyCertificate {
                    owner: selected_owner,
                    selected_tier,
                    baseline_tier: policy.default_tier.clone(),
                    source,
                    eligible_episodes: legacy
                        .as_ref()
                        .map(|summary| summary.adequate_trials)
                        .unwrap_or_default(),
                    independent_tasks: legacy
                        .as_ref()
                        .map(|_| semantic_successes)
                        .unwrap_or_default(),
                    quality: None,
                    economics: None,
                    latency: None,
                    critical_violations: u32::from(uses_legacy && active_pin),
                    verdict,
                    evaluator_config_digest: None,
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
    let cooldown = match i64::try_from(cooldown_secs) {
        Ok(value) => value,
        Err(_) => i64::MAX,
    };
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
    use std::collections::BTreeMap;

    use bitrouter_sdk::config::AdequacyConfig;

    use crate::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
    use crate::adequacy::store::AdequacyStore;
    use crate::adequacy::store::{LegacyPin, PersistedExplorationState, PersistedSemanticSuccess};
    use crate::db;
    use crate::policy_lock::{
        CertificateSource, PolicyDefinition, PolicyLock, PromotionVerdict, deterministic_yaml,
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
            exploration: positive
                .then(|| {
                    vec![PersistedExplorationState {
                        fingerprint: fingerprint.clone(),
                        observed: 8,
                        adequate_trials: 4,
                        locked: true,
                    }]
                })
                .unwrap_or_default(),
            semantic_successes: positive
                .then(|| {
                    vec![PersistedSemanticSuccess {
                        evidence_id: format!("{fingerprint}\ntask-a"),
                        fingerprint,
                        task_id: "task-a".into(),
                    }]
                })
                .unwrap_or_default(),
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
        })?;
        let corrected = super::compile_candidate(super::CompileInput {
            current: &learned.document,
            parent_digest: None,
            legacy: &snapshot(true, Some(1_700_000_000)),
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
    fn compiler_is_deterministic_and_reliability_never_changes_routes() -> anyhow::Result<()> {
        let current = v1(None);
        let evidence = snapshot(false, None);
        let left = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &evidence,
        })?;
        let right = super::compile_candidate(super::CompileInput {
            current: &current,
            parent_digest: None,
            legacy: &evidence,
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
        })?;

        assert!(
            !result.document.policies["auto"]
                .routes
                .contains_key(opening_key)
        );
        Ok(())
    }
}
