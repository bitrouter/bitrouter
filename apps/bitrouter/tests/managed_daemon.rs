//! Actual CLI/daemon/process proof over the original native managed journal.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::managed_task::{
    local::Client,
    protocol::{Operation, Report, socket_path},
};
use bitrouter_orchestrator::core::protocol::{
    CommitStatus, CoreError, ErrorCode, TaskInput, ToolExecute, ToolOutcome,
};
use bitrouter_orchestrator::harness::{
    HarnessConfig,
    managed::{
        session::{NativeApproval, NativeSession, StoredSession},
        store::NativeStore,
    },
};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

#[path = "../../../crates/bitrouter-sdk/tests/support/mcp_fixture.rs"]
mod mcp_fixture;

#[path = "managed_daemon/skills.rs"]
mod skills;

struct Fixture {
    home: tempfile::TempDir,
    workspace: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    daemon: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.home.path().join("effect-ready").exists() {
            let _ = std::fs::write(self.home.path().join("effect-gate"), b"cleanup");
            for _ in 0..20 {
                if self.home.path().join("effect-done").exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            use std::io::{BufRead, Write};
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            if stream.write_all(b"{\"cmd\":\"stop\"}\n").is_ok() {
                let _ = std::io::BufReader::new(stream).read_line(&mut String::new());
            }
        }
        if let Some(child) = self.daemon.as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl Fixture {
    async fn new(upstream: &str) -> Result<Self> {
        Self::configured(upstream, |_, _| Ok(())).await
    }

    async fn configured(
        upstream: &str,
        configure: impl FnOnce(&std::path::Path, &mut Value) -> Result<()>,
    ) -> Result<Self> {
        let home = tempfile::tempdir_in("/tmp")?;
        let root = home.path().canonicalize()?;
        let workspace = root.join("project");
        std::fs::create_dir(&workspace)?;
        let config = root.join("bitrouter.yaml");
        let mut settings = json!({
            "inherit_defaults":false,"registry":{"enabled":false},
            "server":{"listen":"127.0.0.1:0","skip_auth":true},
            "database":{"url":format!("sqlite://{}?mode=rwc",root.join("state.db").display())},
            "chat":{"model":"coding"},
            "providers":{"fixture":{"api_base":upstream,"api_key":"fixture","api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]}},
            "models":{"coding":{"endpoints":[{"provider":"fixture","service_id":"model"}]}},
        });
        configure(&root, &mut settings)?;
        std::fs::write(&config, serde_json::to_vec_pretty(&settings)?)?;
        let mut fixture = Self {
            home,
            workspace,
            config,
            socket: root.join("bitrouter.sock"),
            daemon: None,
        };
        fixture.start().await?;
        Ok(fixture)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("BITROUTER_HOME", self.home.path())
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "bitrouter_orchestrator::core::session::preparation_work=debug,bitrouter::evolution::runtime=debug")
            .env("BITROUTER_TEST_DECISION_KEY", "fixture")
            .env(
                "BITROUTER_TEST_AGENT_TOKEN",
                "fixture-native-api-token-minimum24",
            )
            .current_dir(self.home.path())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn start(&mut self) -> Result<()> {
        ensure!(self.daemon.is_none());
        let log = std::fs::File::create(self.home.path().join("daemon.log"))?;
        self.daemon = Some(
            self.command()
                .args(["serve", "--config"])
                .arg(&self.config)
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?,
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(daemon) = self.daemon.as_mut()
                    && let Some(status) = daemon.try_wait()?
                {
                    anyhow::bail!(
                        "daemon {status}: {}",
                        std::fs::read_to_string(self.home.path().join("daemon.log"))?
                    );
                }
                if bitrouter::daemon::probe_status(&self.socket)
                    .await?
                    .is_some()
                    && Client::connect(&socket_path(&self.socket)).await.is_ok()
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        bitrouter::daemon::send_command(&self.socket, &bitrouter::daemon::DaemonCommand::Stop)
            .await
            .context("stopping the fixture's selected daemon")?;
        if let Some(mut daemon) = self.daemon.take() {
            let status = tokio::time::timeout(Duration::from_secs(25), daemon.wait()).await??;
            ensure!(status.success(), "daemon exited {status}");
        } else {
            tokio::time::timeout(Duration::from_secs(25), async {
                // A status request can be accepted just as graceful shutdown
                // closes ingress. Observe final teardown instead of requiring
                // a dying control server to answer another request.
                while self.socket.exists() || self.socket.with_extension("pid").exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
        }
        Ok(())
    }

    async fn crash(&mut self) -> Result<()> {
        let mut child = self.daemon.take().context("missing owned daemon")?;
        child.start_kill()?;
        tokio::time::timeout(Duration::from_secs(15), child.wait()).await??;
        Ok(())
    }

    async fn task(&self, prompt: &str, key: &str) -> Result<std::process::Output> {
        Ok(tokio::time::timeout(
            Duration::from_secs(45),
            self.command()
                .args([
                    "task",
                    "managed",
                    prompt,
                    "--session",
                    "continued",
                    "--request-id",
                    key,
                    "--read-only",
                    "--max-output-tokens",
                    "128",
                    "--config",
                ])
                .arg(&self.config)
                .arg("--workspace")
                .arg(&self.workspace)
                .output(),
        )
        .await??)
    }

    fn coding_command(&self, key: &str) -> Command {
        let mut command = self.command();
        command
            .args([
                "task",
                "managed",
                "perform one bounded native effect",
                "--session",
                "continued",
                "--request-id",
                key,
                "--max-output-tokens",
                "512",
                "--config",
            ])
            .arg(&self.config)
            .arg("--workspace")
            .arg(&self.workspace);
        command
    }

    async fn inspect(&self, key: &str) -> Result<Report> {
        let output = self
            .command()
            .args([
                "task",
                "managed-status",
                "--session",
                "continued",
                "--request-id",
                key,
                "--config",
            ])
            .arg(&self.config)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout)?;
        Ok(serde_json::from_value(value["payload"].clone())?)
    }

    async fn store(&self) -> Result<bitrouter::managed_store::DatabaseNativeStore> {
        let db = bitrouter::db::connect(&format!(
            "sqlite://{}?mode=rwc",
            self.home.path().join("state.db").display()
        ))
        .await?;
        bitrouter::managed_store::DatabaseNativeStore::new(db, "continued".into())
    }

    async fn stored(&self) -> Result<StoredSession> {
        NativeSession::inspect(&self.store().await?)
            .await?
            .context("missing native journal")
    }
}

fn text_result(text: &str) -> Value {
    json!({"id":"fixture","object":"chat.completion","model":"model", "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
}

fn lines(output: &std::process::Output) -> Result<Vec<Value>> {
    ensure!(
        output.status.success(),
        "CLI failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

async fn wait_ready(fixture: &Fixture) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !fixture.home.path().join("effect-ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_owner_finished(client: &Client, key: &str) -> Result<Report> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let report = client
                .request(Operation::Status {
                    name: "continued".into(),
                    operation_id: Some(key.into()),
                })
                .await?;
            if !report.resident {
                return Ok::<_, anyhow::Error>(report);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

async fn effect_fixture() -> Result<(Fixture, MockServer, Arc<AtomicUsize>)> {
    static EXECUTABLE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let executable = EXECUTABLE
        .get_or_init(|| example_binary::build("bitrouter", "managed_effect_fixture"))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let upstream = MockServer::start().await;
    let fixture = Fixture::new(&upstream.uri()).await?;
    let command = shell_words::join([
        executable.to_string_lossy().into_owned(),
        fixture
            .workspace
            .join("effects.txt")
            .to_string_lossy()
            .into_owned(),
        fixture
            .home
            .path()
            .join("effect-ready")
            .to_string_lossy()
            .into_owned(),
        fixture
            .home
            .path()
            .join("effect-gate")
            .to_string_lossy()
            .into_owned(),
        fixture
            .home
            .path()
            .join("effect-done")
            .to_string_lossy()
            .into_owned(),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move |_:&wiremock::Request| {
            let result = if counter.fetch_add(1,Ordering::SeqCst)==0 {
                json!({"id":"effect","object":"chat.completion","model":"model", "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"native-effect","type":"function","function":{"name":"shell","arguments":json!({"command":command}).to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}})
            } else { text_result("effect complete") };
            ResponseTemplate::new(200).set_body_json(result)
        }).mount(&upstream).await;
    Ok((fixture, upstream, calls))
}

#[tokio::test]
async fn detached_cli_keeps_one_driver_and_cancel_preserves_unknown_effects() -> Result<()> {
    let (mut fixture, _upstream, calls) = effect_fixture().await?;
    let mut submitting = fixture
        .coding_command("effect")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_ready(&fixture).await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let before = client
        .request(Operation::Status {
            name: "continued".into(),
            operation_id: Some("effect".into()),
        })
        .await?;
    ensure!(before.resident && before.receipt.is_some() && !before.released);
    submitting.start_kill()?;
    submitting.wait().await?;
    let connected = fixture
        .command()
        .args(["agents", "--config"])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(
        connected.status.success(),
        "{}",
        String::from_utf8_lossy(&connected.stderr)
    );
    let connected: Value = serde_json::from_slice(&connected.stdout)?;
    ensure!(
        connected["instances"]
            .as_array()
            .context("missing connected instances")?
            .iter()
            .any(|r| r["kind"] == "managed_native_session"
                && r["session_id"].as_str()
                    == before.grant.as_ref().map(|g| g.session_id.as_str())),
        "{connected}"
    );
    let duplicate = fixture
        .coding_command("effect")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let wrong = fixture
        .command()
        .args([
            "task",
            "managed-cancel",
            "--session",
            "continued",
            "--request-id",
            "other",
            "--server-instance",
        ])
        .arg(&client.daemon_instance_id)
        .arg("--config")
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(!wrong.status.success());
    let changed = fixture
        .command()
        .args([
            "task",
            "managed-cancel",
            "--session",
            "continued",
            "--request-id",
            "effect",
            "--server-instance",
            "wrong-boot",
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(!changed.status.success());
    let cancel = fixture
        .command()
        .args([
            "task",
            "managed-cancel",
            "--session",
            "continued",
            "--request-id",
            "effect",
            "--server-instance",
        ])
        .arg(&client.daemon_instance_id)
        .arg("--config")
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(
        cancel.status.success(),
        "{}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    let final_report = wait_owner_finished(&client, "effect").await?;
    ensure!(
        !final_report.released
            && final_report
                .run
                .as_ref()
                .is_some_and(|r| r.status
                    == bitrouter_orchestrator::core::session::RunStatus::RecoveryRequired),
        "{final_report:?}"
    );
    let output =
        tokio::time::timeout(Duration::from_secs(15), duplicate.wait_with_output()).await??;
    ensure!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("recovery_required")
    );
    ensure!(std::fs::read_to_string(fixture.workspace.join("effects.txt"))? == "started\n");
    let native = fixture
        .stored()
        .await?
        .snapshot
        .context("missing native checkpoint")?;
    ensure!(
        native
            .root_turn()
            .context("missing root turn")?
            .invocations
            .iter()
            .any(|call| call
                .result
                .as_ref()
                .is_some_and(|result| result.status == ToolOutcome::EffectUnknown)),
        "partial shell effects lost their original unknown outcome"
    );
    let retry = fixture.coding_command("effect").output().await?;
    ensure!(
        !retry.status.success(),
        "uncertain native input was replayed"
    );
    ensure!(
        calls.load(Ordering::SeqCst) == 1,
        "duplicate submit or cancellation re-ran a model"
    );
    fixture.stop().await?;
    Ok(())
}

struct Approve;

#[async_trait::async_trait]
impl NativeApproval for Approve {
    async fn approve(&self, _: &ToolExecute) -> Result<bool, CoreError> {
        Ok(true)
    }
}

fn native_input(text: &str) -> TaskInput {
    TaskInput {
        text: text.into(),
        model: "coding".into(),
        effort: None,
        max_output_tokens: Some(128),
        context_limit_bytes: None,
        routing: Default::default(),
        max_concurrent_subagents: None,
        discardable_history: None,
        acceptance_criteria: vec![],
        required_materials: vec![],
        verification: None,
        limits: None,
    }
}

#[tokio::test]
async fn original_embedded_journal_continues_in_daemon_without_identity_migration() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(text_result("original embedded context")),
        )
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        fixture.config.clone(),
    ))
    .await?;
    let assembled = bitrouter::assemble::build_app(&config).await?;
    let store = Arc::new(bitrouter::managed_store::DatabaseNativeStore::new(
        assembled.db,
        "continued".into(),
    )?);
    let mut embedded = NativeSession::open(
        Arc::new(assembled.app),
        bitrouter_sdk::caller::CallerContext::local(),
        store.clone(),
        &fixture.workspace,
        bitrouter_orchestrator::agent::ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?;
    embedded
        .run(
            native_input("old embedded input"),
            Arc::new(Approve),
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    embedded.close().await?;
    let old = fixture.stored().await?;
    ensure!(old.released && old.grant.execution_epoch == 1);
    lines(
        &fixture
            .task("continue original journal", "daemon-input")
            .await?,
    )?;
    let next = fixture.stored().await?;
    ensure!(
        next.released
            && next.grant.execution_epoch == 2
            && next.grant.session_id == old.grant.session_id
            && next.grant.harness_id == old.grant.harness_id
    );
    let requests = upstream
        .received_requests()
        .await
        .context("missing requests")?;
    ensure!(requests.len() == 2);
    let resumed: Value = serde_json::from_slice(&requests[1].body)?;
    ensure!(
        resumed["messages"]
            .to_string()
            .contains("original embedded context")
    );
    fixture.stop().await?;
    Ok(())
}

struct LostInputAck {
    inner: bitrouter::managed_store::DatabaseNativeStore,
    lost: AtomicBool,
}

#[async_trait::async_trait]
impl NativeStore for LostInputAck {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError> {
        self.inner.load().await
    }

    async fn save(&self, revision: u64, bytes: Vec<u8>) -> Result<(), CoreError> {
        let journal: Value = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError::rejected(ErrorCode::CheckpointUnavailable, e.to_string()))?;
        let accepted = if journal["checkpoint"].is_object() {
            let batch: bitrouter_orchestrator::core::checkpoint::CheckpointBatch =
                serde_json::from_value(journal["checkpoint"].clone()).map_err(|e| {
                    CoreError::rejected(ErrorCode::CheckpointUnavailable, e.to_string())
                })?;
            let limits = serde_json::from_value(journal["limits"].clone()).map_err(|e| {
                CoreError::rejected(ErrorCode::CheckpointUnavailable, e.to_string())
            })?;
            batch
                .decode(&limits)?
                .events
                .iter()
                .any(|e| e.kind == "input.accepted")
        } else {
            false
        };
        self.inner.save(revision, bytes).await?;
        if accepted && !self.lost.swap(true, Ordering::SeqCst) {
            return Err(CoreError {
                code: ErrorCode::CheckpointUnavailable,
                message: "fixture lost durable input ACK".into(),
                commit_status: CommitStatus::Unknown,
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn durable_input_ack_uncertainty_remains_blocked_without_replay_or_forced_release()
-> Result<()> {
    let upstream = MockServer::start().await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        fixture.config.clone(),
    ))
    .await?;
    let assembled = bitrouter::assemble::build_app(&config).await?;
    let store = Arc::new(LostInputAck {
        inner: bitrouter::managed_store::DatabaseNativeStore::new(
            assembled.db,
            "continued".into(),
        )?,
        lost: AtomicBool::new(false),
    });
    let embedded = NativeSession::open(
        Arc::new(assembled.app),
        bitrouter_sdk::caller::CallerContext::local(),
        store.clone(),
        &fixture.workspace,
        bitrouter_orchestrator::agent::ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await?;
    let error = embedded
        .start("uncertain", native_input("pending input"))
        .await
        .err()
        .context("expected lost ACK")?;
    ensure!(error.commit_status == CommitStatus::Unknown);
    ensure!(
        embedded.close().await.is_err(),
        "uncertain native owner was force-released"
    );
    let before = store.load().await?;
    let inspected = fixture.inspect("uncertain").await?;
    ensure!(
        inspected.receipt.is_some()
            && !inspected.released
            && !inspected.resident
            && inspected.state == "recovery_required"
    );
    let duplicate = fixture.task("pending input", "uncertain").await?;
    ensure!(!duplicate.status.success());
    let changed = fixture.task("new input", "new-operation").await?;
    ensure!(
        !changed.status.success()
            && String::from_utf8_lossy(&changed.stderr).contains("RecoveryRequired")
    );
    ensure!(
        before == store.load().await?,
        "inspection or retry changed uncertain authority"
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing requests")?
            .is_empty(),
        "accepted input was driven after a lost owner ACK"
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn abrupt_daemon_loss_keeps_original_journal_and_effects_recovery_blocked() -> Result<()> {
    let (mut fixture, _upstream, calls) = effect_fixture().await?;
    let child = fixture
        .coding_command("lost-owner")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    wait_ready(&fixture).await?;
    let original = fixture.inspect("lost-owner").await?;
    ensure!(original.resident && original.receipt.is_some());
    fixture.crash().await?;
    let client_output =
        tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await??;
    ensure!(
        !client_output.status.success(),
        "CLI recreated an embedded execution owner"
    );
    fixture.start().await?;
    let cold = fixture.inspect("lost-owner").await?;
    ensure!(
        !cold.resident
            && !cold.released
            && cold.grant == original.grant
            && cold.receipt == original.receipt
    );
    let before = fixture.store().await?.load().await?;
    let duplicate = fixture.coding_command("lost-owner").output().await?;
    ensure!(
        !duplicate.status.success(),
        "lost-owner request was driven again"
    );
    let new_input = fixture.coding_command("new-input").output().await?;
    ensure!(
        !new_input.status.success()
            && String::from_utf8_lossy(&new_input.stderr).contains("RecoveryRequired")
    );
    ensure!(
        before == fixture.store().await?.load().await?,
        "new daemon changed lost native authority"
    );
    ensure!(
        calls.load(Ordering::SeqCst) == 1
            && std::fs::read_to_string(fixture.workspace.join("effects.txt"))? == "started\n"
    );
    let agents = fixture
        .command()
        .args(["agents", "--config"])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(agents.status.success());
    let agents: Value = serde_json::from_slice(&agents.stdout)?;
    ensure!(
        !agents["instances"]
            .as_array()
            .context("missing live rows")?
            .iter()
            .any(|r| r["kind"] == "managed_native_session"),
        "cold journal became a live connection"
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn cli_startup_and_passive_status_use_the_selected_daemon_without_embedded_fallback()
-> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_result("started daemon")))
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    fixture.stop().await?;
    lines(&fixture.task("start the daemon", "startup").await?)?;
    let report = fixture.inspect("startup").await?;
    ensure!(report.released && report.receipt.is_some());
    let before = fixture.store().await?.load().await?;
    fixture.stop().await?;
    let passive = fixture
        .command()
        .args([
            "task",
            "managed-status",
            "--session",
            "continued",
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(!passive.status.success(), "offline query started an owner");
    ensure!(
        bitrouter::daemon::probe_status(&fixture.socket)
            .await?
            .is_none()
    );
    ensure!(before == fixture.store().await?.load().await?);
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing requests")?
            .len()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn version_and_boot_rejection_never_submit_or_fall_back_to_embedded_execution() -> Result<()>
{
    use bitrouter::managed_task::protocol::{
        Command as ManagedCommand, Reply, ResultBody, Submit, VERSION,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let upstream = MockServer::start().await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let request = Submit {
        name: "continued".into(),
        operation_id: "rejected".into(),
        workspace: fixture.workspace.clone(),
        prompt: "must not execute".into(),
        skills: None,
        model: None,
        effort: None,
        max_output_tokens: Some(128),
        check: None,
        read_only: true,
    };
    for (version, boot) in [
        (VERSION + 1, client.daemon_instance_id.as_str()),
        (VERSION, "wrong-boot"),
    ] {
        let mut stream =
            BufReader::new(tokio::net::UnixStream::connect(socket_path(&fixture.socket)).await?);
        let command = ManagedCommand {
            version,
            command_id: "probe".into(),
            daemon_instance_id: Some(boot.into()),
            operation: Operation::Submit(request.clone()),
        };
        stream
            .get_mut()
            .write_all(format!("{}\n", serde_json::to_string(&command)?).as_bytes())
            .await?;
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let response: Reply = serde_json::from_str(&line)?;
        let ResultBody::Error { error } = response.result else {
            anyhow::bail!("invalid contract reached execution")
        };
        ensure!(
            error.code
                == if version != VERSION {
                    ErrorCode::UnsupportedVersion
                } else {
                    ErrorCode::StaleEpoch
                }
        );
    }
    ensure!(fixture.store().await?.load().await?.is_none());
    let original = socket_path(&fixture.socket);
    std::fs::rename(&original, fixture.home.path().join("old-managed.sock"))?;
    let listener = tokio::net::UnixListener::bind(&original)?;
    let legacy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let command: ManagedCommand = serde_json::from_str(&line)?;
        ensure!(matches!(command.operation, Operation::Hello));
        let reply = Reply {
            version: 0,
            command_id: Some(command.command_id),
            daemon_instance_id: "legacy".into(),
            result: ResultBody::Hello {
                max_active_sessions: 1,
                skill_consent: false,
                skill_constraints: false,
            },
        };
        stream
            .get_mut()
            .write_all(format!("{}\n", serde_json::to_string(&reply)?).as_bytes())
            .await?;
        Ok::<_, anyhow::Error>(())
    });
    let rejected = fixture.task("must not execute", "legacy").await?;
    ensure!(
        !rejected.status.success()
            && String::from_utf8_lossy(&rejected.stderr).contains("fallback is disabled")
    );
    legacy.await??;
    ensure!(fixture.store().await?.load().await?.is_none());
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing requests")?
            .is_empty()
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn ctrl_c_and_daemon_shutdown_join_native_effects_without_forcing_release() -> Result<()> {
    for stop_daemon in [false, true] {
        let (mut fixture, _upstream, calls) = effect_fixture().await?;
        let child = fixture
            .coding_command("interrupt")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        wait_ready(&fixture).await?;
        let client = Client::connect(&socket_path(&fixture.socket)).await?;
        if stop_daemon {
            fixture.stop().await?;
        } else {
            let pid = child
                .id()
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
                .context("missing client PID")?;
            rustix::process::kill_process(pid, rustix::process::Signal::INT)?;
            let report = wait_owner_finished(&client, "interrupt").await?;
            ensure!(!report.released && !report.resident);
        }
        let output =
            tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await??;
        ensure!(!output.status.success());
        let stored = fixture.stored().await?;
        ensure!(
            !stored.released
                && stored
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.run.as_ref())
                    .is_some_and(|r| r.status
                        == bitrouter_orchestrator::core::session::RunStatus::RecoveryRequired)
        );
        ensure!(
            calls.load(Ordering::SeqCst) == 1
                && std::fs::read_to_string(fixture.workspace.join("effects.txt"))? == "started\n"
        );
        if !stop_daemon {
            fixture.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn managed_capacity_rejects_a_fifth_owner_before_native_admission() -> Result<()> {
    use bitrouter::managed_task::protocol::Submit;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(8))
                .set_body_json(text_result("capacity complete")),
        )
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    for index in 0..5 {
        let workspace = fixture
            .home
            .path()
            .canonicalize()?
            .join(format!("workspace-{index}"));
        std::fs::create_dir(&workspace)?;
        let name = format!("capacity-{index}");
        let result = client
            .request(Operation::Submit(Submit {
                name: name.clone(),
                operation_id: name.clone(),
                workspace,
                prompt: "bounded capacity".into(),
                skills: None,
                model: None,
                effort: None,
                max_output_tokens: Some(128),
                check: None,
                read_only: true,
            }))
            .await;
        if index < 4 {
            ensure!(result?.resident);
        } else {
            let error = result.err().context("fifth owner was admitted")?;
            ensure!(
                error
                    .downcast_ref::<CoreError>()
                    .is_some_and(|e| e.code == ErrorCode::Busy)
            );
            let rejected = client
                .request(Operation::Status {
                    name,
                    operation_id: Some("capacity-4".into()),
                })
                .await?;
            ensure!(
                !rejected.resident && rejected.receipt.is_none() && rejected.state == "not_found"
            );
        }
    }
    for index in 0..4 {
        let name = format!("capacity-{index}");
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let report = client
                    .request(Operation::Status {
                        name: name.clone(),
                        operation_id: Some(name.clone()),
                    })
                    .await?;
                if !report.resident {
                    ensure!(report.released);
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing requests")?
            .len()
            == 4
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn dropped_submit_ack_queries_original_operation_without_resubmit() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let upstream = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(text_result("ACK recovered"))
        })
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let front = socket_path(&fixture.socket);
    let back = fixture.home.path().join("managed-upstream.sock");
    std::fs::rename(&front, &back)?;
    let listener = tokio::net::UnixListener::bind(&front)?;
    let submits = Arc::new(AtomicUsize::new(0));
    let seen = submits.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let drop_once = dropped.clone();
    let proxy = tokio::spawn(async move {
        let result: Result<()> = async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let mut front = BufReader::new(stream);
                let mut command = String::new();
                front.read_line(&mut command).await?;
                let parsed: bitrouter::managed_task::protocol::Command =
                    serde_json::from_str(&command)?;
                let mut back = BufReader::new(tokio::net::UnixStream::connect(&back).await?);
                back.get_mut().write_all(command.as_bytes()).await?;
                let mut reply = String::new();
                back.read_line(&mut reply).await?;
                if matches!(parsed.operation, Operation::Submit(_)) {
                    seen.fetch_add(1, Ordering::SeqCst);
                    if !drop_once.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                }
                front.get_mut().write_all(reply.as_bytes()).await?;
            }
        }
        .await;
        result
    });
    let output = fixture.task("ACK input", "ack").await?;
    let result = lines(&output)?;
    ensure!(result.len() == 2 && result[1]["released"] == true);
    ensure!(
        dropped.load(Ordering::SeqCst)
            && submits.load(Ordering::SeqCst) == 1
            && calls.load(Ordering::SeqCst) == 1
    );
    ensure!(String::from_utf8_lossy(&output.stderr).contains("request_id=ack"));
    fixture.stop().await?;
    proxy.abort();
    let _ = proxy.await;
    Ok(())
}

#[tokio::test]
async fn cli_options_and_verification_evidence_remain_in_the_original_native_checkpoint()
-> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_result("candidate answer")))
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let output = fixture
        .coding_command("verification")
        .args(["--model", "coding", "--effort", "low", "--check", "exit 7"])
        .output()
        .await?;
    // Existing native shell semantics distinguish execution from command exit
    // status. Migration must preserve both the outcome and original payload.
    lines(&output)?;
    let report = fixture.inspect("verification").await?;
    let run = report.run.context("missing verified run")?;
    ensure!(report.released && !report.resident);
    ensure!(run.status == bitrouter_orchestrator::core::session::RunStatus::Completed);
    ensure!(
        run.input.model == "coding"
            && run.input.effort.as_deref() == Some("low")
            && run.input.max_output_tokens == Some(512)
            && run
                .input
                .verification
                .as_ref()
                .is_some_and(|v| v.tool == "shell" && v.arguments["command"] == "exit 7")
    );
    let snapshot = fixture
        .stored()
        .await?
        .snapshot
        .context("missing checkpoint")?;
    ensure!(
        snapshot
            .root_turn()
            .context("missing verified turn")?
            .invocations
            .iter()
            .any(|call| call.dispatch.verification
                && call
                    .result
                    .as_ref()
                    .is_some_and(|result| result.status == ToolOutcome::Succeeded
                        && result.output.contains("\"exit_status\":7")))
    );
    let requests = upstream
        .received_requests()
        .await
        .context("missing generation")?;
    ensure!(requests.len() == 1);
    let invalid = fixture
        .coding_command("invalid-verifier")
        .args(["--check", ""])
        .output()
        .await?;
    ensure!(
        !invalid.status.success(),
        "unexecuted verifier was reported as success"
    );
    let rejected = fixture.inspect("invalid-verifier").await?;
    ensure!(
        rejected.released
            && rejected.run.as_ref().is_some_and(
                |r| r.status == bitrouter_orchestrator::core::session::RunStatus::Failed
            )
    );
    fixture.stop().await?;
    Ok(())
}

#[tokio::test]
async fn daemon_cli_retries_and_restarts_preserve_native_identity_without_replay() -> Result<()> {
    let upstream = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(text_result("managed complete"))
        })
        .mount(&upstream)
        .await;
    let mut fixture = Fixture::new(&upstream.uri()).await?;
    let client = Client::connect(&socket_path(&fixture.socket)).await?;
    let first_output = fixture.task("first input", "first").await?;
    let first = lines(&first_output)?;
    ensure!(
        first.len() == 2 && first[0]["type"] == "managed_session" && first[1]["type"] == "terminal",
        "{first:?}"
    );
    ensure!(first[1]["released"] == true && first[1]["run"]["status"] == "completed");
    let initial = fixture.inspect("first").await?;
    let grant = initial.grant.clone().context("missing grant")?;
    ensure!(grant.execution_epoch == 1 && initial.released && !initial.resident);
    let duplicate = lines(&fixture.task("first input", "first").await?)?;
    ensure!(calls.load(Ordering::SeqCst) == 1 && duplicate[0]["grant"]["execution_epoch"] == 1);
    let settled_cancel = client
        .request(Operation::Cancel {
            name: "continued".into(),
            operation_id: "first".into(),
        })
        .await?;
    ensure!(settled_cancel.released && settled_cancel.receipt == initial.receipt);
    let before = fixture.store().await?.load().await?;
    let mismatch = fixture.coding_command("mode-mismatch").output().await?;
    ensure!(
        !mismatch.status.success()
            && String::from_utf8_lossy(&mismatch.stderr).contains("RecoveryRequired")
    );
    let other = fixture.home.path().join("other-project");
    std::fs::create_dir(&other)?;
    let mismatch = fixture
        .command()
        .args([
            "task",
            "managed",
            "must not execute",
            "--session",
            "continued",
            "--request-id",
            "workspace-mismatch",
            "--read-only",
            "--workspace",
        ])
        .arg(&other)
        .arg("--config")
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(
        !mismatch.status.success()
            && String::from_utf8_lossy(&mismatch.stderr).contains("RecoveryRequired")
    );
    ensure!(before == fixture.store().await?.load().await? && calls.load(Ordering::SeqCst) == 1);
    let human = fixture
        .command()
        .args([
            "--human",
            "task",
            "managed-status",
            "--session",
            "continued",
            "--request-id",
            "first",
            "--config",
        ])
        .arg(&fixture.config)
        .output()
        .await?;
    ensure!(human.status.success() && String::from_utf8_lossy(&human.stdout).contains("released"));
    let changed = fixture.task("changed first input", "first").await?;
    ensure!(
        !changed.status.success()
            && String::from_utf8_lossy(&changed.stderr).contains("OperationConflict")
    );
    lines(&fixture.task("second input", "second").await?)?;
    let second = fixture.inspect("second").await?;
    let continued = second.grant.context("missing continued grant")?;
    ensure!(
        continued.session_id == grant.session_id
            && continued.harness_id == grant.harness_id
            && continued.execution_epoch == 2
    );
    ensure!(calls.load(Ordering::SeqCst) == 2);
    let historical = fixture.inspect("first").await?;
    ensure!(
        historical.state == "accepted_historical"
            && historical.receipt.is_some()
            && historical.run.is_none()
    );
    fixture.stop().await?;
    fixture.start().await?;
    ensure!(
        client
            .request(Operation::Status {
                name: "continued".into(),
                operation_id: Some("second".into())
            })
            .await
            .is_err(),
        "old client switched daemon boots"
    );
    let restored = fixture.inspect("second").await?;
    ensure!(restored.grant.as_ref() == Some(&continued) && restored.released && !restored.resident);
    lines(&fixture.task("second input", "second").await?)?;
    ensure!(
        calls.load(Ordering::SeqCst) == 2,
        "accepted operation replayed after restart"
    );
    let large_input = "third input ".repeat(6500);
    lines(&fixture.task(&large_input, "third").await?)?;
    let third = fixture
        .inspect("third")
        .await?
        .grant
        .context("missing third grant")?;
    ensure!(
        third.session_id == grant.session_id
            && third.harness_id == grant.harness_id
            && third.execution_epoch == 3
    );
    ensure!(calls.load(Ordering::SeqCst) == 3);
    let requests = upstream
        .received_requests()
        .await
        .context("missing requests")?;
    let resumed: Value = serde_json::from_slice(&requests[1].body)?;
    let first_request: Value = serde_json::from_slice(&requests[0].body)?;
    ensure!(
        first_request["max_tokens"] == 128 || first_request["max_completion_tokens"] == 128,
        "{first_request}"
    );
    let tools = first_request["tools"]
        .as_array()
        .context("missing read-only tools")?;
    ensure!(
        !tools.iter().any(|tool| matches!(
            tool["function"]["name"].as_str(),
            Some("shell" | "write" | "edit")
        )),
        "read-only task gained effectful tools"
    );
    ensure!(
        resumed["messages"].to_string().contains("managed complete"),
        "prior context disappeared"
    );
    fixture.stop().await?;
    Ok(())
}
