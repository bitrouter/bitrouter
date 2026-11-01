//! Shared estimates for writes to the owned database, separate from live receipts.

use std::sync::Arc;

use bitrouter_ai::tenancy::operation::ErrorCode;

use super::{admission::Admission, transaction::Proof};

#[cfg(test)]
mod tests;

// Engineering margins, not SQLite allocation guarantees. Include replacement,
// indexes and journal work even when the logical row count does not increase.
const BASE_BYTES: u64 = 64 * 1024;
const AMPLIFICATION: u64 = 8;

/// Conservative encoded-write estimate, shared by runtime and exclusive owners.
/// This margin is not a bound on SQLite or filesystem allocation.
pub(crate) fn estimate(encoded_bytes: u64) -> Result<u64, ErrorCode> {
    encoded_bytes
        .checked_mul(AMPLIFICATION)
        .and_then(|n| n.checked_add(BASE_BYTES))
        .ok_or(ErrorCode::CapacityExceeded)
}

/// Fresh physical-fit check before startup/offline writes, while the caller is
/// the exclusive owner and no runtime writer ledger is active. Never use this
/// to bypass runtime reservations or credit predicted freelist/compression space.
pub(crate) fn exclusive_write(
    owner: &super::ownership::Lease,
    encoded_bytes: u64,
) -> anyhow::Result<()> {
    exclusive_write_with(owner, encoded_bytes, |volume| {
        super::receipts::capacity::free_space(Some(volume))
    })
}

fn exclusive_write_with(
    owner: &super::ownership::Lease,
    encoded_bytes: u64,
    probe: impl FnOnce(&std::path::Path) -> anyhow::Result<u64>,
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    ensure!(owner.exclusive(), "exclusive database owner required");
    let volume = owner
        .path
        .parent()
        .context("owned database volume missing")?;
    let needed = estimate(encoded_bytes)
        .map_err(|_| anyhow::anyhow!("exclusive database write estimate overflow"))?;
    ensure!(
        probe(volume)? >= needed,
        "insufficient free space for exclusive database write"
    );
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Budget {
    admission: Arc<Admission>,
    proof: Arc<Proof>,
}

impl Budget {
    /// Observe real accounting; an optional pressure override is only useful
    /// for an unbound test volume, never a replacement for pinned-volume statvfs.
    #[cfg(all(test, unix, any(feature = "otel-http", feature = "otel-grpc")))]
    pub(crate) fn calibration_observation(
        &self,
        free: Option<u64>,
    ) -> Result<
        crate::telemetry::receipt_latency::capacity::Snapshot,
        crate::telemetry::receipt_latency::capacity::ReadError,
    > {
        if let Some(free) = free {
            self.admission.receipt_probe(free);
        }
        self.admission.observed_capacity()
    }

    pub(super) fn new(admission: Arc<Admission>, proof: Arc<Proof>) -> Self {
        Self { admission, proof }
    }

    pub(crate) fn reserve(
        &self,
        encoded_bytes: usize,
        maintenance: bool,
    ) -> Result<Charge, ErrorCode> {
        let bytes =
            estimate(u64::try_from(encoded_bytes).map_err(|_| ErrorCode::CapacityExceeded)?)?;
        self.admission.reserve_physical_write(bytes, maintenance)?;
        Ok(Charge {
            budget: self.clone(),
            bytes,
            #[cfg(feature = "harness")]
            encoded_bytes,
            #[cfg(feature = "harness")]
            maintenance,
            started: false,
            settled: false,
        })
    }

    pub(crate) fn fence(&self) {
        self.admission.fence_storage();
        self.proof.fenced();
    }
}

/// A charge is owned by the actual SQL consumer. Dropping an unresolved write
/// preserves its numeric reservation and closes dispatch; it cannot prove SQL
/// rollback. Ordinary capacity refusal never creates this storage-health fence.
pub(crate) struct Charge {
    budget: Budget,
    bytes: u64,
    #[cfg(feature = "harness")]
    encoded_bytes: usize,
    #[cfg(feature = "harness")]
    maintenance: bool,
    started: bool,
    settled: bool,
}

impl Charge {
    /// Extend one transaction's reservation before its next mutation. Earlier
    /// writes remain charged until the entire transaction is acknowledged.
    #[cfg(feature = "harness")]
    pub(crate) fn grow(&mut self, additional_encoded: usize) -> Result<(), ErrorCode> {
        let encoded = self
            .encoded_bytes
            .checked_add(additional_encoded)
            .ok_or(ErrorCode::CapacityExceeded)?;
        let bytes = estimate(u64::try_from(encoded).map_err(|_| ErrorCode::CapacityExceeded)?)?;
        let additional = bytes
            .checked_sub(self.bytes)
            .ok_or(ErrorCode::CapacityExceeded)?;
        self.budget
            .admission
            .reserve_physical_write(additional, self.maintenance)?;
        self.encoded_bytes = encoded;
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn start(&mut self) {
        self.started = true;
    }

    pub(crate) fn complete(mut self) -> Result<(), ErrorCode> {
        let result = self.budget.admission.finish_physical_write(self.bytes);
        if result.is_ok() {
            self.settled = true;
        } else {
            self.budget.fence();
        }
        result
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if self.started {
            self.budget.fence();
            self.budget.proof.quarantine();
        } else if self
            .budget
            .admission
            .finish_physical_write(self.bytes)
            .is_err()
        {
            self.budget.fence();
        }
    }
}

/// Count encoded bytes without creating another copy of an already prepared
/// payload. Overflow is a capacity refusal, never a wrapped estimate.
pub(crate) fn encoded_size(value: &impl serde::Serialize) -> anyhow::Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("write estimate overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

/// A confirmed rollback proves cleanup, not recovery from a storage fault.
/// Keep constraint/trigger refusals and non-storage errors on the existing
/// reconciliation path. Inspect typed driver errors, never their message text.
pub(crate) fn storage_health_error(error: &anyhow::Error) -> bool {
    use sea_orm::sqlx::{Error, error::DatabaseError, sqlite::SqliteError};
    error.chain().any(|cause| {
        let driver = cause.downcast_ref::<Error>();
        if matches!(driver, Some(Error::Io(_))) {
            return true;
        }
        driver
            .and_then(Error::as_database_error)
            .and_then(|error| error.try_downcast_ref::<SqliteError>())
            .or_else(|| cause.downcast_ref::<SqliteError>())
            .and_then(|error| error.code())
            .and_then(|code| code.parse::<i32>().ok())
            // Extended codes retain their primary result in the low byte:
            // READONLY, IOERR, CORRUPT, FULL, CANTOPEN, NOTADB.
            .is_some_and(|code| matches!(code & 0xff, 8 | 10 | 11 | 13 | 14 | 26))
    })
}
