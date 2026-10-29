//! Served request parsing/admission/status semantics against real shared storage.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::gateway::assembly::App;
use crate::gateway::pipeline::routing::StaticRoutingTable;
use crate::gateway::shared::http::{Serving, Snapshot};
use bitrouter_ai::types::OperationScope;

mod cancellation;
mod failures;
mod generations;
mod profiles;

fn application(fixture: &Fixture) -> Result<Arc<App>> {
    let routes = StaticRoutingTable::new();
    for path in ["ok", "stream", "reject", "chat-batch", "live"] {
        routes.insert(path, vec![fixture.target(path)]);
    }
    let mut decisions = fixture.target("decisions");
    decisions.api_protocol = ApiProtocol::Decisions;
    routes.insert("decisions", vec![decisions]);
    Ok(Arc::new(
        App::builder()
            .skip_auth(false)
            .language_model(|pipeline| {
                pipeline
                    .owned_drain()
                    .served_operations(OperationScope::Both)
                    .routing_table(Arc::new(routes))
                    .executor(fixture.executor.clone())
                    .pre_resolution_hook_for(
                        crate::auth::hook::AuthHook::new(sea_orm::DatabaseConnection::Disconnected),
                        OperationScope::Both,
                    );
            })
            .build()?,
    ))
}

fn config() -> Result<crate::config::gateway_state::SharedGatewayConfig> {
    Ok(serde_json::from_value(json!({
        "namespace":"http-fixture", "deployment_key_env":"UNSET_HTTP_FIXTURE",
        "local_max_in_flight":1, "max_request_bytes":1024
    }))?)
}

fn request(
    fixture: &Fixture,
    path: &str,
    body: Value,
    key: Option<&str>,
    correlation: &str,
) -> Result<Request<Body>> {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", fixture.key.secret))
        .header("x-bitrouter-request-id", correlation);
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    Ok(request.body(Body::from(serde_json::to_vec(&body)?))?)
}

fn prompt(model: &str) -> Value {
    json!({"model":model,"messages":[{"role":"user","content":"hello"}]})
}

async fn retire(serving: &Serving) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while serving.custody.pending() > 0 {
            serving.custody.reconcile_one().await?;
            // The terminal handshake can wake a detached finalizer after the
            // HTTP body has dropped. Wait for quiescence, not a fixed sleep.
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("HTTP custody did not become quiescent")??;
    ensure!(serving.custody.pending() == 0);
    Ok(())
}

pub(super) async fn exercise(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    tenant: &str,
) -> Result<()> {
    let current = store
        .get::<Value>(Kind::Tenant, tenant)
        .await?
        .context("missing HTTP tenant")?;
    fixture
        .engine
        .set_tenant(
            tenant,
            current.value["policy_revision"].as_u64(),
            &TenantLimits {
                concurrent_executions: 20,
                requests_per_minute: 1000,
                resource_pools: [fixture.pool.clone()].into(),
            },
        )
        .await?;
    let serving = Serving::new(
        fixture.engine.clone(),
        Snapshot::new(fixture.revision, application(fixture)?)?,
        &config()?,
    )?;
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            prompt("ok"),
            None,
            "not-ready",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE && serving.custody.pending() == 0);
    serving.set_ready(true);
    Box::pin(pre_extractor_bound(fixture, &serving)).await?;
    Box::pin(protocols_and_duplicates(fixture, upstream, &serving)).await?;
    Box::pin(profiles::exercise(
        fixture, upstream, store, tenant, &serving,
    ))
    .await?;
    Box::pin(cancellation::exercise(
        fixture, upstream, store, tenant, &serving,
    ))
    .await?;
    Box::pin(failures::exercise(
        fixture, upstream, store, tenant, &serving,
    ))
    .await?;
    Box::pin(status_authorization(fixture, store, tenant, &serving)).await?;
    Box::pin(real_transport(fixture, &serving)).await?;
    Box::pin(generations::exercise(fixture, upstream, store, &serving)).await?;
    Ok(())
}

struct ObservedBody(Arc<AtomicUsize>);
impl HttpBody for ObservedBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}

async fn pre_extractor_bound(fixture: &Fixture, serving: &Arc<Serving>) -> Result<()> {
    let reservation = serving.custody.reserve()?;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        let polled = Arc::new(AtomicUsize::new(0));
        let mut pending = request(fixture, path, prompt("ok"), None, "bounded")?;
        pending.headers_mut().insert(
            "bitrouter-beta",
            http::HeaderValue::from_static("orchestrator_core=v1"),
        );
        *pending.body_mut() = Body::new(ObservedBody(polled.clone()));
        let response = serving.router().oneshot(pending).await?;
        ensure!(
            response.status() == StatusCode::SERVICE_UNAVAILABLE
                && polled.load(Ordering::SeqCst) == 0
        );
        ensure!(response.headers()["retry-after"] == "1");
        drop(response);
    }
    drop(reservation);
    retire(serving).await?;
    let mut oversized = request(
        fixture,
        "/v1/chat/completions",
        prompt("ok"),
        None,
        "oversized",
    )?;
    *oversized.body_mut() = Body::from(vec![b' '; 2048]);
    let response = serving.router().oneshot(oversized).await?;
    ensure!(response.status() == StatusCode::PAYLOAD_TOO_LARGE);
    drop(response);
    retire(serving).await?;
    let polled = Arc::new(AtomicUsize::new(0));
    let mut unauthorized = request(
        fixture,
        "/v1/chat/completions",
        prompt("ok"),
        None,
        "missing-auth",
    )?;
    unauthorized.headers_mut().remove("authorization");
    *unauthorized.body_mut() = Body::new(ObservedBody(polled.clone()));
    let response = serving.router().oneshot(unauthorized).await?;
    ensure!(response.status() == StatusCode::UNAUTHORIZED && polled.load(Ordering::SeqCst) == 0);
    drop(response);
    retire(serving).await?;
    let mut duplicate = request(
        fixture,
        "/v1/chat/completions",
        prompt("ok"),
        Some("first"),
        "duplicate-header",
    )?;
    duplicate
        .headers_mut()
        .append("idempotency-key", http::HeaderValue::from_static("second"));
    let response = serving.router().oneshot(duplicate).await?;
    ensure!(response.status() == StatusCode::BAD_REQUEST);
    drop(response);
    retire(serving).await?;
    // Incompatible protocol input is rejected before any durable admission.
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            json!({"model":"ok","messages":"wrong-type"}),
            Some("invalid-protocol"),
            "invalid",
        )?)
        .await?;
    ensure!(
        response.status() == StatusCode::BAD_REQUEST
            && !response.headers().contains_key("x-bitrouter-execution-id")
    );
    drop(response);
    retire(serving).await?;
    Ok(())
}

async fn protocols_and_duplicates(
    fixture: &Fixture,
    upstream: &Upstream,
    serving: &Arc<Serving>,
) -> Result<()> {
    let mut execution_ids = Vec::new();
    for (path, document) in [
        ("/v1/chat/completions", prompt("ok")),
        (
            "/v1/messages",
            json!({"model":"ok","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}),
        ),
        ("/v1/responses", json!({"model":"ok","input":"hello"})),
        (
            "/v1/decisions",
            json!({"model":"decisions","input":"evidence","questions":[{"type":"predicate","name":"q","instructions":"evaluate"}]}),
        ),
    ] {
        let key = uuid::Uuid::new_v4().to_string();
        let calls = upstream.calls.load(Ordering::SeqCst);
        let response = serving
            .router()
            .oneshot(request(
                fixture,
                path,
                document.clone(),
                Some(&key),
                "same-correlation",
            )?)
            .await?;
        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = to_bytes(response.into_body(), 65536).await?;
            anyhow::bail!(
                "HTTP protocol failed: {path} {status}: {}",
                std::str::from_utf8(&body)?
            );
        }
        let id = response.headers()["x-bitrouter-execution-id"]
            .to_str()?
            .to_owned();
        ensure!(id != "same-correlation" && !execution_ids.contains(&id));
        ensure!(response.headers()["x-bitrouter-request-id"] == "same-correlation");
        ensure!(response.headers()["location"] == format!("/v1/gateway/executions/{id}"));
        ensure!(
            !serving.custody.reconcile_one().await?,
            "response body lost execution custody"
        );
        let view = fixture.engine.query(&fixture.key.hash, &id).await?;
        ensure!(matches!(view.state, ExecutionState::Terminal { .. }));
        let bytes = to_bytes(response.into_body(), 65536).await?;
        let _: Value = serde_json::from_slice(&bytes)?;
        retire(serving).await?;
        ensure!(upstream.calls.load(Ordering::SeqCst) == calls + 1);
        let duplicate = serving
            .router()
            .oneshot(request(
                fixture,
                path,
                document.clone(),
                Some(&key),
                "new-correlation",
            )?)
            .await?;
        ensure!(
            duplicate.status() == StatusCode::ACCEPTED
                && duplicate.headers()["x-bitrouter-execution-id"] == id
        );
        ensure!(duplicate.headers()["x-bitrouter-request-id"] == "new-correlation");
        let duplicate: bitrouter_ai::gateway::execution::GatewayExecution =
            serde_json::from_slice(&to_bytes(duplicate.into_body(), 65536).await?)?;
        ensure!(duplicate.execution_id == id && duplicate.correlation_id == "same-correlation");
        retire(serving).await?;
        let mut changed = document;
        changed["model"] = json!("reject");
        let conflict = serving
            .router()
            .oneshot(request(fixture, path, changed, Some(&key), "conflict")?)
            .await?;
        ensure!(conflict.status() == StatusCode::CONFLICT);
        drop(conflict);
        retire(serving).await?;
        ensure!(upstream.calls.load(Ordering::SeqCst) == calls + 1);
        execution_ids.push(id);
    }
    for model in ["stream", "reject"] {
        let mut document = prompt(model);
        if model == "stream" {
            document["stream"] = json!(true);
        }
        let response = serving
            .router()
            .oneshot(request(
                fixture,
                "/v1/chat/completions",
                document,
                None,
                "wire-result",
            )?)
            .await?;
        ensure!(
            response.status()
                == if model == "stream" {
                    StatusCode::OK
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
        );
        let id = response.headers()["x-bitrouter-execution-id"]
            .to_str()?
            .to_owned();
        let bytes = to_bytes(response.into_body(), 65536).await?;
        if model == "stream" {
            ensure!(std::str::from_utf8(&bytes)?.contains("[DONE]"));
        }
        retire(serving).await?;
        ensure!(matches!(
            fixture.engine.query(&fixture.key.hash, &id).await?.state,
            ExecutionState::Terminal { .. }
        ));
    }
    Ok(())
}

async fn status_authorization(
    fixture: &Fixture,
    store: &Store,
    tenant: &str,
    serving: &Arc<Serving>,
) -> Result<()> {
    let key = uuid::Uuid::new_v4().to_string();
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            prompt("ok"),
            Some(&key),
            "query",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    let location = response.headers()["location"].to_str()?.to_owned();
    drop(response);
    retire(serving).await?;
    let query = |secret: &str| {
        Request::builder()
            .uri(&location)
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::empty())
    };
    let response = serving
        .router()
        .oneshot(query(&fixture.key.secret)?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    drop(response);
    let foreign_tenant = uuid::Uuid::new_v4().to_string();
    let foreign = crate::auth::keys::generate();
    upsert_user(store.database(), &foreign_tenant).await?;
    insert_api_key(
        store.database(),
        &NewApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            key_hash: foreign.hash,
            user_id: foreign_tenant,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    ensure!(
        serving
            .router()
            .oneshot(query(&foreign.secret)?)
            .await?
            .status()
            == StatusCode::NOT_FOUND
    );
    let another = crate::auth::keys::generate();
    insert_api_key(
        store.database(),
        &NewApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            key_hash: another.hash,
            user_id: tenant.into(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    let mut request = request(
        fixture,
        "/v1/chat/completions",
        prompt("ok"),
        Some(&key),
        "other-principal",
    )?;
    request.headers_mut().insert(
        "authorization",
        http::HeaderValue::from_str(&format!("Bearer {}", another.secret))?,
    );
    ensure!(serving.router().oneshot(request).await?.status() == StatusCode::CONFLICT);
    retire(serving).await?;
    // Revocation is checked afresh even for a completed idempotency binding.
    use crate::auth::entities::api_keys;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
    for active in [0, 1] {
        api_keys::Entity::update_many()
            .col_expr(api_keys::Column::Active, Expr::value(active))
            .filter(api_keys::Column::KeyHash.eq(&fixture.key.hash))
            .exec(store.database())
            .await?;
        if active == 0 {
            ensure!(
                serving
                    .router()
                    .oneshot(query(&fixture.key.secret)?)
                    .await?
                    .status()
                    == StatusCode::UNAUTHORIZED
            );
            let response = serving
                .router()
                .oneshot(self::request(
                    fixture,
                    "/v1/chat/completions",
                    prompt("ok"),
                    Some(&key),
                    "revoked",
                )?)
                .await?;
            ensure!(response.status() == StatusCode::UNAUTHORIZED);
            drop(response);
            retire(serving).await?;
        }
    }
    Ok(())
}

async fn real_transport(fixture: &Fixture, serving: &Arc<Serving>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let router = serving.router();
    let bounds = crate::gateway::shared::transport::Limits::from(&config()?);
    let task = tokio::spawn(async move {
        crate::gateway::shared::transport::serve(listener, router, bounds, async {
            let _ = stopped.await;
        })
        .await
    });
    let result = async {
        let client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .build()?;
        for model in ["ok", "stream"] {
            let mut document = prompt(model);
            document["stream"] = json!(model == "stream");
            let response = client
                .post(&endpoint)
                .bearer_auth(&fixture.key.secret)
                .json(&document)
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::OK
                    && response.headers().contains_key("x-bitrouter-execution-id")
            );
            if model == "stream" {
                ensure!(response.text().await?.contains("[DONE]"));
            } else {
                let _: Value = response.json().await?;
            }
            retire(serving).await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _ = stop.send(());
    task.await??;
    result?;
    retire(serving).await?;
    Ok(())
}
