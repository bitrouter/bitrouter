//! Cargo-built ACP adapter for BRO subprocess tests. Inputs are synthetic.
#[path = "fixture/serve.rs"]
mod serve;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;

fn send(out: &mut impl Write, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|a| a == "--descendant") {
        let dir = std::path::Path::new(args.get(2).context("descendant workspace")?);
        let lifetime = std::net::TcpListener::bind("127.0.0.1:0")?;
        let ready = dir.join("descendant-ready.tmp");
        std::fs::write(&ready, lifetime.local_addr()?.to_string())?;
        std::fs::rename(ready, dir.join("descendant-ready"))?;
        for _ in 0..500 {
            if dir.join("release").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // A natural exit at the fixture deadline is also evidence that BRO did not stop us.
        std::fs::write(dir.join("orphan-effect"), "orphaned")?;
        return Ok(());
    }
    if args.iter().any(|a| a == "--serve-compat") {
        return serve::run();
    }
    let fallback = args.iter().any(|a| a == "--fallback");
    let mismatch = args.iter().any(|a| a == "--mismatch");
    let check_model = args.iter().any(|a| a == "--check-model");
    let codex = args.iter().any(|a| a.contains("codex-acp"));
    let (provider, protocol, base, token) = if codex {
        let config: Value = serde_json::from_str(&std::env::var("CODEX_CONFIG")?)?;
        let p = &config["model_providers"]["bitrouter"];
        ensure!(!check_model || config["model"] == "fixture-model");
        ensure!(config["model_provider"] == "bitrouter" && p["env_key"] == "BITROUTER_API_KEY");
        (
            "openai",
            "openai",
            p["base_url"].as_str().context("missing base")?.to_owned(),
            std::env::var("BITROUTER_API_KEY")?,
        )
    } else {
        ensure!(!check_model || std::env::var("ANTHROPIC_MODEL")? == "fixture-model");
        (
            "main",
            "anthropic",
            std::env::var("ANTHROPIC_BASE_URL")?,
            std::env::var("ANTHROPIC_AUTH_TOKEN")?,
        )
    };
    let mismatch = mismatch || base.ends_with("/mismatch/v1");
    // Exercise a noisy adapter without writing real or test credentials to disk.
    eprintln!("adapter echoes brk_synthetic-acp-bound-token");
    ensure!(!args.iter().any(|a| a.contains(&token)));
    for key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "BRO_FIXTURE_PARENT_SECRET",
        "CLAUDECODE",
    ] {
        ensure!(std::env::var_os(key).is_none(), "inherited {key}");
    }
    let mut configured = false;
    let mut verified = false;
    let mut workspace = None;
    let mut pending = None;
    let mut hanging_prompt = None;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let m: Value = serde_json::from_str(&line?)?;
        let id = &m["id"];
        let p = &m["params"];
        let result = match m["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":1,"agentCapabilities":if fallback { json!({}) } else { json!({"providers":{}}) }})
            }
            Some("providers/set") => {
                ensure!(
                    !fallback
                        && p["providerId"] == provider
                        && p["apiType"] == protocol
                        && p["baseUrl"] == base
                );
                ensure!(p["headers"]["authorization"] == format!("Bearer {token}"));
                ensure!(
                    p["headers"]["x-bitrouter-controller-id"]
                        .as_str()
                        .is_some_and(|c| c.starts_with("brc_"))
                );
                configured = true;
                json!({})
            }
            Some("providers/list") => {
                ensure!(configured);
                verified = true;
                json!({"providers":[{"providerId":provider,"supported":[protocol],"required":true,
                    "current":{"apiType":protocol,"baseUrl":if mismatch {"http://wrong.invalid"} else {&base}}}]})
            }
            Some("session/new") => {
                ensure!(verified || fallback);
                let cwd = std::path::PathBuf::from(p["cwd"].as_str().context("cwd required")?);
                ensure!(cwd.is_absolute() && cwd.is_dir() && p["mcpServers"] == json!([]));
                workspace = Some(cwd);
                json!({"sessionId":"fixture-session"})
            }
            Some("session/prompt") => {
                let cwd = workspace.as_ref().context("new session first")?;
                ensure!(p["sessionId"] == "fixture-session");
                let text = p["prompt"][0]["text"].as_str().context("prompt required")?;
                std::fs::write(cwd.join("prompt-received"), "once")?;
                if text == "disconnect" {
                    return Ok(());
                }
                if text == "hang-with-child" {
                    // Reproduce the npx -> adapter wrapper chain. Retain the child
                    // handle and join when it exits, exactly as a package runner can.
                    std::process::Command::new(std::env::current_exe()?)
                        .arg("--descendant")
                        .arg(cwd)
                        .status()?;
                    continue;
                }
                if text == "hang" {
                    hanging_prompt = Some(id.clone());
                    continue;
                }
                if text == "reject" {
                    send(
                        &mut out,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"brk_synthetic-acp-bound-token"}}),
                    )?;
                    continue;
                }
                if text == "ask" || text == "only-allow" {
                    let mut options =
                        vec![json!({"optionId":"allow","name":"Allow","kind":"allow_once"})];
                    if text == "ask" {
                        options.push(json!({"optionId":"deny","name":"Deny","kind":"reject_once"}));
                    }
                    send(
                        &mut out,
                        json!({"jsonrpc":"2.0","id":"permission","method":"session/request_permission",
                        "params":{"sessionId":"fixture-session","toolCall":{"toolCallId":"t","title":"run","kind":"execute"},"options":options}}),
                    )?;
                    pending = Some((id.clone(), text == "only-allow"));
                    continue;
                }
                if text == "call gateway" {
                    let url = url::Url::parse(&base)?;
                    let host = url.host_str().context("host required")?;
                    let port = url.port().context("port required")?;
                    ensure!(host == "127.0.0.1");
                    let mut conn = TcpStream::connect((host, port))?;
                    let path = if codex { "responses" } else { "v1/messages" };
                    write!(
                        conn,
                        "POST {}/{path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
                        url.path().trim_end_matches('/')
                    )?;
                    let mut response = String::new();
                    conn.read_to_string(&mut response)?;
                    ensure!(response.starts_with("HTTP/1.1 200"));
                }
                send(
                    &mut out,
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fixture-session",
                    "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"adapter reply brk_synthetic-acp-bound-token"}}}}),
                )?;
                json!({"stopReason":"end_turn"})
            }
            Some("session/cancel") => {
                ensure!(p["sessionId"] == "fixture-session");
                if let Some(original) = hanging_prompt.take() {
                    send(
                        &mut out,
                        json!({"jsonrpc":"2.0","id":original,"result":{"stopReason":"cancelled"}}),
                    )?;
                    continue;
                }
                return Ok(());
            }
            None if id == "permission" => {
                let (original, only_allow) = pending.take().context("unexpected reply")?;
                let response = &m["result"]["outcome"];
                ensure!(if only_allow {
                    response["outcome"] == "cancelled"
                } else {
                    response["optionId"] == "deny"
                });
                send(
                    &mut out,
                    json!({"jsonrpc":"2.0","id":original,"result":{"stopReason":"end_turn"}}),
                )?;
                continue;
            }
            _ => anyhow::bail!("unexpected request"),
        };
        send(&mut out, json!({"jsonrpc":"2.0","id":id,"result":result}))?;
    }
    Ok(())
}
