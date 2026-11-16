//! SDK management and actual provider dispatch share the same policy revision.

use super::*;
use axum::{Router, http::StatusCode, routing::post};
use bitrouter_ai::{
    control::ControlScope,
    tenancy::{
        configuration::Activation,
        resources::{CredentialBinding, Definition, ResourcePut},
    },
};
use bitrouter_sdk::tenancy::TenancyClient;
use tokio_util::task::AbortOnDropHandle;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

#[tokio::test]
async fn sdk_policy_submit_read_activate_and_dispatch_use_the_public_contract() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model":"model-b", "messages":[{"role":"user","content":"preserve this complete request"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"policy-test", "object":"chat.completion","created":0,"model":"model-b","choices":[{"index":0,"message":{"role":"assistant","content":"selected policy model"},"finish_reason":"stop"}]})))
        .expect(1).mount(&upstream).await;
    let (fixture, _, slot) = provision(&format!("{}/v1", upstream.uri())).await?;
    const TOKEN: &str = "model-policy-contract-operator-token-0123456789";
    let operators = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "POLICY_CONTRACT_TEST_OPERATOR".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(TOKEN.into()),
    )?;
    let inference = Router::new()
        .route(
            "/v1/chat/completions",
            post(|| async { StatusCode::NOT_FOUND }),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let router = crate::tenancy::http::router(fixture.service.clone(), operators).merge(inference);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let checked: Result<()> = async {
        let client = TenancyClient::new(&endpoint, TOKEN)?;
        let capabilities = client.policy_capabilities(&fixture.installation).await?;
        ensure!(capabilities.provisioning_enabled);
        let source = json!({"spec_version":"policy/1.0.0-beta.1", "policies":{"auto":{"tiers":{"economy":{"model":"mock:model-b"}}, "default_tier":"economy"}}});
        let mut key = slot.key();
        key.resource_kind = ResourceKind::Policy;
        key.resource_id = "model-policy".try_into()?;
        let policy = ResourcePut { format:1, operation_id:"policy-put".try_into()?, installation_id:fixture.installation.clone(), key:key.clone(), expected_revision:None,
            definition:Definition::Policy { document:source.clone(), name:"auto".into(), dependencies:Vec::new() } };
        let applied = client.put_resource(&fixture.tenant, &policy).await?;
        let policy_ref = ResourceRef { owner_kind:key.owner_kind, owner_id:key.owner_id, resource_kind:key.resource_kind, resource_id:key.resource_id, revision_id:applied.resource_revision.context("policy revision")? };
        let read = client.policy_source(&fixture.installation, &fixture.tenant, &policy_ref).await?;
        ensure!(serde_json::to_value(read.document)? == source);
        let mut key = slot.key();
        key.resource_kind = ResourceKind::Route;
        key.resource_id = "auto".try_into()?;
        let route = ResourcePut { format:1, operation_id:"route-put".try_into()?, installation_id:fixture.installation.clone(), key:key.clone(), expected_revision:None,
            definition:Definition::Route {
                routing:json!({"providers":{"mock":{"api_base":format!("{}/v1", upstream.uri()),"models":[{"id":"model-a"},{"id":"model-b"}]}},"routers":{"auto":{"selection":{"kind":"policy","policy":"auto","base_model":"mock:model-a"}}}}),
                selector:"bitrouter/auto".into(), policy:Some(policy_ref.clone()), credentials:vec![CredentialBinding { provider:"mock".into(),account:"default".into(),credential:slot }]
            } };
        let applied = client.put_resource(&fixture.tenant, &route).await?;
        let route_ref = ResourceRef { owner_kind:key.owner_kind, owner_id:key.owner_id, resource_kind:key.resource_kind, resource_id:key.resource_id, revision_id:applied.resource_revision.context("route revision")? };
        let mut input = fixture.configuration_input("policy-configuration")?;
        input.document.routes = vec![route_ref];
        input.document.policy = Some(policy_ref);
        let revision = client.put_configuration(&fixture.tenant, &input).await?;
        client.activate(&fixture.tenant, &Activation { operation_id:"policy-activate".try_into()?, expected_active_revision:None, revision_id:revision.revision_id.clone(), expected_authority_revision:fixture.service.current().map_err(failure)?.revision, installation_id:fixture.installation.clone() }).await?;
        let key = fixture.issue("policy-execution-key").await?;
        fixture.service.dispatch_test_ready.store(true, Ordering::Release);
        let headers = headers(&fixture, key.secret.as_deref().context("issued key")?, &revision.revision_id)?;
        let response = reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(10)).build()?
            .post(format!("{endpoint}/v1/chat/completions")).headers(headers)
            .json(&json!({"model":"auto", "messages":[{"role":"user","content":"preserve this complete request"}]})).send().await?;
        ensure!(response.status().is_success(), "unexpected dispatch status {}", response.status());
        ensure!(response.text().await?.contains("selected policy model"));
        fixture.service.pipelines.close_and_drain().await?;
        Ok(())
    }.await;
    server.abort();
    let server_result = server.await;
    let shutdown = fixture.service.shutdown().await;
    checked?;
    ensure!(
        server_result.is_err_and(|error| error.is_cancelled()),
        "HTTP fixture did not stop by cancellation"
    );
    shutdown?;
    Ok(())
}
