use std::sync::Arc;

use super::payment_gate::PaymentGate;

/// RAII guard that closes a payment channel when dropped.
///
/// Move this into the request handler's `tokio::spawn` block (streaming) or
/// hold it in the handler scope (non-streaming). When the guard is dropped —
/// whether due to success, error, or client disconnect — it spawns a detached
/// tokio task that calls [`PaymentGate::close_channel`].
///
/// Close failures are logged but never propagate; the channel can be settled
/// later if the close transaction fails.
pub struct SessionCloseGuard {
    payment_gate: Arc<dyn PaymentGate>,
    backend_key: String,
    channel_id: String,
}

impl SessionCloseGuard {
    pub fn new(
        payment_gate: Arc<dyn PaymentGate>,
        backend_key: String,
        channel_id: String,
    ) -> Self {
        Self {
            payment_gate,
            backend_key,
            channel_id,
        }
    }
}

impl Drop for SessionCloseGuard {
    fn drop(&mut self) {
        let payment_gate = Arc::clone(&self.payment_gate);
        let backend_key = self.backend_key.clone();
        let channel_id = self.channel_id.clone();

        tokio::spawn(async move {
            if let Err(e) = payment_gate.close_channel(&backend_key, &channel_id).await {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "server-side channel close failed"
                );
            }
        });
    }
}
