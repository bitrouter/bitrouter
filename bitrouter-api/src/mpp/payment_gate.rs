use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::filter::MppPaymentContext;

/// Pinned boxed future used as the return type for [`PaymentGate`] methods.
type GateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trait abstracting payment verification and settlement for MPP handlers.
///
/// Allows downstream crates to provide custom payment logic (e.g. charge-based
/// balance management) while reusing the routing filters and handlers from
/// `bitrouter-api`.
///
/// [`super::MppState`] provides the default implementation that delegates to
/// configured Tempo / Solana session backends.
pub trait PaymentGate: Send + Sync {
    /// Verify payment before request processing.
    ///
    /// `chain` is the CAIP-2 chain identifier from the caller's JWT claims.
    /// `auth_header` is the raw `Authorization` header value.
    ///
    /// Returns [`MppPaymentContext`] on success. If no valid credential is
    /// present, rejects with a [`super::MppChallenge`] (402 Payment Required).
    fn verify_payment(
        &self,
        chain: Option<String>,
        auth_header: Option<String>,
    ) -> GateFuture<'_, Result<MppPaymentContext, warp::Rejection>>;

    /// Deduct `amount` micro-units from the channel in the specified backend.
    ///
    /// For session-based flows this debits from the payment channel store.
    /// Custom implementations may debit from a centralized balance instead.
    fn deduct<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
        amount: u128,
    ) -> GateFuture<'a, Result<(), mpp::server::VerificationError>>;

    /// Wait for the next channel update on the given backend.
    ///
    /// Used by metered SSE to pause until a new voucher arrives (or balance
    /// is replenished).
    fn wait_for_update<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, ()>;

    /// Retrieve channel balance info for the NeedVoucher event.
    ///
    /// Returns `(settled, authorized, deposit)` in micro-units, or `None`
    /// if the channel is not found.
    fn channel_balance<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, Option<(u128, u128, u128)>>;

    /// Close a payment channel.
    ///
    /// For Tempo, this submits an on-chain close transaction. Custom
    /// implementations may no-op if there is no channel to close.
    fn close_channel<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, Result<(), String>>;
}

/// Blanket implementation: `Arc<dyn PaymentGate>` itself implements `PaymentGate`.
impl PaymentGate for Arc<dyn PaymentGate> {
    fn verify_payment(
        &self,
        chain: Option<String>,
        auth_header: Option<String>,
    ) -> GateFuture<'_, Result<MppPaymentContext, warp::Rejection>> {
        (**self).verify_payment(chain, auth_header)
    }

    fn deduct<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
        amount: u128,
    ) -> GateFuture<'a, Result<(), mpp::server::VerificationError>> {
        (**self).deduct(backend_key, channel_id, amount)
    }

    fn wait_for_update<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, ()> {
        (**self).wait_for_update(backend_key, channel_id)
    }

    fn channel_balance<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, Option<(u128, u128, u128)>> {
        (**self).channel_balance(backend_key, channel_id)
    }

    fn close_channel<'a>(
        &'a self,
        backend_key: &'a str,
        channel_id: &'a str,
    ) -> GateFuture<'a, Result<(), String>> {
        (**self).close_channel(backend_key, channel_id)
    }
}
