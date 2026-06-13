//! Serde types for the Chainlink `/v1/inference` request and response envelope.
//! Shapes mirror the dev-preview API docs (submit returns 202 with an id +
//! status; polling returns the same object, `completed` carrying `output`).

use serde::{Deserialize, Serialize};

/// A resource (document) attached to an inference request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resource {
    /// Original filename.
    pub filename: String,
    /// IANA content type, e.g. `text/plain`.
    pub content_type: String,
    /// Base64-encoded content (no `data:` prefix).
    pub content_base64: String,
}

/// Request body for `POST /v1/inference`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InferenceRequest {
    /// Chainlink model id (`gemma4` / `qwen3.6`).
    pub model: String,
    /// The flattened user/assistant transcript.
    pub prompt: String,
    /// Optional system instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Inline resources (documents) the enclave fetches + digests. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<Resource>,
}

/// Pipeline status reported by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Waiting in the queue.
    Queued,
    /// Fetching / preparing resources.
    PreparingResources,
    /// Running inference.
    Processing,
    /// Finished successfully (`output` is present).
    Completed,
    /// Terminal failure.
    Failed,
}

/// Full per-resource digests returned on completion (`resources[]`). All fields
/// optional — present once the resource has been fetched + processed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResourceDigest {
    /// SHA-256 of the original resource content (client-reproducible).
    #[serde(default)]
    pub digest: Option<String>,
    /// SHA-256 of Chainlink's canonical request metadata (not reproducible).
    #[serde(default)]
    pub request_digest: Option<String>,
    /// SHA-256 of Chainlink's canonical response metadata (not reproducible).
    #[serde(default)]
    pub response_digest: Option<String>,
    /// SHA-256 of the blinded filename.
    #[serde(default)]
    pub filename_digest: Option<String>,
    /// Hex random blinding value for the filename.
    #[serde(default)]
    pub filename_blinding: Option<String>,
}

/// Token usage reported on completion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct WireUsage {
    /// Prompt tokens.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub completion_tokens: u64,
}

/// Response body for both the submit (`202`) and poll (`200`) calls.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InferenceResponse {
    /// Job id.
    pub id: String,
    /// Current pipeline status.
    pub status: Status,
    /// Model echo.
    #[serde(default)]
    pub model: Option<String>,
    /// The completion text (present once `status == Completed`).
    #[serde(default)]
    pub output: Option<String>,
    /// Usage (present once completed).
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// Error detail when `status == Failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// Full per-resource digests when resources were used.
    #[serde(default)]
    pub resources: Vec<ResourceDigest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_queued_submit_response() {
        let body = serde_json::json!({
            "id": "0198a69b-2c30-7f12-a411-7d86de7c4a01",
            "status": "queued",
            "queue_position": 1,
            "model": "gemma4",
            "prompt": "hi"
        });
        let r: InferenceResponse = serde_json::from_value(body).expect("parse");
        assert_eq!(r.status, Status::Queued);
        assert_eq!(r.id, "0198a69b-2c30-7f12-a411-7d86de7c4a01");
        assert!(r.output.is_none());
    }

    #[test]
    fn deserializes_completed_response_with_output_and_usage() {
        let body = serde_json::json!({
            "id": "abc",
            "status": "completed",
            "model": "gemma4",
            "output": "The report identifies three risks...",
            "usage": { "prompt_tokens": 1200, "completion_tokens": 180 }
        });
        let r: InferenceResponse = serde_json::from_value(body).expect("parse");
        assert_eq!(r.status, Status::Completed);
        assert_eq!(
            r.output.as_deref(),
            Some("The report identifies three risks...")
        );
        assert_eq!(
            r.usage,
            Some(WireUsage {
                prompt_tokens: 1200,
                completion_tokens: 180
            })
        );
    }

    #[test]
    fn deserializes_preparing_resources_kebab_case() {
        let body = serde_json::json!({ "id": "x", "status": "preparing-resources" });
        let r: InferenceResponse = serde_json::from_value(body).expect("parse");
        assert_eq!(r.status, Status::PreparingResources);
    }

    #[test]
    fn serializes_request_omitting_absent_optional_fields() {
        // Both `system_prompt: None` and `resources: []` are skipped on the wire.
        let req = InferenceRequest {
            model: "gemma4".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resources: Vec::new(),
        };
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(v, serde_json::json!({ "model": "gemma4", "prompt": "hi" }));
    }

    #[test]
    fn serializes_request_with_resources() {
        let req = InferenceRequest {
            model: "gemma4".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resources: vec![Resource {
                filename: "payload.json".into(),
                content_type: "text/plain".into(),
                content_base64: "aGk=".into(),
            }],
        };
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(v["resources"][0]["filename"], "payload.json");
        assert_eq!(v["resources"][0]["content_type"], "text/plain");
        assert_eq!(v["resources"][0]["content_base64"], "aGk=");
    }

    #[test]
    fn deserializes_completed_with_resource_digests() {
        let body = serde_json::json!({
            "id": "abc", "status": "completed", "output": "x",
            "resources": [{
                "digest": "sha-orig",
                "request_digest": "sha-req",
                "response_digest": "sha-resp",
                "filename_digest": "sha-fn",
                "filename_blinding": "beef"
            }]
        });
        let r: InferenceResponse = serde_json::from_value(body).expect("parse");
        assert_eq!(r.resources.len(), 1);
        assert_eq!(r.resources[0].digest.as_deref(), Some("sha-orig"));
    }
}
