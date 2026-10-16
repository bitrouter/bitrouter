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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let router = axum::Router::new().fallback(any(move |request: Request| {
        let calls = recorded.clone();
        async move {
            let result: Result<Response> = async {
                let (parts, body) = request.into_parts();
                let authorized = parts
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == format!("Bearer {PROVIDER_KEY}"))
                    || parts
                        .headers
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        == Some(PROVIDER_KEY);
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
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(address),
    )
    .await;
    ensure!(
        matches!(result, Ok(Err(_))),
        "listener remained reachable after graceful shutdown"
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
    ensure!(!artifact.target.is_empty() && !artifact.handoff_build_id.is_empty());

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
