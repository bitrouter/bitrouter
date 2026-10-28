//! One bounded operation across trajectory and Eval, with H checkpoints.
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Mutex;

pub(crate) type Check<'a> = dyn Fn() -> Result<(), ErrorCode> + Sync + 'a;
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
        Self::with_row_limit(check, 4096)
    }
    /// A participant may narrow the common operation budget, never widen it.
    pub(crate) fn with_row_limit(check: &'a Check<'a>, rows: usize) -> Self {
        Self::with_limits(check, rows, 32 * 1024 * 1024)
    }
    /// Narrow both cumulative limits for a participant such as owned learning.
    /// All its borrowed storage readers must share this one budget.
    pub(crate) fn with_limits(check: &'a Check<'a>, rows: usize, bytes: usize) -> Self {
        Self {
            check,
            used: Mutex::new((0, 0)),
            rows: rows.min(4096),
            bytes: bytes.min(32 * 1024 * 1024),
        }
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(crate) fn usage(&self) -> Result<(usize, usize), Error> {
        self.check()?;
        self.used
            .lock()
            .map(|value| *value)
            .map_err(|_| Error::Unavailable)
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
    fn learner_limits_charge_all_participants_without_raising_other_profiles() -> anyhow::Result<()>
    {
        let check = || Ok(());
        let learner = OperationBudget::with_limits(&check, 4096, 8 * 1024 * 1024);
        learner.charge(1000, 3 * 1024 * 1024)?;
        learner.charge(3096, 5 * 1024 * 1024)?;
        assert!(matches!(learner.charge(0, 1), Err(Error::Capacity)));
        assert!(matches!(learner.charge(1, 0), Err(Error::Capacity)));
        let narrowed = OperationBudget::with_limits(&check, 1, 1);
        narrowed.charge(1, 1)?;
        assert!(matches!(narrowed.charge(0, 1), Err(Error::Capacity)));
        let attempted = OperationBudget::with_limits(&check, usize::MAX, usize::MAX);
        attempted.charge(4096, 32 * 1024 * 1024)?;
        assert!(matches!(attempted.charge(1, 0), Err(Error::Capacity)));
        assert!(matches!(attempted.charge(0, 1), Err(Error::Capacity)));
        Ok(())
    }
    #[test]
    fn participants_can_narrow_but_never_raise_the_shared_row_bound() -> anyhow::Result<()> {
        let check = || Ok(());
        let manual = OperationBudget::with_row_limit(&check, 1024);
        manual.charge(1023, 0)?;
        manual.charge(1, 0)?;
        assert!(matches!(manual.charge(1, 0), Err(Error::Capacity)));
        let attempted_widening = OperationBudget::with_row_limit(&check, usize::MAX);
        attempted_widening.charge(4096, 0)?;
        assert!(matches!(
            attempted_widening.charge(1, 0),
            Err(Error::Capacity)
        ));
        Ok(())
    }
    #[test]
    fn aggregate_bytes_rows_and_rejection_are_shared_and_do_not_wrap() -> anyhow::Result<()> {
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
        assert!(matches!(budget.charge(1, 0), Err(Error::Capacity)));
        assert!(matches!(budget.charge(0, 1), Err(Error::Capacity)));
        assert!(matches!(
            budget.charge(usize::MAX, usize::MAX),
            Err(Error::Capacity)
        ));
        budget.charge(0, 0)?;
        allowed.store(false, Ordering::Release);
        assert!(matches!(
            budget.check(),
            Err(Error::Rejected(ErrorCode::CapacityExceeded))
        ));
        Ok(())
    }
}
