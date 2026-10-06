//! Independent HTTP/WebSocket client over the shipped application pipeline.

use super::*;
use bitrouter::auth::{db, keys};
use bitrouter::orchestrator_api::ManagedCoreApi;
use bitrouter_orchestrator::core::protocol::{ToolExecute, ToolOutcome, ToolResult};
use bitrouter_sdk::server::{AppState, build_router};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[path = "managed_api/authentication.rs"]
mod authentication;

#[path = "managed_api/pressure.rs"]
mod pressure;

#[path = "managed_api/recovery.rs"]
mod recovery;

#[path = "managed_api/provider_limits.rs"]
mod provider_limits;

#[path = "managed_api/collaboration.rs"]
mod collaboration;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    app: Arc<bitrouter_sdk::App>,
    router: axum::Router,
    base: String,
    key: String,
    other_key: String,
    db: sea_orm::DatabaseConnection,
    policy: Arc<bitrouter::policy::PolicyStore>,
    api: ManagedCoreApi,
    upstream: MockServer,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    _home: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// These clients send ephemeral fixture credentials only to loopback listeners.
// Ignore ambient proxies and reject redirects so requests stay at those listeners.
fn fixture_http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
}

async fn fixture() -> Result<Fixture> {
    fixture_with_output(None).await
}

async fn fixture_with_output(output: Option<Value>) -> Result<Fixture> {
    configured_fixture(output, None).await
}

async fn configured_fixture(output: Option<Value>, policy: Option<&str>) -> Result<Fixture> {
    configured_fixture_with_provider(output, policy, None).await
}

async fn configured_fixture_with_provider(
    output: Option<Value>,
    policy: Option<&str>,
    provider: Option<&str>,
) -> Result<Fixture> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/input_tokens"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object":"response.input_tokens","input_tokens":20})),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let done = String::from_utf8_lossy(&request.body).contains("function_call_output");
            ResponseTemplate::new(200).set_body_json(json!({
                "id":"provider_response", "object":"response", "status":"completed", "model":"served",
                "output":if let Some(output) = &output { output.clone() } else if done { json!([{"id":"provider_message","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"verified response","annotations":[]}]}]) }
                else { json!([{"id":"provider_call_item","type":"function_call","call_id":"provider_call","name":"read","arguments":"{\"path\":\"file.txt\"}","status":"completed"}]) },
                "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}
            }))
        }).mount(&upstream).await;
    let home = tempfile::tempdir()?;
    let policy_dir = home.path().join("policies");
    tokio::fs::create_dir(&policy_dir).await?;
    if let Some(policy) = policy {
        tokio::fs::write(policy_dir.join("managed-policy.yaml"), policy).await?;
    }
    let config = bitrouter_sdk::config::parse_with(
        &format!(
            r#"
inherit_defaults: false
registry:
  enabled: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
plugins:
  bitrouter-policy:
    policy_dir: '{}'
providers:
  fixture:
    api_base: {}/v1
    api_key: provider-fixture-secret
    models:
      - id: served
        api_protocol: responses
        capabilities: [tools]
        input_token_counting: responses
        token_limits:
          max_input_tokens: 4000
          max_output_tokens: 128
          context_window: 8000
models:
  fixture-model:
    endpoints:
      - provider: fixture
        service_id: served
"#,
            policy_dir.display(),
            provider.map(String::from).unwrap_or_else(|| upstream.uri())
        ),
        |_| None,
    )?;
    let assembled = bitrouter::assemble::build_app_with_path(
        &config,
        Some(&home.path().join("bitrouter.yaml")),
    )
    .await?;
    let mut credentials = Vec::new();
    for user in ["owner", "other-owner"] {
        db::upsert_user(&assembled.db, user).await?;
        let key = keys::generate();
        db::insert_api_key(
            &assembled.db,
            &db::NewApiKey {
                id: format!("key_{user}"),
                key_hash: key.hash,
                user_id: user.into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: (user == "owner" && policy.is_some()).then(|| "managed-policy".into()),
            },
        )
        .await?;
        credentials.push(key.secret);
    }
    let app = Arc::new(assembled.app);
    let api = ManagedCoreApi::new(app.clone(), assembled.db.clone(), "remote_core".into());
    let router = api.wrap(build_router(AppState {
        language_model: app.language_model().context("pipeline")?.clone(),
        mcp: app.mcp().cloned(),
        skip_auth: app.skip_auth(),
        metrics_renderer: app.metrics_renderer().cloned(),
        prompt_transforms: app.prompt_transforms().to_vec(),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server_router = router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_router).await });
    Ok(Fixture {
        app,
        router,
        base,
        key: credentials[0].clone(),
        other_key: credentials[1].clone(),
        db: assembled.db,
        policy: assembled.policy_store,
        api,
        upstream,
        server,
        _home: home,
    })
}

fn envelope(operation: &str, kind: &str, payload: Value) -> Value {
    json!({"version":1,"type":kind,"session_id":"remote_session","execution_epoch":1,"operation_id":operation,"payload":payload})
}

async fn socket(fixture: &Fixture) -> Result<Socket> {
    let mut request = format!(
        "{}/v1/orchestrator/channel",
        fixture.base.replace("http://", "ws://")
    )
    .into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", fixture.key).parse()?);
    request
        .headers_mut()
        .insert("bitrouter-beta", "orchestrator_core=v1".parse()?);
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

async fn harness(
    fixture: &Fixture,
) -> Result<(
    mpsc::Sender<Value>,
    mpsc::Receiver<ToolExecute>,
    Arc<Mutex<Store>>,
    tokio::task::JoinHandle<Result<()>>,
)> {
    harness_with_limits(fixture, Limits::default()).await
}

async fn harness_with_limits(
    fixture: &Fixture,
    limits: Limits,
) -> Result<(
    mpsc::Sender<Value>,
    mpsc::Receiver<ToolExecute>,
    Arc<Mutex<Store>>,
    tokio::task::JoinHandle<Result<()>>,
)> {
    let socket = socket(fixture).await?;
    let binding = binding(limits)?;
    let grant = binding.grant.clone();
    let first = envelope("bind", "session.bind", serde_json::to_value(binding)?);
    connected_harness(socket, grant, Arc::new(Mutex::new(Store::default())), first).await
}

fn binding(limits: Limits) -> Result<Bind> {
    let tools = vec![bitrouter_orchestrator::core::protocol::HarnessTool {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        effect: bitrouter_orchestrator::core::protocol::ToolEffect::Read,
        approval_required: false,
    }];
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&tools)?,
        tools,
        workspace_id: "workspace".into(),
        workspace_revision: None,
        permission_revision: 1,
        max_tool_output_bytes: 4096,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: Vec::new(),
    };
    let grant = OwnershipGrant {
        session_id: "remote_session".into(),
        harness_id: "remote_harness".into(),
        core_instance_id: "remote_core".into(),
        execution_epoch: 1,
    };
    Ok(Bind {
        grant,
        durable_head: DurableHead::default(),
        checkpoint: None,
        manifest,
        limits,
    })
}

async fn connected_harness(
    mut socket: Socket,
    grant: OwnershipGrant,
    store: Arc<Mutex<Store>>,
    first: Value,
) -> Result<(
    mpsc::Sender<Value>,
    mpsc::Receiver<ToolExecute>,
    Arc<Mutex<Store>>,
    tokio::task::JoinHandle<Result<()>>,
)> {
    socket.send(Message::Text(first.to_string().into())).await?;
    let retained = store.clone();
    let (ready, bound) = oneshot::channel();
    let (send, mut commands) = mpsc::channel::<Value>(4);
    let (tools, receive_tools) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        let mut ready = Some(ready);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => socket.send(Message::Text(command.to_string().into())).await?,
                    None => { socket.close(None).await?; return Ok(()); }
                },
                message = socket.next() => {
                    let Some(message) = message else { return Ok(()); };
                    let message = message?;
                    if matches!(message, Message::Ping(_) | Message::Pong(_)) { continue; }
                    if matches!(message, Message::Close(_)) { return Ok(()); }
                    let message: ServerMessage = serde_json::from_str(message.to_text()?)?;
                    match message {
                        ServerMessage::Checkpoint(batch) => {
                            let mut store = retained.lock().await;
                            let ack = batch.validate_append(&grant, &store.head, &Limits::default(), &BTreeMap::new(), store.acknowledgements.get(&batch.identity.batch_id))?;
                            if !store.acknowledgements.contains_key(&batch.identity.batch_id) {
                                store.head = ack.head(); store.batches.push(batch.clone());
                                store.acknowledgements.insert(batch.identity.batch_id.clone(), ack.clone());
                            }
                            let mut message = envelope(&format!("ack_{}", batch.identity.batch_id), "checkpoint.ack", serde_json::to_value(ack)?);
                            message["execution_epoch"] = json!(grant.execution_epoch);
                            socket.send(Message::Text(message.to_string().into())).await?;
                        }
                        ServerMessage::Head(_) => { retained.lock().await.heads_received += 1; if let Some(ready) = ready.take() { let _ = ready.send(()); } }
                        ServerMessage::ToolExecute(command) => {
                            let store = retained.lock().await;
                            let state = &store.batches.last().context("durable head")?.decode(&Limits::default())?.checkpoint.state;
                            let id = command.response_id.as_deref().context("managed exchange")?;
                            assert!(state["responses"]["exchanges"][id]["completed_state_revision"].is_u64());
                            tools.send(*command).await?;
                        }
                        ServerMessage::Receipt(_) => {}
                        ServerMessage::ToolCancel { .. } => {}
                        ServerMessage::Error(error) => anyhow::bail!("channel error: {error}"),
                        other => anyhow::bail!("unexpected channel event: {other:?}"),
                    }
                }
            }
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), bound).await??;
    Ok((send, receive_tools, store, task))
}

fn create(operation: &str) -> Value {
    json!({"model":"fixture-model","input":"inspect file","max_output_tokens":128,
        "multi_agent":{"enabled":true,"max_concurrent_subagents":3},
        "bitrouter":{"version":1,"execution":"managed","session_id":"remote_session","execution_epoch":1,"operation_id":operation}})
}

async fn post(fixture: &Fixture, key: &str, body: &Value) -> Result<reqwest::Response> {
    Ok(fixture_http_client()
        .build()?
        .post(format!("{}/v1/responses", fixture.base))
        .bearer_auth(key)
        .header("bitrouter-beta", "orchestrator_core=v1")
        .json(body)
        .send()
        .await?)
}

#[tokio::test]
async fn remote_api_binds_auth_and_normalizes_channel_http_results() -> Result<()> {
    let fixture = fixture().await?;
    let client = fixture_http_client().build()?;
    assert_eq!(
        client
            .get(format!("{}/v1/orchestrator/capabilities", fixture.base))
            .send()
            .await?
            .status(),
        401
    );
    let caps: Value = client
        .get(format!("{}/v1/orchestrator/capabilities", fixture.base))
        .bearer_auth(&fixture.key)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(caps["core_instance_id"], "remote_core");
    assert_eq!(
        post(&fixture, &fixture.key, &create("first"))
            .await?
            .status(),
        401
    );
    let (send, mut tools, store, task) = harness(&fixture).await?;
    assert_eq!(
        post(&fixture, &fixture.other_key, &create("first"))
            .await?
            .status(),
        401
    );
    let response = post(&fixture, &fixture.key, &create("first")).await?;
    let status = response.status();
    let first: Value = response.json().await?;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["status"], "completed");
    assert_eq!(first["bitrouter"]["run_status"], "waiting");
    let command = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
        .await?
        .context("tool command")?;
    let call = first["output"]
        .as_array()
        .context("items")?
        .iter()
        .find(|item| item["type"] == "function_call")
        .context("function")?;
    assert_ne!(call["call_id"], "provider_call");
    assert_eq!(call["agent"]["agent_name"], "/root");
    let repeated: Value = post(&fixture, &fixture.key, &create("first"))
        .await?
        .json()
        .await?;
    assert_eq!(repeated, first);
    let result = ToolResult {
        invocation_id: command.invocation_id,
        attempt_id: command.attempt_id,
        status: ToolOutcome::Succeeded,
        output: "file body".into(),
        evidence: Vec::new(),
        workspace_revision: None,
    };
    send.send(envelope(
        "result",
        "tool.result",
        serde_json::to_value(&result)?,
    ))
    .await?;
    // Receipt acceptance is observable in the durable store; no timing sleeps.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let store = store.lock().await;
            if store.batches.last().is_some_and(|batch| {
                batch.decode(&Limits::default()).is_ok_and(|payload| {
                    payload.checkpoint.state["operations"]["result"].is_object()
                })
            }) {
                break;
            }
            drop(store);
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let mut next = create("continue");
    next["previous_response_id"] = first["id"].clone();
    next["input"] = json!([{"type":"function_call_output","call_id":call["call_id"],"output":"file body","bitrouter":{"operation_id":"result","status":"succeeded"}}]);
    let response = post(&fixture, &fixture.key, &next).await?;
    let status = response.status();
    let done: Value = response.json().await?;
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["bitrouter"]["run_status"], "completed");
    assert_ne!(done["id"], first["id"]);
    assert_eq!(
        done["output"]
            .as_array()
            .context("output")?
            .iter()
            .filter(|item| item["type"] == "message"
                && item["content"][0]["text"] == "verified response")
            .count(),
        1
    );

    assert!(
        done["output"]
            .as_array()
            .context("items")?
            .iter()
            .any(|item| item["phase"] == "final_answer"
                && item["content"][0]["text"] == "verified response")
    );
    let records = store.lock().await;
    let decoded = records
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let serialized = serde_json::to_string(&decoded)?;
    assert!(!serialized.contains(&fixture.key));
    assert!(!serialized.contains("provider-fixture-secret"));
    let results = records
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|payload| payload.events)
        .filter(|event| event.kind == "tool.result")
        .count();
    assert_eq!(results, 1);
    drop(records);
    let calls = fixture
        .upstream
        .received_requests()
        .await
        .context("upstream")?;
    assert_eq!(
        calls
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        2
    );
    for request in &calls {
        assert!(!String::from_utf8_lossy(&request.body).contains("\"multi_agent\""));
    }
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn abandoned_sse_consumer_keeps_execution_and_replays_attributed_frames() -> Result<()> {
    let fixture = fixture().await?;
    let (send, mut tools, store, task) = harness(&fixture).await?;
    let mut body = create("streamed");
    body["stream"] = json!(true);
    let mut response = post(&fixture, &fixture.key, &body).await?;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()?
            .starts_with("text/event-stream")
    );
    let mut received = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !received.contains("response.created") {
            let chunk = response.chunk().await?.context("initial SSE frame")?;
            received.push_str(&String::from_utf8_lossy(&chunk));
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    drop(response);
    let _command = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
        .await?
        .context("authorized tool")?;
    let replay = post(&fixture, &fixture.key, &body).await?.text().await?;
    let events = replay
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        events.first().context("first event")?["type"],
        "response.created"
    );
    assert_eq!(
        events.last().context("terminal event")?["type"],
        "response.completed"
    );
    assert!(events.iter().any(
        |event| event["type"] == "response.function_call_arguments.delta"
            && event["agent"]["agent_name"] == "/root"
    ));
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence as u64);
    }
    let state = store
        .lock()
        .await
        .batches
        .last()
        .context("checkpoint")?
        .decode(&Limits::default())?
        .checkpoint
        .state;
    let serialized = state.to_string();
    assert!(!serialized.contains(&fixture.key));
    assert!(!serialized.contains("provider-fixture-secret"));
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn unmanaged_inference_stays_available_and_unknown_extensions_fail_before_execution()
-> Result<()> {
    let fixture = fixture().await?;
    let client = fixture_http_client().build()?;
    let ordinary = client
        .post(format!("{}/v1/responses", fixture.base))
        .json(&json!({"model":"fixture-model","input":"ordinary"}))
        .send()
        .await?;
    assert_eq!(ordinary.status(), 200, "{}", ordinary.text().await?);
    let (send, _tools, _store, task) = harness(&fixture).await?;
    for (field, value) in [
        ("tools", json!([])),
        ("store", json!(false)),
        ("context_management", json!({})),
    ] {
        let mut body = create("invalid");
        body[field] = value;
        assert_eq!(post(&fixture, &fixture.key, &body).await?.status(), 400);
    }
    let mut body = create("invalid");
    body["bitrouter"]["version"] = json!(2);
    assert_eq!(post(&fixture, &fixture.key, &body).await?.status(), 400);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn disconnected_existing_session_restores_unstarted_tools_on_the_same_host() -> Result<()> {
    use bitrouter_orchestrator::core::protocol::{
        Restore, RunActivityReconciliation, ToolObservation, ToolStatus,
    };
    let fixture = fixture().await?;
    let (send, mut tools, store, task) = harness(&fixture).await?;
    let response: Value = post(&fixture, &fixture.key, &create("first"))
        .await?
        .json()
        .await?;
    assert_eq!(response["status"], "completed", "{response}");
    let command = tools.recv().await.context("tool")?;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    let records = store.lock().await;
    let checkpoint = records.batches.last().context("checkpoint")?.clone();
    let state: bitrouter_orchestrator::core::session::SessionSnapshot =
        serde_json::from_value(checkpoint.decode(&Limits::default())?.checkpoint.state)?;
    let run = state.run.as_ref().context("run")?;
    let grant = OwnershipGrant {
        session_id: "remote_session".into(),
        harness_id: "remote_harness".into(),
        core_instance_id: "remote_core".into(),
        execution_epoch: 1,
    };
    let restore = Restore {
        binding: Bind {
            grant: grant.clone(),
            durable_head: records.head.clone(),
            checkpoint: Some(checkpoint),
            manifest: state.manifest,
            limits: Limits::default(),
        },
        journal_tail: Vec::new(),
        tools: vec![ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: ToolStatus::NotStarted,
            evidence: Vec::new(),
        }],
        results: Vec::new(),
        available_artifacts: Vec::new(),
        previous_owner_stopped: true,
        active_time: Some(RunActivityReconciliation {
            run_id: run.run_id.clone(),
            durable_head: records.head.clone(),
            active_ms: run.active_ms,
        }),
    };
    drop(records);
    let (send, mut tools, restored, task) = connected_harness(
        socket(&fixture).await?,
        grant,
        store,
        envelope("restore", "session.restore", serde_json::to_value(restore)?),
    )
    .await?;
    // The initial post-bind drive must redispatch this already-authorized call,
    // even though there is no HTTP consumer or subsequent channel command.
    let redelivered = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
        .await?
        .context("redelivery")?;
    assert_eq!(redelivered.invocation_id, command.invocation_id);
    assert_eq!(redelivered.attempt_id, command.attempt_id);
    let records = restored.lock().await;
    assert!(
        records.batches.iter().any(
            |batch| batch.decode(&Limits::default()).is_ok_and(|payload| payload
                .events
                .iter()
                .any(|event| event.kind == "session.restored"))
        )
    );
    drop(records);
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn restore_ack_loss_keeps_the_registered_replacement_and_exact_batch() -> Result<()> {
    use bitrouter_orchestrator::core::protocol::{
        Restore, RunActivityReconciliation, ToolObservation, ToolStatus,
    };
    for committed in [false, true] {
        let fixture = fixture().await?;
        let (send, mut tools, store, task) = harness(&fixture).await?;
        let first: Value = post(&fixture, &fixture.key, &create("first"))
            .await?
            .json()
            .await?;
        assert_eq!(first["status"], "completed", "{first}");
        let command = tools.recv().await.context("command")?;
        drop(send);
        tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
        let records = store.lock().await;
        let checkpoint = records.batches.last().context("checkpoint")?.clone();
        let state: bitrouter_orchestrator::core::session::SessionSnapshot =
            serde_json::from_value(checkpoint.decode(&Limits::default())?.checkpoint.state)?;
        let run = state.run.as_ref().context("run")?;
        let grant = OwnershipGrant {
            session_id: "remote_session".into(),
            harness_id: "remote_harness".into(),
            core_instance_id: "remote_core".into(),
            execution_epoch: 1,
        };
        let mut binding = Bind {
            grant: grant.clone(),
            durable_head: records.head.clone(),
            checkpoint: Some(checkpoint),
            manifest: state.manifest,
            limits: Limits::default(),
        };
        let restore = Restore {
            binding: binding.clone(),
            journal_tail: Vec::new(),
            tools: vec![ToolObservation {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
                status: ToolStatus::NotStarted,
                evidence: Vec::new(),
            }],
            results: Vec::new(),
            available_artifacts: Vec::new(),
            previous_owner_stopped: true,
            active_time: Some(RunActivityReconciliation {
                run_id: run.run_id.clone(),
                durable_head: records.head.clone(),
                active_ms: run.active_ms,
            }),
        };
        drop(records);
        let mut interrupted = socket(&fixture).await?;
        interrupted
            .send(Message::Text(
                envelope("restore", "session.restore", serde_json::to_value(restore)?)
                    .to_string()
                    .into(),
            ))
            .await?;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), interrupted.next())
            .await?
            .context("proposed restoration")??;
        let proposed: ServerMessage = serde_json::from_str(frame.to_text()?)?;
        let ServerMessage::Checkpoint(batch) = proposed else {
            anyhow::bail!("expected restoration checkpoint, got {proposed:?}");
        };
        assert_eq!(
            batch.decode(&Limits::default())?.events[0].kind,
            "session.restored"
        );
        let mut records = store.lock().await;
        if committed {
            let ack = batch.validate_append(
                &grant,
                &records.head,
                &Limits::default(),
                &BTreeMap::new(),
                None,
            )?;
            records.head = ack.head();
            records.batches.push(batch.clone());
            records
                .acknowledgements
                .insert(batch.identity.batch_id.clone(), ack);
        }
        binding.durable_head = records.head.clone();
        binding.checkpoint = None;
        drop(records);
        interrupted.close(None).await?;
        drop(interrupted);
        let (send, mut tools, store, task) = connected_harness(
            socket(&fixture).await?,
            grant,
            store,
            envelope("reconnect", "session.bind", serde_json::to_value(binding)?),
        )
        .await?;
        let redelivered = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
            .await?
            .context("redelivery")?;
        assert_eq!(redelivered.invocation_id, command.invocation_id);
        let records = store.lock().await;
        let copies: Vec<_> = records
            .batches
            .iter()
            .filter(|candidate| candidate.identity.batch_id == batch.identity.batch_id)
            .collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            serde_json::to_value(copies[0])?,
            serde_json::to_value(&batch)?
        );
        drop(records);
        fixture.api.shutdown().await;
        drop(send);
        tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    }
    Ok(())
}

#[tokio::test]
async fn multipart_final_answer_is_not_duplicated_in_http_projection() -> Result<()> {
    let fixture = fixture_with_output(Some(json!([{
        "id":"multipart", "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":"first ","annotations":[]},
            {"type":"output_text","text":"second","annotations":[]}]
    }])))
    .await?;
    let (send, _tools, _store, task) = harness(&fixture).await?;
    let response: Value = post(&fixture, &fixture.key, &create("multipart"))
        .await?
        .json()
        .await?;
    assert_eq!(
        response["bitrouter"]["run_status"], "completed",
        "{response}"
    );
    let items = response["output"].as_array().context("output")?;
    assert_eq!(items.len(), 2, "{response}");
    assert!(items.iter().all(|item| item["phase"] == "final_answer"));
    assert_eq!(items[0]["content"][0]["text"], "first ");
    assert_eq!(items[1]["content"][0]["text"], "second");
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn verification_is_a_client_call_and_only_success_publishes_the_final_answer() -> Result<()> {
    for succeeded in [false, true] {
        let fixture = fixture_with_output(Some(json!([{
            "id":"candidate", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text","text":"candidate answer","annotations":[]}]
        }])))
        .await?;
        let (send, mut tools, _store, task) = harness(&fixture).await?;
        let mut body = create("verified");
        body["bitrouter"]["verification"] = json!({"tool":"read","arguments":{"path":"file.txt"}});
        let first: Value = post(&fixture, &fixture.key, &body).await?.json().await?;
        assert_eq!(first["bitrouter"]["run_status"], "waiting", "{first}");
        let command = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
            .await?
            .context("verification command")?;
        assert!(command.verification);
        let items = first["output"].as_array().context("output")?;
        assert!(items.iter().all(|item| item["phase"] != "final_answer"));
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call")
            .context("verification item")?;
        assert_eq!(call["name"], "read");
        assert_eq!(call["agent"]["agent_name"], "/root");
        assert_eq!(call["bitrouter"]["verification"], true);
        assert_eq!(
            first["bitrouter"]["pending_invocations"]
                [call["call_id"].as_str().context("call ID")?]["invocation_id"],
            command.invocation_id
        );
        body["bitrouter"]["operation_id"] = json!("finish");
        body["bitrouter"]["verification"] = Value::Null;
        body["previous_response_id"] = first["id"].clone();
        body["input"] = json!([{"type":"function_call_output","call_id":call["call_id"],"output":"verification report",
            "bitrouter":{"operation_id":"verification-result","status":if succeeded {"succeeded"} else {"failed"}}}]);
        let done: Value = post(&fixture, &fixture.key, &body).await?.json().await?;
        assert_eq!(
            done["bitrouter"]["run_status"],
            if succeeded { "completed" } else { "failed" },
            "{done}"
        );
        assert_eq!(
            done["status"],
            if succeeded { "completed" } else { "failed" }
        );
        let finals: Vec<_> = done["output"]
            .as_array()
            .context("final output")?
            .iter()
            .filter(|item| item["phase"] == "final_answer")
            .collect();
        assert_eq!(finals.len(), usize::from(succeeded), "{done}");
        if succeeded {
            assert_eq!(finals[0]["content"][0]["text"], "candidate answer");
        }
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("requests")?
                .iter()
                .filter(|request| request.url.path() == "/v1/responses")
                .count(),
            1
        );
        fixture.api.shutdown().await;
        drop(send);
        tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    }
    Ok(())
}
