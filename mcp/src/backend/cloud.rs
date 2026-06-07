//! `CloudBackend` — thin reqwest client against BitRouter Cloud
//! (`https://api.bitrouter.ai`) with a bearer token. v1 takes the token
//! explicitly; auto-reading the stored OAuth credential is v1.x.

use async_trait::async_trait;

use super::{
    Backend, BackendError, CompleteRequest, CompleteResponse, ModelInfo, StatusInfo, Usage,
};

pub struct CloudBackend {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl CloudBackend {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    fn bearer(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.token)
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
#[derive(serde::Deserialize)]
struct Balance {
    balance_micro_usd: i64,
    pending_micro_usd: i64,
    available_micro_usd: i64,
}

#[async_trait]
impl Backend for CloudBackend {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .bearer(self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
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

    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, BackendError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let mut body = serde_json::json!({ "model": req.model, "messages": req.messages });
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = m.into();
        }
        if let Some(t) = req.temperature {
            body["temperature"] = t.into();
        }
        if let Some(s) = req.system {
            body["system"] = s.into();
        }
        let resp = self
            .bearer(self.http.post(&url).json(&body))
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

    async fn status(&self) -> Result<StatusInfo, BackendError> {
        let url = format!("{}/v1/billing/balance", self.base_url);
        let resp = self
            .bearer(self.http.get(&url))
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let b: Balance = resp
            .json()
            .await
            .map_err(|e| BackendError::Decode(e.to_string()))?;
        Ok(StatusInfo::Cloud {
            available_micro_usd: b.available_micro_usd,
            balance_micro_usd: b.balance_micro_usd,
            pending_micro_usd: b.pending_micro_usd,
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
                "pending_micro_usd": 769_000,
                "available_micro_usd": 4_231_000
            })))
            .mount(&server)
            .await;

        let backend = CloudBackend::new(server.uri(), "brk_test");
        match backend.status().await.expect("status") {
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

        let backend = CloudBackend::new(server.uri(), "brk_test");
        let models = backend.list_models().await.expect("models");
        assert_eq!(
            models,
            vec![ModelInfo {
                id: "openai/gpt-4o".into(),
                provider: "openai".into(),
                active: true,
            }]
        );
    }
}
