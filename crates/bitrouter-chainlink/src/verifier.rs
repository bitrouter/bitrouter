//! [`ChainlinkVerifier`] — the Chainlink `ConfidentialVerifier`.
//!
//! Chainlink's dev-preview exposes **no** signed attestation (no Nitro document,
//! no signature, no nonce), only unsigned per-resource digests. So:
//! - L1 (`verify_attestation`) is always **fail-closed unverified** — there is
//!   nothing to verify. (The real Nitro path slots in here later.)
//! - L1.5 (`verify_exchange`) re-reads the service-reported digests and checks
//!   the one client-reproducible fact: `sha256(uploaded bytes) == reported
//!   digest`. `verified` stays `false` (unsigned); `digests_consistent` carries
//!   the honest sub-result.

use async_trait::async_trait;

use bitrouter_attestation::{
    AttestationVerdict, ConfidentialVerifier, ExchangeInput, IntegrityProof, VerifiedExchange,
    VerifyError, sha256_hex,
};

use crate::PROTOCOL_PROVIDER;
use crate::client::{ChainlinkClient, PollConfig};

/// The Chainlink confidential-inference verifier (shared by the CLI + pay gate).
pub struct ChainlinkVerifier {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl ChainlinkVerifier {
    /// Build a verifier bound to one Chainlink base URL + API key.
    pub fn new(base: String, key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            key,
        }
    }

    fn client(&self) -> ChainlinkClient {
        ChainlinkClient::new(
            self.http.clone(),
            self.base.clone(),
            self.key.clone(),
            PollConfig::default(),
        )
    }
}

#[async_trait]
impl ConfidentialVerifier for ChainlinkVerifier {
    fn provider(&self) -> &str {
        PROTOCOL_PROVIDER
    }

    async fn verify_attestation(
        &self,
        model: &str,
        nonce: &str,
        now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError> {
        // No quote/document is exposed by the dev-preview ⇒ fail-closed.
        Ok(AttestationVerdict::unverified(model, nonce, now_unix))
    }

    async fn verify_exchange(
        &self,
        ex: &ExchangeInput<'_>,
    ) -> Result<VerifiedExchange, VerifyError> {
        let request_hash = sha256_hex(ex.request_body);
        let response_hash = sha256_hex(ex.response_body);

        // Re-read what the service currently reports for this inference.
        let snapshot =
            self.client()
                .fetch(ex.chat_id)
                .await
                .map_err(|e| VerifyError::Transport {
                    what: "chainlink inference",
                    source: Box::new(e),
                })?;
        let r = snapshot.resources.first();

        let resource_digest = r.and_then(|r| r.digest.clone()).unwrap_or_default();
        // The one client-reproducible check: the original-content digest equals
        // sha256 of the bytes we uploaded (passed as request_body).
        let digests_consistent =
            !resource_digest.is_empty() && resource_digest.eq_ignore_ascii_case(&request_hash);

        let integrity = IntegrityProof::ChainlinkResourceDigests {
            inference_id: snapshot.id.clone(),
            request_digest: r.and_then(|r| r.request_digest.clone()).unwrap_or_default(),
            response_digest: r
                .and_then(|r| r.response_digest.clone())
                .unwrap_or_default(),
            resource_digest,
            filename_digest: r
                .and_then(|r| r.filename_digest.clone())
                .unwrap_or_default(),
            filename_blinding: r
                .and_then(|r| r.filename_blinding.clone())
                .unwrap_or_default(),
            digests_consistent,
        };

        Ok(VerifiedExchange {
            provider: self.provider().to_string(),
            model: ex.model.to_string(),
            request_hash,
            response_hash,
            attestation: AttestationVerdict::unverified(ex.model, "", ex.now_unix),
            integrity,
            // Unsigned: there is no enclave signature to make this true.
            verified: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_attestation::{ConfidentialVerifier, ExchangeInput, IntegrityProof};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn attestation_is_fail_closed_unverified() {
        let v = ChainlinkVerifier::new("https://example.invalid".into(), "k".into());
        let verdict = v
            .verify_attestation("gemma4", "nonce", 1_000)
            .await
            .unwrap();
        assert!(!verdict.verified);
        assert!(verdict.attested_addresses.is_empty());
    }

    #[tokio::test]
    async fn exchange_marks_digests_consistent_when_resource_digest_matches() {
        let expected = bitrouter_attestation::sha256_hex(b"hi");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "job-1", "status": "completed", "output": "out",
                "resources": [{ "digest": expected, "request_digest": "rq",
                    "response_digest": "rs", "filename_digest": "fd",
                    "filename_blinding": "bb" }]
            })))
            .mount(&server)
            .await;
        let v = ChainlinkVerifier::new(server.uri(), "k".into());
        let ex = ExchangeInput {
            model: "gemma4",
            request_body: b"hi",
            response_body: b"out",
            chat_id: "job-1",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();
        assert!(!out.verified, "unsigned digests never verify");
        match out.integrity {
            IntegrityProof::ChainlinkResourceDigests {
                digests_consistent,
                inference_id,
                ..
            } => {
                assert!(digests_consistent);
                assert_eq!(inference_id, "job-1");
            }
            other => panic!("expected ChainlinkResourceDigests, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exchange_flags_inconsistent_when_digest_differs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "job-2", "status": "completed", "output": "out",
                "resources": [{ "digest": "deadbeef" }]
            })))
            .mount(&server)
            .await;
        let v = ChainlinkVerifier::new(server.uri(), "k".into());
        let ex = ExchangeInput {
            model: "gemma4",
            request_body: b"hi",
            response_body: b"out",
            chat_id: "job-2",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();
        match out.integrity {
            IntegrityProof::ChainlinkResourceDigests {
                digests_consistent, ..
            } => {
                assert!(!digests_consistent)
            }
            other => panic!("expected ChainlinkResourceDigests, got {other:?}"),
        }
    }
}
