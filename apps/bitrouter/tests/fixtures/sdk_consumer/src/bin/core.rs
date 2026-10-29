//! SDK-only harness acceptance. The service owns scheduling; this process owns
//! its checkpoint journal and supplies one explicitly mocked tool result.
#[path = "core/journal.rs"]
mod journal;

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::core::{
    checkpoint::DurableHead,
    protocol::{
        Bind, ClientMessage, CommitStatus, ErrorCode, HarnessManifest, HarnessTool, Limits,
        OwnershipGrant, ServerMessage, ToolEffect,
    },
    responses::CreateRequest,
};
use bitrouter_sdk::{
    core::{ChannelOptions, CoreChannel},
    error::Error,
    gateway::GatewayClient,
};
use futures_util::StreamExt;
use journal::Journal;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc, oneshot};

const SESSION: &str = "independent-core-session";
const STEP: Duration = Duration::from_secs(10);

fn env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.is_empty());
    Ok(value)
}

fn command(id: &str, kind: &str, payload: Value) -> Result<ClientMessage> {
    Ok(serde_json::from_value(
        json!({"version":1,"type":kind,"session_id":SESSION,
        "execution_epoch":1,"operation_id":id,"payload":payload}),
    )?)
}

fn request(id: &str) -> Result<CreateRequest> {
    Ok(serde_json::from_value(
        json!({"model":"fixture-model","input":"Read file.txt and report its contents",
        "max_output_tokens":128,"multi_agent":{"enabled":true},"bitrouter":{
        "version":1,"execution":"managed","session_id":SESSION,"execution_epoch":1,"operation_id":id}}),
    )?)
}

async fn receive(channel: &mut CoreChannel) -> Result<ServerMessage> {
    tokio::time::timeout(STEP, channel.receive())
        .await??
        .context("core channel ended unexpectedly")
}

async fn close(mut channel: CoreChannel) -> Result<()> {
    channel.close().await?;
    match tokio::time::timeout(STEP, channel.receive()).await? {
        Ok(None) | Err(Error::CoreChannel("peer ended without a closing handshake")) => {}
        other => bail!("unexpected close result: {other:?}"),
    }
    ensure!(channel.receive().await?.is_none());
    Ok(())
}

fn denied<T>(
    result: bitrouter_sdk::error::Result<T>,
    status: u16,
    code: Option<ErrorCode>,
) -> Result<()> {
    let Err(Error::Core(failure)) = result else {
        bail!("expected typed core HTTP {status}");
    };
    ensure!(failure.status == status && failure.body_error.is_none());
    let error = failure.body.context("missing core error envelope")?.error;
    ensure!(error.commit_status == CommitStatus::NotCommitted);
    if let Some(code) = code {
        ensure!(error.code == code, "wrong error: {error:?}");
    }
    Ok(())
}

async fn run() -> Result<()> {
    let endpoint = env("BITROUTER_SDK_INFERENCE_ENDPOINT")?;
    let key = env("BITROUTER_SDK_INFERENCE_CREDENTIAL")?;
    let other_key = env("BITROUTER_SDK_OTHER_CREDENTIAL")?;
    let home = PathBuf::from(env("BITROUTER_SDK_JOURNAL_DIRECTORY")?);
    ensure!(home.is_absolute());
    let client = GatewayClient::new(&endpoint, Some(&key))?;
    let other = GatewayClient::new(&endpoint, Some(&other_key))?;
    for credential in [None, Some("brvk_invalid-independent-core-credential")] {
        let anonymous = GatewayClient::new(&endpoint, credential)?;
        denied(anonymous.core_capabilities().await, 401, None)?;
        denied(
            anonymous.core_channel(ChannelOptions::default()).await,
            401,
            None,
        )?;
    }
    let caps = client.core_capabilities().await?;
    ensure!(caps.version == 1 && caps.operations.iter().any(|s| s == "session.bind"));
    let tools = vec![HarnessTool {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        effect: ToolEffect::Read,
        approval_required: false,
    }];
    let binding = Bind {
        grant: OwnershipGrant {
            session_id: SESSION.into(),
            harness_id: "independent-harness".into(),
            core_instance_id: caps.core_instance_id,
            execution_epoch: 1,
        },
        durable_head: DurableHead::default(),
        checkpoint: None,
        manifest: HarnessManifest {
            tool_manifest_digest: journal::sha256(&serde_json::to_vec(&tools)?),
            tools,
            workspace_id: "private-mock-workspace".into(),
            workspace_revision: None,
            permission_revision: 1,
            max_tool_output_bytes: 4096,
            artifact_quota_bytes: 1024 * 1024,
            max_artifact_chunk_bytes: 8192,
            required_features: Vec::new(),
        },
        limits: Limits::default(),
    };
    let mut journal = Journal::new(&home)?;
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    channel
        .send(&command(
            "bind",
            "session.bind",
            serde_json::to_value(&binding)?,
        )?)
        .await?;
    let ServerMessage::Checkpoint(first) = receive(&mut channel).await? else {
        bail!("missing bind proposal");
    };
    // Even after bytes reach the harness, the SDK must not grant execution.
    denied(
        client.managed_responses(&request("before-persist")?).await,
        401,
        None,
    )?;
    let ack = journal.persist(&binding.grant, first.clone())?;
    denied(
        client.managed_responses(&request("before-ack")?).await,
        401,
        None,
    )?;
    close(channel).await?;
    // A new channel is an explicit caller decision. No SDK reconnect or ACK.
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    channel
        .send(&command(
            "reconnect",
            "session.bind",
            serde_json::to_value(&binding)?,
        )?)
        .await?;
    let ServerMessage::Checkpoint(repeated) = receive(&mut channel).await? else {
        bail!("missing replayed proposal");
    };
    ensure!(repeated == first && journal.persist(&binding.grant, repeated)? == ack);
    ensure!(journal.batches.len() == 1);
    channel
        .send(&command(
            "ack-bind",
            "checkpoint.ack",
            serde_json::to_value(&ack)?,
        )?)
        .await?;
    loop {
        match receive(&mut channel).await? {
            ServerMessage::Checkpoint(batch) => {
                let payload = journal::decode(&batch)?;
                ensure!(
                    payload.events.len() == 1 && payload.events[0].kind == "session.reconnected"
                );
                let ack = journal.persist(&binding.grant, batch)?;
                channel
                    .send(&command(
                        "ack-reconnect",
                        "checkpoint.ack",
                        serde_json::to_value(ack)?,
                    )?)
                    .await?;
            }
            ServerMessage::Head(head) => {
                ensure!(head == journal.head);
                break;
            }
            ServerMessage::Receipt(_) => {}
            other => bail!("unexpected bind completion: {other:?}"),
        }
    }
    denied(
        other.managed_responses(&request("foreign-tenant")?).await,
        401,
        Some(ErrorCode::UnauthorizedScope),
    )?;
    let mut stale = request("stale-http")?;
    stale.bitrouter.execution_epoch = 2;
    denied(
        client.managed_responses(&stale).await,
        409,
        Some(ErrorCode::StaleEpoch),
    )?;
    for (id, field, value, code) in [
        (
            "foreign-session",
            "session_id",
            json!("another-session"),
            ErrorCode::UnauthorizedScope,
        ),
        (
            "stale-channel",
            "execution_epoch",
            json!(2),
            ErrorCode::StaleEpoch,
        ),
        (
            "bad-version",
            "version",
            json!(99),
            ErrorCode::UnsupportedVersion,
        ),
    ] {
        let mut wire = serde_json::to_value(command(
            id,
            "operation.get",
            json!({"target_operation_id":"missing"}),
        )?)?;
        wire[field] = value;
        channel.send(&serde_json::from_value(wire)?).await?;
        let ServerMessage::Error(error) = receive(&mut channel).await? else {
            bail!("missing {id} error");
        };
        ensure!(
            error.code == code && error.commit_status == CommitStatus::NotCommitted,
            "{id}: {error:?}"
        );
    }
    channel
        .send(&command(
            "head",
            "session.head",
            json!({"durable_head":journal.head}),
        )?)
        .await?;
    ensure!(matches!(receive(&mut channel).await?,ServerMessage::Head(head) if head==journal.head));
    channel
        .send(&command(
            "unknown",
            "operation.get",
            json!({"target_operation_id":"missing"}),
        )?)
        .await?;
    ensure!(
        matches!(receive(&mut channel).await?,ServerMessage::UnknownOperation{operation_id} if operation_id=="missing")
    );
    ensure!(
        journal.batches.len() == 2
            && journal.event_count("session.bound")? == 1
            && journal.event_count("session.reconnected")? == 1,
        "rejections changed durable history"
    );

    let retained = Arc::new(Mutex::new(journal));
    let driver_journal = retained.clone();
    let grant = binding.grant.clone();
    let (tools, mut receive_tools) = mpsc::channel(2);
    let (stop, mut stopping) = oneshot::channel::<()>();
    let driver = tokio::spawn(async move {
        let (mut sender, mut receiver) = channel.split();
        let mut dispatched = std::collections::BTreeSet::new();
        loop {
            tokio::select! {
                _ = &mut stopping => {
                    ensure!(dispatched.len() == 1, "expected exactly one mock tool dispatch");
                    sender.close().await?;
                    match tokio::time::timeout(STEP,receiver.receive()).await? {
                        Ok(None) | Err(Error::CoreChannel("peer ended without a closing handshake")) => return Ok::<_,anyhow::Error>(()),
                        other => bail!("unexpected driver close: {other:?}"),
                    }
                }
                message=receiver.receive()=>match message?.context("driver channel ended")? {
                    ServerMessage::Checkpoint(batch)=>{
                        let ack=driver_journal.lock().await.persist(&grant,batch)?;
                        sender.send(&command(&format!("ack-{}",ack.state_revision),"checkpoint.ack",serde_json::to_value(ack)?)?).await?;
                    }
                    ServerMessage::ToolExecute(tool)=>{
                        ensure!(dispatched.insert((tool.invocation_id.clone(), tool.attempt_id.clone()))
                            && dispatched.len() == 1, "duplicate or unexpected tool dispatch");
                        let store=driver_journal.lock().await;
                        ensure!(tool.execution_epoch==grant.execution_epoch
                            && tool.authorizing_event_seq<=store.head.event_seq,
                            "tool execution preceded its durable authorization");
                        ensure!(tool.tool=="read" && tool.arguments==json!({"path":"file.txt"}));
                        ensure!(tool.tool_manifest_digest==binding.manifest.tool_manifest_digest
                            && tool.permission_revision==1 && tool.workspace_id==binding.manifest.workspace_id);
                        tools.send(*tool).await?;
                    }
                    ServerMessage::Head(_) | ServerMessage::Receipt(_) => {}
                    other=>bail!("unexpected harness message: {other:?}"),
                }
            }
        }
    });
    let mut initial = request("read-first")?;
    let first = serde_json::to_value(client.managed_responses(&initial).await?)?;
    ensure!(first["status"] == "completed" && first["bitrouter"]["run_status"] == "waiting");
    let tool = tokio::time::timeout(STEP, receive_tools.recv())
        .await?
        .context("no mock tool dispatch")?;
    let call = first["output"]
        .as_array()
        .context("missing output")?
        .iter()
        .find(|v| v["type"] == "function_call")
        .context("missing function call")?;
    ensure!(
        call["name"] == tool.tool && call["arguments"] == serde_json::to_string(&tool.arguments)?
    );
    let mut conflicting = initial.clone();
    conflicting.input = json!("different input");
    denied(
        client.managed_responses(&conflicting).await,
        409,
        Some(ErrorCode::OperationConflict),
    )?;
    initial.stream = true;
    let mut replay = client.stream_managed_responses(&initial).await?;
    let mut terminal = None;
    let mut sequence = 0;
    while let Some(event) = replay.next().await {
        let event = event?;
        ensure!(event.data.sequence_number == Some(Some(sequence)));
        sequence += 1;
        if event.event == "response.completed" {
            terminal = event.data.response.flatten();
        }
    }
    ensure!(
        sequence > 0 && serde_json::to_value(terminal.context("no replay terminal")?)? == first
    );
    ensure!(retained.lock().await.event_count("tool.result")? == 0);
    // The harness supplies this mock result. It executes no filesystem tool.
    let mut continuation = request("read-continue")?;
    continuation.previous_response_id =
        Some(first["id"].as_str().context("no response ID")?.into());
    continuation.input = json!([{"type":"function_call_output","call_id":call["call_id"],"output":"mock file body",
        "bitrouter":{"operation_id":"mock-result","status":"succeeded","evidence":[]}}]);
    let mut invalid = continuation.clone();
    invalid.bitrouter.operation_id = "invalid-result".into();
    invalid.input[0]["call_id"] = json!("not-issued");
    denied(
        client.managed_responses(&invalid).await,
        400,
        Some(ErrorCode::InvalidToolResult),
    )?;
    ensure!(retained.lock().await.event_count("tool.result")? == 0);
    continuation.stream = true;
    let mut stream = client.stream_managed_responses(&continuation).await?;
    let mut done = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if event.event == "response.completed" {
            done = event.data.response.flatten();
        }
    }
    let done = serde_json::to_value(done.context("no continuation terminal")?)?;
    ensure!(done["bitrouter"]["run_status"] == "completed");
    ensure!(
        done["output"]
            .as_array()
            .context("no final output")?
            .iter()
            .any(|v| v["type"] == "message" && v["content"][0]["text"] == "verified response")
    );
    let store = retained.lock().await;
    ensure!(store.event_count("tool.result")? == 1);
    let receipt = json!({"accepted":true,"scenario":"independent-core-sdk-v1",
        "batches":store.batches.len(),"tool_results":1,"head":store.head});
    drop(store);
    stop.send(())
        .map_err(|_| anyhow::anyhow!("harness driver already ended"))?;
    driver.await??;
    println!("{receipt}");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), run())
        .await
        .context("independent core acceptance timed out")?
}
