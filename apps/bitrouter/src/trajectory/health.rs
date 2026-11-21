use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::route::validate_recorded_route;
use super::types::{
    HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind, TrajectoryHealth,
    TrajectorySnapshot, validate_event,
};
use bitrouter_ai::policy::RouteClass;

const PPM_SCALE: u64 = 1_000_000;

enum RequestPhase {
    Started,
    Routed,
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
    first_canonical_bytes: Option<u64>,
    latest_canonical_bytes: Option<u64>,
    total_tokens: Option<u64>,
    settled_cost_micro_usd: Option<u64>,
}

pub fn reduce(events: &[TrajectoryEvent]) -> Result<TrajectorySnapshot> {
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
    let mut completeness = HistoryCompleteness::Complete;
    let mut request_count = 0_u64;
    let mut settled_request_count = 0_u64;
    let mut same_projection_streak = 0_u64;
    let mut same_selected_tier_streak = 0_u64;
    let mut total_tokens = None;
    let mut settled_cost_micro_usd = None;
    let mut first_canonical_bytes = None;
    let mut latest_canonical_bytes = None;
    let mut previous_projection: Option<String> = None;
    let mut previous_selected_tier: Option<String> = None;
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

        match event.kind {
            TrajectoryEventKind::RequestStarted => {
                let request_id = required_request_id(event)?;
                if requests.contains_key(request_id) {
                    anyhow::bail!("trajectory request '{request_id}' starts more than once")
                }
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
                if let Some(health) = validate_recorded_route(event)? {
                    let factual_prefix = snapshot_from_facts(SnapshotFacts {
                        episode_id,
                        through_sequence: event.sequence.checked_sub(1).ok_or_else(|| {
                            anyhow::anyhow!("recorded route has no factual event prefix")
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
                        first_canonical_bytes,
                        latest_canonical_bytes,
                        total_tokens,
                        settled_cost_micro_usd,
                    })?;
                    if health != factual_prefix.evidence_digest {
                        anyhow::bail!(
                            "recorded route health digest disagrees with its factual pre-intent trajectory snapshot"
                        )
                    }
                }
                let phase = requests.get_mut(request_id).ok_or_else(|| {
                    anyhow::anyhow!("route intent references unknown request '{request_id}'")
                })?;
                match phase {
                    RequestPhase::Started => *phase = RequestPhase::Routed,
                    RequestPhase::Routed => anyhow::bail!(
                        "trajectory request '{request_id}' has duplicate route intent"
                    ),
                    RequestPhase::Settled => anyhow::bail!(
                        "trajectory request '{request_id}' records intent after settlement"
                    ),
                }

                let projection = event.evidence.categorical.get("route.projection");
                let selected_tier = event.evidence.categorical.get("route.selected_tier");
                if let Some(projection) = projection
                    && RouteClass::parse_key(projection).is_none()
                {
                    anyhow::bail!("route intent has invalid semantic classification")
                }
                if projection.is_none() || selected_tier.is_none() {
                    completeness = merge_completeness(completeness, HistoryCompleteness::Unknown);
                }
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
            }
            TrajectoryEventKind::RequestSettled => {
                let request_id = required_request_id(event)?;
                let phase = requests.get_mut(request_id).ok_or_else(|| {
                    anyhow::anyhow!("settlement references unknown request '{request_id}'")
                })?;
                match phase {
                    RequestPhase::Started | RequestPhase::Routed => {
                        // Independent observers need no policy decision to
                        // retain the real request's authoritative settlement.
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
            TrajectoryEventKind::EpisodeClosed => {
                if event.request_id.is_some() {
                    anyhow::bail!("episode close event cannot identify a request")
                }
                closed = true;
            }
        }
    }

    let through_sequence = events
        .last()
        .map(|event| event.sequence)
        .ok_or_else(|| anyhow::anyhow!("trajectory reduction lost its final event"))?;
    snapshot_from_facts(SnapshotFacts {
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
        first_canonical_bytes,
        latest_canonical_bytes,
        total_tokens,
        settled_cost_micro_usd,
    })
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
        context_growth_ppm,
        total_tokens: facts.total_tokens,
        settled_cost_micro_usd: facts.settled_cost_micro_usd,
    };
    let mut snapshot = TrajectorySnapshot {
        episode_id: facts.episode_id.to_owned(),
        through_sequence: facts.through_sequence,
        health,
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
    use std::collections::BTreeMap;

    use super::reduce;
    use crate::trajectory::types::{
        HistoryCompleteness, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind,
        TrajectoryEvidence, TrajectoryHealth,
    };

    #[test]
    fn hand_literal_episode_reduces_every_health_field() -> anyhow::Result<()> {
        let mut events = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "economy")?,
            settled(3, "r1", Some(0), Some(0))?,
            start(4, "r2", 150, "complete", "canonical_prefix")?,
            intent(5, "r2", "orchestrate", "economy")?,
            start(6, "r3", 200, "complete", "canonical_prefix")?,
            intent(7, "r3", "implement", "protected")?,
            settled(8, "r2", Some(20), None)?,
            settled(9, "r3", Some(30), Some(10))?,
            start(10, "r4", 250, "complete", "canonical_prefix")?,
            intent(11, "r4", "implement", "economy")?,
            settled(12, "r4", None, Some(0))?,
        ];

        for (index, event) in events.iter_mut().enumerate().skip(8) {
            event.captured_at = format!("2026-08-01T00:00:{:02}Z", index + 1);
            resign(event)?;
        }
        let snapshot = reduce(&events)?;

        assert_eq!(snapshot.episode_id, "episode-1");
        assert_eq!(snapshot.through_sequence, 12);
        assert_eq!(
            snapshot.health,
            TrajectoryHealth {
                completeness: HistoryCompleteness::Complete,
                request_count: 4,
                settled_request_count: 4,
                unsettled_request_count: 0,
                elapsed_ms: 12_000,
                latest_projection: Some("code:generation|implement|normal".to_string()),
                same_projection_streak: 2,
                same_selected_tier_streak: 1,
                context_growth_ppm: Some(1_500_000),
                total_tokens: None,
                settled_cost_micro_usd: None,
            }
        );
        assert!(snapshot.evidence_digest.starts_with("sha256:"));
        assert_eq!(snapshot.evidence_digest.len(), 71);
        Ok(())
    }

    #[test]
    fn explicit_zero_totals_remain_known_and_failure_settlements_count() -> anyhow::Result<()> {
        let events = vec![
            start(1, "r1", 40, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "economy")?,
            settled_with_outcome(3, "r1", Some(0), Some(0), "failed")?,
        ];

        let snapshot = reduce(&events)?;

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
            intent(2, "r1", "orchestrate", "economy")?,
            settled(3, "r1", None, None)?,
            start(4, "r2", 80, "complete", "canonical_prefix")?,
            intent(5, "r2", "orchestrate", "economy")?,
            settled(6, "r2", Some(100), Some(200))?,
        ];

        let snapshot = reduce(&events)?;

        assert_eq!(snapshot.health.settled_request_count, 2);
        assert_eq!(snapshot.health.total_tokens, None);
        assert_eq!(snapshot.health.settled_cost_micro_usd, None);
        Ok(())
    }

    #[test]
    fn unmatched_start_is_temporarily_unknown_and_valid_intent_clears_it() -> anyhow::Result<()> {
        let started = vec![start(1, "r1", 40, "complete", "explicit_root")?];
        assert_eq!(
            reduce(&started)?.health.completeness,
            HistoryCompleteness::Unknown
        );
        let before_intent = reduce(&started)?;
        assert_eq!(before_intent.health.request_count, 1);
        assert_eq!(before_intent.health.settled_request_count, 0);
        assert_eq!(before_intent.health.unsettled_request_count, 1);
        assert_eq!(before_intent.health.total_tokens, None);
        assert_eq!(before_intent.health.settled_cost_micro_usd, None);

        let routed = vec![
            started[0].clone(),
            intent(2, "r1", "orchestrate", "economy")?,
        ];
        assert_eq!(
            reduce(&routed)?.health.completeness,
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
            intent(2, "r1", "orchestrate", "economy")?,
            start(3, "r2", 80, "incomplete", "unresolved")?,
            intent(4, "r2", "orchestrate", "economy")?,
        ];

        let snapshot = reduce(&events)?;

        assert_eq!(
            snapshot.health.completeness,
            HistoryCompleteness::Incomplete
        );
        Ok(())
    }

    #[test]
    fn route_streaks_reset_on_switch_and_missing_tier() -> anyhow::Result<()> {
        let mut missing_tier = intent(8, "r4", "verify", "economy")?;
        missing_tier
            .evidence
            .categorical
            .remove("route.selected_tier");
        resign(&mut missing_tier)?;
        let events = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            start(3, "r2", 110, "complete", "canonical_prefix")?,
            intent(4, "r2", "orchestrate", "low")?,
            start(5, "r3", 120, "complete", "canonical_prefix")?,
            intent(6, "r3", "verify", "high")?,
            start(7, "r4", 130, "complete", "canonical_prefix")?,
            missing_tier,
        ];

        let snapshot = reduce(&events)?;

        assert_eq!(snapshot.health.same_projection_streak, 2);
        assert_eq!(snapshot.health.same_selected_tier_streak, 0);
        assert_eq!(snapshot.health.completeness, HistoryCompleteness::Unknown);
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
            intent(2, "r1", "orchestrate", "low")?,
            missing,
            intent(4, "r2", "orchestrate", "low")?,
            start(5, "r3", 100, "complete", "canonical_prefix")?,
            intent(6, "r3", "verify", "low")?,
        ];

        assert_eq!(reduce(&events)?.health.context_growth_ppm, Some(0));

        let zero_first = vec![
            start(1, "r1", 0, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
        ];
        assert_eq!(reduce(&zero_first)?.health.context_growth_ppm, None);

        let mut missing_first = start(1, "r1", 1, "complete", "explicit_root")?;
        missing_first
            .evidence
            .structural
            .remove("request.canonical_input_bytes");
        resign(&mut missing_first)?;
        let missing_first = vec![
            missing_first,
            intent(2, "r1", "orchestrate", "low")?,
            start(3, "r2", 100, "complete", "canonical_prefix")?,
            intent(4, "r2", "orchestrate", "low")?,
        ];
        assert_eq!(reduce(&missing_first)?.health.context_growth_ppm, None);

        let mut missing_latest = start(3, "r2", 1, "complete", "canonical_prefix")?;
        missing_latest
            .evidence
            .structural
            .remove("request.canonical_input_bytes");
        resign(&mut missing_latest)?;
        let missing_latest = vec![
            start(1, "r1", 100, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            missing_latest,
            intent(4, "r2", "orchestrate", "low")?,
        ];
        assert_eq!(reduce(&missing_latest)?.health.context_growth_ppm, None);
        Ok(())
    }

    #[test]
    fn reducer_rejects_sequence_identity_digest_and_terminal_corruption() -> anyhow::Result<()> {
        let base = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
        ];

        let mut gap = base.clone();
        gap[1].sequence = 3;
        resign(&mut gap[1])?;
        assert!(reduce(&gap).is_err());

        let mut duplicate = base.clone();
        duplicate[1].sequence = 1;
        resign(&mut duplicate[1])?;
        assert!(reduce(&duplicate).is_err());

        let mut owner = base.clone();
        owner[1].owner_user_id = "owner-2".to_owned();
        resign(&mut owner[1])?;
        assert!(reduce(&owner).is_err());

        let mut episode = base.clone();
        episode[1].episode_id = "episode-2".to_owned();
        resign(&mut episode[1])?;
        assert!(reduce(&episode).is_err());

        let mut digest = base.clone();
        digest[1].content_digest = format!("sha256:{}", "0".repeat(64));
        assert!(reduce(&digest).is_err());

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
        assert!(reduce(&closed).is_err());
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
        assert!(reduce(&close_only).is_err());

        assert!(reduce(&[settled(1, "r1", Some(1), Some(1))?]).is_err());

        let duplicate_settlement = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            settled(3, "r1", Some(1), Some(1))?,
            settled(4, "r1", Some(2), Some(1))?,
        ];
        assert!(reduce(&duplicate_settlement).is_err());

        assert!(reduce(&[intent(1, "r1", "orchestrate", "low")?]).is_err());

        let duplicate_route = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            intent(3, "r1", "orchestrate", "low")?,
        ];
        assert!(reduce(&duplicate_route).is_err());

        let settlement_for_unknown_request = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            settled(2, "r2", Some(1), Some(1))?,
        ];
        assert!(reduce(&settlement_for_unknown_request).is_err());

        let intent_after_settlement = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            settled(3, "r1", Some(1), Some(1))?,
            intent(4, "r1", "orchestrate", "low")?,
        ];
        assert!(reduce(&intent_after_settlement).is_err());

        Ok(())
    }

    #[test]
    fn reducer_rejects_timestamp_regression_and_overflow() -> anyhow::Result<()> {
        let mut regressed = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
        ];
        regressed[1].captured_at = "2026-07-31T23:59:59Z".to_owned();
        resign(&mut regressed[1])?;
        assert!(reduce(&regressed).is_err());

        let total_overflow = vec![
            start(1, "r1", 10, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            settled(3, "r1", Some(u64::MAX), Some(0))?,
            start(4, "r2", 20, "complete", "canonical_prefix")?,
            intent(5, "r2", "orchestrate", "low")?,
            settled(6, "r2", Some(1), Some(0))?,
        ];
        assert!(reduce(&total_overflow).is_err());

        let ppm_overflow = vec![
            start(1, "r1", 1, "complete", "explicit_root")?,
            intent(2, "r1", "orchestrate", "low")?,
            start(3, "r2", u64::MAX, "complete", "canonical_prefix")?,
            intent(4, "r2", "orchestrate", "low")?,
        ];
        assert!(reduce(&ppm_overflow).is_err());
        Ok(())
    }

    #[test]
    fn reducer_rejects_invalid_semantic_route_evidence() -> anyhow::Result<()> {
        for value in [
            "code:generation|invalid|normal",
            "retry after failure",
            "code:generation|implement|normal|extra",
        ] {
            let mut route = intent(2, "r1", "implement", "low")?;
            route
                .evidence
                .categorical
                .insert("route.projection".into(), value.into());
            resign(&mut route)?;
            assert!(reduce(&[start(1, "r1", 10, "complete", "explicit_root")?, route]).is_err());
        }
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
                    format!("code:generation|{state}|normal"),
                ),
                ("route.selected_tier".to_owned(), tier.to_owned()),
            ]),
        )
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
                routing_evidence: Vec::new(),
                structural,
                categorical,
                digests: BTreeMap::new(),
                route_measurement: None,
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
