//! Scalar accounting under the admission mutex. Durable identities stay in SQLite.
use bitrouter_ai::tenancy::operation::ErrorCode;

pub(in crate::tenancy) const ROW_OVERHEAD: u64 = 16 * 1024;
pub(in crate::tenancy) const RESERVATION: u64 = 4 * 1024 * 1024;
#[cfg(test)]
pub(in crate::tenancy) const ADMISSION_FLOOR: u64 = 768 * 1024 * 1024;
#[cfg(test)]
pub(in crate::tenancy) const MAINTENANCE_FLOOR: u64 = 256 * 1024 * 1024;

#[derive(Clone, Default)]
pub(in crate::tenancy) struct Inventory {
    /// Canonical receipt and auxiliary bytes for repair verification.
    pub bytes: u64,
    /// Total canonical receipt rows, including archived history, for verification.
    pub rows: u64,
    /// Inline receipt/auxiliary bytes plus row overhead; the live high-water basis.
    pub live_bytes: u64,
    pub history_bytes: u64,
    pub history_rows: u64,
    pub oauth_bytes: u64,
    pub unknown_count: u64,
    /// Complete validated stream digest, retained by the startup/repair caller.
    pub unknown_digest: [u8; 32],
}

/// A copy contains only scalars; publication recomputes it from current state.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct Totals {
    pub reserved: u64,
    pub bytes: u64,
    pub history_bytes: u64,
    pub history_rows: u64,
    pub oauth_bytes: u64,
    pub unknown: u64,
    pub uncovered: u64,
    pub live: u64,
    pub repair_reserved: u64,
    pub repair_live: u64,
}

pub(in crate::tenancy) struct Accounting {
    limits: crate::config::tenancy::capacity::Receipts,
    volume: Option<std::path::PathBuf>,
    pub fenced: bool,
    pub free: u64,
    pub totals: Totals,
    physical_write_reserved: u64,
}
impl Default for Accounting {
    fn default() -> Self {
        Self::configured(&crate::config::tenancy::capacity::Receipts::default())
    }
}
impl Accounting {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub fn observed_volume_free(&self) -> Option<u64> {
        self.volume.as_ref().map(|_| self.free)
    }

    pub fn configured(limits: &crate::config::tenancy::capacity::Receipts) -> Self {
        Self {
            limits: limits.clone(),
            volume: None,
            fenced: false,
            free: u64::MAX,
            totals: Totals::default(),
            physical_write_reserved: 0,
        }
    }
    pub fn bind_volume(&mut self, volume: Option<std::path::PathBuf>) -> Result<(), ErrorCode> {
        if self.volume.is_some() || self.totals.reserved != 0 || self.physical_write_reserved != 0 {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.volume = volume;
        self.refresh_free();
        if self.fenced {
            Err(ErrorCode::StorageUnavailable)
        } else {
            Ok(())
        }
    }
    /// Fresh observation under the shared mutex, including all writer custody.
    pub fn refresh_free(&mut self) {
        if let Some(volume) = &self.volume {
            match free_space(Some(volume)) {
                Ok(free) => self.free = free,
                Err(error) => {
                    self.fenced = true;
                    tracing::error!(%error, "receipt volume observation failed");
                }
            }
        }
    }
    pub fn observed_free(&mut self, free: u64) {
        if self.volume.is_none() {
            self.free = free;
        }
    }
    fn physical_fit(&self, bytes: u64, floor: u64) -> bool {
        self.totals
            .reserved
            .checked_add(self.physical_write_reserved)
            .and_then(|n| n.checked_add(bytes))
            .and_then(|n| n.checked_add(floor))
            .is_some_and(|n| n <= self.free)
    }
    /// Ordinary writers require admission free space and preserve maintenance space.
    /// Maintenance uses physical fit only.
    pub fn reserve_physical_write(
        &mut self,
        bytes: u64,
        maintenance: bool,
    ) -> Result<(), ErrorCode> {
        if self.fenced {
            return Err(ErrorCode::StorageUnavailable);
        }
        if !maintenance && self.free < self.limits.admission_free_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        let floor = if maintenance {
            0
        } else {
            self.limits.maintenance_free_bytes
        };
        if !self.physical_fit(bytes, floor) {
            return Err(ErrorCode::CapacityExceeded);
        }
        self.physical_write_reserved = self
            .physical_write_reserved
            .checked_add(bytes)
            .ok_or(ErrorCode::CapacityExceeded)?;
        Ok(())
    }
    pub fn finish_physical_write(&mut self, bytes: u64) -> Result<(), ErrorCode> {
        let Some(remaining) = self.physical_write_reserved.checked_sub(bytes) else {
            self.fenced = true;
            return Err(ErrorCode::AuthorityUnavailable);
        };
        self.physical_write_reserved = remaining;
        Ok(())
    }
    /// Complete startup validation is allowed above current operator budgets.
    pub fn initialize(&mut self, inventory: Inventory, free: u64) -> Result<(), ErrorCode> {
        if self.totals.live != 0 || self.totals.reserved != 0 || self.physical_write_reserved != 0 {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        // Pending OAuth obligations need not have receipt rows. No rows>=unknown check.
        self.totals.bytes = inventory.live_bytes;
        self.totals.history_bytes = inventory.history_bytes;
        self.totals.history_rows = inventory.history_rows;
        self.totals.oauth_bytes = inventory.oauth_bytes;
        self.totals.unknown = inventory.unknown_count;
        self.observed_free(free);
        Ok(())
    }
    /// Begin before full: archive while inline bytes and completion custody are
    /// at/above 75% of high-water, or the next completion would not fit. The
    /// subtraction computes ceil(3*high_water/4) without multiplying/overflowing.
    /// Zero inline bytes means there is no retained class to relocate.
    pub fn archive_needed(&self) -> bool {
        let watermark = self.limits.high_water_bytes - self.limits.high_water_bytes / 4;
        self.totals.bytes != 0
            && self
                .totals
                .bytes
                .checked_add(self.totals.reserved)
                .is_none_or(|used| {
                    used >= watermark
                        || used
                            .checked_add(RESERVATION)
                            .is_none_or(|next| next > self.limits.high_water_bytes)
                })
    }

    /// Run before a cold tenant count and again at grant time.
    pub fn globally_ready(&self) -> bool {
        !self.fenced
            && self.free >= self.limits.admission_free_bytes
            && self.physical_fit(RESERVATION, self.limits.maintenance_free_bytes)
            && self
                .totals
                .bytes
                .checked_add(self.totals.reserved)
                .and_then(|n| n.checked_add(RESERVATION))
                .is_some_and(|n| n <= self.limits.high_water_bytes)
            && self
                .totals
                .unknown
                .checked_add(self.totals.uncovered)
                .is_some_and(|n| n < self.limits.unknown_instance as u64)
    }
    pub fn ready(&self, tenant_unknown: u64, tenant_uncovered: u64) -> bool {
        self.globally_ready()
            && tenant_unknown
                .checked_add(tenant_uncovered)
                .is_some_and(|n| n < self.limits.unknown_tenant as u64)
    }
    pub fn reserve(&mut self, tenant_unknown: u64, tenant_uncovered: u64) -> Result<(), ErrorCode> {
        if !self.ready(tenant_unknown, tenant_uncovered) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let mut next = self.totals;
        next.reserved = next
            .reserved
            .checked_add(RESERVATION)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.live = next
            .live
            .checked_add(1)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.uncovered = next
            .uncovered
            .checked_add(1)
            .ok_or(ErrorCode::CapacityExceeded)?;
        self.totals = next;
        Ok(())
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub fn reserve_repair(
        &mut self,
        bytes: u64,
        rows: u64,
        uncovered: u64,
    ) -> Result<u64, ErrorCode> {
        let charged = rows
            .checked_mul(ROW_OVERHEAD)
            .and_then(|n| n.checked_add(bytes))
            .ok_or(ErrorCode::CapacityExceeded)?;
        if self.fenced {
            return Err(ErrorCode::StorageUnavailable);
        }
        if uncovered > rows || !self.physical_fit(charged, 0) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let mut next = self.totals;
        next.reserved = next
            .reserved
            .checked_add(charged)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.repair_reserved = next
            .repair_reserved
            .checked_add(charged)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.live = next
            .live
            .checked_add(rows)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.repair_live = next
            .repair_live
            .checked_add(rows)
            .ok_or(ErrorCode::CapacityExceeded)?;
        next.uncovered = next
            .uncovered
            .checked_add(uncovered)
            .ok_or(ErrorCode::CapacityExceeded)?;
        self.totals = next;
        Ok(charged)
    }
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub fn physical_write_reserved(&self) -> u64 {
        self.physical_write_reserved
    }
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub fn observed(&self, active: u64) -> Option<(u64, u64, u64, u64)> {
        let total = self.totals;
        let executions = total.live.checked_sub(total.repair_live)?;
        if total.uncovered > total.live
            || executions > active
            || executions
                .checked_mul(RESERVATION)?
                .checked_add(total.repair_reserved)?
                != total.reserved
            || total.repair_live.checked_mul(ROW_OVERHEAD)? > total.repair_reserved
        {
            return None;
        }
        // Existing metric means all live potentials, including covered owners;
        // admission uses uncovered alone. Physical writes are separate custody.
        Some((total.reserved, total.bytes, total.live, total.unknown))
    }
    #[cfg(all(test, unix))]
    pub fn snapshot_for_test(&self) -> (u64, u64, bool) {
        (self.totals.reserved, self.free, self.fenced)
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
