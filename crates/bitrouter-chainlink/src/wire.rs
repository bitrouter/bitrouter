//! Serde types for the Chainlink `/v1/inference` request and response envelope.
//! Shapes mirror the dev-preview API docs (submit returns 202 with an id +
//! status; polling returns the same object, `completed` carrying `output`).

use serde::{Deserialize, Serialize};

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

/// Integrity summary for one processed resource.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResourceSummary {
    /// Original filename, when present.
    #[serde(default)]
    pub filename: Option<String>,
    /// SHA-256 of the original content.
    #[serde(default)]
    pub digest: Option<String>,
    /// SHA-256 of the canonical response metadata.
    #[serde(default)]
    pub response_digest: Option<String>,
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
    /// Per-resource integrity summaries (digests) when resources were used.
    #[serde(default)]
    pub resource_summaries: Vec<ResourceSummary>,
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
    fn serializes_request_omitting_absent_system_prompt() {
        let req = InferenceRequest {
            model: "gemma4".into(),
            prompt: "hi".into(),
            system_prompt: None,
        };
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(v, serde_json::json!({ "model": "gemma4", "prompt": "hi" }));
    }
}
