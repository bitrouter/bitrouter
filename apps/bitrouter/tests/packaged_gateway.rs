//! Bounded A03/A04/A14 acceptance using two supplied, independently built executables.
//! No application library is imported or assembled by this test.

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{body::Body, extract::Request, response::Response, routing::any};
use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};
use bitrouter_sdk::{
    control::{ControlClient, ControlOptions},
    gateway::{GatewayClient, TransportOptions},
};
use futures::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, task::JoinHandle};

const TEXT: &str = "hello from packaged gateway";
const PROVIDER_KEY: &str = "fixture-provider-secret";
const DATABASE: &str = "sqlite://gateway.db?mode=rwc";

/// Every child, including setup/stop/consumer commands, is killed AND reaped on
/// error or unwind. `serve` stays foreground; no detached daemon is launched.
struct OwnedChild {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl OwnedChild {
    fn spawn(command: &mut Command, logs: &Path, name: &str) -> Result<Self> {
        let stdout = logs.join(format!("{name}.stdout"));
        let stderr = logs.join(format!("{name}.stderr"));
        let child = command
            .stdin(Stdio::null())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .spawn()
            .with_context(|| format!("spawn {name}"))?;
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }

    async fn wait(&mut self, limit: Duration) -> Result<ExitStatus> {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok::<_, anyhow::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("owned process deadline exceeded")?
    }

    async fn success(&mut self, limit: Duration) -> Result<()> {
        let status = self.wait(limit).await?;
        ensure!(
            status.success(),
            "child failed ({status}): {}",
            std::fs::read_to_string(&self.stderr)?
        );
        Ok(())
    }

    fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(&self.stdout)?)?)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            if let Err(error) = self.child.kill() {
                eprintln!("owned child termination failed: {error}");
            }
            if let Err(error) = self.child.wait() {
                eprintln!("owned child reap failed: {error}");
            }
        }
    }
}

fn binary(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name).with_context(|| format!(
        "{name} is required when selecting this ignored acceptance test; see tests/fixtures/sdk_consumer/README.md"
    ))?);
    ensure!(path.is_absolute(), "{name} must be an absolute path");
    ensure!(
        path.is_file(),
        "{name} must name an existing executable file"
    );
    Ok(path)
}

fn isolated_command(binary: &Path, directory: &Path) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(directory).env_clear();
    // Required by the Windows process/runtime loader, not service discovery.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
}

fn verify_package(binary: &Path, identity: &ArtifactIdentity) -> Result<()> {
    let manifest = binary
        .parent()
        .context("gateway executable has no parent directory")?
        .join("manifest.json");
    let manifest: Value = serde_json::from_slice(&std::fs::read(manifest)?)?;
    ensure!(
        serde_json::from_value::<ArtifactIdentity>(manifest["identity"].clone())? == *identity,
        "archive manifest does not match executable identity"
    );
    ensure!(
        binary.file_name().and_then(|name| name.to_str()) == manifest["binary"]["path"].as_str(),
        "archive manifest names another executable"
    );
    let mut file = File::open(binary)?;
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
        size += length as u64;
    }
    ensure!(
        manifest["binary"]["bytes"] == size
            && manifest["binary"]["sha256"] == hex::encode(hash.finalize()),
        "archive binary does not match its recorded size and SHA-256"
    );
    Ok(())
}

fn host_command(binary: &Path, home: &Path, control_token: &str) -> Command {
    let mut command = isolated_command(binary, home);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("BITROUTER_HOME", home.join("runtime"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("BITROUTER_PACKAGED_CONTROL_TOKEN", control_token);
    command
}

struct UpstreamCall {
    path: String,
    body: Value,
    authorized: bool,
}

struct MockUpstream {
    endpoint: String,
    calls: Arc<Mutex<Vec<UpstreamCall>>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn sse(value: Value) -> String {
    if let Some(kind) = value["type"].as_str() {
        format!("event: {kind}\ndata: {value}\n\n")
    } else {
        format!("data: {value}\n\n")
    }
}

fn chat_chunk(text: &str, finish: Value) -> Value {
    json!({
        "id": "chat-fixture", "object": "chat.completion.chunk", "created": 0,
        "model": "fixture-chat",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": finish}]
    })
}

fn responses_reply() -> Value {
    json!({
        "id": "response-fixture", "object": "response", "created_at": 0,
        "model": "fixture-responses", "status": "completed",
        "output": [{"id": "message-fixture", "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": TEXT, "annotations": []}]}],
        "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18,
            "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}
    })
}

fn mock_reply(path: &str, body: &Value) -> Result<Response> {
    let streaming = body["stream"] == true;
    let response = Response::builder().status(200);
    if path == "/chat/completions"
        && streaming
        && body["messages"][0]["content"] == "interrupt acceptance"
    {
        // Flush real content, then fail the HTTP body (no finish/[DONE]). The
        // delay prevents the transport from collapsing this into a pre-body 502.
        let first = futures::stream::once(async {
            Ok::<_, std::io::Error>(sse(chat_chunk("partial", Value::Null)).into_bytes())
        });
        let broken = futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Err::<Vec<u8>, _>(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "fixture interruption",
            ))
        });
        return Ok(response
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(first.chain(broken)))?);
    }
    let json_body = match path {
        "/chat/completions" => json!({
            "id": "chat-fixture", "object": "chat.completion", "created": 0, "model": "fixture-chat",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": TEXT}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        }),
        "/messages" => json!({
            "id": "messages-fixture", "type": "message", "role": "assistant", "model": "fixture-messages",
            "content": [{"type": "text", "text": TEXT}], "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 11, "output_tokens": 7}
        }),
        "/responses" => responses_reply(),
        "/decisions" => json!({
            "model": "fixture-decisions", "answers": [{"type": "predicate", "name": null, "probability": 0.75}],
            "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18,
                "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}}
        }),
        _ => anyhow::bail!("unexpected upstream path"),
    };
    if !streaming {
        return Ok(response
            .header("content-type", "application/json")
            .body(Body::from(json_body.to_string()))?);
    }
    let events = match path {
        "/chat/completions" => {
            let mut terminal = chat_chunk("", json!("stop"));
            terminal["usage"] =
                json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18});
            sse(chat_chunk(TEXT, Value::Null)) + &sse(terminal) + "data: [DONE]\n\n"
        }
        "/messages" => {
            let mut start = json_body;
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            start["usage"]["output_tokens"] = json!(0);
            [
                json!({"type": "message_start", "message": start}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": TEXT}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 7}}),
                json!({"type": "message_stop"}),
            ].into_iter().map(sse).collect()
        }
        "/responses" => {
            let mut start = json_body.clone();
            start["status"] = json!("in_progress");
            start["output"] = json!([]);
            start["usage"] = Value::Null;
            [
                json!({"type": "response.created", "sequence_number": 0, "response": start}),
                json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
                    "item": {"id": "message-fixture", "type": "message", "role": "assistant", "status": "in_progress", "content": []}}),
                json!({"type": "response.content_part.added", "sequence_number": 2, "output_index": 0, "item_id": "message-fixture", "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []}}),
                json!({"type": "response.output_text.delta", "sequence_number": 3, "output_index": 0, "item_id": "message-fixture", "content_index": 0, "delta": TEXT}),
                json!({"type": "response.output_text.done", "sequence_number": 4, "output_index": 0, "item_id": "message-fixture", "content_index": 0, "text": TEXT}),
                json!({"type": "response.content_part.done", "sequence_number": 5, "output_index": 0, "item_id": "message-fixture", "content_index": 0, "part": json_body["output"][0]["content"][0]}),
                json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 0, "item": json_body["output"][0]}),
                json!({"type": "response.completed", "sequence_number": 7, "response": json_body}),
            ].into_iter().map(sse).collect()
        }
        _ => anyhow::bail!("unexpected upstream stream"),
    };
    Ok(response
        .header("content-type", "text/event-stream")
        .body(Body::from(events))?)
}

async fn mock_upstream() -> Result<MockUpstream> {
    mock_upstream_with_key(PROVIDER_KEY).await
}

async fn mock_upstream_with_key(provider_key: &str) -> Result<MockUpstream> {
    let provider_key = provider_key.to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let router = axum::Router::new().fallback(any(move |request: Request| {
        let calls = recorded.clone();
        let provider_key = provider_key.clone();
        async move {
            let result: Result<Response> = async {
                let (parts, body) = request.into_parts();
                let authorized = parts
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == format!("Bearer {provider_key}"))
                    || parts
                        .headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        == Some(provider_key.as_str());
                // Count arrival before parsing: malformed/replayed requests
                // must not disappear from the dispatch evidence.
                let index = {
                    let mut calls = calls.lock().await;
                    let index = calls.len();
                    calls.push(UpstreamCall {
                        path: parts.uri.path().into(),
                        body: Value::Null,
                        authorized,
                    });
                    index
                };
                let body = axum::body::to_bytes(body, 1024 * 1024).await?;
                let body: Value = serde_json::from_slice(&body)?;
                calls.lock().await[index].body = body.clone();
                ensure!(parts.method == axum::http::Method::POST && authorized);
                mock_reply(parts.uri.path(), &body)
            }
            .await;
            match result {
                Ok(response) => response,
                Err(_) => {
                    let mut response = Response::new(Body::from("unexpected fixture request"));
                    *response.status_mut() = axum::http::StatusCode::BAD_REQUEST;
                    response
                }
            }
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(MockUpstream {
        endpoint,
        calls,
        task,
    })
}

async fn ready(server: &mut OwnedChild, inference: &str, control: &str, token: &str) -> Result<()> {
    let gateway = GatewayClient::with_options(
        inference,
        None,
        TransportOptions {
            connect_timeout: Duration::from_millis(300),
            request_timeout: Some(Duration::from_secs(1)),
            ..TransportOptions::default()
        },
    )?;
    let operator = ControlClient::with_options(
        control,
        token,
        ControlOptions {
            connect_timeout: Duration::from_millis(300),
            request_timeout: Duration::from_secs(1),
            ..ControlOptions::default()
        },
    )?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            ensure!(
                server.child.try_wait()?.is_none(),
                "packaged gateway exited during startup"
            );
            if gateway.health().await.is_ok()
                && let Ok(status) = operator.status().await
                && status.running
            {
                ensure!(
                    status.pid == Some(server.child.id()),
                    "endpoint belongs to another process"
                );
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("packaged gateway readiness deadline exceeded")?
}

async fn closed(address: SocketAddr) -> Result<()> {
    // Windows may take several seconds to report a refused loopback connection.
    // Neither a successful connection nor a timeout counts as a closed listener.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .with_context(|| format!("timed out checking listener closure at {address}"))?;
    ensure!(
        matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused),
        "listener did not refuse connections after graceful shutdown at {address}: {result:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires an unpacked gateway archive and a separately built SDK consumer; see fixtures/sdk_consumer/README.md"]
async fn packaged_gateway_independent_sdk_consumer() -> Result<()> {
    // Never silently skip when explicitly selected, and never substitute the
    // workspace CARGO_BIN_EXE_bro for the candidate archive's executable.
    let gateway_binary = binary("BITROUTER_TEST_GATEWAY_BINARY")?;
    let consumer_binary = binary("BITROUTER_TEST_SDK_CONSUMER_BINARY")?;
    ensure!(gateway_binary != consumer_binary);
    let server_home = tempfile::tempdir()?;
    let consumer_home = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let upstream = mock_upstream().await?;
    let control_token = format!("packaged-control-{}", uuid::Uuid::new_v4());
    let host = || host_command(&gateway_binary, server_home.path(), &control_token);

    let mut identity = OwnedChild::spawn(
        host().args(["--json", "build-info"]),
        logs.path(),
        "identity",
    )?;
    identity.success(Duration::from_secs(10)).await?;
    let artifact: ArtifactIdentity = serde_json::from_value(identity.json()?)?;
    verify_package(&gateway_binary, &artifact)?;
    ensure!(
        artifact.profile == DistributionProfile::Gateway && artifact.features.is_empty(),
        "supply the official minimum-gateway artifact"
    );
    ensure!(
        artifact
            .source_revision
            .as_ref()
            .is_some_and(|s| !s.is_empty()),
        "archive lacks revision identity"
    );
    ensure!(!artifact.target.is_empty());

    let mut signer = OwnedChild::spawn(
        host().args([
            "--json",
            "key",
            "sign",
            "--user",
            "packaged-sdk-consumer",
            "--db",
            DATABASE,
        ]),
        logs.path(),
        "signer",
    )?;
    signer.success(Duration::from_secs(30)).await?;
    let inference_token = signer.json()?["secret"]
        .as_str()
        .context("key sign omitted secret")?
        .to_owned();
    ensure!(inference_token.starts_with("brvk_") && inference_token != control_token);
    ensure!(
        server_home.path().join("gateway.db").is_file(),
        "CLI did not create the private SQLite database"
    );

    // Reserve distinct loopback ports together, releasing immediately before
    // spawn. No fixed default listener or existing user's daemon is addressed.
    let inference_reservation = TcpListener::bind("127.0.0.1:0")?;
    let control_reservation = TcpListener::bind("127.0.0.1:0")?;
    let inference_address = inference_reservation.local_addr()?;
    let control_address = control_reservation.local_addr()?;
    let inference_endpoint = format!("http://{inference_address}");
    let control_endpoint = format!("http://{control_address}");
    let config_path = server_home.path().join("bitrouter.yaml");
    let socket = server_home.path().join("acceptance.sock");
    // JSON is valid YAML; serde quoting also preserves Windows paths safely.
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&json!({
            "inherit_defaults": false, "registry": {"enabled": false},
            "server": {"listen": inference_address.to_string(), "skip_auth": false, "control_socket": socket},
            "database": {"url": DATABASE},
            "control": {"enabled": true, "listen": control_address.to_string(), "credentials": [{
                "id": "packaged-reader", "token_env": "BITROUTER_PACKAGED_CONTROL_TOKEN", "scopes": ["control:read"]
            }]},
            "providers": {"fixture": {
                "api_base": upstream.endpoint, "api_key": PROVIDER_KEY,
                "api_protocol": [
                    {"fixture-chat": "chat_completions"}, {"fixture-messages": "messages"},
                    {"fixture-responses": "responses"}, {"fixture-decisions": "decisions"}
                ],
                "models": [{"id": "fixture-chat"}, {"id": "fixture-messages"}, {"id": "fixture-responses"}, {"id": "fixture-decisions"}]
            }}
        }))?,
    )?;
    drop(inference_reservation);
    drop(control_reservation);
    let mut server = OwnedChild::spawn(
        host().args(["serve", "--config"]).arg(&config_path),
        logs.path(),
        "gateway",
    )?;
    let result: Result<()> = async {
        ready(
            &mut server,
            &inference_endpoint,
            &control_endpoint,
            &control_token,
        )
        .await?;
        ensure!(
            upstream.calls.lock().await.is_empty(),
            "startup dispatched an upstream request"
        );
        // No inherited HOME, config, provider key, database URL, proxy or PATH.
        // Only these four service inputs cross the consumer process boundary.
        let mut consumer = OwnedChild::spawn(
            isolated_command(&consumer_binary, consumer_home.path())
                .env("BITROUTER_SDK_INFERENCE_ENDPOINT", &inference_endpoint)
                .env("BITROUTER_SDK_INFERENCE_CREDENTIAL", &inference_token)
                .env("BITROUTER_SDK_CONTROL_ENDPOINT", &control_endpoint)
                .env("BITROUTER_SDK_CONTROL_CREDENTIAL", &control_token),
            logs.path(),
            "consumer",
        )?;
        consumer.success(Duration::from_secs(100)).await?;
        let receipt = consumer.json()?;
        ensure!(receipt["accepted"] == true && receipt["scenario"] == "packaged-gateway-sdk-v1");
        ensure!(
            serde_json::from_value::<ArtifactIdentity>(receipt["artifact"].clone())? == artifact
        );
        ensure!(
            std::fs::read_dir(consumer_home.path())?.next().is_none(),
            "consumer wrote local service state"
        );

        // Stop via the packaged CLI's explicit private socket, after proving
        // readiness belongs to our child. Drop cleanup never calls `bro stop`.
        ensure!(
            server.child.try_wait()?.is_none(),
            "gateway exited before graceful stop"
        );
        let mut stop = OwnedChild::spawn(
            host()
                .args(["stop", "--config"])
                .arg(&config_path)
                .arg("--socket")
                .arg(&socket),
            logs.path(),
            "stop",
        )?;
        stop.success(Duration::from_secs(15)).await?;
        server.success(Duration::from_secs(20)).await?;
        closed(inference_address).await?;
        closed(control_address).await?;

        // Check after exit: no delayed retry/replay can escape the observation
        // window. Auth failures/discovery/control/public errors dispatch zero.
        let calls = upstream.calls.lock().await;
        let mut actual = BTreeMap::new();
        for call in calls.iter() {
            ensure!(
                call.authorized,
                "gateway did not use the server-owned provider credential"
            );
            let expected_model = match call.path.as_str() {
                "/chat/completions" => "fixture-chat",
                "/messages" => "fixture-messages",
                "/responses" => "fixture-responses",
                "/decisions" => "fixture-decisions",
                _ => anyhow::bail!("unexpected upstream route: {}", call.path),
            };
            ensure!(
                call.body["model"] == expected_model,
                "wrong model at the native upstream"
            );
            let interrupted = call.body["messages"][0]["content"] == "interrupt acceptance";
            *actual
                .entry((call.path.as_str(), call.body["stream"] == true, interrupted))
                .or_insert(0_u32) += 1;
        }
        let expected = BTreeMap::from([
            (("/chat/completions", false, false), 1),
            (("/chat/completions", true, false), 1),
            (("/chat/completions", true, true), 1),
            (("/messages", false, false), 1),
            (("/messages", true, false), 1),
            (("/responses", false, false), 1),
            (("/responses", true, false), 1),
            (("/decisions", false, false), 1),
        ]);
        ensure!(
            actual == expected,
            "unexpected upstream dispatch/replay: {actual:?}"
        );
        Ok(())
    }
    .await;
    if result.is_err() {
        // Read logs before the temporary directories and child guards drop.
        for path in [&server.stdout, &server.stderr] {
            if let Ok(log) = std::fs::read_to_string(path) {
                eprintln!("packaged gateway diagnostic:\n{log}");
            }
        }
    }
    result
}

/// Defaults are exercised only here: target names come from the shipped Policy,
/// not from registry inventory or a hand-authored substitute source.
#[tokio::test]
async fn bundled_auto_source_dispatches_with_tools_effort_and_complete_context() -> Result<()> {
    use bitrouter_ai::{
        control::administration::{PolicyInput, PolicyView},
        policy::Document,
    };

    let binary = Path::new(env!("CARGO_BIN_EXE_bro"));
    let home = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    // A static fixture credential supplies Codex's account-identity claim so
    // native Responses continuation authority is available. There is no OAuth
    // login, credential-store seed, refresh, signature trust or external host.
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "https://api.openai.com/auth":{"chatgpt_account_id":"bundled-fixture-account"}
    }))?);
    let provider_key = format!("e30.{payload}.fixture");
    let upstream = mock_upstream_with_key(&provider_key).await?;
    let token = format!("bundled-control-{}", uuid::Uuid::new_v4());
    let inference = TcpListener::bind("127.0.0.1:0")?;
    let control = TcpListener::bind("127.0.0.1:0")?;
    let inference_address = inference.local_addr()?;
    let control_address = control.local_addr()?;
    let inference_endpoint = format!("http://{inference_address}");
    let control_endpoint = format!("http://{control_address}");
    let config_path = home.path().join("bitrouter.yaml");
    let socket = home.path().join("bundled.sock");
    let bundled: Document = serde_saphyr::from_str(include_str!("../policy/default.yaml"))?;
    let auto = bundled
        .policies
        .get("auto")
        .context("bundled auto missing")?;
    ensure!(auto.default_tier == "strong");
    ensure!(auto.tool_use_tier.as_deref() == Some("strong"));
    ensure!(auto.tool_safe_tiers.contains(&"strong".to_owned()));
    let strong = auto.tiers.get("strong").context("bundled strong missing")?;
    let (_, expected_model) = strong
        .model
        .split_once(':')
        .context("default target is not provider-qualified")?;

    let mut providers = serde_json::Map::new();
    for target in auto.tiers.values() {
        let (provider, model) = target
            .model
            .split_once(':')
            .context("bundled target is not provider-qualified")?;
        let provider = providers.entry(provider.to_owned()).or_insert_with(|| {
            json!({
                "api_base":upstream.endpoint,"api_key":provider_key,"auto_discover":false,
                "api_protocol":[{"*":"responses"}],
                // Explicit protocol endpoints prevent inherited provider defaults
                // from directing any generation to an external subscription host.
                "protocol_endpoints":{"responses":upstream.endpoint},"models":[],
            })
        });
        provider["models"]
            .as_array_mut()
            .context("fixture models not an array")?
            .push(json!({
                "id":model,"provider_model_id":model,"capabilities":["tools","reasoning"],
                "reasoning_effort":{"levels":["low","high"],"default":"high"},
            }));
    }
    // No routers, policy path, or policy.yaml: the child must install the real
    // bundled auto binding/source. Only its provider transport is redirected.
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&json!({
            "inherit_defaults":true,"registry":{"enabled":false},
            "server":{"listen":inference_address.to_string(),"skip_auth":true,"control_socket":socket},
            "database":{"url":DATABASE},
            "control":{"enabled":true,"listen":control_address.to_string(),"credentials":[{
                "id":"default-reader","token_env":"BITROUTER_PACKAGED_CONTROL_TOKEN","scopes":["control:read"]
            }]},"providers":providers,
        }))?,
    )?;
    ensure!(!home.path().join("policy.yaml").exists());
    drop((inference, control));
    let mut child = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .args(["serve", "--config"])
            .arg(&config_path),
        logs.path(),
        "bundled",
    )?;
    let exercise: Result<()> = async {
        ready(&mut child, &inference_endpoint, &control_endpoint, &token).await?;
        let operator = ControlClient::new(&control_endpoint, &token)?;
        let active = operator.policy(&PolicyInput { view:PolicyView::Active, name:Some("auto".into()) }).await?;
        ensure!(active.source.as_ref() == Some(&bundled), "child is not serving the bundled source");
        ensure!(active.bindings.get("auto").map(String::as_str) == Some("auto"));
        ensure!(upstream.calls.lock().await.is_empty(), "startup invoked a provider");
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build()?;
        let instructions = "Keep every turn and the complete tool exchange.";
        let history = json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":"earlier requirement"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"earlier response"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"current requirement"}]},
        ]);
        let tool_history = json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":"earlier requirement"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"earlier response"}]},
            {"type":"function_call","call_id":"bundled-call","name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"},
            {"type":"function_call_output","call_id":"bundled-call","output":"complete source, including earlier requirements"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"current requirement"}]},
        ]);
        let tools = json!([{"type":"function","name":"read_file","description":"Read an explicit file",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]);
        for stream in [false,true] {
            for with_tools in [false,true] {
                let input = if with_tools {&tool_history} else {&history};
                let mut body = json!({"model":"bitrouter/auto","input":input,"instructions":instructions,
                    "reasoning":{"effort":"low"},"stream":stream});
                if with_tools { body["tools"] = tools.clone(); body["tool_choice"] = json!("auto"); }
                let response = client.post(format!("{inference_endpoint}/v1/responses")).json(&body).send().await?;
                let status = response.status();
                let text = response.text().await?;
                ensure!(status.is_success(), "bundled request failed: {status}: {text}");
                if stream {
                    ensure!(text.contains("response.completed") && text.contains(TEXT), "missing successful SSE output: {text}");
                } else {
                    let value:Value = serde_json::from_str(&text)?;
                    ensure!(value["status"] == "completed" && value["output"][0]["content"][0]["text"] == TEXT);
                }
                let calls = upstream.calls.lock().await;
                let sent = calls.last().context("no generation reached mock upstream")?;
                ensure!(sent.authorized && sent.path == "/responses", "wrong provider credential or protocol");
                ensure!(sent.body["model"] == expected_model, "bundled strong/tool-safe target was not served");
                ensure!(sent.body["reasoning"]["effort"] == "low", "provider default replaced caller effort");
                ensure!(sent.body["instructions"] == instructions, "instructions changed");
                // The Codex transport moves declarations into an additional_tools
                // item. The complete ordered conversation still follows it.
                let mut expected_input = Vec::new();
                if with_tools {
                    expected_input.push(json!({"type":"additional_tools","role":"developer","tools":tools}));
                    ensure!(sent.body["tool_choice"] == "auto");
                }
                expected_input.extend(input.as_array().context("fixture input array")?.iter().cloned());
                ensure!(sent.body["input"] == json!(expected_input), "earlier context or tool exchange changed: {}", sent.body);
                ensure!(sent.body.get("tools").is_none(), "transport duplicated tool declarations");
            }
        }
        ensure!(upstream.calls.lock().await.len() == 4, "generation was duplicated or sent to an unexpected endpoint");
        let active_after = operator.policy(&PolicyInput { view:PolicyView::Active, name:Some("auto".into()) }).await?;
        ensure!(active_after.source == active.source && active_after.source_digest == active.source_digest);
        ensure!(!home.path().join("policy.yaml").exists(), "default serving rewrote a policy file");
        Ok(())
    }.await;
    exercise.with_context(|| {
        format!(
            "bundled source process failed: {}",
            match std::fs::read_to_string(&child.stderr) {
                Ok(contents) => contents,
                Err(error) => format!("stderr unavailable: {error}"),
            }
        )
    })?;
    let mut stop = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .args(["stop", "--config"])
            .arg(&config_path)
            .arg("--socket")
            .arg(&socket),
        logs.path(),
        "bundled-stop",
    )?;
    stop.success(Duration::from_secs(15)).await?;
    child.success(Duration::from_secs(20)).await?;
    closed(inference_address).await?;
    closed(control_address).await?;
    Ok(())
}

/// Copy, configure and start the shipped template, then resubmit its exact source.
#[cfg(unix)]
#[tokio::test]
async fn shipped_auto_template_submits_and_reads_back_through_the_service_api() -> Result<()> {
    shipped_template_service_case("auto").await
}

#[cfg(unix)]
#[tokio::test]
async fn shipped_coding_template_starts_and_submits_its_cli_output_through_service_api()
-> Result<()> {
    shipped_template_service_case("coding").await
}

#[cfg(unix)]
async fn shipped_template_service_case(name: &str) -> Result<()> {
    use base64::Engine;
    use bitrouter_ai::{
        policy::Document,
        tenancy::{
            authority::{OwnerKind, ResourceKey, ResourceKind, TenantPut, TenantState},
            configuration::ResourceRef,
            identity::TenantId,
            operation::OperationStatus,
            resources::{Definition, ResourcePut},
        },
    };
    use bitrouter_sdk::tenancy::TenancyClient;

    let binary = Path::new(env!("CARGO_BIN_EXE_bro"));
    let home = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let token = format!("template-operator-{}", uuid::Uuid::new_v4());
    let wrapping_key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 32]>());
    let inference = TcpListener::bind("127.0.0.1:0")?;
    let control = TcpListener::bind("127.0.0.1:0")?;
    let inference_address = inference.local_addr()?;
    let control_address = control.local_addr()?;
    let inference_endpoint = format!("http://{inference_address}");
    let control_endpoint = format!("http://{control_address}");
    let config_path = home.path().join("bitrouter.yaml");
    let socket = home.path().join("template.sock");
    let template_config = match name {
        "auto" => include_str!("../../../templates/auto-router/bitrouter.yaml"),
        "coding" => include_str!("../../../templates/coding-router/bitrouter.yaml"),
        _ => anyhow::bail!("unknown shipped template"),
    };
    std::fs::write(&config_path, template_config)?;
    let mut config: Value = serde_saphyr::from_str(template_config)?;
    // Configure the README's provider placeholders, isolated listeners and
    // authority. The shipped auto source and router binding remain unchanged.
    let upstream = mock_upstream().await?;
    config["inherit_defaults"] = json!(false);
    config["registry"] = json!({"enabled":false});
    config["server"] =
        json!({"listen":inference_address.to_string(),"skip_auth":false,"control_socket":socket});
    config["database"] = json!({"url":DATABASE});
    config["tenancy"] = json!({"enabled":true,"wrapping_key_env":"BITROUTER_TEMPLATE_WRAP"});
    config["control"] = json!({"enabled":true,"listen":control_address.to_string(),"credentials":[{
        "id":"template-operator","token_env":"BITROUTER_PACKAGED_CONTROL_TOKEN","scopes":["control:read","control:tenancy"]
    }]});
    if name == "coding" {
        config["providers"] = json!({"fixture":{
            "api_base":upstream.endpoint,"api_key":PROVIDER_KEY,"auto_discover":false,
            "api_protocol":[{"*":"responses"}],"models":[{"id":"strong"},{"id":"economy"}]
        }});
    } else {
        for provider in config["providers"]
            .as_object_mut()
            .context("template providers")?
            .values_mut()
        {
            provider["api_base"] = json!(upstream.endpoint);
            provider["api_key"] = json!(PROVIDER_KEY);
            provider["auto_discover"] = json!(false);
        }
        std::fs::write(
            home.path().join("policy.yaml"),
            include_str!("../../../templates/auto-router/policy.yaml"),
        )?;
    }
    std::fs::write(&config_path, serde_saphyr::to_string(&config)?)?;
    if name == "coding" {
        let mut init = OwnedChild::spawn(
            host_command(binary, home.path(), &token)
                .args([
                    "--json",
                    "policy",
                    "init",
                    "coding",
                    "--strong",
                    "fixture:strong",
                    "--economy",
                    "fixture:economy",
                    "--config",
                ])
                .arg(&config_path),
            logs.path(),
            "template-init",
        )?;
        init.success(Duration::from_secs(15))
            .await
            .context("coding template policy init")?;
    }
    let mut check = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .args(["--json", "policy", "check", "--config"])
            .arg(&config_path),
        logs.path(),
        "template-check",
    )?;
    check.success(Duration::from_secs(15)).await?;
    let mut show = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .args(["--json", "policy", "show", name, "--disk", "--config"])
            .arg(&config_path),
        logs.path(),
        "template-show",
    )?;
    show.success(Duration::from_secs(15)).await?;
    let template: Document = serde_json::from_value(show.json()?["source"].clone())?;
    let policy_bytes = std::fs::read(home.path().join("policy.yaml"))?;
    let disk: Document = serde_saphyr::from_slice(&policy_bytes)?;
    ensure!(template == disk && template.policies.contains_key(name));
    drop((inference, control));
    let mut child = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .env("BITROUTER_TEMPLATE_WRAP", wrapping_key)
            .args(["serve", "--config"])
            .arg(&config_path),
        logs.path(),
        "template-service",
    )?;
    ready(&mut child, &inference_endpoint, &control_endpoint, &token).await?;
    let client = TenancyClient::new(&control_endpoint, &token)?;
    let capabilities = client.capabilities().await?;
    let installation = capabilities.installation_id;
    let tenant: TenantId = "template-tenant".try_into()?;
    let created = client
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "template-tenant-create".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(created.status == OperationStatus::Applied);
    let key = ResourceKey {
        owner_kind: OwnerKind::Tenant,
        owner_id: tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: format!("template-{name}").try_into()?,
    };
    let command = ResourcePut {
        format: 1,
        operation_id: "template-source-submit".try_into()?,
        installation_id: installation.clone(),
        key: key.clone(),
        expected_revision: None,
        definition: Definition::Policy {
            document: template.clone(),
            name: name.into(),
            dependencies: Vec::new(),
        },
    };
    let written = client.put_resource(&tenant, &command).await?;
    ensure!(written.status == OperationStatus::Applied);
    let reference = ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: written
            .resource_revision
            .clone()
            .context("template resource revision missing")?,
    };
    let shown = client
        .policy_source(&installation, &tenant, &reference)
        .await?;
    ensure!(
        shown.document == template && shown.name == name && shown.status.reference == reference,
        "template source was changed or returned under another immutable identity"
    );
    let replay = client.put_resource(&tenant, &command).await?;
    ensure!(
        replay.resource_revision == written.resource_revision,
        "replay published another source revision"
    );
    if name == "auto" {
        reject_raw_policy_sources_without_mutation(
            &client,
            &control_endpoint,
            &token,
            &command,
            &reference,
        )
        .await?;
    }
    ensure!(
        std::fs::read(home.path().join("policy.yaml"))? == policy_bytes,
        "API submission changed the copied/initialized template source"
    );
    ensure!(
        upstream.calls.lock().await.is_empty(),
        "template setup or Policy submission performed provider work"
    );
    let mut stop = OwnedChild::spawn(
        host_command(binary, home.path(), &token)
            .args(["stop", "--config"])
            .arg(&config_path)
            .arg("--socket")
            .arg(&socket),
        logs.path(),
        "template-stop",
    )?;
    stop.success(Duration::from_secs(15)).await?;
    child.success(Duration::from_secs(20)).await?;
    closed(inference_address).await?;
    closed(control_address).await?;
    Ok(())
}

#[cfg(unix)]
async fn reject_raw_policy_sources_without_mutation(
    client: &bitrouter_sdk::tenancy::TenancyClient,
    endpoint: &str,
    token: &str,
    command: &bitrouter_ai::tenancy::resources::ResourcePut,
    reference: &bitrouter_ai::tenancy::configuration::ResourceRef,
) -> Result<()> {
    use bitrouter_ai::tenancy::identity::{OperationId, TenantId};
    let tenant: TenantId = command.key.owner_id.as_str().try_into()?;
    let installation = &command.installation_id;
    let before = client.directory(installation, None, None, 100).await?;
    ensure!(
        before.next_after.is_none(),
        "template directory fixture exceeds one page"
    );
    let original = client
        .policy_source(installation, &tenant, reference)
        .await?;
    let base = serde_json::to_value(command)?;
    let source = base["definition"]["document"].clone();
    let mut cases = Vec::new();
    for field in ["tiers", "routes", "tool_safe_tiers", "default_tier"] {
        let mut invalid = source.clone();
        invalid["policies"]["auto"][field] = Value::Null;
        cases.push(invalid);
    }
    let mut missing = source.clone();
    missing["policies"]["auto"]
        .as_object_mut()
        .context("policy object")?
        .remove("default_tier");
    cases.push(missing);
    let mut oversized = source.clone();
    oversized["policies"]["auto"]["tiers"]["strong"]["model"] =
        json!(format!("fixture:{}", "x".repeat(1024 * 1024)));
    cases.push(oversized);
    let mut policies = source.clone();
    for n in 0..256 {
        policies["policies"][format!("policy-{n:03}")] = source["policies"]["auto"].clone();
    }
    cases.push(policies);
    let mut tiers = source.clone();
    tiers["policies"]["auto"]["tiers"] = (0..257)
        .map(|n| {
            (
                format!("tier-{n:03}"),
                json!({"model":format!("fixture:model-{n:03}")}),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    cases.push(tiers);
    let http = reqwest::Client::new();
    for (index, invalid) in cases.into_iter().enumerate() {
        let operation: OperationId = format!("template-invalid-source-{index}").try_into()?;
        let mut raw = base.clone();
        raw["operation_id"] = json!(operation);
        raw["key"]["resource_id"] = json!(format!("must-not-be-created-{index}"));
        raw["definition"]["document"] = invalid;
        // Send raw bytes so neither SDK validation nor a pre-decoded Document
        // can hide the null/omission and capacity attacks from real ingress.
        let response = http
            .post(format!("{endpoint}/control/v1/tenants/{tenant}/resources"))
            .bearer_auth(token)
            .header("Bitrouter-Installation-Id", installation.as_str())
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&raw)?)
            .send()
            .await?;
        ensure!(
            matches!(response.status().as_u16(), 400 | 413 | 422),
            "invalid Policy source {index} got HTTP {}",
            response.status()
        );
        ensure!(
            matches!(client.operation(installation, &operation).await,
            Err(bitrouter_sdk::error::Error::Tenancy(failure)) if failure.status == 404),
            "invalid source {index} recorded a management operation"
        );
        let after = client
            .directory(installation, Some(before.authority_revision), None, 100)
            .await?;
        ensure!(
            serde_json::to_value(after)? == serde_json::to_value(&before)?,
            "invalid source {index} changed authority"
        );
        let shown = client
            .policy_source(installation, &tenant, reference)
            .await?;
        ensure!(
            serde_json::to_value(shown)? == serde_json::to_value(&original)?,
            "invalid source {index} changed the immutable source or resource head"
        );
    }
    Ok(())
}
