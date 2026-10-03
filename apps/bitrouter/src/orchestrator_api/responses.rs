//! Managed Responses profile; hosted actions remain BitRouter extensions.
//! Reference: <https://developers.openai.com/api/docs/guides/responses-multi-agent>

mod projection;
mod stream;

#[cfg(test)]
#[path = "responses/authority_tests.rs"]
mod authority_tests;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, BETA, CoreError, DiscardableHistory, ErrorCode, Limits, RoutingSettings,
    TaskInput, ToolOutcome, ToolResult, VERSION, Verification,
};
use bitrouter_orchestrator::core::session::CoreSession;
use bitrouter_orchestrator::core::session::responses::{ResponseExchange, ResponseToolResult};
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use super::{ApiError, ManagedCoreApi, auth::Principal};

#[derive(Clone, Default)]
struct Progress {
    initial: Option<Arc<projection::Initial>>,
    outcome: Option<Result<Arc<ResponseExchange>, CoreError>>,
}

pub(super) struct Job {
    operation_id: String,
    fingerprint: String,
    progress: watch::Receiver<Progress>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MultiAgent {
    enabled: bool,
    max_concurrent_subagents: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reasoning {
    effort: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Extension {
    version: u32,
    execution: String,
    session_id: String,
    execution_epoch: u64,
    operation_id: String,
    expected_state_revision: Option<u64>,
    routing: Option<RoutingSettings>,
    limits: Option<Limits>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    required_materials: Vec<String>,
    verification: Option<Verification>,
    discardable_history: Option<DiscardableHistory>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Create {
    model: String,
    input: Value,
    previous_response_id: Option<String>,
    #[serde(default)]
    stream: bool,
    multi_agent: MultiAgent,
    bitrouter: Extension,
    reasoning: Option<Reasoning>,
    max_output_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultMetadata {
    operation_id: String,
    status: ToolOutcome,
    #[serde(default)]
    evidence: Vec<ArtifactRef>,
    workspace_revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FunctionOutput {
    #[serde(rename = "type")]
    kind: String,
    call_id: String,
    output: String,
    bitrouter: ResultMetadata,
}

pub(super) async fn intercept(
    State(api): State<ManagedCoreApi>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || request.uri().path() != "/v1/responses" {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    if !parts.headers.contains_key("bitrouter-beta") {
        // The ordinary route retains its existing body/SDK contract. Inspection
        // has a separate host bound and cannot consume managed output slots.
        let inspection = match api.shared.inspections.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return ApiError::core(ErrorCode::Busy, "response inspection capacity reached")
                    .into_response();
            }
        };
        let bytes = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            to_bytes(body, 16 * 1024 * 1024),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                return ApiError::core(
                    ErrorCode::LimitExceeded,
                    "request body exceeds inference limit",
                )
                .into_response();
            }
            Err(_) => {
                return ApiError::core(
                    ErrorCode::LimitExceeded,
                    "response inspection body timed out",
                )
                .into_response();
            }
        };
        let managed = serde_json::from_slice::<Value>(&bytes).is_ok_and(|body| {
            body.get("bitrouter").is_some() || body["multi_agent"]["enabled"] == true
        });
        if managed {
            return ApiError::core(
                ErrorCode::UnsupportedVersion,
                "managed responses require BitRouter-Beta: orchestrator_core=v1",
            )
            .into_response();
        }
        drop(inspection);
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    }
    let result = async {
        if parts
            .headers
            .get("bitrouter-beta")
            .and_then(|value| value.to_str().ok())
            != Some(BETA)
        {
            return Err(ApiError::core(
                ErrorCode::UnsupportedVersion,
                "managed responses require BitRouter-Beta: orchestrator_core=v1",
            ));
        }
        let permit = api
            .shared
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::core(
                    ErrorCode::Busy,
                    "managed response consumer capacity reached",
                )
            })?;
        let principal = Principal::authenticate(&api.shared.db, &parts.headers).await?;
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            to_bytes(body, api.shared.capabilities.limits.input_bytes as usize),
        )
        .await
        .map_err(|_| ApiError::core(ErrorCode::LimitExceeded, "managed request body timed out"))?
        .map_err(|_| {
            ApiError::core(
                ErrorCode::LimitExceeded,
                "managed request exceeds host input bound",
            )
        })?;
        let create: Create = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::core(
                ErrorCode::UnsupportedCapability,
                "invalid or unsupported managed request field",
            )
        })?;
        if !create.multi_agent.enabled
            || create.bitrouter.version != VERSION
            || create.bitrouter.execution != "managed"
        {
            return Err(ApiError::core(
                ErrorCode::UnsupportedVersion,
                "managed execution and protocol version 1 are required",
            ));
        }
        let (limits, budget) = api
            .response_scope(
                &principal,
                &create.bitrouter.session_id,
                create.bitrouter.execution_epoch,
            )
            .await?;
        if bytes.len() as u64 > limits.input_bytes {
            return Err(ApiError::core(
                ErrorCode::LimitExceeded,
                "managed request exceeds negotiated input bound",
            ));
        }
        if create
            .bitrouter
            .limits
            .as_ref()
            .is_some_and(|limits| limits.ephemeral_bytes == 0)
        {
            return Err(ApiError::core(
                ErrorCode::LimitExceeded,
                "managed responses require positive output capacity",
            ));
        }
        let streaming = create.stream;
        let scope = budget.scope(limits.ephemeral_bytes, None)?;
        // Uploading the body and looking up the session can outlive the
        // credential that authenticated the initial headers, including replay.
        principal.revalidate().await?;
        let progress = start(&api, principal, create).await?;
        if streaming {
            Ok(stream::sse(
                progress,
                budget,
                limits.ephemeral_bytes,
                scope,
                permit,
            ))
        } else {
            let exchange = wait(progress).await?;
            projection::Projection::new(&exchange)?;
            let initial = projection::Initial::new(&exchange)?;
            let scope = budget.scope(
                limits.ephemeral_bytes,
                Some(initial.run(limits.ephemeral_bytes)),
            )?;
            Ok(stream::json(exchange, scope, permit))
        }
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

async fn start(
    api: &ManagedCoreApi,
    principal: Principal,
    mut create: Create,
) -> Result<watch::Receiver<Progress>, ApiError> {
    create.stream = false;
    let fingerprint = bitrouter_orchestrator::core::checkpoint::sha256(
        &serde_json::to_vec(&create).map_err(|_| {
            ApiError::core(
                ErrorCode::UnsupportedCapability,
                "cannot encode managed request",
            )
        })?,
    );
    let mut sessions = api.shared.sessions.lock().await;
    let entry = sessions
        .get_mut(&ManagedCoreApi::key(
            &principal,
            &create.bitrouter.session_id,
        ))
        .filter(|entry| entry.connected && entry.ready)
        .ok_or_else(ApiError::unauthorized)?;
    if entry.grant.execution_epoch != create.bitrouter.execution_epoch {
        return Err(ApiError::core(
            ErrorCode::StaleEpoch,
            "response binding changed while waiting",
        ));
    }
    if let Some(job) = &entry.job {
        if job.operation_id == create.bitrouter.operation_id
            && job
                .progress
                .borrow()
                .outcome
                .as_ref()
                .is_none_or(Result::is_ok)
        {
            if job.fingerprint != fingerprint {
                return Err(ApiError::core(
                    ErrorCode::OperationConflict,
                    "active response operation has different content",
                ));
            }
            let progress = job.progress.clone();
            drop(sessions);
            // Registry contention can outlive the post-upload check. Cached
            // consumers bypass the worker, so authorize after this wait too.
            principal.revalidate().await?;
            return Ok(progress);
        }
        if job.progress.borrow().outcome.is_none() {
            return Err(ApiError::core(
                ErrorCode::Busy,
                "a managed response operation is active",
            ));
        }
    }
    let session = entry.session.clone().ok_or_else(ApiError::unavailable)?;
    let (sender, progress) = watch::channel(Progress::default());
    entry.job = Some(Job {
        operation_id: create.bitrouter.operation_id.clone(),
        fingerprint,
        progress: progress.clone(),
    });
    let shutdown = api.shared.shutdown.clone();
    // Session ownership, not the lifetime of an HTTP body, owns acceptance and
    // driving. Repeated consumers attach to this bounded per-session job.
    tokio::spawn(async move {
        let result = async {
            principal.revalidate().await?;
            let response_id = accept(&session, &create).await?;
            let exchange = session.response(&response_id).await.ok_or_else(|| ApiError::unavailable().0)?;
            let initial = Arc::new(projection::Initial::new(&exchange).map_err(|error| error.0)?);
            sender.send_modify(|progress| progress.initial = Some(initial));
            drop(exchange);
            loop {
                principal.revalidate().await?;
                match session.drive_response(&response_id).await {
                    Ok(exchange) => return Ok(Arc::new(exchange)),
                    Err(error) if error.code == ErrorCode::Busy => {
                        tokio::select! {
                            _ = shutdown.cancelled() => return Err(CoreError::rejected(ErrorCode::CheckpointUnavailable, "core host stopped")),
                            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }.await;
        sender.send_modify(|progress| progress.outcome = Some(result));
    });
    Ok(progress)
}

async fn accept(session: &CoreSession, create: &Create) -> Result<String, CoreError> {
    let extension = &create.bitrouter;
    let revision = match extension.expected_state_revision {
        Some(revision) => revision,
        None => match session.operation(&extension.operation_id).await {
            Some(receipt) => receipt.state_revision.saturating_sub(1),
            None => session.head().await.state_revision,
        },
    };
    let receipt = if let Some(previous_id) = &create.previous_response_id {
        let previous = session.response(previous_id).await.ok_or_else(|| {
            CoreError::rejected(ErrorCode::UnauthorizedScope, "unknown session response")
        })?;
        let input = previous.input.as_ref().ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::UnsupportedCapability,
                "legacy response lacks frozen request settings",
            )
        })?;
        if create.model != input.model
            || create
                .multi_agent
                .max_concurrent_subagents
                .is_some_and(|value| Some(value) != input.max_concurrent_subagents)
            || create
                .reasoning
                .as_ref()
                .is_some_and(|value| Some(&value.effort) != input.effort.as_ref())
            || create
                .max_output_tokens
                .is_some_and(|value| Some(value) != input.max_output_tokens)
            || extension
                .routing
                .as_ref()
                .is_some_and(|value| value != &input.routing)
            || extension
                .limits
                .as_ref()
                .is_some_and(|value| Some(value) != input.limits.as_ref())
            || !extension.acceptance_criteria.is_empty()
            || !extension.required_materials.is_empty()
            || extension.verification.is_some()
            || extension.discardable_history.is_some()
        {
            return Err(CoreError::rejected(
                ErrorCode::OperationConflict,
                "continuation cannot replace frozen run settings",
            ));
        }
        let outputs: Vec<FunctionOutput> = serde_json::from_value(create.input.clone())
            .map_err(|_| CoreError::rejected(ErrorCode::InvalidToolResult, "continuation input must contain function outputs with BitRouter result metadata"))?;
        let results = outputs
            .into_iter()
            .map(|output| {
                let command = previous
                    .pending
                    .get(&output.call_id)
                    .filter(|_| output.kind == "function_call_output")
                    .ok_or_else(|| {
                        CoreError::rejected(
                            ErrorCode::InvalidToolResult,
                            "unknown pending function call",
                        )
                    })?;
                Ok(ResponseToolResult {
                    operation_id: output.bitrouter.operation_id,
                    call_id: output.call_id,
                    result: ToolResult {
                        invocation_id: command.invocation_id.clone(),
                        attempt_id: command.attempt_id.clone(),
                        status: output.bitrouter.status,
                        output: output.output,
                        evidence: output.bitrouter.evidence,
                        workspace_revision: output.bitrouter.workspace_revision,
                    },
                })
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        session
            .continue_response_with_results(&extension.operation_id, revision, previous_id, results)
            .await?
    } else {
        let text = create.input.as_str().ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::UnsupportedCapability,
                "initial managed input must be a task string",
            )
        })?;
        let input = TaskInput {
            text: text.into(),
            model: create.model.clone(),
            effort: create.reasoning.as_ref().map(|value| value.effort.clone()),
            max_output_tokens: create.max_output_tokens,
            max_concurrent_subagents: Some(
                create.multi_agent.max_concurrent_subagents.unwrap_or(3),
            ),
            routing: extension.routing.clone().unwrap_or_default(),
            limits: extension.limits.clone(),
            acceptance_criteria: extension.acceptance_criteria.clone(),
            required_materials: extension.required_materials.clone(),
            verification: extension.verification.clone(),
            discardable_history: extension.discardable_history.clone(),
        };
        session
            .start_response(&extension.operation_id, revision, input)
            .await?
    };
    receipt
        .assigned_ids
        .get("response_id")
        .cloned()
        .ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::OperationConflict,
                "operation does not identify a response",
            )
        })
}

async fn wait(mut progress: watch::Receiver<Progress>) -> Result<Arc<ResponseExchange>, ApiError> {
    loop {
        if let Some(outcome) = progress.borrow_and_update().outcome.clone() {
            return outcome.map_err(ApiError);
        }
        progress
            .changed()
            .await
            .map_err(|_| ApiError::unavailable())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use bitrouter_sdk::App;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    #[tokio::test]
    async fn consumer_admission_precedes_body_poll_and_preserves_the_ordinary_route()
    -> Result<(), Box<dyn std::error::Error>> {
        let api = ManagedCoreApi::new(
            Arc::new(App::builder().build()?),
            sea_orm::DatabaseConnection::Disconnected,
            "test".into(),
        );
        let _full = api.shared.requests.clone().acquire_many_owned(16).await?;
        let polled = Arc::new(AtomicBool::new(false));
        let body_polled = polled.clone();
        let body = Body::from_stream(futures::stream::poll_fn(move |_| {
            body_polled.store(true, Ordering::SeqCst);
            std::task::Poll::<Option<Result<axum::body::Bytes, std::io::Error>>>::Pending
        }));
        let router = api.wrap(Router::new().route("/v1/responses", post(|| async { "ordinary" })));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/responses")
            .header("bitrouter-beta", BETA)
            .body(body)?;
        let rejected = router.clone().oneshot(request).await?;
        assert_eq!(rejected.status(), http::StatusCode::CONFLICT);
        assert!(!polled.load(Ordering::SeqCst));
        let ordinary = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/responses")
                    .body(Body::from("{\"model\":\"test\",\"input\":\"hello\"}"))?,
            )
            .await?;
        assert_eq!(ordinary.status(), http::StatusCode::OK);
        assert_eq!(to_bytes(ordinary.into_body(), 1024).await?, "ordinary");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_ordinary_upload_releases_inspection_capacity_at_its_deadline()
    -> Result<(), Box<dyn std::error::Error>> {
        let api = ManagedCoreApi::new(
            Arc::new(App::builder().build()?),
            sea_orm::DatabaseConnection::Disconnected,
            "test".into(),
        );
        let body = Body::from_stream(futures::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let router = api.wrap(Router::new().route("/v1/responses", post(|| async { "ordinary" })));
        let mut request = Box::pin(
            router.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/responses")
                    .body(body)?,
            ),
        );
        use futures::FutureExt;
        assert!(request.as_mut().now_or_never().is_none());
        assert_eq!(api.shared.inspections.available_permits(), 3);
        tokio::time::advance(std::time::Duration::from_secs(20)).await;
        assert_eq!(request.await?.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(api.shared.inspections.available_permits(), 4);
        Ok(())
    }
}
