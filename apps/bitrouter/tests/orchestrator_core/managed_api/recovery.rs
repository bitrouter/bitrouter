//! Remote ownership and ACK-loss checks using independently retained journal bytes.

use super::*;
use bitrouter_orchestrator::core::protocol::Restore;
use bitrouter_orchestrator::core::session::SessionSnapshot;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

const GUARD: Duration = Duration::from_secs(60);

struct Peer {
    socket: Socket,
    grant: OwnershipGrant,
    store: Store,
}

impl Peer {
    async fn new(fixture: &Fixture, grant: OwnershipGrant, store: Store) -> Result<Self> {
        Ok(Self {
            socket: socket(fixture).await?,
            grant,
            store,
        })
    }

    fn command(&self, operation: &str, kind: &str, payload: Value) -> Value {
        let mut message = envelope(operation, kind, payload);
        message["execution_epoch"] = json!(self.grant.execution_epoch);
        message["expected_state_revision"] = json!(self.store.head.state_revision);
        message
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<ServerMessage> {
        tokio::time::timeout(GUARD, async {
            loop {
                let frame = self.socket.next().await.context("channel ended")??;
                if matches!(frame, Message::Ping(_) | Message::Pong(_)) {
                    continue;
                }
                return Ok(serde_json::from_str(frame.to_text()?)?);
            }
        })
        .await?
    }

    fn persist(&mut self, batch: CheckpointBatch) -> Result<CheckpointAck> {
        let ack = batch.validate_append(
            &self.grant,
            &self.store.head,
            &Limits::default(),
            &BTreeMap::new(),
            self.store.acknowledgements.get(&batch.identity.batch_id),
        )?;
        if !self
            .store
            .acknowledgements
            .contains_key(&batch.identity.batch_id)
        {
            self.store.head = ack.head();
            self.store
                .acknowledgements
                .insert(batch.identity.batch_id.clone(), ack.clone());
            self.store.batches.push(batch);
        }
        Ok(ack)
    }

    async fn acknowledge(&mut self, batch: CheckpointBatch) -> Result<()> {
        let operation = format!("ack_{}", batch.identity.batch_id);
        let ack = self.persist(batch)?;
        self.send(self.command(&operation, "checkpoint.ack", serde_json::to_value(ack)?))
            .await
    }

    async fn ready(&mut self) -> Result<()> {
        loop {
            match self.receive().await? {
                ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                ServerMessage::Head(head) => {
                    assert_eq!(head, self.store.head);
                    return Ok(());
                }
                other => anyhow::bail!("expected binding checkpoint/head, got {other:?}"),
            }
        }
    }

    async fn receipt(&mut self, operation: &str) -> Result<()> {
        loop {
            match self.receive().await? {
                ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                ServerMessage::Receipt(receipt) => {
                    assert_eq!(receipt.operation_id, operation);
                    assert!(receipt.error.is_none(), "{receipt:?}");
                    return Ok(());
                }
                other => anyhow::bail!("expected operation receipt, got {other:?}"),
            }
        }
    }

    async fn exchange(&mut self, fixture: &Fixture, request: &Value) -> Result<Value> {
        let response = async {
            let response = post(fixture, &fixture.key, request).await?;
            let status = response.status();
            let value: Value = response.json().await?;
            assert_eq!(status, 200, "{value}");
            Ok(value)
        };
        tokio::pin!(response);
        tokio::time::timeout(GUARD, async {
            loop {
                tokio::select! {
                    response = &mut response => return response,
                    message = self.receive() => match message? {
                        ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                        other => anyhow::bail!("unexpected execution message: {other:?}"),
                    },
                }
            }
        })
        .await?
    }

    async fn close(&mut self) -> Result<()> {
        self.socket.close(None).await?;
        tokio::time::timeout(GUARD, async {
            while let Some(frame) = self.socket.next().await {
                use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
                match frame {
                    // The interrupted host aborts its writer while fencing a
                    // pending ACK, so the connection may end without a reply.
                    Ok(Message::Close(_))
                    | Err(Error::ConnectionClosed | Error::AlreadyClosed)
                    | Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => break,
                    Err(error) => return Err(error.into()),
                    Ok(_) => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    fn restoration(&self, epoch: u64) -> Result<Restore> {
        let checkpoint = self.store.batches.last().context("checkpoint")?.clone();
        let state: SessionSnapshot =
            serde_json::from_value(checkpoint.decode(&Limits::default())?.checkpoint.state)?;
        assert!(state.run.as_ref().is_none_or(|run| matches!(
            run.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )));
        let mut grant = self.grant.clone();
        grant.execution_epoch = epoch;
        Ok(Restore {
            binding: Bind {
                grant,
                durable_head: self.store.head.clone(),
                checkpoint: Some(checkpoint),
                manifest: state.manifest,
                limits: Limits::default(),
            },
            journal_tail: Vec::new(),
            tools: Vec::new(),
            results: Vec::new(),
            available_artifacts: Vec::new(),
            previous_owner_stopped: true,
            active_time: None,
        })
    }
}

async fn final_fixture() -> Result<Fixture> {
    fixture_with_output(Some(json!([{
        "id":"final", "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":"finished","annotations":[]}]
    }])))
    .await
}

#[tokio::test]
async fn initial_bind_ack_loss_reconciles_the_exact_proposal() -> Result<()> {
    for persisted in [false, true] {
        let fixture = final_fixture().await?;
        let mut binding = binding(Limits::default())?;
        let mut peer = Peer::new(&fixture, binding.grant.clone(), Store::default()).await?;
        peer.send(peer.command("bind", "session.bind", serde_json::to_value(&binding)?))
            .await?;
        let proposed = peer.receive().await?;
        let ServerMessage::Checkpoint(batch) = proposed else {
            anyhow::bail!("expected first checkpoint, got {proposed:?}");
        };
        assert_eq!(
            batch.decode(&Limits::default())?.events[0].kind,
            "session.bound"
        );
        assert_eq!(
            post(&fixture, &fixture.key, &create("before-ack"))
                .await?
                .status(),
            401
        );
        assert!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .is_empty()
        );
        if persisted {
            peer.persist(batch.clone())?;
        }
        binding.durable_head = peer.store.head.clone();
        peer.close().await?;
        let mut peer = Peer::new(&fixture, binding.grant.clone(), peer.store).await?;
        peer.send(peer.command("reconnect", "session.bind", serde_json::to_value(binding)?))
            .await?;
        peer.ready().await?;
        let copies: Vec<_> = peer
            .store
            .batches
            .iter()
            .filter(|candidate| candidate.identity.batch_id == batch.identity.batch_id)
            .collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            serde_json::to_value(copies[0])?,
            serde_json::to_value(&batch)?
        );
        assert_eq!(
            peer.store
                .batches
                .iter()
                .map(|batch| {
                    Ok::<_, CoreError>(
                        batch
                            .decode(&Limits::default())?
                            .events
                            .into_iter()
                            .filter(|event| event.kind == "session.bound")
                            .count(),
                    )
                })
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .sum::<usize>(),
            1
        );
        let response = peer.exchange(&fixture, &create("after-bind")).await?;
        assert_eq!(
            response["bitrouter"]["run_status"], "completed",
            "{response}"
        );
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .iter()
                .filter(|request| request.url.path() == "/v1/responses")
                .count(),
            1
        );
        peer.close().await?;
        fixture.api.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn release_ack_loss_keeps_its_receipt_without_renewing_ownership() -> Result<()> {
    for persisted in [false, true] {
        let fixture = final_fixture().await?;
        let mut binding = binding(Limits::default())?;
        let mut peer = Peer::new(&fixture, binding.grant.clone(), Store::default()).await?;
        peer.send(peer.command("bind", "session.bind", serde_json::to_value(&binding)?))
            .await?;
        peer.ready().await?;
        peer.send(peer.command("release", "session.release", Value::Null))
            .await?;
        let message = peer.receive().await?;
        let ServerMessage::Checkpoint(batch) = message else {
            anyhow::bail!("expected release proposal, got {message:?}");
        };
        assert_eq!(
            batch.decode(&Limits::default())?.events[0].kind,
            "session.released"
        );
        if persisted {
            peer.persist(batch.clone())?;
        }
        binding.durable_head = peer.store.head.clone();
        peer.close().await?;

        let mut peer = Peer::new(&fixture, binding.grant.clone(), peer.store).await?;
        peer.send(peer.command("reconnect", "session.bind", serde_json::to_value(binding)?))
            .await?;
        peer.ready().await?;
        assert_eq!(peer.store.batches.len(), 2);
        assert_eq!(
            serde_json::to_value(&peer.store.batches[1])?,
            serde_json::to_value(batch)?
        );
        let released = peer.store.head.clone();
        peer.send(peer.command(
            "lookup",
            "operation.get",
            json!({"target_operation_id":"release"}),
        ))
        .await?;
        peer.receipt("release").await?;
        peer.send(peer.command("forbidden", "queue.resume", Value::Null))
            .await?;
        let message = peer.receive().await?;
        assert!(
            matches!(message, ServerMessage::Error(ref error) if error.code == ErrorCode::CheckpointUnavailable),
            "{message:?}"
        );
        let rejected = post(&fixture, &fixture.key, &create("after-release")).await?;
        assert_eq!(rejected.status(), 503);
        assert_eq!(
            rejected.json::<Value>().await?["error"]["code"],
            "checkpoint_unavailable"
        );
        assert_eq!(peer.store.head, released);
        assert!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .is_empty()
        );
        peer.close().await?;
        fixture.api.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn released_epoch_requires_restoration_and_fences_both_transports() -> Result<()> {
    let fixture = final_fixture().await?;
    let binding = binding(Limits::default())?;
    let mut peer = Peer::new(&fixture, binding.grant.clone(), Store::default()).await?;
    peer.send(peer.command("bind", "session.bind", serde_json::to_value(binding)?))
        .await?;
    peer.ready().await?;
    let first = peer.exchange(&fixture, &create("first")).await?;
    assert_eq!(first["bitrouter"]["run_status"], "completed", "{first}");
    peer.send(peer.command("release", "session.release", Value::Null))
        .await?;
    peer.receipt("release").await?;
    let released = peer.store.head.clone();
    let old = peer.restoration(1)?;
    let replacement = peer.restoration(2)?;
    peer.close().await?;

    let mut peer = Peer::new(&fixture, old.binding.grant.clone(), peer.store).await?;
    peer.send(peer.command(
        "stale-restore",
        "session.restore",
        serde_json::to_value(old)?,
    ))
    .await?;
    let message = peer.receive().await?;
    assert!(
        matches!(message, ServerMessage::Error(ref error) if error.code == ErrorCode::StaleEpoch),
        "{message:?}"
    );
    assert_eq!(peer.store.head, released);
    peer.close().await?;

    let mut peer = Peer::new(&fixture, replacement.binding.grant.clone(), peer.store).await?;
    peer.send(peer.command(
        "restore",
        "session.restore",
        serde_json::to_value(replacement)?,
    ))
    .await?;
    peer.ready().await?;
    assert_eq!(peer.store.head.execution_epoch, 2);
    let restored = peer.store.head.clone();
    let stale = post(&fixture, &fixture.key, &create("stale-http")).await?;
    assert_eq!(stale.status(), 409);
    assert_eq!(stale.json::<Value>().await?["error"]["code"], "stale_epoch");
    let mut stale = peer.command("stale-resume", "queue.resume", Value::Null);
    stale["execution_epoch"] = json!(1);
    peer.send(stale).await?;
    let message = peer.receive().await?;
    assert!(
        matches!(message, ServerMessage::Error(ref error) if error.code == ErrorCode::StaleEpoch),
        "{message:?}"
    );
    peer.send(peer.command("head", "session.head", json!({"durable_head":restored})))
        .await?;
    peer.ready().await?;
    assert_eq!(peer.store.head, restored);

    let mut replay = create("first");
    replay["bitrouter"]["execution_epoch"] = json!(2);
    assert_eq!(peer.exchange(&fixture, &replay).await?, first);
    assert_eq!(peer.store.head, restored);
    peer.send(peer.command("resume", "queue.resume", Value::Null))
        .await?;
    peer.receipt("resume").await?;
    let mut next = create("second");
    next["bitrouter"]["execution_epoch"] = json!(2);
    let second = peer.exchange(&fixture, &next).await?;
    assert_eq!(second["bitrouter"]["run_status"], "completed", "{second}");
    assert_ne!(second["bitrouter"]["run_id"], first["bitrouter"]["run_id"]);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        2
    );
    peer.close().await?;
    fixture.api.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn head_query_preserves_a_held_provider_attempt() -> Result<()> {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream = axum::Router::new()
        .route("/v1/responses/input_tokens", axum::routing::post(|| async {
            axum::Json(json!({"object":"response.input_tokens","input_tokens":20}))
        }))
        .route("/v1/responses", axum::routing::post({
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.add_permits(1);
                    let _hold = release.acquire().await;
                    axum::Json(json!({
                        "id":"held", "object":"response", "status":"completed", "model":"served",
                        "output":[{"id":"final","type":"message","role":"assistant","status":"completed",
                            "content":[{"type":"output_text","text":"released","annotations":[]}]}],
                        "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}
                    }))
                }
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, upstream).await });
    let fixture = configured_fixture_with_provider(None, None, Some(&provider)).await?;
    let (send, _tools, store, task) = harness(&fixture).await?;
    let request = create("held-provider");
    let response = post(&fixture, &fixture.key, &request);
    tokio::pin!(response);
    // Poll HTTP while the upstream is held by an explicit permit, not a timed delay.
    let entered = entered.acquire();
    tokio::pin!(entered);
    tokio::time::timeout(GUARD, async {
        tokio::select! {
            response = &mut response => anyhow::bail!("provider unexpectedly finished: {:?}", response?.status()),
            permit = &mut entered => { permit?.forget(); Ok(()) }
        }
    }).await??;
    let before = store.lock().await.head.clone();
    send.send(envelope(
        "read-head",
        "session.head",
        json!({"durable_head":before}),
    ))
    .await?;
    tokio::time::timeout(GUARD, async {
        loop {
            if store.lock().await.heads_received == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(store.lock().await.head, before);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    let response = tokio::time::timeout(GUARD, response).await??;
    let status = response.status();
    let response: Value = response.json().await?;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        response["bitrouter"]["run_status"], "completed",
        "{response}"
    );
    assert_eq!(response["output"][0]["content"][0]["text"], "released");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(GUARD, task).await???;
    server.shutdown().await;
    Ok(())
}
