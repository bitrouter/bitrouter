//! The separately built BRO uses the gateway API for both sides of a tool call.
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn stream(delta: Value, reason: &str) -> ResponseTemplate {
    let chunk = |delta, finish: Option<&str>| {
        json!({
            "created":1, "id":"chatcmpl-fixture", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]
        })
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(delta, None),
        chunk(json!({}), Some(reason))
    );
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn bro(
    binary: &Path,
    home: &Path,
    endpoint: &str,
    args: &[&str],
    input: Option<&str>,
) -> Result<Value> {
    let mut child = Command::new(binary)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("BRO_HOME", home.join("bro"))
        .current_dir(home)
        .args(["--endpoint", endpoint])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(text) = input {
        child
            .stdin
            .take()
            .context("missing piped stdin")?
            .write_all(text.as_bytes())
            .await?;
    }
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
    ensure!(
        !output.stdout.contains(&0x1b),
        "redirected BRO emitted terminal controls"
    );
    ensure!(
        output.status.success(),
        "BRO {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = std::str::from_utf8(&output.stdout)?
        .lines()
        .last()
        .context("empty BRO output")?;
    Ok(serde_json::from_str(line)?)
}

#[tokio::test]
#[ignore = "requires the separately built BRO executable in BITROUTER_TEST_BRO"]
async fn gateway_routes_the_approved_bro_tool_continuation() -> Result<()> {
    let binary = std::fs::canonicalize(
        std::env::var_os("BITROUTER_TEST_BRO").context("BRO executable required")?,
    )?;
    let provider = MockServer::start().await;
    let call = json!({"role":"assistant","tool_calls":[{"index":0,"id":"write-1","type":"function",
        "function":{"name":"write","arguments":json!({"path":"note","content":"approved"}).to_string()}}]});
    let model_call = || {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer fixture-provider-key"))
    };
    model_call()
        .and(|r: &wiremock::Request| {
            r.body_json::<Value>().is_ok_and(|v| {
                v["messages"]
                    .as_array()
                    .is_some_and(|m| m.iter().all(|x| x["role"] != "tool"))
            })
        })
        .respond_with(stream(call, "tool_calls"))
        .up_to_n_times(1)
        .mount(&provider)
        .await;
    model_call()
        .and(|r: &wiremock::Request| {
            r.body_json::<Value>().is_ok_and(|v| {
                v["messages"].as_array().is_some_and(|m| {
                    m.iter()
                        .any(|x| x["role"] == "tool" && x["tool_call_id"] == "write-1")
                })
            })
        })
        .respond_with(stream(
            json!({"role":"assistant","content":"Finished."}),
            "stop",
        ))
        .up_to_n_times(1)
        .mount(&provider)
        .await;

    let service = tempfile::tempdir()?;
    let client = tempfile::tempdir()?;
    let workspace = client.path().join("work");
    std::fs::create_dir(&workspace)?;
    let port = TcpListener::bind("127.0.0.1:0")?;
    let address = port.local_addr()?;
    let endpoint = format!("http://{address}");
    let config = service.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '{address}', skip_auth: true}}\ndatabase: {{url: 'sqlite://service.db?mode=rwc'}}\nproviders:\n  fixture:\n    api_base: '{}'\n    api_key: fixture-provider-key\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n",
            provider.uri()
        ),
    )?;
    drop(port);
    let executable = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_bitrouter"))
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_bitrouter").into());
    let mut gateway = Command::new(executable)
        .args(["serve", "--config"])
        .arg(&config)
        .current_dir(service.path())
        .env_clear()
        .env("HOME", service.path())
        .env("BITROUTER_HOME", service.path())
        .kill_on_drop(true)
        .spawn()?;
    let http = reqwest::Client::builder().no_proxy().build()?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            ensure!(
                gateway.try_wait()?.is_none(),
                "gateway exited before readiness"
            );
            if http
                .get(format!("{endpoint}/v1/models"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("waiting for gateway readiness")??;
    let created = bro(
        &binary,
        client.path(),
        &endpoint,
        &[
            "thread",
            "create",
            "--model",
            "test-model",
            "--workspace",
            workspace.to_str().context("workspace path")?,
        ],
        None,
    )
    .await?;
    let thread = created["thread_id"].as_str().context("thread ID")?;
    let pending = bro(
        &binary,
        client.path(),
        &endpoint,
        &["thread", "start", thread, "--prompt-file", "-"],
        Some("write the note\n"),
    )
    .await?;
    ensure!(pending["thread_id"] == thread);
    let turn = pending["turn_id"].as_str().context("turn ID")?;
    let request = pending["pending_input"]["request_id"]
        .as_str()
        .with_context(|| format!("expected approval: {pending}"))?;
    ensure!(!workspace.join("note").exists());
    let complete = bro(
        &binary,
        client.path(),
        &endpoint,
        &[
            "thread",
            "answer",
            thread,
            "--turn",
            turn,
            "--request",
            request,
            "allow",
        ],
        None,
    )
    .await?;
    ensure!(
        complete["status"] == "completed" && complete["final_answer"] == "Finished.",
        "{complete}"
    );
    ensure!(std::fs::read_to_string(workspace.join("note"))? == "approved");
    ensure!(!service.path().join("note").exists());
    std::fs::write(workspace.join("note"), "after completion")?;
    let resumed = bro(
        &binary,
        client.path(),
        &endpoint,
        &["thread", "resume", thread, "--turn", turn],
        None,
    )
    .await?;
    ensure!(
        resumed["status"] == "completed"
            && std::fs::read_to_string(workspace.join("note"))? == "after completion"
    );
    let received = provider
        .received_requests()
        .await
        .context("provider history")?;
    ensure!(received.len() == 2);
    let first: Value = received[0].body_json()?;
    ensure!(
        first["messages"]
            .as_array()
            .context("message list")?
            .iter()
            .any(|m| m["role"] == "user" && m["content"].to_string().contains("write the note")),
        "piped input missing from upstream request"
    );
    gateway.kill().await?;
    Ok(())
}
