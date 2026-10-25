//! Bounded local execution capacity, independent of grants and configuration revisions.
//!
//! Each acquisition compares its requested limits with the tenant's shared totals.
//! Narrower limits never revoke existing leases. FIFO within each tenant prevents
//! a newer, wider request from bypassing an older queued request; other tenants
//! may proceed while that tenant is full. Authority must be rechecked by the caller
//! after acquisition and before every provider send.

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use bitrouter_ai::tenancy::{configuration::Limits, identity::TenantId, operation::ErrorCode};
use tokio::{sync::Notify, time::Instant};

const QUEUE_WAIT: Duration = Duration::from_secs(5);
const PROFILE: Profile = Profile {
    tenant_active: 16,
    tenant_queue: 32,
    instance_active: 128,
    instance_queue: 256,
};

#[derive(Clone, Copy)]
struct Profile {
    tenant_active: usize,
    tenant_queue: usize,
    instance_active: usize,
    instance_queue: usize,
}

/// One process's bounded tenant execution admission controller.
pub(super) struct Admission {
    profile: Profile,
    state: Mutex<State>,
    drained: Notify,
}

/// Non-clonable ownership of one tenant and one instance execution slot.
///
/// Keep this lease with the execution through provider/body cleanup, rather than
/// with the HTTP waiter. Dropping it releases both counters in one critical section.
pub(super) struct Lease {
    admission: Arc<Admission>,
    tenant: TenantId,
}

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
    tenants: BTreeMap<TenantId, Counts>,
    queue: VecDeque<Arc<Waiter>>,
}

#[derive(Default)]
struct Counts {
    active: usize,
    queued: usize,
}

struct Waiter {
    tenant: TenantId,
    active_limit: usize,
    deadline: Instant,
    ready: Notify,
}

enum Registration {
    Active,
    Queued(Arc<Waiter>),
}

/// Registers cancellation ownership before the first await. There is no active
/// reservation in a notification, so a dropped/unpolled waiter cannot leak one.
struct Queued {
    admission: Arc<Admission>,
    waiter: Arc<Waiter>,
}

impl Admission {
    #[cfg(test)]
    pub(super) fn queued_for_test(&self) -> usize {
        self.lock().queue.len()
    }

    /// Construct the fixed local profile. Request limits can only narrow it.
    pub fn new() -> Arc<Self> {
        Self::build(PROFILE)
    }

    fn build(profile: Profile) -> Arc<Self> {
        Arc::new(Self {
            profile,
            state: Mutex::new(State::default()),
            drained: Notify::new(),
        })
    }

    #[cfg(test)]
    fn with_profile(profile: Profile) -> Arc<Self> {
        Self::build(Profile {
            tenant_active: profile.tenant_active.min(PROFILE.tenant_active),
            tenant_queue: profile.tenant_queue.min(PROFILE.tenant_queue),
            instance_active: profile.instance_active.min(PROFILE.instance_active),
            instance_queue: profile.instance_queue.min(PROFILE.instance_queue),
        })
    }

    /// Acquire capacity, waiting at most five seconds if a queue slot is available.
    ///
    /// # Errors
    /// Returns `CapacityExceeded` for disabled/full capacity or queue expiry, and
    /// `AuthorityUnavailable` after close. A zero queue limit permits immediate
    /// admission only. No grant, credential or revision is retained here.
    pub async fn acquire(
        self: &Arc<Self>,
        tenant: &TenantId,
        limits: &Limits,
    ) -> Result<Lease, ErrorCode> {
        let active_limit = usize::from(limits.active_executions).min(self.profile.tenant_active);
        let queue_limit = usize::from(limits.queued_work).min(self.profile.tenant_queue);
        let registration = self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if active_limit == 0 || self.profile.instance_active == 0 {
                return Err(ErrorCode::CapacityExceeded);
            }
            let (active, queued) = state
                .tenants
                .get(tenant)
                .map_or((0, 0), |counts| (counts.active, counts.queued));
            if active < active_limit
                && state.active < self.profile.instance_active
                && queued == 0
                && state.next_eligible(self.profile).is_none()
            {
                state.reserve(tenant);
                return Ok(Registration::Active);
            }
            if queued >= queue_limit || state.queue.len() >= self.profile.instance_queue {
                return Err(ErrorCode::CapacityExceeded);
            }
            let waiter = Arc::new(Waiter {
                tenant: tenant.clone(),
                active_limit,
                deadline: Instant::now() + QUEUE_WAIT,
                ready: Notify::new(),
            });
            state.tenants.entry(tenant.clone()).or_default().queued += 1;
            state.queue.push_back(waiter.clone());
            Ok(Registration::Queued(waiter))
        })?;
        if let Registration::Queued(waiter) = registration {
            let queued = Queued {
                admission: self.clone(),
                waiter,
            };
            loop {
                let acquired = self.update(|state| {
                    if state.closed {
                        return Err(ErrorCode::AuthorityUnavailable);
                    }
                    if Instant::now() >= queued.waiter.deadline {
                        return Err(ErrorCode::CapacityExceeded);
                    }
                    if state
                        .next_eligible(self.profile)
                        .is_some_and(|next| Arc::ptr_eq(next, &queued.waiter))
                    {
                        state.remove(&queued.waiter);
                        state.reserve(tenant);
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                })?;
                if acquired {
                    break;
                }
                // notify_one stores a permit even before this future is polled.
                // One waiter owns each Notify; notification never grants capacity.
                // https://docs.rs/tokio/latest/tokio/sync/struct.Notify.html
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(queued.waiter.deadline) => {}
                    () = queued.waiter.ready.notified() => {}
                }
            }
        }
        // No await separates reserving both counters from transferring ownership.
        Ok(Lease {
            admission: self.clone(),
            tenant: tenant.clone(),
        })
    }

    /// Reject new and queued acquisitions, waking all queued callers immediately.
    /// Existing leases remain valid until their owners release them.
    pub fn close(&self) {
        self.update(|state| state.closed = true);
    }

    /// Wait until no queued acquisition or active lease remains.
    /// Close first for shutdown; the caller owns any shutdown timeout policy.
    pub async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            // Register before observing counters: a release between the check
            // and await must wake every drain waiter, not just one of them.
            notified.as_mut().enable();
            if self.update(|state| state.active == 0 && state.queue.is_empty()) {
                return;
            }
            notified.await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.closed = true;
                state
            }
        }
    }

    /// Every mutation and its next-waiter selection share the same short lock.
    /// Notification is outside the lock; no callback or await runs under it.
    fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let (result, wake, drained) = {
            let mut state = self.lock();
            let mut wake = state.expire(Instant::now());
            let result = change(&mut state);
            if state.closed {
                while let Some(waiter) = state.queue.pop_front() {
                    state.unqueue(&waiter.tenant);
                    wake.push(waiter);
                }
            } else if let Some(waiter) = state.next_eligible(self.profile) {
                wake.push(waiter.clone());
            }
            let drained = state.active == 0 && state.queue.is_empty();
            (result, wake, drained)
        };
        for waiter in wake {
            waiter.ready.notify_one();
        }
        if drained {
            self.drained.notify_waiters();
        }
        result
    }
}

impl State {
    fn reserve(&mut self, tenant: &TenantId) {
        self.active += 1;
        self.tenants.entry(tenant.clone()).or_default().active += 1;
    }

    fn idle(&mut self, tenant: &TenantId) {
        if self
            .tenants
            .get(tenant)
            .is_some_and(|counts| counts.active == 0 && counts.queued == 0)
        {
            self.tenants.remove(tenant);
        }
    }

    fn unqueue(&mut self, tenant: &TenantId) {
        if let Some(counts) = self.tenants.get_mut(tenant) {
            counts.queued -= 1;
        }
        self.idle(tenant);
    }

    fn remove(&mut self, waiter: &Arc<Waiter>) {
        if let Some(index) = self
            .queue
            .iter()
            .position(|entry| Arc::ptr_eq(entry, waiter))
            && self.queue.remove(index).is_some()
        {
            self.unqueue(&waiter.tenant);
        }
    }

    fn expire(&mut self, now: Instant) -> Vec<Arc<Waiter>> {
        let mut wake = Vec::new();
        let mut index = 0;
        while index < self.queue.len() {
            if self.queue[index].deadline <= now {
                if let Some(waiter) = self.queue.remove(index) {
                    self.unqueue(&waiter.tenant);
                    wake.push(waiter);
                }
            } else {
                index += 1;
            }
        }
        wake
    }

    fn next_eligible(&self, profile: Profile) -> Option<&Arc<Waiter>> {
        if self.closed || self.active >= profile.instance_active {
            return None;
        }
        let mut seen = BTreeSet::new();
        self.queue.iter().find(|waiter| {
            seen.insert(&waiter.tenant)
                && self
                    .tenants
                    .get(&waiter.tenant)
                    .is_some_and(|counts| counts.active < waiter.active_limit)
        })
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.admission.update(|state| state.remove(&self.waiter));
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.admission.update(|state| {
            if let Some(counts) = state.tenants.get_mut(&self.tenant) {
                counts.active -= 1;
                state.active -= 1;
            }
            state.idle(&self.tenant);
        });
    }
}
