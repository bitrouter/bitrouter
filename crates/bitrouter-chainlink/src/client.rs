//! Minimal HTTP client for the Chainlink Confidential Inference API:
//! `POST /v1/inference` then poll `GET /v1/inference/:id` until terminal.

use std::time::{Duration, Instant};

use bitrouter_sdk::{BitrouterError, Result};

use crate::wire::{InferenceRequest, InferenceResponse, Status};

/// Polling cadence + overall deadline for one inference.
#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    /// Delay between poll attempts.
    pub interval: Duration,
    /// Overall deadline before giving up.
    pub timeout: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(1000),
            timeout: Duration::from_secs(120),
        }
    }
}

/// A thin client bound to one base URL + bearer key.
pub struct ChainlinkClient {
    http: reqwest::Client,
    base: String,
    key: String,
    poll: PollConfig,
}

impl ChainlinkClient {
    /// Build a client. `base` is the api base (no trailing slash needed).
    pub fn new(
        http: reqwest::Client,
        base: impl Into<String>,
        key: impl Into<String>,
        poll: PollConfig,
    ) -> Self {
        Self {
            http,
            base: base.into(),
            key: key.into(),
            poll,
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Submit an inference request, returning the initial (`queued`) response.
    pub async fn submit(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
        let resp = self
            .http
            .post(self.url("v1/inference"))
            .bearer_auth(&self.key)
            .json(req)
            .send()
            .await
            .map_err(|e| BitrouterError::internal(format!("chainlink submit: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| BitrouterError::internal(format!("chainlink submit body: {e}")))?;
        if !status.is_success() {
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: text,
            });
        }
        serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
            status: 502,
            message: format!("chainlink submit: malformed response: {e}; body={text}"),
        })
    }

    /// Fetch the current snapshot for `id` (single GET, no polling). Used by the
    /// verifier to re-read the service-reported digests on demand.
    pub async fn fetch(&self, id: &str) -> Result<InferenceResponse> {
        let resp = self
            .http
            .get(self.url(&format!("v1/inference/{id}")))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|e| BitrouterError::internal(format!("chainlink fetch: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| BitrouterError::internal(format!("chainlink fetch body: {e}")))?;
        if !status.is_success() {
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: text,
            });
        }
        serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
            status: 502,
            message: format!("chainlink fetch: malformed response: {e}; body={text}"),
        })
    }

    /// Poll `id` until the job reaches a terminal status, then return the final
    /// response. Errors on `failed`, on a timeout, or on transport/parse errors.
    pub async fn poll_until_done(&self, id: &str) -> Result<InferenceResponse> {
        let started = Instant::now();
        loop {
            let resp = self
                .http
                .get(self.url(&format!("v1/inference/{id}")))
                .bearer_auth(&self.key)
                .send()
                .await
                .map_err(|e| BitrouterError::internal(format!("chainlink poll: {e}")))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| BitrouterError::internal(format!("chainlink poll body: {e}")))?;
            if !status.is_success() {
                return Err(BitrouterError::Upstream {
                    status: status.as_u16(),
                    message: text,
                });
            }
            let parsed: InferenceResponse =
                serde_json::from_str(&text).map_err(|e| BitrouterError::Upstream {
                    status: 502,
                    message: format!("chainlink poll: malformed response: {e}; body={text}"),
                })?;
            match parsed.status {
                Status::Completed => return Ok(parsed),
                Status::Failed => {
                    return Err(BitrouterError::Upstream {
                        status: 502,
                        message: parsed
                            .error
                            .unwrap_or_else(|| "chainlink inference failed".into()),
                    });
                }
                Status::Queued | Status::PreparingResources | Status::Processing => {
                    if started.elapsed() >= self.poll.timeout {
                        return Err(BitrouterError::UpstreamTimeout);
                    }
                    tokio::time::sleep(self.poll.interval).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fast_poll() -> PollConfig {
        PollConfig {
            interval: Duration::from_millis(5),
            timeout: Duration::from_secs(2),
        }
    }

    fn client(base: String) -> ChainlinkClient {
        ChainlinkClient::new(reqwest::Client::new(), base, "test-key", fast_poll())
    }

    #[tokio::test]
    async fn submit_returns_queued() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/inference"))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "id": "job-1", "status": "queued"
            })))
            .mount(&server)
            .await;
        let c = client(server.uri());
        let r = c
            .submit(&InferenceRequest {
                model: "gemma4".into(),
                prompt: "hi".into(),
                system_prompt: None,
                resources: Vec::new(),
            })
            .await
            .expect("submit");
        assert_eq!(r.id, "job-1");
        assert_eq!(r.status, Status::Queued);
    }

    #[tokio::test]
    async fn poll_transitions_processing_then_completed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "job-1", "status": "processing" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "job-1", "status": "completed", "output": "done",
                "usage": { "prompt_tokens": 3, "completion_tokens": 1 }
            })))
            .mount(&server)
            .await;
        let c = client(server.uri());
        let r = c.poll_until_done("job-1").await.expect("poll");
        assert_eq!(r.status, Status::Completed);
        assert_eq!(r.output.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn poll_failed_status_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "job-2", "status": "failed", "error": "enclave error"
            })))
            .mount(&server)
            .await;
        let c = client(server.uri());
        let err = c.poll_until_done("job-2").await.expect_err("should fail");
        match err {
            BitrouterError::Upstream { message, .. } => assert!(message.contains("enclave error")),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_times_out_when_never_completes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/job-3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "job-3", "status": "processing" })),
            )
            .mount(&server)
            .await;
        let c = ChainlinkClient::new(
            reqwest::Client::new(),
            server.uri(),
            "k",
            PollConfig {
                interval: Duration::from_millis(5),
                timeout: Duration::from_millis(30),
            },
        );
        let err = c
            .poll_until_done("job-3")
            .await
            .expect_err("should time out");
        assert!(matches!(err, BitrouterError::UpstreamTimeout));
    }
}
