//! Prebuilt Rust ACP peer for ignored real-process ownership tests.
//! Protocol fields use the official ACP schema; no capture or authority mocks.
//! https://agentclientprotocol.com/protocol/session-setup
//! https://agentclientprotocol.com/protocol/session-config-options
#[cfg(unix)]
#[path = "owned_capture_fixture/reaper.rs"]
mod reaper;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn send(value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    let mut output = tokio::io::stdout();
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}
fn options(model: &str) -> Value {
    json!([{"id":"model","name":"Model","type":"select","currentValue":model,"options":[{"value":"provider:literal-route","name":"Retained route"}]}])
}

#[cfg(unix)]
async fn raw_ingress_attack(mode: &str, session: &Value) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let socket = std::env::var("CAPTURE_FIXTURE_INGRESS_GATE")?;
    let mut gate = tokio::net::UnixStream::connect(socket).await?;
    gate.write_all(b"ready").await?;
    let mut release = [0; 1];
    gate.read_exact(&mut release).await?;
    // Parent holds the real capture writer on this first notification, then
    // releases the raw flood only after observing that actor's Active phase.
    let update = |text: &str| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}});
    send(update("held before forwarding")).await?;
    gate.read_exact(&mut release).await?;
    match mode {
        "unterminated" => {
            let mut stdout = tokio::io::stdout();
            let chunk = [b'x'; 4096];
            for _ in 0..1024 {
                stdout.write_all(&chunk).await?;
                stdout.flush().await?;
            }
        }
        "frames" => {
            for _ in 0..4096 {
                send(update("flood must not forward")).await?;
            }
        }
        "bytes" => {
            let text = "x".repeat(48 * 1024);
            for _ in 0..512 {
                send(update(&text)).await?;
            }
        }
        _ => anyhow::bail!("unsupported ingress attack fixture mode"),
    }
    anyhow::bail!("raw ingress attack completed without disconnect")
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(unix)]
    if reaper::leaf().await? {
        return Ok(());
    }
    #[cfg(unix)]
    let _descendant = reaper::spawn_descendant()?;
    let marker = std::env::var("CAPTURE_FIXTURE_MARKER")?;
    tokio::fs::write(
        &marker,
        serde_json::to_vec(
            &json!({"pid":std::process::id(),"inherited_home":std::env::var_os("HOME").is_some(),"provider_set":0}),
        )?,
    )
    .await?;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut endpoint: Option<Value> = None;
    let mut model = "unselected".to_owned();
    let mut sessions = 0;
    while let Some(line) = input.next_line().await? {
        ensure!(line.len() <= 1024 * 1024, "fixture frame exceeds bound");
        let message: Value = serde_json::from_str(&line)?;
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .context("fixture method")?;
        let params = &message["params"];
        let result = match method {
            "initialize" => {
                #[cfg(unix)]
                if let Ok(socket) = std::env::var("CAPTURE_FIXTURE_INITIALIZE_GATE") {
                    let mut stream = tokio::net::UnixStream::connect(socket).await?;
                    stream.write_all(b"ready").await?;
                    let mut release = [0; 1];
                    tokio::io::AsyncReadExt::read_exact(&mut stream, &mut release).await?;
                    if std::env::var_os("CAPTURE_FIXTURE_EXIT_AT_INITIALIZE").is_some() {
                        return Ok(());
                    }
                }
                json!({"protocolVersion":params["protocolVersion"],"agentCapabilities":{"providers":{},"loadSession":true},"agentInfo":{"name":"owned-capture-fixture","version":"1"},"authMethods":[]})
            }
            "providers/set" => {
                let mut observed: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
                observed["provider_set"] = json!(
                    observed["provider_set"]
                        .as_u64()
                        .context("provider count")?
                        + 1
                );
                tokio::fs::write(&marker, serde_json::to_vec(&observed)?).await?;
                if std::env::var_os("CAPTURE_FIXTURE_FAIL_PROVIDER").is_some() {
                    send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"fixture provider initialization rejected"}})).await?;
                    continue;
                }
                endpoint = Some(params.clone());
                json!({})
            }
            "providers/list" => {
                let configured = endpoint.as_ref().context("provider not configured")?;
                json!({"providers":[{"providerId":configured["providerId"],"supported":[configured["apiType"]],"required":true,"current":{"apiType":configured["apiType"],"baseUrl":configured["baseUrl"]}}]})
            }
            "session/new" => {
                sessions += 1;
                json!({"sessionId":format!("native-{sessions}"),"configOptions":options(&model)})
            }
            "session/set_config_option" => {
                model = params
                    .get("value")
                    .and_then(Value::as_str)
                    .context("model value")?
                    .to_owned();
                json!({"configOptions":options(&model)})
            }
            "session/prompt" => {
                #[cfg(unix)]
                if let Ok(mode) = std::env::var("CAPTURE_FIXTURE_INGRESS_ATTACK") {
                    raw_ingress_attack(&mode, &params["sessionId"]).await?;
                }
                let configured = endpoint.as_ref().context("provider not configured")?;
                let base = configured
                    .get("baseUrl")
                    .and_then(Value::as_str)
                    .context("base URL")?;
                let headers = configured
                    .get("headers")
                    .and_then(Value::as_object)
                    .context("provider headers")?;
                let mut request = reqwest::Client::new().post(format!("{}/chat/completions", base.trim_end_matches('/')))
                    .json(&json!({"model":model,"messages":[{"role":"user","content":"fixture turn"}],"stream":false}));
                for (name, value) in headers {
                    request = request.header(name, value.as_str().context("header text")?);
                }
                let response = request.send().await?;
                ensure!(response.status().is_success(), "fixture inference rejected");
                let body: Value = response.json().await?;
                let text = body["choices"][0]["message"]["content"]
                    .as_str()
                    .context("fixture model content")?;
                send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":params["sessionId"],"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}})).await?;
                json!({"stopReason":"end_turn"})
            }
            _ => {
                send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"fixture unsupported method"}})).await?;
                continue;
            }
        };
        send(json!({"jsonrpc":"2.0","id":id,"result":result})).await?;
    }
    Ok(())
}
