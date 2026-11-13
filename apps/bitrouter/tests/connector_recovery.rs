//! Process-boundary acceptance for the standalone connector authority.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::Notify;

const AUTH: &str = "connector-process-test-auth-not-for-production";
const UPSTREAM_AUTH: &str = "connector-process-upstream-credential-not-for-production";
const WAIT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Upstream {
    dispatches: AtomicUsize,
    tokenizations: AtomicUsize,
    pressure: AtomicBool,
    missing_metrics: AtomicBool,
    hold_dispatch: AtomicBool,
    hold_tokenization: AtomicBool,
    invalid_count: AtomicBool,
    oversized_count: AtomicBool,
    stream_mode: AtomicUsize,
    hold_stream_eof: AtomicBool,
    stream_terminal_sent: AtomicUsize,
    dispatch_release: Notify,
    tokenizer_release: Notify,
    stream_release: Notify,
}

struct Fixture {
    address: SocketAddr,
    state: Arc<Upstream>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Upstream::default());
        let router = Router::new()
            .route("/metrics", get(metrics))
            .route("/tokenize", post(tokenize))
            .route("/v1/chat/completions", post(generate))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.hold_dispatch.store(false, Ordering::SeqCst);
        self.state.hold_tokenization.store(false, Ordering::SeqCst);
        self.state.hold_stream_eof.store(false, Ordering::SeqCst);
        self.state.dispatch_release.notify_waiters();
        self.state.tokenizer_release.notify_waiters();
        self.state.stream_release.notify_waiters();
        self.task.abort();
    }
}

async fn metrics(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> impl IntoResponse {
    if headers.contains_key("authorization") {
        return (
            StatusCode::UNAUTHORIZED,
            "unexpected metrics credential".to_owned(),
        );
    }
    if state.missing_metrics.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    let waiting = usize::from(state.pressure.load(Ordering::SeqCst));
    (
        StatusCode::OK,
        format!(
            "vllm:num_requests_running{{model_name=\"fixture-model\",engine=\"0\"}} 0\nvllm:num_requests_waiting{{model_name=\"fixture-model\",engine=\"0\"}} {waiting}\nvllm:kv_cache_usage_perc{{model_name=\"fixture-model\",engine=\"0\"}} 0.1\n"
        ),
    )
}

async fn tokenize(
    State(state): State<Arc<Upstream>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some(format!("Bearer {UPSTREAM_AUTH}").as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"missing inference credential"})),
        );
    }
    state.tokenizations.fetch_add(1, Ordering::SeqCst);
    while state.hold_tokenization.load(Ordering::SeqCst) {
        state.tokenizer_release.notified().await;
    }
    if body["model"] != "fixture-model"
        || !body["messages"].is_array()
        || body["add_generation_prompt"] != true
        || body["continue_final_message"] != false
        || body["add_special_tokens"] != false
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid fixture input"})),
        );
    }
    let tokens: Vec<u32> = if state.oversized_count.load(Ordering::SeqCst) {
        (0..68).collect()
    } else {
        vec![1, 2, 3, 4]
    };
    let count = if state.invalid_count.load(Ordering::SeqCst) {
        100_000
    } else {
        tokens.len()
    };
    (
        StatusCode::OK,
        Json(json!({"count":count,"tokens":tokens,"max_model_len":8192})),
    )
}

async fn generate(
    State(state): State<Arc<Upstream>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some(format!("Bearer {UPSTREAM_AUTH}").as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"missing inference credential"})),
        )
            .into_response();
    }
    state.dispatches.fetch_add(1, Ordering::SeqCst);
    while state.hold_dispatch.load(Ordering::SeqCst) {
        state.dispatch_release.notified().await;
    }
    if body["model"] != "fixture-model"
        || body["max_tokens"] != 8
        || body["n"] != 1
        || body["add_generation_prompt"] != true
        || body["continue_final_message"] != false
        || body["add_special_tokens"] != false
        || (body["stream"] == true && body["stream_options"]["include_usage"] != true)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid fixture dispatch"})),
        )
            .into_response();
    }
    if body["stream"] == true {
        let mode = state.stream_mode.load(Ordering::SeqCst);
        let stream = async_stream::stream! {
            let first = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","model":"fixture-model",
                "choices":[{"index":0,"delta":{"role":"assistant","content":"fixture answer","tool_calls":[],"function_call":null},"finish_reason":null}]});
            yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {first}\n\n")));
            if mode == 1 {
                yield Ok(Bytes::from_static(b"data: {malformed fixture private body}\n\n"));
            } else {
                let finish = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","model":"fixture-model",
                    "choices":[{"index":0,"delta":{"content":null,"tool_calls":[],"function_call":null},"finish_reason":"stop"}]});
                let usage = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[],
                    "usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}});
                yield Ok(Bytes::from(format!("data: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n")));
                if mode == 2 {
                    yield Ok(Bytes::from_static(b"data: {truncated fixture private body"));
                }
                state.stream_terminal_sent.fetch_add(1, Ordering::SeqCst);
                while state.hold_stream_eof.load(Ordering::SeqCst) {
                    state.stream_release.notified().await;
                }
            }
        };
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "id":"chatcmpl-fixture", "object":"chat.completion", "created":1,
            "model":"fixture-model", "choices":[{"index":0,"message":{"role":"assistant","content":"fixture answer","tool_calls":[],"function_call":null},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}
        })),
    ).into_response()
}

fn config(directory: &Path, upstream: SocketAddr) -> Result<PathBuf> {
    let path = directory.join("connector.yaml");
    let value = json!({
        "version":1,
        "pool":{"id":"recovery-pilot","model":"fixture-model","scope":{"kind":"static_members"},"members":[{
            "id":"replica-a","inference_endpoint":format!("http://{upstream}/v1"),
            "metrics_endpoint":format!("http://{upstream}/metrics"),"engines":["0"],
            "inference_bearer_env":"CONNECTOR_RECOVERY_TEST_INFERENCE_AUTH"
        }]},
        "observation":{"timeout_ms":200,"max_body_bytes":4096,"max_sample_age_ms":1000},
        "execution":{
            "enabled":true,"exclusive_pool_owner":true,"allow_collection_only_metrics":true,
            "state_dir":directory.join("authority"),"auth_bearer_env":"CONNECTOR_RECOVERY_TEST_AUTH",
            "max_dispatch_rps":100.0,"burst":1,"max_extra_inflight":1,"max_records":100,
            "max_input_tokens":64,"max_output_tokens":16,"max_deadline_ms":30000,
            "max_request_bytes":16384,"max_response_bytes":16384,"stream_idle_timeout_ms":10000,
            "lease_ms":400,"update_interval_ms":50,"health_window_ms":100,
            "max_running_requests":10,"max_waiting_requests":0,"max_kv_cache_usage_fraction":0.8
        }
    });
    let source = serde_json::to_string_pretty(&value)?;
    bitrouter::connector::config::ConnectorConfig::from_yaml(&source)?;
    std::fs::write(&path, source)?;
    Ok(path)
}

fn command(path: &Path, address: SocketAddr, initialize: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .args(["connector", "serve", "--config"])
        .arg(path)
        .arg("--listen")
        .arg(address.to_string())
        .env("CONNECTOR_RECOVERY_TEST_AUTH", AUTH)
        .env("CONNECTOR_RECOVERY_TEST_INFERENCE_AUTH", UPSTREAM_AUTH)
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if initialize {
        command.arg("--initialize-state");
    }
    command
}

async fn available_address() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?)
}

struct Running {
    child: Child,
    base: String,
    client: reqwest::Client,
}

impl Running {
    async fn start(path: &Path, initialize: bool) -> Result<Self> {
        let address = available_address().await?;
        let child = command(path, address, initialize)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut running = Self {
            child,
            base: format!("http://{address}"),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(WAIT)
                .build()?,
        };
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(status) = running.child.try_wait()? {
                    bail!("connector exited during startup: {status}");
                }
                if running.get_status().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("connector startup deadline")??;
        Ok(running)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(AUTH)
    }

    async fn get_status(&self) -> Result<Value> {
        Ok(self
            .request(reqwest::Method::GET, "/v1/connector/status")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn wait_budget(&self, enabled: bool) -> Result<Value> {
        tokio::time::timeout(WAIT, async {
            loop {
                let status = self.get_status().await?;
                let rate = status["budget"]["max_dispatch_rps"]
                    .as_f64()
                    .context("status budget rate missing")?;
                if (rate > 0.0) == enabled {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("budget transition deadline")?
    }

    async fn record(&self, id: &str) -> Result<Value> {
        Ok(self
            .request(
                reqwest::Method::GET,
                &format!("/v1/connector/requests/{id}"),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn wait_record(&self, id: &str, expected: &str) -> Result<Value> {
        tokio::time::timeout(WAIT, async {
            loop {
                let record = self.record(id).await?;
                if record["state"] == expected {
                    return Ok(record);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("execution state transition deadline")?
    }

    async fn control(&self, operation: &str) -> Result<()> {
        self.request(reqwest::Method::POST, &format!("/v1/connector/{operation}"))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}

fn request(status: &Value, id: &str) -> Value {
    json!({"request_id":id,"pool_id":"recovery-pilot","pool_generation":status["pool_generation"],"deadline_ms":20000,
        "request":{"model":"fixture-model","messages":[{"role":"user","content":"local bounded test"}],"max_tokens":8,"stream":false}})
}

async fn wait_count(counter: &AtomicUsize, count: usize) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        while counter.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("upstream request deadline")?;
    Ok(())
}

#[tokio::test]
async fn killed_owner_recovers_unknown_without_replay_or_capacity_reset() -> Result<()> {
    let fixture = Fixture::start().await?;
    fixture.state.hold_dispatch.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut first = Running::start(&path, true).await?;
    let initial = first.wait_budget(true).await?;
    let original = request(&initial, "crash-after-send");
    let pending = first
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original);
    let response = tokio::spawn(async move { pending.send().await });
    wait_count(&fixture.state.dispatches, 1).await?;
    first.stop().await?;
    let _ = tokio::time::timeout(WAIT, response).await?;

    let mut recovered = Running::start(&path, false).await?;
    let record = recovered.record("crash-after-send").await?;
    ensure!(
        record["state"] == "unknown",
        "ambiguous dispatch was not retained: {record}"
    );
    let status = recovered.get_status().await?;
    ensure!(
        status["inflight"]["unknown"] == 1 && status["inflight"]["total"] == 1,
        "unknown occupancy lost: {status}"
    );
    ensure!(
        status["owner_epoch"] != initial["owner_epoch"],
        "owner epoch was reused"
    );
    let duplicate = recovered
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?;
    ensure!(
        duplicate.status().is_success(),
        "identical retry must return existing state"
    );
    let duplicate: Value = duplicate.json().await?;
    ensure!(duplicate["state"] == "unknown");
    let mut changed = original.clone();
    changed["request"]["messages"][0]["content"] = json!("different content");
    let conflict = recovered
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&changed)
        .send()
        .await?;
    ensure!(conflict.status() == reqwest::StatusCode::CONFLICT);
    recovered.control("pause").await?;
    recovered.control("resume").await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let next = recovered
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&status, "cannot-reuse-unknown-slot"))
        .send()
        .await?;
    ensure!(!next.status().is_success(), "unknown slot was reused");
    ensure!(
        fixture.state.dispatches.load(Ordering::SeqCst) == 1,
        "recovery or retry resent inference"
    );
    recovered.stop().await?;

    let mut again = Running::start(&path, false).await?;
    let unknown = again.record("crash-after-send").await?;
    ensure!(unknown["state"] == "unknown");
    ensure!(again.get_status().await?["inflight"]["total"] == 1);

    // Explicit trusted operator attestation after external diagnosis, not an
    // inferred result or proof that the original response reached its caller.
    fixture.state.hold_dispatch.store(false, Ordering::SeqCst);
    fixture.state.dispatch_release.notify_one();
    let mut attestation = json!({
        "attempt_id":unknown["attempt_id"],"pool_generation":unknown["pool_generation"],
        "expected_revision":unknown["revision"],"expected_state":"unknown",
        "confirmed_not_running":false,"evidence_reference":"fixture-operator-confirmed-upstream-stopped"
    });
    let denied = again
        .request(
            reqwest::Method::POST,
            "/v1/connector/requests/crash-after-send/reconcile",
        )
        .json(&attestation)
        .send()
        .await?;
    ensure!(!denied.status().is_success());
    ensure!(again.get_status().await?["inflight"]["unknown"] == 1);
    attestation["confirmed_not_running"] = json!(true);
    let reconciled = again
        .request(
            reqwest::Method::POST,
            "/v1/connector/requests/crash-after-send/reconcile",
        )
        .json(&attestation)
        .send()
        .await?
        .error_for_status()?;
    let reconciled: Value = reconciled.json().await?;
    ensure!(reconciled["state"] == "reconciled");
    ensure!(reconciled["evidence"] == "operator_confirmed_stopped");
    ensure!(again.get_status().await?["inflight"]["total"] == 0);
    let healthy = again.wait_budget(true).await?;
    let admitted = again
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&healthy, "new-id-after-reconciliation"))
        .send()
        .await?
        .error_for_status()?;
    let _: Value = admitted.json().await?;
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 2);
    let original_retry = again
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    let original_retry: Value = original_retry.json().await?;
    ensure!(original_retry["state"] == "reconciled");
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 2);
    again.stop().await?;
    let mut final_restart = Running::start(&path, false).await?;
    ensure!(final_restart.record("crash-after-send").await?["state"] == "reconciled");
    ensure!(final_restart.get_status().await?["inflight"]["total"] == 0);
    final_restart.stop().await?;
    // Retained identity remains authoritative even after the active model and
    // generation change; replay is a lookup, never a new execution authority.
    let mut changed_config: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    changed_config["pool"]["model"] = json!("replacement-model");
    std::fs::write(&path, serde_json::to_vec_pretty(&changed_config)?)?;
    let mut changed_generation = Running::start(&path, false).await?;
    ensure!(
        changed_generation.get_status().await?["pool_generation"] != initial["pool_generation"]
    );
    let retained = changed_generation
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    let retained: Value = retained.json().await?;
    ensure!(retained["state"] == "reconciled");
    ensure!(retained["pool_generation"] == initial["pool_generation"]);
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 2);
    changed_generation.stop().await?;
    Ok(())
}

#[tokio::test]
async fn pause_during_tokenization_and_primary_pressure_block_generation() -> Result<()> {
    let fixture = Fixture::start().await?;
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut connector = Running::start(&path, true).await?;
    let status = connector.wait_budget(true).await?;
    fixture
        .state
        .hold_tokenization
        .store(true, Ordering::SeqCst);
    let pending = connector
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&status, "pause-before-send"));
    let response = tokio::spawn(async move { pending.send().await });
    wait_count(&fixture.state.tokenizations, 1).await?;
    connector.control("pause").await?;
    fixture
        .state
        .hold_tokenization
        .store(false, Ordering::SeqCst);
    fixture.state.tokenizer_release.notify_one();
    let response = tokio::time::timeout(WAIT, response).await???;
    ensure!(
        !response.status().is_success(),
        "paused request was dispatched"
    );
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 0);

    fixture.state.pressure.store(true, Ordering::SeqCst);
    connector.control("resume").await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let pressured = connector.wait_budget(false).await?;
    let refusal = connector
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&pressured, "primary-is-busy"))
        .send()
        .await?;
    ensure!(!refusal.status().is_success());
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 0);

    fixture.state.pressure.store(false, Ordering::SeqCst);
    let healthy = connector.wait_budget(true).await?;
    let success = connector
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&healthy, "healthy-after-window"))
        .send()
        .await?;
    ensure!(success.status().is_success());
    let _: Value = success.json().await?;
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
    ensure!(connector.record("healthy-after-window").await?["state"] == "succeeded");

    fixture.state.missing_metrics.store(true, Ordering::SeqCst);
    let stale = connector.wait_budget(false).await?;
    let refusal = connector
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&stale, "metrics-unavailable"))
        .send()
        .await?;
    ensure!(!refusal.status().is_success());
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
    connector.stop().await?;
    Ok(())
}

#[tokio::test]
async fn missing_store_second_owner_and_invalid_token_count_fail_closed() -> Result<()> {
    let fixture = Fixture::start().await?;
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let absent = tokio::time::timeout(
        WAIT,
        command(&path, available_address().await?, false).output(),
    )
    .await??;
    ensure!(
        !absent.status.success(),
        "serve silently initialized absent recovery storage"
    );
    let mut owner = Running::start(&path, true).await?;
    let status = owner.wait_budget(true).await?;
    let second = tokio::time::timeout(
        WAIT,
        command(&path, available_address().await?, true).output(),
    )
    .await??;
    ensure!(
        !second.status.success(),
        "a second process acquired the same pool authority"
    );
    ensure!(owner.get_status().await?["owner_epoch"] == status["owner_epoch"]);
    for (method, route) in [
        (reqwest::Method::GET, "/v1/connector/status"),
        (reqwest::Method::POST, "/v1/connector/pause"),
        (reqwest::Method::POST, "/v1/connector/resume"),
        (reqwest::Method::POST, "/v1/connector/requests"),
        (reqwest::Method::GET, "/v1/connector/requests/missing"),
        (
            reqwest::Method::POST,
            "/v1/connector/requests/missing/cancel",
        ),
        (
            reqwest::Method::POST,
            "/v1/connector/requests/missing/reconcile",
        ),
    ] {
        let denied = owner
            .client
            .request(method, format!("{}{route}", owner.base))
            .send()
            .await?;
        ensure!(denied.status() == reqwest::StatusCode::UNAUTHORIZED);
    }
    ensure!(fixture.state.tokenizations.load(Ordering::SeqCst) == 0);
    fixture.state.invalid_count.store(true, Ordering::SeqCst);
    let result = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&status, "invalid-upstream-count"))
        .send()
        .await?;
    ensure!(!result.status().is_success());
    ensure!(
        fixture.state.dispatches.load(Ordering::SeqCst) == 0,
        "unverified token count reached inference"
    );
    fixture.state.invalid_count.store(false, Ordering::SeqCst);
    fixture.state.oversized_count.store(true, Ordering::SeqCst);
    let over_limit = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&request(&status, "valid-count-over-input-limit"))
        .send()
        .await?;
    ensure!(!over_limit.status().is_success());
    ensure!(
        fixture.state.dispatches.load(Ordering::SeqCst) == 0,
        "over-limit input reached generation"
    );
    for bytes in [
        &absent.stdout,
        &absent.stderr,
        &second.stdout,
        &second.stderr,
    ] {
        ensure!(!String::from_utf8_lossy(bytes).contains(AUTH));
        ensure!(!String::from_utf8_lossy(bytes).contains(UPSTREAM_AUTH));
    }
    owner.stop().await?;
    Ok(())
}

fn streaming_request(status: &Value, id: &str) -> Value {
    let mut value = request(status, id);
    value["request"]["stream"] = json!(true);
    value
}

async fn first_chunk(response: &mut reqwest::Response) -> Result<Bytes> {
    tokio::time::timeout(WAIT, response.chunk())
        .await??
        .context("expected first inference stream chunk")
}

#[tokio::test]
async fn streaming_success_waits_for_clean_upstream_eof_and_persists_before_done() -> Result<()> {
    let fixture = Fixture::start().await?;
    fixture.state.hold_stream_eof.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut owner = Running::start(&path, true).await?;
    let status = owner.wait_budget(true).await?;
    let mut response = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&streaming_request(&status, "stream-complete"))
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    tokio::time::timeout(WAIT, async {
        while !String::from_utf8_lossy(&body).contains("completion_tokens") {
            let chunk = response
                .chunk()
                .await?
                .context("stream ended before usage")?;
            body.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    wait_count(&fixture.state.stream_terminal_sent, 1).await?;
    ensure!(
        !String::from_utf8_lossy(&body).contains("[DONE]"),
        "local terminal preceded upstream EOF"
    );
    ensure!(owner.record("stream-complete").await?["state"] == "running");
    ensure!(owner.get_status().await?["inflight"]["total"] == 1);
    fixture.state.hold_stream_eof.store(false, Ordering::SeqCst);
    fixture.state.stream_release.notify_one();
    let mut saw_done = false;
    tokio::time::timeout(WAIT, async {
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&body).contains("[DONE]") {
                ensure!(
                    owner.record("stream-complete").await?["state"] == "succeeded",
                    "client terminal preceded durable completion"
                );
                saw_done = true;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(saw_done);
    ensure!(owner.get_status().await?["inflight"]["total"] == 0);
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
    owner.stop().await?;
    Ok(())
}

#[tokio::test]
async fn malformed_or_truncated_stream_retains_unknown_without_replay() -> Result<()> {
    for (mode, id) in [(1, "malformed-stream"), (2, "truncated-after-terminal")] {
        let fixture = Fixture::start().await?;
        fixture.state.stream_mode.store(mode, Ordering::SeqCst);
        let directory = tempfile::tempdir()?;
        let path = config(directory.path(), fixture.address)?;
        let mut owner = Running::start(&path, true).await?;
        let status = owner.wait_budget(true).await?;
        let original = streaming_request(&status, id);
        let response = owner
            .request(reqwest::Method::POST, "/v1/connector/requests")
            .json(&original)
            .send()
            .await?
            .error_for_status()?;
        let body = tokio::time::timeout(WAIT, response.text()).await??;
        ensure!(body.contains("event: error") && !body.contains("[DONE]"));
        ensure!(
            !body.contains("fixture private body")
                && !body.contains(AUTH)
                && !body.contains(UPSTREAM_AUTH)
        );
        owner.wait_record(id, "unknown").await?;
        let unknown = owner.get_status().await?;
        ensure!(unknown["inflight"]["unknown"] == 1 && unknown["inflight"]["total"] == 1);
        let duplicate = owner
            .request(reqwest::Method::POST, "/v1/connector/requests")
            .json(&original)
            .send()
            .await?
            .error_for_status()?;
        ensure!(duplicate.json::<Value>().await?["state"] == "unknown");
        let rejected = owner
            .request(reqwest::Method::POST, "/v1/connector/requests")
            .json(&streaming_request(&status, "cannot-reuse-stream-slot"))
            .send()
            .await?;
        ensure!(!rejected.status().is_success());
        ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
        owner.stop().await?;
        let mut restarted = Running::start(&path, false).await?;
        ensure!(restarted.record(id).await?["state"] == "unknown");
        ensure!(restarted.get_status().await?["inflight"]["total"] == 1);
        restarted.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn client_body_drop_after_upstream_send_retains_unknown_occupancy() -> Result<()> {
    let fixture = Fixture::start().await?;
    fixture.state.hold_stream_eof.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut owner = Running::start(&path, true).await?;
    let status = owner.wait_budget(true).await?;
    let original = streaming_request(&status, "disconnect-after-send");
    let mut response = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    let chunk = first_chunk(&mut response).await?;
    ensure!(String::from_utf8_lossy(&chunk).contains("fixture answer"));
    wait_count(&fixture.state.dispatches, 1).await?;
    owner
        .wait_record("disconnect-after-send", "running")
        .await?;
    drop(response);
    let unknown = owner
        .wait_record("disconnect-after-send", "unknown")
        .await?;
    ensure!(unknown["evidence"] == "client_disconnected");
    ensure!(owner.get_status().await?["inflight"]["total"] == 1);
    let duplicate = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    ensure!(duplicate.json::<Value>().await?["state"] == "unknown");
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
    owner.stop().await?;
    Ok(())
}

#[tokio::test]
async fn deadline_after_upstream_send_keeps_unknown_while_client_is_connected() -> Result<()> {
    let fixture = Fixture::start().await?;
    fixture.state.hold_stream_eof.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut owner = Running::start(&path, true).await?;
    let status = owner.wait_budget(true).await?;
    let mut original = streaming_request(&status, "deadline-after-send");
    original["deadline_ms"] = json!(2000);
    let mut response = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    let chunk = first_chunk(&mut response).await?;
    ensure!(String::from_utf8_lossy(&chunk).contains("fixture answer"));
    wait_count(&fixture.state.dispatches, 1).await?;
    owner.wait_record("deadline-after-send", "running").await?;
    // Keep the local response alive: only its explicit deadline ends execution.
    let unknown = owner.wait_record("deadline-after-send", "unknown").await?;
    ensure!(unknown["evidence"] == "deadline");
    ensure!(owner.get_status().await?["inflight"]["unknown"] == 1);
    drop(response);
    let duplicate = owner
        .request(reqwest::Method::POST, "/v1/connector/requests")
        .json(&original)
        .send()
        .await?
        .error_for_status()?;
    ensure!(duplicate.json::<Value>().await?["state"] == "unknown");
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);
    owner.stop().await?;
    Ok(())
}

fn inference_cli(path: &Path, file: &Path, endpoint: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .args(["connector", "request", "--config"])
        .arg(path)
        .arg("--file")
        .arg(file)
        .arg("--endpoint")
        .arg(endpoint)
        .env("CONNECTOR_RECOVERY_TEST_AUTH", AUTH)
        .env("RUST_LOG", "error")
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn inference_cli_success_requires_durable_success_and_unknown_exits_nonzero_without_resend()
-> Result<()> {
    let fixture = Fixture::start().await?;
    let directory = tempfile::tempdir()?;
    let path = config(directory.path(), fixture.address)?;
    let mut bounded: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    bounded["execution"]["max_response_bytes"] = json!(1024);
    std::fs::write(&path, serde_json::to_vec(&bounded)?)?;
    let mut owner = Running::start(&path, true).await?;
    let status = owner.wait_budget(true).await?;
    let mut inspect = Command::new(env!("CARGO_BIN_EXE_bro"));
    inspect
        .args(["connector", "status", "--config"])
        .arg(&path)
        .arg("--endpoint")
        .arg(&owner.base)
        .env("CONNECTOR_RECOVERY_TEST_AUTH", AUTH)
        .kill_on_drop(true);
    let control = tokio::time::timeout(WAIT, inspect.output()).await??;
    ensure!(
        control.status.success(),
        "minimum inference body cap broke control status"
    );
    ensure!(
        control.stdout.len() > 1024,
        "fixture must exercise independent control response bound"
    );
    let control: Value = serde_json::from_slice(&control.stdout)?;
    ensure!(control["pool_generation"] == status["pool_generation"]);
    let request_path = directory.path().join("request.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request(&status, "cli-completed"))?,
    )?;
    let success = tokio::time::timeout(
        WAIT,
        inference_cli(&path, &request_path, &owner.base).output(),
    )
    .await??;
    ensure!(success.status.success(), "completed CLI inference failed");
    let response: Value = serde_json::from_slice(&success.stdout)?;
    ensure!(response["choices"][0]["message"]["content"] == "fixture answer");
    ensure!(owner.record("cli-completed").await?["state"] == "succeeded");
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 1);

    fixture.state.hold_stream_eof.store(true, Ordering::SeqCst);
    let mut original = streaming_request(&status, "cli-deadline-after-send");
    original["deadline_ms"] = json!(2000);
    std::fs::write(&request_path, serde_json::to_vec(&original)?)?;
    let mut cli = inference_cli(&path, &request_path, &owner.base);
    let pending = tokio::spawn(async move { cli.output().await });
    wait_count(&fixture.state.dispatches, 2).await?;
    let unresolved = tokio::time::timeout(WAIT, pending).await???;
    ensure!(
        !unresolved.status.success(),
        "HTTP 200 made unknown CLI execution successful"
    );
    let diagnostic = String::from_utf8_lossy(&unresolved.stderr);
    ensure!(
        diagnostic.contains("unknown")
            && diagnostic.contains("request-status")
            && diagnostic.contains("cli-deadline-after-send")
    );
    let streamed = String::from_utf8_lossy(&unresolved.stdout);
    ensure!(streamed.contains("fixture answer") && streamed.contains("event: error"));
    ensure!(owner.record("cli-deadline-after-send").await?["state"] == "unknown");
    ensure!(owner.get_status().await?["inflight"]["unknown"] == 1);
    let duplicate = tokio::time::timeout(
        WAIT,
        inference_cli(&path, &request_path, &owner.base).output(),
    )
    .await??;
    ensure!(!duplicate.status.success());
    let record: Value = serde_json::from_slice(&duplicate.stdout)?;
    ensure!(record["state"] == "unknown");
    ensure!(fixture.state.dispatches.load(Ordering::SeqCst) == 2);
    for bytes in [&success.stderr, &unresolved.stderr, &duplicate.stderr] {
        let diagnostic = String::from_utf8_lossy(bytes);
        ensure!(
            !diagnostic.contains(AUTH)
                && !diagnostic.contains(UPSTREAM_AUTH)
                && !diagnostic.contains("local bounded test")
        );
    }
    owner.stop().await?;
    Ok(())
}
