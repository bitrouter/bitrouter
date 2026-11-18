//! Keep the initialize response private until the adapter confirms its gateway.
//! After that both JSON-RPC peers own their respective request-ID namespaces.
use super::Endpoint;
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use serde_json::{Value, json};
use std::io::{BufRead, Read};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, LinesCodec};

const MAX_FRAME: usize = 8 * 1024 * 1024;
type AdapterOutput = FramedRead<ChildStdout, LinesCodec>;

fn parse(bytes: &[u8]) -> Result<Value> {
    let frame: Value = serde_json::from_slice(bytes).context("invalid ACP JSON")?;
    ensure!(frame["jsonrpc"] == "2.0", "invalid ACP envelope");
    Ok(frame)
}

// Tokio's stdin uses an uncancellable blocking read. A dedicated OS thread
// lets BRO exit even if the adapter dies while the manager keeps stdin open.
fn manager_input() -> Result<mpsc::Receiver<Result<Value>>> {
    let (sender, receiver) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("acp-manager-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                let read = (&mut stdin)
                    .take((MAX_FRAME + 1) as u64)
                    .read_until(b'\n', &mut buffer);
                let message = match read {
                    Ok(0) => break,
                    Ok(n) if n > MAX_FRAME => {
                        Err(anyhow::anyhow!("ACP manager frame exceeds size limit"))
                    }
                    Ok(_) => parse(&buffer),
                    Err(error) => Err(error.into()),
                };
                let finished = message.is_err();
                if sender.blocking_send(message).is_err() || finished {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), frame: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    timeout(Duration::from_secs(5), async {
        writer.write_all(&line).await?;
        writer.flush().await
    })
    .await
    .context("ACP peer stopped reading")??;
    Ok(())
}

fn rejection(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn response(output: &mut AdapterOutput, id: &Value) -> Result<Value> {
    let line = output
        .next()
        .await
        .context("ACP adapter disconnected during initialize")??;
    let frame = parse(line.as_bytes())?;
    ensure!(
        frame.get("method").is_none() && frame.get("id") == Some(id),
        "unexpected response during ACP initialize"
    );
    Ok(frame)
}

async fn initialize(
    input: &mut ChildStdin,
    output: &mut AdapterOutput,
    request: &Value,
    name: &str,
    plan: Option<&Endpoint<'_>>,
) -> Result<Value> {
    send(input, request).await?;
    let mut frame = response(output, &request["id"]).await?;
    if frame.get("error").is_some() {
        return Ok(frame);
    }
    let result = frame["result"]
        .as_object_mut()
        .context("invalid initialize result")?;
    ensure!(
        result.get("protocolVersion") == Some(&json!(1)),
        "adapter did not negotiate ACP v1"
    );
    let capabilities = result
        .entry("agentCapabilities")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("invalid adapter capabilities")?;
    if capabilities
        .remove("providers")
        .is_some_and(|v| !v.is_null())
        && let Some(plan) = plan
    {
        let id = json!("bro-provider-set");
        send(
            input,
            &json!({"jsonrpc":"2.0","id":id,"method":"providers/set","params":plan.set_provider()}),
        )
        .await?;
        let set = response(output, &id).await?;
        ensure!(
            set["result"].is_object() && set.get("error").is_none(),
            "adapter rejected the gateway binding"
        );
        let id = json!("bro-provider-list");
        send(
            input,
            &json!({"jsonrpc":"2.0","id":id,"method":"providers/list","params":{}}),
        )
        .await?;
        let listed = response(output, &id).await?;
        let providers = serde_json::from_value(listed["result"].clone())
            .context("invalid provider confirmation")?;
        ensure!(
            listed.get("error").is_none() && plan.matches(&providers),
            "adapter did not confirm the gateway binding"
        );
    }
    let upstream = result.insert("agentInfo".into(), json!({
        "name":"bitrouter-acp-controller", "title":"BitRouter ACP Controller","version":env!("CARGO_PKG_VERSION")
    }));
    result
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("invalid initialize metadata")?
        .insert(
            "bitrouter.dev/controller".into(),
            json!({"harnessId":name,"upstreamAgentInfo":upstream}),
        );
    Ok(frame)
}

fn redact(frame: &mut Value, plan: Option<&Endpoint<'_>>) {
    if let Some(plan) = plan {
        for key in ["params", "result", "error"] {
            if let Some(body) = frame.get_mut(key) {
                super::super::wire::redact(body, plan.token());
            }
        }
    }
}

pub(super) async fn bridge(
    mut input: ChildStdin,
    child_output: ChildStdout,
    name: &str,
    plan: Option<&Endpoint<'_>>,
) -> Result<()> {
    let mut output = FramedRead::new(child_output, LinesCodec::new_with_max_length(MAX_FRAME));
    let mut manager = manager_input()?;
    let mut stdout = tokio::io::stdout();
    loop {
        let incoming = tokio::select! {
            frame = manager.recv() => frame.transpose()?,
            line = output.next() => {
                line.context("ACP adapter disconnected")??;
                anyhow::bail!("unexpected adapter traffic before initialize");
            }
        };
        let Some(request) = incoming else {
            return Ok(());
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        if request["method"] != "initialize" {
            send(&mut stdout, &rejection(id, -32002, "initialize required")).await?;
            continue;
        }
        if request["params"]["protocolVersion"].as_u64().is_none() {
            send(
                &mut stdout,
                &rejection(id, -32602, "protocolVersion required"),
            )
            .await?;
            continue;
        }
        // Continue watching manager EOF while the child or gateway negotiation stalls.
        let handshake = timeout(
            Duration::from_secs(60),
            initialize(&mut input, &mut output, &request, name, plan),
        );
        tokio::pin!(handshake);
        let result = loop {
            tokio::select! {
                result = &mut handshake => break result.context("ACP initialize timed out").and_then(|r| r),
                pending = manager.recv() => {
                    let Some(pending) = pending.transpose()? else { return Ok(()); };
                    if let Some(id) = pending.get("id") {
                        send(&mut stdout, &rejection(id, -32002, "initialize in progress")).await?;
                    }
                }
            }
        };
        let mut reply =
            result.unwrap_or_else(|_| rejection(id, -32603, "ACP adapter initialization failed"));
        redact(&mut reply, plan);
        send(&mut stdout, &reply).await?;
        ensure!(
            reply.get("error").is_none(),
            "ACP adapter initialization failed"
        );
        break;
    }

    loop {
        enum Incoming {
            Manager(Option<Value>),
            Adapter(Value),
        }
        let incoming = tokio::select! {
            frame = manager.recv() => Incoming::Manager(frame.transpose()?),
            line = output.next() => {
                let line = line.context("ACP adapter disconnected")??;
                Incoming::Adapter(parse(line.as_bytes())?)
            }
        };
        match incoming {
            Incoming::Manager(None) => return Ok(()),
            Incoming::Manager(Some(frame)) => {
                let blocked = match frame["method"].as_str() {
                    Some("initialize") => Some((-32600, "already initialized")),
                    Some(m) if m.starts_with("providers/") || m.starts_with("_bitrouter/") => {
                        Some((-32601, "method unavailable"))
                    }
                    _ => None,
                };
                if let Some((code, message)) = blocked {
                    if let Some(id) = frame.get("id") {
                        send(&mut stdout, &rejection(id, code, message)).await?;
                    }
                } else {
                    send(&mut input, &frame).await?;
                }
            }
            Incoming::Adapter(mut frame) => {
                redact(&mut frame, plan);
                send(&mut stdout, &frame).await?;
            }
        }
    }
}
