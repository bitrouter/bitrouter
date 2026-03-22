//! Solana MPP session client builder.
//!
//! Constructs a [`ClientWithMiddleware`] that automatically handles HTTP 402
//! responses from upstream providers using Solana session payments.
//! When an upstream returns 402 with a `WWW-Authenticate` challenge, the
//! middleware signs an Ed25519 voucher incrementing the session's cumulative
//! amount and retries with a credential.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitrouter_core::auth::keys::MasterKeypair;
use http::Extensions;
use mpp::client::PaymentProvider;
use mpp::protocol::core::{AUTHORIZATION_HEADER, format_authorization, parse_www_authenticate};
use reqwest::{Request, Response, StatusCode, header::WWW_AUTHENTICATE};
use reqwest_middleware::{ClientWithMiddleware, Middleware, Next};

use bitrouter_api::mpp::solana_types::{
    SignatureType, SignedSolanaSessionVoucher, SolanaSessionCredentialPayload, SolanaSessionVoucher,
};
use bitrouter_api::mpp::solana_voucher::serialize_voucher;

use crate::runtime::error::Result;

/// Client-side Solana session provider.
///
/// Implements `PaymentProvider` for the "solana/session" method, providing
/// automatic voucher signing with channel state tracking. On the first 402
/// the provider opens a channel; on subsequent 402s it signs update vouchers
/// with incrementing sequence numbers and cumulative amounts.
#[derive(Clone)]
pub struct SolanaSessionProvider {
    /// Raw seed for Ed25519 signing.
    seed: [u8; 32],
    /// Base58-encoded public key of the signer.
    pubkey: String,
    /// Per-realm channel state: key = `recipient:channelProgram`.
    channels: Arc<Mutex<HashMap<String, ChannelEntry>>>,
}

/// Tracks client-side channel state for an active session.
#[derive(Clone, Debug)]
struct ChannelEntry {
    channel_id: String,
    channel_program: String,
    chain_id: String,
    recipient: String,
    server_nonce: String,
    sequence: u64,
    cumulative_amount: u128,
    deposit: u128,
}

impl SolanaSessionProvider {
    pub fn new(keypair: &MasterKeypair) -> Self {
        Self {
            seed: *keypair.seed(),
            pubkey: keypair.solana_pubkey_b58(),
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Sign a message with Ed25519 using the stored seed.
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        let kp = MasterKeypair::from_seed(self.seed);
        kp.sign_ed25519(message)
    }

    /// Construct a channel key from challenge details.
    fn channel_key(recipient: &str, channel_program: &str) -> String {
        format!("{recipient}:{channel_program}")
    }

    /// Sign a voucher, returning a `SignedSolanaSessionVoucher`.
    fn sign_voucher(&self, voucher: SolanaSessionVoucher) -> SignedSolanaSessionVoucher {
        let message = serialize_voucher(&voucher);
        let sig_bytes = self.sign(&message);
        SignedSolanaSessionVoucher {
            signature: bs58::encode(&sig_bytes).into_string(),
            signature_type: SignatureType::Ed25519,
            signer: self.pubkey.clone(),
            voucher,
        }
    }

    /// Build a credential payload for a session request.
    fn build_credential(
        &self,
        challenge: &mpp::PaymentChallenge,
    ) -> std::result::Result<serde_json::Value, mpp::MppError> {
        let request: mpp::SessionRequest = challenge.request.decode().map_err(|e| {
            mpp::MppError::invalid_payload(format!("failed to decode session request: {e}"))
        })?;

        let recipient = request.recipient.as_deref().unwrap_or("");
        let method_details = request
            .method_details
            .as_ref()
            .ok_or_else(|| mpp::MppError::invalid_payload("missing methodDetails in challenge"))?;
        let channel_program = method_details
            .get("channelProgram")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let network = method_details
            .get("network")
            .and_then(|v| v.as_str())
            .unwrap_or("mainnet-beta");

        let chain_id = if network.starts_with("solana:") {
            network.to_string()
        } else {
            format!("solana:{network}")
        };

        let key = Self::channel_key(recipient, channel_program);

        // Parse the requested amount.
        let amount: u128 = request.amount.parse().unwrap_or(1);

        let channels = self
            .channels
            .lock()
            .map_err(|e| mpp::MppError::InvalidConfig(format!("channel lock poisoned: {e}")))?;

        if let Some(entry) = channels.get(&key) {
            // Existing channel → update voucher.
            let new_cumulative = entry.cumulative_amount + amount;
            let new_seq = entry.sequence + 1;

            if new_cumulative > entry.deposit {
                return Err(mpp::MppError::invalid_payload(
                    "cumulative amount would exceed channel deposit",
                ));
            }

            let voucher = SolanaSessionVoucher {
                chain_id: entry.chain_id.clone(),
                channel_id: entry.channel_id.clone(),
                channel_program: entry.channel_program.clone(),
                cumulative_amount: new_cumulative.to_string(),
                expires_at: None,
                meter: "token".into(),
                payer: self.pubkey.clone(),
                recipient: entry.recipient.clone(),
                sequence: new_seq,
                server_nonce: entry.server_nonce.clone(),
                units: amount.to_string(),
            };

            let signed = self.sign_voucher(voucher);
            let payload = SolanaSessionCredentialPayload::Update {
                channel_id: entry.channel_id.clone(),
                voucher: signed,
            };

            drop(channels);

            // Update tracked state.
            let mut channels = self
                .channels
                .lock()
                .map_err(|e| mpp::MppError::InvalidConfig(format!("channel lock poisoned: {e}")))?;
            if let Some(entry) = channels.get_mut(&key) {
                entry.sequence = new_seq;
                entry.cumulative_amount = new_cumulative;
            }

            serde_json::to_value(payload)
                .map_err(|e| mpp::MppError::InvalidConfig(format!("serialize payload: {e}")))
        } else {
            drop(channels);

            // No channel → open one.
            let deposit = request
                .suggested_deposit
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(amount * 100);

            let channel_id = format!(
                "{:x}{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                uuid::Uuid::new_v4().as_u64_pair().0
            );
            let server_nonce = uuid::Uuid::new_v4().to_string();

            let voucher = SolanaSessionVoucher {
                chain_id: chain_id.clone(),
                channel_id: channel_id.clone(),
                channel_program: channel_program.to_string(),
                cumulative_amount: amount.to_string(),
                expires_at: None,
                meter: "token".into(),
                payer: self.pubkey.clone(),
                recipient: recipient.to_string(),
                sequence: 1,
                server_nonce: server_nonce.clone(),
                units: amount.to_string(),
            };

            let signed = self.sign_voucher(voucher);
            let payload = SolanaSessionCredentialPayload::Open {
                channel_id: channel_id.clone(),
                open_tx: "client-managed".into(),
                voucher: signed,
                payer: self.pubkey.clone(),
                deposit_amount: deposit.to_string(),
                authorization_mode:
                    bitrouter_api::mpp::solana_types::AuthorizationMode::SwigSession,
                capabilities: None,
                expires_at: None,
            };

            // Store channel state.
            let mut channels = self
                .channels
                .lock()
                .map_err(|e| mpp::MppError::InvalidConfig(format!("channel lock poisoned: {e}")))?;
            channels.insert(
                key,
                ChannelEntry {
                    channel_id,
                    channel_program: channel_program.to_string(),
                    chain_id,
                    recipient: recipient.to_string(),
                    server_nonce,
                    sequence: 1,
                    cumulative_amount: amount,
                    deposit,
                },
            );

            serde_json::to_value(payload)
                .map_err(|e| mpp::MppError::InvalidConfig(format!("serialize payload: {e}")))
        }
    }
}

impl PaymentProvider for SolanaSessionProvider {
    fn supports(&self, method: &str, intent: &str) -> bool {
        method == "solana" && intent == "session"
    }

    async fn pay(
        &self,
        challenge: &mpp::PaymentChallenge,
    ) -> std::result::Result<mpp::PaymentCredential, mpp::MppError> {
        let payload = self.build_credential(challenge)?;

        Ok(mpp::PaymentCredential {
            challenge: challenge.to_echo(),
            source: None,
            payload,
        })
    }
}

/// Reqwest 0.5 middleware that intercepts 402 responses and pays via Solana session.
struct MppSolanaPaymentMiddleware<P> {
    provider: P,
}

impl<P> MppSolanaPaymentMiddleware<P> {
    fn new(provider: P) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<P> Middleware for MppSolanaPaymentMiddleware<P>
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

/// Build a Solana session MPP-capable HTTP client.
pub fn build_mpp_solana_client(
    keypair: &MasterKeypair,
    base_client: reqwest::Client,
) -> Result<ClientWithMiddleware> {
    let provider = SolanaSessionProvider::new(keypair);
    let middleware = MppSolanaPaymentMiddleware::new(provider);
    let client = reqwest_middleware::ClientBuilder::new(base_client)
        .with(middleware)
        .build();
    Ok(client)
}
