//! An affirmative response to an old approval cannot authorize a new MCP effect.
use super::*;
use bitrouter_ai::core::protocol::{ToolOutcome, ToolResult, ToolStatus};

#[derive(Default)]
struct HeldApproval {
    entered: Notify,
    release: CancellationToken,
    command: Mutex<Option<ToolExecute>>,
}

#[async_trait]
impl NativeApproval for HeldApproval {
    async fn approve(&self, command: &ToolExecute) -> std::result::Result<bool, CoreError> {
        *self.command.lock().await = Some(command.clone());
        self.entered.notify_one();
        self.release.cancelled().await;
        Ok(true)
    }
}

async fn reply(
    State(state): State<ProviderState>,
    version: http::Version,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let attempt = state.calls.fetch_add(1, Ordering::SeqCst);
    if state
        .received
        .try_send(Observed {
            version,
            path: "/v1/chat/completions".into(),
            headers,
            body,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    // Real Chat Completions tool call followed by a terminal assistant message.
    // https://platform.openai.com/docs/api-reference/chat/object
    let choice = if attempt == 0 {
        json!({"index":0,"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"native-approval-call","type":"function",
                "function":{"name":"retained__echo","arguments":"{}"}}]},
            "finish_reason":"tool_calls"})
    } else {
        json!({"index":0,"message":{"role":"assistant","content":"finished"},"finish_reason":"stop"})
    };
    Json(
        json!({"id":format!("chatcmpl-approval-{attempt}"),"object":"chat.completion",
        "created":0,"model":"model-a","choices":[choice],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}),
    )
    .into_response()
}

async fn tool_provider() -> Result<Provider> {
    let (received, observed) = mpsc::channel(16);
    let state = ProviderState {
        received,
        calls: Arc::new(AtomicUsize::new(0)),
        release: CancellationToken::new(),
        stop: CancellationToken::new(),
        empty_choices: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        count_reply: Arc::new(std::sync::Mutex::new(None)),
        count_stall_body: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    state.release.cancel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let router = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(state.clone());
    let stop = state.stop.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
    }));
    Ok(Provider {
        url,
        received: observed,
        state,
        task: Some(task),
    })
}

#[tokio::test]
async fn native36_approval_wait_rechecks_revocation_and_scope_before_real_mcp_call() -> Result<()> {
    for change in ["allowed", "revoke", "scope"] {
        let mut provider = tool_provider().await?;
        let mut endpoint = Endpoint::start(false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let gate = fixture.service.hold_host(HostBoundary::Transport)?;
        let approval = Arc::new(HeldApproval::default());
        let release_on_failure = approval.release.clone().drop_guard();
        let checked =
            tokio::time::timeout(Duration::from_secs(25), async {
                let (headers, key) = ready(&fixture, &route).await?;
                let app = Arc::new(
                    host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                        .with_tenancy(fixture.service.clone()),
                );
                let config = endpoint.config();
                let opener = AbortOnDropHandle::new(tokio::spawn(async move {
                    NativeSession::open_authenticated(
                        app,
                        headers,
                        "tool-wait",
                        &workspace,
                        ToolMode::Coding,
                        &config,
                    )
                    .await
                }));
                let weak = observe(&gate).await?;
                let mut session = opener.await??;
                ensure!(endpoint.wire.pages.load(Ordering::SeqCst) == 1);
                let core = session.core().clone();
                let held = approval.clone();
                let running = AbortOnDropHandle::new(tokio::spawn(async move {
                    let outcome = session.run(input(), held, CancellationToken::new()).await;
                    (session, outcome)
                }));
                approval.entered.notified().await;
                let command = approval
                    .command
                    .lock()
                    .await
                    .clone()
                    .context("actual tool approval")?;
                ensure!(command.tool == "retained__echo");
                let snapshot = core.snapshot().await;
                let invocation = snapshot
                    .agents
                    .values()
                    .filter_map(|agent| agent.turn.as_ref())
                    .flat_map(|turn| &turn.invocations)
                    .find(|call| call.dispatch == command)
                    .context("durable pending invocation")?;
                ensure!(invocation.result.is_none());
                ensure!(
                    invocation
                        .tool_observations
                        .values()
                        .any(|observation| observation.status == ToolStatus::WaitingApproval)
                );
                let before = saved(&fixture).await?;
                let started = &before["journal"]["starts"][&command.invocation_id];
                ensure!(started["command"] == serde_json::to_value(&command)?);
                ensure!(started["started"] == false && started["result"].is_null());
                ensure!(provider.calls() == 1 && endpoint.wire.calls.load(Ordering::SeqCst) == 0);
                ensure!(!joined(&gate)? && fixture.service.host_slots_for_test() == 0);
                let request = provider.receive().await?;
                ensure!(request.body["model"] == "model-a");
                ensure!(
                    request
                        .headers
                        .get("authorization")
                        .is_some_and(|h| h == "Bearer owned-upstream-secret")
                );
                if change != "allowed" {
                    change_authority(&fixture, &key, &route, change).await?;
                }
                // A weak witness cannot keep the production worker alive through
                // revocation. Retain it for the final joined-state observation only now.
                let observed = weak.upgrade().context("approval wait lost its MCP owner")?;
                ensure!(
                    Arc::strong_count(&observed) > 1,
                    "test became the sole MCP owner"
                );
                ensure!(fixture.service.host_slots_for_test() == 0);
                ensure!(
                    !joined(&gate)?,
                    "authority mutation discarded a live MCP worker"
                );
                // Return true deliberately: denial must come from current authority
                // at the actual dispatch gate, not from the approval fixture.
                approval.release.cancel();
                let (session, outcome) = running.await?;
                if change == "allowed" {
                    let completed = outcome?;
                    ensure!(completed.run.as_ref().is_some_and(|run| run.status
                        == crate::storage::checkpoint::snapshot::RunStatus::Completed));
                    ensure!(
                        endpoint.wire.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 2
                    );
                    provider.receive().await?;
                } else {
                    if let Err(error) = outcome {
                        ensure!(
                            matches!(error.code,
                        bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope
                            | bitrouter_ai::core::protocol::ErrorCode::CheckpointUnavailable),
                            "unexpected post-approval failure: {error:?}"
                        );
                    }
                    ensure!(
                        endpoint.wire.calls.load(Ordering::SeqCst) == 0,
                        "{change}: a stale affirmative approval sent tools/call"
                    );
                    ensure!(
                        provider.calls() == 1,
                        "{change}: a later model step escaped authorization"
                    );
                }
                session.close().await?;
                let after = saved(&fixture).await?;
                let retained = &after["journal"]["starts"][&command.invocation_id];
                ensure!(retained["command"] == serde_json::to_value(&command)?);
                let result: ToolResult = serde_json::from_value(retained["result"].clone())?;
                ensure!(
                    result.invocation_id == command.invocation_id
                        && result.attempt_id == command.attempt_id
                );
                ensure!(
                    result.status
                        == if change == "allowed" {
                            ToolOutcome::Succeeded
                        } else {
                            ToolOutcome::NotExecuted
                        }
                );
                ensure!(
                    after["journal"]["released"] == true,
                    "admitted cleanup failed after authority changed"
                );
                ensure!(joined(&gate)? && fixture.service.host_slots_for_test() == 1);
                ensure!(fixture.service.current().is_ok());
                drop(observed);
                ensure!(
                    gate.mcp
                        .get()
                        .context("owner observer")?
                        .upgrade()
                        .is_none()
                );
                Ok(())
            })
            .await
            .context("native tool approval deadline")
            .and_then(|outcome| outcome);
        drop(release_on_failure);
        gate.release();
        let stopped = endpoint.finish().await;
        let cleaned = finish(&fixture, provider, checked).await;
        stopped?;
        cleaned?;
    }
    Ok(())
}
