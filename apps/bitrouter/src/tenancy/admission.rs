//! Bounded local execution capacity, independent of grants and configuration revisions.
//!
//! Each acquisition compares its requested limits with the tenant's shared totals.
//! Narrower limits never revoke existing leases. FIFO within each tenant prevents
//! a newer, wider request from bypassing an older queued request; other tenants
//! may proceed while that tenant is full. Authority must be rechecked by the caller
//! after acquisition and before every provider send.

#[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
mod observation_tests;
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
    completion: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
    #[cfg(all(feature = "core", feature = "harness"))]
    learning: usize,
    ingress: usize,
    ingress_bytes: usize,
    tenants: BTreeMap<TenantId, Counts>,
    queue: VecDeque<Arc<Waiter>>,
    receipts: super::receipts::capacity::Accounting,
}

#[derive(Default)]
struct Counts {
    #[cfg(all(feature = "core", feature = "harness"))]
    learning: usize,
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

pub(super) struct Ingress {
    admission: Arc<Admission>,
    bytes: usize,
}
impl Drop for Ingress {
    fn drop(&mut self) {
        self.admission.update(|state| {
            state.ingress = state.ingress.saturating_sub(1);
            state.ingress_bytes = state.ingress_bytes.saturating_sub(self.bytes);
        });
    }
}
#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) struct Repair {
    admission: Arc<Admission>,
    tenant: TenantId,
    charged: u64,
    entries: Vec<(
        bitrouter_ai::tenancy::identity::ExecutionId,
        chrono::DateTime<chrono::Utc>,
    )>,
    finished: bool,
}
#[cfg(all(feature = "core", feature = "harness"))]
impl Repair {
    pub(in crate::tenancy) fn finish(&mut self, committed: bool) -> Result<(), ErrorCode> {
        if self.finished {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.admission.update(|state| {
            let result = state.receipts.finish_repair(
                &self.tenant,
                self.charged,
                self.entries.len(),
                committed.then_some(self.entries.as_slice()),
            );
            if result.is_err() {
                state.receipts.fenced = true;
            }
            result
        })?;
        self.finished = true;
        Ok(())
    }
}
// No Drop release: an unacknowledged consuming COMMIT retains its charge in H
// custody. Only confirmed rollback/commit calls finish; startup inventories a
// genuinely new exclusive owner's durable rows.
impl Admission {
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn reserve_repair(
        self: &Arc<Self>,
        tenant: &TenantId,
        bytes: u64,
        entries: Vec<(
            bitrouter_ai::tenancy::identity::ExecutionId,
            chrono::DateTime<chrono::Utc>,
        )>,
    ) -> Result<Repair, ErrorCode> {
        self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let charged = state
                .receipts
                .reserve_repair(tenant, bytes, entries.len())?;
            Ok(Repair {
                admission: self.clone(),
                tenant: tenant.clone(),
                charged,
                entries,
                finished: false,
            })
        })
    }

    /// Pure observation: neither poison recovery, queue expiry nor waiter wakes.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(super) fn observed_capacity(
        &self,
    ) -> Result<
        crate::telemetry::receipt_latency::capacity::Snapshot,
        crate::telemetry::receipt_latency::capacity::ReadError,
    > {
        use crate::telemetry::receipt_latency::capacity::{ReadError, Snapshot};
        let state = self.state.try_lock().map_err(|_| ReadError::Unavailable)?;
        let active = u64::try_from(state.active).map_err(|_| ReadError::Invalid)?;
        let (completion_reserved, accounted, potential_unknown, live_unknown) =
            state.receipts.observed(active).ok_or(ReadError::Invalid)?;
        Snapshot {
            completion_reserved,
            accounted,
            potential_unknown,
            live_unknown,
            ingress_reserved: u64::try_from(state.ingress_bytes).map_err(|_| ReadError::Invalid)?,
            active,
            queued: u64::try_from(state.queue.len()).map_err(|_| ReadError::Invalid)?,
            ingress: u64::try_from(state.ingress).map_err(|_| ReadError::Invalid)?,
            closed: state.closed,
            fenced: state.receipts.fenced,
        }
        .validate()
    }

    /// Schedule a real callback under the actual mutex, without changing state.
    #[cfg(all(test, feature = "otel-http"))]
    pub(super) fn with_observation_lock_for_test<T>(
        &self,
        collect: impl FnOnce() -> T,
    ) -> Result<T, crate::telemetry::receipt_latency::capacity::ReadError> {
        let _held = self
            .state
            .try_lock()
            .map_err(|_| crate::telemetry::receipt_latency::capacity::ReadError::Unavailable)?;
        Ok(collect())
    }

    /// Explicit age input for the real restored accounting; no inventory mutation.
    #[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
    pub(super) fn escalate_receipts_at_for_test(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> usize {
        self.update(|state| state.receipts.escalate(now))
    }

    /// Reserve bytes before reading or parsing. A declared length is a hard
    /// per-request ceiling; chunked bodies reserve the entire 16 MiB maximum.
    pub(super) fn ingress(self: &Arc<Self>, bytes: usize) -> Result<Ingress, ErrorCode> {
        if bytes > 16 * 1024 * 1024 {
            return Err(ErrorCode::PayloadTooLarge);
        }
        self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let total = state
                .ingress_bytes
                .checked_add(bytes)
                .ok_or(ErrorCode::CapacityExceeded)?;
            if state.ingress >= 128 || total > 64 * 1024 * 1024 {
                return Err(ErrorCode::CapacityExceeded);
            }
            state.ingress += 1;
            state.ingress_bytes = total;
            Ok(Ingress {
                admission: self.clone(),
                bytes,
            })
        })
    }

    pub(super) fn initialize_receipts(
        &self,
        inventory: super::receipts::capacity::Inventory,
        free: u64,
    ) -> Result<(), ErrorCode> {
        self.update(|state| state.receipts.initialize(inventory, free))
    }
    pub(super) fn next_receipt_escalation(&self) -> Option<Instant> {
        self.lock().receipts.next_escalation()
    }
    pub(super) fn escalate_receipts(&self) {
        self.update(|state| state.receipts.escalate(chrono::Utc::now()));
    }
    pub(super) fn receipt_probe(&self, free: u64) {
        self.update(|state| state.receipts.free = free);
    }
    pub(super) fn fence_receipts(&self) {
        self.update(|state| state.receipts.fenced = true);
    }
    pub(super) fn receipts_ready(&self) -> bool {
        !self.lock().receipts.fenced
    }

    /// Copies completion reservation bytes, last probed free bytes and fence.
    #[cfg(all(test, unix))]
    pub(super) fn receipt_capacity_for_test(&self) -> Result<(u64, u64, bool), ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        Ok(state.receipts.snapshot_for_test())
    }

    /// Read-only socket-test witness: ingress count/bytes, active and queued work.
    #[cfg(test)]
    pub(super) fn ingress_for_test(&self) -> Result<(usize, usize, usize, usize), ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        Ok((
            state.ingress,
            state.ingress_bytes,
            state.active,
            state.queue.len(),
        ))
    }

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

    #[cfg(test)]
    pub async fn acquire(
        self: &Arc<Self>,
        tenant: &TenantId,
        limits: &Limits,
    ) -> Result<Lease, ErrorCode> {
        self.acquire_until(tenant, limits, Instant::now() + QUEUE_WAIT)
            .await
    }

    /// Acquire capacity before the request deadline, waiting at most five seconds.
    ///
    /// # Errors
    /// Returns `CapacityExceeded` for disabled/full capacity or queue expiry, and
    /// `AuthorityUnavailable` after close. A zero queue limit permits immediate
    /// admission only. No grant, credential or revision is retained here.
    pub async fn acquire_until(
        self: &Arc<Self>,
        tenant: &TenantId,
        limits: &Limits,
        request_deadline: Instant,
    ) -> Result<Lease, ErrorCode> {
        let deadline = request_deadline.min(Instant::now() + QUEUE_WAIT);
        let active_limit = usize::from(limits.active_executions).min(self.profile.tenant_active);
        let queue_limit = usize::from(limits.queued_work).min(self.profile.tenant_queue);
        let registration = self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if Instant::now() >= deadline {
                return Err(ErrorCode::CapacityExceeded);
            }
            if !state.receipts.ready(tenant) {
                return Err(ErrorCode::CapacityExceeded);
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
                state.reserve(tenant)?;
                return Ok(Registration::Active);
            }
            if queued >= queue_limit || state.queue.len() >= self.profile.instance_queue {
                return Err(ErrorCode::CapacityExceeded);
            }
            let waiter = Arc::new(Waiter {
                tenant: tenant.clone(),
                active_limit,
                deadline,
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
                        state.reserve(tenant)?;
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
            completion: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// A nested paid Choice must never wait behind its own admitted parent.
    /// Reuse the same atomic active/receipt reservation and FIFO policy.
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(super) fn try_acquire(
        self: &Arc<Self>,
        tenant: &TenantId,
        limits: &Limits,
        deadline: Instant,
    ) -> Result<Lease, ErrorCode> {
        let limit = usize::from(limits.active_executions).min(self.profile.tenant_active);
        self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let (active, queued) = state
                .tenants
                .get(tenant)
                .map_or((0, 0), |counts| (counts.active, counts.queued));
            if Instant::now() >= deadline
                || !state.receipts.ready(tenant)
                || active >= limit
                || state.active >= self.profile.instance_active
                || queued != 0
                || state.next_eligible(self.profile).is_some()
            {
                return Err(ErrorCode::CapacityExceeded);
            }
            state.reserve(tenant)?;
            Ok(Lease {
                admission: self.clone(),
                tenant: tenant.clone(),
                completion: std::sync::atomic::AtomicBool::new(true),
            })
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
            if self.update(|state| state.drained()) {
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
            let drained = state.drained();
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
    fn reserve(&mut self, tenant: &TenantId) -> Result<(), ErrorCode> {
        self.receipts.reserve(tenant)?;
        self.active += 1;
        self.tenants.entry(tenant.clone()).or_default().active += 1;
        Ok(())
    }

    fn idle(&mut self, tenant: &TenantId) {
        if self.tenants.get(tenant).is_some_and(|counts| counts.idle()) {
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
                && self.receipts.ready(&waiter.tenant)
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
            if self
                .completion
                .swap(false, std::sync::atomic::Ordering::AcqRel)
                && state.receipts.release(&self.tenant, None).is_err()
            {
                state.receipts.fenced = true;
            }
            if let Some(counts) = state.tenants.get_mut(&self.tenant) {
                counts.active -= 1;
                state.active -= 1;
            }
            state.idle(&self.tenant);
        });
    }
}

impl Lease {
    pub(super) fn confirm_receipt(
        &self,
        bytes: u64,
        unknown: Option<(
            bitrouter_ai::tenancy::identity::ExecutionId,
            chrono::DateTime<chrono::Utc>,
        )>,
    ) -> Result<(), ErrorCode> {
        self.admission.update(|state| {
            if !self
                .completion
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let result = state.receipts.release(&self.tenant, Some((bytes, unknown)));
            if result.is_err() {
                state.receipts.fenced = true;
            }
            result
        })
    }
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) struct LearningLease {
    admission: Arc<Admission>,
    tenant: TenantId,
}
#[cfg(all(feature = "core", feature = "harness"))]
impl Drop for LearningLease {
    fn drop(&mut self) {
        self.admission.update(|state| {
            state.learning = state.learning.saturating_sub(1);
            if let Some(counts) = state.tenants.get_mut(&self.tenant) {
                counts.learning = counts.learning.saturating_sub(1);
            }
            state.idle(&self.tenant);
        });
    }
}
#[cfg(all(feature = "core", feature = "harness"))]
impl Admission {
    pub(in crate::tenancy) fn learning(
        self: &Arc<Self>,
        tenant: &TenantId,
        maximum: u32,
    ) -> Result<LearningLease, ErrorCode> {
        self.update(|state| {
            if state.closed {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let limit = usize::try_from(maximum.min(2)).map_err(|_| ErrorCode::CapacityExceeded)?;
            if state.learning >= 8
                || state
                    .tenants
                    .get(tenant)
                    .map_or(0, |counts| counts.learning)
                    >= limit
            {
                return Err(ErrorCode::CapacityExceeded);
            }
            state.learning += 1;
            state.tenants.entry(tenant.clone()).or_default().learning += 1;
            Ok(LearningLease {
                admission: self.clone(),
                tenant: tenant.clone(),
            })
        })
    }
}
impl Counts {
    fn idle(&self) -> bool {
        #[cfg(all(feature = "core", feature = "harness"))]
        if self.learning != 0 {
            return false;
        }
        self.active == 0 && self.queued == 0
    }
}
impl State {
    fn drained(&self) -> bool {
        #[cfg(all(feature = "core", feature = "harness"))]
        if self.learning != 0 {
            return false;
        }
        self.active == 0 && self.queue.is_empty()
    }
}

#[cfg(all(test, feature = "core", feature = "harness"))]
mod learning_tests {
    use super::*;
    #[tokio::test]
    async fn learning_capacity_is_shared_finite_and_retained_through_close() -> anyhow::Result<()> {
        let admission = Admission::new();
        let mut leases = Vec::new();
        for index in 0..4 {
            let tenant: TenantId = format!("tenant-{index}").try_into()?;
            for _ in 0..2 {
                leases.push(
                    admission
                        .learning(&tenant, 8)
                        .map_err(|code| anyhow::anyhow!("capacity: {code:?}"))?,
                );
            }
            assert!(matches!(
                admission.learning(&tenant, 8),
                Err(ErrorCode::CapacityExceeded)
            ));
        }
        let extra = "extra".try_into()?;
        assert!(matches!(
            admission.learning(&extra, 8),
            Err(ErrorCode::CapacityExceeded)
        ));
        assert!(matches!(
            admission.learning(&extra, 0),
            Err(ErrorCode::CapacityExceeded)
        ));
        admission.close();
        assert!(matches!(
            admission.learning(&extra, 1),
            Err(ErrorCode::AuthorityUnavailable)
        ));
        assert_eq!(admission.lock().learning, 8);
        drop(leases);
        admission.wait_drained().await;
        assert!(admission.lock().tenants.is_empty());
        Ok(())
    }
}

#[cfg(all(test, feature = "core", feature = "harness"))]
mod repair_tests {
    use super::*;
    use anyhow::{Result, ensure};
    #[test]
    fn repair_reservations_share_receipt_limits_without_execution_capacity() -> Result<()> {
        let admission = Admission::new();
        let tenant: TenantId = "repair-tenant".try_into()?;
        let entries = (0..100)
            .map(|n| Ok((format!("execution-{n}").try_into()?, chrono::Utc::now())))
            .collect::<Result<Vec<_>>>()?;
        let mut repair = admission
            .reserve_repair(&tenant, 1000, entries)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        {
            let state = admission.lock();
            ensure!(state.active == 0 && state.ingress == 0 && state.queue.is_empty());
            ensure!(!state.receipts.ready(&tenant));
        }
        let extra = vec![("extra".try_into()?, chrono::Utc::now())];
        ensure!(admission.reserve_repair(&tenant, 1, extra).is_err());
        repair.finish(false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(admission.lock().receipts.ready(&tenant));
        let mut repair = admission
            .reserve_repair(
                &tenant,
                25,
                vec![("durable".try_into()?, chrono::Utc::now())],
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        repair.finish(true).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(repair.finish(true).is_err());
        let state = admission.lock();
        ensure!(state.receipts.bytes == 25 + 16 * 1024 && state.active == 0);
        Ok(())
    }
}
