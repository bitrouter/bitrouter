//! `LocalBackend` — thin reqwest client against the local BYOK daemon
//! (`http://127.0.0.1:4356`). Pure HTTP: no control socket, no config, no
//! dependency on `apps/bitrouter` (which would be a cycle).

use std::sync::Arc;

use async_trait::async_trait;

use super::{
    Backend, BackendError, CallerAuth, CompleteRequest, CompleteResponse, ModelsEnvelope, Usage,
};
use crate::actions::models::{ModelsQuery, ModelsReport};
use crate::actions::status::StatusQuery;
use crate::error::ToolError;

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

    /// `None`: a local daemon's liveness is a control-socket question, and
    /// this backend only speaks `/v1/*` HTTP. The embedding binary injects the
    /// real `status` port (see `bitrouter::actions::status`); a `GET /v1/models`
    /// standing in for a health check reported neither pid nor socket and could
    /// not distinguish "stopped" from "broken".
    fn status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>> {
        None
    }

    /// `Some(self)`: `GET /v1/models` on the daemon *is* the live catalog, and
    /// a `--local-url` client with no filesystem access to the daemon's config
    /// has no better source. The embedding binary overrides it on stdio + local
    /// with a port that also answers when the daemon is down.
    fn models_port(self: Arc<Self>) -> Option<Arc<dyn ModelsQuery>> {
        Some(self)
    }
}

#[async_trait]
impl ModelsQuery for LocalBackend {
    /// The daemon's own `/v1/models`, so the list is what it will actually
    /// route — [`ModelsSource::Live`](crate::actions::models::ModelsSource::Live).
    ///
    /// Every provider per model is kept. This path used to collapse the list to
    /// `providers.first()`, which silently dropped each model's fallback chain.
    async fn list_models(&self, _caller: &CallerAuth) -> Result<ModelsReport, ToolError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))
            .map_err(|e| ToolError::new(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ToolError::new(
                BackendError::Upstream {
                    status: status.as_u16(),
                    body: resp.text().await.unwrap_or_default(),
                }
                .to_string(),
            ));
        }
        let env: ModelsEnvelope = resp
            .json()
            .await
            .map_err(|e| ToolError::new(BackendError::Decode(e.to_string()).to_string()))?;
        Ok(ModelsReport::live(env.into_models()))
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
    use crate::actions::models::ModelsSource;
    use bitrouter_sdk::language_model::routing::ModelInfo;
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

    /// The drift phase 2 exists to fix: a model served by three providers must
    /// arrive with all three. This path used to keep `providers.first()`, so an
    /// agent asking what could serve `openai/gpt-4o` was told one answer where
    /// there were three, and the fallback chain was invisible.
    #[tokio::test]
    async fn list_models_keeps_every_provider_per_model() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "id": "openai/gpt-4o", "object": "model",
                      "providers": ["openai", "azure", "openrouter"] },
                    { "id": "claude/sonnet",  "object": "model", "providers": ["anthropic"] }
                ]
            })))
            .mount(&server)
            .await;

        let backend = LocalBackend::new(server.uri());
        let report = ModelsQuery::list_models(&backend, &CallerAuth::default())
            .await
            .expect("list_models");

        assert_eq!(
            report,
            ModelsReport::live(vec![
                ModelInfo {
                    id: "openai/gpt-4o".into(),
                    providers: vec!["openai".into(), "azure".into(), "openrouter".into()],
                },
                ModelInfo {
                    id: "claude/sonnet".into(),
                    providers: vec!["anthropic".into()],
                },
            ])
        );
    }

    /// The daemon answered, so the catalog is what it will actually route.
    #[tokio::test]
    async fn a_daemon_answer_is_a_live_catalog() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": []
            })))
            .mount(&server)
            .await;
        let backend = LocalBackend::new(server.uri());
        let report = ModelsQuery::list_models(&backend, &CallerAuth::default())
            .await
            .expect("list_models");
        assert_eq!(report.resolved_via, ModelsSource::Live);
    }

    /// A backend with no daemon is a tool error, not an empty catalog: "no
    /// models" and "nothing answered" are different answers.
    #[tokio::test]
    async fn an_unreachable_daemon_is_an_error_not_an_empty_list() {
        // Port 1 on loopback: nothing binds it, so the connect fails fast.
        let backend = LocalBackend::new("http://127.0.0.1:1");
        let err = ModelsQuery::list_models(&backend, &CallerAuth::default())
            .await
            .expect_err("an unreachable daemon must not read as an empty catalog");
        assert!(err.to_string().contains("bitrouter start"), "{err}");
    }
}
