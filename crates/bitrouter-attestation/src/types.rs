//! Normalized result types shared by every [`ConfidentialVerifier`] impl.
//!
//! These mirror private-ai-gateway's `UpstreamVerifiedEvent` / `ChannelBinding`
//! normalization (`src/aci/receipt.rs`), but are produced **client-side** and
//! carry each provider's *native* integrity proof rather than a re-signed ACI
//! receipt. See the refactor spec §2.

/// The exact bytes of one request/response exchange to verify (L1.5).
///
/// Hashing is over these raw bytes verbatim — including any trailing newlines a
/// streamed response carries — because the TEE signs `sha256` of the same
/// bytes. Anything that re-serializes the body (e.g. NEAR's gateway) breaks the
/// match, so verifiable calls must use NEAR direct-completions (spec §3).
pub struct ExchangeInput<'a> {
    pub model: &'a str,
    /// Exact bytes the client sent.
    pub request_body: &'a [u8],
    /// Exact bytes the client received.
    pub response_body: &'a [u8],
    /// Chat id taken from the response body's `id` field; selects the signature.
    pub chat_id: &'a str,
    pub now_unix: u64,
}

/// The per-check breakdown of an attestation, mirroring private-ai-gateway's
/// `AciDcapVerifier` check set (`src/aci/verifier/dcap.rs`). Every field is
/// surfaced so a caller can see *why* a verdict is (un)verified, gateway-style.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttestationChecks {
    /// NVIDIA NRAS verdict PASS, nonce echoed, EAT signature valid.
    pub gpu_nras_pass: bool,
    /// Intel signature + collateral + measurements (via `dcap-qvl`).
    pub dcap_quote_valid: bool,
    /// `report_data` embeds the attested signing key and the client nonce.
    pub report_data_binds_key_and_nonce: bool,
    /// `sha256(compose) == mr_config`.
    pub compose_matches_mr_config: bool,
    /// LOAD-BEARING (spec §1.5 cond. 1): `workload_id ∈ accepted_workload_ids`
    /// OR `image_digest ∈ accepted_image_digests`, under
    /// `accepted_kms_root_public_keys`. The policy fails to construct if
    /// unpinned; without this, every other check passes for an attacker-owned
    /// genuine TEE running a malicious model.
    pub policy_accepts: bool,
    /// TD debug-bit off.
    pub debug_disabled: bool,
    /// dstack RTMR3 / event-log replay, when the report carries an event log.
    pub event_log_rtmr_ok: Option<bool>,
    /// Surfaced as a claim, not a hard fail (matches gateway behavior).
    pub tcb_status: Option<String>,
}

impl AttestationChecks {
    /// All-false checks — the fail-closed default for a node whose evidence
    /// couldn't be gathered or fully evaluated.
    pub fn failed() -> Self {
        Self {
            gpu_nras_pass: false,
            dcap_quote_valid: false,
            report_data_binds_key_and_nonce: false,
            compose_matches_mr_config: false,
            policy_accepts: false,
            debug_disabled: false,
            event_log_rtmr_ok: None,
            tcb_status: None,
        }
    }

    /// True iff every mandatory check passed. `tcb_status` is a claim, not a
    /// gate. `event_log_rtmr_ok` is **required** to be `Some(true)`: it is the
    /// anchor that binds the cloud-supplied `info` (and thus `policy_accepts`)
    /// to the genuine TEE measurement, so a `None` ("not checked") or
    /// `Some(false)` ("replay/binding failed") verdict must not pass.
    pub fn all_pass(&self) -> bool {
        self.gpu_nras_pass
            && self.dcap_quote_valid
            && self.report_data_binds_key_and_nonce
            && self.compose_matches_mr_config
            && self.policy_accepts
            && self.debug_disabled
            && self.event_log_rtmr_ok == Some(true)
    }
}

/// L1 verdict: the model endpoint is genuine TEE hardware running the
/// *legitimate* (policy-pinned) model. Yields the attested signing identity set
/// that L1.5 binds a chat signature to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttestationVerdict {
    pub model: String,
    pub verified: bool,
    /// The attested signing addresses from `model_attestations[]` — a SET,
    /// because NEAR serves multi-node and caches per node. A chat signature is
    /// trusted iff it recovers to one of these AND `checks.policy_accepts`
    /// holds (spec §1.5 cond. 1 & 2).
    pub attested_addresses: Vec<String>,
    /// Honest trust-boundary label (gateway convention): `"near-ai-model"` when
    /// we verified the model quote directly; would be `"near-ai-gateway"` if we
    /// ever fell back to trusting NEAR's gateway (as `nearai.py` does).
    pub trust_boundary: String,
    pub nonce: String,
    pub checks: AttestationChecks,
    pub verified_at_unix: u64,
}

impl AttestationVerdict {
    /// A fully-failed, fail-closed verdict (spec §1.5 cond. 3) with every check
    /// false. Used when a fetch is withheld or a sub-check fails — never a
    /// silent pass.
    pub fn unverified(model: impl Into<String>, nonce: impl Into<String>, now_unix: u64) -> Self {
        Self {
            model: model.into(),
            verified: false,
            attested_addresses: Vec::new(),
            trust_boundary: String::new(),
            nonce: nonce.into(),
            checks: AttestationChecks::failed(),
            verified_at_unix: now_unix,
        }
    }
}

/// Each provider's *native* integrity proof — the portable artifact a third
/// party could re-check. Mirrors the gateway's evidence/`ChannelBinding`, but
/// kept provider-native rather than normalized to one receipt shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IntegrityProof {
    /// NEAR's per-chat signature over `{model}:{sha256(req)}:{sha256(resp)}`,
    /// EIP-191 ECDSA, recoverable to the attested signing address.
    NearChatSignature {
        text: String,
        signature: String,
        signing_address: String,
    },
    /// A real ACI gateway's signed receipt (future `AciGatewayVerifier`).
    AciReceipt {
        receipt: serde_json::Value,
        gateway_attestation: serde_json::Value,
    },
    /// Chainlink Confidential AI's per-inference resource digests. **UNSIGNED** —
    /// the dev-preview exposes no enclave signature, so this is tamper-evidence
    /// relative to the service's self-report, not a TEE trust anchor.
    /// `digests_consistent` records whether the resource `digest` the client
    /// re-computed locally (sha256 of the bytes it uploaded) matches the reported
    /// one. The `request_digest`/`response_digest` are over Chainlink's
    /// unpublished canonical metadata and are not client-reproducible.
    ChainlinkResourceDigests {
        inference_id: String,
        request_digest: String,
        response_digest: String,
        resource_digest: String,
        filename_digest: String,
        filename_blinding: String,
        digests_consistent: bool,
    },
}

/// L1.5 result: a specific exchange provably ran in the attested TEE unmodified.
/// ← gateway's `UpstreamVerifiedEvent`, normalized and client-side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerifiedExchange {
    pub provider: String,
    pub model: String,
    /// `sha256(request_body)`, hex.
    pub request_hash: String,
    /// `sha256(response_body)`, hex.
    pub response_hash: String,
    pub attestation: AttestationVerdict,
    pub integrity: IntegrityProof,
    /// `attestation.verified && integrity holds && binds to attested key`.
    pub verified: bool,
}

#[cfg(test)]
mod integrity_proof_tests {
    use super::*;

    fn chainlink_proof(digests_consistent: bool) -> IntegrityProof {
        IntegrityProof::ChainlinkResourceDigests {
            inference_id: "abc".to_string(),
            request_digest: "rq".to_string(),
            response_digest: "rs".to_string(),
            resource_digest: "rd".to_string(),
            filename_digest: "fd".to_string(),
            filename_blinding: "fb".to_string(),
            digests_consistent,
        }
    }

    #[test]
    fn chainlink_resource_digests_roundtrips() {
        // Round-trip both `digests_consistent` cases so a future
        // `skip_serializing_if` that drops the `false` case can't slip through.
        for consistent in [true, false] {
            let proof = chainlink_proof(consistent);
            let json = serde_json::to_value(&proof).expect("serialize");
            let back: IntegrityProof = serde_json::from_value(json).expect("deserialize");
            assert_eq!(proof, back);
        }
    }
}

/// Errors a [`ConfidentialVerifier`] can return. Note that a *failed
/// verification* is not an error — it is a verdict with `verified=false`
/// (fail-closed). `VerifyError` is reserved for the verifier being unable to
/// even reach a verdict it can trust (misconfiguration, malformed input).
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A network fetch (report, signature, NRAS) failed.
    #[error("transport error fetching {what}: {source}")]
    Transport {
        what: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A wire payload could not be parsed into the expected shape.
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
    /// The DCAP policy was constructed without the mandatory pins (spec §1.5
    /// cond. 1). The verifier refuses to run unpinned.
    #[error("attestation policy misconfigured: {0}")]
    Policy(String),
    /// No verifier is registered for the requested provider.
    #[error("no confidential verifier registered for provider {0:?}")]
    UnknownProvider(String),
}
