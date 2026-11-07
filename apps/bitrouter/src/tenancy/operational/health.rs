//! Exact process-local circuit ownership. Deadlines are elapsed durations so
//! even a full-width Retry-After cannot overflow an Instant or shorten a block.

use std::time::{Duration, Instant};

use bitrouter_ai::tenancy::admission::{HealthObservation, HealthProfileV1};

use super::state::{Denial, HealthOutcome};

#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
pub(super) struct Health {
    phase: Phase,
    active: u32,
}

#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
enum Phase {
    #[default]
    Healthy,
    Failures(u32),
    Open {
        since: Instant,
        duration: Duration,
    },
    Probe {
        since: Instant,
        duration: Duration,
        lease: u64,
    },
}

impl Health {
    pub(super) fn observe(&self, now: Instant) -> HealthObservation {
        match self.phase {
            Phase::Healthy | Phase::Failures(_) => HealthObservation::Healthy,
            Phase::Open { since, duration }
            | Phase::Probe {
                since, duration, ..
            } if now.saturating_duration_since(since) >= duration => HealthObservation::ProbeReady,
            _ => HealthObservation::Blocked,
        }
    }

    pub(super) fn claim(
        &mut self,
        rules: &HealthProfileV1,
        next_lease: &mut u64,
        now: Instant,
    ) -> Result<Option<u64>, Denial> {
        let active = self.active.checked_add(1).ok_or(Denial::Capacity)?;
        let lease = match self.observe(now) {
            HealthObservation::Healthy => None,
            HealthObservation::ProbeReady => {
                let lease = next_lease.checked_add(1).ok_or(Denial::Capacity)?;
                *next_lease = lease;
                self.phase = Phase::Probe {
                    since: now,
                    duration: Duration::from_secs(rules.probe_lease_seconds),
                    lease,
                };
                Some(lease)
            }
            _ => return Err(Denial::Circuit),
        };
        self.active = active;
        Ok(lease)
    }

    pub(super) fn finish(
        &mut self,
        lease: Option<u64>,
        rules: &HealthProfileV1,
        outcome: HealthOutcome,
        now: Instant,
    ) {
        self.active = self.active.saturating_sub(1);
        // A late ordinary request may finish before the next probe starts, but
        // neither it nor an expired probe can complete a newer probe's lease.
        let owns = match self.phase {
            Phase::Probe { lease: current, .. } => lease == Some(current),
            _ => lease.is_none(),
        };
        if !owns {
            return;
        }
        match outcome {
            HealthOutcome::Neutral => {
                if lease.is_some() {
                    self.phase = Phase::Open {
                        since: now,
                        duration: Duration::ZERO,
                    };
                }
            }
            HealthOutcome::Success => self.phase = Phase::Healthy,
            HealthOutcome::Persistent => {
                self.phase = Phase::Open {
                    since: now,
                    duration: Duration::from_secs(rules.persistent_cooldown_seconds),
                };
            }
            HealthOutcome::Transient { retry_after } => {
                let failures = match self.phase {
                    Phase::Healthy => 1,
                    Phase::Failures(count) => count.saturating_add(1),
                    Phase::Open { .. } | Phase::Probe { .. } => rules.failure_threshold,
                };
                self.phase = if failures >= rules.failure_threshold {
                    Phase::Open {
                        since: now,
                        duration: Duration::from_secs(
                            rules.transient_cooldown_seconds.max(retry_after),
                        ),
                    }
                } else {
                    Phase::Failures(failures)
                };
            }
        }
    }
}
