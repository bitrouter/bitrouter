//! Protected stopped-native repair; shares physical custody, not ordinary eligibility.
use super::unknown::{Completion, Live, Read};
use super::*;
use crate::tenancy::receipts::store::obligations::Key;
use bitrouter_ai::tenancy::identity::ExecutionId;

impl Admission {
    /// Entries carry the validated pre-write projection presence for each key.
    /// Caller supplies a complete bounded repair group under its existing owner.
    pub(in crate::tenancy) fn reserve_repair(
        self: &Arc<Self>,
        read: &Read,
        tenant: &TenantId,
        bytes: u64,
        entries: Vec<(ExecutionId, bool)>,
    ) -> Result<Repair, ErrorCode> {
        if !Arc::ptr_eq(self, &read.admission) || !read.maintenance {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let mut unique = BTreeSet::new();
        let mut owners = Vec::with_capacity(entries.len());
        let mut uncovered = 0u64;
        for (execution, covered) in entries {
            let key = Key {
                tenant: tenant.clone(),
                execution: execution.clone(),
            };
            if !unique.insert(key.clone()) {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let owner = Arc::new(Completion::default());
            owner
                .execution
                .set(execution)
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            uncovered = uncovered
                .checked_add(u64::from(!covered))
                .ok_or(ErrorCode::CapacityExceeded)?;
            owners.push((
                key,
                Live {
                    owner,
                    covered,
                    pending: false,
                    dropped: false,
                    repair: true,
                    published: false,
                },
            ));
        }
        if owners.is_empty() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let keys = owners
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        self.update(|state| {
            state.check_read(read)?;
            state.unknown_available(true)?;
            if owners
                .iter()
                .any(|(key, _)| state.executions.contains_key(key))
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let counts = state.tenants.get(tenant);
            let repairs = counts
                .map_or(0, |c| c.repairs)
                .checked_add(owners.len())
                .ok_or(ErrorCode::CapacityExceeded)?;
            let tenant_uncovered = counts
                .map_or(0, |c| c.uncovered)
                .checked_add(uncovered)
                .ok_or(ErrorCode::CapacityExceeded)?;
            let rows = u64::try_from(owners.len()).map_err(|_| ErrorCode::CapacityExceeded)?;
            let charged = state.receipts.reserve_repair(bytes, rows, uncovered)?;
            let counts = state.tenants.entry(tenant.clone()).or_default();
            counts.repairs = repairs;
            counts.uncovered = tenant_uncovered;
            state.executions.extend(owners);
            Ok(Repair {
                admission: self.clone(),
                tenant: tenant.clone(),
                charged,
                keys,
                finished: false,
            })
        })
    }
}
impl Repair {
    /// Known outcome only. On COMMIT, projection publication must already be consumed.
    /// This releases repair potentials/bytes, never mutates durable unknown counts.
    pub(in crate::tenancy) fn finish(&mut self, committed: bool) -> Result<(), ErrorCode> {
        if self.finished {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.admission.update(|state| {
            if state.publication_pending.is_some() || state.receipts.fenced {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let mut uncovered = 0u64;
            for key in &self.keys {
                let live = state
                    .executions
                    .get(key)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                if !live.repair
                    || live.pending
                    || live.owner.done.load(Ordering::Acquire)
                    || (committed && !live.published)
                {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                uncovered = uncovered
                    .checked_add(u64::from(!live.covered))
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
            }
            let rows =
                u64::try_from(self.keys.len()).map_err(|_| ErrorCode::AuthorityUnavailable)?;
            let counts = state
                .tenants
                .get(&self.tenant)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            let remaining = counts
                .repairs
                .checked_sub(self.keys.len())
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            let tenant_uncovered = counts
                .uncovered
                .checked_sub(uncovered)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            let mut next = state.receipts.totals;
            next.live = next
                .live
                .checked_sub(rows)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            next.repair_live = next
                .repair_live
                .checked_sub(rows)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            next.uncovered = next
                .uncovered
                .checked_sub(uncovered)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            next.reserved = next
                .reserved
                .checked_sub(self.charged)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            next.repair_reserved = next
                .repair_reserved
                .checked_sub(self.charged)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if committed {
                next.bytes = next
                    .bytes
                    .checked_add(self.charged)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
            }
            state.receipts.totals = next;
            if let Some(counts) = state.tenants.get_mut(&self.tenant) {
                counts.repairs = remaining;
                counts.uncovered = tenant_uncovered;
            }
            for key in &self.keys {
                state.executions.remove(key);
            }
            state.idle(&self.tenant);
            self.finished = true;
            Ok(())
        })
    }
}
impl Drop for Repair {
    fn drop(&mut self) {
        if !self.finished {
            self.admission.fence_storage();
        }
    }
}

#[cfg(test)]
mod tests;
