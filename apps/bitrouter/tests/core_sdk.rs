//! Independent SDK harness against a separately built, core-only official host.
//! The client workspace imports no application code, database or server framework.
use anyhow::{Context, Result, ensure};
use bitrouter_ai::artifact::ArtifactIdentity;
use bitrouter_sdk::{control::ControlClient, gateway::GatewayClient};
use serde_json::{Value, json};
use std::{
    fs::File,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

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
            .spawn()?;
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }
    async fn success(&mut self, seconds: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(seconds), async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    ensure!(
                        status.success(),
                        "child failed ({status}): {}",
                        std::fs::read_to_string(&self.stderr)?
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("owned child timeout")?
    }
    fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(&self.stdout)?)?)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            if let Err(error) = self.child.kill() {
                eprintln!("owned child termination: {error}");
            }
            if let Err(error) = self.child.wait() {
                eprintln!("owned child reap: {error}");
            }
        }
    }
}
fn binary(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os(name)
            .with_context(|| format!("{name} is required; see fixtures/sdk_consumer/README.md"))?,
    );
    ensure!(
        path.is_absolute() && path.is_file(),
        "invalid supplied binary: {}",
        path.display()
    );
    Ok(path)
}
fn command(binary: &Path, home: &Path) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(home).env_clear();
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
}
fn host(binary: &Path, home: &Path) -> Command {
    let mut cmd = command(binary, home);
    cmd.args(["--context", "local"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("BITROUTER_HOME", home.join("runtime"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env(
            "BITROUTER_CONTROL_TOKEN",
            "private-core-sdk-control-token-0123456789",
        );
    cmd
}

#[tokio::test]
#[ignore = "requires an immutable core-only bro and independently built core SDK consumer"]
async fn independent_core_sdk_preserves_harness_authority() -> Result<()> {
    let server_binary = binary("BITROUTER_TEST_CORE_BINARY")?;
    let consumer_binary = binary("BITROUTER_TEST_CORE_SDK_CONSUMER_BINARY")?;
    let revision = std::env::var("BITROUTER_TEST_EXPECTED_REVISION")?;
    ensure!(!revision.is_empty() && server_binary != consumer_binary);
    let server_home = tempfile::tempdir()?;
    let consumer_home = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/input_tokens"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object":"response.input_tokens","input_tokens":20})),
        )
        .mount(&upstream)
        .await;
    // OpenAI Responses fixture uses its published HTTP response contract.
    // https://platform.openai.com/docs/api-reference/responses/object
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(|request:&Request| {
            let done=String::from_utf8_lossy(&request.body).contains("function_call_output");
            ResponseTemplate::new(200).set_body_json(json!({"id":"provider-response","object":"response","status":"completed","model":"served",
                "output":if done {json!([{"id":"provider-message","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"verified response","annotations":[]}]}])}
                else {json!([{"id":"provider-call-item","type":"function_call","call_id":"provider-call","name":"read","arguments":"{\"path\":\"file.txt\"}","status":"completed"}])},
                "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}))
        }).mount(&upstream).await;
    let mut identity = OwnedChild::spawn(
        host(&server_binary, server_home.path()).args(["--json", "build-info"]),
        logs.path(),
        "identity",
    )?;
    identity.success(10).await?;
    let artifact: ArtifactIdentity = serde_json::from_value(identity.json()?)?;
    ensure!(
        artifact.features == ["core"]
            && artifact.source_revision.as_deref() == Some(revision.as_str()),
        "wrong core candidate: {artifact:?}"
    );
    let mut credentials = Vec::new();
    for user in ["core-owner", "other-owner"] {
        let mut signer = OwnedChild::spawn(
            host(&server_binary, server_home.path()).args([
                "--json",
                "key",
                "sign",
                "--user",
                user,
                "--db",
                "sqlite://core.db?mode=rwc",
            ]),
            logs.path(),
            user,
        )?;
        signer.success(30).await?;
        credentials.push(
            signer.json()?["secret"]
                .as_str()
                .context("missing signed key")?
                .to_owned(),
        );
    }
    let inference = TcpListener::bind("127.0.0.1:0")?;
    let control = TcpListener::bind("127.0.0.1:0")?;
    let inference_addr = inference.local_addr()?;
    let control_addr = control.local_addr()?;
    let endpoint = format!("http://{inference_addr}");
    let control_endpoint = format!("http://{control_addr}");
    let config = server_home.path().join("private.yaml");
    let socket = server_home.path().join("control.sock");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "inherit_defaults":false,"registry":{"enabled":false},
            "server":{"listen":inference_addr.to_string(),"control_socket":socket,"skip_auth":false},
            "control":{"enabled":true,"listen":control_addr.to_string()},
            "database":{"url":"sqlite://core.db?mode=rwc"},"agent_api":{"enabled":false},
            "providers":{"fixture":{"api_base":format!("{}/v1",upstream.uri()),"api_key":"private-provider-secret",
                "models":[{"id":"served","api_protocol":"responses","capabilities":["tools"],"input_token_counting":"responses",
                    "token_limits":{"max_input_tokens":4000,"max_output_tokens":128,"context_window":8000}}]}},
            "models":{"fixture-model":{"endpoints":[{"provider":"fixture","service_id":"served"}]}}
        }))?,
    )?;
    drop((inference, control));
    let mut server = OwnedChild::spawn(
        host(&server_binary, server_home.path())
            .args(["serve", "--config"])
            .arg(&config),
        logs.path(),
        "server",
    )?;
    let result: Result<()> = async {
        let operator = ControlClient::new(
            &control_endpoint,
            "private-core-sdk-control-token-0123456789",
        )?;
        let gateway = GatewayClient::new(&endpoint, Some(&credentials[0]))?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                ensure!(
                    server.child.try_wait()?.is_none(),
                    "server exited before ready"
                );
                if let (Ok(status), Ok(_)) = (operator.status().await, gateway.health().await) {
                    ensure!(status.pid == Some(server.child.id()));
                    ensure!(status.daemon_artifact.as_ref() == Some(&artifact));
                    if status.running {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("core readiness timeout")??;
        ensure!(
            upstream
                .received_requests()
                .await
                .context("provider history")?
                .is_empty()
        );
        let mut consumer = OwnedChild::spawn(
            command(&consumer_binary, consumer_home.path())
                .env("BITROUTER_SDK_INFERENCE_ENDPOINT", &endpoint)
                .env("BITROUTER_SDK_INFERENCE_CREDENTIAL", &credentials[0])
                .env("BITROUTER_SDK_OTHER_CREDENTIAL", &credentials[1])
                .env("BITROUTER_SDK_JOURNAL_DIRECTORY", consumer_home.path()),
            logs.path(),
            "consumer",
        )?;
        consumer.success(100).await?;
        let receipt = consumer.json()?;
        ensure!(
            receipt["accepted"] == true
                && receipt["scenario"] == "independent-core-sdk-v1"
                && receipt["tool_results"] == 1
        );
        ensure!(receipt["batches"].as_u64().is_some_and(|n| n > 1));
        ensure!(
            std::fs::read_dir(consumer_home.path())?.count() == 1,
            "client bootstrapped local service state"
        );
        let mut stop = OwnedChild::spawn(
            host(&server_binary, server_home.path())
                .args(["stop", "--config"])
                .arg(&config)
                .arg("--socket")
                .arg(&socket),
            logs.path(),
            "stop",
        )?;
        stop.success(15).await?;
        server.success(20).await?;
        for address in [inference_addr, control_addr] {
            ensure!(
                matches!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        tokio::net::TcpStream::connect(address)
                    )
                    .await,
                    Ok(Err(_))
                ),
                "listener remained after stop"
            );
        }
        let requests = upstream
            .received_requests()
            .await
            .context("provider history")?;
        let mut generation = 0;
        let mut counting = 0;
        for request in requests {
            ensure!(
                request
                    .headers
                    .get("authorization")
                    .is_some_and(|v| v == "Bearer private-provider-secret")
            );
            match request.url.path() {
                "/v1/responses" => {
                    generation += 1;
                    let body: Value = serde_json::from_slice(&request.body)?;
                    ensure!(body["model"] == "served");
                    let outputs: Vec<_> = body["input"]
                        .as_array()
                        .context("missing provider input")?
                        .iter()
                        .filter(|item| item["type"] == "function_call_output")
                        .collect();
                    if generation == 1 {
                        ensure!(outputs.is_empty(), "tool output preceded its invocation");
                    } else {
                        ensure!(
                            outputs.len() == 1
                                && outputs[0]["call_id"] == "provider-call"
                                && outputs[0]["output"] == "mock file body",
                            "mock result changed before provider delivery: {outputs:?}"
                        );
                    }
                }
                "/v1/responses/input_tokens" => counting += 1,
                other => anyhow::bail!("unexpected provider request {other}"),
            }
        }
        ensure!(
            generation == 2 && counting >= 2,
            "unexpected provider counts: generation={generation}, counting={counting}"
        );
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        eprintln!(
            "core SDK fixture failed: {error:#}\nserver: {}",
            std::fs::read_to_string(&server.stderr)?
        );
    }
    result
}
