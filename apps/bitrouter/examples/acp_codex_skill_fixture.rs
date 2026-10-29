//! Independent Codex ACP text-to-model contract fixture; no real Codex process.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn send(output: &mut impl Write, value: Value) -> TestResult {
    serde_json::to_writer(&mut *output, &value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

fn main() -> TestResult {
    let arguments: Vec<_> = std::env::args().collect();
    let journal = arguments
        .windows(2)
        .find(|pair| pair[0] == "--journal")
        .map(|pair| pair[1].clone())
        .ok_or("journal required")?;
    let hold = arguments.iter().any(|arg| arg == "--hold");
    let reject = arguments.iter().any(|arg| arg == "--reject");
    let permission = arguments.iter().any(|arg| arg == "--permission");
    let effect = std::path::PathBuf::from(&journal).with_extension("effect");
    let mut history = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)?;
    let input = std::io::stdin();
    let mut output = std::io::stdout().lock();
    let mut waiting = None;
    for line in input.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        serde_json::to_writer(&mut history, &request)?;
        writeln!(history)?;
        history.flush()?;
        let id = request["id"].clone();
        if request["method"].is_null() && id == "codex-permission" {
            if request["result"]["outcome"]["optionId"] == "allow" {
                std::fs::write(&effect, "approved Codex fixture effect")?;
            }
            if let Some(original) = waiting.take() {
                send(
                    &mut output,
                    json!({"jsonrpc":"2.0","id":original,"result":{"stopReason":"end_turn"}}),
                )?;
            }
            continue;
        }
        match request["method"].as_str() {
            Some("initialize") => send(
                &mut output,
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"embeddedContext":true}},
                    "agentInfo":{"name":"codex-acp-fixture","version":"1.10.0-contract"},"authMethods":[]
                }}),
            )?,
            Some("session/new") => send(
                &mut output,
                json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"codex-skill-session"}}),
            )?,
            Some("session/prompt") => {
                let blocks = request["params"]["prompt"]
                    .as_array()
                    .ok_or("prompt array")?;
                let mut model_inputs = Vec::new();
                for block in blocks {
                    if block["type"] != "text" {
                        return Err("Codex carrier must supply text".into());
                    }
                    let text = block["text"].as_str().ok_or("text body")?;
                    // Mirrors the pinned adapter's buildPromptItems text arm.
                    model_inputs.push(json!({"type":"text","text":text,"text_elements":[]}));
                }
                serde_json::to_writer(
                    &mut history,
                    &json!({"event":"codex_model_input","inputs":model_inputs}),
                )?;
                writeln!(history)?;
                history.flush()?;
                if reject {
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Codex fixture result unavailable after input"}}),
                    )?;
                } else if permission {
                    waiting = Some(id);
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":"codex-permission","method":"session/request_permission","params":{
                            "sessionId":"codex-skill-session","toolCall":{"toolCallId":"codex-execute","title":"shell fixture effect","kind":"execute"},
                            "options":[{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"deny","name":"Reject once","kind":"reject_once"}]
                        }}),
                    )?;
                } else if hold {
                    waiting = Some(id);
                } else {
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"codex-skill-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"codex fixture accepted"}}}}),
                    )?;
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                    )?;
                }
            }
            Some("session/cancel") => {
                if let Some(original) = waiting.take() {
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":original,"result":{"stopReason":"cancelled"}}),
                    )?;
                }
            }
            _ if !id.is_null() => send(
                &mut output,
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"not supported by Codex contract fixture"}}),
            )?,
            _ => {}
        }
    }
    Ok(())
}
