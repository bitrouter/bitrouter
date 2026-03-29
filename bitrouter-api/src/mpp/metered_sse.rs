//! Metered SSE streaming with per-token deduction.
//!
//! Wraps a warp SSE mpsc sender with balance deduction logic. Before each
//! data event is sent, the configured `tick_cost` is atomically deducted
//! from the payment channel. If the balance is exhausted, a
//! `payment-need-voucher` SSE event is emitted and the stream pauses
//! until the client submits a new voucher.

use std::sync::Arc;

use mpp::server::sse::NeedVoucherEvent;
use tokio::sync::mpsc;

use super::payment_gate::PaymentGate;

/// Default poll interval when `wait_for_update` returns immediately.
const DEFAULT_POLL_INTERVAL_MS: u64 = 100;

/// Maximum number of consecutive need-voucher retries before giving up.
const MAX_NEED_VOUCHER_RETRIES: u32 = 60;

/// Context for metered SSE streaming.
///
/// Passed into the streaming task so it can atomically deduct per-token
/// costs from the payment channel and emit `payment-need-voucher` events
/// when the balance is exhausted.
pub struct MeteredSseContext {
    pub payment_gate: Arc<dyn PaymentGate>,
    pub backend_key: String,
    pub channel_id: String,
    pub tick_cost: u128,
}

impl MeteredSseContext {
    /// Deduct `tick_cost` from the channel before sending a data event.
    ///
    /// If the balance is insufficient, emits a `payment-need-voucher` event
    /// via `tx` and waits for a channel update. Returns `true` if the
    /// deduction succeeded (caller should proceed to send data), `false`
    /// if the client disconnected or retries were exhausted.
    pub async fn deduct_or_pause(
        &self,
        tx: &mpsc::Sender<Result<warp::sse::Event, std::convert::Infallible>>,
    ) -> bool {
        if self.tick_cost == 0 {
            return true;
        }

        for _ in 0..MAX_NEED_VOUCHER_RETRIES {
            match self
                .payment_gate
                .deduct(&self.backend_key, &self.channel_id, self.tick_cost)
                .await
            {
                Ok(()) => return true,
                Err(_) => {
                    // Emit need-voucher event.
                    if let Some((settled, authorized, deposit)) = self
                        .payment_gate
                        .channel_balance(&self.backend_key, &self.channel_id)
                        .await
                    {
                        let event = NeedVoucherEvent {
                            channel_id: self.channel_id.clone(),
                            required_cumulative: (settled + self.tick_cost).to_string(),
                            accepted_cumulative: authorized.to_string(),
                            deposit: deposit.to_string(),
                        };
                        let sse = warp::sse::Event::default()
                            .event("payment-need-voucher")
                            .data(serde_json::to_string(&event).unwrap_or_default());
                        if tx.send(Ok(sse)).await.is_err() {
                            return false; // client disconnected
                        }
                    }

                    // Wait for channel update or poll interval.
                    tokio::select! {
                        () = self.payment_gate.wait_for_update(&self.backend_key, &self.channel_id) => {},
                        () = tokio::time::sleep(tokio::time::Duration::from_millis(DEFAULT_POLL_INTERVAL_MS)) => {},
                    }
                }
            }
        }

        tracing::warn!(
            channel_id = %self.channel_id,
            "metered SSE: max need-voucher retries exceeded"
        );
        false
    }
}
