//! Reuse the production pure decision builder as the typed storage fixture.

use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::gateway::routing::workflow::ir::RouteProjection;
use crate::trajectory::{
    guard::{IncompleteHistoryAction, ProgressGuardPolicy},
    store::{GuardedRouteInput, build_guarded_route_batch},
    types::{HistoryCompleteness, TrajectoryEvent},
};

pub(crate) fn guarded_batch(
    prior: &[TrajectoryEvent],
    start: &TrajectoryEvent,
) -> Result<Vec<TrajectoryEvent>> {
    let input = GuardedRouteInput {
        route_event_id: format!("{}-route", start.event_id),
        guard_event_id: format!("{}-guard", start.event_id),
        policy_name: "auto:cost".into(),
        route_projection: "semantic_route/v1|code:generation|implement|normal".into(),
        request_key: "agent_trace/v2|edit|normal".into(),
        baseline_tier: Some("reference".into()),
        baseline_effort: None,
        tier_efforts: Default::default(),
        preset: Some("auto:cost".into()),
        projection: RouteProjection::parse_key("agent_trace/v2|edit|normal")
            .context("invalid fixture projection")?,
        candidate_tier: Some("economy".into()),
        policy_digest: format!("sha256:{}", "a".repeat(64)),
        experiment: None,
        route_measurement: None,
        policy: ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: None,
            max_same_projection_unprotected: None,
            max_recovery_count: None,
            max_episode_requests: Some(1),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 2,
            incomplete_history: IncompleteHistoryAction::Observe,
        },
        carries_tools: false,
        tool_use_tier: Some("strong".into()),
        tool_safe_tiers: BTreeSet::from(["strong".into()]),
    };
    let batch = build_guarded_route_batch(prior, start, HistoryCompleteness::Complete, &input)?;
    let mut events = vec![batch.route_event];
    events.extend(batch.guard_event);
    Ok(events)
}
