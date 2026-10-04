//! Real process death while an independent provider withholds its HTTP body end.
//! Activity reconciliation below is scripted trusted input, not clock evidence.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct InflightProvider {
    endpoint: String,
    sent: oneshot::Receiver<Value>,
    stopped: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for InflightProvider {
    fn drop(&mut self) {
        self.stopped.abort();
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Result<(String, Value)> {
    let mut received = Vec::new();
    let mut part = [0; 4096];
    loop {
        let count = socket.read(&mut part).await?;
        anyhow::ensure!(count != 0, "provider request ended early");
        received.extend_from_slice(&part[..count]);
        anyhow::ensure!(received.len() <= 1024 * 1024, "fixture request too large");
        if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&received[..end])?;
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .context("content length")?
                .parse::<usize>()?;
            anyhow::ensure!(length <= 1024 * 1024, "fixture body too large");
            if received.len() >= end + 4 + length {
                let target = headers
                    .lines()
                    .next()
                    .context("request line")?
                    .split_whitespace()
                    .nth(1)
                    .context("request target")?
                    .to_owned();
                return Ok((
                    target,
                    serde_json::from_slice(&received[end + 4..end + 4 + length])?,
                ));
            }
        }
    }
}

impl InflightProvider {
    async fn start(complete_json: bool) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (sent, receive) = oneshot::channel();
        let stopped = tokio::spawn(async move {
            tokio::time::timeout(GUARD, async {
                let (mut socket, _) = listener.accept().await?;
                let (target, _) = read_request(&mut socket).await?;
                anyhow::ensure!(target == "/v1/responses/input_tokens", "{target}");
                let count = r#"{"object":"response.input_tokens","input_tokens":20}"#;
                // HTTP/1.1 framing and incomplete entities:
                // https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{count}",
                    count.len()
                );
                socket.write_all(wire.as_bytes()).await?;
                socket.shutdown().await?;
                drop(socket);

                let (mut socket, _) = listener.accept().await?;
                let (target, request) = read_request(&mut socket).await?;
                anyhow::ensure!(target == "/v1/responses", "{target}");
                let full = serde_json::to_vec(&json!({
                    "id":"interrupted-response", "object":"response", "status":"completed",
                    "model":"served", "output":[{
                        "id":"partial-item", "type":"function_call", "call_id":"partial-call",
                        "name":"write", "arguments":"{\"path\":\"partial.txt\"}", "status":"completed"
                    }],
                    "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}
                }))?;
                let length = if complete_json {
                    full.len()
                } else {
                    full.windows(b"partial.txt".len())
                        .position(|part| part == b"partial.txt")
                        .context("partial argument")? + 4
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    full.len() + 16
                );
                socket.write_all(header.as_bytes()).await?;
                socket.write_all(&full[..length]).await?;
                socket.flush().await?;
                sent.send(request).map_err(|_| anyhow::anyhow!("lost provider observer"))?;
                // Keep the entity incomplete until the killed CLI closes its
                // transport. Neither a test-side disconnect nor a timer ends it.
                match socket.read(&mut [0; 1]).await {
                    Ok(0) => Ok(()),
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted) => Ok(()),
                    other => anyhow::bail!("expected closed provider connection: {other:?}"),
                }
            }).await?
        });
        Ok(Self {
            endpoint,
            sent: receive,
            stopped,
        })
    }
}

fn restoration(peer: &Peer) -> Result<Restore> {
    let checkpoint = peer.store.batches.last().context("checkpoint")?.clone();
    let state = snapshot(&checkpoint)?;
    Ok(Restore {
        binding: Bind {
            grant: peer.grant.clone(),
            durable_head: peer.store.head.clone(),
            checkpoint: Some(checkpoint),
            manifest: state.manifest,
            limits: Limits::default(),
        },
        journal_tail: Vec::new(),
        tools: Vec::new(),
        results: Vec::new(),
        available_artifacts: Vec::new(),
        previous_owner_stopped: false,
        active_time: None,
    })
}

async fn inflight_crash(complete_json: bool) -> Result<()> {
    let env = Environment::new().await?;
    let mut provider = InflightProvider::start(complete_json).await?;
    let mut original = env.start_with_provider(&provider.endpoint).await?;
    let mut peer = env.peer(&original, 1, Store::default()).await?;
    let bind = process_binding(peer.grant.clone())?;
    peer.send(peer.command("bind", "session.bind", serde_json::to_value(bind)?))
        .await?;
    peer.ready().await?;
    let response = reqwest::Client::builder()
        .timeout(GUARD)
        .build()?
        .post(format!("{}/v1/responses", original.base))
        .bearer_auth(&env.key)
        .header("bitrouter-beta", "orchestrator_core=v1")
        .json(&create("task"))
        .send();
    // The managed endpoint can send headers before finishing its JSON body.
    // Only a complete response entity would constitute premature completion.
    let response = async move { response.await?.json::<Value>().await };
    tokio::pin!(response);
    let request = tokio::time::timeout(GUARD, async {
        loop {
            tokio::select! {
                value = &mut response => anyhow::bail!("incomplete entity completed HTTP: {value:?}"),
                request = &mut provider.sent => return Ok(request?),
                message = peer.receive() => match message? {
                    ServerMessage::Checkpoint(batch) => peer.acknowledge(batch).await?,
                    other => anyhow::bail!("incomplete entity produced output: {other:?}"),
                }
            }
        }
    }).await??;
    assert_eq!(request["model"], "served");
    assert_ne!(request["stream"], true);
    assert!(
        !provider.stopped.is_finished(),
        "provider stopped before process kill"
    );
    let retained = serde_json::to_vec(&peer.store.batches)?;
    tokio::fs::write(env.home.path().join("harness-journal.json"), &retained).await?;
    original.crash().await?;
    tokio::time::timeout(GUARD, &mut provider.stopped).await???;
    assert!(
        response.await.is_err(),
        "killed HTTP exchange unexpectedly completed"
    );
    drop(peer);

    let mut host = env.start().await?;
    assert_ne!(host.instance, original.instance);
    let store = read_journal(&env, &retained).await?;
    let before = snapshot(store.batches.last().context("checkpoint")?)?;
    let run = before.run.as_ref().context("run")?;
    let turn = before.root_turn().context("turn")?;
    assert!(turn.invocations.is_empty() && turn.core_calls.is_empty());
    assert_eq!(turn.steps.len(), 1);
    let step = &turn.steps[0];
    assert!(!step.settled && !step.interrupted);
    assert_eq!(step.attempts.len(), 1);
    let attempt = &step.attempts[0];
    assert!(attempt.receipt.is_none());
    let exposure = before.cost_work[&run.run_id]
        .work
        .get(&attempt.attempt_id)
        .context("attempt cost")?;
    assert_eq!(exposure.kind, CostWorkKind::ProviderAttempt);
    assert_eq!(exposure.state, CostWorkState::IntentRecorded);
    assert!(exposure.token_estimate.is_none() && exposure.elapsed_ms.is_none());
    assert!(!exposure.unknown_cost_reason.is_empty());
    assert!(exposure.provider_source.is_some() && exposure.request_id.is_some());
    assert_eq!(run.model_attempts, 1);
    assert!(!env.home.path().join("file.txt").exists());

    for stopped in [false, true] {
        let mut rejected = env
            .peer(&host, 2, read_journal(&env, &retained).await?)
            .await?;
        let mut restore = restoration(&rejected)?;
        restore.previous_owner_stopped = stopped;
        rejected
            .send(rejected.command("restore", "session.restore", serde_json::to_value(restore)?))
            .await?;
        let message = rejected.receive().await?;
        let ServerMessage::Error(error) = message else {
            anyhow::bail!("unsafe restoration admitted: {message:?}");
        };
        assert_eq!(error.code, ErrorCode::RecoveryRequired);
        assert_eq!(
            error.commit_status,
            bitrouter_orchestrator::core::protocol::CommitStatus::NotCommitted
        );
        assert!(
            error
                .message
                .contains(if stopped { "activity" } else { "provider I/O" }),
            "{error}"
        );
        assert_eq!(rejected.store.head, store.head);
        rejected.close().await?;
        let mut probe = create("unbound");
        probe["bitrouter"]["execution_epoch"] = json!(2);
        let unbound = raw_post(&host, &env.key, &probe).await?;
        assert_eq!(unbound.status(), 401);
        assert_eq!(
            unbound.json::<Value>().await?["error"]["code"],
            "unauthorized_scope"
        );
        assert!(
            env.upstream
                .received_requests()
                .await
                .context("upstream")?
                .is_empty()
        );
    }

    let mut peer = env.peer(&host, 2, store).await?;
    let mut restore = restoration(&peer)?;
    restore.previous_owner_stopped = true;
    // This is a scripted harness attestation used to exercise the trusted-input
    // protocol. It is NOT measured activity and proves no production clock
    // handoff, remote Running restoration, or cross-machine timing guarantee.
    // Neither transport arrival time nor restart downtime supplies this value.
    restore.active_time = Some(RunActivityReconciliation {
        run_id: run.run_id.clone(),
        durable_head: peer.store.head.clone(),
        active_ms: run
            .active_ms
            .checked_add(1000)
            .context("scripted activity")?,
    });
    peer.send(peer.command("restore", "session.restore", serde_json::to_value(restore)?))
        .await?;
    let message = peer.receive().await?;
    let ServerMessage::Checkpoint(proposal) = message else {
        anyhow::bail!("expected restoration checkpoint: {message:?}");
    };
    let restored = snapshot(&proposal)?;
    let interrupted = restored
        .root_turn()
        .context("restored turn")?
        .steps
        .first()
        .context("old step")?;
    assert_eq!(interrupted.step_id, step.step_id);
    assert!(interrupted.interrupted && interrupted.settled);
    assert!(interrupted.attempts[0].receipt.is_none());
    assert_eq!(
        &restored.cost_work[&run.run_id].work[&attempt.attempt_id],
        exposure
    );
    assert!(
        env.upstream
            .received_requests()
            .await
            .context("upstream")?
            .is_empty()
    );
    peer.acknowledge(proposal).await?;
    peer.ready().await?;

    let mut replay = create("task");
    replay["bitrouter"]["execution_epoch"] = json!(2);
    let (first, commands) = exchange(&mut peer, &host, &env.key, &replay).await?;
    let command = command(&mut peer, commands).await?;
    assert_eq!(command.execution_epoch, 2);
    assert_eq!(command.arguments, json!({"path":"file.txt"}));
    let resumed = snapshot(peer.store.batches.last().context("checkpoint")?)?;
    let turn = resumed.root_turn().context("resumed turn")?;
    assert_eq!(turn.steps.len(), 2);
    assert_ne!(turn.steps[1].step_id, step.step_id);
    assert_ne!(
        turn.steps[1].plan.as_ref().context("new plan")?.request_id,
        step.plan.as_ref().context("old plan")?.request_id
    );
    assert_ne!(turn.steps[1].attempts[0].attempt_id, attempt.attempt_id);
    assert_eq!(turn.invocations.len(), 1);
    assert_eq!(turn.invocations[0].provider_call_id, "provider-call");
    assert!(turn.core_calls.is_empty());
    let result = execute(&env, &mut peer, &command).await?;
    let request = continuation(&first, &result, 2)?;
    let (done, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert!(commands.is_empty());
    assert_eq!(done["bitrouter"]["run_status"], "completed");
    assert_eq!(done["bitrouter"]["run_id"], run.run_id);
    assert_eq!(
        tokio::fs::read(env.home.path().join("file.txt")).await?,
        b"effect\n"
    );
    let finished = snapshot(peer.store.batches.last().context("checkpoint")?)?;
    assert_eq!(
        finished
            .run
            .as_ref()
            .context("finished run")?
            .model_attempts,
        3
    );
    let costs = &finished.cost_work[&run.run_id].work;
    assert_eq!(&costs[&attempt.attempt_id], exposure);
    assert_eq!(
        costs
            .values()
            .filter(|work| work.kind == CostWorkKind::ProviderAttempt
                && work.state == CostWorkState::OutcomeRecorded)
            .count(),
        2
    );
    let stale = raw_post(&host, &env.key, &continuation(&first, &result, 1)?).await?;
    assert_eq!(stale.status(), 409);
    assert_eq!(stale.json::<Value>().await?["error"]["code"], "stale_epoch");
    let head = peer.store.head.clone();
    let (repeated, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert_eq!(repeated, done);
    assert!(commands.is_empty());
    assert_eq!(peer.store.head, head);
    assert_eq!(
        env.upstream
            .received_requests()
            .await
            .context("upstream")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        2
    );
    peer.send(peer.command("release", "session.release", Value::Null))
        .await?;
    peer.receipt("release").await?;
    host.crash().await?;
    Ok(())
}

#[tokio::test]
async fn process_crash_during_partial_tool_json_retains_unknown_attempt() -> Result<()> {
    inflight_crash(false).await
}

#[tokio::test]
async fn process_crash_after_json_before_http_body_end_retains_unknown_attempt() -> Result<()> {
    inflight_crash(true).await
}
