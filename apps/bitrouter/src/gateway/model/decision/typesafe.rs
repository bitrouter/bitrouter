//! TypeSafe HTTP executor. Official API: <https://api.typesafe.ai/docs> and
//! <https://api.typesafe.ai/openapi.json> (`POST /v1/systemone`).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::DecisionExecutor;
use super::types::{
    DecisionError, DecisionFailure, DecisionRequest, DecisionResponse, DecisionUsage,
};

/// One TypeSafe connection. Debug deliberately does not expose the credential.
pub struct TypeSafeExecutor {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    authorization: http::HeaderValue,
    timeout: Duration,
    max_response_bytes: usize,
}

impl TypeSafeExecutor {
    /// Build an executor with redirects disabled and an end-to-end deadline.
    /// `base_url` is the API root, normally `https://api.typesafe.ai`.
    pub fn new(
        base_url: &str,
        api_key: &str,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, DecisionError> {
        let mut endpoint = reqwest::Url::parse(base_url)
            .map_err(|_| DecisionError::invalid_request("invalid TypeSafe API URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || api_key.trim().is_empty()
            || timeout.is_zero()
            || max_response_bytes == 0
        {
            return Err(DecisionError::invalid_request(
                "invalid TypeSafe connection settings",
            ));
        }
        let path = format!("{}/v1/systemone", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        let mut authorization = http::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| DecisionError::invalid_request("invalid TypeSafe credential"))?;
        authorization.set_sensitive(true);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| DecisionError::invalid_request("could not build TypeSafe HTTP client"))?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            timeout,
            max_response_bytes,
        })
    }

    async fn dispatch(&self, request: &DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(http::header::AUTHORIZATION, self.authorization.clone())
            .json(request)
            .send()
            .await
            .map_err(|_| failure(DecisionFailure::Transport, "HTTP dispatch failed", None))?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|size| size > self.max_response_bytes as u64)
        {
            return Err(failure(
                DecisionFailure::ResponseTooLarge,
                "response byte limit exceeded",
                None,
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failure(DecisionFailure::Transport, "response read failed", None))?
        {
            if chunk.len() > self.max_response_bytes.saturating_sub(body.len()) {
                return Err(failure(
                    DecisionFailure::ResponseTooLarge,
                    "response byte limit exceeded",
                    None,
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            failure(
                if status.is_success() {
                    DecisionFailure::InvalidResponse
                } else {
                    DecisionFailure::Http
                },
                "provider response is not valid JSON",
                None,
            )
        })?;
        let usage = value
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok());
        if !status.is_success() {
            return Err(failure(
                DecisionFailure::Http,
                &format!("HTTP {}", status.as_u16()),
                usage,
            ));
        }
        let decoded: DecisionResponse = serde_json::from_value(value).map_err(|_| {
            failure(
                DecisionFailure::InvalidResponse,
                "invalid typed response",
                usage,
            )
        })?;
        decoded.validate(request)?;
        Ok(decoded)
    }
}

#[async_trait]
impl DecisionExecutor for TypeSafeExecutor {
    async fn execute(
        &self,
        request: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError> {
        request.validate()?;
        if cancellation.is_cancelled() {
            return Err(DecisionError {
                kind: DecisionFailure::Cancelled,
                message: "cancelled before dispatch".into(),
                usage: None,
                may_have_run: false,
            });
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(failure(DecisionFailure::Cancelled, "attempt cancelled", None)),
            result = tokio::time::timeout(self.timeout, self.dispatch(request)) => {
                result.map_err(|_| failure(DecisionFailure::Timeout, "attempt deadline exceeded", None))?
            }
        }
    }
}

fn failure(kind: DecisionFailure, message: &str, usage: Option<DecisionUsage>) -> DecisionError {
    DecisionError {
        kind,
        message: message.into(),
        usage,
        may_have_run: true,
    }
}
