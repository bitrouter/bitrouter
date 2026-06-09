//! `CloudBackend` — thin reqwest client against BitRouter Cloud
//! (`https://api.bitrouter.ai`) with a bearer token. v1 takes the token
//! explicitly; auto-reading the stored OAuth credential is v1.x.

use async_trait::async_trait;
use bitrouter_cloud_sdk::management::billing::BalanceResponse;

use super::{
    Backend, BackendError, CallerAuth, CompleteRequest, CompleteResponse, ModelInfo,
    ModelsEnvelope, StatusInfo, Usage,
};

/// How a [`CloudBackend`] authenticates upstream.
pub enum CloudAuth {
    /// One configured token used for every call (stdio → cloud, single-tenant).
    Static(String),
    /// Every call must carry the caller's own bearer; no fallback (http multi-tenant).
    PerCaller,
}

pub struct CloudBackend {
    base_url: String,
    auth: CloudAuth,
    http: reqwest::Client,
}

impl CloudBackend {
    pub fn new(base_url: impl Into<String>, auth: CloudAuth) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            auth,
            http: reqwest::Client::new(),
        }
    }

    /// Resolve the bearer to send: the caller's wins; else the `Static` token;
    /// `PerCaller` with no caller bearer is a (middleware-prevented) error.
    fn resolve_bearer<'a>(&'a self, caller: &'a CallerAuth) -> Result<&'a str, BackendError> {
        match (&self.auth, caller.bearer.as_deref()) {
            (_, Some(b)) => Ok(b),
            (CloudAuth::Static(t), None) => Ok(t),
            (CloudAuth::PerCaller, None) => Err(BackendError::MissingCredential),
        }
    }

    fn authed(&self, bearer: &str, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(bearer)
    }
}

#[async_trait]
impl Backend for CloudBackend {
    async fn list_models(&self, caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> {
        let bearer = self.resolve_bearer(caller)?;
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .authed(bearer, self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(BackendError::Upstream {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let env: ModelsEnvelope = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(env
            .data
            .into_iter()
            .map(|m| ModelInfo {
                provider: m.providers.first().cloned().unwrap_or_default(),
                id: m.id,
            })
            .collect())
    }

    async fn complete(
        &self,
        caller: &CallerAuth,
        req: CompleteRequest,
    ) -> Result<CompleteResponse, BackendError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let mut body = serde_json::json!({ "model": req.model, "messages": req.messages });
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = m.into();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = t.into();
        }
        if let Some(s) = req.system {
            // OpenAI's contract carries the system prompt as a leading
            // system-role message, not a top-level field (see `LocalBackend`).
            if let Some(arr) = body["messages"].as_array_mut() {
                arr.insert(0, serde_json::json!({ "role": "system", "content": s }));
            }
        }
        let bearer = self.resolve_bearer(caller)?;
        let resp = self
            .authed(bearer, self.http.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(BackendError::Upstream {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        let choice = v
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| BackendError::Decode("no choices in response".into()))?;
        Ok(CompleteResponse {
            content: choice
                .pointer("/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_owned(),
            finish_reason: choice
                .get("finish_reason")
                .and_then(|f| f.as_str())
                .unwrap_or_default()
                .to_owned(),
            usage: Usage {
                input_tokens: v
                    .pointer("/usage/prompt_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                output_tokens: v
                    .pointer("/usage/completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
            },
            model: req.model,
        })
    }

    async fn status(&self, caller: &CallerAuth) -> Result<StatusInfo, BackendError> {
        let bearer = self.resolve_bearer(caller)?;
        let url = format!("{}/v1/billing/balance", self.base_url);
        let resp = self
            .authed(bearer, self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(BackendError::Upstream {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let b: BalanceResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(StatusInfo::Cloud {
            available_micro_usd: b.available_micro_usd,
            balance_micro_usd: b.balance_micro_usd,
            pending_micro_usd: b.pending_debits_micro_usd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn status_reads_billing_balance_with_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/billing/balance"))
            .and(header("authorization", "Bearer brk_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "balance_micro_usd": 5_000_000,
                "pending_debits_micro_usd": 769_000,
                "available_micro_usd": 4_231_000,
                "currency": "USD"
            })))
            .mount(&server)
            .await;

        let backend = CloudBackend::new(server.uri(), CloudAuth::Static("brk_test".into()));
        match backend
            .status(&CallerAuth::default())
            .await
            .expect("status")
        {
            StatusInfo::Cloud {
                available_micro_usd,
                balance_micro_usd,
                pending_micro_usd,
            } => {
                assert_eq!(available_micro_usd, 4_231_000);
                assert_eq!(balance_micro_usd, 5_000_000);
                assert_eq!(pending_micro_usd, 769_000);
            }
            other => panic!("expected Cloud, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_maps_non_2xx_to_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let backend = CloudBackend::new(server.uri(), CloudAuth::Static("brk_bad".into()));
        match backend.list_models(&CallerAuth::default()).await {
            Err(BackendError::Upstream { status, .. }) => assert_eq!(status, 401),
            other => panic!("expected Upstream 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_sends_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer brk_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [ { "id": "openai/gpt-4o", "providers": ["openai"] } ]
            })))
            .mount(&server)
            .await;

        let backend = CloudBackend::new(server.uri(), CloudAuth::Static("brk_test".into()));
        let models = backend
            .list_models(&CallerAuth::default())
            .await
            .expect("models");
        assert_eq!(
            models,
            vec![ModelInfo {
                id: "openai/gpt-4o".into(),
                provider: "openai".into(),
            }]
        );
    }

    #[tokio::test]
    async fn per_caller_without_bearer_errors() {
        let backend = CloudBackend::new("https://api.bitrouter.ai", CloudAuth::PerCaller);
        let err = backend
            .list_models(&CallerAuth::default())
            .await
            .expect_err("should error");
        assert!(matches!(err, BackendError::MissingCredential));
    }

    #[tokio::test]
    async fn caller_bearer_overrides_configured_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer caller-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": []
            })))
            .mount(&server)
            .await;
        let backend = CloudBackend::new(server.uri(), CloudAuth::Static("configured-tok".into()));
        let caller = CallerAuth {
            bearer: Some("caller-tok".into()),
        };
        backend.list_models(&caller).await.expect("list_models");
    }
}
