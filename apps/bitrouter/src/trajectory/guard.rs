use std::collections::BTreeSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::types::{HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind, TrajectorySnapshot};
use crate::workflow_state::ir::{RouteProjection, WorkflowStateKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteHistoryAction {
    Observe,
    Escalate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressGuardPolicy {
    pub escalation_tier: String,
    pub protected_tiers: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_unprotected: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_same_projection_unprotected: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_recovery_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_episode_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_episode_elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_episode_cost_micro_usd: Option<u64>,
    pub hold_for_requests: u64,
    pub incomplete_history: IncompleteHistoryAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteIntentClauseDisposition {
    Applied,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteIntentClause {
    pub clause_id: String,
    pub disposition: RouteIntentClauseDisposition,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteIntent {
    pub candidate_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub clauses: Vec<RouteIntentClause>,
    /// Digest of the reducer snapshot through the current RequestStarted and
    /// before this intent. This makes the input auditable without a digest
    /// cycle through the selected tier.
    pub trajectory_snapshot_digest: String,
    pub policy_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardEvaluation {
    pub intent: RouteIntent,
    pub causal_completeness: HistoryCompleteness,
    /// True only for a new trigger. An already-active hold escalates without
    /// re-activating or extending itself.
    pub activated: bool,
}

/// Pure, reducer-derived input at the routing boundary.
pub struct ProgressGuardInput<'a> {
    pub prior_snapshot: Option<&'a TrajectorySnapshot>,
    pub pre_intent_snapshot: &'a TrajectorySnapshot,
    pub correlation_completeness: HistoryCompleteness,
    pub current_projection: &'a RouteProjection,
    pub candidate_tier: Option<&'a str>,
    pub policy_digest: &'a str,
}

pub fn evaluate(
    policy: &ProgressGuardPolicy,
    input: ProgressGuardInput<'_>,
) -> Result<GuardEvaluation> {
    validate_policy_shape(policy)?;
    let candidate_is_protected = input
        .candidate_tier
        .is_some_and(|tier| policy.protected_tiers.contains(tier));
    let prior_health = input.prior_snapshot.map(|snapshot| &snapshot.health);
    let hold_active = input
        .prior_snapshot
        .is_some_and(|snapshot| snapshot.active_hold_remaining > 0);
    let projection = input.current_projection.key();
    let prospective_same_projection_unprotected = if candidate_is_protected {
        0
    } else {
        match prior_health {
            Some(health) if health.latest_projection.as_deref() == Some(projection.as_str()) => {
                health
                    .same_projection_streak
                    .min(health.consecutive_unprotected_requests)
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("same projection streak overflow"))?
            }
            _ => 1,
        }
    };
    let prospective_unprotected = if candidate_is_protected {
        0
    } else {
        prior_health
            .map_or(0, |health| health.consecutive_unprotected_requests)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("consecutive unprotected streak overflow"))?
    };
    let prospective_recovery_count = prior_health
        .map_or(0, |health| health.recovery_count)
        .checked_add(u64::from(
            input.current_projection.state_kind == WorkflowStateKind::Recovery,
        ))
        .ok_or_else(|| anyhow::anyhow!("recovery count overflow"))?;
    let causal_completeness = merge_completeness(
        prior_health.map_or(HistoryCompleteness::Complete, |health| health.completeness),
        input.correlation_completeness,
    );

    let mut clauses = Vec::new();
    clauses.push(clause(
        "progress_guard.active_hold",
        hold_active,
        candidate_is_protected,
        "persisted hold is active for this request",
        "no persisted hold applies to this request",
    ));

    let incomplete = causal_completeness != HistoryCompleteness::Complete;
    let incomplete_trigger =
        incomplete && policy.incomplete_history == IncompleteHistoryAction::Escalate;
    clauses.push(clause(
        "progress_guard.incomplete_history",
        incomplete_trigger,
        candidate_is_protected,
        "causal history is not complete and policy requires escalation",
        if incomplete {
            "causal history is not complete and policy observes only"
        } else {
            "causal history is complete"
        },
    ));
    threshold_clause(
        &mut clauses,
        "progress_guard.max_consecutive_unprotected",
        policy.max_consecutive_unprotected,
        Some(prospective_unprotected),
        candidate_is_protected,
    );
    threshold_clause(
        &mut clauses,
        "progress_guard.max_same_projection_unprotected",
        policy.max_same_projection_unprotected,
        Some(prospective_same_projection_unprotected),
        candidate_is_protected,
    );
    threshold_clause(
        &mut clauses,
        "progress_guard.max_recovery_count",
        policy.max_recovery_count,
        Some(prospective_recovery_count),
        candidate_is_protected,
    );
    threshold_clause(
        &mut clauses,
        "progress_guard.max_episode_requests",
        policy.max_episode_requests,
        Some(input.pre_intent_snapshot.health.request_count),
        candidate_is_protected,
    );
    threshold_clause(
        &mut clauses,
        "progress_guard.max_episode_elapsed_ms",
        policy.max_episode_elapsed_ms,
        Some(input.pre_intent_snapshot.health.elapsed_ms),
        candidate_is_protected,
    );
    threshold_clause(
        &mut clauses,
        "progress_guard.max_episode_cost_micro_usd",
        policy.max_episode_cost_micro_usd,
        input.pre_intent_snapshot.health.settled_cost_micro_usd,
        candidate_is_protected,
    );

    if hold_active {
        for clause in clauses.iter_mut().skip(1) {
            if clause.disposition == RouteIntentClauseDisposition::Applied {
                clause.disposition = RouteIntentClauseDisposition::Skipped;
                clause.explanation =
                    "persisted hold already applies; a new activation is suppressed".to_string();
            }
        }
    }

    let escalated = clauses
        .iter()
        .any(|clause| clause.disposition == RouteIntentClauseDisposition::Applied);
    let activated = !hold_active
        && clauses.iter().any(|clause| {
            clause.disposition == RouteIntentClauseDisposition::Applied
                && clause.clause_id != "progress_guard.active_hold"
        });
    let selected_tier = if escalated {
        Some(policy.escalation_tier.clone())
    } else {
        input.candidate_tier.map(ToOwned::to_owned)
    };
    if escalated
        && !selected_tier
            .as_ref()
            .is_some_and(|tier| policy.protected_tiers.contains(tier))
    {
        anyhow::bail!("progress guard selected a tier outside protected_tiers")
    }
    Ok(GuardEvaluation {
        intent: RouteIntent {
            candidate_tier: input.candidate_tier.map(ToOwned::to_owned),
            selected_tier,
            clauses,
            trajectory_snapshot_digest: input.pre_intent_snapshot.evidence_digest.clone(),
            policy_digest: input.policy_digest.to_owned(),
        },
        causal_completeness,
        activated,
    })
}

fn merge_completeness(
    left: HistoryCompleteness,
    right: HistoryCompleteness,
) -> HistoryCompleteness {
    match (left, right) {
        (HistoryCompleteness::Incomplete, _) | (_, HistoryCompleteness::Incomplete) => {
            HistoryCompleteness::Incomplete
        }
        (HistoryCompleteness::Unknown, _) | (_, HistoryCompleteness::Unknown) => {
            HistoryCompleteness::Unknown
        }
        _ => HistoryCompleteness::Complete,
    }
}

fn validate_policy_shape(policy: &ProgressGuardPolicy) -> Result<()> {
    if policy.escalation_tier.trim().is_empty() {
        anyhow::bail!("progress guard escalation tier cannot be empty")
    }
    if policy.protected_tiers.is_empty()
        || !policy.protected_tiers.contains(&policy.escalation_tier)
    {
        anyhow::bail!("progress guard protected tiers must contain its escalation tier")
    }
    if policy.hold_for_requests == 0 || policy.hold_for_requests > u32::MAX as u64 {
        anyhow::bail!("progress guard hold length must be in 1..=u32::MAX")
    }
    if thresholds(policy).any(|value| value == 0) {
        anyhow::bail!("progress guard thresholds must be positive")
    }
    Ok(())
}

fn thresholds(policy: &ProgressGuardPolicy) -> impl Iterator<Item = u64> {
    [
        policy.max_consecutive_unprotected,
        policy.max_same_projection_unprotected,
        policy.max_recovery_count,
        policy.max_episode_requests,
        policy.max_episode_elapsed_ms,
        policy.max_episode_cost_micro_usd,
    ]
    .into_iter()
    .flatten()
}

fn threshold_clause(
    clauses: &mut Vec<RouteIntentClause>,
    clause_id: &str,
    threshold: Option<u64>,
    observed: Option<u64>,
    candidate_is_protected: bool,
) {
    let (triggered, skipped) = match (threshold, observed) {
        (Some(threshold), Some(observed)) => (
            observed >= threshold,
            format!("observed {observed} is below threshold {threshold}"),
        ),
        (Some(_), None) => (false, "required evidence is unknown".to_string()),
        (None, _) => (false, "clause is not configured".to_string()),
    };
    clauses.push(clause(
        clause_id,
        triggered,
        candidate_is_protected,
        "configured threshold is reached",
        &skipped,
    ));
}

fn clause(
    clause_id: &str,
    triggered: bool,
    candidate_is_protected: bool,
    applied_explanation: &str,
    skipped_explanation: &str,
) -> RouteIntentClause {
    let applied = triggered && !candidate_is_protected;
    RouteIntentClause {
        clause_id: clause_id.to_owned(),
        disposition: if applied {
            RouteIntentClauseDisposition::Applied
        } else {
            RouteIntentClauseDisposition::Skipped
        },
        explanation: if triggered && candidate_is_protected {
            "candidate is already protected; guard cannot downgrade it".to_string()
        } else if applied {
            applied_explanation.to_string()
        } else {
            skipped_explanation.to_string()
        },
    }
}

pub(crate) fn validate_persisted_route_intent(
    event: &TrajectoryEvent,
) -> Result<Option<Vec<(String, RouteIntentClauseDisposition)>>> {
    if event.kind != TrajectoryEventKind::RouteIntentRecorded {
        anyhow::bail!("route-intent evidence validator received another event kind")
    }
    let has_guarded_marker = event.evidence.digests.contains_key("route.policy_lock")
        || event.evidence.digests.contains_key("route.health_snapshot")
        || event
            .evidence
            .structural
            .contains_key("route.applied_clause_count")
        || event
            .evidence
            .categorical
            .keys()
            .any(|key| key.starts_with("route.clause_"));
    if !has_guarded_marker {
        return Ok(None);
    }
    if !event.evidence.digests.contains_key("route.policy_lock")
        || !event.evidence.digests.contains_key("route.health_snapshot")
    {
        anyhow::bail!("guarded route intent requires policy and health digests")
    }
    let applied_count = event
        .evidence
        .structural
        .get("route.applied_clause_count")
        .copied()
        .ok_or_else(|| anyhow::anyhow!("guarded route intent is missing applied clause count"))?;
    if event
        .evidence
        .structural
        .get("route.selected_is_protected")
        .is_some_and(|value| !matches!(value, 0 | 1))
    {
        anyhow::bail!("guarded route intent has an invalid selected-tier protection fact")
    }
    let mut clauses = Vec::new();
    for (index, expected_id) in STABLE_CLAUSE_IDS.iter().enumerate() {
        let prefix = format!("route.clause_{index:02}");
        let id = event.evidence.categorical.get(&format!("{prefix}.id"));
        let disposition = event
            .evidence
            .categorical
            .get(&format!("{prefix}.disposition"));
        match (id, disposition) {
            (Some(id), Some(disposition)) => {
                if id != expected_id {
                    anyhow::bail!(
                        "guarded route intent clause index {index} must contain '{expected_id}'"
                    )
                }
                let disposition = match disposition.as_str() {
                    "applied" => RouteIntentClauseDisposition::Applied,
                    "skipped" => RouteIntentClauseDisposition::Skipped,
                    _ => anyhow::bail!("guarded route intent contains invalid clause disposition"),
                };
                clauses.push((id.clone(), disposition));
            }
            (None, None) => anyhow::bail!("guarded route intent has a clause index gap"),
            _ => anyhow::bail!("guarded route intent has an incomplete clause record"),
        }
    }
    let clause_keys = event
        .evidence
        .categorical
        .keys()
        .filter(|key| key.starts_with("route.clause_"))
        .collect::<Vec<_>>();
    if clause_keys.len() != STABLE_CLAUSE_IDS.len() * 2
        || clause_keys.iter().any(|key| {
            !(0..STABLE_CLAUSE_IDS.len()).any(|index| {
                key.as_str() == format!("route.clause_{index:02}.id")
                    || key.as_str() == format!("route.clause_{index:02}.disposition")
            })
        })
    {
        anyhow::bail!("guarded route intent clause fields are not canonical")
    }
    let actual_applied = u64::try_from(
        clauses
            .iter()
            .filter(|(_, disposition)| *disposition == RouteIntentClauseDisposition::Applied)
            .count(),
    )?;
    if actual_applied != applied_count {
        anyhow::bail!("guarded route intent applied clause count is inconsistent")
    }
    Ok(Some(clauses))
}

pub(crate) fn validate_persisted_guard_activation(event: &TrajectoryEvent) -> Result<bool> {
    if event.kind != TrajectoryEventKind::GuardActivated {
        anyhow::bail!("guard evidence validator received another event kind")
    }
    let has_policy = event.evidence.digests.contains_key("guard.policy_lock");
    let has_health = event.evidence.digests.contains_key("guard.health_snapshot");
    if has_policy != has_health {
        anyhow::bail!("guard activation must carry policy and health digests together")
    }
    Ok(has_policy)
}

const STABLE_CLAUSE_IDS: [&str; 9] = [
    "progress_guard.active_hold",
    "progress_guard.incomplete_history",
    "progress_guard.max_consecutive_unprotected",
    "progress_guard.max_same_projection_unprotected",
    "progress_guard.max_recovery_count",
    "progress_guard.max_episode_requests",
    "progress_guard.max_episode_elapsed_ms",
    "progress_guard.max_episode_cost_micro_usd",
    "tool_safety.floor",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::trajectory::health::reduce;
    use crate::trajectory::types::{
        TRAJECTORY_SCHEMA_VERSION, TrajectoryEvidence, TrajectoryHealth,
    };

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn policy() -> ProgressGuardPolicy {
        ProgressGuardPolicy {
            escalation_tier: "protected".into(),
            protected_tiers: BTreeSet::from(["protected".into(), "tool-safe".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: Some(4),
            max_recovery_count: Some(1),
            max_episode_requests: Some(8),
            max_episode_elapsed_ms: Some(1_000),
            max_episode_cost_micro_usd: Some(500),
            hold_for_requests: 2,
            incomplete_history: IncompleteHistoryAction::Observe,
        }
    }

    fn snapshot() -> TrajectorySnapshot {
        TrajectorySnapshot {
            episode_id: "episode-1".into(),
            through_sequence: 1,
            health: TrajectoryHealth {
                completeness: HistoryCompleteness::Complete,
                request_count: 1,
                settled_request_count: 0,
                unsettled_request_count: 1,
                elapsed_ms: 0,
                latest_projection: None,
                same_projection_streak: 0,
                same_selected_tier_streak: 0,
                consecutive_unprotected_requests: 0,
                recovery_count: 0,
                requests_since_recovery: None,
                context_growth_ppm: Some(0),
                total_tokens: None,
                settled_cost_micro_usd: None,
            },
            active_hold_remaining: 0,
            evidence_digest: DIGEST.into(),
        }
    }

    fn projection(key: &str) -> Result<RouteProjection> {
        RouteProjection::parse_key(key).ok_or_else(|| anyhow::anyhow!("invalid test projection"))
    }

    fn input<'a>(
        prior: Option<&'a TrajectorySnapshot>,
        current: &'a TrajectorySnapshot,
        projection: &'a RouteProjection,
        candidate: Option<&'a str>,
        completeness: HistoryCompleteness,
    ) -> ProgressGuardInput<'a> {
        ProgressGuardInput {
            prior_snapshot: prior,
            pre_intent_snapshot: current,
            correlation_completeness: completeness,
            current_projection: projection,
            candidate_tier: candidate,
            policy_digest: DIGEST,
        }
    }

    #[test]
    fn current_projection_and_tier_advance_exact_boundaries() -> Result<()> {
        let edit = projection("agent_trace/v2|edit|normal")?;
        let test = projection("agent_trace/v2|test|normal")?;
        let mut prior = snapshot();
        prior.health.latest_projection = Some(edit.key());
        prior.health.same_projection_streak = 2;
        prior.health.consecutive_unprotected_requests = 1;
        let current = snapshot();

        let below = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert_eq!(below.intent.selected_tier.as_deref(), Some("economy"));

        prior.health.consecutive_unprotected_requests = 2;
        let boundary = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert!(boundary.activated);

        let mut same_projection_policy = policy();
        same_projection_policy.max_consecutive_unprotected = None;
        prior.health.consecutive_unprotected_requests = 3;
        prior.health.same_projection_streak = 3;
        let same_boundary = evaluate(
            &same_projection_policy,
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert!(same_boundary.activated);
        let changed = evaluate(
            &same_projection_policy,
            input(
                Some(&prior),
                &current,
                &test,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert!(!changed.activated);
        let protected = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("tool-safe"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert_eq!(protected.intent.selected_tier.as_deref(), Some("tool-safe"));
        assert!(!protected.activated);

        prior.health.same_projection_streak = 9;
        prior.health.consecutive_unprotected_requests = 0;
        let after_protected_streak = evaluate(
            &same_projection_policy,
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert!(!after_protected_streak.activated);
        Ok(())
    }

    #[test]
    fn current_recovery_incomplete_cost_and_hold_have_exact_semantics() -> Result<()> {
        let edit = projection("agent_trace/v2|edit|normal")?;
        let recovery = projection("agent_trace/v2|recovery|guarded")?;
        let mut prior = snapshot();
        let mut current = snapshot();
        current.health.settled_cost_micro_usd = None;

        let recovery_result = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &recovery,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert!(recovery_result.activated);

        let unknown_cost = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        let cost = unknown_cost
            .intent
            .clauses
            .iter()
            .find(|clause| clause.clause_id.ends_with("cost_micro_usd"))
            .ok_or_else(|| anyhow::anyhow!("missing cost clause"))?;
        assert_eq!(cost.explanation, "required evidence is unknown");

        current.health.completeness = HistoryCompleteness::Unknown;
        let observed = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Incomplete,
            ),
        )?;
        assert_eq!(observed.intent.selected_tier.as_deref(), Some("economy"));
        let mut conservative_policy = policy();
        conservative_policy.incomplete_history = IncompleteHistoryAction::Escalate;
        let conservative = evaluate(
            &conservative_policy,
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Incomplete,
            ),
        )?;
        assert!(conservative.activated);

        prior.active_hold_remaining = 1;
        prior.health.recovery_count = 1;
        let held = evaluate(
            &policy(),
            input(
                Some(&prior),
                &current,
                &edit,
                Some("economy"),
                HistoryCompleteness::Complete,
            ),
        )?;
        assert_eq!(held.intent.selected_tier.as_deref(), Some("protected"));
        assert!(!held.activated, "an active hold must not reset itself");
        assert_eq!(
            held.intent.trajectory_snapshot_digest,
            current.evidence_digest
        );
        Ok(())
    }

    #[test]
    fn persisted_guard_evidence_rejects_corrupt_typed_contracts() -> Result<()> {
        let route = persisted_route(None)?;
        assert!(validate_persisted_route_intent(&route)?.is_some());

        let mut missing_policy = route.clone();
        missing_policy.evidence.digests.remove("route.policy_lock");
        missing_policy.content_digest = missing_policy.semantic_digest()?;
        assert!(validate_persisted_route_intent(&missing_policy).is_err());

        let mut duplicate_id = route.clone();
        duplicate_id.evidence.categorical.insert(
            "route.clause_01.id".into(),
            "progress_guard.active_hold".into(),
        );
        duplicate_id.content_digest = duplicate_id.semantic_digest()?;
        assert!(validate_persisted_route_intent(&duplicate_id).is_err());

        let mut extra_suffix = route.clone();
        extra_suffix.evidence.categorical.insert(
            "route.clause_00.explanation".into(),
            "mutable wording".into(),
        );
        extra_suffix.content_digest = extra_suffix.semantic_digest()?;
        assert!(validate_persisted_route_intent(&extra_suffix).is_err());

        let mut wrong_count = route;
        wrong_count
            .evidence
            .structural
            .insert("route.applied_clause_count".into(), 1);
        wrong_count.content_digest = wrong_count.semantic_digest()?;
        assert!(validate_persisted_route_intent(&wrong_count).is_err());

        let mut partial_guard = persisted_guard()?;
        partial_guard.evidence.digests.remove("guard.policy_lock");
        partial_guard.content_digest = partial_guard.semantic_digest()?;
        assert!(validate_persisted_guard_activation(&partial_guard).is_err());
        Ok(())
    }

    #[test]
    fn reducer_requires_guard_activation_exactly_for_new_progress_trigger() -> Result<()> {
        let start = persisted_start()?;
        let prefix_digest = reduce(std::slice::from_ref(&start), &BTreeSet::new())?.evidence_digest;
        let trigger_route = with_health_digest(
            persisted_route(Some(3))?,
            "route.health_snapshot",
            &prefix_digest,
        )?;
        assert!(reduce(&[start.clone(), trigger_route.clone()], &BTreeSet::new()).is_err());

        let guard =
            with_health_digest(persisted_guard()?, "guard.health_snapshot", &prefix_digest)?;
        assert!(
            reduce(
                &[start.clone(), trigger_route, guard.clone()],
                &BTreeSet::new()
            )
            .is_ok()
        );

        let all_skipped = with_health_digest(
            persisted_route(None)?,
            "route.health_snapshot",
            &prefix_digest,
        )?;
        assert!(
            reduce(
                &[start.clone(), all_skipped, guard.clone()],
                &BTreeSet::new()
            )
            .is_err()
        );

        let active_hold_only = with_health_digest(
            persisted_route(Some(0))?,
            "route.health_snapshot",
            &prefix_digest,
        )?;
        assert!(
            reduce(
                &[start.clone(), active_hold_only, guard.clone()],
                &BTreeSet::new()
            )
            .is_err()
        );

        let legacy_guard = TrajectoryEvent {
            evidence: TrajectoryEvidence {
                structural: BTreeMap::from([("guard.hold_for_requests".into(), 2)]),
                categorical: BTreeMap::new(),
                digests: BTreeMap::new(),
            },
            content_digest: String::new(),
            ..guard
        };
        let mut legacy_guard = legacy_guard;
        legacy_guard.content_digest = legacy_guard.semantic_digest()?;
        let skipped_route = with_health_digest(
            persisted_route(None)?,
            "route.health_snapshot",
            &prefix_digest,
        )?;
        assert!(reduce(&[start, skipped_route, legacy_guard], &BTreeSet::new()).is_err());
        Ok(())
    }

    fn with_health_digest(
        mut event: TrajectoryEvent,
        key: &str,
        digest: &str,
    ) -> Result<TrajectoryEvent> {
        event.evidence.digests.insert(key.into(), digest.into());
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }

    fn persisted_start() -> Result<TrajectoryEvent> {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "start-1".into(),
            owner_user_id: "owner-a".into(),
            episode_id: "episode-1".into(),
            request_id: Some("request-1".into()),
            sequence: 1,
            kind: TrajectoryEventKind::RequestStarted,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::from([("request.canonical_input_bytes".into(), 10)]),
                categorical: BTreeMap::from([
                    ("correlation.source".into(), "explicit_root".into()),
                    ("history.completeness".into(), "complete".into()),
                ]),
                digests: BTreeMap::new(),
            },
            captured_at: "2026-08-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }

    /// `applied_index`: `None` means all skipped, `Some(0)` active-hold only,
    /// and `Some(3)` a new max-episode-requests trigger.
    fn persisted_route(applied_index: Option<usize>) -> Result<TrajectoryEvent> {
        let mut categorical = BTreeMap::from([
            (
                "route.projection".into(),
                "agent_trace/v2|edit|normal".into(),
            ),
            ("route.workflow_state".into(), "edit".into()),
            ("route.candidate_tier".into(), "economy".into()),
            ("route.selected_tier".into(), "strong".into()),
        ]);
        for (index, id) in STABLE_CLAUSE_IDS.iter().enumerate() {
            categorical.insert(format!("route.clause_{index:02}.id"), (*id).into());
            categorical.insert(
                format!("route.clause_{index:02}.disposition"),
                if applied_index == Some(index) {
                    "applied"
                } else {
                    "skipped"
                }
                .into(),
            );
        }
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "route-1".into(),
            owner_user_id: "owner-a".into(),
            episode_id: "episode-1".into(),
            request_id: Some("request-1".into()),
            sequence: 2,
            kind: TrajectoryEventKind::RouteIntentRecorded,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::from([(
                    "route.applied_clause_count".into(),
                    u64::from(applied_index.is_some()),
                )]),
                categorical,
                digests: BTreeMap::from([
                    ("route.health_snapshot".into(), DIGEST.into()),
                    ("route.policy_lock".into(), DIGEST.into()),
                ]),
            },
            captured_at: "2026-08-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }

    fn persisted_guard() -> Result<TrajectoryEvent> {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "guard-1".into(),
            owner_user_id: "owner-a".into(),
            episode_id: "episode-1".into(),
            request_id: Some("request-1".into()),
            sequence: 3,
            kind: TrajectoryEventKind::GuardActivated,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::from([("guard.hold_for_requests".into(), 2)]),
                categorical: BTreeMap::new(),
                digests: BTreeMap::from([
                    ("guard.health_snapshot".into(), DIGEST.into()),
                    ("guard.policy_lock".into(), DIGEST.into()),
                ]),
            },
            captured_at: "2026-08-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }
}
