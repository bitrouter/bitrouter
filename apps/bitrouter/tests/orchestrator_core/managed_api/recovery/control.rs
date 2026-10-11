//! Independent control clients retain exact journal bytes across socket loss.

use super::*;
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::protocol::OperationReceipt;
use bitrouter_orchestrator::core::session::steering::SteeringDisposition;
use std::collections::BTreeSet;

struct ControlPeer {
    peer: Peer,
    tools: Vec<ToolExecute>,
    cancelled: BTreeSet<ToolStartFence>,
    fences: BTreeSet<ToolStartFence>,
}

impl ControlPeer {
    async fn bind(fixture: &Fixture) -> Result<Self> {
        let binding = binding(Limits::default())?;
        let mut peer = Peer::new(fixture, binding.grant.clone(), Store::default()).await?;
        peer.send(peer.command("bind", "session.bind", serde_json::to_value(binding)?))
            .await?;
        peer.ready().await?;
        Ok(Self {
            peer,
            tools: Vec::new(),
            cancelled: BTreeSet::new(),
            fences: BTreeSet::new(),
        })
    }

    fn state(&self) -> Result<SessionSnapshot> {
        Ok(serde_json::from_value(
            self.peer
                .store
                .batches
                .last()
                .context("checkpoint")?
                .decode(&Limits::default())?
                .checkpoint
                .state,
        )?)
    }

    fn persist(&mut self, batch: CheckpointBatch) -> Result<CheckpointAck> {
        let fences = batch.decode(&Limits::default())?.tool_start_fences;
        let ack = self.peer.persist(batch)?;
        // This single task retains journal and fence records before ACK.
        // Actual workspace execution belongs to the production harness.
        self.fences.extend(fences);
        Ok(ack)
    }

    async fn acknowledge(&mut self, batch: CheckpointBatch) -> Result<()> {
        let operation = format!("ack_{}", batch.identity.batch_id);
        let ack = self.persist(batch)?;
        self.peer
            .send(
                self.peer
                    .command(&operation, "checkpoint.ack", serde_json::to_value(ack)?),
            )
            .await
    }

    async fn receive(&mut self) -> Result<ServerMessage> {
        loop {
            let message = self.peer.receive().await?;
            if let Some(message) = self.observe(message) {
                return Ok(message);
            }
        }
    }

    fn observe(&mut self, message: ServerMessage) -> Option<ServerMessage> {
        match message {
            ServerMessage::ToolExecute(tool) => self.tools.push(*tool),
            ServerMessage::ToolCancel {
                invocation_id,
                attempt_id,
                execution_epoch,
            } => {
                assert_eq!(execution_epoch, self.peer.grant.execution_epoch);
                self.cancelled.insert(ToolStartFence {
                    invocation_id,
                    attempt_id,
                });
            }
            other => return Some(other),
        }
        None
    }

    async fn close(&mut self) -> Result<()> {
        use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
        self.peer.socket.close(None).await?;
        tokio::time::timeout(GUARD, async {
            while let Some(frame) = self.peer.socket.next().await {
                match frame {
                    Ok(Message::Text(text)) => {
                        // Observe buffered effects too: closing must not hide a
                        // duplicate delivery from the final invocation count.
                        self.observe(serde_json::from_str(&text)?);
                    }
                    Ok(Message::Close(_))
                    | Err(Error::ConnectionClosed | Error::AlreadyClosed)
                    | Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => break,
                    Err(Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error.into()),
                    Ok(_) => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn receipt(&mut self, operation: &str) -> Result<OperationReceipt> {
        loop {
            match self.receive().await? {
                ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                ServerMessage::Receipt(receipt) => {
                    assert_eq!(receipt.operation_id, operation);
                    assert!(receipt.error.is_none(), "{receipt:?}");
                    return Ok(receipt);
                }
                other => anyhow::bail!("expected receipt for {operation}, got {other:?}"),
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
                        other => anyhow::bail!("unexpected exchange message: {other:?}"),
                    }
                }
            }
        })
        .await?
    }

    async fn lose_ack(
        &mut self,
        fixture: &Fixture,
        command: Value,
        persisted: bool,
    ) -> Result<OperationReceipt> {
        let operation = command["operation_id"].as_str().context("operation")?;
        self.peer.send(command.clone()).await?;
        let message = self.receive().await?;
        let ServerMessage::Checkpoint(batch) = message else {
            anyhow::bail!("expected {operation} proposal, got {message:?}");
        };
        let state: SessionSnapshot =
            serde_json::from_value(batch.decode(&Limits::default())?.checkpoint.state)?;
        let expected = state
            .operations
            .get(operation)
            .context("proposed receipt")?;
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .iter()
                .filter(|request| request.url.path() == "/v1/responses")
                .count(),
            1,
            "dependent work must not run before {operation} is acknowledged"
        );
        if persisted {
            self.persist(batch.clone())?;
        }
        self.close().await?;
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("upstream")?
                .iter()
                .filter(|request| request.url.path() == "/v1/responses")
                .count(),
            1,
            "disconnect must not start dependent work for {operation}"
        );
        let mut binding = binding(Limits::default())?;
        binding.durable_head = self.peer.store.head.clone();
        self.peer = Peer::new(
            fixture,
            self.peer.grant.clone(),
            std::mem::take(&mut self.peer.store),
        )
        .await?;
        self.peer
            .send(
                self.peer
                    .command("reconnect", "session.bind", serde_json::to_value(binding)?),
            )
            .await?;
        loop {
            match self.receive().await? {
                ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                ServerMessage::Head(head) => {
                    assert_eq!(head, self.peer.store.head);
                    break;
                }
                other => anyhow::bail!("unexpected reconnect message: {other:?}"),
            }
        }
        let copies = self
            .peer
            .store
            .batches
            .iter()
            .filter(|candidate| candidate.identity.batch_id == batch.identity.batch_id)
            .collect::<Vec<_>>();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            serde_json::to_value(copies[0])?,
            serde_json::to_value(&batch)?
        );
        // Preserve the ORIGINAL expected revision when retrying the same ID.
        self.peer.send(command.clone()).await?;
        let replay = self.receipt(operation).await?;
        assert_eq!(
            serde_json::to_value(&replay)?,
            serde_json::to_value(expected)?
        );
        self.peer
            .send(self.peer.command(
                "lookup",
                "operation.get",
                json!({"target_operation_id":operation}),
            ))
            .await?;
        assert_eq!(
            serde_json::to_value(self.receipt(operation).await?)?,
            serde_json::to_value(expected)?
        );
        let mut conflict = command.clone();
        conflict["expected_state_revision"] = json!(
            command["expected_state_revision"]
                .as_u64()
                .context("revision")?
                + 1
        );
        self.peer.send(conflict).await?;
        loop {
            match self.receive().await? {
                ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                ServerMessage::Error(error) => {
                    assert_eq!(error.code, ErrorCode::OperationConflict);
                    break;
                }
                other => anyhow::bail!("expected operation conflict, got {other:?}"),
            }
        }
        assert_eq!(
            serde_json::to_value(
                self.state()?
                    .operations
                    .get(operation)
                    .context("retained operation")?
            )?,
            serde_json::to_value(expected)?
        );
        Ok(replay)
    }

    async fn wait_run(&mut self, run_id: &str, status: RunStatus) -> Result<()> {
        tokio::time::timeout(GUARD, async {
            loop {
                if self
                    .state()?
                    .run
                    .as_ref()
                    .is_some_and(|run| run.run_id == run_id && run.status == status)
                {
                    return Ok(());
                }
                match self.receive().await? {
                    ServerMessage::Checkpoint(batch) => self.acknowledge(batch).await?,
                    other => anyhow::bail!("unexpected progress message: {other:?}"),
                }
            }
        })
        .await?
    }
}

fn queued(text: &str) -> Value {
    json!({"text":text,"model":"fixture-model","max_output_tokens":128,
        "acceptance_criteria":[],"required_materials":[]})
}

#[tokio::test]
async fn queue_steer_cancel_reconnect_preserves_receipts_and_paused_work() -> Result<()> {
    for persistence in [
        [false; 6],
        [true; 6],
        [false, true, false, true, false, true],
        [true, false, true, false, true, false],
    ] {
        let fixture = fixture().await?;
        let mut client = ControlPeer::bind(&fixture).await?;
        let request = create("root");
        let response = client.exchange(&fixture, &request).await?;
        // The completed HTTP response precedes tool delivery. Read the socket
        // explicitly so later steering concerns an already delivered intent.
        while client.tools.is_empty() {
            match client.peer.receive().await? {
                ServerMessage::ToolExecute(tool) => client.tools.push(*tool),
                ServerMessage::Checkpoint(batch) => client.acknowledge(batch).await?,
                other => anyhow::bail!("expected tool, got {other:?}"),
            }
        }
        let tool = client.tools[0].clone();
        let identity = ToolStartFence {
            invocation_id: tool.invocation_id.clone(),
            attempt_id: tool.attempt_id.clone(),
        };
        client
            .peer
            .send(client.peer.command(
                "approval",
                "tool.status",
                json!({
                    "invocation_id":tool.invocation_id,"attempt_id":tool.attempt_id,
                    "status":"waiting_approval","evidence":[]
                }),
            ))
            .await?;
        client.receipt("approval").await?;

        let first = client
            .lose_ack(
                &fixture,
                client
                    .peer
                    .command("first", "input.enqueue", queued("first queued task")),
                persistence[0],
            )
            .await?;
        let second = client
            .lose_ack(
                &fixture,
                client
                    .peer
                    .command("second", "input.enqueue", queued("second queued task")),
                persistence[1],
            )
            .await?;
        let first_id = first.assigned_ids.get("run_id").context("first run")?;
        let second_id = second.assigned_ids.get("run_id").context("second run")?;
        let state = client.state()?;
        assert_eq!(
            state
                .root_queue
                .pending
                .iter()
                .map(|entry| &entry.run_id)
                .collect::<Vec<_>>(),
            [first_id, second_id]
        );
        assert!(!state.cost_work.contains_key(first_id));
        assert!(!state.cost_work.contains_key(second_id));

        client
            .lose_ack(
                &fixture,
                client
                    .peer
                    .command("cancel-queued", "run.cancel", json!({"run_id":second_id})),
                persistence[2],
            )
            .await?;
        client.lose_ack(&fixture, client.peer.command("steer", "input.steer", json!({
            "run_id":tool.run_id,"agent_turn_id":tool.agent_turn_id,"text":"do not read that file"
        })), persistence[3]).await?;
        assert!(client.fences.contains(&identity));
        assert_eq!(
            client
                .state()?
                .steering
                .get("steer")
                .context("steer")?
                .disposition,
            SteeringDisposition::Received
        );
        client
            .lose_ack(
                &fixture,
                client
                    .peer
                    .command("cancel-root", "run.cancel", json!({"run_id":tool.run_id})),
                persistence[4],
            )
            .await?;
        let state = client.state()?;
        assert!(state.root_queue.paused);
        assert_eq!(state.root_queue.pending.len(), 1);
        assert_eq!(&state.root_queue.pending[0].run_id, first_id);
        assert_eq!(
            state.run.as_ref().context("active run")?.run_id,
            tool.run_id
        );
        assert_eq!(client.tools.len(), 1);
        assert_eq!(client.exchange(&fixture, &request).await?, response);

        let head = client.peer.store.head.clone();
        client
            .peer
            .send(
                client
                    .peer
                    .command("premature-resume", "queue.resume", Value::Null),
            )
            .await?;
        let message = client.receive().await?;
        assert!(
            matches!(message, ServerMessage::Error(ref error) if error.code == ErrorCode::Busy),
            "{message:?}"
        );
        assert_eq!(client.peer.store.head, head);
        // The retained fence tells a harness to refuse delayed approval.
        // This client checks its presence; it executes no workspace effects.
        assert!(client.fences.contains(&identity));
        client
            .peer
            .send(client.peer.command(
                "not-executed",
                "tool.result",
                json!({
                    "invocation_id":tool.invocation_id,"attempt_id":tool.attempt_id,
                    "status":"not_executed","output":"","evidence":[]
                }),
            ))
            .await?;
        client.receipt("not-executed").await?;
        client.wait_run(&tool.run_id, RunStatus::Cancelled).await?;
        assert!(client.state()?.root_queue.paused);
        assert_eq!(
            client
                .state()?
                .steering
                .get("steer")
                .context("steer")?
                .disposition,
            SteeringDisposition::Cancelled
        );

        client
            .lose_ack(
                &fixture,
                client.peer.command("resume", "queue.resume", Value::Null),
                persistence[5],
            )
            .await?;
        client.wait_run(first_id, RunStatus::Completed).await?;
        let state = client.state()?;
        assert!(state.root_queue.pending.is_empty());
        assert!(!state.root_queue.paused);
        assert!(!state.cost_work.contains_key(second_id));
        assert!(state.cost_work.contains_key(first_id));
        assert_eq!(
            serde_json::to_value(state.operations.get("first").context("first receipt")?)?,
            serde_json::to_value(&first)?
        );
        assert_eq!(
            serde_json::to_value(state.operations.get("second").context("second receipt")?)?,
            serde_json::to_value(&second)?
        );
        assert_eq!(
            &state
                .run
                .as_ref()
                .context("completed queued run")?
                .agent_turn_id,
            first
                .assigned_ids
                .get("agent_turn_id")
                .context("reserved queued turn")?
        );
        assert_eq!(client.tools.len(), 1);
        assert!(client.cancelled.contains(&identity));
        let requests = fixture
            .upstream
            .received_requests()
            .await
            .context("upstream")?;
        let model_requests = requests
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .collect::<Vec<_>>();
        assert_eq!(model_requests.len(), 2);
        let final_input = String::from_utf8_lossy(&model_requests[1].body);
        assert!(final_input.contains("first queued task"));
        assert!(!final_input.contains("second queued task"));
        assert!(!final_input.contains("do not read that file"));
        client
            .peer
            .send(
                client
                    .peer
                    .command("release", "session.release", Value::Null),
            )
            .await?;
        client.receipt("release").await?;
        client.close().await?;
        assert_eq!(client.tools.len(), 1);
        fixture.api.shutdown().await;
    }
    Ok(())
}
