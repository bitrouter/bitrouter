//! Client-side payment gate for Proceeds / x402 / MPP paywalls.

use async_trait::async_trait;
use serde_json::Value;

/// A single payment-gated upstream call.
#[derive(Debug, Clone)]
pub struct PaymentRouteRequest {
    /// Proceeds paywall URL (`/api/x402/pay/...` or `/api/mpp/pay/...`).
    pub url: String,
    /// When true, run attestation after payment succeeds.
    pub attested: bool,
    /// Optional JSON POST body for the upstream call.
    pub body: Option<Value>,
    /// When true, use the MPP payment flow; otherwise x402.
    pub mpp: bool,
    /// Model id for attested routes (`qwen3.6` or `gemma4`).
    pub model: Option<String>,
    /// Prompt for attested routes.
    pub prompt: Option<String>,
}

/// Successful payment-gated response body.
#[derive(Debug, Clone)]
pub struct PaymentGateResult {
    /// Parsed upstream JSON body after payment succeeds.
    pub body: Value,
}

/// Pluggable client-side payment gate.
///
/// Deployments provide an implementation (e.g. the `bitrouter-pay` plugin's
/// `ArcPaymentGate`) and wire it into outbound HTTP middleware or host-specific
/// hooks.
#[async_trait]
pub trait PaymentGate: Send + Sync {
    /// Pay for access to a Proceeds paywall URL and return the upstream body.
    async fn pay(&self, request: PaymentRouteRequest) -> Result<PaymentGateResult, String>;
}
