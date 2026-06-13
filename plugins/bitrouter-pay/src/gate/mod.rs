//! [`ArcPaymentGate`] — composes OWS signing, x402, MPP, and Chainlink attestation.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::{PaymentGate, PaymentGateResult, PaymentRouteRequest};
use serde_json::Value;
use tracing::info;

use crate::attester::{ChainlinkAttester, Resource};
#[cfg(feature = "mpp")]
use crate::payment::mpp::{ArcMppBackend, MppBackend, MppClient};
#[cfg(feature = "x402")]
use crate::payment::x402::X402Client;
use crate::wallet::ArcSigner;
use crate::PayError;

/// Configuration for [`ArcPaymentGate`].
pub struct ArcPaymentGateConfig {
    pub wallet_id: String,
    pub chainlink_api_key: Option<String>,
}

/// Payment gate for Arc testnet Proceeds paywalls.
pub struct ArcPaymentGate {
    signer: Arc<ArcSigner>,
    #[cfg(feature = "x402")]
    x402: X402Client,
    #[cfg(feature = "mpp")]
    mpp: MppClient,
    attester: Option<ChainlinkAttester>,
}

impl ArcPaymentGate {
    pub fn new(config: ArcPaymentGateConfig) -> Result<Self, PayError> {
        let signer = Arc::new(ArcSigner::new(config.wallet_id)?);
        let attester = config
            .chainlink_api_key
            .map(ChainlinkAttester::new);

        Ok(Self {
            #[cfg(feature = "x402")]
            x402: X402Client::new(signer.clone()),
            #[cfg(feature = "mpp")]
            mpp: MppClient::new(Arc::new(ArcMppBackend::new(signer.clone())) as Arc<dyn MppBackend>),
            attester,
            signer,
        })
    }

    pub fn signer(&self) -> &ArcSigner {
        &self.signer
    }

    async fn pay_internal(&self, request: PaymentRouteRequest) -> Result<Value, PayError> {
        let body = match (request.mpp, ()) {
            #[cfg(feature = "mpp")]
            (true, ()) => self.mpp.post(&request.url, request.body).await?,
            #[cfg(not(feature = "mpp"))]
            (true, ()) => {
                return Err(PayError::PaymentFailed(
                    "MPP support not compiled (enable the `mpp` feature)".into(),
                ));
            }
            #[cfg(feature = "x402")]
            (false, ()) => self.x402.post(&request.url, request.body).await?,
            #[cfg(not(feature = "x402"))]
            (false, ()) => {
                return Err(PayError::PaymentFailed(
                    "x402 support not compiled (enable the `x402` feature)".into(),
                ));
            }
        };

        if request.attested {
            let attester = self.attester.as_ref().ok_or_else(|| {
                PayError::AttestError("attested route requires chainlink_api_key".into())
            })?;
            let model = request.model.ok_or_else(|| {
                PayError::AttestError("attested route requires model".into())
            })?;
            let prompt = request.prompt.unwrap_or_default();
            let resources = vec![Resource::from_bytes(
                "payload.json",
                "text/plain",
                body.to_string().as_bytes(),
            )];
            let receipt = attester.infer(&model, &prompt, resources).await?;
            record_ledger(&request.url, &receipt.inference_id);
            return serde_json::to_value(receipt).map_err(|e| PayError::AttestError(e.to_string()));
        }

        record_ledger(&request.url, "x402-or-mpp");
        Ok(body)
    }
}

#[async_trait]
impl PaymentGate for ArcPaymentGate {
    async fn pay(&self, request: PaymentRouteRequest) -> Result<PaymentGateResult, String> {
        self.pay_internal(request)
            .await
            .map(|body| PaymentGateResult { body })
            .map_err(|e| e.to_string())
    }
}

fn record_ledger(url: &str, reference: &str) {
    #[cfg(feature = "observe-ledger")]
    {
        let _ = bitrouter_observe::OTEL_ENABLED;
        info!(
            target: "bitrouter_pay.ledger",
            url = url,
            reference = reference,
            "payment ledger row"
        );
    }
    #[cfg(not(feature = "observe-ledger"))]
    {
        info!(
            target: "bitrouter_pay.ledger",
            url = url,
            reference = reference,
            "payment ledger row"
        );
    }
}
