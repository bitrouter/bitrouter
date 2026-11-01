//! A few real mock-provider sends on the same retained database, not seeded successes.
use super::*;
use crate::tenancy::receipts::store::{self as receipts, Candidate};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKey, ResourceKind},
    configuration::ConfigurationRevision,
    execution::{ExecutionReceipt, Outcome},
    identity::ExecutionId,
    resources::{CredentialBinding, Definition, ResourcePut},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header_exists, method, path},
};

const MODEL: &str = "history-serving-route";

pub(super) struct Serving {
    provider: MockServer,
    revision: ConfigurationRevision,
    token: String,
}

impl Serving {
    pub async fn new(h: &Harness, credential: &ResourceRef) -> Result<Self> {
        let provider = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"chatcmpl-history", "object":"chat.completion", "created":0,
                "model":"model-a", "choices":[{"index":0,"message":{"role":"assistant","content":"history reply"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,
                    "prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},
                    "completion_tokens_details":{"reasoning_tokens":4}}
            }))).mount(&provider).await;
        let input = ResourcePut {
            format: 1,
            operation_id: "history-serving-resource".try_into()?,
            installation_id: h.fixture.installation.clone(),
            key: ResourceKey {
                owner_kind: OwnerKind::Tenant,
                owner_id: h.fixture.tenant.as_str().try_into()?,
                resource_kind: ResourceKind::Route,
                resource_id: MODEL.try_into()?,
            },
            expected_revision: None,
            definition: Definition::Route {
                routing: serde_json::json!({"providers":{"mock":{
                    "api_base":format!("{}/v1",provider.uri()),
                    "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]
                }}}),
                selector: "mock:model-a".into(),
                policy: None,
                credentials: vec![CredentialBinding {
                    provider: "mock".into(),
                    account: "default".into(),
                    credential: credential.clone(),
                }],
            },
        };
        let applied: OperationResult = h
            .api(
                Method::POST,
                &format!("/control/v1/tenants/{}/resources", h.fixture.tenant),
                &input,
            )
            .await?;
        ensure!(applied.status == OperationStatus::Applied);
        let mut configuration = h.fixture.configuration_input("history-serving-config")?;
        configuration.document.routes = vec![ResourceRef {
            owner_kind: input.key.owner_kind,
            owner_id: input.key.owner_id,
            resource_kind: input.key.resource_kind,
            resource_id: input.key.resource_id,
            revision_id: applied.resource_revision.context("route revision")?,
        }];
        let revision = match h
            .fixture
            .service
            .execute(
                h.fixture.operator.clone(),
                Command::Configuration(h.fixture.tenant.clone(), configuration),
            )
            .await
            .map_err(failure)?
        {
            StoredResponse::Configuration(revision) => revision,
            _ => anyhow::bail!("configuration response"),
        };
        let token = h
            .fixture
            .issue("history-serving-credential")
            .await?
            .secret
            .context("serving token")?;
        Ok(Self {
            provider,
            revision,
            token,
        })
    }

    pub async fn send(&self, h: &Harness) -> Result<Candidate> {
        let before = self
            .provider
            .received_requests()
            .await
            .context("provider capture")?
            .len();
        h.fixture
            .service
            .dispatch_test_ready
            .store(true, std::sync::atomic::Ordering::Release);
        let state = h.fixture.service.current().map_err(failure)?;
        let binding = h
            .fixture
            .service
            .capability_document(&state)
            .map_err(|code| anyhow::anyhow!("capability: {code:?}"))?
            .bind(
                self.revision.revision_id.clone(),
                "history-served".try_into()?,
            )?;
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json");
        let binding = serde_json::to_value(binding)?;
        for (name, value) in binding.as_object().context("binding headers")? {
            request = request.header(name.as_str(), value.as_str().context("binding header")?);
        }
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(|| async { StatusCode::NOT_FOUND }),
            )
            .layer(axum::middleware::from_fn_with_state(
                h.fixture.service.clone(),
                tenancy::http::dispatch,
            ));
        let response = router
            .oneshot(
                request.body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false
                }))?))?,
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "actual provider dispatch: {}",
            response.status()
        );
        let id: ExecutionId = response
            .headers()
            .get("x-bitrouter-request-id")
            .context("actual execution identity")?
            .to_str()?
            .try_into()?;
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        ensure!(body["choices"][0]["message"]["content"] == "history reply");
        let requests = self
            .provider
            .received_requests()
            .await
            .context("provider capture")?;
        ensure!(requests.len() == before + 1);
        ensure!(
            requests
                .last()
                .context("provider request")?
                .body_json::<serde_json::Value>()?["model"]
                == "model-a"
        );
        let (status, bytes) = h
            .wire(
                Method::GET,
                &format!("/control/v1/tenants/{}/executions/{id}", h.fixture.tenant),
                &self.token,
                &(),
            )
            .await?;
        ensure!(status == StatusCode::OK);
        let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.execution_id == id
                && receipt.outcome == Outcome::Completed
                && receipt.attempts.len() == 1
        );
        ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
        ensure!(receipt.execution_revision == self.revision.revision_id);
        receipts::lookup_candidate(
            &h.fixture.db,
            &h.fixture.installation,
            &h.fixture.tenant,
            &id,
        )
        .await?
        .context("settled actual receipt")
    }
}
