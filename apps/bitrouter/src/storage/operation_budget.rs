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
}
impl<'a> OperationBudget<'a> {
    pub(crate) fn new(check: &'a Check<'a>) -> Self {
        Self {
            check,
            used: Mutex::new((0, 0)),
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
        if rows > 4096 || bytes > 32 * 1024 * 1024 {
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
