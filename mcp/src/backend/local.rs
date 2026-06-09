//! `LocalBackend` — thin reqwest client against the local BYOK daemon
//! (`http://127.0.0.1:4356`). Pure HTTP: no control socket, no config, no
//! dependency on `apps/bitrouter` (which would be a cycle).

use async_trait::async_trait;

use super::{
    Backend, BackendError, CallerAuth, CompleteRequest, CompleteResponse, ModelInfo,
    ModelsEnvelope, ProviderStatus, StatusInfo, Usage,
};

/// Routes tool calls to the local daemon's `/v1/*` HTTP API.
pub struct LocalBackend {
    base_url: String,
    http: reqwest::Client,
}

impl LocalBackend {
    /// `base_url` is the daemon root, e.g. `http://127.0.0.1:4356`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Backend for LocalBackend {
    async fn list_models(&self, _caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
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
        _caller: &CallerAuth,
        req: CompleteRequest,
    ) -> Result<CompleteResponse, BackendError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
        });
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = m.into();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = t.into();
        }
        if let Some(s) = req.system {
            // OpenAI's contract carries the system prompt as a leading
            // system-role message, not a top-level field (the daemon's
            // `ChatRequest` has no `system` slot; a top-level one would be
            // splatted onto the outbound provider request unchanged).
            if let Some(arr) = body["messages"].as_array_mut() {
                arr.insert(0, serde_json::json!({ "role": "system", "content": s }));
            }
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
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

    async fn status(&self, _caller: &CallerAuth) -> Result<StatusInfo, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
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

        let mut seen = std::collections::BTreeSet::new();
        let mut providers = Vec::new();
        for m in &env.data {
            for p in &m.providers {
                if seen.insert(p.clone()) {
                    providers.push(ProviderStatus { id: p.clone() });
                }
            }
        }
        Ok(StatusInfo::Local {
            listen: self.base_url.clone(),
            models: env.data.len(),
            providers,
        })
    }
}

/// Map a reqwest transport error to `DaemonUnreachable` when it is a connect
/// failure, else a generic transport error.
trait IfNot {
    fn if_not(self, e: reqwest::Error) -> BackendError;
}
impl IfNot for BackendError {
    fn if_not(self, e: reqwest::Error) -> BackendError {
        if e.is_connect() {
            self
        } else {
            BackendError::Transport(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_posts_full_openai_body_and_extracts_content() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        // Pin the *entire* outbound body: model, sampling params, and — crucially
        // — that `system` is prepended as a leading system-role message rather
        // than sent as a top-level field the daemon would ignore.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({
                "model": "openai/gpt-4o",
                "max_tokens": 64,
                "temperature": 0.5,
                "messages": [
                    { "role": "system", "content": "be terse" },
                    { "role": "user", "content": "hi" }
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [ { "message": { "content": "hi there" }, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 12, "completion_tokens": 5 }
            })))
            .mount(&server)
            .await;

        let backend = LocalBackend::new(server.uri());
        let out = backend
            .complete(
                &CallerAuth::default(),
                CompleteRequest {
                    model: "openai/gpt-4o".into(),
                    messages: vec![serde_json::json!({ "role": "user", "content": "hi" })],
                    max_tokens: Some(64),
                    temperature: Some(0.5),
                    system: Some("be terse".into()),
                },
            )
            .await
            .expect("complete");

        assert_eq!(out.content, "hi there");
        assert_eq!(out.finish_reason, "stop");
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 5
            }
        );
        assert_eq!(out.model, "openai/gpt-4o");
    }

    #[tokio::test]
    async fn status_maps_non_2xx_to_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;
        let backend = LocalBackend::new(server.uri());
        match backend.status(&CallerAuth::default()).await {
            Err(BackendError::Upstream { status, .. }) => assert_eq!(status, 500),
            other => panic!("expected Upstream 500, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_summarizes_models_and_distinct_providers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "openai/gpt-4o", "providers": ["openai"] },
                    { "id": "openai/gpt-4o-mini", "providers": ["openai"] },
                    { "id": "claude/sonnet", "providers": ["anthropic"] }
                ]
            })))
            .mount(&server)
            .await;

        let backend = LocalBackend::new(server.uri());
        match backend
            .status(&CallerAuth::default())
            .await
            .expect("status")
        {
            StatusInfo::Local {
                models,
                mut providers,
                ..
            } => {
                assert_eq!(models, 3);
                providers.sort_by(|a, b| a.id.cmp(&b.id));
                assert_eq!(
                    providers,
                    vec![
                        ProviderStatus {
                            id: "anthropic".into(),
                        },
                        ProviderStatus {
                            id: "openai".into(),
                        },
                    ]
                );
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_maps_data_to_modelinfo() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "openai/gpt-4o", "object": "model", "providers": ["openai"] },
                    { "id": "claude/sonnet",  "object": "model", "providers": ["anthropic"] }
                ]
            })))
            .mount(&server)
            .await;

        let backend = LocalBackend::new(server.uri());
        let models = backend
            .list_models(&CallerAuth::default())
            .await
            .expect("list_models");

        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "openai/gpt-4o".into(),
                    provider: "openai".into(),
                },
                ModelInfo {
                    id: "claude/sonnet".into(),
                    provider: "anthropic".into(),
                },
            ]
        );
    }
}
