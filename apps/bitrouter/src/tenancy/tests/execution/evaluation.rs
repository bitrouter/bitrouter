//! Real policy execution must write into its admitted namespace.

use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::tenancy::configuration::LearningMode;
use sea_orm::{ConnectionTrait, Statement};
use std::{sync::atomic::AtomicUsize, time::Duration};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tower::ServiceExt;

mod exploration;
mod owned;
mod trajectory;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Provider {
    calls: Arc<AtomicUsize>,
    fail: bool,
    stream: bool,
}

async fn provider(State(state): State<Provider>, Json(body): Json<serde_json::Value>) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if body["model"] != "model-a" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if state.fail {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"fixture rejected","type":"invalid_request_error"}})),
        )
            .into_response();
    }
    if state.stream {
        let delta = json!({"id":"eval-stream", "object":"chat.completion.chunk", "created":0,
            "model":"model-a", "choices":[{"index":0,"delta":{"role":"assistant","content":"observed"},"finish_reason":null}]});
        let end = json!({"id":"eval-stream", "object":"chat.completion.chunk", "created":0,
            "model":"model-a", "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}});
        return (
            [("content-type", "text/event-stream")],
            format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
        )
            .into_response();
    }
    Json(json!({"id":"eval-response", "object":"chat.completion", "created":0,
        "model":"model-a", "choices":[{"index":0,"message":{"role":"assistant","content":"observed"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}))
    .into_response()
}

async fn policy_fixture(url: &str, mode: LearningMode) -> Result<(Fixture, RevisionId)> {
    let mut document = crate::policy_lock::PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [(
                "economy".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("mock:model-a".into()),
            )]
            .into(),
            default_tier: Some("economy".into()),
            ..Default::default()
        },
    );
    policy_fixture_with_document(url, mode, document, true).await
}

async fn policy_fixture_with_document(
    url: &str,
    mode: LearningMode,
    document: crate::policy_lock::PolicyLock,
    bind_route: bool,
) -> Result<(Fixture, RevisionId)> {
    policy_fixture_with_trajectory(url, mode, document, bind_route, None).await
}

async fn policy_fixture_with_trajectory(
    url: &str,
    mode: LearningMode,
    document: crate::policy_lock::PolicyLock,
    bind_route: bool,
    trajectory: Option<serde_json::Value>,
) -> Result<(Fixture, RevisionId)> {
    policy_fixture_named(url, mode, document, bind_route, trajectory, "selected").await
}

async fn policy_fixture_named(
    url: &str,
    mode: LearningMode,
    document: crate::policy_lock::PolicyLock,
    bind_route: bool,
    trajectory: Option<serde_json::Value>,
    name: &str,
) -> Result<(Fixture, RevisionId)> {
    policy_fixture_with_existing(
        Fixture::new().await?,
        url,
        mode,
        (name, document),
        bind_route,
        trajectory,
        &mut |_| {},
    )
    .await
}

async fn policy_fixture_with_existing(
    fixture: Fixture,
    url: &str,
    mode: LearningMode,
    policy: (&str, crate::policy_lock::PolicyLock),
    bind_route: bool,
    trajectory: Option<serde_json::Value>,
    started: &mut impl FnMut(&Service),
) -> Result<(Fixture, RevisionId)> {
    let (name, document) = policy;
    let (mut fixture, mut route, slot) =
        provision_protocol_with_fixture(fixture, url, "chat_completions", started).await?;
    fixture.service.shutdown().await?;
    let policy = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: "observed-policy".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let resources = super::super::super::resources::Resources::with_key(
        fixture.installation.clone(),
        [29; 32],
    )?;
    let mut imports = vec![json!({"reference":policy,
        "definition":{"kind":"policy","name":name,"document":document}})];
    if bind_route {
        route.revision_id = "r2".try_into()?;
        imports.push(json!({"reference":route,"definition":{"kind":"route","selector":"bitrouter/observed","policy":policy,
                "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]}},
                    "routers":{"observed":{"selection":{"kind":"policy","policy":name,"base_model":"mock:model-a"}}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}));
    }
    if let Some(trajectory) = trajectory {
        for artifact in &mut imports {
            if artifact["definition"]["kind"] == "route" {
                artifact["definition"]["routing"]["trajectory"] = trajectory.clone();
            }
        }
    }
    resources
        .provision_bytes(
            &fixture.db,
            &serde_json::to_vec(&json!({
                "format":1, "installation_id":fixture.installation, "resources":imports
            }))?,
        )
        .await?;
    let snapshot = super::super::super::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        super::super::super::actor::OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    started(&fixture.service);
    let mut input = fixture.configuration_input("observed-config")?;
    input.document.routes = vec![route];
    input.document.policy = Some(policy);
    input.document.learning.mode = mode;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(revision) => revision.revision_id,
        _ => anyhow::bail!("expected policy configuration"),
    };
    Ok((fixture, revision))
}

// Reading the scoped store previously used a throwaway execution. Admission now
// owns a durable no-send receipt, so dropping that execution is not synchronous
// capacity release. Join its real cleanup before a second admission; retain the
// production one-active/zero-queue limits and prove the cancelled receipt exists.
async fn scoped_eval_store(
    fixture: &Fixture,
    headers: &::http::HeaderMap,
) -> Result<crate::core::eval::store::EvalStore> {
    let execution = admitted(fixture, headers).await?;
    let id = execution.id().clone();
    let store = execution.eval_store();
    drop(execution);
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained())
        .await
        .context("scoped-store probe admission did not durably drain")?;
    let receipt = crate::tenancy::receipts::store::lookup(
        &fixture.db,
        &fixture.installation,
        &fixture.tenant,
        &id,
    )
    .await?
    .context("abandoned scoped-store probe has no durable receipt")?;
    ensure!(receipt.execution_id == id && receipt.attempts.is_empty());
    ensure!(receipt.outcome == bitrouter_ai::tenancy::execution::Outcome::Cancelled);
    ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Incomplete);
    ensure!(!receipt.usage.complete && store.list_subjects().await?.is_empty());
    ensure!(fixture.service.current().is_ok());
    Ok(store)
}

// Each caller here has a fresh database and exactly one real invocation. The
// unfiltered bounded ID query also catches an unexpected additional receipt;
// lookup validates its immutable bytes and exact owner before assertions.
async fn only_receipt(
    fixture: &Fixture,
) -> Result<bitrouter_ai::tenancy::execution::ExecutionReceipt> {
    let rows = fixture
        .db
        .query_all(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT execution_id FROM tenancy_execution_receipts LIMIT 2".to_owned(),
        ))
        .await?;
    ensure!(
        rows.len() == 1,
        "expected one durable invocation receipt, got {}",
        rows.len()
    );
    let id = rows
        .first()
        .context("receipt row")?
        .try_get::<String>("", "execution_id")?
        .try_into()?;
    crate::tenancy::receipts::store::lookup(
        &fixture.db,
        &fixture.installation,
        &fixture.tenant,
        &id,
    )
    .await?
    .context("receipt owner/integrity lookup")
}

#[tokio::test]
async fn policy_settlement_uses_owned_store_for_success_error_and_stream_only_when_enabled()
-> Result<()> {
    for (mode, fail, stream) in [
        (LearningMode::Off, false, false),
        (LearningMode::Observe, false, false),
        (LearningMode::Observe, true, false),
        (LearningMode::Observe, false, true),
    ] {
        let state = Provider {
            calls: Arc::new(AtomicUsize::new(0)),
            fail,
            stream,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let provider_router = Router::new()
            .route("/v1/chat/completions", post(provider))
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, provider_router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        let (fixture, revision) = policy_fixture(&url, mode).await?;
        let credential = fixture.issue("observed-execution").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision,
        )?;
        let store = scoped_eval_store(&fixture, &headers).await?;
        let checked: Result<()> = async {
            let router = Router::new().fallback(|| async { StatusCode::NOT_FOUND })
                .layer(axum::middleware::from_fn_with_state(fixture.service.clone(), crate::tenancy::http::dispatch));
            let mut request = Request::post("/v1/chat/completions")
                .body(Body::from(serde_json::to_vec(&json!({"model":MODEL,"stream":stream,
                    "messages":[{"role":"user","content":"Summarize this paragraph."}]}))?))?;
            *request.headers_mut() = headers;
            let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
            ensure!(response.status().is_success() != fail, "unexpected policy execution status: {}", response.status());
            let response = tokio::time::timeout(TIMEOUT, axum::body::to_bytes(response.into_body(), 1024 * 1024)).await??;
            if !fail { ensure!(String::from_utf8_lossy(&response).contains("observed")); }
            fixture.service.pipelines.close_and_drain().await?;
            ensure!(state.calls.load(Ordering::SeqCst) == 1);
            let subjects = store.list_subjects().await?;
            ensure!(subjects.len() == usize::from(mode != LearningMode::Off));
            ensure!(crate::core::eval::store::EvalStore::new(fixture.db.clone()).list_subjects().await?.is_empty());
            if let Some(subject) = subjects.first() {
                ensure!(subject.eval_id == format!("request:{}", subject.subject_id));
                ensure!(subject.subject_id != "same-correlation");
                ensure!(subject.decisions.len() == 1 && subject.decisions[0].selected_tier == "economy");
                let outcome = subject.evidence.iter().find(|item| item.evidence_id == "request-outcome").context("outcome")?;
                ensure!(outcome.attributes.get("success") == Some(&(!fail).to_string()));
                let rows = fixture.db.query_all(Statement::from_string(fixture.db.get_database_backend(),
                    "SELECT installation_id, tenant_id, creator_principal_id FROM eval_subjects".to_owned())).await?;
                ensure!(rows.len() == 1);
                ensure!(rows[0].try_get::<String>("", "installation_id")? == fixture.installation.as_str());
                ensure!(rows[0].try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
                ensure!(rows[0].try_get::<String>("", "creator_principal_id")? == fixture.principal.as_str());
            }
            Ok(())
        }.await;
        let shutdown = fixture.service.shutdown().await;
        stop.cancel();
        let joined = tokio::time::timeout(TIMEOUT, task).await;
        checked?;
        shutdown?;
        joined???;
    }
    Ok(())
}
