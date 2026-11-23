//! Ordinary provider YAML selects exactly one credential header on real sends.

use super::*;

#[derive(Clone, Copy)]
enum Declaration {
    Default,
    BearerAccount,
    InheritedBearer,
    OverrideToApiKey,
}

struct MessagesProvider {
    url: String,
    received: tokio::sync::mpsc::Receiver<(HeaderMap, Value)>,
    _task: AbortOnDropHandle<std::io::Result<()>>,
}

impl MessagesProvider {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
        let (sent, received) = tokio::sync::mpsc::channel::<(HeaderMap, Value)>(4);
        let router = Router::new()
            .route("/v1/messages", post(
                |State(sent): State<tokio::sync::mpsc::Sender<(HeaderMap, Value)>>,
                 headers: HeaderMap, Json(body): Json<Value>| async move {
                    if sent.try_send((headers, body)).is_err() {
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"unexpected extra send"})));
                    }
                    (StatusCode::OK, Json(json!({
                        "id":"msg-configured-auth", "type":"message", "role":"assistant",
                        "model":"native-shared", "content":[{"type":"text","text":"auth reply"}],
                        "stop_reason":"end_turn", "stop_sequence":null,
                        "usage":{"input_tokens":2,"output_tokens":1}
                    })))
                },
            ))
            .with_state(sent);
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            received,
            _task: task,
        })
    }
}

fn provider_yaml(url: &str, secret: &str, declaration: Declaration) -> (String, &'static str) {
    let common = format!(
        r#"
    api_base: {url}
    api_protocol: [{{"*": messages}}]
    models: [{{id: acme/shared, provider_model_id: native-shared}}]
"#
    );
    match declaration {
        Declaration::Default => (
            format!("  mock:\n{common}    api_key: {secret:?}\n"),
            "default",
        ),
        Declaration::BearerAccount => (
            format!(
                "  mock:\n{common}    auth_scheme: bearer\n    accounts:\n      - {{label: primary, api_key: {secret:?}}}\n"
            ),
            "primary",
        ),
        Declaration::InheritedBearer | Declaration::OverrideToApiKey => {
            let parent_secret = uuid::Uuid::new_v4().to_string();
            let explicit = if matches!(declaration, Declaration::OverrideToApiKey) {
                "    auth_scheme: x-api-key\n"
            } else {
                ""
            };
            (
                format!(
                    "  base:\n{common}    api_key: {parent_secret:?}\n    auth_scheme: bearer\n  mock:\n    derives: base\n    api_base: {url}\n    api_key: {secret:?}\n{explicit}"
                ),
                "default",
            )
        }
    }
}

async fn actual_messages_auth(declaration: Declaration) -> Result<()> {
    let mut provider = MessagesProvider::start().await?;
    let mut harness = Harness::new(&provider.url).await?;
    let secret = uuid::Uuid::new_v4().to_string();
    let (providers, account) = provider_yaml(&provider.url, &secret, declaration);
    harness.config = crate::config::parse(&format!(
        r#"
inherit_defaults: false
server:
  skip_auth: false
control:
  enabled: true
  credentials:
    - id: catalog-operator
      token_env: CONFIGURED_CATALOG_OPERATOR_TOKEN
      scopes: [control:tenancy]
providers:
{providers}
models:
  acme/shared:
    endpoints: [{{provider: mock, service_id: native-shared}}]
tenancy:
  enabled: true
  wrapping_key_env: CONFIGURED_CATALOG_WRAPPING_KEY
  catalog:
    pool_id: platform
    routes: {{{ROUTE}: acme/shared}}
    profiles:
      mock/{account}/acme/shared:
        schema_version: 1
        rate: null
        health: null
        health_group_id: null
        fallback: stop
"#
    ))?;
    if matches!(
        declaration,
        Declaration::InheritedBearer | Declaration::OverrideToApiKey
    ) {
        // Derivation inherits the scheme, not the endpoint or child key.
        let child = harness
            .config
            .providers
            .get("mock")
            .context("derived auth fixture provider missing")?;
        ensure!(
            child.api_base == provider.url,
            "derived auth fixture must declare its actual local endpoint"
        );
    }
    harness = harness.restart().await?;
    let descriptor = harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            ROUTE,
            GenerationProtocol::Messages,
        )
        .await
        .map_err(failure)?;
    ensure!(descriptor.manifest.candidates.len() == 1);
    ensure!(
        descriptor.sources[0].provider == "mock"
            && descriptor.sources[0].outbound_protocol == ApiProtocol::Messages
    );
    let credential = grant(
        &mut harness.fixture,
        scope(&descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "messages-auth",
    )
    .await?;
    let before = publication_counts(&harness.fixture).await?;
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            harness.fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request =
        Request::post("/v1/messages").body(Body::from(serde_json::to_vec(&json!({
            "model":ROUTE, "max_tokens":16, "messages":[{"role":"user","content":"hello"}]
        }))?))?;
    *request.headers_mut() = headers(&harness.fixture, &credential, &descriptor).await?;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let execution = response
        .headers()
        .get("x-bitrouter-request-id")
        .map(|value| -> Result<ExecutionId> { Ok(value.to_str()?.try_into()?) })
        .transpose()?;
    let response = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    ensure!(
        status == StatusCode::OK,
        "Messages dispatch failed: {status} {}",
        String::from_utf8_lossy(&response)
    );
    let response: Value = serde_json::from_slice(&response)?;
    ensure!(response["content"][0]["text"] == "auth reply");
    let (observed, body) = tokio::time::timeout(TIMEOUT, provider.received.recv())
        .await?
        .context("Messages provider was not called")?;
    let bearer = matches!(
        declaration,
        Declaration::BearerAccount | Declaration::InheritedBearer
    );
    if bearer {
        ensure!(
            observed
                .get("authorization")
                .context("Bearer header missing")?
                .to_str()?
                == format!("Bearer {secret}")
        );
        ensure!(
            !observed.contains_key("x-api-key"),
            "competing credential header"
        );
    } else {
        ensure!(
            observed
                .get("x-api-key")
                .context("API key header missing")?
                .to_str()?
                == secret
        );
        ensure!(
            !observed.contains_key("authorization"),
            "competing credential header"
        );
    }
    ensure!(body["model"] == "native-shared");
    ensure!(
        body.get(bitrouter_ai::tenancy::catalog::BODY_MEMBER)
            .is_none()
    );
    ensure!(!serde_json::to_string(&body)?.contains(&secret));
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let receipt = receipt(
        &harness.fixture,
        execution.context("execution identity missing")?,
    )
    .await?;
    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
    ensure!(publication_counts(&harness.fixture).await? == before);
    ensure!(
        provider.received.try_recv().is_err(),
        "unexpected second provider request"
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_messages_default_auth_uses_only_x_api_key() -> Result<()> {
    actual_messages_auth(Declaration::Default).await
}

#[tokio::test]
async fn configured_messages_account_explicit_bearer_uses_only_authorization() -> Result<()> {
    actual_messages_auth(Declaration::BearerAccount).await
}

#[tokio::test]
async fn configured_messages_derived_provider_inherits_bearer_and_child_key() -> Result<()> {
    actual_messages_auth(Declaration::InheritedBearer).await
}

#[tokio::test]
async fn configured_messages_derived_provider_can_override_bearer_with_x_api_key() -> Result<()> {
    actual_messages_auth(Declaration::OverrideToApiKey).await
}

#[test]
fn provider_configuration_refuses_unknown_auth_schemes() {
    for scheme in ["basic", "Bearer"] {
        assert!(crate::config::parse(&format!(
            "inherit_defaults: false\nproviders:\n  fixture:\n    api_base: https://fixture.invalid/v1\n    api_protocol: [{{\"*\": messages}}]\n    models: [{{id: model}}]\n    auth_scheme: {scheme}\n"
        )).is_err());
    }
}
