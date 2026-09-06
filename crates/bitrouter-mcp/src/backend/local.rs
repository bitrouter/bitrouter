//! `LocalBackend` — thin reqwest client against the local BYOK daemon
//! (`http://127.0.0.1:4356`), serving `list_models` off its `GET /v1/models`.
//! Pure HTTP: no control socket, no config, no dependency on `apps/bitrouter`
//! (which would be a cycle).

use std::sync::Arc;

use async_trait::async_trait;

use super::{Backend, BackendError, CallerAuth, ModelsEnvelope};
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
