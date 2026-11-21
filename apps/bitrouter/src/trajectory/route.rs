//! Validation of recorded model-selection facts; this module never selects a model.

use super::types::TrajectoryEvent;
use anyhow::{Result, ensure};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteIntent {
    pub candidate_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub trajectory_snapshot_digest: String,
    pub policy_digest: String,
}

/// A recorded snapshot must be paired with a policy identity, never an orphaned marker.
pub(super) fn validate_recorded_route(event: &TrajectoryEvent) -> Result<Option<&str>> {
    match (
        event.evidence.digests.get("route.health_snapshot"),
        event.evidence.digests.get("route.policy_lock"),
    ) {
        (None, None) => Ok(None),
        (Some(health), Some(_)) => {
            ensure!(
                event.evidence.categorical.contains_key("route.projection"),
                "recorded route requires semantic classification"
            );
            Ok(Some(health.as_str()))
        }
        _ => anyhow::bail!("recorded route requires policy and health digests"),
    }
}
