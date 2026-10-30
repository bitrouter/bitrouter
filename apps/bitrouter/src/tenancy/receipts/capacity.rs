//! Accounting carried under the existing admission mutex, never a second semaphore.

use bitrouter_ai::tenancy::{
    identity::{ExecutionId, TenantId},
    operation::ErrorCode,
};
use std::collections::BTreeMap;

const ROW_OVERHEAD: u64 = 16 * 1024;

type DurableReceipt = (u64, Option<(ExecutionId, chrono::DateTime<chrono::Utc>)>);

pub(in crate::tenancy) const RESERVATION: u64 = 4 * 1024 * 1024;
pub(in crate::tenancy) const ADMISSION_FLOOR: u64 = 768 * 1024 * 1024;
pub(in crate::tenancy) const MAINTENANCE_FLOOR: u64 = 256 * 1024 * 1024;
pub(in crate::tenancy) const HIGH_WATER: u64 = 8 * 1024 * 1024 * 1024;
const UNKNOWN_TENANT: usize = 100;
const UNKNOWN_INSTANCE: usize = 1000;

pub(in crate::tenancy) struct Inventory {
    pub bytes: u64,
    pub rows: u64,
    pub unknown: Vec<(TenantId, ExecutionId, chrono::DateTime<chrono::Utc>)>,
}

struct Unknown {
    due: chrono::DateTime<chrono::Utc>,
    escalated: bool,
}

pub(in crate::tenancy) struct Accounting {
    pub fenced: bool,
    pub free: u64,
    pub bytes: u64,
    reserved: u64,
    // Decompose the shared reservation without allocating execution slots for
    // stopped-native repair. These charges remain subject to the same limits.
    #[cfg(all(feature = "core", feature = "harness"))]
    repair_reserved: u64,
    #[cfg(all(feature = "core", feature = "harness"))]
    repair_potential: usize,
    unknown: BTreeMap<(TenantId, ExecutionId), Unknown>,
    potential_unknown: BTreeMap<TenantId, usize>,
}

impl Default for Accounting {
    fn default() -> Self {
        Self {
            fenced: false,
            free: u64::MAX,
            bytes: 0,
            reserved: 0,
            #[cfg(all(feature = "core", feature = "harness"))]
            repair_reserved: 0,
            #[cfg(all(feature = "core", feature = "harness"))]
            repair_potential: 0,
            unknown: BTreeMap::new(),
            potential_unknown: BTreeMap::new(),
        }
    }
}

impl Accounting {
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn reserve_repair(
        &mut self,
        tenant: &TenantId,
        bytes: u64,
        rows: usize,
    ) -> Result<u64, ErrorCode> {
        let charged = u64::try_from(rows)
            .ok()
            .and_then(|n| n.checked_mul(ROW_OVERHEAD))
            .and_then(|n| n.checked_add(bytes))
            .ok_or(ErrorCode::CapacityExceeded)?;
        let potential = self
            .potential_unknown
            .values()
            .try_fold(0usize, |a, n| a.checked_add(*n))
            .ok_or(ErrorCode::CapacityExceeded)?;
        let instance = self
            .unknown
            .len()
            .checked_add(potential)
            .and_then(|n| n.checked_add(rows));
        let tenant_count = self
            .unknown
            .keys()
            .filter(|(key, _)| key == tenant)
            .count()
            .checked_add(self.potential_unknown.get(tenant).copied().unwrap_or(0))
            .and_then(|n| n.checked_add(rows));
        if self.fenced
            || self.free < ADMISSION_FLOOR
            || self
                .reserved
                .checked_add(charged)
                .and_then(|n| n.checked_add(MAINTENANCE_FLOOR))
                .is_none_or(|n| n > self.free)
            || self
                .bytes
                .checked_add(self.reserved)
                .and_then(|n| n.checked_add(charged))
                .is_none_or(|n| n > HIGH_WATER)
            || instance.is_none_or(|n| n > UNKNOWN_INSTANCE)
            || tenant_count.is_none_or(|n| n > UNKNOWN_TENANT)
        {
            return Err(ErrorCode::CapacityExceeded);
        }
        let repair_reserved = self
            .repair_reserved
            .checked_add(charged)
            .ok_or(ErrorCode::CapacityExceeded)?;
        let repair_potential = self
            .repair_potential
            .checked_add(rows)
            .ok_or(ErrorCode::CapacityExceeded)?;
        self.reserved += charged;
        self.repair_reserved = repair_reserved;
        self.repair_potential = repair_potential;
        *self.potential_unknown.entry(tenant.clone()).or_default() += rows;
        Ok(charged)
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn finish_repair(
        &mut self,
        tenant: &TenantId,
        charged: u64,
        rows: usize,
        committed: Option<&[(ExecutionId, chrono::DateTime<chrono::Utc>)]>,
    ) -> Result<(), ErrorCode> {
        let reserved = self
            .reserved
            .checked_sub(charged)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let repair_reserved = self
            .repair_reserved
            .checked_sub(charged)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let repair_potential = self
            .repair_potential
            .checked_sub(rows)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let pending = self
            .potential_unknown
            .get(tenant)
            .copied()
            .unwrap_or(0)
            .checked_sub(rows)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let bytes = if let Some(entries) = committed {
            if entries.len() != rows
                || entries
                    .iter()
                    .any(|(id, _)| self.unknown.contains_key(&(tenant.clone(), id.clone())))
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            self.bytes
                .checked_add(charged)
                .ok_or(ErrorCode::AuthorityUnavailable)?
        } else {
            self.bytes
        };
        self.reserved = reserved;
        self.repair_reserved = repair_reserved;
        self.repair_potential = repair_potential;
        self.bytes = bytes;
        if pending == 0 {
            self.potential_unknown.remove(tenant);
        } else {
            self.potential_unknown.insert(tenant.clone(), pending);
        }
        if let Some(entries) = committed {
            for (id, at) in entries {
                self.unknown.insert(
                    (tenant.clone(), id.clone()),
                    Unknown {
                        due: *at + chrono::Duration::minutes(15),
                        escalated: false,
                    },
                );
            }
        }
        Ok(())
    }

    /// Fixed aggregate under the caller's existing admission guard; no clock or mutation.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(in crate::tenancy) fn observed(&self, active: u64) -> Option<(u64, u64, u64, u64)> {
        if self.potential_unknown.len() > 128 {
            return None;
        }
        let potential = self
            .potential_unknown
            .values()
            .try_fold(0u64, |total, n| total.checked_add(u64::try_from(*n).ok()?))?;
        #[cfg(all(feature = "core", feature = "harness"))]
        let (repair_reserved, repair_potential) = (
            self.repair_reserved,
            u64::try_from(self.repair_potential).ok()?,
        );
        #[cfg(not(all(feature = "core", feature = "harness")))]
        let (repair_reserved, repair_potential) = (0u64, 0u64);
        let executions = potential.checked_sub(repair_potential)?;
        // Ordinary leases reserve 4 MiB each. Repair reserves exact candidate
        // bytes + row overhead and consumes no active execution lease.
        if executions > active
            || executions
                .checked_mul(RESERVATION)?
                .checked_add(repair_reserved)?
                != self.reserved
            || repair_potential.checked_mul(ROW_OVERHEAD)? > repair_reserved
            || (repair_potential == 0 && repair_reserved != 0)
        {
            return None;
        }
        Some((
            self.reserved,
            self.bytes,
            potential,
            u64::try_from(self.unknown.len()).ok()?,
        ))
    }

    #[cfg(all(test, unix))]
    pub(in crate::tenancy) fn snapshot_for_test(&self) -> (u64, u64, bool) {
        (self.reserved, self.free, self.fenced)
    }

    /// Initialization precedes publication of the Service and any execution.
    pub fn initialize(&mut self, inventory: Inventory, free: u64) -> Result<(), ErrorCode> {
        if self.reserved != 0 || !self.potential_unknown.is_empty() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        #[cfg(all(feature = "core", feature = "harness"))]
        if self.repair_reserved != 0 || self.repair_potential != 0 {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.bytes = inventory
            .rows
            .checked_mul(ROW_OVERHEAD)
            .and_then(|overhead| inventory.bytes.checked_add(overhead))
            .ok_or(ErrorCode::StorageUnavailable)?;
        let unknown_count = inventory.unknown.len();
        if unknown_count > UNKNOWN_INSTANCE || inventory.rows < unknown_count as u64 {
            return Err(ErrorCode::StorageUnavailable);
        }
        self.unknown = inventory
            .unknown
            .into_iter()
            .map(|(tenant, id, recorded)| {
                (
                    (tenant, id),
                    Unknown {
                        due: recorded + chrono::Duration::minutes(15),
                        escalated: false,
                    },
                )
            })
            .collect();
        if self.unknown.len() != unknown_count
            || self.unknown.keys().any(|(tenant, _)| {
                self.unknown.keys().filter(|(key, _)| key == tenant).count() > UNKNOWN_TENANT
            })
        {
            return Err(ErrorCode::StorageUnavailable);
        }
        self.free = free;
        Ok(())
    }

    pub fn next_escalation(&self) -> Option<tokio::time::Instant> {
        let due = self
            .unknown
            .values()
            .filter(|entry| !entry.escalated)
            .map(|entry| entry.due)
            .min()?;
        let remaining = due
            .signed_duration_since(chrono::Utc::now())
            .to_std()
            .unwrap_or_default()
            .min(std::time::Duration::from_secs(900));
        Some(tokio::time::Instant::now() + remaining)
    }
    pub fn escalate(&mut self, now: chrono::DateTime<chrono::Utc>) -> usize {
        let mut count = 0;
        for ((tenant, execution), entry) in &mut self.unknown {
            if !entry.escalated && entry.due <= now {
                entry.escalated = true;
                count += 1;
                tracing::warn!(
                    tenant_id = tenant.as_str(),
                    execution_id = execution.as_str(),
                    "unknown execution requires operator investigation; evidence retained without redispatch"
                );
            }
        }
        count
    }
    pub fn ready(&self, tenant: &TenantId) -> bool {
        !self.fenced
            && self.free >= ADMISSION_FLOOR
            && self
                .reserved
                .checked_add(RESERVATION)
                .and_then(|n| n.checked_add(MAINTENANCE_FLOOR))
                .is_some_and(|n| n <= self.free)
            && self
                .bytes
                .checked_add(self.reserved)
                .and_then(|n| n.checked_add(RESERVATION))
                .is_some_and(|n| n <= HIGH_WATER)
            && self.unknown.len() + self.potential_unknown.values().sum::<usize>()
                < UNKNOWN_INSTANCE
            && self.unknown.keys().filter(|(key, _)| key == tenant).count()
                + self.potential_unknown.get(tenant).copied().unwrap_or(0)
                < UNKNOWN_TENANT
    }

    pub fn reserve(&mut self, tenant: &TenantId) -> Result<(), ErrorCode> {
        if !self.ready(tenant) {
            return Err(ErrorCode::CapacityExceeded);
        }
        self.reserved = self
            .reserved
            .checked_add(RESERVATION)
            .ok_or(ErrorCode::CapacityExceeded)?;
        *self.potential_unknown.entry(tenant.clone()).or_default() += 1;
        Ok(())
    }

    pub fn release(
        &mut self,
        tenant: &TenantId,
        durable: Option<DurableReceipt>,
    ) -> Result<(), ErrorCode> {
        let count = self
            .potential_unknown
            .get_mut(tenant)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        *count = count
            .checked_sub(1)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if *count == 0 {
            self.potential_unknown.remove(tenant);
        }
        self.reserved = self
            .reserved
            .checked_sub(RESERVATION)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if let Some((bytes, unknown)) = durable {
            self.bytes = self
                .bytes
                .checked_add(bytes)
                .and_then(|n| {
                    if bytes == 0 {
                        Some(n)
                    } else {
                        n.checked_add(ROW_OVERHEAD)
                    }
                })
                .ok_or(ErrorCode::CapacityExceeded)?;
            if let Some((id, recorded)) = unknown
                && self
                    .unknown
                    .insert(
                        (tenant.clone(), id),
                        Unknown {
                            due: recorded + chrono::Duration::minutes(15),
                            escalated: false,
                        },
                    )
                    .is_some()
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
        }
        Ok(())
    }
}

/// Probe the pinned database volume, never the database inode (closing that
/// inode could release SQLite POSIX locks). Production profile is Unix-only.
pub(in crate::tenancy) fn free_space(volume: Option<&std::path::Path>) -> anyhow::Result<u64> {
    #[cfg(test)]
    if volume.is_none() {
        return Ok(u64::MAX);
    }
    let volume = volume.ok_or_else(|| anyhow::anyhow!("receipt volume is not pinned"))?;
    #[cfg(unix)]
    {
        let status = rustix::fs::statvfs(volume)?;
        let blocks = status.f_bavail;
        let size = status.f_frsize;
        blocks
            .checked_mul(size)
            .ok_or_else(|| anyhow::anyhow!("receipt volume size overflow"))
    }
    #[cfg(not(unix))]
    {
        let _ = volume;
        anyhow::bail!("receipt volume probing is unsupported on this host")
    }
}

#[cfg(all(
    test,
    feature = "core",
    feature = "harness",
    any(feature = "otel-http", feature = "otel-grpc")
))]
mod observation_tests;
#[cfg(test)]
mod tests;
