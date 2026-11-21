//! Request-local execution constraints.

use crate::gateway::error::Result;
use crate::managed_contract::evidence::NativeInputCount;

/// Kept inside the live pipeline; only request digests cross the core boundary.
#[derive(Default)]
pub(crate) struct NativeCountedRequests(std::sync::Mutex<Option<String>>);

impl NativeCountedRequests {
    pub(crate) fn select(&self, count: Option<&NativeInputCount>) -> Result<()> {
        let mut current = self.0.lock().map_err(|_| {
            crate::gateway::error::BitrouterError::internal("input count binding unavailable")
        })?;
        *current = match count {
            Some(NativeInputCount::Counted { request_sha256, .. }) => Some(request_sha256.clone()),
            _ => None,
        };
        Ok(())
    }

    pub(crate) fn matches(&self, digest: &str) -> Result<bool> {
        Ok(self
            .0
            .lock()
            .map_err(|_| {
                crate::gateway::error::BitrouterError::internal("input count binding unavailable")
            })?
            .as_deref()
            == Some(digest))
    }
}

/// A managed request's immutable output bound, checked after provider shaping
/// and authentication, immediately before the HTTP request can be sent.
pub(crate) struct NativeOutputReservation(pub u32);

/// Every controlled call carries this marker, even before output admission.
pub(crate) struct NativeManagedRequest;

/// A model-only policy requires the complete admitted input, including when
/// the native caller pins a model and bypasses policy selection.
pub(crate) struct PreserveInput;

/// Maximum decoded HTTP entity bytes consumed for one managed provider reply.
pub(crate) struct NativeResponseByteLimit(pub u64);
