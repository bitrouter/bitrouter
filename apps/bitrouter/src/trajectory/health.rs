use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};

use super::guard::{validate_persisted_guard_activation, validate_persisted_route_intent};
use super::types::{
    HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind, TrajectoryHealth,
    TrajectorySnapshot, validate_event,
};
use crate::workflow_state::ir::{RouteProjection, WorkflowStateKind};

const PPM_SCALE: u64 = 1_000_000;
const MAX_HOLD_REQUESTS: u64 = u32::MAX as u64;

enum RequestPhase {
    Started,
    Routed,
    RoutedGuarded,
    Settled,
}

struct SnapshotFacts<'a> {
    episode_id: &'a str,
    through_sequence: u64,
    completeness: HistoryCompleteness,
    has_started_request: bool,
    request_count: u64,
    settled_request_count: u64,
    first_timestamp: &'a chrono::DateTime<chrono::FixedOffset>,
    last_timestamp: &'a chrono::DateTime<chrono::FixedOffset>,
    latest_projection: Option<&'a str>,
    same_projection_streak: u64,
    same_selected_tier_streak: u64,
    consecutive_unprotected_requests: u64,
    recovery_count: u64,
    requests_since_recovery: Option<u64>,
    first_canonical_bytes: Option<u64>,
    latest_canonical_bytes: Option<u64>,
    total_tokens: Option<u64>,
    settled_cost_micro_usd: Option<u64>,
    active_hold_remaining: u64,
}

pub(crate) enum PrefixReduction {
    Complete(Box<TrajectorySnapshot>),
    AwaitingGuardActivation,
}

pub fn reduce(
    events: &[TrajectoryEvent],
    protected_tiers: &BTreeSet<String>,
) -> Result<TrajectorySnapshot> {
    match reduce_prefix(events, protected_tiers)? {
        PrefixReduction::Complete(snapshot) => Ok(*snapshot),
        PrefixReduction::AwaitingGuardActivation => {
            anyhow::bail!("guarded route trigger is missing its guard activation")
        }
    }
}

pub(crate) fn reduce_prefix(
    events: &[TrajectoryEvent],
    protected_tiers: &BTreeSet<String>,
) -> Result<PrefixReduction> {
    let first = events
        .first()
        .ok_or_else(|| anyhow::anyhow!("trajectory reduction requires at least one event"))?;
    if first.kind != TrajectoryEventKind::RequestStarted {
        anyhow::bail!("trajectory episode must begin with RequestStarted")
    }
    let owner_user_id = &first.owner_user_id;
    let episode_id = &first.episode_id;
    let first_timestamp = chrono::DateTime::parse_from_rfc3339(&first.captured_at)
        .context("parsing first trajectory timestamp")?;
    let mut previous_timestamp = first_timestamp;
    let mut last_timestamp = first_timestamp;
    let mut requests = BTreeMap::<String, RequestPhase>::new();
    let mut guarded_route_digests = BTreeMap::<String, (String, String)>::new();
    let mut pending_guard_activation: Option<String> = None;
    let mut completeness = HistoryCompleteness::Complete;
    let mut request_count = 0_u64;
    let mut settled_request_count = 0_u64;
    let mut same_projection_streak = 0_u64;
    let mut same_selected_tier_streak = 0_u64;
    let mut consecutive_unprotected_requests = 0_u64;
    let mut recovery_count = 0_u64;
    let mut requests_since_recovery = None;
    let mut total_tokens = None;
    let mut settled_cost_micro_usd = None;
    let mut first_canonical_bytes = None;
    let mut latest_canonical_bytes = None;
    let mut previous_projection: Option<String> = None;
    let mut previous_selected_tier: Option<String> = None;
    let mut active_hold_remaining = 0_u64;
    let mut closed = false;

    for (index, event) in events.iter().enumerate() {
        validate_event(event)?;
        let expected_sequence = u64::try_from(index)
            .context("trajectory event count exceeds sequence range")?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory event sequence overflow"))?;
        if event.sequence != expected_sequence {
            anyhow::bail!(
                "trajectory reducer expected sequence {expected_sequence}, found {}",
                event.sequence
            )
        }
        if &event.owner_user_id != owner_user_id || &event.episode_id != episode_id {
            anyhow::bail!("trajectory reducer events must share one owner and episode")
        }
        if closed {
            anyhow::bail!("trajectory episode contains an event after closure")
        }
        let captured_at = chrono::DateTime::parse_from_rfc3339(&event.captured_at)
            .context("parsing trajectory event timestamp")?;
        if captured_at < previous_timestamp {
            anyhow::bail!("trajectory event timestamps regress in sequence order")
        }
        let prefix_timestamp = last_timestamp;
        previous_timestamp = captured_at;
        last_timestamp = captured_at;

        if let Some(request_id) = pending_guard_activation.as_deref()
            && (event.kind != TrajectoryEventKind::GuardActivated
                || event.request_id.as_deref() != Some(request_id))
        {
            anyhow::bail!("guarded route trigger must be followed by its guard activation")
        }

        match event.kind {
            TrajectoryEventKind::RequestStarted => {
                let request_id = required_request_id(event)?;
                if requests.contains_key(request_id) {
                    anyhow::bail!("trajectory request '{request_id}' starts more than once")
                }
                active_hold_remaining = active_hold_remaining.saturating_sub(1);
                let canonical_input_bytes = event
                    .evidence
                    .structural
                    .get("request.canonical_input_bytes")
                    .copied();
                if request_count == 0 {
                    first_canonical_bytes = canonical_input_bytes;
                }
                latest_canonical_bytes = canonical_input_bytes;
                request_count = checked_increment(request_count, "request count")?;
                completeness = merge_completeness(
                    completeness,
                    parse_start_completeness(
                        event.evidence.categorical.get("history.completeness"),
                    )?,
                );
                completeness = merge_completeness(
                    completeness,
                    validate_correlation_source(
                        event.evidence.categorical.get("correlation.source"),
                    )?,
                );
                requests.insert(request_id.to_owned(), RequestPhase::Started);
            }
            TrajectoryEventKind::RouteIntentRecorded => {
                let request_id = required_request_id(event)?;
                let persisted_guarded_route = validate_persisted_route_intent(event)?;
                if let Some(clauses) = &persisted_guarded_route {
                    let policy = event
                        .evidence
                        .digests
                        .get("route.policy_lock")
                        .ok_or_else(|| anyhow::anyhow!("guarded route lost its policy digest"))?;
                    let health = event
                        .evidence
                        .digests
                        .get("route.health_snapshot")
                        .ok_or_else(|| anyhow::anyhow!("guarded route lost its health digest"))?;
                    let factual_prefix = snapshot_from_facts(SnapshotFacts {
                        episode_id,
                        through_sequence: event.sequence.checked_sub(1).ok_or_else(|| {
                            anyhow::anyhow!("guarded route has no factual event prefix")
                        })?,
                        completeness,
                        has_started_request: requests
                            .values()
                            .any(|phase| matches!(phase, RequestPhase::Started)),
                        request_count,
                        settled_request_count,
                        first_timestamp: &first_timestamp,
                        last_timestamp: &prefix_timestamp,
                        latest_projection: previous_projection.as_deref(),
                        same_projection_streak,
                        same_selected_tier_streak,
                        consecutive_unprotected_requests,
                        recovery_count,
                        requests_since_recovery,
                        first_canonical_bytes,
                        latest_canonical_bytes,
                        total_tokens,
                        settled_cost_micro_usd,
                        active_hold_remaining,
                    })?;
                    if health != &factual_prefix.evidence_digest {
                        anyhow::bail!(
                            "guarded route health digest disagrees with its factual pre-intent trajectory snapshot"
                        )
                    }
                    guarded_route_digests
                        .insert(request_id.to_owned(), (policy.clone(), health.clone()));
                    if clauses.iter().any(|(id, disposition)| {
                        *disposition == super::guard::RouteIntentClauseDisposition::Applied
                            && id.starts_with("progress_guard.")
                            && id != "progress_guard.active_hold"
                    }) {
                        pending_guard_activation = Some(request_id.to_owned());
                    }
                }
                let phase = requests.get_mut(request_id).ok_or_else(|| {
                    anyhow::anyhow!("route intent references unknown request '{request_id}'")
                })?;
                match phase {
                    RequestPhase::Started => *phase = RequestPhase::Routed,
                    RequestPhase::Routed | RequestPhase::RoutedGuarded => anyhow::bail!(
                        "trajectory request '{request_id}' has duplicate route intent"
                    ),
                    RequestPhase::Settled => anyhow::bail!(
                        "trajectory request '{request_id}' records intent after settlement"
                    ),
                }

                let projection = event.evidence.categorical.get("route.projection");
                let workflow_state = event.evidence.categorical.get("route.workflow_state");
                let selected_tier = event.evidence.categorical.get("route.selected_tier");
                let parsed_projection = projection
                    .map(|value| {
                        RouteProjection::parse_key(value).ok_or_else(|| {
                            anyhow::anyhow!("route intent has invalid canonical projection")
                        })
                    })
                    .transpose()?;
                let parsed_workflow_state = workflow_state
                    .map(|value| {
                        RouteProjection::parse_key(&format!("agent_trace/v2|{value}|normal"))
                            .map(|projection| projection.state_kind)
                            .ok_or_else(|| {
                                anyhow::anyhow!("route intent has invalid generic workflow state")
                            })
                    })
                    .transpose()?;
                if let (Some(projection), Some(workflow_state)) =
                    (&parsed_projection, &parsed_workflow_state)
                    && projection.state_kind != *workflow_state
                {
                    anyhow::bail!("route intent projection and workflow state disagree")
                }
                if projection.is_none() || workflow_state.is_none() || selected_tier.is_none() {
                    completeness = merge_completeness(completeness, HistoryCompleteness::Unknown);
                }

                let previous_was_recovery = previous_projection
                    .as_deref()
                    .and_then(RouteProjection::parse_key)
                    .is_some_and(|projection| projection.state_kind == WorkflowStateKind::Recovery);

                same_projection_streak = update_streak(
                    &mut previous_projection,
                    projection.map(String::as_str),
                    same_projection_streak,
                    "same projection streak",
                )?;
                same_selected_tier_streak = update_streak(
                    &mut previous_selected_tier,
                    selected_tier.map(String::as_str),
                    same_selected_tier_streak,
                    "same selected tier streak",
                )?;
                let selected_is_protected = if let Some(value) = event
                    .evidence
                    .structural
                    .get("route.selected_is_protected")
                    .copied()
                {
                    match value {
                        0 => false,
                        1 => true,
                        _ => anyhow::bail!(
                            "guarded route has an invalid selected-tier protection fact"
                        ),
                    }
                } else {
                    selected_tier.is_some_and(|tier| protected_tiers.contains(tier))
                };
                consecutive_unprotected_requests = match selected_tier {
                    Some(_) if selected_is_protected => 0,
                    Some(_) => checked_increment(
                        consecutive_unprotected_requests,
                        "consecutive unprotected request count",
                    )?,
                    None => 0,
                };
                if parsed_workflow_state == Some(WorkflowStateKind::Recovery) {
                    if !previous_was_recovery {
                        recovery_count = checked_increment(recovery_count, "recovery count")?;
                    }
                    requests_since_recovery = Some(0);
                } else if let Some(count) = requests_since_recovery {
                    requests_since_recovery =
                        Some(checked_increment(count, "requests since recovery count")?);
                }
            }
            TrajectoryEventKind::RequestSettled => {
                let request_id = required_request_id(event)?;
                let phase = requests.get_mut(request_id).ok_or_else(|| {
                    anyhow::anyhow!("settlement references unknown request '{request_id}'")
                })?;
                match phase {
                    RequestPhase::Started => anyhow::bail!(
                        "trajectory request '{request_id}' settles before route intent"
                    ),
                    RequestPhase::Routed | RequestPhase::RoutedGuarded => {
                        *phase = RequestPhase::Settled;
                    }
                    RequestPhase::Settled => {
                        anyhow::bail!("trajectory request '{request_id}' settles more than once")
                    }
                }
                let had_settled_requests = settled_request_count > 0;
                total_tokens = checked_optional_total(
                    total_tokens,
                    event.evidence.structural.get("settlement.total_tokens"),
                    had_settled_requests,
                    "total token count",
                )?;
                settled_cost_micro_usd = checked_optional_total(
                    settled_cost_micro_usd,
                    event.evidence.structural.get("settlement.cost_micro_usd"),
                    had_settled_requests,
                    "settled cost",
                )?;
                settled_request_count =
                    checked_increment(settled_request_count, "settled request count")?;
            }
            TrajectoryEventKind::GuardActivated => {
                let request_id = required_request_id(event)?;
                if validate_persisted_guard_activation(event)? {
                    if pending_guard_activation.as_deref() != Some(request_id) {
                        anyhow::bail!(
                            "guard activation has no immediately preceding new guard trigger"
                        )
                    }
                    let route_digests = guarded_route_digests.get(request_id).ok_or_else(|| {
                        anyhow::anyhow!("guard activation has no guarded route intent")
                    })?;
                    let guard_policy =
                        event
                            .evidence
                            .digests
                            .get("guard.policy_lock")
                            .ok_or_else(|| {
                                anyhow::anyhow!("guard activation lost its policy digest")
                            })?;
                    let guard_health = event
                        .evidence
                        .digests
                        .get("guard.health_snapshot")
                        .ok_or_else(|| {
                            anyhow::anyhow!("guard activation lost its health digest")
                        })?;
                    if route_digests != &(guard_policy.clone(), guard_health.clone()) {
                        anyhow::bail!(
                            "guard activation policy/health digests disagree with route intent"
                        )
                    }
                    pending_guard_activation = None;
                } else if guarded_route_digests.contains_key(request_id) {
                    anyhow::bail!("guarded route requires typed guard activation evidence")
                }
                let phase = requests.get_mut(request_id).ok_or_else(|| {
                    anyhow::anyhow!("guard activation references unknown request '{request_id}'")
                })?;
                match phase {
                    RequestPhase::Started => {
                        anyhow::bail!("guard activation requires a recorded route intent")
                    }
                    RequestPhase::Routed => *phase = RequestPhase::RoutedGuarded,
                    RequestPhase::RoutedGuarded => anyhow::bail!(
                        "trajectory request '{request_id}' has duplicate guard activation"
                    ),
                    RequestPhase::Settled => anyhow::bail!(
                        "trajectory request '{request_id}' activates guard after settlement"
                    ),
                }
                let hold = event
                    .evidence
                    .structural
                    .get("guard.hold_for_requests")
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("guard activation is missing hold length"))?;
                if hold == 0 || hold > MAX_HOLD_REQUESTS {
                    anyhow::bail!("guard hold length must be in 1..=u32::MAX")
                }
                active_hold_remaining = hold;
            }
            TrajectoryEventKind::EpisodeClosed => {
                if event.request_id.is_some() {
                    anyhow::bail!("episode close event cannot identify a request")
                }
                closed = true;
            }
        }
    }

    if pending_guard_activation.is_some() {
        return Ok(PrefixReduction::AwaitingGuardActivation);
    }

    let through_sequence = events
        .last()
        .map(|event| event.sequence)
        .ok_or_else(|| anyhow::anyhow!("trajectory reduction lost its final event"))?;
    Ok(PrefixReduction::Complete(Box::new(snapshot_from_facts(
        SnapshotFacts {
            episode_id,
            through_sequence,
            completeness,
            has_started_request: requests
                .values()
                .any(|phase| matches!(phase, RequestPhase::Started)),
            request_count,
            settled_request_count,
            first_timestamp: &first_timestamp,
            last_timestamp: &last_timestamp,
            latest_projection: previous_projection.as_deref(),
            same_projection_streak,
            same_selected_tier_streak,
            consecutive_unprotected_requests,
            recovery_count,
            requests_since_recovery,
            first_canonical_bytes,
            latest_canonical_bytes,
            total_tokens,
            settled_cost_micro_usd,
            active_hold_remaining,
        },
    )?)))
}

fn snapshot_from_facts(facts: SnapshotFacts<'_>) -> Result<TrajectorySnapshot> {
    let completeness = if facts.has_started_request {
        merge_completeness(facts.completeness, HistoryCompleteness::Unknown)
    } else {
        facts.completeness
    };
    let unsettled_request_count = facts
        .request_count
        .checked_sub(facts.settled_request_count)
        .ok_or_else(|| anyhow::anyhow!("settled request count exceeds request count"))?;
    let elapsed_ms = u64::try_from(
        facts
            .last_timestamp
            .signed_duration_since(facts.first_timestamp)
            .num_milliseconds(),
    )
    .context("trajectory elapsed time is negative")?;
    let context_growth_ppm =
        context_growth_ppm(facts.first_canonical_bytes, facts.latest_canonical_bytes)?;
    let health = TrajectoryHealth {
        completeness,
        request_count: facts.request_count,
        settled_request_count: facts.settled_request_count,
        unsettled_request_count,
        elapsed_ms,
        latest_projection: facts.latest_projection.map(ToOwned::to_owned),
        same_projection_streak: facts.same_projection_streak,
        same_selected_tier_streak: facts.same_selected_tier_streak,
        consecutive_unprotected_requests: facts.consecutive_unprotected_requests,
        recovery_count: facts.recovery_count,
        requests_since_recovery: facts.requests_since_recovery,
        context_growth_ppm,
        total_tokens: facts.total_tokens,
        settled_cost_micro_usd: facts.settled_cost_micro_usd,
    };
    let mut snapshot = TrajectorySnapshot {
        episode_id: facts.episode_id.to_owned(),
        through_sequence: facts.through_sequence,
        health,
        active_hold_remaining: facts.active_hold_remaining,
        evidence_digest: String::new(),
    };
    snapshot.evidence_digest = snapshot.semantic_digest()?;
    Ok(snapshot)
}

fn required_request_id(event: &TrajectoryEvent) -> Result<&str> {
    event
        .request_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("trajectory request event is missing request_id"))
}

fn parse_start_completeness(value: Option<&String>) -> Result<HistoryCompleteness> {
    match value.map(String::as_str) {
        Some("complete") => Ok(HistoryCompleteness::Complete),
        Some("incomplete") => Ok(HistoryCompleteness::Incomplete),
        Some("unknown") | None => Ok(HistoryCompleteness::Unknown),
        Some(_) => anyhow::bail!("request start has invalid history completeness"),
    }
}

fn validate_correlation_source(value: Option<&String>) -> Result<HistoryCompleteness> {
    match value.map(String::as_str) {
        Some("native_parent_id" | "canonical_prefix" | "explicit_root" | "unresolved") => {
            Ok(HistoryCompleteness::Complete)
        }
        None => Ok(HistoryCompleteness::Unknown),
        Some(_) => anyhow::bail!("request start has invalid correlation source"),
    }
}

fn merge_completeness(
    current: HistoryCompleteness,
    next: HistoryCompleteness,
) -> HistoryCompleteness {
    match (current, next) {
        (HistoryCompleteness::Incomplete, _) | (_, HistoryCompleteness::Incomplete) => {
            HistoryCompleteness::Incomplete
        }
        (HistoryCompleteness::Unknown, _) | (_, HistoryCompleteness::Unknown) => {
            HistoryCompleteness::Unknown
        }
        _ => HistoryCompleteness::Complete,
    }
}

fn update_streak(
    previous: &mut Option<String>,
    current: Option<&str>,
    streak: u64,
    field: &str,
) -> Result<u64> {
    let Some(current) = current else {
        *previous = None;
        return Ok(0);
    };
    let next = if previous.as_deref() == Some(current) {
        checked_increment(streak, field)?
    } else {
        1
    };
    *previous = Some(current.to_owned());
    Ok(next)
}

fn checked_increment(value: u64, field: &str) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("{field} overflow"))
}

fn checked_optional_total(
    total: Option<u64>,
    value: Option<&u64>,
    had_prior_values: bool,
    field: &str,
) -> Result<Option<u64>> {
    if !had_prior_values {
        return Ok(value.copied());
    }
    match (total, value) {
        (Some(total), Some(value)) => total
            .checked_add(*value)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("{field} overflow")),
        _ => Ok(None),
    }
}

fn context_growth_ppm(first: Option<u64>, latest: Option<u64>) -> Result<Option<u64>> {
    let (Some(first), Some(latest)) = (first, latest) else {
        return Ok(None);
    };
    if first == 0 {
        return Ok(None);
    }
    let growth = latest.saturating_sub(first);
    let scaled = growth
        .checked_mul(PPM_SCALE)
        .ok_or_else(|| anyhow::anyhow!("context growth ppm overflow"))?;
    Ok(Some(scaled / first))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::reduce;
    use crate::trajectory::types::{
        HistoryCompleteness, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind,
        TrajectoryEvidence, TrajectoryHealth,
    };

    const TEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn hand_literal_episode_reduces_every_health_field() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "planning", "economy")?,
            settled(3, "r1", Some(0), Some(0))?,
            start(4, "r2", 150, "complete", "canonical_prefix")?,
            intent(5, "r2", "planning", "economy")?,
            start(6, "r3", 200, "complete", "canonical_prefix")?,
            intent(7, "r3", "recovery", "protected")?,
            settled(8, "r2", Some(20), None)?,
            guard(9, "r3", 2)?,
            settled(10, "r3", Some(30), Some(10))?,
            start(11, "r4", 250, "complete", "canonical_prefix")?,
            intent(12, "r4", "recovery", "economy")?,
            settled(13, "r4", None, Some(0))?,
        ];

        let snapshot = reduce(&events, &BTreeSet::from(["protected".to_owned()]))?;

        assert_eq!(snapshot.episode_id, "episode-1");
        assert_eq!(snapshot.through_sequence, 13);
        assert_eq!(
            snapshot.health,
            TrajectoryHealth {
                completeness: HistoryCompleteness::Complete,
                request_count: 4,
                settled_request_count: 4,
                unsettled_request_count: 0,
                elapsed_ms: 12_000,
                latest_projection: Some("agent_trace/v2|recovery|normal".to_string()),
                same_projection_streak: 2,
                same_selected_tier_streak: 1,
                consecutive_unprotected_requests: 1,
                recovery_count: 1,
                requests_since_recovery: Some(0),
                context_growth_ppm: Some(1_500_000),
                total_tokens: None,
                settled_cost_micro_usd: None,
            }
        );
        assert_eq!(snapshot.active_hold_remaining, 1);
        assert!(snapshot.evidence_digest.starts_with("sha256:"));
        assert_eq!(snapshot.evidence_digest.len(), 71);
        Ok(())
    }

    #[test]
    fn explicit_zero_totals_remain_known_and_failure_settlements_count() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 40, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "economy")?,
            settled_with_outcome(3, "r1", Some(0), Some(0), "failed")?,
        ];

        let snapshot = reduce(&events, &BTreeSet::new())?;

        assert_eq!(snapshot.health.settled_request_count, 1);
        assert_eq!(snapshot.health.unsettled_request_count, 0);
        assert_eq!(snapshot.health.total_tokens, Some(0));
        assert_eq!(snapshot.health.settled_cost_micro_usd, Some(0));
        Ok(())
    }

    #[test]
    fn missing_settlement_evidence_permanently_poisons_totals() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 40, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "economy")?,
            settled(3, "r1", None, None)?,
            start(4, "r2", 80, "complete", "canonical_prefix")?,
            intent(5, "r2", "planning", "economy")?,
            settled(6, "r2", Some(100), Some(200))?,
        ];

        let snapshot = reduce(&events, &BTreeSet::new())?;

        assert_eq!(snapshot.health.settled_request_count, 2);
        assert_eq!(snapshot.health.total_tokens, None);
        assert_eq!(snapshot.health.settled_cost_micro_usd, None);
        Ok(())
    }

    #[test]
    fn unmatched_start_is_temporarily_unknown_and_valid_intent_clears_it() -> anyhow::Result<()> {
        let started = vec![start(1, "r1", 40, "complete", "explicit_root")?];
        assert_eq!(
            reduce(&started, &BTreeSet::new())?.health.completeness,
            HistoryCompleteness::Unknown
        );
        let before_intent = reduce(&started, &BTreeSet::new())?;
        assert_eq!(before_intent.health.request_count, 1);
        assert_eq!(before_intent.health.settled_request_count, 0);
        assert_eq!(before_intent.health.unsettled_request_count, 1);
        assert_eq!(before_intent.health.total_tokens, None);
        assert_eq!(before_intent.health.settled_cost_micro_usd, None);

        let routed = vec![started[0].clone(), intent(2, "r1", "opening", "economy")?];
        assert_eq!(
            reduce(&routed, &BTreeSet::new())?.health.completeness,
            HistoryCompleteness::Complete
        );
        Ok(())
    }

    #[test]
    fn incomplete_dominates_unknown_and_unknown_dominates_complete() -> anyhow::Result<()> {
        let mut missing_source = start(1, "r1", 40, "complete", "explicit_root")?;
        missing_source
            .evidence
            .categorical
            .remove("correlation.source");
        resign(&mut missing_source)?;
        let events = vec![
            missing_source,
            intent(2, "r1", "opening", "economy")?,
            start(3, "r2", 80, "incomplete", "unresolved")?,
            intent(4, "r2", "planning", "economy")?,
        ];

        let snapshot = reduce(&events, &BTreeSet::new())?;

        assert_eq!(
            snapshot.health.completeness,
            HistoryCompleteness::Incomplete
        );
        Ok(())
    }

    #[test]
    fn route_streaks_reset_on_switch_and_missing_tier_is_not_unprotected() -> anyhow::Result<()> {
        let mut missing_tier = intent(8, "r4", "test", "economy")?;
        missing_tier
            .evidence
            .categorical
            .remove("route.selected_tier");
        resign(&mut missing_tier)?;
        let events = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "planning", "low")?,
            start(3, "r2", 110, "complete", "canonical_prefix")?,
            intent(4, "r2", "planning", "low")?,
            start(5, "r3", 120, "complete", "canonical_prefix")?,
            intent(6, "r3", "test", "high")?,
            start(7, "r4", 130, "complete", "canonical_prefix")?,
            missing_tier,
        ];

        let snapshot = reduce(&events, &BTreeSet::from(["high".to_owned()]))?;

        assert_eq!(snapshot.health.same_projection_streak, 2);
        assert_eq!(snapshot.health.same_selected_tier_streak, 0);
        assert_eq!(snapshot.health.consecutive_unprotected_requests, 0);
        assert_eq!(snapshot.health.completeness, HistoryCompleteness::Unknown);
        Ok(())
    }

    #[test]
    fn recovery_is_exact_and_requests_since_recovery_advance_on_intents() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "recovery", "low")?,
            start(3, "r2", 20, "complete", "canonical_prefix")?,
            intent(4, "r2", "planning", "low")?,
            start(5, "r3", 30, "complete", "canonical_prefix")?,
            intent(6, "r3", "review", "low")?,
        ];

        let snapshot = reduce(&events, &BTreeSet::new())?;

        assert_eq!(snapshot.health.recovery_count, 1);
        assert_eq!(snapshot.health.requests_since_recovery, Some(2));
        Ok(())
    }

    #[test]
    fn consecutive_recovery_projections_count_one_recovery_edge() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            start(3, "r2", 20, "complete", "canonical_prefix")?,
            intent(4, "r2", "recovery", "high")?,
            start(5, "r3", 30, "complete", "canonical_prefix")?,
            intent(6, "r3", "recovery", "high")?,
            start(7, "r4", 40, "complete", "canonical_prefix")?,
            intent(8, "r4", "recovery", "high")?,
            start(9, "r5", 50, "complete", "canonical_prefix")?,
            intent(10, "r5", "review", "low")?,
        ];

        let snapshot = reduce(&events, &BTreeSet::from(["high".to_owned()]))?;

        assert_eq!(snapshot.health.recovery_count, 1);
        assert_eq!(snapshot.health.requests_since_recovery, Some(1));
        Ok(())
    }

    #[test]
    fn context_growth_uses_first_and_latest_authoritative_size() -> anyhow::Result<()> {
        let mut missing = start(3, "r2", 1, "complete", "canonical_prefix")?;
        missing
            .evidence
            .structural
            .remove("request.canonical_input_bytes");
        resign(&mut missing)?;
        let events = vec![
            start(1, "r1", 200, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            missing,
            intent(4, "r2", "planning", "low")?,
            start(5, "r3", 100, "complete", "canonical_prefix")?,
            intent(6, "r3", "test", "low")?,
        ];

        assert_eq!(
            reduce(&events, &BTreeSet::new())?.health.context_growth_ppm,
            Some(0)
        );

        let zero_first = vec![
            start(1, "r1", 0, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
        ];
        assert_eq!(
            reduce(&zero_first, &BTreeSet::new())?
                .health
                .context_growth_ppm,
            None
        );

        let mut missing_first = start(1, "r1", 1, "complete", "explicit_root")?;
        missing_first
            .evidence
            .structural
            .remove("request.canonical_input_bytes");
        resign(&mut missing_first)?;
        let missing_first = vec![
            missing_first,
            intent(2, "r1", "opening", "low")?,
            start(3, "r2", 100, "complete", "canonical_prefix")?,
            intent(4, "r2", "planning", "low")?,
        ];
        assert_eq!(
            reduce(&missing_first, &BTreeSet::new())?
                .health
                .context_growth_ppm,
            None
        );

        let mut missing_latest = start(3, "r2", 1, "complete", "canonical_prefix")?;
        missing_latest
            .evidence
            .structural
            .remove("request.canonical_input_bytes");
        resign(&mut missing_latest)?;
        let missing_latest = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            missing_latest,
            intent(4, "r2", "planning", "low")?,
        ];
        assert_eq!(
            reduce(&missing_latest, &BTreeSet::new())?
                .health
                .context_growth_ppm,
            None
        );
        Ok(())
    }

    #[test]
    fn hold_counts_exactly_subsequent_request_starts() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "protected")?,
            guard(3, "r1", 2)?,
        ];
        assert_eq!(reduce(&events, &BTreeSet::new())?.active_hold_remaining, 2);

        let mut one_later = events;
        one_later.push(start(4, "r2", 20, "complete", "canonical_prefix")?);
        assert_eq!(
            reduce(&one_later, &BTreeSet::new())?.active_hold_remaining,
            1
        );

        one_later.push(intent(5, "r2", "planning", "protected")?);
        one_later.push(start(6, "r3", 30, "complete", "canonical_prefix")?);
        assert_eq!(
            reduce(&one_later, &BTreeSet::new())?.active_hold_remaining,
            0
        );
        Ok(())
    }

    #[test]
    fn reducer_rejects_sequence_identity_digest_and_terminal_corruption() -> anyhow::Result<()> {
        let base = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
        ];

        let mut gap = base.clone();
        gap[1].sequence = 3;
        resign(&mut gap[1])?;
        assert!(reduce(&gap, &BTreeSet::new()).is_err());

        let mut duplicate = base.clone();
        duplicate[1].sequence = 1;
        resign(&mut duplicate[1])?;
        assert!(reduce(&duplicate, &BTreeSet::new()).is_err());

        let mut owner = base.clone();
        owner[1].owner_user_id = "owner-2".to_owned();
        resign(&mut owner[1])?;
        assert!(reduce(&owner, &BTreeSet::new()).is_err());

        let mut episode = base.clone();
        episode[1].episode_id = "episode-2".to_owned();
        resign(&mut episode[1])?;
        assert!(reduce(&episode, &BTreeSet::new()).is_err());

        let mut digest = base.clone();
        digest[1].content_digest = format!("sha256:{}", "0".repeat(64));
        assert!(reduce(&digest, &BTreeSet::new()).is_err());

        let closed = vec![
            base[0].clone(),
            base[1].clone(),
            event(
                3,
                None,
                TrajectoryEventKind::EpisodeClosed,
                BTreeMap::new(),
                BTreeMap::new(),
            )?,
            start(4, "r2", 20, "complete", "canonical_prefix")?,
        ];
        assert!(reduce(&closed, &BTreeSet::new()).is_err());
        Ok(())
    }

    #[test]
    fn reducer_rejects_invalid_request_event_lifecycle() -> anyhow::Result<()> {
        let close_only = vec![event(
            1,
            None,
            TrajectoryEventKind::EpisodeClosed,
            BTreeMap::new(),
            BTreeMap::new(),
        )?];
        assert!(reduce(&close_only, &BTreeSet::new()).is_err());

        assert!(reduce(&[settled(1, "r1", Some(1), Some(1))?], &BTreeSet::new()).is_err());

        let duplicate_settlement = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            settled(3, "r1", Some(1), Some(1))?,
            settled(4, "r1", Some(2), Some(1))?,
        ];
        assert!(reduce(&duplicate_settlement, &BTreeSet::new()).is_err());

        assert!(reduce(&[intent(1, "r1", "opening", "low")?], &BTreeSet::new()).is_err());

        let duplicate_route = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            intent(3, "r1", "planning", "low")?,
        ];
        assert!(reduce(&duplicate_route, &BTreeSet::new()).is_err());

        let settlement_before_intent = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            settled(2, "r1", Some(1), Some(1))?,
        ];
        assert!(reduce(&settlement_before_intent, &BTreeSet::new()).is_err());

        let intent_after_settlement = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            settled(3, "r1", Some(1), Some(1))?,
            intent(4, "r1", "planning", "low")?,
        ];
        assert!(reduce(&intent_after_settlement, &BTreeSet::new()).is_err());

        let guard_before_intent = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            guard(2, "r1", 1)?,
        ];
        assert!(reduce(&guard_before_intent, &BTreeSet::new()).is_err());

        let duplicate_guard = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            guard(3, "r1", 1)?,
            guard(4, "r1", 1)?,
        ];
        assert!(reduce(&duplicate_guard, &BTreeSet::new()).is_err());

        let guard_after_settlement = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            settled(3, "r1", Some(1), Some(1))?,
            guard(4, "r1", 1)?,
        ];
        assert!(reduce(&guard_after_settlement, &BTreeSet::new()).is_err());

        assert!(reduce(&[guard(1, "r1", 2)?], &BTreeSet::new()).is_err());
        Ok(())
    }

    #[test]
    fn reducer_rejects_timestamp_regression_invalid_hold_and_overflow() -> anyhow::Result<()> {
        let mut regressed = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
        ];
        regressed[1].captured_at = "2026-07-31T23:59:59Z".to_owned();
        resign(&mut regressed[1])?;
        assert!(reduce(&regressed, &BTreeSet::new()).is_err());

        for hold in [0, u64::from(u32::MAX) + 1] {
            let events = vec![
                start(1, "r1", 10, "complete", "explicit_root")?,
                intent(2, "r1", "opening", "low")?,
                guard(3, "r1", hold)?,
            ];
            assert!(reduce(&events, &BTreeSet::new()).is_err());
        }

        let total_overflow = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            settled(3, "r1", Some(u64::MAX), Some(0))?,
            start(4, "r2", 20, "complete", "canonical_prefix")?,
            intent(5, "r2", "planning", "low")?,
            settled(6, "r2", Some(1), Some(0))?,
        ];
        assert!(reduce(&total_overflow, &BTreeSet::new()).is_err());

        let ppm_overflow = vec![
            start(1, "r1", 1, "complete", "explicit_root")?,
            intent(2, "r1", "opening", "low")?,
            start(3, "r2", u64::MAX, "complete", "canonical_prefix")?,
            intent(4, "r2", "planning", "low")?,
        ];
        assert!(reduce(&ppm_overflow, &BTreeSet::new()).is_err());
        Ok(())
    }

    #[test]
    fn reducer_rejects_mismatched_or_untyped_route_evidence() -> anyhow::Result<()> {
        let mut mismatch = intent(2, "r1", "planning", "low")?;
        mismatch
            .evidence
            .categorical
            .insert("route.workflow_state".to_owned(), "recovery".to_owned());
        resign(&mut mismatch)?;
        let events = vec![start(1, "r1", 10, "complete", "explicit_root")?, mismatch];
        assert!(reduce(&events, &BTreeSet::new()).is_err());

        let mut failure_text = intent(2, "r1", "planning", "low")?;
        failure_text.evidence.categorical.insert(
            "route.workflow_state".to_owned(),
            "retry after failure".to_owned(),
        );
        resign(&mut failure_text)?;
        let events = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            failure_text,
        ];
        assert!(reduce(&events, &BTreeSet::new()).is_err());
        Ok(())
    }

    #[test]
    fn typed_route_protection_is_historical_across_policy_reload() -> anyhow::Result<()> {
        let mut events = vec![start(1, "r1", 10, "complete", "explicit_root")?];
        let first = typed_intent(&events, 2, "r1", "planning", "shared", true)?;
        events.push(first);
        events.push(start(3, "r2", 20, "complete", "canonical_prefix")?);
        let second = typed_intent(&events, 4, "r2", "planning", "shared", false)?;
        events.push(second);

        let replay_with_old_policy = reduce(&events, &BTreeSet::from(["shared".to_owned()]))?;
        let replay_with_new_policy = reduce(&events, &BTreeSet::new())?;
        assert_eq!(
            replay_with_old_policy
                .health
                .consecutive_unprotected_requests,
            1
        );
        assert_eq!(replay_with_old_policy, replay_with_new_policy);
        Ok(())
    }

    fn start(
        sequence: u64,
        request_id: &str,
        bytes: u64,
        completeness: &str,
        source: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        event(
            sequence,
            Some(request_id),
            TrajectoryEventKind::RequestStarted,
            BTreeMap::from([("request.canonical_input_bytes".to_owned(), bytes)]),
            BTreeMap::from([
                ("history.completeness".to_owned(), completeness.to_owned()),
                ("correlation.source".to_owned(), source.to_owned()),
            ]),
        )
    }

    fn intent(
        sequence: u64,
        request_id: &str,
        state: &str,
        tier: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        event(
            sequence,
            Some(request_id),
            TrajectoryEventKind::RouteIntentRecorded,
            BTreeMap::new(),
            BTreeMap::from([
                (
                    "route.projection".to_owned(),
                    format!("agent_trace/v2|{state}|normal"),
                ),
                ("route.selected_tier".to_owned(), tier.to_owned()),
                ("route.workflow_state".to_owned(), state.to_owned()),
            ]),
        )
    }

    fn typed_intent(
        prefix: &[TrajectoryEvent],
        sequence: u64,
        request_id: &str,
        state: &str,
        tier: &str,
        selected_is_protected: bool,
    ) -> anyhow::Result<TrajectoryEvent> {
        let snapshot = reduce(prefix, &BTreeSet::new())?;
        let mut route = intent(sequence, request_id, state, tier)?;
        route
            .evidence
            .structural
            .insert("route.applied_clause_count".to_owned(), 0);
        route.evidence.structural.insert(
            "route.selected_is_protected".to_owned(),
            u64::from(selected_is_protected),
        );
        route
            .evidence
            .digests
            .insert("route.policy_lock".to_owned(), TEST_DIGEST.to_owned());
        route
            .evidence
            .digests
            .insert("route.health_snapshot".to_owned(), snapshot.evidence_digest);
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
        Ok(route)
    }

    fn settled(
        sequence: u64,
        request_id: &str,
        tokens: Option<u64>,
        cost: Option<u64>,
    ) -> anyhow::Result<TrajectoryEvent> {
        settled_with_outcome(sequence, request_id, tokens, cost, "succeeded")
    }

    fn settled_with_outcome(
        sequence: u64,
        request_id: &str,
        tokens: Option<u64>,
        cost: Option<u64>,
        outcome: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        let mut structural = BTreeMap::new();
        if let Some(tokens) = tokens {
            structural.insert("settlement.total_tokens".to_owned(), tokens);
        }
        if let Some(cost) = cost {
            structural.insert("settlement.cost_micro_usd".to_owned(), cost);
        }
        event(
            sequence,
            Some(request_id),
            TrajectoryEventKind::RequestSettled,
            structural,
            BTreeMap::from([("settlement.outcome".to_owned(), outcome.to_owned())]),
        )
    }

    fn guard(sequence: u64, request_id: &str, hold: u64) -> anyhow::Result<TrajectoryEvent> {
        event(
            sequence,
            Some(request_id),
            TrajectoryEventKind::GuardActivated,
            BTreeMap::from([("guard.hold_for_requests".to_owned(), hold)]),
            BTreeMap::new(),
        )
    }

    fn event(
        sequence: u64,
        request_id: Option<&str>,
        kind: TrajectoryEventKind,
        structural: BTreeMap<String, u64>,
        categorical: BTreeMap<String, String>,
    ) -> anyhow::Result<TrajectoryEvent> {
        let seconds = sequence
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("test sequence must be positive"))?;
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: format!("event-{sequence}"),
            owner_user_id: "owner-1".to_owned(),
            episode_id: "episode-1".to_owned(),
            request_id: request_id.map(str::to_owned),
            sequence,
            kind,
            evidence: TrajectoryEvidence {
                structural,
                categorical,
                digests: BTreeMap::new(),
            },
            captured_at: format!("2026-08-01T00:00:{seconds:02}Z"),
            content_digest: String::new(),
        };
        resign(&mut event)?;
        Ok(event)
    }

    fn resign(event: &mut TrajectoryEvent) -> anyhow::Result<()> {
        event.content_digest = event.semantic_digest()?;
        Ok(())
    }
}
