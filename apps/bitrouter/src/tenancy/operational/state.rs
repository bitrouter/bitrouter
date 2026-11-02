//! Synchronous operational state. Call admission with the authority read guard
//! held; never acquire authority while holding this runtime lock.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    time::Instant,
};

use bitrouter_ai::tenancy::{
    admission::{AvailabilityObservation, HealthObservation, HealthProfileV1, RateObservationV1},
    authority::ResourceKey,
    configuration::ResourceRef,
    identity::{InstallationId, OwnerId, ResourceId},
};

use super::{
    health::Health,
    rate::{self, Rates},
};

use crate::tenancy::resources::admission::profile::View as ProfileView;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HealthScope {
    ProfileLocal(ResourceKey),
    SharedGroup(OwnerId, ResourceId),
}

type HealthKey = (InstallationId, HealthScope);

fn health_binding<'a>(
    installation: &InstallationId,
    reference: &ResourceRef,
    profile: ProfileView<'a>,
) -> Result<Option<(HealthKey, &'a HealthProfileV1)>, Denial> {
    let scope = match profile {
        ProfileView::Local(_) => HealthScope::ProfileLocal(reference.key()),
        ProfileView::Shared(profile) => match (&profile.health, &profile.health_group_id) {
            (Some(_), Some(group)) => {
                HealthScope::SharedGroup(reference.owner_id.clone(), group.clone())
            }
            (None, None) => return Ok(None),
            _ => return Err(Denial::Unavailable),
        },
    };
    Ok(profile
        .health()
        .map(|rules| ((installation.clone(), scope), rules)))
}

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
#[cfg_attr(test, derive(Debug))]
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
    #[cfg(test)]
    pub(in crate::tenancy) fn test_state(&self) -> Result<String, Denial> {
        self.0
            .try_lock()
            .map(|state| format!("{state:?}"))
            .map_err(read_lock_failure)
    }

    pub(in crate::tenancy) fn admit<'a>(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: impl Into<ProfileView<'a>>,
    ) -> Result<Ticket, Denial> {
        self.admit_clock(installation, reference, rules.into(), Instant::now)
    }

    #[cfg(test)]
    pub(super) fn admit_at<'a>(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: impl Into<ProfileView<'a>>,
        now: Instant,
    ) -> Result<Ticket, Denial> {
        self.admit_clock(installation, reference, rules.into(), || now)
    }

    fn admit_clock(
        self: &Arc<Self>,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: ProfileView<'_>,
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
        let binding = if let Some((key, rules)) = health_binding(installation, reference, rules)? {
            if !health.contains_key(&key) {
                if health.len() >= super::MAX_RUNTIME_KEYS {
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
        if let Some(rate) = rules.rate()
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

    /// Bounded management reads do not wait on admission or completion work.
    pub(in crate::tenancy) fn try_snapshot(&self) -> Result<ReadSnapshot<'_>, Denial> {
        let state = self.0.try_lock().map_err(read_lock_failure)?;
        Ok(ReadSnapshot {
            state,
            now: Instant::now(),
        })
    }

    #[cfg(test)]
    pub(super) fn observe<'a>(
        &self,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: impl Into<ProfileView<'a>>,
        now: Instant,
    ) -> Result<View, Denial> {
        let mut snapshot = self.snapshot()?;
        snapshot.now = now;
        Ok(snapshot.observe(installation, reference, rules))
    }
}

fn read_lock_failure<T>(error: TryLockError<T>) -> Denial {
    match error {
        TryLockError::WouldBlock => Denial::Capacity,
        TryLockError::Poisoned(_) => Denial::Unavailable,
    }
}

impl ReadSnapshot<'_> {
    pub(in crate::tenancy) fn observe<'a>(
        &self,
        installation: &InstallationId,
        reference: &ResourceRef,
        rules: impl Into<ProfileView<'a>>,
    ) -> View {
        let state = &self.state;
        let now = self.now;
        let rules = rules.into();
        let binding = match health_binding(installation, reference, rules) {
            Ok(binding) => binding,
            Err(_) => {
                return View {
                    rate: None,
                    observed_shared_count: None,
                    health: HealthObservation::Unavailable,
                    availability: AvailabilityObservation::AuthorityDenied,
                };
            }
        };
        let health = if binding.is_none() {
            HealthObservation::Disabled
        } else {
            binding
                .as_ref()
                .and_then(|(key, _)| state.health.get(key))
                .map_or(HealthObservation::Healthy, |entry| entry.observe(now))
        };
        let rate = rules.rate().map(|rate| RateObservationV1 {
            member_rpm: rate.member_rpm,
            shared_count: state
                .rates
                .count(&rate_key(installation, reference, &rate.rate_group_id), now),
        });
        let observed_shared_count = rules.observed_group().map(|group| {
            state
                .rates
                .count(&rate_key(installation, reference, group), now)
        });
        let capacity = (binding
            .as_ref()
            .is_none_or(|(key, _)| state.health.contains_key(key))
            || state.health.len() < super::MAX_RUNTIME_KEYS)
            && rules.rate().is_none_or(|rate| {
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

#[cfg(test)]
mod profile_read_errors {
    use super::*;
    #[test]
    fn poison_is_unavailable_while_contention_is_capacity() {
        assert_eq!(
            read_lock_failure(TryLockError::Poisoned(std::sync::PoisonError::new(()))),
            Denial::Unavailable
        );
        assert_eq!(
            read_lock_failure::<()>(TryLockError::WouldBlock),
            Denial::Capacity
        );
    }
}
