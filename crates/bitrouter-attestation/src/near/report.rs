//! Wire types for NEAR AI Cloud's attestation report
//! (`GET {base}/v1/attestation/report`).
//!
//! NEAR's **model** report is a full dstack attestation: each entry in
//! `model_attestations[]` carries an Intel TDX quote, an NVIDIA GPU payload, a
//! dstack event log, and an `info` block exposing the KMS root, app/workload
//! id, and image digests (spec §1.5, Decision 8). We model only the fields the
//! verifier consumes; serde ignores the rest.

/// Top-level report body: `{ gateway_attestation, model_attestations }`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttestationReport {
    /// A list — NEAR serves multi-node and caches the signature per node, so a
    /// model can present more than one attested signing identity (spec §1.5
    /// cond. 2). Verify the model attestation directly; we don't trust the
    /// gateway attestation (that is `nearai.py`'s weaker path).
    pub model_attestations: Vec<ModelAttestation>,
}

/// One serving node's attestation of a model.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelAttestation {
    pub model_name: String,
    /// The attested signing identity (ECDSA address) an L1.5 chat signature
    /// must recover to.
    pub signing_address: String,
    pub signing_algo: String,
    /// Intel TDX quote, hex-encoded.
    pub intel_quote: String,
    /// NVIDIA GPU attestation payload, a JSON document carried as a string
    /// (`{"arch":"HOPPER","evidence_list":[…]}`), forwarded to NRAS.
    pub nvidia_payload: String,
    /// The client nonce echoed back, bound into `report_data`.
    pub request_nonce: String,
    pub info: AttestationInfo,
}

/// The dstack `info` block. Source of the policy-pinning fields (spec §1.5
/// Decision 8). These map to the ported DCAP policy's pins (see
/// [`crate::near::dcap::model_identity`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttestationInfo {
    /// dstack app id — the **workload id** the policy allowlists.
    pub app_id: String,
    /// `sha256(app_compose)` — one of the **image digests** the policy
    /// allowlists, and the target of the compose↔mr_config binding (Task 3).
    pub compose_hash: String,
    /// Guest OS image hash — another **image digest** the policy allowlists.
    pub os_image_hash: String,
    /// dstack key-provider block, a JSON string `{"name":"kms","id":"<hex>"}`.
    /// The `id` is the **KMS root** public key (a P-256 DER SPKI) the policy
    /// pins (spec §1.5 cond. 1).
    pub key_provider_info: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");

    #[test]
    fn deserializes_live_near_model_report() {
        let report: AttestationReport =
            serde_json::from_str(FIXTURE).expect("fixture should deserialize");

        assert_eq!(report.model_attestations.len(), 1);
        let m = &report.model_attestations[0];

        assert_eq!(m.model_name, "zai-org/GLM-5.1-FP8");
        assert_eq!(
            m.signing_address,
            "0xbb4d2e7ffe98eefcd9690e2139be41e92b95e333"
        );
        assert_eq!(m.signing_algo, "ecdsa");
        assert_eq!(
            m.request_nonce,
            "9a01356cb451dc2c3c0ce9a195245a0be984a3f73617f55f87913fc2f059cba7"
        );
        // Intel TDX quote is hex; NVIDIA payload is a JSON string for NRAS.
        assert!(m.intel_quote.starts_with("040002008100"));
        assert!(m.nvidia_payload.contains("\"arch\":\"HOPPER\""));
        // compose hash drives the compose↔mr_config binding (Task 3).
        assert_eq!(
            m.info.compose_hash,
            "c445f29994165e94e85bdfc4824f4bcba89b0a883f45e7912f1bfd7c2634a698"
        );
    }
}
