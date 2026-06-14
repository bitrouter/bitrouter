//! Arc testnet MPP backend — mirrors the Tempo `TempoProvider` wiring from the
//! v0 payment middleware, substituting Arc chain config.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::PayError;
use crate::chain::arc::{ARC_TESTNET_CAIP2, ARC_TESTNET_RPC, ARC_TESTNET_USDC};
use crate::wallet::{ArcLocalSigner, ArcSigner};

/// MPP payment backend trait (matches `mpp_br::client::PaymentProvider` surface).
#[async_trait]
pub trait MppBackend: Send + Sync {
    fn supports(&self, method: &str, intent: &str) -> bool;
    async fn pay(&self, www_authenticate: &str) -> Result<String, PayError>;
}

/// Arc testnet MPP charge backend.
///
/// Wallet identity (and the x402 EIP-712 path) comes from [`ArcSigner`] (OWS
/// CLI). On-chain MPP settlement signs a bare transaction hash, which the OWS
/// CLI cannot do, so it is signed by an [`ArcLocalSigner`] loaded lazily from
/// the same vault and verified to match the OWS wallet address.
pub struct ArcMppBackend {
    signer: Arc<ArcSigner>,
    local: OnceCell<Arc<ArcLocalSigner>>,
    usdc: String,
    rpc_url: String,
    caip2: String,
}

impl ArcMppBackend {
    pub fn new(signer: Arc<ArcSigner>) -> Self {
        Self {
            signer,
            local: OnceCell::new(),
            usdc: ARC_TESTNET_USDC.to_string(),
            rpc_url: ARC_TESTNET_RPC.to_string(),
            caip2: ARC_TESTNET_CAIP2.to_string(),
        }
    }

    /// Lazily load and cache the raw-key signer used for on-chain settlement,
    /// asserting that the decrypted key matches the OWS wallet identity.
    async fn local_signer(&self) -> Result<Arc<ArcLocalSigner>, PayError> {
        let expected = self.signer.address();
        self.local
            .get_or_try_init(|| async move {
                let local = tokio::task::spawn_blocking(ArcLocalSigner::agent_treasury)
                    .await
                    .map_err(|e| {
                        PayError::SignerError(format!("local signer load task failed: {e}"))
                    })??;
                if local.address() != expected {
                    return Err(PayError::SignerError(format!(
                        "decrypted MPP key {} does not match OWS wallet {expected}",
                        local.address()
                    )));
                }
                Ok(Arc::new(local))
            })
            .await
            .cloned()
    }

    #[cfg(feature = "mpp")]
    fn tempo_provider(
        &self,
        local: ArcLocalSigner,
    ) -> Result<mpp_br::client::TempoProvider, PayError> {
        mpp_br::client::TempoProvider::new(local, &self.rpc_url)
            .map_err(|e| PayError::PaymentFailed(e.to_string()))
    }
}

#[async_trait]
impl MppBackend for ArcMppBackend {
    fn supports(&self, method: &str, intent: &str) -> bool {
        let _ = (&self.usdc, &self.caip2);
        method == "tempo" && intent == "charge"
    }

    async fn pay(&self, www_authenticate: &str) -> Result<String, PayError> {
        #[cfg(feature = "mpp")]
        {
            use mpp_br::client::PaymentProvider;

            let challenge = mpp_br::parse_www_authenticate(www_authenticate).map_err(|e| {
                PayError::InvalidChallenge(format!("invalid WWW-Authenticate: {e}"))
            })?;

            let local = self.local_signer().await?;
            let provider = self.tempo_provider((*local).clone())?;
            let credential = PaymentProvider::pay(&provider, &challenge)
                .await
                .map_err(|e| PayError::PaymentFailed(e.to_string()))?;

            // Diagnostics for the "-32602 failed to decode signed transaction"
            // class of RPC rejection: surface the exact signed payload the
            // server will broadcast. mpp-br's `tempo` method does NOT emit a
            // standard EIP-155 / EIP-1559 RLP transaction — it emits a Tempo
            // account-abstraction typed envelope (leading byte `0x76`, or
            // `0x78` in fee-payer mode) wrapping ITIP20 precompile calls. That
            // envelope is only decodable by a Tempo node (chain 4217 / 42431);
            // a generic EVM chain such as Arc testnet (5042002) rejects it.
            if let Ok(payload) = credential.charge_payload() {
                let data = payload.data();
                let kind = if payload.is_transaction() {
                    "transaction"
                } else if payload.is_proof() {
                    "proof (zero-amount, no broadcast)"
                } else {
                    "other"
                };
                let type_byte = data
                    .strip_prefix("0x")
                    .and_then(|h| h.get(0..2))
                    .unwrap_or("");
                tracing::warn!(
                    payload_kind = kind,
                    tx_type_byte = %type_byte,
                    tx_len = data.len(),
                    signed_payload = %data,
                    "MPP tempo signed payload built — type byte 0x76/0x78 is a Tempo AA \
                     envelope (valid only on Tempo chains 4217/42431, NOT Arc 5042002)"
                );
            }

            mpp_br::format_authorization(&credential)
                .map_err(|e| PayError::PaymentFailed(e.to_string()))
        }
        #[cfg(not(feature = "mpp"))]
        {
            let _ = www_authenticate;
            Err(PayError::PaymentFailed(
                "MPP support not compiled (enable the `mpp` feature)".into(),
            ))
        }
    }
}

/// POST helper that pays via MPP when the upstream returns 402.
pub struct MppClient {
    backend: Arc<dyn MppBackend>,
    http: reqwest::Client,
}

impl MppClient {
    pub fn new(backend: Arc<dyn MppBackend>) -> Self {
        Self {
            backend,
            http: reqwest::Client::new(),
        }
    }

    pub async fn post(
        &self,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, PayError> {
        let first = self.send(url, body.clone(), None).await?;
        if first.status().as_u16() != 402 {
            return parse_json(first).await;
        }

        let www_auth = first
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .ok_or_else(|| PayError::InvalidChallenge("missing WWW-Authenticate header".into()))?
            .to_str()
            .map_err(|e| PayError::InvalidChallenge(e.to_string()))?;

        let authorization = self.backend.pay(www_auth).await?;
        let paid = self.send(url, body, Some(authorization)).await?;
        if !paid.status().is_success() {
            let status = paid.status();
            let text = paid.text().await.unwrap_or_default();
            return Err(PayError::PaymentFailed(format!(
                "MPP retry returned {status}: {text}"
            )));
        }
        parse_json(paid).await
    }

    async fn send(
        &self,
        url: &str,
        body: Option<serde_json::Value>,
        authorization: Option<String>,
    ) -> Result<reqwest::Response, PayError> {
        let mut req = self.http.post(url);
        if let Some(json) = body {
            req = req.json(&json);
        }
        if let Some(auth) = authorization {
            req = req.header(mpp_br::AUTHORIZATION_HEADER, auth);
        }
        req.send()
            .await
            .map_err(|e| PayError::UpstreamError(e.to_string()))
    }
}

async fn parse_json(resp: reqwest::Response) -> Result<serde_json::Value, PayError> {
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(PayError::UpstreamError(format!(
            "upstream returned {status}: {text}"
        )));
    }
    resp.json()
        .await
        .map_err(|e| PayError::UpstreamError(e.to_string()))
}
