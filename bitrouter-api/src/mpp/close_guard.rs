use std::sync::Arc;

use super::state::MppState;

/// RAII guard that closes a Tempo payment channel when dropped.
///
/// Move this into the request handler's `tokio::spawn` block (streaming) or
/// hold it in the handler scope (non-streaming). When the guard is dropped —
/// whether due to success, error, or client disconnect — it spawns a detached
/// tokio task that submits the on-chain `close()` transaction using the
/// highest voucher the server received during the session.
///
/// Close failures are logged but never propagate; the channel can be settled
/// later if the close transaction fails.
pub struct SessionCloseGuard {
    mpp_state: Arc<MppState>,
    backend_key: String,
    channel_id: String,
}

impl SessionCloseGuard {
    pub fn new(mpp_state: Arc<MppState>, backend_key: String, channel_id: String) -> Self {
        Self {
            mpp_state,
            backend_key,
            channel_id,
        }
    }
}

impl Drop for SessionCloseGuard {
    #[cfg(feature = "mpp-tempo")]
    fn drop(&mut self) {
        let mpp_state = Arc::clone(&self.mpp_state);
        let backend_key = self.backend_key.clone();
        let channel_id = self.channel_id.clone();

        tokio::spawn(async move {
            if let Err(e) = mpp_state.close_channel(&backend_key, &channel_id).await {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "server-side channel close failed"
                );
            }
        });
    }

    #[cfg(not(feature = "mpp-tempo"))]
    fn drop(&mut self) {
        // Channel close is only implemented for Tempo currently.
        let _ = (&self.mpp_state, &self.backend_key, &self.channel_id);
    }
}
