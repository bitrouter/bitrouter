//! Request-scoped settlement receipts for inference credentials.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, InvalidHeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Terminal or transitional state reported for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementState {
    /// The request exists but authoritative settlement has not finished.
    Pending,
    /// Usage and final charge are authoritative.
    Computed,
    /// The request terminated before a billable provider result.
    NotCharged,
    /// The server cannot produce complete authoritative evidence.
    Unknown,
}

/// Non-overlapping token buckets from the authoritative receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementUsage {
    /// Input tokens that were neither cache reads nor cache writes.
    pub uncached_input_tokens: i64,
    /// Input tokens served from a provider cache.
    pub cache_read_tokens: i64,
    /// Input tokens written to a provider cache.
    pub cache_write_tokens: i64,
    /// Non-reasoning output tokens.
    pub output_tokens: i64,
    /// Reasoning output tokens.
    pub reasoning_tokens: i64,
}

/// Content-free authoritative settlement receipt for one inference request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementReceipt {
    /// Stable request identity shared by router and hosted gateway.
    pub request_id: String,
    /// Current settlement state.
    pub state: SettlementState,
    /// Canonical model id, when the request reached routing.
    pub model_id: Option<String>,
    /// Canonical provider id, when the request reached routing.
    pub provider_id: Option<String>,
    /// Authoritative non-overlapping token buckets.
    pub usage: SettlementUsage,
    /// Final charge in micro-USD. Present only for `computed`.
    pub final_charge_micro_usd: Option<i64>,
}

/// Failures returned by [`SettlementClient`].
#[derive(Debug, Error)]
pub enum SettlementError {
    /// The configured API root was not a valid URL.
    #[error("invalid settlement API root: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    /// The API root cannot accept hierarchical path segments.
    #[error("settlement API root cannot be used as a hierarchical URL")]
    CannotBeBase,
    /// The inference credential could not be represented as an HTTP header.
    #[error("invalid inference credential header")]
    InvalidApiKey(#[source] InvalidHeaderValue),
    /// The HTTP client could not be constructed.
    #[error("building settlement client: {0}")]
    BuildClient(reqwest::Error),
    /// The HTTP request failed before a response was received.
    #[error("settlement transport error: {0}")]
    Transport(reqwest::Error),
    /// The server returned a non-success status.
    #[error("settlement server returned HTTP {status}: {message}")]
    Http {
        /// Raw HTTP status.
        status: u16,
        /// Content-free server error description.
        message: String,
    },
    /// The response did not match the receipt schema.
    #[error("decoding settlement receipt: {0}")]
    Decode(serde_json::Error),
    /// The response identity differed from the requested identity.
    #[error("settlement receipt identity mismatch")]
    IdentityMismatch,
    /// The response combined an invalid state, charge, or usage value.
    #[error("invalid settlement receipt: {0}")]
    InvalidReceipt(&'static str),
}

impl SettlementError {
    /// Whether this error represents an absent or credential-invisible request.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Http { status: 404, .. })
    }
}

/// Convenience result returned by settlement operations.
pub type Result<T> = std::result::Result<T, SettlementError>;

/// API-key scoped client for request settlement receipts.
pub struct SettlementClient {
    base_url: url::Url,
    api_key: HeaderValue,
    http: reqwest::Client,
}

impl fmt::Debug for SettlementClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettlementClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl SettlementClient {
    /// Construct a client from an API root ending in `/v1` and an inference key.
    pub fn new(base_url: impl AsRef<str>, api_key: impl AsRef<str>) -> Result<Self> {
        let mut base_url = url::Url::parse(base_url.as_ref())?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        let mut api_key =
            HeaderValue::from_str(api_key.as_ref()).map_err(SettlementError::InvalidApiKey)?;
        api_key.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(SettlementError::BuildClient)?;
        Ok(Self {
            base_url,
            api_key,
            http,
        })
    }

    /// Fetch the exact receipt visible to this inference credential.
    pub async fn get(&self, request_id: &str) -> Result<SettlementReceipt> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| SettlementError::CannotBeBase)?;
            segments.pop_if_empty();
            segments.push("requests");
            segments.push(request_id);
            segments.push("settlement");
        }
        let response = self
            .http
            .get(url)
            .header("x-api-key", self.api_key.clone())
            .send()
            .await
            .map_err(SettlementError::Transport)?;
        let status = response.status();
        let body = response.bytes().await.map_err(SettlementError::Transport)?;
        if !status.is_success() {
            #[derive(Deserialize)]
            struct ErrorBody {
                error_description: String,
            }
            let message = serde_json::from_slice::<ErrorBody>(&body)
                .map(|error| error.error_description)
                .unwrap_or_else(|_| "request failed".to_owned());
            return Err(SettlementError::Http {
                status: status.as_u16(),
                message,
            });
        }
        let receipt: SettlementReceipt =
            serde_json::from_slice(&body).map_err(SettlementError::Decode)?;
        validate_receipt(request_id, &receipt)?;
        Ok(receipt)
    }
}

fn validate_receipt(request_id: &str, receipt: &SettlementReceipt) -> Result<()> {
    if receipt.request_id != request_id {
        return Err(SettlementError::IdentityMismatch);
    }
    let usage = &receipt.usage;
    if [
        usage.uncached_input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        usage.reasoning_tokens,
    ]
    .into_iter()
    .any(|tokens| tokens < 0)
    {
        return Err(SettlementError::InvalidReceipt("negative token bucket"));
    }
    match (receipt.state, receipt.final_charge_micro_usd) {
        (SettlementState::Computed, Some(charge)) if charge >= 0 => Ok(()),
        (SettlementState::Computed, _) => Err(SettlementError::InvalidReceipt(
            "computed receipt needs a nonnegative final charge",
        )),
        (
            SettlementState::Pending | SettlementState::NotCharged | SettlementState::Unknown,
            None,
        ) => Ok(()),
        _ => Err(SettlementError::InvalidReceipt(
            "non-computed receipt cannot carry a final charge",
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{SettlementClient, SettlementState};

    #[tokio::test]
    async fn fetches_exact_request_with_inference_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/requests/req-123/settlement"))
            .and(header("x-api-key", "brk_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "request_id": "req-123",
                "state": "computed",
                "model_id": "model-a",
                "provider_id": "provider-a",
                "usage": {
                    "uncached_input_tokens": 2,
                    "cache_read_tokens": 3,
                    "cache_write_tokens": 5,
                    "output_tokens": 7,
                    "reasoning_tokens": 11
                },
                "final_charge_micro_usd": 29
            })))
            .mount(&server)
            .await;

        let client =
            SettlementClient::new(format!("{}/v1", server.uri()), "brk_test").expect("client");
        let receipt = client.get("req-123").await.expect("receipt");

        assert_eq!(receipt.request_id, "req-123");
        assert_eq!(receipt.state, SettlementState::Computed);
        assert_eq!(receipt.usage.uncached_input_tokens, 2);
        assert_eq!(receipt.usage.cache_read_tokens, 3);
        assert_eq!(receipt.usage.cache_write_tokens, 5);
        assert_eq!(receipt.usage.output_tokens, 7);
        assert_eq!(receipt.usage.reasoning_tokens, 11);
        assert_eq!(receipt.final_charge_micro_usd, Some(29));
    }

    #[tokio::test]
    async fn percent_encodes_request_id_as_one_path_segment() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/requests/case%2Fhop/settlement"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": "not_found",
                "error_description": "request not found"
            })))
            .mount(&server)
            .await;

        let client =
            SettlementClient::new(format!("{}/v1", server.uri()), "brk_test").expect("client");
        let error = client.get("case/hop").await.expect_err("not found");

        assert!(error.is_not_found());
    }

    #[test]
    fn client_debug_never_exposes_api_key() {
        let client = SettlementClient::new("https://example.com/v1", "brk_secret").expect("client");

        let rendered = format!("{client:?}");

        assert!(!rendered.contains("brk_secret"));
    }
}
