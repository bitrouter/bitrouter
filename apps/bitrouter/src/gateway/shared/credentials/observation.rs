//! Bind authentication to the credential transaction that actually supplied it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use super::CredentialStamp;
use crate::gateway::providers::auth::store::StoreError;

tokio::task_local! {
    static CURRENT: Observation;
}

/// One authentication call. A transaction retains this handle from acquisition
/// through successful release, including across awaits and cancellation.
#[derive(Clone, Default)]
pub(crate) struct Observation(Arc<Mutex<Option<Result<CredentialStamp, StoreError>>>>);

impl Observation {
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(self.clone(), future).await
    }

    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }

    /// Only an awaited successful snapshot read or lease release may expose a
    /// version. A refresh records its committed replacement, never the
    /// pre-refresh version or a later read.
    pub fn record(&self, stamp: &CredentialStamp) -> Result<(), StoreError> {
        let mut observed = self.0.lock().map_err(|_| StoreError::Unavailable)?;
        if observed
            .as_ref()
            .is_some_and(|previous| previous.as_ref() != Ok(stamp))
        {
            *observed = Some(Err(StoreError::Conflict));
            return Err(StoreError::Conflict);
        }
        *observed = Some(Ok(stamp.clone()));
        Ok(())
    }

    pub fn finish(&self) -> Result<CredentialStamp, StoreError> {
        self.0
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .clone()
            .unwrap_or(Err(StoreError::Conflict))
    }
}
