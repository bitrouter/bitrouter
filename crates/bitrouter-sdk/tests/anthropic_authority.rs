use std::sync::{Arc, Mutex};

use bitrouter_ai::auth::AuthAppliers;
use bitrouter_ai::protocol::OutboundDispatch;
use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::builder::PipelineBuilder;
use bitrouter_sdk::language_model::executor::HttpExecutor;
use bitrouter_sdk::language_model::routing::StaticRoutingTable;
use bitrouter_sdk::language_model::settlement::{RequiredFinalizationContext, RequiredFinalizer};
use bitrouter_sdk::language_model::types::{OutboundHeaderRule, PipelineRequest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use async_trait::async_trait;
use bitrouter_ai::auth::credentials::Credential;
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, DEFAULT_ACCOUNT, MemoryCredentialStore,
};
use bitrouter_ai::auth::{AuthApplier, ContinuationAuthority, CredentialAuthority};
use bitrouter_ai::providers::anthropic::{AnthropicApiKeyApplier, PROVIDER_ID, headers};
use bitrouter_ai::types::AuthScheme;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Capture(Arc<Mutex<Vec<Option<ContinuationAuthority>>>>);

#[async_trait]
impl RequiredFinalizer for Capture {
    async fn finalize(&self, ctx: &RequiredFinalizationContext) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| BitrouterError::internal("fixture capture poisoned"))?
            .push(ctx.credential_authority.clone());
        Ok(())
    }
}

fn target(key: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: PROVIDER_ID.into(),
        service_id: "claude-opus-4-7".into(),
        api_base: "https://api.anthropic.com/v1".into(),
        api_key: key.into(),
        api_protocol: ApiProtocol::Messages,
        chat_google_extensions: false,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        // The applier selects x-api-key regardless of configured scheme.
        auth_scheme: AuthScheme::Bearer,
        headers: Vec::new(),
    }
}

fn expected(key: &str) -> ContinuationAuthority {
    ContinuationAuthority::new(
        CredentialAuthority::derive("anthropic/api-key", key),
        AuthScheme::XApiKey,
    )
}

async fn execute_and_capture(
    applier: Arc<AnthropicApiKeyApplier>,
    mut target: RoutingTarget,
    expected_wire_key: &str,
) -> TestResult<Option<ContinuationAuthority>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"msg_authority", "type":"message", "role":"assistant",
            "model":"claude-opus-4-7", "content":[{"type":"text","text":"ok"}],
            "stop_reason":"end_turn", "usage":{"input_tokens":4,"output_tokens":1}
        })))
        .expect(1)
        .mount(&server)
        .await;
    target.api_base = format!("{}/v1", server.uri());
    let table = StaticRoutingTable::new();
    table.insert("test-model", vec![target]);
    let captures = Arc::new(Mutex::new(Vec::new()));
    let executor = HttpExecutor::with_dispatch_and_auth(
        Default::default(),
        OutboundDispatch::builtin(),
        AuthAppliers::new().with(PROVIDER_ID, applier),
    )?;
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(table))
        .executor(Arc::new(executor))
        .required_finalizer(Capture(captures.clone()));
    let pipeline = builder.build()?;
    let response = pipeline
        .execute(PipelineRequest::new(
            "test-model",
            CallerContext::local(),
            Prompt {
                model: "test-model".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "hello")],
                tools: Vec::new(),
                params: GenerationParams::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            },
        ))
        .await?;
    assert_eq!(
        response.result.usage().map(|usage| usage.prompt_tokens),
        Some(4)
    );
    let requests = server
        .received_requests()
        .await
        .ok_or("requests unavailable")?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["x-api-key"], expected_wire_key);
    assert!(!requests[0].headers.contains_key("authorization"));
    assert_eq!(
        requests[0].headers["anthropic-version"],
        headers::ANTHROPIC_VERSION
    );
    let mut results = captures.lock().map_err(|_| "fixture capture poisoned")?;
    assert_eq!(results.len(), 1);
    results
        .pop()
        .ok_or_else(|| "missing authority capture".into())
}

#[tokio::test]
async fn api_key_authority_matches_serving_credential_and_actual_scheme() -> TestResult {
    let store = Arc::new(MemoryCredentialStore::default());
    let applier = Arc::new(AnthropicApiKeyApplier::new(store.clone()));
    let mut route = target("base-key");
    assert_eq!(
        execute_and_capture(applier.clone(), route.clone(), "base-key").await?,
        Some(expected("base-key"))
    );
    route.api_key_override = Some("override-key".into());
    assert_eq!(
        applier
            .continuation_authority_proof(&route.model_target())
            .await?,
        Some(expected("override-key"))
    );
    assert_eq!(
        execute_and_capture(applier.clone(), route.clone(), "override-key").await?,
        Some(expected("override-key"))
    );
    route.api_key_override = None;
    store_key(
        store.as_ref(),
        DEFAULT_ACCOUNT,
        Credential::api_key("stored-key"),
    )
    .await?;
    let before = applier
        .continuation_authority_proof(&route.model_target())
        .await?;
    assert_eq!(before, Some(expected("stored-key")));
    assert_eq!(
        execute_and_capture(applier.clone(), route.clone(), "stored-key").await?,
        before
    );
    // Rotate under the same label between planning and the real request. The
    // finalizer must observe the newly installed key, never the earlier proof.
    store_key(
        store.as_ref(),
        DEFAULT_ACCOUNT,
        Credential::api_key("rotated-key"),
    )
    .await?;
    let served = execute_and_capture(applier.clone(), route.clone(), "rotated-key").await?;
    assert_eq!(served, Some(expected("rotated-key")));
    assert_ne!(served, before);
    store_key(store.as_ref(), "other", Credential::api_key("other-key")).await?;
    route.account_label = Some("other".into());
    assert_eq!(
        execute_and_capture(applier, route, "other-key").await?,
        Some(expected("other-key"))
    );
    Ok(())
}

#[tokio::test]
async fn explicit_workspace_is_unverified_without_breaking_ordinary_requests() -> TestResult {
    let applier = Arc::new(AnthropicApiKeyApplier::new(Arc::new(
        MemoryCredentialStore::default(),
    )));
    let mut route = target("base-key");
    route.headers.push(OutboundHeaderRule::new(
        "anthropic-workspace-id",
        Some("workspace-one"),
        false,
    )?);
    assert_eq!(
        bitrouter_sdk::language_model::native_auth::continuation_authority_for_request(
            &AuthAppliers::new().with(PROVIDER_ID, applier.clone()),
            &route,
            None
        )
        .await?,
        None
    );
    assert_eq!(execute_and_capture(applier, route, "base-key").await?, None);
    Ok(())
}

async fn store_key(
    store: &dyn CredentialStore,
    account: &str,
    credential: Credential,
) -> TestResult {
    let mut transaction = store
        .begin(&CredentialKey {
            provider: PROVIDER_ID.into(),
            account: account.into(),
        })
        .await?;
    transaction.stage(credential);
    transaction.commit().await?;
    Ok(())
}
