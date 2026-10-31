//! Real provider handshakes exercise ownership before response headers and while
//! a completed HTTP handler's response is waiting for its receiver.

use std::{
    any::TypeId,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::{
    tenancy::{
        configuration::{Limits, ResourceRef},
        operation::ErrorCode,
    },
    types::{ApiProtocol, OperationScope},
};
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tower::ServiceExt;

use super::{Fixture, MODEL, draft, failure, headers, provision};
use crate::{
    gateway::{
        error::{BitrouterError, Result as GatewayResult},
        model::client::HttpTimeouts,
        pipeline::{
            execution::Pipeline,
            executor::HttpExecutor,
            operations::HookRegistration,
            settlement::{SettlementContext, SettlementRecorder},
            types::{PipelineInput, PipelineRequest},
        },
    },
    tenancy::execution::TenantExecution,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-lifetime\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-lifetime\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone)]
struct Provider {
    received: mpsc::Sender<(HeaderMap, Value)>,
    release: Arc<Notify>,
    stop: CancellationToken,
    calls: Arc<AtomicUsize>,
}

async fn provider(
    State(state): State<Provider>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if state.received.try_send((headers, body)).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    // The POST has been completely received, but no response headers exist yet.
    tokio::select! {
        _ = state.release.notified() => {}
        _ = state.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    ([("content-type", "text/event-stream")], SSE).into_response()
}

struct Settlement {
    request_id: String,
    principal: String,
    streamed: bool,
}

#[derive(Default)]
struct SettlementGate {
    entered: Notify,
    release: CancellationToken,
}

struct Recorder {
    recorded: Arc<Mutex<Vec<Settlement>>>,
    gate: Arc<SettlementGate>,
}

#[async_trait]
impl SettlementRecorder for Recorder {
    async fn record(&self, ctx: &mut SettlementContext) -> GatewayResult<()> {
        self.gate.entered.notify_one();
        self.gate.release.cancelled().await;
        self.recorded
            .lock()
            .map_err(|_| BitrouterError::internal("test settlement lock"))?
            .push(Settlement {
                request_id: ctx.request_id.clone(),
                principal: ctx.caller.user_id().to_owned(),
                streamed: ctx.streamed,
            });
        Ok(())
    }
}

type PendingRequest = Option<(Arc<Pipeline>, PipelineRequest)>;

#[derive(Clone)]
struct Ingress {
    request: Arc<Mutex<PendingRequest>>,
    prepared: Arc<AtomicBool>,
}

async fn ingress(State(state): State<Ingress>, Json(body): Json<Value>) -> GatewayResult<Response> {
    if body["model"] != MODEL || body["stream"] != true {
        return Err(BitrouterError::bad_request("unexpected test request"));
    }
    let (pipeline, request) = state
        .request
        .lock()
        .map_err(|_| BitrouterError::internal("test ingress lock"))?
        .take()
        .ok_or_else(|| BitrouterError::internal("test request already consumed"))?;
    // Exercise the same prepared-stream entry used by the production HTTP
    // adapter. This shim only permits adding a recorder before registration;
    // the returned body is deliberately never polled in either test.
    let stream = pipeline.execute_stream(request).await?;
    let body = Body::from_stream(stream.map(|part| {
        part.and_then(|part| {
            serde_json::to_vec(&part)
                .map_err(|_| BitrouterError::internal("test stream serialization"))
        })
    }));
    state.prepared.store(true, Ordering::Release);
    Ok(([("content-type", "text/event-stream")], body).into_response())
}

fn request_body() -> Value {
    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":true})
}

struct Invocation {
    router: Router,
    pipeline: Weak<Pipeline>,
    execution: Weak<TenantExecution>,
    request_id: String,
    recorded: Arc<Mutex<Vec<Settlement>>>,
    settlement: Arc<SettlementGate>,
    prepared: Arc<AtomicBool>,
}

async fn invocation(fixture: &Fixture, route: &ResourceRef) -> Result<Invocation> {
    let revision = draft(fixture, route, "lifecycle-draft", 25).await?;
    let credential = fixture.issue("lifecycle-execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        fixture,
        credential.secret.as_deref().context("execution secret")?,
        &revision.revision_id,
    )?;
    let submission = fixture.service.submission(&headers).map_err(failure)?;
    let execution = fixture
        .service
        .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
        .await
        .map_err(failure)?;
    let PipelineInput::Generation(prompt) =
        crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions, request_body())?
    else {
        anyhow::bail!("expected generation");
    };
    let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
    let request_id = execution.id().to_string();
    request.request_id = request_id.clone();
    request.headers = headers;
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    let mut pipeline = execution.pipeline()?;
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let settlement = Arc::new(SettlementGate::default());
    Arc::get_mut(&mut pipeline)
        .context("new pipeline already shared")?
        .settlement_recorders
        .push(HookRegistration::new(
            Arc::new(Recorder {
                recorded: recorded.clone(),
                gate: settlement.clone(),
            }),
            OperationScope::Generation,
            TypeId::of::<Recorder>(),
        ));
    execution.attach(pipeline.clone(), &mut request)?;
    let weak_pipeline = Arc::downgrade(&pipeline);
    let weak_execution = Arc::downgrade(&execution);
    let prepared = Arc::new(AtomicBool::new(false));
    let state = Ingress {
        request: Arc::new(Mutex::new(Some((pipeline, request)))),
        prepared: prepared.clone(),
    };
    // The test keeps only weak owner references. Any surviving lease after
    // abort must belong to the registered pipeline and its tracked work.
    drop(execution);
    Ok(Invocation {
        router: Router::new()
            .route("/v1/chat/completions", post(ingress))
            .with_state(state),
        pipeline: weak_pipeline,
        execution: weak_execution,
        request_id,
        recorded,
        settlement,
        prepared,
    })
}

fn retained(invocation: &Invocation, preparing: bool) -> Result<()> {
    let pipeline = invocation
        .pipeline
        .upgrade()
        .context("registered pipeline was released")?;
    let pending = pipeline.owned_tasks().context("owned task set")?.pending();
    ensure!(pending.sources > 0, "request lost its external producer");
    if preparing {
        ensure!(
            pending.executions > 0,
            "provider handshake is not tracked as owned work"
        );
    }
    ensure!(
        invocation.execution.strong_count() > 0,
        "tenant execution owner was released"
    );
    Ok(())
}

fn no_settlement(invocation: &Invocation) -> Result<()> {
    ensure!(
        invocation
            .recorded
            .lock()
            .map_err(|_| anyhow::anyhow!("test settlement lock"))?
            .is_empty(),
        "unreleased request was already settled"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum Disconnect {
    BeforeHeaders,
    UnclaimedResponse,
}

async fn exercise(disconnect: Disconnect) -> Result<()> {
    let (received, mut observed) = mpsc::channel(1);
    let release = Arc::new(Notify::new());
    let stop = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_router = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(Provider {
            received,
            release: release.clone(),
            stop: stop.clone(),
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
    let stopped = stop.clone();
    let provider_task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, provider_router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));
    let (mut fixture, route, _) = provision(&url).await?;
    fixture.service.executor = Arc::new(HttpExecutor::new(HttpTimeouts {
        connect: Duration::from_secs(2),
        read: Duration::from_secs(15),
        total: Some(Duration::from_secs(20)),
        ..HttpTimeouts::default()
    })?);
    let mut invocation = invocation(&fixture, &route).await?;
    let cleanup = invocation.settlement.release.clone().drop_guard();
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&request_body())?))?;
    // Move the only router state into the handler, including its single-use
    // request. No test-owned router can accidentally retain the source ticket.
    let router = std::mem::take(&mut invocation.router);
    let mut handler = Some(AbortOnDropHandle::new(tokio::spawn(
        router.oneshot(request),
    )));
    let checked = async {
        let (headers, body) = tokio::time::timeout(TIMEOUT, observed.recv())
            .await?
            .context("provider did not receive the POST")?;
        ensure!(
            headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer owned-upstream-secret")
        );
        ensure!(body["model"] == "model-a" && body["stream"] == true);
        ensure!(calls.load(Ordering::SeqCst) == 1);
        ensure!(!invocation.prepared.load(Ordering::Acquire));
        retained(&invocation, true)?;
        no_settlement(&invocation)?;

        match disconnect {
            Disconnect::BeforeHeaders => {
                let task = handler.take().context("HTTP handler")?;
                task.abort();
                ensure!(matches!(task.await, Err(error) if error.is_cancelled()));
                // Transport cancellation must reach the original settlement
                // callback without releasing its external producer custody.
                tokio::time::timeout(TIMEOUT, invocation.settlement.entered.notified()).await?;
                retained(&invocation, false)?;
            }
            Disconnect::UnclaimedResponse => {
                release.notify_one();
                let task = handler.as_ref().context("HTTP handler")?;
                tokio::time::timeout(TIMEOUT, async {
                    while !task.is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
                ensure!(
                    invocation.prepared.load(Ordering::Acquire),
                    "HTTP stream preparation failed"
                );
                // Do not await the handle: its successful HTTP response, and
                // unpolled stream body, remain queued for a vanished receiver.
                retained(&invocation, false)?;
            }
        }
        no_settlement(&invocation)?;
        let limits = Limits {
            active_executions: 1,
            queued_work: 0,
            learning_jobs: 0,
        };
        ensure!(
            matches!(
                fixture
                    .service
                    .admission
                    .acquire(&fixture.tenant, &limits)
                    .now_or_never(),
                Some(Err(ErrorCode::CapacityExceeded))
            ),
            "active tenant lease was released early"
        );
        {
            let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
            ensure!(
                futures::poll!(&mut drain).is_pending(),
                "registry lost unfinished invocation"
            );
        }
        let mut shutdown = Box::pin(fixture.service.shutdown());
        ensure!(
            futures::poll!(&mut shutdown).is_pending(),
            "shutdown completed before disconnect cleanup"
        );
        {
            let mut admission = Box::pin(fixture.service.admission.wait_drained());
            ensure!(
                futures::poll!(&mut admission).is_pending(),
                "shutdown lost active lease"
            );
        }
        match disconnect {
            Disconnect::BeforeHeaders => {}
            Disconnect::UnclaimedResponse => {
                drop(handler.take());
                tokio::time::timeout(TIMEOUT, invocation.settlement.entered.notified()).await?;
                retained(&invocation, false)?;
            }
        }
        // The provider's withheld headers stay blocked in BeforeHeaders. Only
        // the real settlement ACK may now release the pipeline and its lease.
        invocation.settlement.release.cancel();
        tokio::time::timeout(TIMEOUT, shutdown).await??;
        ensure!(
            invocation.pipeline.upgrade().is_none(),
            "pipeline/service cycle survived clean drain"
        );
        ensure!(
            invocation.execution.upgrade().is_none(),
            "tenant execution survived clean drain"
        );
        {
            let recorded = invocation
                .recorded
                .lock()
                .map_err(|_| anyhow::anyhow!("test settlement lock"))?;
            ensure!(recorded.len() == 1, "disconnect must settle exactly once");
            let recorded = recorded.first().context("missing settlement")?;
            ensure!(recorded.request_id == invocation.request_id);
            ensure!(recorded.principal == fixture.principal.as_str());
            ensure!(recorded.streamed == matches!(disconnect, Disconnect::UnclaimedResponse));
        }
        let receipt = crate::tenancy::receipts::store::lookup(
            &fixture.db,
            &fixture.installation,
            &fixture.tenant,
            &invocation.request_id.as_str().try_into()?,
        )
        .await?
        .context("durable disconnected receipt")?;
        ensure!(receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed);
        ensure!(receipt.attempts.len() == 1);
        if matches!(disconnect, Disconnect::BeforeHeaders) {
            ensure!(
                receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                    && receipt.attempts[0].outcome
                        == bitrouter_ai::tenancy::execution::Outcome::Unknown
            );
        }
        ensure!(
            calls.load(Ordering::SeqCst) == 1,
            "disconnect retried provider work"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    drop(cleanup);
    drop(handler);
    release.notify_one();
    stop.cancel();
    let provider_done = tokio::time::timeout(TIMEOUT, provider_task).await;
    let shutdown = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    checked?;
    provider_done.context("provider cleanup timeout")???;
    shutdown.context("fixture cleanup timeout")??;
    Ok(())
}

#[tokio::test]
async fn disconnect_before_provider_headers_retains_execution_until_settlement() -> Result<()> {
    exercise(Disconnect::BeforeHeaders).await
}

#[tokio::test]
async fn disappearing_prepared_response_receiver_settles_unpolled_stream_once() -> Result<()> {
    exercise(Disconnect::UnclaimedResponse).await
}
