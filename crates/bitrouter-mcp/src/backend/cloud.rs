//! `CloudBackend` — thin reqwest client against BitRouter Cloud
//! (`https://api.bitrouter.ai`) with a bearer token. v1 takes the token
//! explicitly; auto-reading the stored OAuth credential is v1.x.

use std::sync::Arc;

use async_trait::async_trait;

use super::{
    Backend, BackendError, CallerAuth, CompleteRequest, CompleteResponse, ModelInfo,
    ModelsEnvelope, Usage,
};
use crate::actions::status::{Spend, SpendLimit, StatusQuery, StatusReport};
use crate::error::ToolError;

/// Wire shape for `GET /v1/billing/balance`.
///
/// Field-for-field the same shape as the CLI's
/// `bitrouter::cloud::management::billing::BalanceResponse`. It is re-declared
/// here only because this crate must not depend on `apps/bitrouter` (that edge
/// would be a cycle).
#[derive(Debug, serde::Deserialize)]
struct BillingBalanceResponse {
    /// Raw balance from the credit account (before pending debits).
    balance_micro_usd: i64,
    /// Sum of pending debits not yet drained into the credit account.
    pending_debits_micro_usd: i64,
    /// `max(balance - pending, 0)` — what the next inference call will see.
    available_micro_usd: i64,
    /// Currency code (today: `"USD"`).
    currency: String,
}

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

    /// The cloud account's credits are exactly what this backend is positioned
    /// to answer — it holds the base URL and resolves the caller's own bearer —
    /// so it hands itself over as the `status` port.
    fn status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>> {
        Some(self)
    }
}

#[async_trait]
impl StatusQuery for CloudBackend {
    /// `GET /v1/billing/balance` with the **caller's** bearer, so a
    /// multi-tenant HTTP deployment reports each client's own credits.
    ///
    /// There is no process, listen address or control socket to report — the
    /// deployment is somebody else's — so reaching the account at all is the
    /// liveness answer.
    ///
    /// Fills only the [`SpendLimit`] half of the spend position. The balance
    /// endpoint is a ledger of what remains; it does not report spend-to-date,
    /// and this crate has no metering database of its own to read one from, so
    /// `spent` stays `None` rather than being invented.
    async fn status(&self, caller: &CallerAuth) -> Result<StatusReport, ToolError> {
        let bearer = self
            .resolve_bearer(caller)
            .map_err(|e| ToolError::new(e.to_string()))?;
        let url = format!("{}/v1/billing/balance", self.base_url);
        let resp = self
            .authed(bearer, self.http.get(&url))
            .send()
            .await
            .map_err(|e| ToolError::new(BackendError::Transport(e.to_string()).to_string()))?;
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
        let b: BillingBalanceResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::new(BackendError::Decode(e.to_string()).to_string()))?;
        Ok(StatusReport::metered(Spend {
            currency: b.currency,
            spent: None,
            limit: Some(SpendLimit {
                balance_micro_usd: b.balance_micro_usd,
                pending_micro_usd: b.pending_debits_micro_usd,
                remaining_micro_usd: b.available_micro_usd,
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn billing_balance_response_decodes_locally() -> anyhow::Result<()> {
        let response: BillingBalanceResponse = serde_json::from_value(serde_json::json!({
            "available_micro_usd": 11,
            "balance_micro_usd": 17,
            "pending_debits_micro_usd": 6,
            "currency": "USD"
        }))?;
        assert_eq!(response.available_micro_usd, 11);
        assert_eq!(response.balance_micro_usd, 17);
        assert_eq!(response.pending_debits_micro_usd, 6);
        assert_eq!(response.currency, "USD");
        Ok(())
    }

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
        let report = StatusQuery::status(&backend, &CallerAuth::default())
            .await
            .expect("status");
        assert!(report.running);
        assert_eq!(
            report.spend,
            Some(Spend {
                currency: "USD".into(),
                // A balance endpoint is a ledger of what remains; it knows
                // nothing of spend-to-date, and the cloud path must not
                // invent a figure for the half it cannot see.
                spent: None,
                limit: Some(SpendLimit {
                    balance_micro_usd: 5_000_000,
                    pending_micro_usd: 769_000,
                    remaining_micro_usd: 4_231_000,
                }),
            })
        );
    }

    #[tokio::test]
    async fn status_port_is_the_backend_itself() {
        let backend = Arc::new(CloudBackend::new(
            "https://api.bitrouter.ai",
            CloudAuth::PerCaller,
        ));
        assert!(Backend::status_port(backend).is_some());
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
