//! Client-side payment middleware for upstream provider 402 handling.
//!
//! When an upstream provider returns `402 Payment Required`, the
//! [`PaymentLayer`] intercepts the response, signs a payment using the
//! configured OWS wallet, and retries the request with the payment credential.
//!
//! Supported payment methods:
//! - **Tempo session** — opens a payment channel, sends incremental vouchers
//! - **Tempo charge** — signs a one-time TIP-20 transfer transaction
//! - **Solana charge** — builds and broadcasts an SPL token transfer (feature-gated)

#[cfg(feature = "mpp-solana")]
pub mod solana_charge;

use std::path::Path;
use std::sync::Arc;

use bitrouter_config::config::BitrouterConfig;
use reqwest::{Request, Response, StatusCode, header::WWW_AUTHENTICATE};
use reqwest_middleware::{Middleware, Next};

use crate::runtime::ows_signer::OwsSigner;

/// Default Tempo RPC URL (Moderato testnet).
const DEFAULT_TEMPO_RPC_URL: &str = "https://rpc.moderato.tempo.xyz";

/// Default Solana RPC URL (mainnet-beta).
#[cfg(feature = "mpp-solana")]
const DEFAULT_SOLANA_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// reqwest-middleware 0.5 adapter that wraps an `mpp::client::MultiProvider`.
///
/// `mpp-br` targets reqwest-middleware 0.4, so we implement the 0.5 trait
/// directly and delegate the actual payment to the provider.
pub struct PaymentLayer {
    provider: Arc<mpp::client::MultiProvider>,
}

impl Middleware for PaymentLayer {
    fn handle<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        req: Request,
        extensions: &'life1 mut warp::http::Extensions,
        next: Next<'life2>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = reqwest_middleware::Result<Response>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let retry_req = req.try_clone();
            let resp = next.clone().run(req, extensions).await?;

            if resp.status() != StatusCode::PAYMENT_REQUIRED {
                return Ok(resp);
            }

            let retry_req = retry_req.ok_or_else(|| {
                reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                    "request could not be cloned for payment retry"
                ))
            })?;

            let www_auth = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .ok_or_else(|| {
                    reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                        "402 response missing WWW-Authenticate header"
                    ))
                })?
                .to_str()
                .map_err(|e| {
                    reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                        "invalid WWW-Authenticate header: {e}"
                    ))
                })?;

            let challenge = mpp::parse_www_authenticate(www_auth).map_err(|e| {
                reqwest_middleware::Error::Middleware(anyhow::anyhow!("invalid challenge: {e}"))
            })?;

            let credential = mpp::client::PaymentProvider::pay(provider.as_ref(), &challenge)
                .await
                .map_err(|e| {
                    reqwest_middleware::Error::Middleware(anyhow::anyhow!("payment failed: {e}"))
                })?;

            let auth_value = mpp::format_authorization(&credential).map_err(|e| {
                reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                    "failed to format credential: {e}"
                ))
            })?;

            let mut retry_req = retry_req;
            retry_req.headers_mut().insert(
                mpp::AUTHORIZATION_HEADER,
                auth_value.parse().map_err(|e| {
                    reqwest_middleware::Error::Middleware(anyhow::anyhow!(
                        "invalid authorization header value: {e}"
                    ))
                })?,
            );

            next.run(retry_req, extensions).await
        })
    }
}

/// Build the payment middleware for the reqwest client stack.
///
/// Returns `None` if no wallet payment config is present. The returned
/// middleware handles 402 responses automatically using the configured
/// OWS wallet.
pub fn build_payment_middleware(
    config: &BitrouterConfig,
) -> Result<Option<PaymentLayer>, PaymentSetupError> {
    let (wallet, payment_config) = match config
        .wallet
        .as_ref()
        .and_then(|w| w.payment.as_ref().map(|p| (w, p)))
    {
        Some(pair) => pair,
        None => return Ok(None),
    };

    let credential = std::env::var("OWS_PASSPHRASE").unwrap_or_default();
    let vault_path = wallet.vault_path.as_deref().map(Path::new);

    let tempo_rpc = payment_config
        .tempo_rpc_url
        .as_deref()
        .or_else(|| resolve_tempo_rpc_from_mpp(config))
        .unwrap_or(DEFAULT_TEMPO_RPC_URL);

    let mut multi = mpp::client::MultiProvider::new();

    // Tempo charge provider.
    let charge_signer = OwsSigner::new(&wallet.name, &credential, None, vault_path, None)
        .map_err(PaymentSetupError::Signer)?;
    let charge_addr = alloy::signers::Signer::address(&charge_signer);
    let tempo_charge = mpp::client::TempoProvider::new(charge_signer, tempo_rpc)
        .map_err(|e| PaymentSetupError::Provider(e.to_string()))?
        .with_client_id("bitrouter");
    multi.add(tempo_charge);

    // Tempo session provider.
    let session_signer = OwsSigner::new(&wallet.name, &credential, None, vault_path, None)
        .map_err(PaymentSetupError::Signer)?;
    let mut tempo_session = mpp::client::TempoSessionProvider::new(session_signer, tempo_rpc)
        .map_err(|e| PaymentSetupError::Provider(e.to_string()))?;

    if let Some(max) = payment_config.session_max_deposit {
        tempo_session = tempo_session.with_max_deposit(max);
    }
    if let Some(default) = payment_config.session_default_deposit {
        tempo_session = tempo_session.with_default_deposit(default);
    }
    multi.add(tempo_session);

    // Solana charge provider (behind feature flag).
    #[cfg(feature = "mpp-solana")]
    {
        let solana_rpc = payment_config
            .solana_rpc_url
            .as_deref()
            .or(config.solana_rpc_url.as_deref())
            .unwrap_or(DEFAULT_SOLANA_RPC_URL);

        let solana_provider =
            solana_charge::SolanaChargeProvider::new(wallet, &credential, solana_rpc)
                .map_err(|e| PaymentSetupError::Provider(e.to_string()))?;
        tracing::info!("payment client: Solana charge enabled");
        multi.add(solana_provider);
    }

    tracing::info!(
        wallet = %wallet.name,
        address = %charge_addr,
        "payment client enabled (Tempo session + charge)",
    );

    Ok(Some(PaymentLayer {
        provider: Arc::new(multi),
    }))
}

/// Try to resolve the Tempo RPC URL from the server-side MPP config.
fn resolve_tempo_rpc_from_mpp(config: &BitrouterConfig) -> Option<&str> {
    config
        .mpp
        .as_ref()
        .and_then(|m| m.networks.tempo.as_ref())
        .and_then(|t| t.rpc_url.as_deref())
}

/// Errors during payment middleware setup.
#[derive(Debug, thiserror::Error)]
pub enum PaymentSetupError {
    #[error("OWS signer: {0}")]
    Signer(crate::runtime::ows_signer::OwsSignerError),

    #[error("payment provider: {0}")]
    Provider(String),
}
