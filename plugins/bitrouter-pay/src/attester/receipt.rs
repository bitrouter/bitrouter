//! Attestation receipt persisted immediately after Chainlink inference completes.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttestationReceipt {
    pub inference_id: String,
    pub model: String,
    pub request_digest: String,
    pub response_digest: String,
    pub resource_digest: String,
    pub filename_digest: String,
    pub filename_blinding: String,
    pub completed_at: String,
    pub attested: bool,
}
