//! Service cache behavior under real dispatch, read pressure and retained-data faults.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use bitrouter_ai::{
    tenancy::{
        authority::{
            BindingPut, GrantScope, OwnerKind, PrincipalPut, TenantPut, TenantRole, TenantState,
        },
        configuration::ResourceRef,
        identity::{RevisionId, TenantId},
        operation::{ErrorCode, Mutation},
    },
    types::ApiProtocol,
};
use chrono::Utc;
use sea_orm::{
    ConnectionTrait,
    sea_query::{Alias, Expr, Query},
};
use serde_json::{Value, json};
use tokio::sync::{SemaphorePermit, mpsc};
use tokio_util::task::AbortOnDropHandle;
use tower::ServiceExt;

use super::{Fixture, MODEL, admitted, draft, expiry, failure, headers, issued, provision, result};
use crate::{
    gateway::{
        pipeline::{
            routing::{RoutingPrefs, RoutingTable},
            types::RoutingTarget,
        },
        providers::auth::credentials::Credential,
    },
    tenancy::{
        Service,
        actor::OperationLimits,
        command::Command,
        execution::{TenantExecution, cached::Caches},
        model::Deadline,
        store,
    },
};

const TIMEOUT: Duration = Duration::from_secs(10);
const SECRET: &str = "owned-upstream-secret";

#[derive(Clone)]
struct Observation {
    sent: mpsc::Sender<(HeaderMap, Value)>,
    calls: Arc<AtomicUsize>,
}

async fn provider(
    State(observation): State<Observation>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    observation.calls.fetch_add(1, Ordering::SeqCst);
    if observation.sent.try_send((headers, body)).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"unexpected provider request"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "id":"chatcmpl-cache", "object":"chat.completion", "model":"model-a",
            "choices":[{"index":0,"message":{"role":"assistant","content":"cached reply"},
                "finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
        })),
    )
}

struct Provider {
    url: String,
    received: mpsc::Receiver<(HeaderMap, Value)>,
    calls: Arc<AtomicUsize>,
    _task: AbortOnDropHandle<std::io::Result<()>>,
}

impl Provider {
    async fn start() -> Result<Self> {
        let (sent, received) = mpsc::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
        let router = Router::new()
            .route("/v1/chat/completions", post(provider))
            .with_state(Observation {
                sent,
                calls: calls.clone(),
            });
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            received,
            calls,
            _task: task,
        })
    }

    async fn generation(
        &mut self,
        service: &Service,
        binding: &HeaderMap,
        tokens: u32,
    ) -> Result<()> {
        self.generation_with_secret(service, binding, tokens, SECRET)
            .await
    }

    async fn generation_with_secret(
        &mut self,
        service: &Service,
        binding: &HeaderMap,
        tokens: u32,
        secret: &str,
    ) -> Result<()> {
        let (status, response) = dispatch(service, binding).await?;
        ensure!(status == StatusCode::OK, "dispatch returned {status}");
        ensure!(response["choices"][0]["message"]["content"] == "cached reply");
        let (headers, body) = tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("provider request missing")?;
        let expected = format!("Bearer {secret}");
        ensure!(
            headers
                .get("authorization")
                .is_some_and(|value| value == expected.as_str())
        );
        ensure!(body["model"] == "model-a");
        ensure!(body["max_tokens"] == tokens, "incorrect retained defaults");
        Ok(())
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn dispatch(service: &Service, binding: &HeaderMap) -> Result<(StatusCode, Value)> {
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request =
        Request::post("/v1/chat/completions").body(Body::from(serde_json::to_vec(&json!({
            "model":MODEL, "messages":[{"role":"user","content":"hello"}]
        }))?))?;
    *request.headers_mut() = binding.clone();
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    Ok((status, serde_json::from_slice(&body)?))
}

fn occupy_reads(service: &Service) -> Result<Vec<SemaphorePermit<'_>>> {
    let permits = (0..8)
        .map(|_| service.read_permit().map_err(failure))
        .collect::<Result<Vec<_>>>()?;
    ensure!(matches!(service.read_permit(), Err(error)
        if error.0.code == ErrorCode::CapacityExceeded));
    Ok(permits)
}

async fn selected_target(execution: &TenantExecution) -> Result<RoutingTarget> {
    execution
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
        .await?
        .into_iter()
        .next()
        .context("selected route has no target")
}

async fn assert_static_key(execution: &TenantExecution, target: &RoutingTarget) -> Result<()> {
    let resolved = execution
        .credential(target)
        .await
        .map_err(|code| anyhow::anyhow!("credential lookup rejected: {code:?}"))?;
    ensure!(matches!(resolved.credential, Credential::ApiKey { value } if value == SECRET));
    Ok(())
}

async fn reject_admission(fixture: &Fixture, binding: &HeaderMap, code: ErrorCode) -> Result<()> {
    let submission = fixture.service.submission(binding).map_err(failure)?;
    let admitted = fixture
        .service
        .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
        .await;
    ensure!(matches!(admitted, Err(error) if error.0.code == code));
    Ok(())
}

fn enable_dispatch(fixture: &Fixture) {
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
}

async fn corrupt_configuration(fixture: &Fixture, revision: &RevisionId) -> Result<()> {
    let update = Query::update()
        .table(Alias::new("tenancy_configurations"))
        .value(Alias::new("compiled_json"), "{}")
        .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(revision.as_str()))
        .to_owned();
    ensure!(
        fixture
            .db
            .execute(fixture.db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}

async fn corrupt_credential(fixture: &Fixture, reference: &ResourceRef) -> Result<()> {
    let update = Query::update()
        .table(Alias::new("tenancy_resource_revisions"))
        .value(Alias::new("payload_json"), "{}")
        .and_where(Expr::col(Alias::new("owner_kind")).eq("tenant"))
        .and_where(Expr::col(Alias::new("owner_id")).eq(reference.owner_id.as_str()))
        .and_where(Expr::col(Alias::new("resource_kind")).eq("credential"))
        .and_where(Expr::col(Alias::new("resource_id")).eq(reference.resource_id.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(reference.revision_id.as_str()))
        .to_owned();
    ensure!(
        fixture
            .db
            .execute(fixture.db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn warm_dispatch_needs_no_read_slot_and_cold_revision_pressure_is_retryable() -> Result<()> {
    let mut provider = Provider::start().await?;
    let (fixture, route, _) = provision(&provider.url).await?;
    // All mutations precede the first load, so only the revision key differs.
    let first = draft(&fixture, &route, "cache-first", 25).await?;
    let second = draft(&fixture, &route, "cache-second", 37).await?;
    ensure!(first.revision_id != second.revision_id);
    let credential = fixture.issue("cache-user").await?;
    enable_dispatch(&fixture);
    let token = credential.secret.as_deref().context("secret")?;
    let first_headers = headers(&fixture, token, &first.revision_id)?;
    let second_headers = headers(&fixture, token, &second.revision_id)?;
    provider
        .generation(&fixture.service, &first_headers, 25)
        .await?;

    let permits = occupy_reads(&fixture.service)?;
    provider
        .generation(&fixture.service, &first_headers, 25)
        .await?;
    let (status, rejection) = dispatch(&fixture.service, &second_headers).await?;
    ensure!(status == StatusCode::TOO_MANY_REQUESTS);
    ensure!(rejection["code"] == "capacity_exceeded");
    ensure!(rejection["dispatch_state"] == "not_started");
    ensure!(rejection["execution_id"].is_null());
    ensure!(provider.calls() == 2, "cold rejection reached the provider");
    // Management reads must still participate in the bounded DB read budget.
    ensure!(matches!(fixture.service.configuration(
        &fixture.operator, &fixture.tenant, &first.revision_id).await,
        Err(error) if error.0.code == ErrorCode::CapacityExceeded));
    ensure!(
        fixture.service.current().is_ok(),
        "read pressure fenced authority"
    );
    drop(permits);

    provider
        .generation(&fixture.service, &second_headers, 37)
        .await?;
    let permits = occupy_reads(&fixture.service)?;
    provider
        .generation(&fixture.service, &first_headers, 25)
        .await?;
    provider
        .generation(&fixture.service, &second_headers, 37)
        .await?;
    ensure!(provider.calls() == 5);
    drop(permits);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_route_with_cold_credential_returns_retryable_http_capacity_without_a_send()
-> Result<()> {
    let mut provider = Provider::start().await?;
    let (fixture, route, _) = provision(&provider.url).await?;
    let revision = draft(&fixture, &route, "cache-draft", 25).await?;
    let credential = fixture.issue("cache-user").await?;
    enable_dispatch(&fixture);
    let binding = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    // Admission loads the route, but authentication has not loaded its key.
    drop(admitted(&fixture, &binding).await?);
    let permits = occupy_reads(&fixture.service)?;
    let (status, rejection) = dispatch(&fixture.service, &binding).await?;
    ensure!(
        status == StatusCode::TOO_MANY_REQUESTS,
        "credential cache pressure was misclassified as {status}"
    );
    ensure!(rejection["code"] == "capacity_exceeded");
    ensure!(rejection["dispatch_state"] == "not_started");
    ensure!(rejection["execution_id"].as_str().is_some());
    ensure!(provider.calls() == 0);
    ensure!(
        fixture.service.current().is_ok(),
        "credential read pressure fenced authority"
    );
    drop(permits);
    provider.generation(&fixture.service, &binding, 25).await?;
    ensure!(provider.calls() == 1);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn authority_publication_separates_both_route_and_static_key_cache_namespaces() -> Result<()>
{
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "cache-draft", 25).await?;
    let credential = fixture.issue("cache-user").await?;
    enable_dispatch(&fixture);
    let binding = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    let original = admitted(&fixture, &binding).await?;
    let target = selected_target(&original).await?;
    assert_static_key(&original, &target).await?;
    let before = fixture.service.current().map_err(failure)?.revision;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Principal(
                    "unrelated-principal".try_into()?,
                    PrincipalPut {
                        operation_id: "unrelated-publication".try_into()?,
                        expected_revision: None,
                        issuer: "cache-test".into(),
                        subject: "unrelated-subject".into(),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.service.current().map_err(failure)?.revision > before);
    ensure!(
        original.revalidate().is_ok(),
        "unrelated mutation changed eligibility"
    );

    let permits = occupy_reads(&fixture.service)?;
    reject_admission(&fixture, &binding, ErrorCode::CapacityExceeded).await?;
    // An already retained route must not reuse a key from the old authority namespace.
    ensure!(matches!(
        original.credential(&target).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(fixture.service.current().is_ok());
    drop(permits);

    let current = admitted(&fixture, &binding).await?;
    let current_target = selected_target(&current).await?;
    let permits = occupy_reads(&fixture.service)?;
    // Loading the new route namespace must not accidentally warm the credential.
    ensure!(matches!(
        current.credential(&current_target).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(permits);
    assert_static_key(&current, &current_target).await?;
    let permits = occupy_reads(&fixture.service)?;
    let warm = admitted(&fixture, &binding).await?;
    assert_static_key(&original, &target).await?;
    assert_static_key(&current, &current_target).await?;
    ensure!(fixture.service.current().is_ok());
    drop(permits);
    drop(warm);
    drop(current);
    drop(original);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_dispatch_and_retained_execution_recheck_revocation_suspension_and_scope() -> Result<()>
{
    for change in ["revoke", "suspend", "scope"] {
        let mut provider = Provider::start().await?;
        let (fixture, route, _) = provision(&provider.url).await?;
        let revision = draft(&fixture, &route, "cache-draft", 25).await?;
        let credential = fixture.issue("cache-user").await?;
        enable_dispatch(&fixture);
        let binding = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision.revision_id,
        )?;
        provider.generation(&fixture.service, &binding, 25).await?;
        let execution = admitted(&fixture, &binding).await?;
        let target = selected_target(&execution).await?;
        assert_static_key(&execution, &target).await?;
        let command = match change {
            "revoke" => Command::Revoke(
                fixture.tenant.clone(),
                credential.credential_id,
                Mutation {
                    operation_id: "cache-revoke".try_into()?,
                    expected_revision: credential.credential_revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
            "suspend" => Command::Tenant(
                fixture.tenant.clone(),
                TenantPut {
                    operation_id: "cache-suspend".try_into()?,
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
                    operation_id: "cache-reduce-scope".try_into()?,
                    expected_revision: fixture.binding_result.resource_revision.clone(),
                    principal_id: fixture.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: GrantScope::Resources {
                        resources: vec![route.key()],
                    },
                    expires_at: expiry()?,
                    expected_management_owner: Some(
                        crate::tenancy::operator_principal("test-operator").map_err(failure)?,
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
        let expected = match change {
            "revoke" => ErrorCode::Unauthorized,
            "suspend" => ErrorCode::TenantInactive,
            _ => ErrorCode::Forbidden,
        };
        let permits = occupy_reads(&fixture.service)?;
        ensure!(matches!(execution.credential(&target).await, Err(code) if code == expected));
        ensure!(matches!(execution.admit_send(&target), Err(code) if code == expected));
        let (status, rejection) = dispatch(&fixture.service, &binding).await?;
        // A scope publication invalidates the old immutable-cache namespace.
        // A new invocation must read its selected dependency closure before it
        // can observe that the credential grant was removed. The pinned old
        // execution above already has that closure and denies without reading.
        let busy_code = if change == "scope" {
            ensure!(status == StatusCode::TOO_MANY_REQUESTS);
            ErrorCode::CapacityExceeded
        } else {
            ensure!(
                status
                    == if change == "revoke" {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::FORBIDDEN
                    }
            );
            expected
        };
        ensure!(rejection["code"] == serde_json::to_value(busy_code)?);
        ensure!(provider.calls() == 1, "{change} allowed an additional send");
        ensure!(fixture.service.current().is_ok());
        drop(permits);
        let (status, rejection) = dispatch(&fixture.service, &binding).await?;
        ensure!(
            status
                == if change == "revoke" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::FORBIDDEN
                }
        );
        ensure!(rejection["code"] == serde_json::to_value(expected)?);
        ensure!(provider.calls() == 1, "{change} allowed an additional send");
        drop(execution);
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn warmed_entries_recheck_credential_and_binding_expiry_without_publication() -> Result<()> {
    for expire_binding in [false, true] {
        let mut provider = Provider::start().await?;
        let (fixture, route, _) = provision(&provider.url).await?;
        let revision = draft(&fixture, &route, "cache-draft", 25).await?;
        let credential = fixture.issue("cache-user").await?;
        enable_dispatch(&fixture);
        let binding = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision.revision_id,
        )?;
        provider.generation(&fixture.service, &binding, 25).await?;
        let execution = admitted(&fixture, &binding).await?;
        let target = selected_target(&execution).await?;
        assert_static_key(&execution, &target).await?;
        let before = fixture.service.current().map_err(failure)?.revision;
        // Move only the checked deadline across now: expiry must work without a
        // publication/cache-key change. No wall-clock sleeps or public clock API.
        let expired = Deadline::parse(
            (Utc::now() - chrono::Duration::seconds(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()?,
        )?;
        {
            let mut published = fixture
                .service
                .snapshot
                .write()
                .map_err(|_| anyhow::anyhow!("authority snapshot lock"))?;
            let snapshot = Arc::make_mut(&mut *published);
            if expire_binding {
                snapshot
                    .bindings
                    .get_mut(&(fixture.tenant.clone(), fixture.binding.clone()))
                    .context("binding")?
                    .expires = expired;
            } else {
                snapshot
                    .credentials
                    .get_mut(&credential.credential_id)
                    .context("credential")?
                    .expires = expired;
            }
        }
        ensure!(fixture.service.current().map_err(failure)?.revision == before);
        let expected = if expire_binding {
            ErrorCode::Forbidden
        } else {
            ErrorCode::Unauthorized
        };
        let permits = occupy_reads(&fixture.service)?;
        ensure!(matches!(execution.credential(&target).await, Err(code) if code == expected));
        ensure!(matches!(execution.admit_send(&target), Err(code) if code == expected));
        let (status, rejection) = dispatch(&fixture.service, &binding).await?;
        ensure!(
            status
                == if expire_binding {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNAUTHORIZED
                }
        );
        ensure!(rejection["code"] == serde_json::to_value(expected)?);
        ensure!(
            provider.calls() == 1,
            "expired authority reached the provider"
        );
        ensure!(fixture.service.current().is_ok());
        drop(permits);
        drop(execution);
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn warm_immutable_execution_survives_db_corruption_but_management_get_is_uncached()
-> Result<()> {
    let mut provider = Provider::start().await?;
    let (fixture, route, _) = provision(&provider.url).await?;
    let revision = draft(&fixture, &route, "cache-draft", 25).await?;
    let credential = fixture.issue("cache-user").await?;
    enable_dispatch(&fixture);
    let binding = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    provider.generation(&fixture.service, &binding, 25).await?;
    corrupt_configuration(&fixture, &revision.revision_id).await?;
    provider.generation(&fixture.service, &binding, 25).await?;
    ensure!(provider.calls() == 2);
    ensure!(
        matches!(fixture.service.configuration(&fixture.operator, &fixture.tenant, &revision.revision_id).await,
        Err(error) if error.0.code == ErrorCode::StorageUnavailable)
    );
    ensure!(
        fixture.service.current().is_err(),
        "management integrity failure did not fence"
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_configuration_and_static_key_bytes_do_not_survive_service_restart() -> Result<()> {
    for corrupt_key in [false, true] {
        let mut provider = Provider::start().await?;
        let (mut fixture, route, slot) = provision(&provider.url).await?;
        let revision = draft(&fixture, &route, "cache-draft", 25).await?;
        let credential = fixture.issue("cache-user").await?;
        let token = credential.secret.as_deref().context("secret")?;
        enable_dispatch(&fixture);
        let binding = headers(&fixture, token, &revision.revision_id)?;
        provider.generation(&fixture.service, &binding, 25).await?;
        if corrupt_key {
            corrupt_credential(&fixture, &slot).await?;
        } else {
            corrupt_configuration(&fixture, &revision.revision_id).await?;
        }
        provider.generation(&fixture.service, &binding, 25).await?;
        ensure!(provider.calls() == 2);
        let before = fixture.service.current().map_err(failure)?.revision;
        fixture.service.shutdown().await?;
        let snapshot = store::load(&fixture.db, Utc::now()).await?;
        ensure!(snapshot.revision == before);
        // No activation pointer: reopen the fixture with the same installation,
        // wrapping key and authority revision, but a fresh Service-local cache.
        let reopened = Service::start_prepared(
            fixture.db.clone(),
            None,
            OperationLimits::default(),
            snapshot,
            fixture.service.resources.clone(),
        )
        .await;
        if corrupt_key {
            // The fixture now uses production's authenticated actor bootstrap.
            // Corrupt credential rows must fail there before any Service/cache
            // is exposed; inactive configuration corruption is still read below.
            ensure!(
                reopened.is_err(),
                "cold corrupt credential passed bootstrap"
            );
            ensure!(provider.calls() == 2);
            continue;
        }
        fixture.service = reopened?;
        enable_dispatch(&fixture);
        let binding = headers(&fixture, token, &revision.revision_id)?;
        let (status, rejection) = dispatch(&fixture.service, &binding).await?;
        ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
        ensure!(rejection["code"] == "storage_unavailable");
        ensure!(
            provider.calls() == 2,
            "cold corrupt bytes reached the provider"
        );
        ensure!(fixture.service.current().is_err());
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn escaped_route_keeps_the_entire_revision_pin_until_its_last_clone_drops() -> Result<()> {
    let (mut fixture, route, _) = provision("https://example.invalid/v1").await?;
    fixture.service.caches = Arc::new(Caches::with_limits(
        1,
        128 * 1024 * 1024,
        256,
        8 * 1024 * 1024,
    )?);
    // Do not advance the authority namespace between these two cache loads.
    let first = draft(&fixture, &route, "cache-first", 25).await?;
    let second = draft(&fixture, &route, "cache-second", 37).await?;
    let credential = fixture.issue("cache-user").await?;
    enable_dispatch(&fixture);
    let token = credential.secret.as_deref().context("secret")?;
    let first_headers = headers(&fixture, token, &first.revision_id)?;
    let second_headers = headers(&fixture, token, &second.revision_id)?;
    let execution = admitted(&fixture, &first_headers).await?;
    let escaped = execution.route().clone();
    let caller = execution.caller();
    drop(execution);
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    reject_admission(&fixture, &second_headers, ErrorCode::CapacityExceeded).await?;
    ensure!(
        fixture.service.current().is_ok(),
        "pinned-cache pressure fenced authority"
    );
    ensure!(
        !escaped
            .route_chain(MODEL, &RoutingPrefs::default(), &caller)
            .await?
            .is_empty()
    );
    let last = escaped.clone();
    drop(escaped);
    reject_admission(&fixture, &second_headers, ErrorCode::CapacityExceeded).await?;
    drop(last);
    let next = admitted(&fixture, &second_headers).await?;
    let target = selected_target(&next).await?;
    assert_static_key(&next, &target).await?;
    drop(next);
    // The old revision also remains reloadable after ordinary eviction.
    drop(admitted(&fixture, &first_headers).await?);
    ensure!(fixture.service.current().is_ok());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn identical_credential_names_remain_isolated_across_tenants_and_operator_pool_owners()
-> Result<()> {
    let mut provider = Provider::start().await?;
    let (mut first, first_route, first_slot) = provision(&provider.url).await?;
    first.service.shutdown().await?;
    let tenant: TenantId = "tenant-b".try_into()?;
    let mut second_slot = first_slot.clone();
    second_slot.owner_id = "tenant-b".try_into()?;
    let mut pool_slot = first_slot.clone();
    // The pool has even the same owner ID as tenant A: only owner_kind differs.
    pool_slot.owner_kind = OwnerKind::OperatorPool;
    let mut second_route = first_route.clone();
    second_route.owner_id = "tenant-b".try_into()?;
    let mut pool_route = second_route.clone();
    pool_route.revision_id = "r2".try_into()?;
    let route_definition = |reference: &ResourceRef, slot: &ResourceRef| {
        json!({
            "reference":reference,
            "definition":{
                "kind":"route", "selector":"mock:model-a", "policy":null,
                "routing":{"providers":{"mock":{
                    "api_base":provider.url, "api_protocol":[{"*":"chat_completions"}],
                    "models":[{"id":"model-a"}]
                }}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]
            }
        })
    };
    first
        .service
        .resources
        .provision_bytes(
            &first.db,
            &serde_json::to_vec(&json!({
                "format":1, "installation_id":first.installation,
                "resources":[
                    {"reference":second_slot,"definition":{"kind":"credential","credential":{
                        "type":"api_key","data":{"value":"tenant-b-secret"}}}},
                    {"reference":pool_slot,"definition":{"kind":"credential","credential":{
                        "type":"api_key","data":{"value":"pool-secret"}}}},
                    route_definition(&second_route, &second_slot),
                    route_definition(&pool_route, &pool_slot)
                ]
            }))?,
        )
        .await?;
    first.service = Service::start_prepared(
        first.db.clone(),
        None,
        OperationLimits::default(),
        store::load(&first.db, Utc::now()).await?,
        first.service.resources.clone(),
    )
    .await?;
    result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Tenant(
                    tenant.clone(),
                    TenantPut {
                        operation_id: "cache-create-second-tenant".try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let scope = GrantScope::Tenant {
        operator_pools: vec![pool_slot.key()],
    };
    let binding_result = result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    first.binding.clone(),
                    BindingPut {
                        operation_id: "cache-create-second-binding".try_into()?,
                        expected_revision: None,
                        principal_id: first.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: scope.clone(),
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let second = Fixture {
        service: first.service.clone(),
        db: first.db.clone(),
        operator: first.operator.clone(),
        installation: first.installation.clone(),
        tenant,
        principal: first.principal.clone(),
        binding: first.binding.clone(),
        binding_result,
    };
    let first_revision = draft(&first, &first_route, "cache-first-tenant", 25).await?;
    let second_revision = draft(&second, &second_route, "cache-second-tenant", 37).await?;
    let pool_revision = draft(&second, &pool_route, "cache-pool-revision", 49).await?;
    let first_key = first.issue("cache-first-user").await?;
    let mut input = second.issuance("cache-second-user")?;
    input.scope = scope;
    let second_key = issued(
        second
            .service
            .execute(
                second.operator.clone(),
                Command::Issue(second.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )?;
    enable_dispatch(&first);
    let first_token = first_key.secret.as_deref().context("first secret")?;
    let second_token = second_key.secret.as_deref().context("second secret")?;
    let first_headers = headers(&first, first_token, &first_revision.revision_id)?;
    let second_headers = headers(&second, second_token, &second_revision.revision_id)?;
    let pool_headers = headers(&second, second_token, &pool_revision.revision_id)?;
    // These credentials share provider/account, resource name and revision. The
    // actual Authorization header must follow the fully qualified owner binding.
    for (binding, tokens, secret) in [
        (&first_headers, 25, SECRET),
        (&second_headers, 37, "tenant-b-secret"),
        (&pool_headers, 49, "pool-secret"),
    ] {
        provider
            .generation_with_secret(&first.service, binding, tokens, secret)
            .await?;
    }
    let permits = occupy_reads(&first.service)?;
    for (binding, tokens, secret) in [
        (&pool_headers, 49, "pool-secret"),
        (&first_headers, 25, SECRET),
        (&second_headers, 37, "tenant-b-secret"),
    ] {
        provider
            .generation_with_secret(&second.service, binding, tokens, secret)
            .await?;
    }
    drop(permits);
    let foreign = headers(&first, first_token, &second_revision.revision_id)?;
    let (status, rejection) = dispatch(&first.service, &foreign).await?;
    ensure!(status == StatusCode::NOT_FOUND);
    ensure!(rejection["code"] == "not_found");
    ensure!(
        provider.calls() == 6,
        "a foreign warmed revision reached the provider"
    );
    ensure!(first.service.current().is_ok());
    first.service.shutdown().await?;
    Ok(())
}
