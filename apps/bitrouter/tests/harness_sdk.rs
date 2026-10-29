//! Exercise the public execution SDK against a separate official executable.
//! The client uses no application library, local IPC or server store handles.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::{
    api,
    runtime::ErrorCode,
    thread::{ThreadChange, ThreadObservation},
    turn::{TurnSnapshot, TurnStatus},
};
use bitrouter_sdk::{error::Error, gateway::TransportOptions, harness::HarnessClient};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TOKEN: &str = "fixture-harness-sdk-execution-credential";

struct Server {
    child: Child,
    config: PathBuf,
    log: PathBuf,
}

impl Server {
    fn start(config: &Path, log: &Path) -> Result<Self> {
        let output = std::fs::File::create(log)?;
        let child = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(["serve", "--config"])
            .arg(config)
            .env("BRO_HARNESS_SDK_TEST_TOKEN", TOKEN)
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .stdout(output.try_clone()?)
            .stderr(output)
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self {
            child,
            config: config.into(),
            log: log.into(),
        })
    }

    async fn connect(&mut self, endpoint: &str) -> Result<HarnessClient> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let options = TransportOptions {
                connect_timeout: Duration::from_secs(1),
                read_timeout: Some(Duration::from_secs(5)),
                ..TransportOptions::default()
            };
            if let Ok(client) = HarnessClient::connect_with_options(endpoint, TOKEN, options).await
            {
                return Ok(client);
            }
            ensure!(
                self.child.try_wait()?.is_none(),
                "server exited: {}",
                std::fs::read_to_string(&self.log)?
            );
            ensure!(
                tokio::time::Instant::now() < deadline,
                "server not ready: {}",
                std::fs::read_to_string(&self.log)?
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn stop(&mut self) -> Result<()> {
        let stopped = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(env!("CARGO_BIN_EXE_bro"))
                .args(["stop", "--config"])
                .arg(&self.config)
                .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
                .output(),
        )
        .await??;
        ensure!(
            stopped.status.success(),
            "stop failed: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        let status = tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await??;
        ensure!(
            status.success(),
            "server failed: {}",
            std::fs::read_to_string(&self.log)?
        );
        Ok(())
    }
}

fn generation(delta: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id":"fixture-harness-sdk", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],
            "usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18},
        })
    )
}

fn create_request(workspace: &Path) -> api::CreateThreadRequest {
    api::CreateThreadRequest {
        skills: None,
        workspace: workspace.into(),
        model: "test-model".into(),
        model_mode: bitrouter_ai::core::protocol::ModelMode::Fixed,
        effort: None,
        max_output_tokens: Some(512),
        read_only: false,
        verification_command: None,
    }
}

async fn wait_turn(
    client: &HarnessClient,
    thread: &str,
    turn: &str,
    status: TurnStatus,
) -> Result<TurnSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = client.read_turn(thread, turn).await?;
        if snapshot.status == status {
            return Ok(snapshot);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Turn failed to reach {status:?}: {:?}",
            snapshot.status
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn service_code(error: Error, expected: ErrorCode) -> Result<()> {
    let Error::Harness(failure) = error else {
        anyhow::bail!("expected typed harness error: {error}");
    };
    ensure!(failure.body.context("missing typed service error")?.code == expected);
    Ok(())
}

#[tokio::test]
async fn sdk_execution_preserves_acceptance_approval_history_and_restart_identity() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(|request:&wiremock::Request|{
        let body=String::from_utf8_lossy(&request.body);
        let (delta,finish)=if body.contains("approval probe") && !body.contains("blocked-write") {
            (json!({"role":"assistant","tool_calls":[{"index":0,"id":"blocked-write","type":"function",
                "function":{"name":"write","arguments":json!({"path":"never.txt","content":"not authorized"}).to_string()}}]}),"tool_calls")
        } else {(json!({"role":"assistant","content":"Done."}),"stop")};
        ResponseTemplate::new(200).set_body_raw(generation(delta,finish),"text/event-stream")
    }).mount(&upstream).await;
    let home = tempfile::tempdir()?;
    let workspace = home.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let other_workspace = home.path().join("ungranted");
    std::fs::create_dir(&other_workspace)?;
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = probe.local_addr()?;
    drop(probe);
    let endpoint = format!("http://{address}/agent/v2");
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\nagent_api:\n  enabled: true\n  listen: '{address}'\n  token_env: BRO_HARNESS_SDK_TEST_TOKEN\n  workspaces: ['{}']\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models: [{{id: test-model}}]\n",
            workspace.display(),
            home.path().join("database.db").display(),
            upstream.uri()
        ),
    )?;
    let mut server = Server::start(&config, &home.path().join("server.log"))?;
    let client = server.connect(&endpoint).await?;
    ensure!(client.capabilities().runtime.native_execution);
    let instance = client.capabilities().runtime.server_instance_id.clone();
    let bad_credential = HarnessClient::connect(&endpoint, "fixture-inference-key")
        .await
        .err()
        .context("inference credential accepted")?;
    let Error::Harness(failure) = bad_credential else {
        anyhow::bail!("expected HTTP authentication failure");
    };
    ensure!(failure.status == 401);
    service_code(
        client
            .create_thread("ungranted", &create_request(&other_workspace))
            .await
            .err()
            .context("workspace accepted")?,
        ErrorCode::InvalidRequest,
    )?;

    let created = client
        .create_thread("create", &create_request(&workspace))
        .await?;
    let duplicate = client
        .create_thread("create", &create_request(&workspace))
        .await?;
    ensure!(created.thread_id == duplicate.thread_id);
    let thread = &created.thread_id;
    let input = api::TurnRequest {
        prompt: "Say done".into(),
        mode: api::TurnMode::Start,
    };
    let receipt = client.submit_turn(thread, "turn", &input).await?;
    let duplicate = client.submit_turn(thread, "turn", &input).await?;
    ensure!(receipt.turn_id == duplicate.turn_id);
    let conflict = api::TurnRequest {
        prompt: "Different payload".into(),
        mode: api::TurnMode::Start,
    };
    service_code(
        client
            .submit_turn(thread, "turn", &conflict)
            .await
            .err()
            .context("conflicting key accepted")?,
        ErrorCode::Conflict,
    )?;
    let completed = wait_turn(&client, thread, &receipt.turn_id, TurnStatus::Completed).await?;
    ensure!(completed.final_answer.as_deref() == Some("Done."));
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing provider records")?
            .len()
            == 1,
        "duplicate acceptance dispatched twice"
    );

    let approval_thread = client
        .create_thread("approval-thread", &create_request(&workspace))
        .await?;
    let approval_id = &approval_thread.thread_id;
    let active = client
        .submit_turn(
            approval_id,
            "approval-turn",
            &api::TurnRequest {
                prompt: "approval probe".into(),
                mode: api::TurnMode::Start,
            },
        )
        .await?;
    let waiting = wait_turn(
        &client,
        approval_id,
        &active.turn_id,
        TurnStatus::WaitingForInput,
    )
    .await?;
    let approval = waiting.pending_input.context("missing approval metadata")?;
    let mut observation = client
        .observe(approval_id, &api::ObserveQuery { after: None })
        .await?;
    let initial = tokio::time::timeout(Duration::from_secs(3), observation.next())
        .await?
        .context("missing observation")??;
    ensure!(matches!(initial.data, ThreadObservation::Snapshot { .. }));
    drop(observation);
    ensure!(
        client.read_turn(approval_id, &active.turn_id).await?.status == TurnStatus::WaitingForInput
    );
    ensure!(!workspace.join("never.txt").exists());
    let queued = client
        .submit_turn(
            approval_id,
            "queued",
            &api::TurnRequest {
                prompt: "must not execute".into(),
                mode: api::TurnMode::Enqueue,
            },
        )
        .await?;
    let cancelled = client
        .cancel_turn(
            approval_id,
            &queued.turn_id,
            "withdraw",
            &api::CancelRequest {
                mode: api::CancelMode::Queued,
            },
        )
        .await?;
    ensure!(cancelled.status == TurnStatus::Cancelled);
    let steering = client
        .steer(
            approval_id,
            "steering",
            &api::SteeringRequest {
                expected_turn_id: active.turn_id.clone(),
                text: "Keep the workspace unchanged".into(),
            },
        )
        .await?;
    ensure!(steering.turn_id == active.turn_id);
    // Steering retires the old approval before another effect can be admitted.
    service_code(
        client
            .answer_input(
                approval_id,
                "stale-answer",
                &api::InputRequest {
                    skill_consent: false,
                    turn_id: active.turn_id.clone(),
                    request_id: approval.request_id,
                    approved: true,
                },
            )
            .await
            .err()
            .context("retired approval was accepted")?,
        ErrorCode::Conflict,
    )?;
    wait_turn(&client, approval_id, &active.turn_id, TurnStatus::Completed).await?;
    ensure!(!workspace.join("never.txt").exists());

    // A distinct pending approval is answered explicitly, without steering it.
    let denied_thread = client
        .create_thread("denial-thread", &create_request(&workspace))
        .await?;
    let denied = client
        .submit_turn(
            &denied_thread.thread_id,
            "denial-turn",
            &api::TurnRequest {
                prompt: "approval probe".into(),
                mode: api::TurnMode::Start,
            },
        )
        .await?;
    let pending = wait_turn(
        &client,
        &denied_thread.thread_id,
        &denied.turn_id,
        TurnStatus::WaitingForInput,
    )
    .await?
    .pending_input
    .context("missing explicit denial target")?;
    client
        .answer_input(
            &denied_thread.thread_id,
            "deny",
            &api::InputRequest {
                skill_consent: false,
                turn_id: denied.turn_id.clone(),
                request_id: pending.request_id,
                approved: false,
            },
        )
        .await
        .context("explicit denial failed")?;
    wait_turn(
        &client,
        &denied_thread.thread_id,
        &denied.turn_id,
        TurnStatus::Completed,
    )
    .await?;
    ensure!(!workspace.join("never.txt").exists());
    service_code(
        client
            .resume_queue(thread, "idle-resume")
            .await
            .err()
            .context("idle queue resumed")?,
        ErrorCode::Conflict,
    )?;

    let mut after = 0;
    let mut cutoff = None;
    let mut events = Vec::new();
    loop {
        let page = client
            .history(
                thread,
                &api::HistoryQuery {
                    after,
                    cutoff,
                    limit: 1,
                },
            )
            .await?;
        ensure!(page.server_instance_id == instance);
        if let Some(cutoff) = cutoff {
            ensure!(page.cutoff == cutoff);
        }
        cutoff = Some(page.cutoff);
        events.extend(page.events);
        let Some(next) = page.next_after else {
            break;
        };
        ensure!(next > after);
        after = next;
    }
    ensure!(events.iter().any(|event| {
        event
            .changes
            .iter()
            .any(|change| matches!(change, ThreadChange::AssistantResponse { .. }))
    }));
    let mut observation = client
        .observe(thread, &api::ObserveQuery { after: Some(0) })
        .await?;
    ensure!(matches!(
        tokio::time::timeout(Duration::from_secs(3), observation.next())
            .await?
            .context("missing terminal snapshot")??
            .data,
        ThreadObservation::Snapshot { .. }
    ));
    ensure!(
        tokio::time::timeout(Duration::from_millis(40), observation.next())
            .await
            .is_err(),
        "settled Turn ended its continuing attachment"
    );
    drop(observation);
    server.stop().await?;

    let mut restarted = Server::start(&config, &home.path().join("restarted.log"))?;
    let current = restarted.connect(&endpoint).await?;
    ensure!(current.capabilities().runtime.server_instance_id != instance);
    service_code(
        client
            .read_thread(thread)
            .await
            .err()
            .context("stale client rebound")?,
        ErrorCode::InstanceChanged,
    )?;
    let stored = current.read_thread(thread).await?;
    ensure!(stored.thread.server_instance_id == current.capabilities().runtime.server_instance_id);
    let page = current
        .history(
            thread,
            &api::HistoryQuery {
                after: 0,
                cutoff: None,
                limit: 1000,
            },
        )
        .await?;
    ensure!(
        page.events
            .iter()
            .any(|event| event.server_instance_id == instance)
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("missing provider records")?
            .len()
            == 5,
        "unexpected execution after inspection/restart"
    );
    restarted.stop().await?;
    Ok(())
}
