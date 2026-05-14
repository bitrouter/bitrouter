//! Pipeline events emitted by `bitrouter-auth`.
//!
//! Downstream hooks (`bitrouter-settlement`'s `BalanceCheckHook` and the
//! `ChargeStrategy` chain) read these events for caller identity instead of
//! querying the `api_keys` table directly — plugin DB isolation (003 §3.4.6).

use serde::Serialize;

use bitrouter_sdk::PipelineEvent;
use bitrouter_sdk::caller::PaymentMethod;

/// Authentication succeeded — carries the caller's identity. Downstream hooks
/// take identity from this event, not from the `api_keys` table.
#[derive(Debug, Clone, Serialize)]
pub struct Authenticated {
    /// The authenticated api key id.
    pub api_key_id: String,
    /// The owning user id.
    pub user_id: String,
    /// How this caller pays.
    pub payment_method: PaymentMethod,
    /// The policy id bound to the key, if any (read by `bitrouter-policy`).
    pub policy_id: Option<String>,
}

impl PipelineEvent for Authenticated {
    fn event_name(&self) -> &'static str {
        "auth.authenticated"
    }
}

/// An MPP payment credential was verified — carries the channel session id and
/// the verified channel balance (micro-USD).
#[derive(Debug, Clone, Serialize)]
pub struct MppVerified {
    /// The MPP channel session id.
    pub session_id: String,
    /// The verified channel balance in micro-USD.
    pub channel_balance: i64,
}

impl PipelineEvent for MppVerified {
    fn event_name(&self) -> &'static str {
        "auth.mpp_verified"
    }
}
