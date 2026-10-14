//! Retained native API contracts migrated from the retired Code product UI.
//! Goal terminal coverage lives in goal_tui_pty; model judgments here are fixtures.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use bitrouter::agent_local::{Operation, ReplyResult, ThreadClient};
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::thread::{ThreadObservation, ThreadStatus};
use bitrouter_orchestrator::turn::{TurnStatus, VerificationStatus};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct Server {
    home: tempfile::TempDir,
    workspace: PathBuf,
    control: PathBuf,
    socket: PathBuf,
    child: tokio::process::Child,
}
impl Server {
    async fn start(upstream: &MockServer, extra: &str, managed: bool) -> Result<Self> {
        let home = tempfile::tempdir()?;
        let workspace = home.path().join("project");
        std::fs::create_dir(&workspace)?;
        let config = home.path().join("bitrouter.yaml");
        std::fs::write(
            &config,
            format!(
                "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite://{}?mode=rwc'}}\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\n{}",
                home.path().join("state.db").display(),
                upstream.uri(),
                extra
            ),
        )?;
        let source = bitrouter::paths::ConfigSource::File(config.clone());
        let cfg = bitrouter::paths::load_config(&source).await?;
        let control = bitrouter::daemon::socket_path_for(&source, &cfg);
        let socket = bitrouter::agent_local::socket_path(&control);
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command.args(["serve", "--config"]).arg(&config);
        if managed {
            command.arg("--managed-child");
        }
        let mut child = command
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("BRO_UI_TEST_DECISION_KEY", "fixture")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if matches!(
                bitrouter::agent_local::request(&socket, Operation::Capabilities).await,
                Ok(ReplyResult::Capabilities { .. })
            ) {
                break;
            }
            ensure!(child.try_wait()?.is_none(), "native fixture daemon exited");
            ensure!(
                tokio::time::Instant::now() < deadline,
                "native fixture daemon unavailable"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self {
            home,
            workspace,
            control,
            socket,
            child,
        })
    }
    async fn stop(mut self) -> Result<()> {
        bitrouter::daemon::send_command(&self.control, &bitrouter::daemon::DaemonCommand::Stop)
            .await?;
        tokio::time::timeout(Duration::from_secs(15), self.child.wait()).await??;
        Ok(())
    }
}
struct Execution {
    thread_id: String,
    turn_id: String,
}
async fn submit(socket: &Path, workspace: &Path, check: Option<String>) -> Result<Execution> {
    let client = ThreadClient::connect(socket).await?;
    let (thread, turn) = client
        .create_and_start(
            workspace.into(),
            AgentConfig::fixed("test-model", None),
            check,
            "Change note.txt".into(),
        )
        .await?;
    Ok(Execution {
        thread_id: thread.thread_id,
        turn_id: turn.turn_id,
    })
}
async fn read(
    socket: &Path,
    execution: &Execution,
) -> Result<bitrouter_orchestrator::turn::TurnSnapshot> {
    let ReplyResult::Turn { snapshot } = bitrouter::agent_local::request(
        socket,
        Operation::ReadTurn {
            thread_id: execution.thread_id.clone(),
            turn_id: execution.turn_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("expected native Turn");
    };
    Ok(*snapshot)
}
async fn wait(
    socket: &Path,
    execution: &Execution,
    predicate: impl Fn(&bitrouter_orchestrator::turn::TurnSnapshot) -> bool,
) -> Result<bitrouter_orchestrator::turn::TurnSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = read(socket, execution).await?;
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "native state not reached: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
async fn wait_status(
    socket: &Path,
    execution: &Execution,
    status: TurnStatus,
) -> Result<bitrouter_orchestrator::turn::TurnSnapshot> {
    wait(socket, execution, |snapshot| snapshot.status == status).await
}
async fn approve(
    client: &ThreadClient,
    execution: &Execution,
    snapshot: &bitrouter_orchestrator::turn::TurnSnapshot,
    key: &str,
) -> Result<()> {
    let request = snapshot
        .pending_input
        .as_ref()
        .context("approval request missing")?;
    client
        .request(Operation::Input {
            thread_id: execution.thread_id.clone(),
            turn_id: execution.turn_id.clone(),
            request_id: request.request_id.clone(),
            approved: true,
            idempotency_key: key.into(),
        })
        .await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum QueueEnding {
    Completed,
    Cancelled,
    Refused,
}

fn queue_request(request: &Request) -> Result<(String, bool)> {
    let body: serde_json::Value = serde_json::from_slice(&request.body)?;
    let messages = body["messages"]
        .as_array()
        .context("queue model messages missing")?;
    let index = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .context("queue user message missing")?;
    let content = &messages[index]["content"];
    let prompt = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        content
            .as_array()
            .context("queue user content unsupported")?
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect()
    };
    Ok((
        prompt,
        messages[index + 1..]
            .iter()
            .any(|message| message["role"] == "tool"),
    ))
}

async fn verify_native_queue(ending: QueueEnding) -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(move |request: &Request| {
        let Ok((prompt,after_tool)) = queue_request(request) else { return ResponseTemplate::new(400); };
        let first = prompt == "queue first";
        let (delta,reason) = if first && !after_tool {
            (json!({"role":"assistant","tool_calls":[{"index":0,"id":"queue-write","type":"function","function":{
                "name":"write","arguments":json!({"path":"first.txt","content":"FIRST\n"}).to_string()
            }}]}),"tool_calls")
        } else if first && matches!(ending, QueueEnding::Refused) {
            (json!({"role":"assistant","content":"This fixture refuses to continue."}),"content_filter")
        } else {
            (json!({"role":"assistant","content":format!("Finished {prompt}")}),"stop")
        };
        let chunk = |delta,reason| format!("data: {}\n\n",json!({"id":"queue-model","object":"chat.completion.chunk","model":"test-model",
            "choices":[{"index":0,"delta":delta,"finish_reason":reason}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}));
        ResponseTemplate::new(200).set_body_raw(format!("{}{}data: [DONE]\n\n",chunk(delta,None::<&str>),chunk(json!({}),Some(reason))),"text/event-stream")
    }).mount(&upstream).await;
    let server = Server::start(&upstream, "", false).await?;
    let client = ThreadClient::connect(&server.socket).await?;
    let (thread, turn) = client
        .create_and_start(
            server.workspace.clone(),
            AgentConfig::fixed("test-model", None),
            None,
            "queue first".into(),
        )
        .await?;
    let first = Execution {
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id,
    };
    let pending = wait_status(&server.socket, &first, TurnStatus::WaitingForInput).await?;
    let second_prompt = "  queued second 中文 👩‍💻\nkeep trailing spaces  ";
    let mut queued = Vec::new();
    for (index, prompt) in [second_prompt, "queued third"].into_iter().enumerate() {
        let operation = || Operation::EnqueueTurn {
            thread_id: first.thread_id.clone(),
            prompt: prompt.into(),
            idempotency_key: format!("queue-{index}"),
        };
        let ReplyResult::Receipt { receipt } = client.request(operation()).await? else {
            anyhow::bail!("queue receipt missing");
        };
        let ReplyResult::Receipt { receipt: duplicate } = client.request(operation()).await? else {
            anyhow::bail!("duplicate receipt missing");
        };
        ensure!(
            serde_json::to_value(&receipt)? == serde_json::to_value(&duplicate)?,
            "retry replaced queue identity"
        );
        ensure!(receipt.status == TurnStatus::Queued);
        queued.push(receipt);
    }
    ensure!(queued[0].queue_order < queued[1].queue_order);
    let ReplyResult::View { view: before } = client
        .request(Operation::ReadThread {
            thread_id: first.thread_id.clone(),
        })
        .await?
    else {
        anyhow::bail!("thread view missing");
    };
    ensure!(
        before.thread.status == ThreadStatus::Busy
            && before.thread.active_turn_id.as_deref() == Some(&first.turn_id)
    );
    ensure!(
        before
            .thread
            .queued
            .iter()
            .map(|receipt| &receipt.turn_id)
            .collect::<Vec<_>>()
            == queued
                .iter()
                .map(|receipt| &receipt.turn_id)
                .collect::<Vec<_>>()
    );
    ensure!(
        !server.workspace.join("first.txt").exists(),
        "unapproved tool executed"
    );
    let expected = match ending {
        QueueEnding::Completed => TurnStatus::Completed,
        QueueEnding::Cancelled => {
            let cancel = || Operation::CancelTurn {
                thread_id: first.thread_id.clone(),
                turn_id: first.turn_id.clone(),
                idempotency_key: "cancel-queue-first".into(),
            };
            client.request(cancel()).await?;
            wait_status(&server.socket, &first, TurnStatus::Cancelled).await?;
            client.request(cancel()).await?;
            TurnStatus::Cancelled
        }
        QueueEnding::Refused => TurnStatus::Failed,
    };
    if !matches!(ending, QueueEnding::Cancelled) {
        approve(&client, &first, &pending, "approve-queue-first").await?;
    }
    wait_status(&server.socket, &first, expected).await?;
    if !matches!(ending, QueueEnding::Completed) {
        let ReplyResult::View { view } = client
            .request(Operation::ReadThread {
                thread_id: first.thread_id.clone(),
            })
            .await?
        else {
            anyhow::bail!("paused queue missing");
        };
        ensure!(view.thread.status == ThreadStatus::Paused && view.thread.active_turn_id.is_none());
        ensure!(
            view.thread
                .queued
                .iter()
                .map(|receipt| &receipt.turn_id)
                .collect::<Vec<_>>()
                == queued
                    .iter()
                    .map(|receipt| &receipt.turn_id)
                    .collect::<Vec<_>>()
        );
        let count = upstream
            .received_requests()
            .await
            .context("queue requests unavailable")?
            .len();
        drop(client);
        for _ in 0..3 {
            let reader = ThreadClient::connect(&server.socket).await?;
            let mut observation = reader.observe(&first.thread_id, None).await?;
            tokio::time::timeout(Duration::from_secs(5), observation.next())
                .await??
                .context("paused queue observation missing")?;
            for receipt in &queued {
                ensure!(
                    read(
                        &server.socket,
                        &Execution {
                            thread_id: first.thread_id.clone(),
                            turn_id: receipt.turn_id.clone()
                        }
                    )
                    .await?
                    .status
                        == TurnStatus::Queued
                );
            }
        }
        ensure!(
            upstream
                .received_requests()
                .await
                .context("queue requests unavailable")?
                .len()
                == count,
            "observer drained paused queue"
        );
        if matches!(ending, QueueEnding::Refused) {
            ensure!(std::fs::read_to_string(server.workspace.join("first.txt"))? == "FIRST\n");
            let prompts = upstream
                .received_requests()
                .await
                .context("queue requests unavailable")?
                .iter()
                .map(queue_request)
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                prompts
                    .iter()
                    .map(|(prompt, _)| prompt.as_str())
                    .collect::<Vec<_>>()
                    == vec!["queue first", "queue first"],
                "refusal replayed or drained queued work"
            );
            server.stop().await?;
            return Ok(());
        }
        ensure!(
            !server.workspace.join("first.txt").exists(),
            "cancelled tool executed or replayed"
        );
        let controller = ThreadClient::connect(&server.socket).await?;
        controller
            .request(Operation::ResumeQueue {
                thread_id: first.thread_id.clone(),
                idempotency_key: "explicit-resume".into(),
            })
            .await?;
    }
    for receipt in &queued {
        wait_status(
            &server.socket,
            &Execution {
                thread_id: first.thread_id.clone(),
                turn_id: receipt.turn_id.clone(),
            },
            TurnStatus::Completed,
        )
        .await?;
    }
    let ReplyResult::View { view } = bitrouter::agent_local::request(
        &server.socket,
        Operation::ReadThread {
            thread_id: first.thread_id.clone(),
        },
    )
    .await?
    else {
        anyhow::bail!("settled queue view missing");
    };
    ensure!(
        view.thread.status == ThreadStatus::Idle
            && view.thread.queued.is_empty()
            && view.thread.active_turn_id.is_none()
    );
    let prompts = upstream
        .received_requests()
        .await
        .context("queue requests unavailable")?
        .iter()
        .map(queue_request)
        .collect::<Result<Vec<_>>>()?;
    let expected_prompts = if matches!(ending, QueueEnding::Cancelled) {
        vec!["queue first", second_prompt, "queued third"]
    } else {
        vec!["queue first", "queue first", second_prompt, "queued third"]
    };
    ensure!(
        prompts
            .iter()
            .map(|(prompt, _)| prompt.as_str())
            .collect::<Vec<_>>()
            == expected_prompts,
        "queue did not dispatch in exact FIFO order"
    );
    ensure!(
        server.workspace.join("first.txt").exists() == matches!(ending, QueueEnding::Completed)
    );
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn native_api_queue_drains_in_exact_fifo_order_after_normal_completion() -> Result<()> {
    verify_native_queue(QueueEnding::Completed).await
}

#[tokio::test]
async fn native_api_cancel_pauses_fifo_until_explicit_resume_without_replay() -> Result<()> {
    verify_native_queue(QueueEnding::Cancelled).await
}

#[tokio::test]
async fn native_api_refusal_preserves_paused_fifo_without_replay_or_drain() -> Result<()> {
    verify_native_queue(QueueEnding::Refused).await
}

async fn observation_proxy(
    listener: tokio::net::UnixListener,
    upstream: std::path::PathBuf,
    blocked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    disconnect: tokio::sync::watch::Sender<u64>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            connection=listener.accept()=> {
                let (incoming,_)=connection?;
                let upstream=upstream.clone();let blocked=blocked.clone();let mut disconnect=disconnect.subscribe();
                connections.spawn(async move {
                    let mut incoming=tokio::io::BufReader::new(incoming);
                    let mut line=String::new();incoming.read_line(&mut line).await?;
                    let command:bitrouter::agent_local::ThreadCommand=serde_json::from_str(&line)?;
                    let observing=matches!(command.operation,Operation::Observe { .. });
                    if observing && blocked.load(std::sync::atomic::Ordering::SeqCst) { return Ok::<_,anyhow::Error>(()); }
                    let mut outgoing=tokio::net::UnixStream::connect(&upstream).await?;
                    outgoing.write_all(line.as_bytes()).await?;
                    if observing {
                        tokio::select! {
                            result=tokio::io::copy_bidirectional(&mut incoming,&mut outgoing)=> { result?; },
                            _=disconnect.changed()=> {},
                        }
                    } else { tokio::io::copy_bidirectional(&mut incoming,&mut outgoing).await?; }
                    Ok(())
                });
            },
            Some(_)=connections.join_next(),if !connections.is_empty()=> {},
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_api_approvals_disconnect_queue_context_and_cancel_remain_durable() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|request: &Request| {
            let saw_tool = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|body| body["messages"].as_array().cloned())
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            let (delta, reason) = if saw_tool {
                (json!({"role": "assistant", "content": "Done through native API."}), "stop")
            } else {
                (json!({"role": "assistant", "tool_calls": [{
                    "index": 0, "id": "edit-tui", "type": "function", "function": {
                        "name": "edit", "arguments": json!({"path": "note.txt", "edits": [{"oldText": "before", "newText": "after"}]}).to_string()
                    }
                }]}), "tool_calls")
            };
            let chunk = |delta, reason| format!("data: {}\n\n", json!({
                "id": "chatcmpl-tui", "object": "chat.completion.chunk", "model": "test-model",
                "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
            }));
            ResponseTemplate::new(200).set_body_raw(
                format!("{}{}data: [DONE]\n\n", chunk(delta, None::<&str>), chunk(json!({}), Some(reason))),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;

    let server = Server::start(&upstream, "", false).await?;
    std::fs::write(server.workspace.join("note.txt"), "before\n")?;
    let execution = submit(
        &server.socket,
        &server.workspace,
        Some("test \"$(cat note.txt)\" = after".into()),
    )
    .await?;
    let pending = wait_status(&server.socket, &execution, TurnStatus::WaitingForInput).await?;
    ensure!(
        pending
            .pending_input
            .as_ref()
            .is_some_and(|input| input.tool_name == "edit")
    );
    let client = ThreadClient::connect(&server.socket).await?;
    let ReplyResult::Receipt { receipt: queued } = client
        .request(Operation::EnqueueTurn {
            thread_id: execution.thread_id.clone(),
            prompt: "queued before detach".into(),
            idempotency_key: "queue-before-detach".into(),
        })
        .await?
    else {
        anyhow::bail!("queued receipt missing");
    };
    let proxy_control = server.home.path().join("proxy.sock");
    let proxy_socket = bitrouter::agent_local::socket_path(&proxy_control);
    let listener = tokio::net::UnixListener::bind(&proxy_socket)?;
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (disconnect, _) = tokio::sync::watch::channel(0_u64);
    let proxy = tokio::spawn(observation_proxy(
        listener,
        server.socket.clone(),
        blocked.clone(),
        disconnect.clone(),
    ));
    let proxy_client = ThreadClient::connect(&proxy_socket).await?;
    let mut stream = proxy_client.observe(&execution.thread_id, None).await?;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await??
        .context("initial observation missing")?;
    let ThreadObservation::Snapshot { view, .. } = first else {
        anyhow::bail!("initial snapshot missing");
    };
    ensure!(view.thread.queued.len() == 1 && view.thread.queued[0].turn_id == queued.turn_id);
    let cursor = view.thread.cursor;
    blocked.store(true, std::sync::atomic::Ordering::SeqCst);
    disconnect.send(1)?;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if stream.next().await?.is_none() {
                break;
            }
        }
        Result::<()>::Ok(())
    })
    .await?;
    ended?;
    drop(stream);
    let still_pending =
        wait_status(&server.socket, &execution, TurnStatus::WaitingForInput).await?;
    ensure!(
        still_pending.pending_input_id == pending.pending_input_id,
        "observation disconnect changed approval"
    );
    blocked.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut stream = proxy_client
        .observe(&execution.thread_id, Some(cursor))
        .await?;
    let ThreadObservation::Snapshot { view, .. } =
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await??
            .context("reconnection missing")?
    else {
        anyhow::bail!("reconnection snapshot missing");
    };
    ensure!(
        view.thread.queued.len() == 1
            && view
                .latest_turn
                .as_ref()
                .is_some_and(|turn| turn.pending_input_id == pending.pending_input_id)
    );
    drop(stream);
    proxy.abort();
    let _ = proxy.await;
    let before = upstream
        .received_requests()
        .await
        .context("requests unavailable")?
        .len();
    for _ in 0..9 {
        let reader = ThreadClient::connect(&server.socket).await?;
        let mut stream = reader.observe(&execution.thread_id, None).await?;
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await??
            .context("subscription missing")?;
        drop(stream);
        ensure!(
            read(&server.socket, &execution).await?.pending_input_id == pending.pending_input_id
        );
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .len()
            == before,
        "observation ran a model"
    );
    client
        .request(Operation::CancelQueuedTurn {
            thread_id: execution.thread_id.clone(),
            turn_id: queued.turn_id,
            idempotency_key: "withdraw-queue".into(),
        })
        .await?;
    approve(&client, &execution, &pending, "approve-edit").await?;
    let checking = wait(&server.socket, &execution, |snapshot| {
        snapshot
            .pending_input
            .as_ref()
            .is_some_and(|input| input.tool_name == "shell")
    })
    .await?;
    approve(&client, &execution, &checking, "approve-check").await?;
    let completed = wait_status(&server.socket, &execution, TurnStatus::Completed).await?;
    ensure!(
        completed.verification == VerificationStatus::Passed
            && completed.final_answer.as_deref() == Some("Done through native API.")
    );
    ensure!(std::fs::read_to_string(server.workspace.join("note.txt"))? == "after\n");
    let ReplyResult::Receipt { receipt } = client
        .request(Operation::StartTurn {
            thread_id: execution.thread_id.clone(),
            prompt: "Remember prior answer".into(),
            idempotency_key: "followup".into(),
        })
        .await?
    else {
        anyhow::bail!("followup receipt missing");
    };
    let followup = Execution {
        thread_id: execution.thread_id.clone(),
        turn_id: receipt.turn_id,
    };
    let check = wait_status(&server.socket, &followup, TurnStatus::WaitingForInput).await?;
    approve(&client, &followup, &check, "followup-check").await?;
    wait_status(&server.socket, &followup, TurnStatus::Completed).await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .iter()
            .any(
                |request| serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(
                    |body| {
                        let messages = body["messages"].to_string();
                        messages.contains("Remember prior answer")
                            && messages.contains("Done through native API.")
                            && messages.contains("Change note.txt")
                    }
                )
            ),
        "continuation lost settled context"
    );
    std::fs::write(server.workspace.join("note.txt"), "before\n")?;
    let cancel = submit(&server.socket, &server.workspace, None).await?;
    wait_status(&server.socket, &cancel, TurnStatus::WaitingForInput).await?;
    client
        .request(Operation::CancelTurn {
            thread_id: cancel.thread_id.clone(),
            turn_id: cancel.turn_id.clone(),
            idempotency_key: "cancel-before-edit".into(),
        })
        .await?;
    wait_status(&server.socket, &cancel, TurnStatus::Cancelled).await?;
    ensure!(std::fs::read_to_string(server.workspace.join("note.txt"))? == "before\n");
    server.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_api_cold_directory_history_and_open_preserve_context_without_execution()
-> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(|_: &Request| ResponseTemplate::new(200).set_body_raw(format!("data: {}\n\ndata: [DONE]\n\n", json!({
            "id":"reply", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"Retained answer for this conversation."},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":4,"total_tokens":8}
        })), "text/event-stream")).mount(&upstream).await;

    let server = Server::start(&upstream, "", true).await?;
    let first = submit(&server.socket, &server.workspace, None).await?;
    wait_status(&server.socket, &first, TurnStatus::Completed).await?;
    let second = submit(&server.socket, &server.workspace, None).await?;
    wait_status(&server.socket, &second, TurnStatus::Completed).await?;
    let handoff = bitrouter::daemon::send_command(
        &server.control,
        &bitrouter::daemon::DaemonCommand::HandoffPrepare,
    )
    .await?;
    ensure!(
        matches!(handoff,bitrouter::daemon::DaemonResponse::HandoffBusy {ref reason} if reason.contains("operations")),
        "native residency did not fence replacement: {handoff:?}"
    );
    let client = ThreadClient::connect(&server.socket).await?;
    for id in [&first.thread_id, &second.thread_id] {
        client
            .request(Operation::UnloadThread {
                thread_id: id.clone(),
            })
            .await?;
    }
    let requests = upstream
        .received_requests()
        .await
        .context("requests unavailable")?
        .len();
    let ReplyResult::Directory { page } = client
        .request(Operation::ListThreads {
            after: 0,
            cutoff: None,
            limit: 16,
        })
        .await?
    else {
        anyhow::bail!("directory missing");
    };
    ensure!(
        page.entries
            .iter()
            .any(|row| row.thread.thread_id == first.thread_id)
            && page
                .entries
                .iter()
                .any(|row| row.thread.thread_id == second.thread_id)
    );
    let ReplyResult::View { view } = client
        .request(Operation::ReadThread {
            thread_id: second.thread_id.clone(),
        })
        .await?
    else {
        anyhow::bail!("cold view missing");
    };
    ensure!(view.latest_turn.as_ref().is_some_and(
        |turn| turn.final_answer.as_deref() == Some("Retained answer for this conversation.")
    ));
    let ReplyResult::History { page } = client
        .request(Operation::History {
            thread_id: second.thread_id.clone(),
            after: 0,
            cutoff: None,
            limit: 16,
        })
        .await?
    else {
        anyhow::bail!("history missing");
    };
    ensure!(!page.events.is_empty());
    let mut stream = client.observe(&second.thread_id, None).await?;
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await??
        .context("durable observation missing")?;
    drop(stream);
    ensure!(
        upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .len()
            == requests,
        "reading cold history executed a turn"
    );
    let ReplyResult::Receipt { receipt } = client
        .request(Operation::StartTurn {
            thread_id: second.thread_id.clone(),
            prompt: "Continue the opened conversation".into(),
            idempotency_key: "continue-second".into(),
        })
        .await?
    else {
        anyhow::bail!("continuation receipt missing");
    };
    wait_status(
        &server.socket,
        &Execution {
            thread_id: second.thread_id,
            turn_id: receipt.turn_id,
        },
        TurnStatus::Completed,
    )
    .await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .iter()
            .any(
                |request| serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(
                    |body| body["messages"]
                        .to_string()
                        .contains("Continue the opened conversation")
                        && body["messages"]
                            .to_string()
                            .contains("Retained answer for this conversation.")
                )
            )
    );
    server.stop().await
}

#[tokio::test]
async fn native_api_reports_classifier_generation_and_final_routing_facts() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(|request: &Request| {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else { return ResponseTemplate::new(400) };
        let Some(questions) = body["questions"].as_object() else { return ResponseTemplate::new(400) };
        let answers = questions.iter().map(|(id, question)| {
            let choice = match id.as_str() { "routing_task_family" => "code:frontend_ui", "routing_next_role" => "implement", "routing_progress" => "progressing", _ => "full" };
            let probabilities = question["criteria"].as_object().into_iter().flat_map(|criteria| criteria.keys()).map(|key| (key.clone(), json!(if key == choice {1.0} else {0.0}))).collect::<serde_json::Map<_,_>>();
            (id.clone(), json!({"type":"choice","choice":choice,"confidence":0.99,"probabilities":probabilities}))
        }).collect::<serde_json::Map<_,_>>();
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(json!({"model":"ui-classifier", "answers":answers,"usage":{"input_tokens":12,"output_tokens":3}}))
    }).mount(&upstream).await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_raw(
        format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"id":"ui-generation","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"content":"Status bar complete."},"finish_reason":null}]}), json!({"id":"ui-generation","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}})), "text/event-stream")).mount(&upstream).await;

    let extra = format!(
        "decision_model:\n  model: ui-classifier\n  base_url: {}\n  api_key_env: BRO_UI_TEST_DECISION_KEY\n",
        upstream.uri()
    );
    let server = Server::start(&upstream, &extra, false).await?;
    let client = ThreadClient::connect(&server.socket).await?;
    let config = AgentConfig::fixed("test-model", None)
        .read_only()
        .with_model_mode(bitrouter_orchestrator::core::protocol::ModelMode::Policy);
    let (thread, turn) = client
        .create_and_start(
            server.workspace.clone(),
            config,
            None,
            "Improve the frontend UI. Reply briefly without tools.".into(),
        )
        .await?;
    let execution = Execution {
        thread_id: thread.thread_id,
        turn_id: turn.turn_id,
    };
    use bitrouter_orchestrator::core::context_router::inspection::RoutingPhase;
    for phase in [RoutingPhase::Classifying, RoutingPhase::Generating] {
        let snapshot = wait(&server.socket, &execution, |snapshot| {
            snapshot.context_routing.as_ref().is_some_and(|inspection| {
                inspection.tasks.iter().any(|task| {
                    task.routing
                        .as_ref()
                        .is_some_and(|routing| routing.phase == phase)
                })
            })
        })
        .await?;
        if phase == RoutingPhase::Generating {
            ensure!(
                snapshot
                    .context_routing
                    .as_ref()
                    .is_some_and(|inspection| inspection.tasks.iter().any(|task| task
                        .routing
                        .as_ref()
                        .is_some_and(|routing| routing.model.as_deref()
                            == Some("mock:test-model")
                            && routing.workflow.as_ref().is_some_and(|workflow| workflow
                                .task_family
                                .key()
                                == "code:frontend_ui"
                                && workflow.next_step_role.key() == "implement")))),
                "generation facts missing"
            );
        }
    }
    let final_snapshot = wait_status(&server.socket, &execution, TurnStatus::Completed).await?;
    ensure!(final_snapshot.final_answer.as_deref() == Some("Status bar complete."));
    ensure!(
        final_snapshot
            .context_routing
            .as_ref()
            .is_some_and(|inspection| inspection.tasks.iter().any(|task| task
                .routing
                .as_ref()
                .is_some_and(|routing| routing
                    .workflow
                    .as_ref()
                    .is_some_and(|workflow| workflow.task_family.key() == "code:frontend_ui"
                        && workflow.next_step_role.key() == "implement"))))
    );
    server.stop().await
}
