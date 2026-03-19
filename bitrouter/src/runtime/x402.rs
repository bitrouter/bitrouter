//! x402 payment signer and JWT auth middleware.
//!
//! Loads the master Solana keypair at startup and constructs an x402
//! [`ClientWithMiddleware`] that automatically signs payment flows
//! when an upstream provider returns HTTP 402.
//!
//! For bitrouter-cloud, a [`JwtAuthMiddleware`] is stacked on top so
//! that every request also carries a short-lived JWT proving the
//! caller's on-chain identity.

use std::path::Path;

use async_trait::async_trait;
use bitrouter_core::auth::{
    chain::Chain,
    claims::{BitrouterClaims, TokenScope},
    keys::MasterKeypair,
    token,
};
use http::Extensions;
use reqwest::{Request, Response, header::HeaderValue};
use reqwest_middleware::{ClientWithMiddleware, Middleware, Next};
use solana_keypair::read_keypair_file;
use x402_signer::{X402Client, middleware::X402PaymentMiddleware, svm::SvmPaymentSigner};

use crate::runtime::error::{Result, RuntimeError};

/// JWT validity period (5 minutes).
const JWT_LIFETIME_SECS: u64 = 300;

// ── JWT auth middleware ───────────────────────────────────────

/// Reqwest middleware that attaches a short-lived JWT `Authorization` header
/// to every outgoing request.
///
/// The JWT is signed with SOL_EDDSA using the master keypair and carries the
/// caller's CAIP-10 Solana identity as `iss`.
pub struct JwtAuthMiddleware {
    keypair: MasterKeypair,
    chain: Chain,
}

impl JwtAuthMiddleware {
    pub fn new(keypair: MasterKeypair, chain: Chain) -> Self {
        Self { keypair, chain }
    }
}

#[async_trait]
impl Middleware for JwtAuthMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let caip10 = self
            .keypair
            .caip10(&self.chain)
            .map_err(reqwest_middleware::Error::middleware)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(reqwest_middleware::Error::middleware)?
            .as_secs();

        let claims = BitrouterClaims {
            iss: caip10.format(),
            chain: self.chain.caip2(),
            iat: Some(now),
            exp: Some(now + JWT_LIFETIME_SECS),
            scope: TokenScope::Api,
            models: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };

        let jwt =
            token::sign(&claims, &self.keypair).map_err(reqwest_middleware::Error::middleware)?;

        let value = HeaderValue::from_str(&format!("Bearer {jwt}"))
            .map_err(reqwest_middleware::Error::middleware)?;
        req.headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);

        next.run(req, extensions).await
    }
}

// ── x402 client builders ─────────────────────────────────────

/// Build an x402-capable HTTP client from the master wallet and RPC URL.
///
/// The returned [`ClientWithMiddleware`] wraps the given base client with
/// [`X402PaymentMiddleware`]: every outgoing request is sent normally, but if
/// the upstream returns HTTP 402 with a `PAYMENT-REQUIRED` header the
/// middleware automatically signs a Solana payment and retries.
///
/// When `with_jwt` is `true`, a [`JwtAuthMiddleware`] is also stacked so that
/// each request carries an `Authorization: Bearer <jwt>` header signed by the
/// master keypair. This is used for bitrouter-cloud, which requires both
/// identity (JWT) and payment (x402).
pub fn build_x402_client(
    wallet_path: &Path,
    rpc_url: &str,
    base_client: reqwest::Client,
    with_jwt: bool,
) -> Result<ClientWithMiddleware> {
    let keypair = read_keypair_file(wallet_path).map_err(|e| {
        RuntimeError::X402(format!(
            "failed to load master wallet from {}: {e}",
            wallet_path.display(),
        ))
    })?;

    // Extract the 32-byte seed for MasterKeypair (first 32 bytes of the 64-byte keypair).
    let seed: [u8; 32] = keypair.to_bytes()[..32]
        .try_into()
        .map_err(|_| RuntimeError::X402("invalid keypair seed length".to_owned()))?;

    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_owned());
    let signer = SvmPaymentSigner::new(keypair, rpc);
    let x402_client = X402Client::new(signer);
    let x402_middleware = X402PaymentMiddleware::new(x402_client);

    let mut builder = reqwest_middleware::ClientBuilder::new(base_client);

    if with_jwt {
        let master_keypair = MasterKeypair::from_seed(seed);
        let jwt_middleware = JwtAuthMiddleware::new(master_keypair, Chain::solana_mainnet());
        builder = builder.with(jwt_middleware);
    }

    let client = builder.with(x402_middleware).build();

    Ok(client)
}
