//! `/v1/namespaces/{nsid}/usage` and `…/requests` — aggregate spend /
//! token counts and the request history for the client's namespace. Both
//! require `usage:read`.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::usage`. The `{nsid}`
//! segment is resolved from the credential's baked namespace.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// Query string for `GET /v1/usage`. Both bounds are optional — the
/// server defaults to a 30-day rolling window when neither is set.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageQuery {
    /// Lower bound (inclusive). Defaults to `to - 30 days`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// Upper bound (exclusive). Defaults to `now`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
}

/// Wire shape for `GET /v1/usage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageResponse {
    /// Effective lower bound used (echoed for clarity when defaulted).
    pub from: DateTime<Utc>,
    /// Effective upper bound used.
    pub to: DateTime<Utc>,
    /// Total spend in micro-USD over the window.
    pub spend_micro_usd: i64,
    /// Prompt-token count over the window.
    pub prompt_tokens: i64,
    /// Completion-token count over the window.
    pub completion_tokens: i64,
    /// Number of admitted requests in the window.
    pub request_count: i64,
}

/// Query string for `GET /v1/requests` — paged list of recent requests.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestsQuery {
    /// Page size (server clamps to `[1, 100]`; defaults to 25).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Offset into the result set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// Wire shape for `GET /v1/requests`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestsResponse {
    /// One envelope per request in this page.
    pub data: Vec<RequestEnvelope>,
    /// Effective page size used.
    pub limit: u64,
    /// Effective offset used.
    pub offset: u64,
}

/// One row of the `requests` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Internal row id.
    pub id: String,
    /// `x-request-id` value seen on the wire (often a uuid).
    pub request_id: String,
    /// `pending` / `succeeded` / `failed` / `streaming` …
    pub status: String,
    /// Canonical model id (e.g. `claude-opus-4-7`).
    #[serde(default)]
    pub model_id: Option<String>,
    /// Upstream provider id (e.g. `anthropic`).
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Prompt-token count for this single request.
    pub prompt_tokens: i64,
    /// Completion-token count.
    pub completion_tokens: i64,
    /// Final settled charge in micro-USD; absent until settlement
    /// completes.
    #[serde(default)]
    pub final_charge_micro_usd: Option<i64>,
    /// `cloud_credit` / `byok` / etc.
    #[serde(default)]
    pub funding_source: Option<String>,
    /// `Some(true)` when the response was served as a stream.
    #[serde(default)]
    pub streamed: Option<bool>,
    /// When the request was admitted.
    pub created_at: DateTime<Utc>,
    /// When the request finished (success or failure).
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl ManagementClient {
    /// `GET /v1/namespaces/{nsid}/usage` — aggregate spend + token
    /// counts over a window.
    pub async fn usage_aggregate(&self, query: &UsageQuery) -> Result<UsageResponse> {
        let path = self.namespaced("/usage")?;
        self.get_with_query(&path, query).await
    }

    /// `GET /v1/namespaces/{nsid}/requests` — paged list of recent
    /// requests.
    pub async fn list_requests(&self, query: &RequestsQuery) -> Result<RequestsResponse> {
        let path = self.namespaced("/requests")?;
        self.get_with_query(&path, query).await
    }
}
