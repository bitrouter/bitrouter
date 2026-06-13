//! Fetching the signed attestation report (and, in P2, the per-chat signature)
//! from a provider endpoint.
//!
//! Every fetch goes through the **untrusted** cloud, which can withhold it, so
//! callers treat any transport failure as fail-closed (spec §1.5 cond. 3). The
//! [`ReportTransport`] trait keeps the network at the edge so the verifier's
//! crypto is exercised offline against fixtures via [`MockTransport`].

use async_trait::async_trait;

use crate::VerifyError;
use crate::near::report::AttestationReport;
use crate::near::signature::ChatSignature;

/// The signing algorithm bitrouter requests and verifies (spec Decision 7):
/// secp256k1 ECDSA with EIP-191 recovery, matching NEAR's published vector.
pub const SIGNING_ALGO: &str = "ecdsa";

/// Fetches confidential-inference evidence for a provider. Mockable so the
/// verifier's crypto runs offline in CI.
#[async_trait]
pub trait ReportTransport: Send + Sync {
    /// `GET {base}/v1/attestation/report?model={model}&signing_algo=ecdsa&nonce={nonce}`.
    async fn fetch_report(
        &self,
        model: &str,
        nonce: &str,
    ) -> Result<AttestationReport, VerifyError>;

    /// POST a model's `nvidia_payload` to NRAS and return the raw EAT response
    /// body. The daemon calls NVIDIA **directly** (Decision 4); the default impl
    /// does exactly that via [`crate::post_nras`], independent of the report
    /// base URL, so most transports inherit the correct behavior.
    async fn fetch_gpu_eat(&self, nvidia_payload: &str) -> Result<Vec<u8>, VerifyError> {
        crate::post_nras(&reqwest::Client::new(), crate::NRAS_GPU_URL, nvidia_payload).await
    }

    /// `GET {base}/v1/signature/{chat_id}?model={model}&signing_algo=ecdsa` —
    /// the per-chat signature (L1.5). The default errors; transports that can
    /// reach a signature endpoint (e.g. [`ReqwestTransport`]) override it.
    async fn fetch_signature(
        &self,
        _chat_id: &str,
        _model: &str,
    ) -> Result<ChatSignature, VerifyError> {
        Err(VerifyError::Malformed {
            what: "chat signature",
            detail: "this transport does not support signature fetch".to_string(),
        })
    }
}

/// Live `reqwest` transport pointed at a provider base URL (e.g. the cloud's
/// `/v1/aci` passthrough, or `https://cloud-api.near.ai/v1`).
pub struct ReqwestTransport {
    base_url: String,
    http: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ReportTransport for ReqwestTransport {
    async fn fetch_report(
        &self,
        model: &str,
        nonce: &str,
    ) -> Result<AttestationReport, VerifyError> {
        let url = format!("{}/attestation/report", self.base_url);
        let resp = self
            .http
            .get(url)
            .query(&[
                ("model", model),
                ("signing_algo", SIGNING_ALGO),
                ("nonce", nonce),
            ])
            .send()
            .await
            .map_err(|e| VerifyError::Transport {
                what: "attestation report",
                source: Box::new(e),
            })?
            .error_for_status()
            .map_err(|e| VerifyError::Transport {
                what: "attestation report",
                source: Box::new(e),
            })?;
        resp.json::<AttestationReport>()
            .await
            .map_err(|e| VerifyError::Malformed {
                what: "attestation report",
                detail: e.to_string(),
            })
    }

    async fn fetch_signature(
        &self,
        chat_id: &str,
        model: &str,
    ) -> Result<ChatSignature, VerifyError> {
        let url = format!("{}/signature/{chat_id}", self.base_url);
        let resp = self
            .http
            .get(url)
            .query(&[("model", model), ("signing_algo", SIGNING_ALGO)])
            .send()
            .await
            .map_err(|e| VerifyError::Transport {
                what: "chat signature",
                source: Box::new(e),
            })?
            .error_for_status()
            .map_err(|e| VerifyError::Transport {
                what: "chat signature",
                source: Box::new(e),
            })?;
        resp.json::<ChatSignature>()
            .await
            .map_err(|e| VerifyError::Malformed {
                what: "chat signature",
                detail: e.to_string(),
            })
    }
}

/// In-memory transport that replays a canned report — the verifier's offline
/// test seam. Public so the plugin/daemon can reuse it in their own tests.
#[derive(Debug, Clone)]
pub struct MockTransport {
    report: AttestationReport,
}

impl MockTransport {
    pub fn new(report: AttestationReport) -> Self {
        Self { report }
    }

    /// Build a mock from raw report JSON (e.g. the bundled golden fixture).
    pub fn from_report_json(bytes: &[u8]) -> Result<Self, VerifyError> {
        let report = serde_json::from_slice::<AttestationReport>(bytes).map_err(|e| {
            VerifyError::Malformed {
                what: "attestation report fixture",
                detail: e.to_string(),
            }
        })?;
        Ok(Self::new(report))
    }
}

#[async_trait]
impl ReportTransport for MockTransport {
    async fn fetch_report(
        &self,
        _model: &str,
        _nonce: &str,
    ) -> Result<AttestationReport, VerifyError> {
        Ok(self.report.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/near_report.json");

    #[tokio::test]
    async fn mock_transport_replays_the_fixture_report() {
        let mock = MockTransport::from_report_json(FIXTURE).expect("fixture parses");
        let report = mock
            .fetch_report("zai-org/GLM-5.1-FP8", "any-nonce")
            .await
            .expect("mock never fails");
        assert_eq!(
            report.model_attestations[0].signing_address,
            "0xbb4d2e7ffe98eefcd9690e2139be41e92b95e333"
        );
    }

    #[tokio::test]
    async fn from_report_json_rejects_garbage() {
        let err = MockTransport::from_report_json(b"not json").unwrap_err();
        assert!(matches!(err, VerifyError::Malformed { .. }));
    }
}
