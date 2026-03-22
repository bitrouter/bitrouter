//! MPP (Machine Payment Protocol) client builder.
//!
//! Constructs a [`ClientWithMiddleware`] that automatically handles HTTP 402
//! responses from upstream providers using the Tempo payment network.
//! When an upstream returns 402 with a `WWW-Authenticate` challenge, the
//! middleware signs a TIP-20 transfer and retries with a credential.
//!
//! The mpp crate ships its own `PaymentMiddleware` for `reqwest-middleware 0.4`,
//! but bitrouter uses `0.5`. This module provides a thin adapter
//! ([`MppPaymentMiddleware`]) that bridges the version gap by calling the
//! provider's `pay()` method directly.

use async_trait::async_trait;
use bitrouter_core::auth::keys::MasterKeypair;
use http::Extensions;
use mpp::client::{PaymentProvider, TempoProvider};
use mpp::protocol::core::{AUTHORIZATION_HEADER, format_authorization, parse_www_authenticate};
use reqwest::{Request, Response, StatusCode, header::WWW_AUTHENTICATE};
use reqwest_middleware::{ClientWithMiddleware, Middleware, Next};

use crate::runtime::error::{Result, RuntimeError};

/// Default Tempo RPC endpoint.
pub const DEFAULT_TEMPO_RPC_URL: &str = "https://rpc.tempo.xyz";

/// Reqwest 0.5 middleware that intercepts 402 responses and pays via MPP.
struct MppPaymentMiddleware<P> {
    provider: P,
}

impl<P> MppPaymentMiddleware<P> {
    fn new(provider: P) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<P> Middleware for MppPaymentMiddleware<P>
where
    P: PaymentProvider + 'static,
{
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let retry_req = req.try_clone();
        let resp = next.clone().run(req, extensions).await?;

        if resp.status() != StatusCode::PAYMENT_REQUIRED {
            return Ok(resp);
        }

        let retry_req = retry_req.ok_or_else(|| {
            reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                "request could not be cloned for payment retry",
            ))
        })?;

        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .ok_or_else(|| {
                reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                    "402 response missing WWW-Authenticate header",
                ))
            })?
            .to_str()
            .map_err(|e| {
                reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                    "invalid WWW-Authenticate header: {e}",
                ))
            })?;

        let challenge = parse_www_authenticate(www_auth).map_err(|e| {
            reqwest_middleware::Error::Middleware(anyhow::anyhow!("invalid challenge: {e}"))
        })?;

        let credential = self.provider.pay(&challenge).await.map_err(|e| {
            reqwest_middleware::Error::Middleware(anyhow::anyhow!("payment failed: {e}"))
        })?;

        let auth_header = format_authorization(&credential).map_err(|e| {
            reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                "failed to format credential: {e}",
            ))
        })?;

        let mut retry_req = retry_req;
        let header_name = reqwest::header::HeaderName::from_static(AUTHORIZATION_HEADER);

        // Combine existing Bearer token (if any) with the Payment credential.
        let combined = if let Some(existing) = retry_req.headers().get(&header_name) {
            let existing_str = existing.to_str().unwrap_or("");
            let bearer_part = existing_str
                .split(',')
                .map(|s| s.trim())
                .find(|s| s.starts_with("Bearer "));
            if let Some(bearer) = bearer_part {
                format!("{bearer}, {auth_header}")
            } else {
                auth_header
            }
        } else {
            auth_header
        };

        retry_req.headers_mut().insert(
            header_name,
            combined
                .parse()
                .map_err(|e: reqwest::header::InvalidHeaderValue| {
                    reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                        "invalid authorization header: {e}",
                    ))
                })?,
        );

        next.run(retry_req, extensions).await
    }
}

/// Build an MPP-capable HTTP client from a master keypair and RPC URL.
///
/// The returned [`ClientWithMiddleware`] wraps the given base client with
/// [`MppPaymentMiddleware`]: every outgoing request is sent normally, but if
/// the upstream returns HTTP 402 with a `WWW-Authenticate` header the
/// middleware signs a Tempo payment and retries.
pub fn build_mpp_client(
    keypair: &MasterKeypair,
    rpc_url: &str,
    base_client: reqwest::Client,
) -> Result<ClientWithMiddleware> {
    let signer = keypair
        .evm_signer()
        .map_err(|e| RuntimeError::Mpp(format!("failed to derive EVM signer: {e}")))?;
    let provider = TempoProvider::new(signer, rpc_url)
        .map_err(|e| RuntimeError::Mpp(format!("failed to create Tempo provider: {e}")))?;
    let middleware = MppPaymentMiddleware::new(provider);
    let client = reqwest_middleware::ClientBuilder::new(base_client)
        .with(middleware)
        .build();
    Ok(client)
}
