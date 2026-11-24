//! Shared operation accounting and checkpoints, with caller-selected limits.
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Mutex;

pub(crate) type Check<'a> = dyn Fn() -> Result<(), ErrorCode> + Sync + 'a;
/// Physical write reservation/statement bound, independent of read accounting.
pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("owned operation rejected")]
    Rejected(ErrorCode),
    #[error("owned operation capacity exceeded")]
    Capacity,
    #[error("owned operation budget unavailable")]
    Unavailable,
}
pub(crate) struct OperationBudget<'a> {
    check: &'a Check<'a>,
    used: Mutex<(usize, usize)>,
    rows: usize,
    bytes: usize,
}
impl<'a> OperationBudget<'a> {
    pub(crate) fn new(check: &'a Check<'a>) -> Self {
        Self::with_row_limit(check, usize::MAX)
    }
    /// Apply the caller's cumulative row limit, without an implicit byte limit.
    pub(crate) fn with_row_limit(check: &'a Check<'a>, rows: usize) -> Self {
        Self::with_limits(check, rows, usize::MAX)
    }
    /// Apply both caller-selected cumulative limits without clamping them.
    /// All its borrowed storage readers must share this one budget.
    pub(crate) fn with_limits(check: &'a Check<'a>, rows: usize, bytes: usize) -> Self {
        Self {
            check,
            used: Mutex::new((0, 0)),
            rows,
            bytes,
        }
    }
    pub(crate) fn check(&self) -> Result<(), Error> {
        (self.check)().map_err(Error::Rejected)
    }
    pub(crate) fn charge(&self, rows: usize, bytes: usize) -> Result<(), Error> {
        self.check()?;
        let mut used = self.used.lock().map_err(|_| Error::Unavailable)?;
        let rows = used.0.checked_add(rows).ok_or(Error::Capacity)?;
        let bytes = used.1.checked_add(bytes).ok_or(Error::Capacity)?;
        if rows > self.rows || bytes > self.bytes {
            return Err(Error::Capacity);
        }
        *used = (rows, bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[test]
    fn explicit_limits_are_shared_and_preserved() -> anyhow::Result<()> {
        let check = || Ok(());
        let participant = OperationBudget::with_limits(&check, 4096, 8 * 1024 * 1024);
        participant.charge(1000, 3 * 1024 * 1024)?;
        participant.charge(3096, 5 * 1024 * 1024)?;
        assert!(matches!(participant.charge(0, 1), Err(Error::Capacity)));
        assert!(matches!(participant.charge(1, 0), Err(Error::Capacity)));
        let narrowed = OperationBudget::with_limits(&check, 1, 1);
        narrowed.charge(1, 1)?;
        assert!(matches!(narrowed.charge(0, 1), Err(Error::Capacity)));
        let larger = OperationBudget::with_limits(&check, 8192, 64 * 1024 * 1024);
        larger.charge(4096, 32 * 1024 * 1024)?;
        larger.charge(4096, 32 * 1024 * 1024)?;
        assert!(matches!(larger.charge(1, 0), Err(Error::Capacity)));
        assert!(matches!(larger.charge(0, 1), Err(Error::Capacity)));
        let empty = OperationBudget::with_limits(&check, 0, 0);
        empty.charge(0, 0)?;
        assert!(matches!(empty.charge(1, 0), Err(Error::Capacity)));
        assert!(matches!(empty.charge(0, 1), Err(Error::Capacity)));
        Ok(())
    }
    #[test]
    fn explicit_row_limits_do_not_add_a_byte_limit_or_clamp_rows() -> anyhow::Result<()> {
        let check = || Ok(());
        let manual = OperationBudget::with_row_limit(&check, 1024);
        manual.charge(1023, 64 * 1024 * 1024)?;
        manual.charge(1, 0)?;
        assert!(matches!(manual.charge(1, 0), Err(Error::Capacity)));
        let larger = OperationBudget::with_row_limit(&check, 8192);
        larger.charge(8192, 0)?;
        assert!(matches!(larger.charge(1, 0), Err(Error::Capacity)));
        Ok(())
    }
    #[test]
    fn default_accounting_exceeds_former_limits_and_preserves_cancellation() -> anyhow::Result<()> {
        let allowed = AtomicBool::new(true);
        let check = || {
            if allowed.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(ErrorCode::CapacityExceeded)
            }
        };
        let budget = OperationBudget::new(&check);
        budget.charge(2048, 16 * 1024 * 1024)?;
        budget.charge(2048, 16 * 1024 * 1024)?;
        budget.charge(1, 1)?;
        budget.charge(0, 0)?;
        allowed.store(false, Ordering::Release);
        assert!(matches!(
            budget.check(),
            Err(Error::Rejected(ErrorCode::CapacityExceeded))
        ));
        assert!(matches!(
            budget.charge(0, 0),
            Err(Error::Rejected(ErrorCode::CapacityExceeded))
        ));
        allowed.store(true, Ordering::Release);
        budget.charge(1, 1)?;
        Ok(())
    }
    #[test]
    fn row_and_byte_overflow_do_not_partially_consume_the_shared_budget() -> anyhow::Result<()> {
        let check = || Ok(());
        for (rows, bytes) in [(usize::MAX, 1), (1, usize::MAX)] {
            let budget = OperationBudget::new(&check);
            budget.charge(rows, bytes)?;
            assert!(matches!(budget.charge(1, 1), Err(Error::Capacity)));
            budget.charge(usize::MAX - rows, usize::MAX - bytes)?;
            assert!(matches!(budget.charge(1, 0), Err(Error::Capacity)));
            assert!(matches!(budget.charge(0, 1), Err(Error::Capacity)));
            budget.charge(0, 0)?;
        }
        Ok(())
    }
}
