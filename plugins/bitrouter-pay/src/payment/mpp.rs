//! Arc testnet MPP backend — mirrors the Tempo `TempoProvider` wiring from the
//! v0 payment middleware, substituting Arc chain config.

use std::sync::Arc;

use async_trait::async_trait;

use crate::PayError;
use crate::chain::arc::{ARC_TESTNET_CAIP2, ARC_TESTNET_RPC, ARC_TESTNET_USDC};
use crate::wallet::ArcSigner;

/// MPP payment backend trait (matches `mpp_br::client::PaymentProvider` surface).
#[async_trait]
pub trait MppBackend: Send + Sync {
    fn supports(&self, method: &str, intent: &str) -> bool;
    async fn pay(&self, www_authenticate: &str) -> Result<String, PayError>;
}

/// Arc testnet MPP charge backend using an OWS-backed alloy signer.
pub struct ArcMppBackend {
    signer: Arc<ArcSigner>,
    usdc: String,
    rpc_url: String,
    caip2: String,
}

impl ArcMppBackend {
    pub fn new(signer: Arc<ArcSigner>) -> Self {
        Self {
            signer,
            usdc: ARC_TESTNET_USDC.to_string(),
            rpc_url: ARC_TESTNET_RPC.to_string(),
            caip2: ARC_TESTNET_CAIP2.to_string(),
        }
    }

    #[cfg(feature = "mpp")]
    fn tempo_provider(&self) -> Result<mpp_br::client::TempoProvider, PayError> {
        mpp_br::client::TempoProvider::new(self.signer.as_ref().clone(), &self.rpc_url)
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

            let provider = self.tempo_provider()?;
            let credential = PaymentProvider::pay(&provider, &challenge)
                .await
                .map_err(|e| PayError::PaymentFailed(e.to_string()))?;

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
