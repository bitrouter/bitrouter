//! NEAR AI Cloud confidential-inference verifier — the first
//! [`ConfidentialVerifier`](crate::ConfidentialVerifier) impl.
//!
//! Verification is split across focused modules: `report` (wire types),
//! `binding` (report_data + compose), `tdx` (DCAP quote), `nvidia` (NRAS GPU
//! EAT), `dcap` (the load-bearing policy pin), and `signature` (L1.5, Phase 2).
//! [`NearVerifier`] composes them into one attestation verdict.

pub mod binding;
pub mod dcap;
pub mod eventlog;
pub mod nvidia;
pub mod report;
pub mod signature;
pub mod tdx;

use std::sync::Arc;

use async_trait::async_trait;

use crate::cache::AttestationCache;
use crate::near::binding::{compose_matches_mr_config, report_data_binds};
use crate::near::dcap::{AciDcapVerifierPolicy, model_identity};
use crate::near::eventlog::event_log_binds_info;
use crate::near::nvidia::{NvidiaEatKey, check_nras_eat};
use crate::near::report::ModelAttestation;
use crate::near::signature::{chat_signing_text, recover_eip191_address, sha256_hex};
use crate::near::tdx::QuoteVerifier;
use crate::transport::ReportTransport;
use crate::types::{
    AttestationChecks, AttestationVerdict, ExchangeInput, IntegrityProof, VerifiedExchange,
};
use crate::{ConfidentialVerifier, VerifyError};

/// Honest trust-boundary label: we verify NEAR's **model** quote directly, not
/// (as `nearai.py` does) trust NEAR's gateway.
pub const TRUST_BOUNDARY: &str = "near-ai-model";
/// Default verdict cache TTL (spec §8).
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 600;
/// Cap for caching an **unverified** verdict. Failures (e.g. a transient NRAS
/// outage) are still cached so we don't hammer NRAS per request, but only
/// briefly so verification recovers quickly rather than denying a model for a
/// full TTL (review finding #4).
pub const UNVERIFIED_CACHE_TTL_SECONDS: u64 = 60;

/// The NEAR [`ConfidentialVerifier`]. Composes the report fetch, GPU NRAS check,
/// DCAP quote verification, report_data/compose bindings, and the load-bearing
/// policy pin into a single [`AttestationVerdict`], **fail-closed** at every
/// step (spec §1.5 cond. 3) and TTL-cached.
pub struct NearVerifier {
    transport: Arc<dyn ReportTransport>,
    quotes: Arc<dyn QuoteVerifier>,
    policy: Arc<AciDcapVerifierPolicy>,
    /// NVIDIA's EAT-verification key (pinned/configured by the host).
    nvidia_key: Arc<NvidiaEatKey>,
    cache: AttestationCache,
    cache_ttl_seconds: u64,
}

impl NearVerifier {
    pub fn new(
        transport: Arc<dyn ReportTransport>,
        quotes: Arc<dyn QuoteVerifier>,
        policy: Arc<AciDcapVerifierPolicy>,
        nvidia_key: Arc<NvidiaEatKey>,
    ) -> Self {
        Self {
            transport,
            quotes,
            policy,
            nvidia_key,
            cache: AttestationCache::new(),
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        }
    }

    pub fn with_cache_ttl(mut self, ttl_seconds: u64) -> Self {
        self.cache_ttl_seconds = ttl_seconds;
        self
    }

    /// Serve a TTL-cached verdict, re-verifying on miss with a fresh internal
    /// nonce — the plugin/daemon hot-path entrypoint (spec §5.1). Caches the
    /// verdict it computes (verified or not) so a flaky NRAS/PCCS isn't hit per
    /// request; a transient failure simply re-verifies after the TTL.
    pub async fn verdict_cached(
        &self,
        model: &str,
        now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError> {
        if let Some(cached) = self.cache.get(model, now_unix) {
            return Ok(cached);
        }
        let nonce = crate::fresh_nonce_hex();
        let verdict = self.verify_attestation(model, &nonce, now_unix).await?;
        // Cache a confirmed verdict for the full TTL; cap an unverified one to a
        // short retry window so a transient failure recovers quickly.
        let ttl = if verdict.verified {
            self.cache_ttl_seconds
        } else {
            self.cache_ttl_seconds.min(UNVERIFIED_CACHE_TTL_SECONDS)
        };
        self.cache.put(verdict.clone(), ttl, now_unix);
        Ok(verdict)
    }

    /// Evaluate one serving node's attestation into a per-check breakdown.
    /// Every sub-check fails closed: missing evidence ⇒ `false`.
    async fn evaluate_node(
        &self,
        m: &ModelAttestation,
        nonce: &str,
        now_unix: u64,
    ) -> AttestationChecks {
        let Ok(raw_quote) = hex::decode(&m.intel_quote) else {
            return AttestationChecks::failed();
        };

        let gpu_nras_pass = match self.transport.fetch_gpu_eat(&m.nvidia_payload).await {
            Ok(eat) => check_nras_eat(&eat, nonce, &self.nvidia_key).passed(),
            Err(_) => false,
        };

        let measurements = self.quotes.measurements(&raw_quote, now_unix).await.ok();
        let dcap_quote_valid = measurements.is_some() && self.quotes.is_authenticated();

        // report_data must bind our key+nonce AND the report must echo our nonce
        // (so a stale report for a different nonce can't pass).
        let nonce_echoed = m.request_nonce.eq_ignore_ascii_case(nonce);
        let report_data_binds_key_and_nonce = nonce_echoed
            && measurements
                .as_ref()
                .is_some_and(|mm| report_data_binds(&mm.report_data, &m.signing_address, nonce));

        let debug_disabled = measurements
            .as_ref()
            .is_some_and(super::near::tdx::TdxMeasurements::debug_disabled);

        // Anchor the cloud-supplied `info` fields to the genuine quote by
        // replaying the event log into RTMR3 and binding its payloads. Only when
        // this holds are `compose`/`policy` below checking TEE-measured facts
        // rather than cloud assertions (spec §1.5 cond. 1). `None` when the quote
        // never parsed (we can't anchor without its RTMR3).
        let event_log_rtmr_ok = measurements
            .as_ref()
            .map(|mm| event_log_binds_info(&m.event_log, &mm.rtmr3, &m.info));

        let compose_matches_mr_config =
            compose_matches_mr_config(&m.info.tcb_info.app_compose, &m.info.compose_hash);

        let policy_accepts = match model_identity(&m.info) {
            Ok(id) => {
                self.policy.accepts(&id.workload_id, &id.image_digests)
                    && self.policy.accepts_kms_root(&id.kms_root_public_key)
            }
            Err(_) => false,
        };

        AttestationChecks {
            gpu_nras_pass,
            dcap_quote_valid,
            report_data_binds_key_and_nonce,
            compose_matches_mr_config,
            policy_accepts,
            debug_disabled,
            event_log_rtmr_ok,
            tcb_status: None,
        }
    }
}

#[async_trait]
impl ConfidentialVerifier for NearVerifier {
    fn provider(&self) -> &str {
        "near-ai"
    }

    /// Serve a TTL-cached verdict (the hot path) instead of the trait default's
    /// fresh-nonce verify.
    async fn attestation_cached(
        &self,
        model: &str,
        now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError> {
        self.verdict_cached(model, now_unix).await
    }

    async fn verify_attestation(
        &self,
        model: &str,
        nonce: &str,
        now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError> {
        // Fail-closed: a withheld report ⇒ unverified, never a silent pass.
        let Ok(report) = self.transport.fetch_report(model, nonce).await else {
            return Ok(AttestationVerdict::unverified(model, nonce, now_unix));
        };

        // The report may carry several serving nodes (multi-node; signature
        // cached per node). A node is attested only if ALL its checks pass; the
        // verdict collects every fully-passing node's signing address so L1.5
        // can bind a chat signature to any of them (spec §1.5 cond. 2).
        let mut attested_addresses = Vec::new();
        let mut summary = AttestationChecks::failed();
        let mut saw_node = false;
        for m in report
            .model_attestations
            .iter()
            .filter(|m| m.model_name == model)
        {
            let checks = self.evaluate_node(m, nonce, now_unix).await;
            if !saw_node {
                summary = checks.clone();
                saw_node = true;
            }
            if checks.all_pass() {
                summary = checks;
                attested_addresses.push(m.signing_address.clone());
            }
        }

        Ok(AttestationVerdict {
            model: model.to_string(),
            verified: !attested_addresses.is_empty(),
            attested_addresses,
            trust_boundary: TRUST_BOUNDARY.to_string(),
            nonce: nonce.to_string(),
            checks: summary,
            verified_at_unix: now_unix,
        })
    }

    async fn verify_exchange(
        &self,
        ex: &ExchangeInput<'_>,
    ) -> Result<VerifiedExchange, VerifyError> {
        let request_hash = sha256_hex(ex.request_body);
        let response_hash = sha256_hex(ex.response_body);
        let expected_text = chat_signing_text(ex.model, &request_hash, &response_hash);

        // L1 first: the attested (policy-accepted) signing-address set this
        // signature must recover into (spec §1.5 cond. 2).
        let attestation = self.attestation_cached(ex.model, ex.now_unix).await?;

        // Fail-closed: a withheld/invalid signature ⇒ unverified, not an error.
        let Ok(sig) = self.transport.fetch_signature(ex.chat_id, ex.model).await else {
            return Ok(VerifiedExchange {
                provider: self.provider().to_string(),
                model: ex.model.to_string(),
                request_hash,
                response_hash,
                attestation,
                integrity: IntegrityProof::NearChatSignature {
                    text: String::new(),
                    signature: String::new(),
                    signing_address: String::new(),
                },
                verified: false,
            });
        };

        // The signed text must equal the one our exact bytes produce, and the
        // signature must recover to the address the TEE claims AND to a member of
        // the policy-accepted attested set.
        let text_matches = sig.text == expected_text;
        let recovered = recover_eip191_address(sig.text.as_bytes(), &sig.signature);
        let recovers_to_claim = recovered
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(&sig.signing_address));
        let attested = recovered.as_deref().is_some_and(|r| {
            attestation
                .attested_addresses
                .iter()
                .any(|a| a.eq_ignore_ascii_case(r))
        });

        let verified = attestation.verified && text_matches && recovers_to_claim && attested;

        Ok(VerifiedExchange {
            provider: self.provider().to_string(),
            model: ex.model.to_string(),
            request_hash,
            response_hash,
            attestation,
            integrity: IntegrityProof::NearChatSignature {
                text: sig.text,
                signature: sig.signature,
                signing_address: sig.signing_address,
            },
            verified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::near::report::AttestationReport;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");
    const MODEL: &str = "zai-org/GLM-5.1-FP8";
    const FIXTURE_NONCE: &str = "9a01356cb451dc2c3c0ce9a195245a0be984a3f73617f55f87913fc2f059cba7";
    const APP_ID: &str = "2c0a0c96cb6dbd659bf1446e2f3fce58172ff91b";
    const KMS_ROOT_DER_SPKI: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004228f800590a10442cba9d0e6adb2fa9f195eea9e75e23dd35990d52b59dda2415a63674c38adebde4ffd4d4b265bf818985933820c8053cee3ce29b5fb0fbcbc";
    const TEST_EC_PRIVATE_PKCS8_PEM: &str =
        include_str!("../../tests/fixtures/nras_test_ec_private_pkcs8.pem");
    const TEST_EC_PUBLIC_PEM: &str = include_str!("../../tests/fixtures/nras_test_ec_public.pem");

    fn nvidia_key() -> Arc<NvidiaEatKey> {
        Arc::new(NvidiaEatKey::from_ec_pem(TEST_EC_PUBLIC_PEM.as_bytes()).unwrap())
    }

    /// Sign an NRAS-shaped EAT carrying a passing result and the given nonce.
    fn signed_eat(eat_nonce: &str) -> Vec<u8> {
        let ek = EncodingKey::from_ec_pem(TEST_EC_PRIVATE_PKCS8_PEM.as_bytes()).unwrap();
        let claims = serde_json::json!({
            "x-nvidia-overall-att-result": true,
            "eat_nonce": eat_nonce,
        });
        let jwt = encode(&Header::new(Algorithm::ES256), &claims, &ek).unwrap();
        serde_json::to_vec(&serde_json::json!([["JWT", jwt], {}])).unwrap()
    }

    /// Replays the fixture report and a freshly-signed EAT bound to a fixed
    /// nonce; counts report fetches for the cache test.
    struct StubTransport {
        eat_nonce: String,
        fail_report: bool,
        report_fetches: AtomicUsize,
    }

    impl StubTransport {
        fn passing() -> Self {
            Self {
                eat_nonce: FIXTURE_NONCE.to_string(),
                fail_report: false,
                report_fetches: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ReportTransport for StubTransport {
        async fn fetch_report(
            &self,
            _model: &str,
            _nonce: &str,
        ) -> Result<AttestationReport, VerifyError> {
            self.report_fetches.fetch_add(1, Ordering::SeqCst);
            if self.fail_report {
                return Err(VerifyError::Malformed {
                    what: "report",
                    detail: "withheld".to_string(),
                });
            }
            Ok(serde_json::from_str(FIXTURE).unwrap())
        }

        async fn fetch_gpu_eat(&self, _nvidia_payload: &str) -> Result<Vec<u8>, VerifyError> {
            Ok(signed_eat(&self.eat_nonce))
        }
    }

    /// Parse-only quote verifier: returns real measurements from the fixture
    /// quote and claims authenticity (the live Intel-signature path is the
    /// #[ignore]d test in `tdx`).
    struct TrustingQuoteVerifier;

    #[async_trait]
    impl QuoteVerifier for TrustingQuoteVerifier {
        async fn measurements(
            &self,
            raw_quote: &[u8],
            _now_unix: u64,
        ) -> Result<crate::near::tdx::TdxMeasurements, VerifyError> {
            crate::near::tdx::parse_tdx_quote(raw_quote)
        }
        fn is_authenticated(&self) -> bool {
            true
        }
    }

    fn policy(workload: &str, kms: &str) -> Arc<AciDcapVerifierPolicy> {
        Arc::new(AciDcapVerifierPolicy::new([workload.to_string()], [], [kms.to_string()]).unwrap())
    }

    fn verifier(
        transport: Arc<dyn ReportTransport>,
        pol: Arc<AciDcapVerifierPolicy>,
    ) -> NearVerifier {
        NearVerifier::new(
            transport,
            Arc::new(TrustingQuoteVerifier),
            pol,
            nvidia_key(),
        )
    }

    #[tokio::test]
    async fn verifies_the_legitimate_model_end_to_end() {
        let v = verifier(
            Arc::new(StubTransport::passing()),
            policy(APP_ID, KMS_ROOT_DER_SPKI),
        );
        let verdict = v
            .verify_attestation(MODEL, FIXTURE_NONCE, 1_000)
            .await
            .unwrap();
        assert!(verdict.verified, "checks: {:?}", verdict.checks);
        assert!(verdict.checks.all_pass());
        assert_eq!(
            verdict.attested_addresses,
            vec!["0xbb4d2e7ffe98eefcd9690e2139be41e92b95e333".to_string()]
        );
        assert_eq!(verdict.trust_boundary, TRUST_BOUNDARY);
    }

    #[tokio::test]
    async fn rejects_a_genuine_tee_when_policy_pins_a_different_model() {
        // THE load-bearing case: every hardware check passes, but the policy
        // doesn't allowlist this model ⇒ unverified.
        let v = verifier(
            Arc::new(StubTransport::passing()),
            policy("some-other-workload", KMS_ROOT_DER_SPKI),
        );
        let verdict = v
            .verify_attestation(MODEL, FIXTURE_NONCE, 1_000)
            .await
            .unwrap();
        assert!(!verdict.verified);
        assert!(!verdict.checks.policy_accepts);
        assert!(verdict.attested_addresses.is_empty());
    }

    #[tokio::test]
    async fn fail_closed_when_the_report_is_withheld() {
        let transport = StubTransport {
            fail_report: true,
            ..StubTransport::passing()
        };
        let v = verifier(Arc::new(transport), policy(APP_ID, KMS_ROOT_DER_SPKI));
        let verdict = v
            .verify_attestation(MODEL, FIXTURE_NONCE, 1_000)
            .await
            .unwrap();
        assert!(!verdict.verified);
        assert!(!verdict.checks.all_pass());
    }

    #[tokio::test]
    async fn stale_nonce_fails_report_data_binding() {
        // A report bound to the fixture nonce can't satisfy a different request.
        let v = verifier(
            Arc::new(StubTransport::passing()),
            policy(APP_ID, KMS_ROOT_DER_SPKI),
        );
        let verdict = v
            .verify_attestation(MODEL, "00".repeat(32).as_str(), 1_000)
            .await
            .unwrap();
        assert!(!verdict.verified);
        assert!(!verdict.checks.report_data_binds_key_and_nonce);
    }

    #[tokio::test]
    async fn verdict_cached_fetches_once_within_ttl_then_refreshes_after() {
        let transport = Arc::new(StubTransport::passing());
        let v = verifier(transport.clone(), policy(APP_ID, KMS_ROOT_DER_SPKI)).with_cache_ttl(600);

        // verdict_cached uses a fresh random nonce, so against the fixed fixture
        // the verdict is unverified and cached only for the short retry window.
        v.verdict_cached(MODEL, 1_000).await.unwrap();
        v.verdict_cached(MODEL, 1_030).await.unwrap();
        assert_eq!(
            transport.report_fetches.load(Ordering::SeqCst),
            1,
            "second call within the retry window hits cache"
        );

        v.verdict_cached(MODEL, 1_061).await.unwrap(); // past the 60s unverified TTL
        assert_eq!(
            transport.report_fetches.load(Ordering::SeqCst),
            2,
            "re-verifies after the retry window"
        );
    }

    #[tokio::test]
    async fn verified_verdict_caches_for_the_full_ttl() {
        // With the fixture nonce the verdict verifies; it should cache for the
        // full configured TTL, not the short unverified retry window.
        let transport = Arc::new(StubTransport::passing());
        let v = verifier(transport, policy(APP_ID, KMS_ROOT_DER_SPKI)).with_cache_ttl(600);
        let verdict = v
            .verify_attestation(MODEL, FIXTURE_NONCE, 1_000)
            .await
            .unwrap();
        assert!(verdict.verified);
        v.cache.put(verdict, 600, 1_000);
        assert!(
            v.cache.get(MODEL, 1_599).is_some(),
            "still fresh within TTL"
        );
        assert!(
            v.cache.get(MODEL, 1_600).is_none(),
            "expired after full TTL"
        );
    }

    // ===== L1.5: verify_exchange =====

    use crate::near::signature::ChatSignature;

    /// 20-byte address for secp256k1 private key = 1.
    const ADDR1: &str = "7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    /// EIP-191 sign `text` with the secp256k1 key whose 32nd byte is `priv_lsb`.
    fn sign_chat(priv_lsb: u8, text: &str) -> String {
        use k256::ecdsa::SigningKey;
        use sha3::{Digest, Keccak256};
        let mut pk = [0u8; 32];
        pk[31] = priv_lsb;
        let key = SigningKey::from_slice(&pk).unwrap();
        let mut h = Keccak256::new();
        h.update(b"\x19Ethereum Signed Message:\n");
        h.update(text.len().to_string().as_bytes());
        h.update(text.as_bytes());
        let digest: [u8; 32] = h.finalize().into();
        let (sig, rec) = key.sign_prehash_recoverable(&digest).unwrap();
        let mut out = sig.to_bytes().to_vec();
        out.push(27 + rec.to_byte());
        hex::encode(out)
    }

    /// The fixture report with its quote `report_data` and `signing_address`
    /// rewritten to bind `ADDR1` + `nonce` (parse-only quote verification lets us
    /// splice the 64-byte report_data without breaking the RTMR3/event-log
    /// anchor, which is unchanged).
    fn spliced_report(nonce_hex: &str) -> AttestationReport {
        let mut report: AttestationReport = serde_json::from_str(FIXTURE).unwrap();
        let m = &mut report.model_attestations[0];
        let mut quote = hex::decode(&m.intel_quote).unwrap();
        let addr = hex::decode(ADDR1).unwrap();
        let nonce = hex::decode(nonce_hex).unwrap();
        let mut rd = [0u8; 64];
        rd[..20].copy_from_slice(&addr);
        rd[32..64].copy_from_slice(&nonce);
        quote[568..632].copy_from_slice(&rd);
        m.intel_quote = hex::encode(&quote);
        m.signing_address = format!("0x{ADDR1}");
        m.request_nonce = nonce_hex.to_string();
        report
    }

    /// Nonce-aware transport: each `fetch_report(nonce)` binds `ADDR1` + that
    /// nonce, and `fetch_gpu_eat` echoes it, so the verifier's fresh random nonce
    /// verifies. `chat_sig` is the canned signature (None ⇒ withheld).
    struct ExchangeStub {
        last_nonce: std::sync::Mutex<String>,
        chat_sig: Option<ChatSignature>,
    }

    #[async_trait]
    impl ReportTransport for ExchangeStub {
        async fn fetch_report(
            &self,
            _model: &str,
            nonce: &str,
        ) -> Result<AttestationReport, VerifyError> {
            *self.last_nonce.lock().unwrap() = nonce.to_string();
            Ok(spliced_report(nonce))
        }
        async fn fetch_gpu_eat(&self, _payload: &str) -> Result<Vec<u8>, VerifyError> {
            Ok(signed_eat(&self.last_nonce.lock().unwrap()))
        }
        async fn fetch_signature(
            &self,
            _chat_id: &str,
            _model: &str,
        ) -> Result<ChatSignature, VerifyError> {
            self.chat_sig.clone().ok_or(VerifyError::Malformed {
                what: "chat signature",
                detail: "withheld".to_string(),
            })
        }
    }

    fn exchange_verifier(chat_sig: Option<ChatSignature>) -> NearVerifier {
        NearVerifier::new(
            Arc::new(ExchangeStub {
                last_nonce: std::sync::Mutex::new(String::new()),
                chat_sig,
            }),
            Arc::new(TrustingQuoteVerifier),
            policy(APP_ID, KMS_ROOT_DER_SPKI),
            nvidia_key(),
        )
    }

    fn signature_over(model: &str, req: &[u8], resp: &[u8], priv_lsb: u8) -> ChatSignature {
        let text =
            crate::chat_signing_text(model, &crate::sha256_hex(req), &crate::sha256_hex(resp));
        let signature = sign_chat(priv_lsb, &text);
        let signing_address = crate::recover_eip191_address(text.as_bytes(), &signature).unwrap();
        ChatSignature {
            text,
            signature,
            signing_address,
            signing_algo: "ecdsa".to_string(),
        }
    }

    #[tokio::test]
    async fn verifies_a_genuine_exchange_end_to_end() {
        let (req, resp) = (
            b"the request bytes".as_slice(),
            b"the response bytes".as_slice(),
        );
        // Signed by privkey 1, whose address (ADDR1) is the attested signer.
        let sig = signature_over(MODEL, req, resp, 1);
        let v = exchange_verifier(Some(sig));

        let ex = crate::ExchangeInput {
            model: MODEL,
            request_body: req,
            response_body: resp,
            chat_id: "chat-1",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();

        assert!(out.verified, "attestation: {:?}", out.attestation.checks);
        assert_eq!(out.request_hash, crate::sha256_hex(req));
        assert!(matches!(
            out.integrity,
            crate::IntegrityProof::NearChatSignature { .. }
        ));
    }

    #[tokio::test]
    async fn tampered_response_body_fails_the_text_match() {
        let (req, resp) = (b"req".as_slice(), b"original response".as_slice());
        // Signature is over the ORIGINAL bodies...
        let sig = signature_over(MODEL, req, resp, 1);
        let v = exchange_verifier(Some(sig));

        // ...but the client presents a DIFFERENT response: text won't match.
        let ex = crate::ExchangeInput {
            model: MODEL,
            request_body: req,
            response_body: b"tampered response",
            chat_id: "chat-1",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();
        assert!(!out.verified);
    }

    #[tokio::test]
    async fn signature_from_an_unattested_key_fails() {
        // Valid signature, recovers to its claimed address — but that address
        // (privkey 2) is NOT the attested signer (ADDR1). The load-bearing L1.5
        // case: a real signature from the wrong key.
        let (req, resp) = (b"req".as_slice(), b"resp".as_slice());
        let sig = signature_over(MODEL, req, resp, 2);
        let v = exchange_verifier(Some(sig));

        let ex = crate::ExchangeInput {
            model: MODEL,
            request_body: req,
            response_body: resp,
            chat_id: "chat-1",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();
        assert!(!out.verified);
    }

    #[tokio::test]
    async fn withheld_signature_is_fail_closed() {
        let (req, resp) = (b"req".as_slice(), b"resp".as_slice());
        let v = exchange_verifier(None); // signature withheld by the cloud
        let ex = crate::ExchangeInput {
            model: MODEL,
            request_body: req,
            response_body: resp,
            chat_id: "chat-1",
            now_unix: 1_000,
        };
        let out = v.verify_exchange(&ex).await.unwrap();
        assert!(!out.verified);
    }
}
