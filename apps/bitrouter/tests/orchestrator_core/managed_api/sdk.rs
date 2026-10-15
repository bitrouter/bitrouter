use super::*;
use bitrouter_ai::core::protocol::ClientMessage;
use bitrouter_sdk::core::ChannelOptions;
use bitrouter_sdk::error::Error;
use bitrouter_sdk::gateway::GatewayClient;

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
