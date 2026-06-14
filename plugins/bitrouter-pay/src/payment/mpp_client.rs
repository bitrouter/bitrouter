//! Client-side MPP pay-as-you-go wrapper around [`ArcMppBackend`].
//!
//! This is the caller side of the Machine Payments Protocol (mpp.dev): it drives
//! the full pay-as-you-go handshake against any MPP-gated endpoint
//!
//! 1. POST the request — BitRouter replies `402 Payment Required`. The charge
//!    challenge is carried in the JSON error body, not a `WWW-Authenticate`
//!    header:
//!
//!    ```json
//!    {"error":{"message":"payment required: Payment id=\"...\", realm=\"bitrouter\", method=\"tempo\", intent=\"charge\", request=\"...\", expires=\"...Z\"","type":"payment_required"}}
//!    ```
//!
//!    The embedded `Payment ...` token is extracted from `error.message`.
//! 2. Sign the charge with the OWS-backed `agent-treasury` wallet (delegated to
//!    [`MppBackend::pay`], which signs with `ArcLocalSigner` and submits the
//!    on-chain transfer on Arc testnet).
//! 3. Re-POST with the `Authorization: Payment <credential>` header.
//! 4. The server verifies on-chain and returns `200` plus a `Payment-Receipt`
//!    header carrying the settled transaction hash.
//!
//! For visibility, a `WWW-Authenticate` header carrying a `Payment` challenge is
//! still honoured as a fallback.
//!
//! [`MppClient`](super::mpp::MppClient) does the same retry but discards the
//! receipt; this wrapper surfaces it so callers can log / assert the tx hash.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::PayError;
use crate::payment::mpp::MppBackend;

/// BitRouter's `402` error envelope: `{"error":{"message":"...","type":"..."}}`.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    message: String,
}

/// A response obtained by paying for an MPP-gated endpoint.
#[derive(Debug, Clone)]
pub struct PaidResponse {
    /// Final HTTP status of the paid (post-402) request.
    pub status: u16,
    /// Parsed JSON body returned by the endpoint after payment.
    pub body: Value,
    /// The on-chain settlement receipt, when the server returned a
    /// `Payment-Receipt` header. `reference` holds the transaction hash.
    #[cfg(feature = "mpp")]
    pub receipt: Option<mpp_br::Receipt>,
}

impl PaidResponse {
    /// The settled transaction hash, when the server returned a receipt.
    #[cfg(feature = "mpp")]
    pub fn tx_hash(&self) -> Option<&str> {
        self.receipt.as_ref().map(|r| r.reference.as_str())
    }
}

/// Pay-as-you-go MPP client: wraps an [`MppBackend`] (typically
/// [`ArcMppBackend`](super::mpp::ArcMppBackend)) and a `reqwest` client.
pub struct ArcMppPayClient {
    backend: Arc<dyn MppBackend>,
    http: reqwest::Client,
}

impl ArcMppPayClient {
    /// Build a pay-as-you-go client over the given backend.
    pub fn new(backend: Arc<dyn MppBackend>) -> Self {
        Self {
            backend,
            http: reqwest::Client::new(),
        }
    }

    /// Run the full pay-as-you-go flow against `url`, returning the paid
    /// response body together with the settlement receipt.
    ///
    /// If the first request already succeeds (no paywall), it is returned as-is
    /// with no receipt. If it returns `402`, the challenge is paid and the
    /// request retried; a non-success retry is reported as
    /// [`PayError::PaymentFailed`].
    pub async fn post(&self, url: &str, body: Option<Value>) -> Result<PaidResponse, PayError> {
        info!(url, "MPP pay-as-you-go: sending initial request");
        let first = self.send(url, body.clone(), None).await?;
        let status = first.status();

        if status.as_u16() != 402 {
            debug!(
                status = status.as_u16(),
                "non-402 response; no payment needed"
            );
            return Self::into_paid_response(first).await;
        }

        // 402 Payment Required — surface the raw challenge before parsing it.
        let www_auth = first
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let raw_body = first
            .text()
            .await
            .map_err(|e| PayError::UpstreamError(format!("reading 402 body: {e}")))?;
        debug!(%raw_body, "raw 402 challenge body from upstream");

        // BitRouter carries the challenge in the JSON error body; a
        // `WWW-Authenticate: Payment ...` header is honoured as a fallback.
        let challenge = match www_auth.as_deref() {
            Some(h) if h.to_ascii_lowercase().contains("payment ") => {
                info!("parsing MPP challenge from WWW-Authenticate header");
                h.to_string()
            }
            _ => {
                info!("parsing MPP challenge from 402 JSON error body");
                challenge_from_error_body(&raw_body)?
            }
        };
        debug!(challenge, "parsed MPP charge challenge");

        // Sign + submit the on-chain payment via the OWS-backed backend
        // (ArcLocalSigner settles the tempo transfer on Arc testnet).
        info!("signing + submitting tempo payment via ArcLocalSigner");
        let authorization = self.backend.pay(&challenge).await?;

        info!("payment submitted; retrying request with Authorization header");
        let paid = self.send(url, body, Some(authorization)).await?;
        let paid_status = paid.status();
        if !paid_status.is_success() {
            let text = paid.text().await.unwrap_or_default();
            // A SECOND 402 means the server saw the credential but did not accept
            // it — surface the verbatim rejection body, and (when it carries a
            // fresh `Payment ...` challenge) the re-challenge message, so the
            // mismatch (malformed credential vs on-chain verification failure)
            // is visible in the logs.
            if paid_status.as_u16() == 402 {
                warn!(
                    status = 402,
                    body = %text,
                    "MPP retry REJECTED with a second 402 — server did not accept the signed credential"
                );
                if let Ok(rechallenge) = challenge_from_error_body(&text) {
                    warn!(
                        %rechallenge,
                        "server issued a fresh challenge instead of accepting payment"
                    );
                }
            } else {
                warn!(
                    status = paid_status.as_u16(),
                    body = %text,
                    "MPP retry failed with a non-success status"
                );
            }
            return Err(PayError::PaymentFailed(format!(
                "MPP retry returned {paid_status}: {text}"
            )));
        }
        info!(
            status = paid_status.as_u16(),
            "payment accepted; received inference response"
        );
        Self::into_paid_response(paid).await
    }

    async fn send(
        &self,
        url: &str,
        body: Option<Value>,
        authorization: Option<String>,
    ) -> Result<reqwest::Response, PayError> {
        let mut req = self.http.post(url);
        if let Some(json) = body {
            req = req.json(&json);
        }
        if let Some(auth) = authorization {
            #[cfg(feature = "mpp")]
            {
                // Log the exact header name + value being sent so a server-side
                // re-challenge can be diagnosed against what the client emitted.
                debug!(
                    header = mpp_br::AUTHORIZATION_HEADER,
                    value = %auth,
                    "attaching MPP credential to retry request"
                );
                req = req.header(mpp_br::AUTHORIZATION_HEADER, auth);
            }
            #[cfg(not(feature = "mpp"))]
            {
                debug!(header = "authorization", value = %auth, "attaching credential");
                req = req.header("authorization", auth);
            }
        }
        req.send()
            .await
            .map_err(|e| PayError::UpstreamError(e.to_string()))
    }

    /// Convert a successful response into a [`PaidResponse`], parsing the
    /// `Payment-Receipt` header when present.
    async fn into_paid_response(resp: reqwest::Response) -> Result<PaidResponse, PayError> {
        let status = resp.status().as_u16();

        #[cfg(feature = "mpp")]
        let receipt = resp
            .headers()
            .get(mpp_br::PAYMENT_RECEIPT_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| mpp_br::parse_receipt(v).ok());

        let raw = resp
            .text()
            .await
            .map_err(|e| PayError::UpstreamError(format!("reading response body: {e}")))?;
        let body = serde_json::from_str::<Value>(&raw).map_err(|e| {
            PayError::UpstreamError(format!("decoding response body as JSON ({e}); body: {raw}"))
        })?;

        Ok(PaidResponse {
            status,
            body,
            #[cfg(feature = "mpp")]
            receipt,
        })
    }
}

/// Extract the MPP `Payment ...` charge challenge from BitRouter's `402` JSON
/// error body.
///
/// The body is `{"error":{"message":"payment required: Payment id=\"...\", ...","type":"payment_required"}}`.
/// The `error.message` value (already JSON-unescaped by serde) is scanned for the
/// `Payment ` token, and everything from there on is returned verbatim so it can
/// be parsed by `mpp_br::parse_www_authenticate`.
fn challenge_from_error_body(raw: &str) -> Result<String, PayError> {
    let envelope: ErrorEnvelope = serde_json::from_str(raw).map_err(|e| {
        PayError::InvalidChallenge(format!(
            "402 body is not the expected error JSON ({e}); body: {raw}"
        ))
    })?;

    let message = envelope.error.message;
    let start = message.find("Payment ").ok_or_else(|| {
        PayError::InvalidChallenge(format!(
            "no MPP 'Payment' challenge found in error message: {message}"
        ))
    })?;

    Ok(message[start..].trim().to_string())
}
