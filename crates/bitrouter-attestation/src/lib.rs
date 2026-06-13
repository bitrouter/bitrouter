//! # bitrouter-attestation
//!
//! Provider-agnostic, **client-side** confidential-inference verification. The
//! central abstraction is [`ConfidentialVerifier`]: given a model and a nonce
//! it proves (L1) the serving endpoint is genuine TEE hardware running the
//! *legitimate, policy-pinned* model, and given an exact request/response it
//! proves (L1.5) that exchange ran in that TEE unmodified.
//!
//! The design mirrors private-ai-gateway's `UpstreamVerifier` /
//! `UpstreamVerifiedEvent` normalization, but runs in the caller's own trusted
//! process (bitrouter-cli's local daemon) instead of inside an attested
//! re-signing gateway — so it needs **no TEE of its own**. See the refactor
//! spec (`bitrouter-cloud/docs/bitrouter-attestation-plugin.md`).
//!
//! This crate is intentionally pure: no SDK, axum, or server dependency, so it
//! ships in the daemon, the `bitrouter verify` CLI, the cloud `/v1/aci/verify`
//! endpoint, and third-party clients alike.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

mod near;
mod transport;
mod types;

pub use near::binding::{compose_matches_mr_config, report_data_binds};
pub use near::report::{AttestationInfo, AttestationReport, ModelAttestation};
pub use near::tdx::{PHALA_PCCS_URL, TdxMeasurements, parse_tdx_quote, verify_tdx_quote};
pub use transport::{MockTransport, ReportTransport, ReqwestTransport, SIGNING_ALGO};
pub use types::{
    AttestationChecks, AttestationVerdict, ExchangeInput, IntegrityProof, VerifiedExchange,
    VerifyError,
};

/// Verifies confidential inference for one provider family, client-side.
///
/// A *failed* verification is **not** an `Err` — it is an `Ok` verdict with
/// `verified == false` (fail-closed; spec §1.5 cond. 3). `Err` is reserved for
/// the verifier being unable to reach a trustworthy verdict at all
/// (misconfiguration, malformed input).
#[async_trait]
pub trait ConfidentialVerifier: Send + Sync {
    /// Provider type handled — `"near-ai"` (later `"aci-gateway"`, `"tinfoil"`).
    fn provider(&self) -> &str;

    /// L1 — prove the model endpoint is a genuine, policy-pinned TEE. Yields the
    /// attested signing identity set that L1.5 binds an exchange signature to.
    async fn verify_attestation(
        &self,
        model: &str,
        nonce: &str,
        now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError>;

    /// L1.5 — prove a specific exchange ran in that TEE unmodified.
    async fn verify_exchange(
        &self,
        ex: &ExchangeInput<'_>,
    ) -> Result<VerifiedExchange, VerifyError>;
}

/// Dispatches by provider so the daemon/cloud can hold one handle and serve
/// many confidential providers. ← gateway's `RoutingUpstreamVerifier`. NEAR is
/// the only impl today; the registry exists so Tinfoil/Phala/AciGateway slot in
/// without touching callers.
#[derive(Default, Clone)]
pub struct VerifierRegistry {
    map: HashMap<String, Arc<dyn ConfidentialVerifier>>,
}

impl VerifierRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a verifier under its own `provider()` key. Returns `self` for
    /// builder-style chaining at boot.
    pub fn with(mut self, verifier: Arc<dyn ConfidentialVerifier>) -> Self {
        self.map.insert(verifier.provider().to_string(), verifier);
        self
    }

    /// Look up the verifier for a provider, or `UnknownProvider` if none is
    /// registered — callers fail closed rather than silently skip verification.
    pub fn get(&self, provider: &str) -> Result<&Arc<dyn ConfidentialVerifier>, VerifyError> {
        self.map
            .get(provider)
            .ok_or_else(|| VerifyError::UnknownProvider(provider.to_string()))
    }

    /// True if a verifier is registered for `provider`.
    pub fn handles(&self, provider: &str) -> bool {
        self.map.contains_key(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubVerifier;

    #[async_trait]
    impl ConfidentialVerifier for StubVerifier {
        fn provider(&self) -> &str {
            "near-ai"
        }
        async fn verify_attestation(
            &self,
            model: &str,
            nonce: &str,
            now_unix: u64,
        ) -> Result<AttestationVerdict, VerifyError> {
            Ok(AttestationVerdict::unverified(model, nonce, now_unix))
        }
        async fn verify_exchange(
            &self,
            _ex: &ExchangeInput<'_>,
        ) -> Result<VerifiedExchange, VerifyError> {
            Err(VerifyError::Malformed {
                what: "exchange",
                detail: "not implemented in P1".to_string(),
            })
        }
    }

    #[test]
    fn registry_dispatches_by_provider_and_fails_closed_on_unknown() {
        let reg = VerifierRegistry::new().with(Arc::new(StubVerifier));
        assert!(reg.handles("near-ai"));
        assert!(reg.get("near-ai").is_ok());
        assert!(!reg.handles("tinfoil"));
        match reg.get("tinfoil").err() {
            Some(VerifyError::UnknownProvider(p)) => assert_eq!(p, "tinfoil"),
            _ => panic!("expected UnknownProvider for an unregistered provider"),
        }
    }

    #[test]
    fn unverified_verdict_is_fail_closed() {
        let v = AttestationVerdict::unverified("zai-org/GLM-5.1-FP8", "nonce123", 42);
        assert!(!v.verified);
        assert!(!v.checks.all_pass());
        assert!(v.attested_addresses.is_empty());
    }
}
