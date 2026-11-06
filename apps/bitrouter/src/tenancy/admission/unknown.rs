//! Process-local publication of the validated SQLite obligation projection.
//! Caller custody bounds pins and changed keys; no dormant tenant directory lives here.
use super::*;
use crate::tenancy::{
    receipts::{
        capacity::{RESERVATION, ROW_OVERHEAD, Totals},
        store::obligations::{Change, Key},
    },
    transaction::{Disposition, Proof},
};

pub(in crate::tenancy) struct CountPin {
    admission: Arc<Admission>,
    tenant: TenantId,
}
pub(in crate::tenancy) struct Read {
    pub(super) admission: Arc<Admission>,
    generation: u64,
    pub(super) maintenance: bool,
}
/// Receipt bytes are charged once here, including a zero-byte duplicate completion.
pub(in crate::tenancy) struct Terminal {
    pub key: Key,
    pub bytes: u64,
}

#[derive(Default)]
pub(super) struct Completion {
    pub execution: std::sync::OnceLock<bitrouter_ai::tenancy::identity::ExecutionId>,
    pub done: AtomicBool,
}
pub(super) struct Live {
    pub owner: Arc<Completion>,
    pub covered: bool,
    pub pending: bool,
    pub dropped: bool,
    pub repair: bool,
    pub published: bool,
}
struct Step {
    key: Key,
    before: bool,
    after: bool,
    terminal: Option<u64>,
    owner: Option<Arc<Completion>>,
}
struct TenantChange {
    tenant: TenantId,
    durable: i128,
    uncovered: i128,
    next_unknown: Option<u64>,
    next_uncovered: u64,
    next_active: usize,
}
/// Non-clonable, tied to this admission owner, generation and actual writer proof.
#[must_use]
pub(in crate::tenancy) struct Publication {
    admission: Arc<Admission>,
    proof: Arc<Proof>,
    token: Arc<()>,
    generation: u64,
    steps: Vec<Step>,
    tenants: Vec<TenantChange>,
    finished: bool,
}
fn adjust(value: u64, delta: i128) -> Result<u64, ErrorCode> {
    u64::try_from(i128::from(value) + delta).map_err(|_| ErrorCode::AuthorityUnavailable)
}
impl Admission {
    /// Called inside the actor immediately before a cold COUNT or source transaction.
    /// Ordinary global refusal occurs before SQL; maintenance bypasses policy only.
    pub(in crate::tenancy) fn unknown_read(
        self: &Arc<Self>,
        maintenance: bool,
    ) -> Result<Read, ErrorCode> {
        self.update(|state| {
            state.unknown_available(maintenance)?;
            Ok(Read {
                admission: self.clone(),
                generation: state.unknown_generation,
                maintenance,
            })
        })
    }
    /// Reuse a hot count (or prove zero from a zero global count). None needs SQL.
    /// The caller retains the pin through admission and owns its existing bounded read/ingress slot.
    pub(in crate::tenancy) fn pin_unknown(
        self: &Arc<Self>,
        tenant: &TenantId,
        maintenance: bool,
    ) -> Result<Option<CountPin>, ErrorCode> {
        self.update(|state| {
            state.unknown_available(maintenance)?;
            let Some(count) = state.tenant_unknown(tenant) else {
                return Ok(None);
            };
            state.add_pin(tenant, count)?;
            Ok(Some(CountPin {
                admission: self.clone(),
                tenant: tenant.clone(),
            }))
        })
    }
    /// Install a complete indexed COUNT before the actor serves another writer job.
    pub(in crate::tenancy) fn seed_unknown(
        self: &Arc<Self>,
        read: &Read,
        tenant: &TenantId,
        count: u64,
    ) -> Result<CountPin, ErrorCode> {
        if !Arc::ptr_eq(self, &read.admission) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.update(|state| {
            state.check_read(read)?;
            state.unknown_available(read.maintenance)?;
            if count > state.receipts.totals.unknown
                || state.tenant_unknown(tenant).is_some_and(|old| old != count)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            state.add_pin(tenant, count)?;
            Ok(CountPin {
                admission: self.clone(),
                tenant: tenant.clone(),
            })
        })
    }
    /// After final per-key reconciliation, before the consuming COMMIT.
    /// The actor passes its live Proof, and calls publish/rollback synchronously
    /// after the matching known outcome, before serving another writer job.
    pub(in crate::tenancy) fn prepare_unknown(
        self: &Arc<Self>,
        read: &Read,
        changes: Vec<Change>,
        terminals: Vec<Terminal>,
        proof: Arc<Proof>,
    ) -> Result<Publication, ErrorCode> {
        if !Arc::ptr_eq(self, &read.admission) || proof.disposition() != Disposition::Pending {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let mut terminal_map = BTreeMap::new();
        for terminal in terminals {
            if terminal_map.insert(terminal.key, terminal.bytes).is_some() {
                return Err(ErrorCode::AuthorityUnavailable);
            }
        }
        let mut seen = BTreeSet::new();
        let mut steps = Vec::with_capacity(changes.len());
        for change in changes {
            if !seen.insert(change.key.clone())
                || change.before.as_ref().is_some_and(|r| r.key != change.key)
                || change.after.as_ref().is_some_and(|r| r.key != change.key)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let terminal = terminal_map.remove(&change.key);
            steps.push(Step {
                key: change.key,
                before: change.before.is_some(),
                after: change.after.is_some(),
                terminal,
                owner: None,
            });
        }
        if !terminal_map.is_empty() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let token = Arc::new(());
        let mut publication = Publication {
            admission: self.clone(),
            proof,
            token: token.clone(),
            generation: read.generation,
            steps,
            tenants: Vec::new(),
            finished: true,
        };
        self.update(|state| {
            state.check_read(read)?;
            // Source completion is protected cleanup, not a new ordinary admission.
            if state.receipts.fenced {
                return Err(ErrorCode::StorageUnavailable);
            }
            let mut tenants = BTreeMap::<TenantId, (i128, i128)>::new();
            for step in &mut publication.steps {
                let deltas = tenants.entry(step.key.tenant.clone()).or_default();
                deltas.0 += i128::from(step.after) - i128::from(step.before);
                if let Some(live) = state.executions.get(&step.key) {
                    if live.owner.done.load(Ordering::Acquire) {
                        if step.terminal.is_some_and(|bytes| bytes != 0) {
                            return Err(ErrorCode::AuthorityUnavailable);
                        }
                        continue;
                    }
                    if live.pending
                        || live.covered != step.before
                        || (live.repair && step.terminal.is_some())
                    {
                        return Err(ErrorCode::AuthorityUnavailable);
                    }
                    step.owner = Some(live.owner.clone());
                    deltas.1 += i128::from(!step.after && step.terminal.is_none())
                        - i128::from(!step.before);
                } else if step.terminal.is_some_and(|bytes| bytes != 0) {
                    // A missing owner permits a zero-byte duplicate only.
                    return Err(ErrorCode::AuthorityUnavailable);
                }
            }
            for (tenant, (durable, uncovered)) in tenants {
                if let Some(counts) = state.tenants.get(&tenant) {
                    counts
                        .pins
                        .checked_add(1)
                        .ok_or(ErrorCode::CapacityExceeded)?;
                    publication.tenants.push(TenantChange {
                        tenant,
                        durable,
                        uncovered,
                        next_unknown: None,
                        next_uncovered: 0,
                        next_active: 0,
                    });
                } else if uncovered != 0 {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
            }
            publication.calculate(state, false)?;
            for tenant in &publication.tenants {
                if let Some(counts) = state.tenants.get_mut(&tenant.tenant) {
                    counts.pins += 1;
                }
            }
            for step in &publication.steps {
                if step.owner.is_some()
                    && let Some(live) = state.executions.get_mut(&step.key)
                {
                    live.pending = true;
                }
            }
            state.publication_pending = Some(token);
            publication.finished = false;
            Ok(())
        })?;
        Ok(publication)
    }
}
impl State {
    pub(super) fn unknown_available(&self, maintenance: bool) -> Result<(), ErrorCode> {
        if (self.closed && !maintenance)
            || self.publication_pending.is_some()
            || self.receipts.fenced
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if !maintenance && !self.receipts.globally_ready() {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(())
    }
    pub(super) fn check_read(&self, read: &Read) -> Result<(), ErrorCode> {
        if self.publication_pending.is_some() || self.unknown_generation != read.generation {
            Err(ErrorCode::AuthorityUnavailable)
        } else {
            Ok(())
        }
    }
    pub(super) fn tenant_unknown(&self, tenant: &TenantId) -> Option<u64> {
        self.tenants
            .get(tenant)
            .and_then(|counts| counts.unknown)
            .or_else(|| (self.receipts.totals.unknown == 0).then_some(0))
    }
    fn add_pin(&mut self, tenant: &TenantId, count: u64) -> Result<(), ErrorCode> {
        let pins = self
            .tenants
            .get(tenant)
            .map_or(0, |c| c.pins)
            .checked_add(1)
            .ok_or(ErrorCode::CapacityExceeded)?;
        let counts = self.tenants.entry(tenant.clone()).or_default();
        counts.pins = pins;
        counts.unknown = Some(count);
        Ok(())
    }
    pub(super) fn receipts_ready(&self, tenant: &TenantId) -> bool {
        self.publication_pending.is_none()
            && self.tenant_unknown(tenant).is_some_and(|unknown| {
                self.receipts
                    .ready(unknown, self.tenants.get(tenant).map_or(0, |c| c.uncovered))
            })
    }
}
impl Drop for CountPin {
    fn drop(&mut self) {
        self.admission.update(|state| {
            if let Some(counts) = state.tenants.get_mut(&self.tenant)
                && let Some(pins) = counts.pins.checked_sub(1)
            {
                counts.pins = pins;
            } else {
                state.receipts.fenced = true;
            }
            state.idle(&self.tenant);
        });
    }
}
impl Lease {
    /// Bind once immediately after mint, before any external effect. No new permit.
    pub(in crate::tenancy) fn bind_execution(
        &self,
        execution: &bitrouter_ai::tenancy::identity::ExecutionId,
    ) -> Result<(), ErrorCode> {
        self.admission.update(|state| {
            if state.publication_pending.is_some()
                || state.receipts.fenced
                || self.completion.done.load(Ordering::Acquire)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if let Some(old) = self.completion.execution.get() {
                return if old == execution {
                    Ok(())
                } else {
                    Err(ErrorCode::AuthorityUnavailable)
                };
            }
            let key = Key {
                tenant: self.tenant.clone(),
                execution: execution.clone(),
            };
            if state.executions.contains_key(&key) {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            self.completion
                .execution
                .set(execution.clone())
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            state.executions.insert(
                key,
                Live {
                    owner: self.completion.clone(),
                    covered: false,
                    pending: false,
                    dropped: false,
                    repair: false,
                    published: false,
                },
            );
            Ok(())
        })
    }
}
impl Publication {
    fn calculate(&mut self, state: &State, rollback: bool) -> Result<(Totals, usize), ErrorCode> {
        state
            .unknown_generation
            .checked_add(1)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let mut next = state.receipts.totals;
        let mut active = state.active;
        let mut durable = 0i128;
        let mut uncovered = 0i128;
        let mut release = 0u64;
        let mut bytes = 0u64;
        for tenant in &mut self.tenants {
            let counts = state
                .tenants
                .get(&tenant.tenant)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if !self.finished && counts.pins == 0 {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            tenant.next_active = counts.active;
            tenant.next_unknown = match counts.unknown {
                Some(n) => Some(adjust(n, if rollback { 0 } else { tenant.durable })?),
                None => None,
            };
            tenant.next_uncovered = adjust(
                counts.uncovered,
                if rollback { 0 } else { tenant.uncovered },
            )?;
        }
        for step in &self.steps {
            if !rollback {
                durable += i128::from(step.after) - i128::from(step.before);
            }
            if let Some(owner) = &step.owner {
                let live = state
                    .executions
                    .get(&step.key)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                if !Arc::ptr_eq(owner, &live.owner)
                    || live.covered != step.before
                    || owner.done.load(Ordering::Acquire)
                {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                let terminal = !rollback && step.terminal.is_some();
                if !rollback {
                    uncovered += i128::from(!step.after && !terminal) - i128::from(!step.before);
                }
                if terminal {
                    release = release
                        .checked_add(1)
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                    let added = step.terminal.ok_or(ErrorCode::AuthorityUnavailable)?;
                    bytes = bytes
                        .checked_add(added)
                        .and_then(|n| {
                            if added == 0 {
                                Some(n)
                            } else {
                                n.checked_add(ROW_OVERHEAD)
                            }
                        })
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                }
                if live.dropped {
                    if live.repair {
                        return Err(ErrorCode::AuthorityUnavailable);
                    }
                    active = active
                        .checked_sub(1)
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                    let tenant = self
                        .tenants
                        .iter_mut()
                        .find(|t| t.tenant == step.key.tenant)
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                    tenant.next_active = tenant
                        .next_active
                        .checked_sub(1)
                        .ok_or(ErrorCode::AuthorityUnavailable)?;
                    if !terminal {
                        release = release
                            .checked_add(1)
                            .ok_or(ErrorCode::AuthorityUnavailable)?;
                        let was_uncovered = if rollback { !step.before } else { !step.after };
                        if was_uncovered {
                            uncovered -= 1;
                            let tenant = self
                                .tenants
                                .iter_mut()
                                .find(|t| t.tenant == step.key.tenant)
                                .ok_or(ErrorCode::AuthorityUnavailable)?;
                            tenant.next_uncovered = tenant
                                .next_uncovered
                                .checked_sub(1)
                                .ok_or(ErrorCode::AuthorityUnavailable)?;
                        }
                    }
                }
            }
        }
        next.unknown = adjust(next.unknown, durable)?;
        next.uncovered = adjust(next.uncovered, uncovered)?;
        next.live = next
            .live
            .checked_sub(release)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        next.reserved = next
            .reserved
            .checked_sub(
                release
                    .checked_mul(RESERVATION)
                    .ok_or(ErrorCode::AuthorityUnavailable)?,
            )
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        next.bytes = next
            .bytes
            .checked_add(bytes)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if next.uncovered > next.live {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        Ok((next, active))
    }
    pub(in crate::tenancy) fn publish(mut self) -> Result<(), ErrorCode> {
        self.finish(false)
    }
    #[cfg(any(test, all(feature = "core", feature = "harness")))]
    pub(in crate::tenancy) fn rollback(mut self) -> Result<(), ErrorCode> {
        self.finish(true)
    }
    fn finish(&mut self, rollback: bool) -> Result<(), ErrorCode> {
        let expected = if rollback {
            Disposition::RolledBack
        } else {
            Disposition::Committed
        };
        if self.finished || self.proof.disposition() != expected {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let admission = self.admission.clone();
        admission.update(|state| {
            if state.receipts.fenced
                || state.unknown_generation != self.generation
                || !state
                    .publication_pending
                    .as_ref()
                    .is_some_and(|token| Arc::ptr_eq(token, &self.token))
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let (next, active) = self.calculate(state, rollback)?;
            // No owner locks/callbacks, allocation, fallible operation or await follows.
            state.receipts.totals = next;
            state.active = active;
            for tenant in &self.tenants {
                if let Some(counts) = state.tenants.get_mut(&tenant.tenant) {
                    counts.unknown = tenant.next_unknown;
                    counts.uncovered = tenant.next_uncovered;
                    counts.active = tenant.next_active;
                    counts.pins -= 1;
                }
            }
            for step in &self.steps {
                if step.owner.is_some()
                    && let Some(live) = state.executions.get_mut(&step.key)
                {
                    live.pending = false;
                    if !rollback {
                        live.covered = step.after;
                        live.published = true;
                    }
                    if !rollback && step.terminal.is_some() {
                        live.owner.done.store(true, Ordering::Release);
                    }
                    if live.dropped {
                        live.owner.done.store(true, Ordering::Release);
                        state.executions.remove(&step.key);
                    }
                }
            }
            if !rollback {
                state.unknown_generation += 1;
            }
            state.publication_pending = None;
            for tenant in &self.tenants {
                state.idle(&tenant.tenant);
            }
            self.finished = true;
            Ok(())
        })
    }
}
impl Drop for Publication {
    fn drop(&mut self) {
        if !self.finished {
            // Keep the pending marker, pins, owners and charges. A new exclusive
            // process validates SQL; this process cannot guess the commit outcome.
            self.proof.quarantine();
            self.admission.fence_storage();
        }
    }
}

#[cfg(test)]
mod tests;

impl State {
    pub(super) fn release_lease(
        &mut self,
        tenant: &TenantId,
        owner: &Arc<Completion>,
    ) -> Result<(), ErrorCode> {
        let key = owner.execution.get().map(|execution| Key {
            tenant: tenant.clone(),
            execution: execution.clone(),
        });
        let mut covered = false;
        if let Some(key) = &key {
            let live = self
                .executions
                .get_mut(key)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if !Arc::ptr_eq(&live.owner, owner) || live.repair || live.dropped {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if live.pending {
                live.dropped = true;
                return Ok(());
            }
            covered = live.covered;
        }
        let counts = self
            .tenants
            .get(tenant)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let active = self
            .active
            .checked_sub(1)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let tenant_active = counts
            .active
            .checked_sub(1)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let mut uncovered = counts.uncovered;
        let mut next = self.receipts.totals;
        if !owner.done.load(Ordering::Acquire) {
            next.live = next
                .live
                .checked_sub(1)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            next.reserved = next
                .reserved
                .checked_sub(RESERVATION)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if !covered {
                next.uncovered = next
                    .uncovered
                    .checked_sub(1)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                uncovered = uncovered
                    .checked_sub(1)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
            }
        }
        self.receipts.totals = next;
        self.active = active;
        if let Some(counts) = self.tenants.get_mut(tenant) {
            counts.active = tenant_active;
            counts.uncovered = uncovered;
        }
        owner.done.store(true, Ordering::Release);
        if let Some(key) = &key {
            self.executions.remove(key);
        }
        self.idle(tenant);
        Ok(())
    }
}
