use super::*;
use bitrouter_sdk::language_model::executor::HttpExecutor;
use bitrouter_sdk::language_model::native::NativeInputCount;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Counter {
    base: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    counted: Arc<Semaphore>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<(String, Value), std::io::Error> {
    let mut bytes = Vec::new();
    let (path, end, length) = loop {
        let mut part = [0u8; 4096];
        let read = socket.read(&mut part).await?;
        if read == 0 || bytes.len() > 128 * 1024 {
            return Err(std::io::Error::other("invalid fixture request"));
        }
        bytes.extend_from_slice(&part[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).map_err(std::io::Error::other)?;
            let path = headers
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .ok_or_else(|| std::io::Error::other("missing request path"))?
                .to_owned();
            assert!(headers.lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer count-fixture-key")
            }));
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>())
                })
                .transpose()
                .map_err(std::io::Error::other)?
                .ok_or_else(|| std::io::Error::other("missing content length"))?;
            break (path, end + 4, length);
        }
    };
    while bytes.len() < end + length {
        let mut part = [0u8; 4096];
        let read = socket.read(&mut part).await?;
        if read == 0 {
            return Err(std::io::Error::other("short request"));
        }
        bytes.extend_from_slice(&part[..read]);
    }
    Ok((
        path,
        serde_json::from_slice(&bytes[end..end + length]).map_err(std::io::Error::other)?,
    ))
}

async fn counter(hold: bool) -> Result<Counter, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let counted = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let task = tokio::spawn({
        let requests = requests.clone();
        let counted = counted.clone();
        let release = release.clone();
        async move {
            loop {
                let (mut socket, _) = listener.accept().await?;
                let (path, body) = read_request(&mut socket).await?;
                requests.lock().await.push((path.clone(), body.clone()));
                let model = body["model"]
                    .as_str()
                    .ok_or_else(|| std::io::Error::other("missing model"))?;
                let (status, response) = if path == "/v1/responses/input_tokens" {
                    counted.add_permits(1);
                    if hold {
                        release
                            .acquire()
                            .await
                            .map_err(std::io::Error::other)?
                            .forget();
                    }
                    match model {
                        "unavailable" => (
                            503,
                            json!({"error":"count-fixture-key must never be persisted"}),
                        ),
                        "invalid" => (
                            200,
                            json!({"object":"response.input_tokens","input_tokens":-1}),
                        ),
                        "wrong-object" => (200, json!({"object":"response","input_tokens":1})),
                        "overflow" => (
                            200,
                            json!({"object":"response.input_tokens","input_tokens":u64::MAX}),
                        ),
                        _ => (
                            200,
                            json!({"object":"response.input_tokens","input_tokens":100}),
                        ),
                    }
                } else {
                    assert_eq!(path, "/v1/responses");
                    // A real Responses adapter consumes the successful terminal.
                    (
                        200,
                        json!({"id":"resp_count_fixture","object":"response","status":"completed",
                        "model":model,"output":[{"type":"message","id":"msg_1","role":"assistant",
                            "status":"completed","content":[{"type":"output_text","text":"Counted task complete.","annotations":[]}]}],
                        "usage":{"input_tokens":100,"output_tokens":12,"total_tokens":112}}),
                    )
                };
                let response = response.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await?;
            }
        }
    });
    Ok(Counter {
        base,
        requests,
        counted,
        release,
        task,
    })
}

async fn session_for(
    counter: &Counter,
    models: &[(&str, Value)],
    harness: Arc<Harness>,
) -> Result<CoreSession, Box<dyn std::error::Error>> {
    let declared = models.iter().map(|(id, limits)| json!({
        "id":id,"api_protocol":"responses","capabilities":["tools"],"input_token_counting":"responses","token_limits":limits,
    })).collect::<Vec<_>>();
    let endpoints = models
        .iter()
        .map(|(id, _)| json!({"provider":"counter","service_id":id}))
        .collect::<Vec<_>>();
    let config = serde_json::from_value(json!({
        "providers":{"counter":{"api_base":counter.base,"api_key":"count-fixture-key","models":declared}},
        "models":{"fixture-model":{"endpoints":endpoints}},
    }))?;
    let table = bitrouter_sdk::config::ConfigRoutingTable::from_config(config);
    let executor = Arc::new(HttpExecutor::with_defaults()?);
    let app = App::builder()
        .language_model(|builder| {
            builder.routing_table(Arc::new(table)).executor(executor);
        })
        .build()?;
    bind_app(Arc::new(app), harness).await
}

fn limits(input: u64, window: u64) -> Value {
    json!({"max_input_tokens":input,"max_output_tokens":128,"context_window":window})
}

async fn start_counted(session: &CoreSession) -> TestResult {
    let mut task = input();
    task.max_output_tokens = Some(128);
    task.text = "Count 中文 and emoji 🦀 plus tool schemas.".into();
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    Ok(())
}

#[tokio::test]
async fn provider_count_filters_input_and_combined_capacity_before_generation() -> TestResult {
    let counter = counter(false).await?;
    let harness = Arc::new(Harness::new(None, None));
    let session = session_for(
        &counter,
        &[
            ("input-too-large", limits(99, 4096)),
            ("window-too-small", limits(1000, 227)),
            ("unavailable", limits(1000, 4096)),
            ("fit", limits(100, 228)),
        ],
        harness.clone(),
    )
    .await?;
    start_counted(&session).await?;
    let state = session.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let step = &state.root_turn().ok_or("missing turn")?.steps[0];
    assert_eq!(step.input_counts.len(), 4);
    assert_eq!(
        step.attempts
            .iter()
            .map(|attempt| attempt.index)
            .collect::<Vec<_>>(),
        [3]
    );
    let decision = step.decision.as_ref().ok_or("missing decision")?;
    assert_eq!(
        decision.routes[0].rejection_reasons,
        ["input_limit_exceeded"]
    );
    assert_eq!(
        decision.routes[1].rejection_reasons,
        ["context_window_exceeded"]
    );
    assert_eq!(
        decision.routes[2].rejection_reasons,
        ["input_token_count_unavailable"]
    );
    assert!(decision.routes[3].rejection_reasons.is_empty());
    assert!(decision.routes[3].unverified_constraints.is_empty());
    assert!(
        matches!(step.input_counts[3].report.as_ref().map(|r| &r.outcome),
        Some(NativeInputCount::Counted { input_tokens:100, source, .. }) if source == "provider_responses_input_tokens")
    );
    let requests = counter.requests.lock().await;
    assert_eq!(requests.len(), 5);
    for (path, body) in &requests[..4] {
        assert_eq!(path, "/v1/responses/input_tokens");
        assert!(
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        );
        assert!(body["input"].to_string().contains("中文"));
        assert!(body.get("max_output_tokens").is_none());
    }
    assert_eq!(requests[4].0, "/v1/responses");
    assert_eq!(requests[4].1["model"], "fit");
    for key in [
        "model",
        "instructions",
        "input",
        "tools",
        "tool_choice",
        "reasoning",
    ] {
        assert_eq!(requests[3].1.get(key), requests[4].1.get(key));
    }
    assert_eq!(state.run.as_ref().map(|run| run.model_attempts), Some(1));
    assert!(!serde_json::to_string(&state)?.contains("count-fixture-key"));
    let kinds = harness.committed_kinds().await?;
    let outcome = kinds
        .iter()
        .rposition(|kind| kind == "model.input_count.outcome")
        .ok_or("missing count outcome")?;
    let plan = kinds
        .iter()
        .position(|kind| kind == "model.plan")
        .ok_or("missing plan event")?;
    assert!(outcome < plan);
    Ok(())
}

#[tokio::test]
async fn invalid_or_unavailable_count_never_falls_back_to_unknown_capacity() -> TestResult {
    for model in ["unavailable", "invalid", "wrong-object", "overflow"] {
        let counter = counter(false).await?;
        let session = session_for(
            &counter,
            &[(model, limits(u64::MAX, u64::MAX))],
            Arc::new(Harness::new(None, None)),
        )
        .await?;
        start_counted(&session).await?;
        let state = session.drive().await?;
        assert_eq!(
            state.run.as_ref().map(|run| run.status),
            Some(RunStatus::Failed)
        );
        let step = &state.root_turn().ok_or("missing turn")?.steps[0];
        assert!(step.attempts.is_empty());
        assert_eq!(
            step.application
                .as_ref()
                .and_then(|applied| applied.reason.as_ref())
                .map(|error| error.code),
            Some(ErrorCode::NoFeasibleRoute)
        );
        assert_eq!(counter.requests.lock().await.len(), 1);
        assert!(!serde_json::to_string(&state)?.contains("count-fixture-key"));
    }
    Ok(())
}

#[tokio::test]
async fn count_intent_ack_precedes_network_and_outcome_ack_precedes_generation() -> TestResult {
    for barrier in ["model.input_count.intent", "model.input_count.outcome"] {
        let counter = counter(false).await?;
        let harness = Arc::new(Harness::new(None, Some(barrier)));
        let session = session_for(&counter, &[("fit", limits(100, 228))], harness.clone()).await?;
        start_counted(&session).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        assert_eq!(
            counter.requests.lock().await.len(),
            if barrier.ends_with("intent") { 0 } else { 1 }
        );
        assert!(
            session
                .snapshot()
                .await
                .root_turn()
                .ok_or("missing turn")?
                .steps[0]
                .attempts
                .is_empty()
        );
        harness.hold_enabled.store(false, Ordering::SeqCst);
        harness.resume.add_permits(1);
        let state = tokio::time::timeout(Duration::from_secs(5), driver).await???;
        assert_eq!(
            state.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
    }
    Ok(())
}

#[tokio::test]
async fn changed_signals_during_count_reject_the_plan_before_generation() -> TestResult {
    let counter = counter(true).await?;
    let session = session_for(
        &counter,
        &[("fit", limits(100, 228))],
        Arc::new(Harness::new(None, None)),
    )
    .await?;
    start_counted(&session).await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), counter.counted.acquire())
        .await??
        .forget();
    let mut update = signal_update(&session, Vec::new()).await;
    update.manifest.permission_revision += 1;
    session.signals("changed", update).await?;
    counter.release.add_permits(1);
    let state = tokio::time::timeout(Duration::from_secs(5), driver).await???;
    let step = &state.root_turn().ok_or("missing turn")?.steps[0];
    assert!(step.input_counts[0].report.is_some());
    assert!(step.attempts.is_empty());
    assert_eq!(
        step.application
            .as_ref()
            .and_then(|applied| applied.reason.as_ref())
            .map(|error| error.code),
        Some(ErrorCode::StaleRevision)
    );
    assert_eq!(counter.requests.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancellation_and_disconnect_during_count_prevent_more_provider_work() -> TestResult {
    for disconnect in [false, true] {
        let counter = counter(true).await?;
        let session = session_for(
            &counter,
            &[("fit", limits(100, 228))],
            Arc::new(Harness::new(None, None)),
        )
        .await?;
        start_counted(&session).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(5), counter.counted.acquire())
            .await??
            .forget();
        if disconnect {
            session.disconnect().await;
        } else {
            let run_id = session.snapshot().await.run.ok_or("missing run")?.run_id;
            session
                .cancel_run("cancel", session.head().await.state_revision, &run_id)
                .await?;
        }
        counter.release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), driver).await??;
        if disconnect {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result?.run.as_ref().map(|run| run.status),
                Some(RunStatus::Cancelled)
            );
        }
        assert!(
            session
                .snapshot()
                .await
                .root_turn()
                .ok_or("missing turn")?
                .steps[0]
                .attempts
                .is_empty()
        );
        assert_eq!(counter.requests.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn count_outcome_ack_failure_blocks_generation_and_later_counts() -> TestResult {
    let counter = counter(false).await?;
    let session = session_for(
        &counter,
        &[("fit", limits(100, 228)), ("fallback", limits(100, 228))],
        Arc::new(Harness::new(Some("model.input_count.outcome"), None)),
    )
    .await?;
    start_counted(&session).await?;
    assert!(session.drive().await.is_err());
    let state = session.snapshot().await;
    let step = &state.root_turn().ok_or("missing turn")?.steps[0];
    assert!(step.attempts.is_empty());
    assert_eq!(step.input_counts.len(), 1);
    assert!(step.input_counts[0].report.is_none());
    assert_eq!(counter.requests.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn counting_consumes_active_time_without_consuming_generation_attempts() -> TestResult {
    let counter = counter(true).await?;
    let session = session_for(
        &counter,
        &[("fit", limits(100, 228))],
        Arc::new(Harness::new(None, None)),
    )
    .await?;
    let mut task = input();
    task.max_output_tokens = Some(128);
    task.limits = Some(Limits {
        active_seconds: 1,
        ..Limits::default()
    });
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(Duration::from_secs(5), counter.counted.acquire())
        .await??
        .forget();
    tokio::time::sleep(Duration::from_millis(1050)).await;
    counter.release.add_permits(1);
    let state = tokio::time::timeout(Duration::from_secs(5), driver).await???;
    let run = state.run.as_ref().ok_or("missing run")?;
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.active_ms >= 1000);
    assert_eq!(run.model_attempts, 0);
    assert_eq!(counter.requests.lock().await.len(), 1);
    Ok(())
}
