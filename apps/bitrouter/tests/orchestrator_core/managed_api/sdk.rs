use super::*;
use bitrouter_ai::core::protocol::{ClientMessage, CommitStatus, ErrorCode};
use bitrouter_sdk::core::ChannelOptions;
use bitrouter_sdk::error::Error;
use bitrouter_sdk::gateway::GatewayClient;

#[tokio::test]
async fn sdk_managed_responses_preserve_json_sse_and_harness_result_authority() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), managed_responses()).await?
}

async fn managed_responses() -> Result<()> {
    use bitrouter_ai::core::responses::{CreateRequest, FunctionOutput, ResultMetadata};
    let fixture = fixture().await?;
    let client = GatewayClient::new(&fixture.base, Some(&fixture.key))?;
    let mut request: CreateRequest = serde_json::from_value(create("sdk-first"))?;
    assert!(matches!(client.managed_responses(&request).await,
        Err(Error::Core(ref failure)) if failure.status == 401));
    let (send, mut tools, store, task) = harness(&fixture).await?;
    let denied = GatewayClient::new(&fixture.base, Some(&fixture.other_key))?;
    assert!(matches!(denied.managed_responses(&request).await,
        Err(Error::Core(ref failure)) if failure.status == 401));
    let raw = post(&fixture, &fixture.key, &create("sdk-first")).await?;
    assert_eq!(raw.status(), 200);
    let first: Value = raw.json().await?;
    assert_eq!(
        serde_json::to_value(client.managed_responses(&request).await?)?,
        first
    );
    assert_eq!(first["status"], "completed");
    assert_eq!(first["bitrouter"]["run_status"], "waiting");
    let command = tools.recv().await.context("pending harness command")?;
    assert_eq!(command.tool, "read");
    let call = first["output"]
        .as_array()
        .context("output")?
        .iter()
        .find(|item| item["type"] == "function_call")
        .context("function call")?;
    let mut conflict = request.clone();
    conflict.input = json!("different task with the same operation ID");
    let Err(Error::Core(failure)) = client.managed_responses(&conflict).await else {
        anyhow::bail!("expected an operation fingerprint conflict");
    };
    let error = failure.body.context("conflict error body")?.error;
    assert_eq!(error.code, ErrorCode::OperationConflict);
    assert_eq!(error.commit_status, CommitStatus::NotCommitted);

    // Reobserving the committed exchange uses its original operation identity.
    // The separate harness owns all checkpoint ACKs; HTTP consumption does not.
    request.stream = true;
    let mut stream = client.stream_managed_responses(&request).await?;
    let mut sequence = 0;
    let mut retained_events = 0;
    let mut repeated = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        assert_eq!(event.data.sequence_number, Some(Some(sequence)));
        sequence += 1;
        if event.event.starts_with("bitrouter.") {
            assert!(event.data.extra.contains_key("event"));
            retained_events += 1;
        }
        if event.event == "response.completed" {
            repeated = event.data.response.flatten();
        }
    }
    assert_eq!(
        retained_events,
        first["bitrouter"]["events"]
            .as_array()
            .context("retained events")?
            .len()
    );
    assert_eq!(
        serde_json::to_value(repeated.context("terminal response")?)?,
        first
    );
    assert!(store.lock().await.batches.iter().all(|batch| {
        batch.decode(&Limits::default()).is_ok_and(|payload| {
            payload
                .events
                .iter()
                .all(|event| event.kind != "tool.result")
        })
    }));

    let mut next: CreateRequest = serde_json::from_value(create("sdk-continue"))?;
    next.stream = true;
    next.previous_response_id = Some(first["id"].as_str().context("response ID")?.into());
    next.input = serde_json::to_value([FunctionOutput {
        kind: "function_call_output".into(),
        call_id: call["call_id"].as_str().context("call ID")?.into(),
        output: "file body".into(),
        bitrouter: ResultMetadata {
            operation_id: "sdk-result".into(),
            status: ToolOutcome::Succeeded,
            evidence: Vec::new(),
            workspace_revision: None,
        },
    }])?;
    let mut invalid = next.clone();
    invalid.stream = false;
    invalid.bitrouter.operation_id = "sdk-invalid-continuation".into();
    invalid.input[0]["call_id"] = json!("not-an-issued-call");
    let Err(Error::Core(failure)) = client.managed_responses(&invalid).await else {
        anyhow::bail!("expected an invalid continuation result");
    };
    let error = failure.body.context("invalid result error body")?.error;
    assert_eq!(error.code, ErrorCode::InvalidToolResult);
    assert_eq!(error.commit_status, CommitStatus::NotCommitted);
    assert!(store.lock().await.batches.iter().all(|batch| {
        batch.decode(&Limits::default()).is_ok_and(|payload| {
            payload
                .events
                .iter()
                .all(|event| event.kind != "tool.result")
        })
    }));
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("provider requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    let mut stream = client.stream_managed_responses(&next).await?;
    let mut done = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if event.event == "response.completed" {
            done = event.data.response.flatten();
        }
    }
    let done = serde_json::to_value(done.context("completed continuation")?)?;
    assert_eq!(done["bitrouter"]["run_status"], "completed");
    assert!(
        done["output"]
            .as_array()
            .context("output")?
            .iter()
            .any(|item| item["phase"] == "final_answer"
                && item["content"][0]["text"] == "verified response")
    );
    let records = store.lock().await;
    let results = records
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|payload| payload.events)
        .filter(|event| event.kind == "tool.result")
        .count();
    assert_eq!(results, 1);
    drop(records);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("provider requests")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        2
    );
    fixture.api.shutdown().await;
    drop(send);
    task.await??;
    Ok(())
}

#[tokio::test]
async fn sdk_core_channel_uses_real_authority_and_explicit_checkpoint_ack() -> Result<()> {
    let fixture = fixture().await?;
    let anonymous = GatewayClient::new(&fixture.base, None)?;
    let denied = anonymous.core_capabilities().await;
    assert!(matches!(denied, Err(Error::Core(ref failure)) if failure.status == 401));
    let denied = anonymous.core_channel(ChannelOptions::default()).await;
    assert!(matches!(denied, Err(Error::Core(ref failure)) if failure.status == 401));

    let client = GatewayClient::new(&fixture.base, Some(&fixture.key))?;
    let capabilities = client.core_capabilities().await?;
    assert_eq!(capabilities.version, 1);
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    let binding = binding(Limits::default())?;
    let bind: ClientMessage = serde_json::from_value(envelope(
        "bind",
        "session.bind",
        serde_json::to_value(&binding)?,
    ))?;
    channel.send(&bind).await.context("send bind")?;
    let proposal = tokio::time::timeout(std::time::Duration::from_secs(5), channel.receive())
        .await??
        .context("missing initial checkpoint")?;
    let ServerMessage::Checkpoint(batch) = proposal else {
        anyhow::bail!("expected a proposed checkpoint");
    };
    // The SDK delivers bytes; it does not acknowledge a proposal or invent a
    // durable head on the harness's behalf.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), channel.receive())
            .await
            .is_err()
    );
    let ack = batch.validate_append(
        &binding.grant,
        &binding.durable_head,
        &binding.limits,
        &BTreeMap::new(),
        None,
    )?;
    let harness_home = tempfile::tempdir()?;
    let journal = harness_home.path().join("sdk-harness-checkpoint.json");
    let mut file = std::fs::File::create(&journal)?;
    serde_json::to_writer(&mut file, &batch)?;
    file.sync_all()?;
    let retained: bitrouter_ai::core::checkpoint::CheckpointBatch =
        serde_json::from_reader(std::fs::File::open(&journal)?)?;
    assert_eq!(retained, batch);
    let ack_message = serde_json::from_value(envelope(
        "ack_bind",
        "checkpoint.ack",
        serde_json::to_value(&ack)?,
    ))?;
    channel
        .send(&ack_message)
        .await
        .context("send explicit ACK")?;
    let head = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match channel.receive().await? {
                Some(ServerMessage::Head(head)) => return Ok::<_, anyhow::Error>(head),
                Some(ServerMessage::Receipt(_)) => {}
                other => anyhow::bail!("unexpected bind acknowledgement: {other:?}"),
            }
        }
    })
    .await??;
    assert_eq!(head, ack.head());
    let inspect = serde_json::from_value(envelope(
        "inspect",
        "operation.get",
        json!({"target_operation_id":"not-submitted"}),
    ))?;
    channel
        .send(&inspect)
        .await
        .context("send operation inspection")?;
    let inspected =
        tokio::time::timeout(std::time::Duration::from_secs(5), channel.receive()).await??;
    assert!(
        matches!(inspected, Some(ServerMessage::UnknownOperation { operation_id }) if operation_id == "not-submitted")
    );
    channel.close().await.context("initiate close")?;
    // The existing service drops its split socket on Close without flushing
    // the acknowledgement. Preserve that behavior and report the incomplete
    // handshake; a disconnect never establishes execution completion.
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), channel.receive())
            .await
            .context("wait for explicit close")?,
        Err(Error::CoreChannel("peer ended without a closing handshake"))
    ));
    assert!(channel.receive().await?.is_none());
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream history")?
            .is_empty()
    );
    Ok(())
}
