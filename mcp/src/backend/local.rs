//! `LocalBackend` — thin reqwest client against the local BYOK daemon
//! (`http://127.0.0.1:4356`). Pure HTTP: no control socket, no config, no
//! dependency on `apps/bitrouter` (which would be a cycle).

use async_trait::async_trait;

use super::{Backend, BackendError, CompleteRequest, CompleteResponse, ModelInfo, StatusInfo};

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

#[derive(serde::Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}

#[derive(serde::Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    providers: Vec<String>,
}

#[async_trait]
impl Backend for LocalBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable(self.base_url.clone()).if_not(e))?;
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
                active: true,
            })
            .collect())
    }

    async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
        unimplemented!("Task 4")
    }

    async fn status(&self) -> Result<StatusInfo, BackendError> {
        unimplemented!("Task 5")
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
        let models = backend.list_models().await.expect("list_models");

        assert_eq!(models, vec![
            ModelInfo { id: "openai/gpt-4o".into(), provider: "openai".into(), active: true },
            ModelInfo { id: "claude/sonnet".into(), provider: "anthropic".into(), active: true },
        ]);
    }
}
