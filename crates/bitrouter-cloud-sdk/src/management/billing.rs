//! `/v1/billing/*` — credit balance and Stripe checkout sessions.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::billing`. The
//! checkout endpoint requires `billing:write`, which is *not* in the
//! default scope set ([`crate::auth::settings::DEFAULT_SCOPE`]); callers
//! who want it must re-login with `--scope '… billing:write'`.

use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// Wire shape for `GET /v1/billing/balance`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceResponse {
    /// Raw balance from the credit account (before pending debits).
    pub balance_micro_usd: i64,
    /// Sum of pending debits not yet drained into the credit account.
    pub pending_debits_micro_usd: i64,
    /// `max(balance - pending, 0)` — what the next inference call
    /// will see.
    pub available_micro_usd: i64,
    /// Currency code (today: `"USD"`).
    pub currency: String,
}

/// Body for `POST /v1/billing/checkout/sessions`.
#[derive(Debug, Clone, Serialize)]
pub struct CheckoutSessionRequest {
    /// Credit-purchase amount in USD cents (not micro-USD). Subject
    /// to the node's configured min/max bounds.
    pub amount_cents: i64,
}

/// Wire shape returned by `POST /v1/billing/checkout/sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutSessionResponse {
    /// Stripe Checkout session ID.
    pub id: String,
    /// Hosted checkout URL — open in a browser to complete payment.
    pub url: String,
}

impl ManagementClient {
    /// `GET /v1/billing/balance` — read the account's credit balance.
    pub async fn billing_balance(&self) -> Result<BalanceResponse> {
        self.get_json("/v1/billing/balance").await
    }

    /// `POST /v1/billing/checkout/sessions` — start a Stripe checkout
    /// flow for a credit top-up. The response carries a URL the user
    /// should open to complete payment.
    pub async fn create_checkout_session(
        &self,
        body: &CheckoutSessionRequest,
    ) -> Result<CheckoutSessionResponse> {
        self.post_json("/v1/billing/checkout/sessions", body).await
    }
}
