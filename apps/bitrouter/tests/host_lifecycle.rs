//! Bounded lifecycle regressions against the current Cargo-built official host.
//! Every control endpoint, credential, database and child belongs to this test.
#![cfg(unix)]

use std::fs::{self, File};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use axum::{Json, extract::State, http::HeaderMap, routing::post};
use bitrouter::auth::{db, keys};
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse};
use bitrouter::metering::entities::requests;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const CASE_TIMEOUT: Duration = Duration::from_secs(60);
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const CONTROL_TOKEN: &str = "host-lifecycle-private-control-token";
const PROVIDER_KEY: &str = "host-lifecycle-private-provider-key";
const REQUEST_ID: &str = "host-lifecycle-control-error";
const REPLY: &str = "completed after control shutdown failed";

#[path = "host_lifecycle/tenancy.rs"]
mod tenancy;

struct Host {
    child: Option<Child>,
    home: PathBuf,
    directory: Option<tempfile::TempDir>,
    log: PathBuf,
    socket: PathBuf,
    inference: SocketAddr,
    remote: SocketAddr,
    client: reqwest::Client,
}

impl Host {
    fn start(upstream: &str) -> Result<Self> {
        Self::start_profile(upstream, false)
    }

    fn start_profile(upstream: &str, tenancy: bool) -> Result<Self> {
        // Short, owner-only paths also fit macOS's Unix-socket path limit.
        let directory = tempfile::Builder::new()
            .prefix("bro-life-")
            .tempdir_in("/tmp")?;
        let home = directory.path().to_path_buf();
        let inference = TcpListener::bind("127.0.0.1:0")?;
        let remote = TcpListener::bind("127.0.0.1:0")?;
        let mut host = Self {
            child: None,
            socket: home.join("control.sock"),
            log: home.join("host.log"),
            inference: inference.local_addr()?,
            remote: remote.local_addr()?,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(1))
                .timeout(CASE_TIMEOUT)
                .build()?,
            home,
            directory: Some(directory),
        };
        fs::set_permissions(&host.home, fs::Permissions::from_mode(0o700))?;
        for name in ["home", "runtime", "config", "cache", "data", "tmp"] {
            fs::create_dir(host.home.join(name))?;
        }
        let config = host.home.join("private.yaml");
        let tenancy_config = if tenancy {
            "tenancy: {enabled: true}\n"
        } else {
            ""
        };
        let control_credentials = if tenancy {
            "  credentials:\n    - id: test-operator\n      token_env: BITROUTER_CONTROL_TOKEN\n      scopes: [control:read, control:tenancy]\n"
        } else {
            ""
        };
        fs::write(
            &config,
            format!(
                r#"{tenancy_config}inherit_defaults: false
registry:
  enabled: false
server:
  listen: {}
  control_socket: control.sock
  skip_auth: false
control:
  enabled: true
  listen: {}
{control_credentials}database:
  url: 'sqlite://host.db?mode=rwc'
agent_api:
  enabled: false
providers:
  fixture:
    api_base: {upstream}
    api_key: {PROVIDER_KEY}
    models:
      - id: model
models:
  lifecycle-model:
    endpoints:
      - provider: fixture
        service_id: model
"#,
                host.inference, host.remote,
            ),
        )?;
        // Reserve distinct ephemeral ports until immediately before spawn.
        drop((inference, remote));
        host.spawn(&config)?;
        Ok(host)
    }

    fn spawn(&mut self, config: &std::path::Path) -> Result<()> {
        let output = File::options().create(true).append(true).open(&self.log)?;
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_bro"))
                .args(["serve", "--managed-child", "--config"])
                .arg(config)
                .current_dir(&self.home)
                // Do not inherit credentials, discovery paths, proxies, OTel,
                // workflow tracing, or feature activation from the developer.
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.home.join("home"))
                .env("USERPROFILE", self.home.join("home"))
                .env("BITROUTER_HOME", self.home.join("runtime"))
                .env("XDG_CONFIG_HOME", self.home.join("config"))
                .env("XDG_CACHE_HOME", self.home.join("cache"))
                .env("XDG_DATA_HOME", self.home.join("data"))
                .env("TMPDIR", self.home.join("tmp"))
                .env("BITROUTER_CONTROL_TOKEN", CONTROL_TOKEN)
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()
                .context("spawn the Cargo-built official host")?,
        );
        Ok(())
    }

    fn child(&mut self) -> Result<&mut Child> {
        self.child.as_mut().context("host child was not started")
    }

    fn logs(&self) -> String {
        match fs::read_to_string(&self.log) {
            Ok(log) => log,
            Err(error) => format!("cannot read host log: {error}"),
        }
    }

    fn alive(&mut self) -> Result<()> {
        let status = self.child()?.try_wait()?;
        ensure!(status.is_none(), "host exited early: {status:?}");
        Ok(())
    }

    async fn command(&self, command: &DaemonCommand) -> Result<DaemonResponse> {
        tokio::time::timeout(STEP_TIMEOUT, daemon::send_command(&self.socket, command))
            .await
            .with_context(|| format!("private control command timed out: {command:?}"))?
    }

    async fn health(&self) -> Result<()> {
        self.client
            .get(format!("http://{}/health", self.inference))
            .timeout(Duration::from_secs(1))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(())
    }

    async fn ready(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                self.alive()?;
                if let Ok(DaemonResponse::Status { pid, .. }) =
                    self.command(&DaemonCommand::Status).await
                {
                    ensure!(
                        pid == self.child()?.id(),
                        "private endpoint has another owner"
                    );
                    let remote_ready: Result<()> = async {
                        let response = self
                            .client
                            .get(format!("http://{}/control/v1/capabilities", self.remote))
                            .bearer_auth(CONTROL_TOKEN)
                            .timeout(Duration::from_secs(1))
                            .send()
                            .await?
                            .error_for_status()?
                            .bytes()
                            .await?;
                        ensure!(!response.is_empty(), "empty remote capabilities");
                        Ok(())
                    }
                    .await;
                    if remote_ready.is_ok()
                        && self.health().await.is_ok()
                        && self.logs().contains("— serving on")
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .context("official host readiness timed out")??;
        let hello = self.command(&DaemonCommand::HandoffHello).await?;
        ensure!(
            matches!(
                hello,
                DaemonResponse::HandoffHello {
                    cli_owned: true,
                    ..
                }
            ),
            "must exercise managed-child admission, not external-host rejection: {hello:?}"
        );
        Ok(())
    }

    async fn database(&self) -> Result<DatabaseConnection> {
        // Only open the database the child already created and migrated.
        tokio::time::timeout(
            STEP_TIMEOUT,
            Database::connect(format!(
                "sqlite://{}?mode=rw",
                self.home.join("host.db").display()
            )),
        )
        .await
        .context("opening private database timed out")?
        .context("open private host database")
    }

    async fn credential(&self) -> Result<String> {
        let database = self.database().await?;
        db::upsert_user(&database, "lifecycle-owner").await?;
        let key = keys::generate();
        db::insert_api_key(
            &database,
            &db::NewApiKey {
                id: "lifecycle-key".into(),
                key_hash: key.hash,
                user_id: "lifecycle-owner".into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        database.close().await?;
        Ok(key.secret)
    }

    async fn exited(&mut self) -> Result<ExitStatus> {
        tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                if let Some(status) = self.child()?.try_wait()? {
                    return Ok(status);
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .context("host exit timed out")?
    }

    async fn ensure_closed(&self) -> Result<()> {
        for address in [self.inference, self.remote] {
            ensure!(
                listener_closed(address).await?,
                "listener survived shutdown: {address}"
            );
        }
        ensure!(!self.socket.exists(), "control socket survived shutdown");
        ensure!(
            !self.socket.with_extension("pid").exists(),
            "PID file survived shutdown"
        );
        for entry in fs::read_dir(&self.home)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            ensure!(
                !entry.file_type()?.is_socket(),
                "owned socket survived shutdown: {name}"
            );
            ensure!(
                !(name.starts_with(".bitrouter-daemon-") && name.ends_with(".json")),
                "daemon locator survived shutdown: {name}"
            );
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill().context("kill owned host after test failure")?;
                let deadline = Instant::now() + CLEANUP_TIMEOUT;
                loop {
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    ensure!(Instant::now() < deadline, "owned host could not be reaped");
                    std::thread::sleep(POLL);
                }
            }
            self.child = None;
        }
        if let Some(directory) = self.directory.take() {
            directory.close().context("remove private host directory")?;
        }
        Ok(())
    }

    fn finish(&mut self, result: Result<()>) -> Result<()> {
        let logs = self.logs();
        let cleanup = self.cleanup();
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => {
                Err(error).with_context(|| format!("host lifecycle regression\n{logs}"))
            }
            (Err(error), Err(cleanup)) => {
                Err(error).with_context(|| format!("cleanup also failed: {cleanup:#}\n{logs}"))
            }
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("host lifecycle cleanup failed: {error:#}\n{}", self.logs());
        }
    }
}

async fn listener_closed(address: SocketAddr) -> Result<bool> {
    let probe = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(address),
    )
    .await?;
    connection_was_refused(probe.map(drop)).context("probe owned HTTP listener")
}

fn connection_was_refused(probe: std::io::Result<()>) -> std::io::Result<bool> {
    match probe {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(true),
        // Reset/abort can race shutdown without proving the listener is closed.
        // Keep polling for an explicit refusal under the existing deadline.
        // https://doc.rust-lang.org/std/io/enum.ErrorKind.html
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[test]
fn listener_probe_requires_a_refused_connection() -> Result<()> {
    use std::io::{Error, ErrorKind};

    ensure!(!connection_was_refused(Ok(()))?);
    for transient in [ErrorKind::ConnectionReset, ErrorKind::ConnectionAborted] {
        ensure!(
            !connection_was_refused(Err(Error::from(transient)))?,
            "transient probe failure must not prove listener closure"
        );
    }
    ensure!(connection_was_refused(Err(Error::from(
        ErrorKind::ConnectionRefused
    )))?);
    for unexpected in [ErrorKind::PermissionDenied, ErrorKind::TimedOut] {
        ensure!(connection_was_refused(Err(Error::from(unexpected))).is_err());
    }
    Ok(())
}

#[derive(Clone)]
struct ProviderState {
    observed: mpsc::Sender<(bool, Value)>,
    release: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
}

struct Provider {
    endpoint: String,
    observed: mpsc::Receiver<(bool, Value)>,
    release: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
    shutdown: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

async fn provider_request(
    State(state): State<ProviderState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let result: Result<Json<Value>> = async {
        state.calls.fetch_add(1, Ordering::SeqCst);
        let authorized = headers.get("authorization")
            .is_some_and(|value| value == format!("Bearer {PROVIDER_KEY}").as_str());
        state.observed.send((authorized, body)).await.context("request observer disappeared")?;
        let permit = tokio::time::timeout(CASE_TIMEOUT, state.release.acquire()).await??;
        permit.forget();
        // OpenAI Chat Completions fixture; usage is deliberately distinctive.
        // https://platform.openai.com/docs/api-reference/chat/object
        Ok(Json(json!({
            "id": "lifecycle-response", "object": "chat.completion", "created": 0,
            "model": "model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": REPLY}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        })))
    }.await;
    result.map_err(|error| {
        eprintln!("mock provider failed: {error:#}");
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            error.to_string(),
        )
    })
}

impl Provider {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (observed_tx, observed) = mpsc::channel(4);
        let release = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let router = axum::Router::new()
            .route("/chat/completions", post(provider_request))
            .with_state(ProviderState {
                observed: observed_tx,
                release: release.clone(),
                calls: calls.clone(),
            });
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            observed,
            release,
            calls,
            shutdown,
            task,
        })
    }

    async fn stop(&mut self) -> Result<()> {
        self.release.close();
        self.shutdown.cancel();
        tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.task)
            .await
            .context("mock provider cleanup timed out")?
            .context("mock provider task failed")??;
        Ok(())
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.release.close();
        self.shutdown.cancel();
        if !self.task.is_finished() {
            self.task.abort();
            eprintln!("aborted mock provider after unfinished test cleanup");
        }
    }
}

#[tokio::test]
async fn sigterm_control_error_drains_inflight_inference_before_nonzero_exit() -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut host = Host::start(&provider.endpoint)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let key = host.credential().await?;
        let request = host.client
            .post(format!("http://{}/v1/chat/completions", host.inference))
            .bearer_auth(key)
            .header("x-bitrouter-request-id", REQUEST_ID)
            .json(&json!({
                "model": "lifecycle-model", "stream": false,
                "messages": [{"role": "user", "content": "hold until listeners close"}]
            }));
        let inference = async move {
            Ok::<_, anyhow::Error>(request.send().await?.error_for_status()?.json::<Value>().await?)
        };
        tokio::pin!(inference);
        let (authorized, body) = tokio::select! {
            observed = provider.observed.recv() => observed.context("provider observation closed")?,
            response = &mut inference => bail!("inference finished before reaching the held provider: {response:?}"),
        };
        ensure!(authorized && body["model"] == "model" && body["stream"] == false, "unexpected upstream request: {body}");
        ensure!(body["messages"][0]["content"] == "hold until listeners close");
        ensure!(provider.calls.load(Ordering::SeqCst) == 1, "unexpected upstream dispatch count");

        // Provider observation establishes in-flight work; removing only our
        // authenticated child's socket forces the signal's local Stop IPC to fail.
        host.alive()?;
        fs::remove_file(&host.socket)?;
        let pid = rustix::process::Pid::from_raw(i32::try_from(host.child()?.id())?)
            .context("owned child PID is invalid")?;
        rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;

        // A listener refusing new connections while this owned child is alive
        // proves graceful HTTP shutdown began. The upstream remains blocked,
        // so an early return/abort cannot satisfy this ordering assertion.
        tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                host.alive()?;
                if listener_closed(host.inference).await? && listener_closed(host.remote).await? {
                    host.alive()?;
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::select! {
                    response = &mut inference => bail!("inference abandoned before upstream release: {response:?}"),
                    _ = tokio::time::sleep(POLL) => {}
                }
            }
        }).await.context("HTTP listeners did not enter graceful shutdown after IPC failure")??;

        let database = host.database().await?;
        ensure!(requests::Entity::find_by_id(REQUEST_ID).one(&database).await?.is_none(),
            "held provider request was finalized before upstream release");
        database.close().await?;
        provider.release.add_permits(1);
        let response = tokio::time::timeout(STEP_TIMEOUT, &mut inference).await
            .context("in-flight inference did not finish after upstream release")??;
        ensure!(response["choices"][0]["message"]["content"] == REPLY, "incomplete inference: {response}");
        ensure!(response["choices"][0]["finish_reason"] == "stop" && response["usage"]["total_tokens"] == 18);
        let status = host.exited().await?;
        ensure!(status.code() == Some(1), "shutdown IPC failure must produce the CLI error exit, not a signal or panic: {status}");
        host.ensure_closed().await?;
        let logs = host.logs();
        ensure!(logs.contains("connecting to") && logs.contains(host.socket.to_string_lossy().as_ref()),
            "exit did not report the failed private shutdown IPC: {logs}");

        // Reopen after reaping: no live host can populate missing settlement
        // evidence after this check. The child must have persisted it before exit.
        let database = host.database().await?;
        let rows = requests::Entity::find().all(&database).await?;
        database.close().await?;
        ensure!(rows.len() == 1, "expected one persisted request, got {rows:?}");
        let row = rows.first().context("missing persisted request")?;
        ensure!(row.request_id == REQUEST_ID && row.api_key_id == "lifecycle-key" && row.user_id == "lifecycle-owner");
        ensure!(row.provider_id == "fixture" && row.error.is_none() && row.streamed == 0, "failed request finalization: {row:?}");
        ensure!(row.prompt_tokens == 11 && row.completion_tokens == 7 && row.usage_origin == "provider_reported", "usage was lost on shutdown: {row:?}");
        let raw: Value = serde_json::from_str(row.raw_usage_json.as_deref().context("missing provider usage")?)?;
        ensure!(raw["total_tokens"] == 18, "provider usage was not retained: {raw}");
        ensure!(provider.calls.load(Ordering::SeqCst) == 1, "shutdown retried upstream inference");
        Ok(())
    }).await.context("control-error lifecycle case timed out").and_then(|result| result);
    let provider_cleanup = provider.stop().await;
    let result = match (result, provider_cleanup) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error).context(format!("provider cleanup also failed: {cleanup:#}"))
        }
    };
    host.finish(result)
}

#[cfg(feature = "core")]
mod core {
    use super::*;
    use bitrouter::storage::checkpoint::CheckpointBatchExt;
    use bitrouter::storage::checkpoint::validation::HarnessManifestExt;
    use bitrouter_ai::core::checkpoint::{CheckpointBatch, DurableHead};
    use bitrouter_ai::core::protocol::{
        Bind, ClientMessage, HarnessManifest, Limits, OwnershipGrant, ServerMessage,
    };
    use bitrouter_sdk::core::ChannelOptions;
    use bitrouter_sdk::gateway::{GatewayClient, TransportOptions};
    use std::collections::BTreeMap;

    fn envelope(operation: &str, kind: &str, payload: Value) -> Result<ClientMessage> {
        Ok(serde_json::from_value(json!({
            "version": 1, "type": kind, "session_id": "lifecycle-session",
            "execution_epoch": 1, "operation_id": operation, "payload": payload
        }))?)
    }

    async fn busy(host: &Host) -> Result<()> {
        let response = host.command(&DaemonCommand::HandoffPrepare).await?;
        ensure!(
            matches!(&response, DaemonResponse::HandoffBusy { reason } if reason == "daemon operations are in progress"),
            "core lifetime must retain admission, even without harness: {response:?}"
        );
        host.health()
            .await
            .context("Busy response closed inference admission")
    }

    #[tokio::test]
    async fn core_lifetime_blocks_handoff_with_unacknowledged_managed_checkpoint() -> Result<()> {
        let mut host = Host::start("http://127.0.0.1:1")?;
        let result = tokio::time::timeout(CASE_TIMEOUT, async {
            host.ready().await?;
            // The conservative baseline deliberately keeps idle core hosts Busy.
            busy(&host).await?;
            let credential = host.credential().await?;
            let client = GatewayClient::with_options(
                &format!("http://{}", host.inference), Some(&credential),
                TransportOptions { request_timeout: Some(STEP_TIMEOUT), ..TransportOptions::default() },
            )?;
            let capabilities = client.core_capabilities().await?;
            let mut channel = client.core_channel(ChannelOptions::default()).await?;
            let tools = Vec::new();
            let binding = Bind {
                grant: OwnershipGrant {
                    session_id: "lifecycle-session".into(), harness_id: "lifecycle-harness".into(),
                    core_instance_id: capabilities.core_instance_id, execution_epoch: 1,
                },
                durable_head: DurableHead::default(), checkpoint: None,
                manifest: HarnessManifest {
                    tool_manifest_digest: HarnessManifest::digest(&tools)?, tools,
                    workspace_id: "lifecycle-workspace".into(), workspace_revision: None,
                    permission_revision: 1, max_tool_output_bytes: 4096,
                    artifact_quota_bytes: 1024 * 1024, max_artifact_chunk_bytes: 8192,
                    required_features: Vec::new(),
                },
                limits: Limits::default(),
            };
            channel.send(&envelope("bind", "session.bind", serde_json::to_value(&binding)?)?).await?;
            let proposed = tokio::time::timeout(STEP_TIMEOUT, channel.receive()).await??
                .context("channel ended before proposing a checkpoint")?;
            let ServerMessage::Checkpoint(batch) = proposed else {
                bail!("expected the initial bind checkpoint, got {proposed:?}");
            };
            // Receiving a checkpoint proves the HTTP upgrade completed and real
            // managed work is awaiting a harness ACK. The SDK never ACKs for us.
            busy(&host).await?;
            let ack = batch.validate_append(&binding.grant, &binding.durable_head,
                &binding.limits, &BTreeMap::new(), None)?;
            let checkpoint = host.home.join("harness-checkpoint.json");
            let mut file = File::create(&checkpoint)?;
            serde_json::to_writer(&mut file, &batch)?;
            file.sync_all()?;
            drop(file);
            let retained: CheckpointBatch = serde_json::from_reader(File::open(checkpoint)?)?;
            ensure!(retained == batch, "checkpoint was not durably retained before ACK");
            channel.send(&envelope("ack-bind", "checkpoint.ack", serde_json::to_value(&ack)?)?).await?;
            tokio::time::timeout(STEP_TIMEOUT, async {
                loop {
                    match channel.receive().await? {
                        Some(ServerMessage::Head(head)) => {
                            ensure!(head == ack.head(), "ACK established the wrong durable head");
                            return Ok::<_, anyhow::Error>(());
                        }
                        Some(ServerMessage::Receipt(_)) => {}
                        other => bail!("unexpected bind acknowledgement: {other:?}"),
                    }
                }
            }).await.context("channel did not accept ACK after rejected handoff")??;
            channel.send(&envelope("inspect", "operation.get", json!({"target_operation_id": "not-submitted"}))?).await?;
            let inspected = tokio::time::timeout(STEP_TIMEOUT, channel.receive()).await??;
            ensure!(matches!(inspected, Some(ServerMessage::UnknownOperation { operation_id }) if operation_id == "not-submitted"),
                "channel was unusable after rejected handoff");
            channel.close().await?;
            match tokio::time::timeout(STEP_TIMEOUT, channel.receive()).await? {
                Ok(None) => {}
                // Match the existing managed-channel SDK test: the server
                // currently drops its split socket without flushing Close.
                Err(bitrouter_sdk::error::Error::CoreChannel("peer ended without a closing handshake")) => {}
                other => bail!("unexpected channel close result: {other:?}"),
            }
            drop(channel);
            // Disconnect is not durable completion and must not release the
            // official host's conservative core lifetime admission.
            busy(&host).await?;
            ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
            ensure!(host.exited().await?.success(), "orderly core host stop failed");
            host.ensure_closed().await?;
            Ok(())
        }).await.context("managed-core lifecycle case timed out").and_then(|result| result);
        host.finish(result)
    }
}
