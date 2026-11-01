//! Strict receipts reuse the admitted execution, writer and completion custody.
//! Public receipt DTOs are evidence; none of them creates an execution authority.

#[cfg(test)]
pub(in crate::tenancy) mod authority_test;
pub(crate) mod auxiliary;
pub(in crate::tenancy) mod auxiliary_store;
pub(in crate::tenancy) mod capacity;
pub(crate) mod evidence;
pub(crate) mod journal;
pub(crate) mod output;
pub(crate) mod owner;
pub(in crate::tenancy) mod read;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod recovery;
pub(in crate::tenancy) mod store;
pub(in crate::tenancy) mod usage_evidence;

use bitrouter_ai::tenancy::{execution::Outcome, operation::ErrorCode};

pub(crate) fn journal_error(error: journal::Error) -> ErrorCode {
    match error {
        journal::Error::Capacity => ErrorCode::CapacityExceeded,
        journal::Error::Binding => ErrorCode::Forbidden,
        journal::Error::Target => ErrorCode::CapabilityMismatch,
        journal::Error::Closed
        | journal::Error::Busy
        | journal::Error::Phase
        | journal::Error::Conflict
        | journal::Error::Poisoned => ErrorCode::AuthorityUnavailable,
    }
}

impl super::execution::TenantExecution {
    /// Evidence can forbid another attempt, never authorize one. The ordinary
    /// fallback policy and the current-authority send gate still both apply.
    pub(crate) fn receipt_blocks_fallback(&self) -> bool {
        match self
            .journal()
            .and_then(|journal| journal.fallback_evidence().map_err(journal_error))
        {
            Ok(None) => false,
            Ok(Some(attempt)) => {
                attempt.outcome != Outcome::Failed
                    || attempt
                        .http_status
                        .is_none_or(|status| matches!(status, 401 | 403))
            }
            Err(_) => true,
        }
    }
}
