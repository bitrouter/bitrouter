use super::*;
use crate::gateway::pipeline::routing::{RoutingPrefs, RoutingTable};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::{ConfigurationRevision, ResourceRef},
        identity::RevisionId,
    },
    types::ApiProtocol,
};
use serde_json::json;
use std::sync::{Arc, atomic::Ordering};

const MODEL: &str = "provider:literal-route";

mod cache;
mod defaults;
mod ingress;
mod lifecycle_http;
mod rejections;
mod secrets;

async fn provision(url: &str) -> Result<(Fixture, ResourceRef, ResourceRef)> {
    provision_protocol(url, "chat_completions").await
}

async fn provision_protocol(
    url: &str,
    protocol: &str,
) -> Result<(Fixture, ResourceRef, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources =
        super::super::resources::Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let slot = reference(ResourceKind::Credential, "upstream-slot")?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1, "installation_id":fixture.installation,
        "resources":[
            {"reference":slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
                "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":protocol}],"models":[{"id":"model-a"}]}},
                    "routers":{"owned":{"selection":{"kind":"model","model":"mock:model-a"},
                        "defaults":{"params":{"max_tokens":999,"max_completion_tokens":777,"max_output_tokens":888,"temperature":1.9}}}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
        ]
    }))?).await?;
    let snapshot = super::super::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        super::super::actor::OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )?;
    Ok((fixture, route, slot))
}

async fn draft(
    fixture: &Fixture,
    route: &ResourceRef,
    operation: &str,
    tokens: u32,
) -> Result<ConfigurationRevision> {
    let mut input = fixture.configuration_input(operation)?;
    input.document.routes.push(route.clone());
    input.document.generation_defaults.max_output_tokens = tokens;
    input.document.limits.active_executions = 4;
    input.document.limits.queued_work = 4;
    match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(revision) => Ok(revision),
        _ => anyhow::bail!("expected configuration"),
    }
}

fn headers(fixture: &Fixture, token: &str, revision: &RevisionId) -> Result<::http::HeaderMap> {
    let caller = fixture.service.authenticate(token).map_err(failure)?;
    let binding = fixture
        .service
        .capabilities(&caller)
        .map_err(failure)?
        .bind(revision.clone(), "same-correlation".try_into()?)?;
    let mut headers = ::http::HeaderMap::new();
    let object = serde_json::to_value(binding)?;
    for (name, value) in object.as_object().context("binding object")? {
        headers.insert(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.as_str().context("binding text")?.parse()?,
        );
    }
    headers.insert("authorization", format!("Bearer {token}").parse()?);
    Ok(headers)
}

async fn admitted(
    fixture: &Fixture,
    headers: &::http::HeaderMap,
) -> Result<Arc<super::super::execution::TenantExecution>> {
    let submission = fixture.service.submission(headers).map_err(failure)?;
    fixture
        .service
        .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
        .await
        .map_err(failure)
}

#[tokio::test]
async fn queued_execution_rechecks_revoke_suspend_and_reduced_credential_scope() -> Result<()> {
    for change in ["revoke", "suspend", "scope"] {
        let (fixture, route, _) = provision("https://example.invalid/v1").await?;
        let mut input = fixture.configuration_input("queued-config")?;
        input.document.routes = vec![route.clone()];
        input.document.limits.active_executions = 1;
        input.document.limits.queued_work = 1;
        let revision = match fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?
        {
            StoredResponse::Configuration(revision) => revision,
            _ => anyhow::bail!("expected configuration"),
        };
        let credential = fixture.issue("queue-user").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision.revision_id,
        )?;
        let first = admitted(&fixture, &headers).await?;
        let service = fixture.service.clone();
        let queued = tokio::spawn(async move {
            let submission = service.submission(&headers)?;
            service
                .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while fixture.service.admission.queued_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let command = match change {
            "revoke" => Command::Revoke(
                fixture.tenant.clone(),
                credential.credential_id,
                Mutation {
                    operation_id: "stop-queued".try_into()?,
                    expected_revision: credential.credential_revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
            "suspend" => Command::Tenant(
                fixture.tenant.clone(),
                TenantPut {
                    operation_id: "stop-queued".try_into()?,
                    expected_revision: Some(
                        fixture
                            .service
                            .current()
                            .map_err(failure)?
                            .tenants
                            .get(&fixture.tenant)
                            .context("tenant")?
                            .revision
                            .clone(),
                    ),
                    state: TenantState::Suspended,
                    installation_id: fixture.installation.clone(),
                },
            ),
            _ => Command::Binding(
                fixture.tenant.clone(),
                fixture.binding.clone(),
                BindingPut {
                    operation_id: "stop-queued".try_into()?,
                    expected_revision: fixture.binding_result.resource_revision.clone(),
                    principal_id: fixture.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: GrantScope::Resources {
                        resources: vec![route.key()],
                    },
                    expires_at: expiry()?,
                    expected_management_owner: Some(
                        super::super::operator_principal("test-operator").map_err(failure)?,
                    ),
                    installation_id: fixture.installation.clone(),
                },
            ),
        };
        result(
            fixture
                .service
                .execute(fixture.operator.clone(), command)
                .await
                .map_err(failure)?,
        )?;
        drop(first);
        let rejection = tokio::time::timeout(std::time::Duration::from_secs(2), queued).await??;
        let expected = match change {
            "revoke" => ErrorCode::Unauthorized,
            "suspend" => ErrorCode::TenantInactive,
            _ => ErrorCode::Forbidden,
        };
        ensure!(matches!(rejection, Err(error) if error.0.code == expected));
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn execution_uses_submit_and_credential_grants_without_management_read() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "draft", 25).await?;
    let mut issue = fixture.issuance("execution-only")?;
    issue.permissions = vec![Permission::ExecutionSubmit, Permission::CredentialUse];
    let credential = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), issue),
            )
            .await
            .map_err(failure)?,
    )?;
    let token = credential.secret.as_deref().context("secret")?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(&fixture, token, &revision.revision_id)?;
    let execution = admitted(&fixture, &headers).await?;
    let caller = fixture.service.authenticate(token).map_err(failure)?;
    ensure!(
        fixture
            .service
            .configuration(&caller, &fixture.tenant, &revision.revision_id)
            .await
            .is_err()
    );
    let target = execution
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
        .await?
        .remove(0);
    let resolved = execution
        .credential(&target)
        .await
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(
        matches!(resolved.credential, crate::gateway::providers::auth::credentials::Credential::ApiKey { value } if value == "owned-upstream-secret")
    );
    execution
        .admit_send(&target)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    drop(execution);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tenant_http_uses_literal_routes_owned_auth_and_existing_json_sse_renderers() -> Result<()>
{
    use axum::{
        Json, Router, body::Body, extract::State, http::StatusCode, response::IntoResponse,
        routing::post,
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct Observed {
        calls: Vec<serde_json::Value>,
        authenticated: usize,
    }
    async fn provider(
        State(observed): State<Arc<Mutex<Observed>>>,
        headers: ::http::HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        if let Ok(mut observed) = observed.lock() {
            observed.authenticated += usize::from(
                headers
                    .get("authorization")
                    .is_some_and(|v| v == "Bearer owned-upstream-secret"),
            );
            observed.calls.push(body.clone());
        }
        if body["stream"] == true {
            let chunks = [
                "data: {\"id\":\"chatcmpl-owned\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"owned reply\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chatcmpl-owned\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
                "data: [DONE]\n\n",
            ];
            let stream =
                futures::stream::iter(chunks.into_iter().map(Ok::<_, std::convert::Infallible>));
            return (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response();
        }
        Json(json!({"id":"chatcmpl-owned","object":"chat.completion","created":0,"model":"model-a",
            "choices":[{"index":0,"message":{"role":"assistant","content":"owned reply"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}})).into_response()
    }
    let observed = Arc::new(Mutex::new(Observed::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
    let provider_router = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(observed.clone());
    let provider = tokio::spawn(async move { axum::serve(listener, provider_router).await });
    let (fixture, route, _) = provision(&url).await?;
    let revision = draft(&fixture, &route, "draft-http", 25).await?;
    let credential = fixture.issue("http-execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            super::super::http::dispatch,
        ));
    let ingress = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let ingress_url = format!("http://{}", ingress.local_addr()?);
    let ingress_task = tokio::spawn(async move { axum::serve(ingress, router).await });
    let protocols: &[bool] = if cfg!(feature = "http2") {
        &[false, true]
    } else {
        &[false]
    };
    for &http2 in protocols {
        let builder = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10));
        #[cfg(feature = "http2")]
        let builder = if http2 {
            builder.http2_prior_knowledge()
        } else {
            builder.http1_only()
        };
        let client = builder.build()?;
        for stream in [false, true] {
            for (path, body) in [
                (
                    "/v1/chat/completions",
                    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
                ),
                (
                    "/v1/messages",
                    json!({"model":MODEL,"max_tokens":7,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
                ),
                (
                    "/v1/responses",
                    json!({"model":MODEL,"input":"hello","stream":stream}),
                ),
            ] {
                let response = client
                    .post(format!("{ingress_url}{path}"))
                    .headers(headers.clone())
                    .json(&body)
                    .send()
                    .await?;
                ensure!(
                    response.version()
                        == if http2 {
                            ::http::Version::HTTP_2
                        } else {
                            ::http::Version::HTTP_11
                        }
                );
                let status = response.status();
                let body = response.bytes().await?;
                ensure!(
                    status == StatusCode::OK,
                    "{path} stream={stream}: {status}: {}",
                    String::from_utf8_lossy(&body)
                );
                ensure!(String::from_utf8_lossy(&body).contains("owned reply"));
            }
        }
    }
    {
        let observed = observed
            .lock()
            .map_err(|_| anyhow::anyhow!("observation lock"))?;
        ensure!(
            observed.calls.len() == 6 * protocols.len()
                && observed.authenticated == observed.calls.len()
        );
        ensure!(observed.calls.iter().all(|body| body["model"] == "model-a"));
        for (index, body) in observed.calls.iter().enumerate() {
            let expected = if index % 3 == 1 { 7 } else { 25 };
            ensure!(body["max_tokens"] == expected);
            ensure!(
                body.get("max_output_tokens").is_none()
                    && body.get("max_completion_tokens").is_none()
            );
            ensure!(body["temperature"] == 0.0);
        }
    }
    fixture.service.shutdown().await?;
    provider.abort();
    ingress_task.abort();
    Ok(())
}

#[tokio::test]
async fn every_send_rechecks_revocation_and_same_correlation_never_deduplicates() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "draft", 25).await?;
    let credential = fixture.issue("execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    let first = admitted(&fixture, &headers).await?;
    let second = admitted(&fixture, &headers).await?;
    ensure!(first.id() != second.id());
    let target = first
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &first.caller())
        .await?
        .remove(0);
    let a = first
        .admit_send(&target)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    let b = first
        .admit_send(&target)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(a != b);
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Revoke(
                fixture.tenant.clone(),
                credential.credential_id.clone(),
                Mutation {
                    operation_id: "revoke".try_into()?,
                    expected_revision: credential.credential_revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    ensure!(matches!(
        first.admit_send(&target),
        Err(ErrorCode::Unauthorized)
    ));
    ensure!(fixture.service.submission(&headers).is_err());
    drop((first, second));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn retained_execution_does_not_substitute_a_new_active_revision() -> Result<()> {
    use bitrouter_ai::tenancy::configuration::Activation;
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let r1 = draft(&fixture, &route, "r1", 25).await?;
    let r2 = draft(&fixture, &route, "r2", 50).await?;
    let credential = fixture.issue("execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let old_headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &r1.revision_id,
    )?;
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Activate(
                fixture.tenant.clone(),
                Activation {
                    operation_id: "activate-r2".try_into()?,
                    expected_active_revision: None,
                    revision_id: r2.revision_id,
                    expected_authority_revision: fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let execution = admitted(&fixture, &old_headers).await?;
    let mut input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
    )?;
    execution.route().apply_generation_defaults(&mut input);
    ensure!(
        input
            .generation_prompt()
            .context("generation")?
            .params
            .max_tokens
            == Some(25)
    );
    let target = execution
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
        .await?
        .remove(0);
    let mut attempts = std::collections::HashSet::new();
    for _ in 0..8 {
        attempts.insert(
            execution
                .admit_send(&target)
                .map_err(|code| anyhow::anyhow!("{code:?}"))?,
        );
    }
    ensure!(attempts.len() == 8);
    ensure!(matches!(
        execution.admit_send(&target),
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(execution);
    fixture.service.shutdown().await?;
    Ok(())
}
