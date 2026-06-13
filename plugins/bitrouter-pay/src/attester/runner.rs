//! Run one attested Chainlink inference and return an honest `VerifiedExchange`.

use base64::Engine as _;

use bitrouter_attestation::{ConfidentialVerifier, ExchangeInput, VerifiedExchange};
use bitrouter_chainlink::{
    ChainlinkClient, ChainlinkVerifier, InferenceRequest, PollConfig, Resource, Status,
};

use crate::PayError;

const BASE_URL: &str = "https://confidential-ai-dev-preview.cldev.cloud";

/// Submit `prompt` + `resource_bytes` to Chainlink, poll to completion, then
/// build an honest `VerifiedExchange` (verified=false; digests_consistent set by
/// the shared verifier).
pub async fn run_attested_inference(
    api_key: &str,
    model: &str,
    prompt: &str,
    resource_bytes: &[u8],
    now_unix: u64,
) -> Result<VerifiedExchange, PayError> {
    let http = reqwest::Client::new();
    let client = ChainlinkClient::new(
        http,
        BASE_URL.to_string(),
        api_key.to_string(),
        PollConfig::default(),
    );

    let req = InferenceRequest {
        model: model.to_string(),
        prompt: prompt.to_string(),
        system_prompt: None,
        resources: vec![Resource {
            filename: "payload.json".to_string(),
            content_type: "text/plain".to_string(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(resource_bytes),
        }],
    };

    let submitted = client
        .submit(&req)
        .await
        .map_err(|e| PayError::AttestError(e.to_string()))?;
    let done = client
        .poll_until_done(&submitted.id)
        .await
        .map_err(|e| PayError::AttestError(e.to_string()))?;
    if done.status != Status::Completed {
        return Err(PayError::AttestError(
            "chainlink inference did not complete".into(),
        ));
    }
    let output = done.output.unwrap_or_default();

    let verifier = ChainlinkVerifier::new(BASE_URL.to_string(), api_key.to_string());
    let ex = ExchangeInput {
        model,
        request_body: resource_bytes,
        response_body: output.as_bytes(),
        chat_id: &done.id,
        now_unix,
    };
    verifier
        .verify_exchange(&ex)
        .await
        .map_err(|e| PayError::AttestError(e.to_string()))
}
