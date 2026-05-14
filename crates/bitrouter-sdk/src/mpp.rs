//! `MppVerifier` — the MPP credential-verification contract (crate root).
//!
//! `AuthHook` (in `bitrouter-auth`) authenticates an MPP-paying caller by
//! verifying a `Payment-SIGNATURE` header; the actual channel state lives in
//! `bitrouter-settlement`'s `MppState`. Rather than make `bitrouter-auth`
//! depend on `bitrouter-settlement`, the SDK defines this trait — auth
//! consumes `Arc<dyn MppVerifier>`, settlement implements it (the same
//! SDK-defines-the-trait pattern as [`crate::MetricsStore`]).
//!
//! v1.0 verifies the **Tempo** channel only (008 §1.1).

use crate::Result;
use async_trait::async_trait;

/// The verified identity behind an MPP payment credential.
#[derive(Debug, Clone)]
pub struct MppVerification {
    /// The MPP channel session id.
    pub session_id: String,
    /// The user id that opened the channel.
    pub user_id: String,
    /// The channel's current balance, in micro-USD.
    pub channel_balance_micro_usd: i64,
}

/// Verifies an MPP payment credential (the `Payment-SIGNATURE` header value).
///
/// The core crate defines the trait; `bitrouter-settlement::MppState`
/// implements it. `Ok(Some(_))` means the credential resolved to a known
/// channel; `Ok(None)` means it did not (the caller then 402s); `Err` is an
/// infrastructure failure.
#[async_trait]
pub trait MppVerifier: Send + Sync {
    /// Verify `credential`, resolving it to a channel session if it is valid.
    async fn verify(&self, credential: &str) -> Result<Option<MppVerification>>;
}
