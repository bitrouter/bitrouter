//! Conservative pilot policy. Pressure gates tighten operator-provided limits;
//! no signal is converted into an estimate of sellable or guaranteed capacity.

use std::time::Duration;

use serde::Serialize;

use super::{
    config::ConnectorConfig,
    observer::{ObservationStatus, PoolSnapshot, SourceFreshness},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    ObservationOnly,
    ManualPause,
    ClockUntrusted,
    MissingObservation,
    ScopeMismatch,
    TelemetryUnknown,
    TelemetryStale,
    SourceFreshnessUnknown,
    MainPressure,
    HealthWindow,
    UnknownExecution,
    BudgetExpired,
    InflightLimit,
    DispatchRate,
    JournalFull,
    StorageUnavailable,
}

impl PauseReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::ObservationOnly => "observation_only",
            Self::ManualPause => "manual_pause",
            Self::ClockUntrusted => "clock_untrusted",
            Self::MissingObservation => "missing_observation",
            Self::ScopeMismatch => "scope_mismatch",
            Self::TelemetryUnknown => "telemetry_unknown",
            Self::TelemetryStale => "telemetry_stale",
            Self::SourceFreshnessUnknown => "source_freshness_unknown",
            Self::MainPressure => "main_pressure",
            Self::HealthWindow => "health_window",
            Self::UnknownExecution => "unknown_execution",
            Self::BudgetExpired => "budget_expired",
            Self::InflightLimit => "inflight_limit",
            Self::DispatchRate => "dispatch_rate",
            Self::JournalFull => "journal_full",
            Self::StorageUnavailable => "storage_unavailable",
        }
    }
}

/// Pure pressure guard over explicit operator limits, not a residual-capacity
/// estimator. Callers re-evaluate observations at reservation and sending.
/// Aggregate gauges include extra traffic; every unfinished local attempt is
/// also added because sample inclusion cannot be proven. Reflected work can
/// therefore be counted twice and cause deliberately lower admission.
pub(super) fn pressure(
    config: &ConnectorConfig,
    snapshot: &PoolSnapshot,
    now_ms: i64,
    retained_for: Duration,
    local_unfinished: u64,
) -> Result<(), PauseReason> {
    let Some(limits) = &config.execution else {
        return Err(PauseReason::ObservationOnly);
    };
    if !limits.enabled {
        return Err(PauseReason::ObservationOnly);
    }
    if snapshot.pool_id != config.pool.id
        || snapshot.model != config.pool.model
        || snapshot.members.len() != config.pool.members.len()
        || snapshot.summary.declared_members != config.pool.members.len()
    {
        return Err(PauseReason::ScopeMismatch);
    }
    let max_age = config.observation.max_sample_age_ms;
    if snapshot.observed_at_ms > now_ms {
        return Err(PauseReason::ClockUntrusted);
    }
    if u64::try_from(now_ms.saturating_sub(snapshot.observed_at_ms))
        .map_or(true, |age| age > max_age)
        || retained_for > Duration::from_millis(max_age)
    {
        return Err(PauseReason::TelemetryStale);
    }
    for (member, declared) in snapshot.members.iter().zip(&config.pool.members) {
        if member.member_id != declared.id
            || member.declared_engines != declared.engines
            || !member.metrics_scope_validated
        {
            return Err(PauseReason::ScopeMismatch);
        }
        if member.status != ObservationStatus::Observed || member.metrics.is_none() {
            return Err(PauseReason::TelemetryUnknown);
        }
        let freshness = &member.freshness;
        let Some(collected) = freshness.collection_completed_at_ms else {
            return Err(PauseReason::TelemetryUnknown);
        };
        if collected > now_ms {
            return Err(PauseReason::ClockUntrusted);
        }
        if now_ms.saturating_sub(collected) as u64 > max_age
            || freshness.collection_age_ms.is_none_or(|age| {
                Duration::from_millis(age).saturating_add(retained_for)
                    > Duration::from_millis(max_age)
            })
        {
            return Err(PauseReason::TelemetryStale);
        }
        match freshness.source {
            SourceFreshness::ExplicitTimestamps => {}
            SourceFreshness::CollectionOnly if limits.allow_collection_only_metrics => {}
            _ => return Err(PauseReason::SourceFreshnessUnknown),
        }
        if let Some(sample) = freshness.oldest_source_sample_at_ms {
            if sample > now_ms {
                return Err(PauseReason::ClockUntrusted);
            }
            if now_ms.saturating_sub(sample) as u64 > max_age
                || freshness.oldest_source_sample_age_ms.is_none_or(|age| {
                    Duration::from_millis(age).saturating_add(retained_for)
                        > Duration::from_millis(max_age)
                })
            {
                return Err(PauseReason::TelemetryStale);
            }
        } else if !limits.allow_collection_only_metrics {
            return Err(PauseReason::SourceFreshnessUnknown);
        }
    }
    let metrics = snapshot
        .summary
        .metrics
        .as_ref()
        .ok_or(PauseReason::TelemetryUnknown)?;
    if metrics.running_requests.saturating_add(local_unfinished) > limits.max_running_requests
        || metrics.running_requests >= limits.max_running_requests
        || metrics.waiting_requests > limits.max_waiting_requests
        || !metrics.kv_cache_usage_fraction_max.is_finite()
        || metrics.kv_cache_usage_fraction_max >= limits.max_kv_cache_usage_fraction
    {
        return Err(PauseReason::MainPressure);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct BudgetDecision {
    pub pool_id: String,
    pub pool_generation: String,
    pub workload_class: &'static str,
    pub policy_mode: &'static str,
    pub policy_revision: String,
    pub owner_epoch: u64,
    pub sequence: u64,
    pub decision_id: String,
    pub issued_at_ms: i64,
    pub valid_until_ms: i64,
    pub observed_at_ms: Option<i64>,
    pub max_dispatch_rps: f64,
    pub burst: u32,
    pub max_extra_inflight: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub reason: Option<PauseReason>,
}
