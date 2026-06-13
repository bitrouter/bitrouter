//! Chainlink Confidential AI Attester client.

use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::attester::receipt::AttestationReceipt;
use crate::PayError;

const BASE_URL: &str = "https://confidential-ai-dev-preview.cldev.cloud";
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const TIMEOUT: Duration = Duration::from_secs(600);

const SUPPORTED_MODELS: &[&str] = &["qwen3.6", "gemma4"];

/// Attestation-specific failures.
#[derive(Debug, thiserror::Error)]
pub enum AttestError {
    #[error("upstream returned error: {0}")]
    Upstream(String),
    #[error("inference failed: {0}")]
    Failed(String),
    #[error("unsupported model: {0}")]
    UnsupportedModel(String),
    #[error("timeout")]
    Timeout,
}

impl From<AttestError> for PayError {
    fn from(value: AttestError) -> Self {
        match value {
            AttestError::Timeout => PayError::Timeout,
            other => PayError::AttestError(other.to_string()),
        }
    }
}

/// Resource attached to an inference request.
#[derive(Debug, Clone, Serialize)]
pub struct Resource {
    pub filename: String,
    pub content_type: String,
    pub content_base64: String,
}

impl Resource {
    /// Build a resource from raw bytes, preferring `text/plain` or `image/png`.
    pub fn from_bytes(filename: impl Into<String>, content_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            content_base64: STANDARD.encode(bytes),
        }
    }
}

/// Chainlink Confidential AI Attester HTTP client.
pub struct ChainlinkAttester {
    api_key: String,
    client: reqwest::Client,
}

impl ChainlinkAttester {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Submit an inference request and poll until completion.
    pub async fn infer(
        &self,
        model: &str,
        prompt: &str,
        resources: Vec<Resource>,
    ) -> Result<AttestationReceipt, AttestError> {
        if !SUPPORTED_MODELS.contains(&model) {
            return Err(AttestError::UnsupportedModel(model.to_string()));
        }

        let submit = InferenceSubmit {
            model: model.to_string(),
            prompt: prompt.to_string(),
            resources,
        };

        let resp = self
            .client
            .post(format!("{BASE_URL}/v1/inference"))
            .bearer_auth(&self.api_key)
            .json(&submit)
            .send()
            .await
            .map_err(|e| AttestError::Upstream(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 202 {
            let body = resp.text().await.unwrap_or_default();
            return Err(AttestError::Upstream(format!(
                "POST /v1/inference returned {status}: {body}"
            )));
        }

        let queued: QueuedResponse = resp
            .json()
            .await
            .map_err(|e| AttestError::Upstream(e.to_string()))?;

        let started = Instant::now();
        loop {
            if started.elapsed() >= TIMEOUT {
                return Err(AttestError::Timeout);
            }

            tokio::time::sleep(POLL_INTERVAL).await;

            let status_resp = self
                .client
                .get(format!("{BASE_URL}/v1/inference/{}", queued.id))
                .bearer_auth(&self.api_key)
                .send()
                .await
                .map_err(|e| AttestError::Upstream(e.to_string()))?;

            let snapshot: InferenceStatus = status_resp
                .json()
                .await
                .map_err(|e| AttestError::Upstream(e.to_string()))?;

            match snapshot.status.as_str() {
                "completed" => return map_receipt(snapshot),
                "failed" => {
                    let msg = snapshot
                        .error
                        .unwrap_or_else(|| "inference failed".into());
                    return Err(AttestError::Failed(msg));
                }
                _ => continue,
            }
        }
    }
}

#[derive(Serialize)]
struct InferenceSubmit {
    model: String,
    prompt: String,
    resources: Vec<Resource>,
}

#[derive(Deserialize)]
struct QueuedResponse {
    id: String,
}

/// Per-resource attestation fields returned by the Chainlink enclave.
#[derive(Deserialize)]
struct ResourceStatus {
    /// SHA-256 of the original resource content.
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    request_digest: Option<String>,
    #[serde(default)]
    response_digest: Option<String>,
    #[serde(default)]
    filename_digest: Option<String>,
    #[serde(default)]
    filename_blinding: Option<String>,
}

/// Top-level polling response from `GET /v1/inference/:id`.
#[derive(Deserialize)]
struct InferenceStatus {
    id: String,
    status: String,
    model: Option<String>,
    #[serde(default)]
    error: Option<String>,
    /// Populated on completion; digests live inside each resource entry.
    #[serde(default)]
    resources: Vec<ResourceStatus>,
    completed_at: Option<String>,
}

fn map_receipt(snapshot: InferenceStatus) -> Result<AttestationReceipt, AttestError> {
    // Digests are per-resource; we take the first resource (typically the only one).
    let res = snapshot.resources.into_iter().next().unwrap_or(ResourceStatus {
        digest: None,
        request_digest: None,
        response_digest: None,
        filename_digest: None,
        filename_blinding: None,
    });
    Ok(AttestationReceipt {
        inference_id: snapshot.id,
        model: snapshot.model.unwrap_or_default(),
        request_digest: res.request_digest.unwrap_or_default(),
        response_digest: res.response_digest.unwrap_or_default(),
        resource_digest: res.digest.unwrap_or_default(),
        filename_digest: res.filename_digest.unwrap_or_default(),
        filename_blinding: res.filename_blinding.unwrap_or_default(),
        completed_at: snapshot.completed_at.unwrap_or_default(),
        attested: true,
    })
}
