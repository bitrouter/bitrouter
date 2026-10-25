//! Retained fixed headers remain private when an upstream rejects a request.

use std::{sync::Arc, sync::atomic::Ordering, time::Duration};

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
        authority::{OwnerKind, Permission, ResourceKind},
        configuration::ResourceRef,
    },
    types::ApiProtocol,
};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tower::ServiceExt;

use super::{Fixture, MODEL, draft, failure, headers, issued};
use crate::tenancy::{
    Service, actor::OperationLimits, command::Command, resources::Resources, store,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const PRIVATE_HEADER: &str = "retained-private-\"quoted\"-value";
const LICENSE_HEADER: &str = "retained-vendor-license-value";
const CREDENTIAL: &str = "owned-upstream-secret";

struct Observed {
    headers: HeaderMap,
    body: Value,
}

async fn provider(
    State(observed): State<mpsc::Sender<Observed>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing header")
    };
    let reply = json!({"error": {
        "message": format!("invalid input; private={}; license={}; auth={}",
            value("x-private"), value("x-vendor-license"), value("authorization")),
        "type": "invalid_request_error",
        "code": "provider_invalid_input",
        "param": "input",
        "detail": {"echo": value("x-private"), "limit": 17},
        "retryable": false
    }});
    if observed.try_send(Observed { headers, body }).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "unexpected provider request"})),
        );
    }
    (StatusCode::BAD_REQUEST, Json(reply))
}

async fn provision_private_headers(
    url: &str,
    protocol: &ApiProtocol,
) -> Result<(Fixture, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), [53; 32])?;
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
            {"reference":slot,"definition":{"kind":"credential","credential":{
                "type":"api_key","data":{"value":CREDENTIAL}}}},
            {"reference":route,"definition":{"kind":"route","selector":"mock:model-a","policy":null,
                "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":protocol}],
                    "headers":{"x-private":PRIVATE_HEADER,"x-vendor-license":LICENSE_HEADER},
                    "models":[{"id":"model-a"}]}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
        ]
    }))?).await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    Ok((fixture, route))
}

async fn assert_private_header_echo_is_redacted(protocol: ApiProtocol, stream: bool) -> Result<()> {
    let (path, body) = match protocol {
        ApiProtocol::ChatCompletions => (
            "/v1/chat/completions",
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
        ),
        ApiProtocol::Decisions => (
            "/v1/decisions",
            json!({"model":MODEL,"input":"evidence","questions":[{
                "type":"predicate","name":"q","instructions":"Evaluate the evidence"}]}),
        ),
        _ => anyhow::bail!("unsupported fixture protocol"),
    };
    let (observed, mut received) = mpsc::channel(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
    let provider_router = Router::new()
        .route(path, post(provider))
        .with_state(observed);
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let provider_task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, provider_router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));

    let (fixture, route) = provision_private_headers(&url, &protocol).await?;
    let revision = draft(&fixture, &route, "draft-private-headers", 25).await?;
    let mut issuance = fixture.issuance("execute-private-headers")?;
    issuance.permissions = vec![Permission::ExecutionSubmit, Permission::CredentialUse];
    let credential = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), issuance),
            )
            .await
            .map_err(failure)?,
    )?;
    let token = credential.secret.as_deref().context("credential secret")?;
    let caller = fixture.service.authenticate(token).map_err(failure)?;
    ensure!(
        fixture
            .service
            .configuration(&caller, &fixture.tenant, &revision.revision_id)
            .await
            .is_err()
    );
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request = Request::post(path).body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers(&fixture, token, &revision.revision_id)?;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let bytes = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    ensure!(
        status == StatusCode::BAD_REQUEST,
        "{protocol:?}, stream={stream}: {status}"
    );
    let reply: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        reply
            == json!({"error": {
                "message": "invalid input; private=[redacted credential]; license=[redacted credential]; auth=[redacted credential]",
                "type": "invalid_request_error", "code": "provider_invalid_input", "param": "input",
                "detail": {"echo": "[redacted credential]", "limit": 17}, "retryable": false
            }}),
        "{protocol:?}, stream={stream}: unexpected error payload"
    );
    let sent = tokio::time::timeout(TIMEOUT, received.recv())
        .await?
        .context("provider did not observe a request")?;
    ensure!(
        sent.headers
            .get("x-private")
            .is_some_and(|value| value == PRIVATE_HEADER)
    );
    ensure!(
        sent.headers
            .get("x-vendor-license")
            .is_some_and(|value| value == LICENSE_HEADER)
    );
    ensure!(
        sent.headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer owned-upstream-secret")
    );
    ensure!(sent.body["model"] == "model-a");
    if protocol == ApiProtocol::ChatCompletions {
        ensure!(sent.body["stream"].as_bool().unwrap_or(false) == stream);
    }
    ensure!(matches!(
        received.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    ensure!(fixture.service.current().is_ok());
    fixture.service.shutdown().await?;
    stop.cancel();
    tokio::time::timeout(TIMEOUT, provider_task).await???;
    Ok(())
}

#[tokio::test]
async fn generation_400_echo_redacts_retained_fixed_headers() -> Result<()> {
    assert_private_header_echo_is_redacted(ApiProtocol::ChatCompletions, false).await
}

#[tokio::test]
async fn decisions_400_echo_redacts_retained_fixed_headers() -> Result<()> {
    assert_private_header_echo_is_redacted(ApiProtocol::Decisions, false).await
}

#[tokio::test]
async fn sse_handshake_400_echo_redacts_retained_fixed_headers() -> Result<()> {
    assert_private_header_echo_is_redacted(ApiProtocol::ChatCompletions, true).await
}
