//! Synchronous operational state. Call admission with the authority read guard
//! held; never acquire authority while holding this runtime lock.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use bitrouter_ai::tenancy::{
    admission::{
        AdmissionProfileV1, AvailabilityObservation, HealthObservation, HealthProfileV1,
        RateObservationV1,
    },
    authority::ResourceKey,
    configuration::ResourceRef,
    identity::InstallationId,
};

use super::{
    health::Health,
    rate::{self, Rates},
};

type HealthKey = (InstallationId, ResourceKey);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Denial {
    Rate,
    Circuit,
    Capacity,
    Unavailable,
}

#[derive(Clone, Copy)]
pub(super) enum HealthOutcome {
    Neutral,
    Success,
    Persistent,
    Transient { retry_after: u64 },
}

#[derive(Default)]
struct State {
    rates: Rates,
    health: HashMap<HealthKey, Health>,
    next_lease: u64,
}

#[derive(Default)]
pub(in crate::tenancy) struct Runtime(Mutex<State>);

/// One bounded read view for all candidates. Never held over I/O or an await.
pub(in crate::tenancy) struct ReadSnapshot<'a> {
    state: MutexGuard<'a, State>,
    now: Instant,
}

/// Not cloneable. One admitted hop owns its health lease until a typed outcome
/// or neutral drop. The already charged rate timestamp is never refunded.
pub(in crate::tenancy) struct Ticket {
    runtime: Arc<Runtime>,
    health: Option<(HealthKey, HealthProfileV1, Option<u64>)>,
}

pub(in crate::tenancy) struct View {
    pub rate: Option<RateObservationV1>,
    pub observed_shared_count: Option<u64>,
    pub health: HealthObservation,
    pub availability: AvailabilityObservation,
}

fn rate_key(
    installation: &InstallationId,
    reference: &ResourceRef,
    group: &bitrouter_ai::tenancy::identity::ResourceId,
) -> rate::Key {
    (
        installation.clone(),
        reference.owner_id.clone(),
        group.clone(),
    )
}

impl Runtime {
    pub(in crate::tenancy) fn admit(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: &AdmissionProfileV1,
    ) -> Result<Ticket, Denial> {
        self.admit_clock(installation, reference, rules, Instant::now)
    }

    #[cfg(test)]
    pub(super) fn admit_at(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: &AdmissionProfileV1,
        now: Instant,
    ) -> Result<Ticket, Denial> {
        self.admit_clock(installation, reference, rules, || now)
    }

    fn admit_clock(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: &AdmissionProfileV1,
        clock: impl FnOnce() -> Instant,
    ) -> Result<Ticket, Denial> {
        let mut state = self.0.lock().map_err(|_| Denial::Unavailable)?;
        // Stamp after acquiring the lock. Contending callers cannot append
        // timestamps out of order or spend a timestamp taken before waiting.
        let now = clock();
        let State {
            health,
            rates,
            next_lease,
        } = &mut *state;
        let binding = if let Some(rules) = &rules.health {
            let key = (installation.clone(), reference.key());
            if !health.contains_key(&key) {
                if health.len() >= super::super::resources::lifecycle::MAX_RECORDS {
                    return Err(Denial::Capacity);
                }
                health.try_reserve(1).map_err(|_| Denial::Capacity)?;
            }
            let entry = health.entry(key.clone()).or_default();
            let lease = entry.claim(rules, next_lease, now)?;
            Some((key, rules.clone(), lease))
        } else {
            None
        };
        if let Some(rate) = &rules.rate
            && let Err(error) = rates.admit(
                rate_key(installation, reference, &rate.rate_group_id),
                rate.member_rpm,
                now,
            )
        {
            if let Some((key, rules, lease)) = &binding
                && let Some(entry) = health.get_mut(key)
            {
                entry.finish(*lease, rules, HealthOutcome::Neutral, now);
            }
            return Err(error);
        }
        Ok(Ticket {
            runtime: self.clone(),
            health: binding,
        })
    }

    pub(in crate::tenancy) fn snapshot(&self) -> Result<ReadSnapshot<'_>, Denial> {
        let state = self.0.lock().map_err(|_| Denial::Unavailable)?;
        Ok(ReadSnapshot {
            state,
            now: Instant::now(),
        })
    }

    #[cfg(test)]
    pub(super) fn observe(
        &self,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: &AdmissionProfileV1,
        now: Instant,
    ) -> Result<View, Denial> {
        let mut snapshot = self.snapshot()?;
        snapshot.now = now;
        Ok(snapshot.observe(installation, reference, rules))
    }
}

impl ReadSnapshot<'_> {
    pub(in crate::tenancy) fn observe(
        &self,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: &AdmissionProfileV1,
    ) -> View {
        let state = &self.state;
        let now = self.now;
        let key = (installation.clone(), reference.key());
        let health = if rules.health.is_none() {
            HealthObservation::Disabled
        } else {
            state
                .health
                .get(&key)
                .map_or(HealthObservation::Healthy, |entry| entry.observe(now))
        };
        let rate = rules.rate.as_ref().map(|rate| RateObservationV1 {
            member_rpm: rate.member_rpm,
            shared_count: state
                .rates
                .count(&rate_key(installation, reference, &rate.rate_group_id), now),
        });
        let observed_shared_count = rules.observe_rate_group_id.as_ref().map(|group| {
            state
                .rates
                .count(&rate_key(installation, reference, group), now)
        });
        let capacity = (rules.health.is_none()
            || state.health.contains_key(&key)
            || state.health.len() < super::super::resources::lifecycle::MAX_RECORDS)
            && rules.rate.as_ref().is_none_or(|rate| {
                state
                    .rates
                    .has_capacity(&rate_key(installation, reference, &rate.rate_group_id), now)
            });
        let availability = if health == HealthObservation::Blocked {
            AvailabilityObservation::CircuitBlocked
        } else if rate
            .as_ref()
            .is_some_and(|rate| rate.shared_count >= u64::from(rate.member_rpm))
        {
            AvailabilityObservation::RateLimited
        } else if !capacity {
            AvailabilityObservation::CapacityDenied
        } else {
            AvailabilityObservation::Eligible
        };
        View {
            rate,
            observed_shared_count,
            health,
            availability,
        }
    }
}

impl Ticket {
    pub(super) fn finish(mut self, outcome: HealthOutcome, now: Instant) {
        self.release(outcome, now);
    }

    fn release(&mut self, outcome: HealthOutcome, now: Instant) {
        if let Some((key, rules, lease)) = self.health.take()
            && let Ok(mut state) = self.runtime.0.lock()
            && let Some(entry) = state.health.get_mut(&key)
        {
            entry.finish(lease, &rules, outcome, now);
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.release(HealthOutcome::Neutral, Instant::now());
    }
}
