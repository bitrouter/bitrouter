//! Direct manager compatibility fixture. It has no provider or gateway credentials.
use super::send;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::BufRead;

pub(super) fn run() -> Result<()> {
    ensure!(std::env::var_os("BITROUTER_API_KEY").is_none());
    ensure!(std::env::var_os("BRO_FIXTURE_PARENT_SECRET").is_none());
    ensure!(std::env::var("BRO_FIXTURE_LOCAL")? == "local");
    let mut out = std::io::stdout().lock();
    let mut sessions = 0;
    for line in std::io::stdin().lock().lines() {
        let frame: Value = serde_json::from_str(&line?)?;
        let params = &frame["params"];
        let reply = match frame["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{},"resume":{}}},"agentInfo":{"name":"stub-harness","version":"1.0.0"}})
            }
            Some("session/new") => {
                sessions += 1;
                json!({"sessionId":if sessions == 1 { "native-a" } else { "native-b" }})
            }
            Some("session/load") => {
                ensure!(params["sessionId"] == "native-a");
                json!({"_meta":{"loadedBy":"harness"}})
            }
            Some("session/prompt") => {
                let session = &params["sessionId"];
                ensure!(session == "native-a" || session == "native-b");
                for update in [
                    json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}),
                    json!({"sessionUpdate":"plan","entries":[{"content":"step one","priority":"high","status":"pending"}]}),
                    json!({"sessionUpdate":"available_commands_update","availableCommands":[{"name":"create_plan","description":"draft a plan"}]}),
                    json!({"sessionUpdate":"current_mode_update","currentModeId":"plan"}),
                    json!({"sessionUpdate":"config_option_update","configOptions":[]}),
                    json!({"sessionUpdate":"session_info_update","title":"conformance"}),
                ] {
                    send(
                        &mut out,
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}),
                    )?;
                }
                json!({"stopReason":"end_turn"})
            }
            _ => anyhow::bail!("unsupported direct fixture request"),
        };
        send(
            &mut out,
            json!({"jsonrpc":"2.0","id":frame.get("id").context("expected request")?,"result":reply}),
        )?;
    }
    Ok(())
}
