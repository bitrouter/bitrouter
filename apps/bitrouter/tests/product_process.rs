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
    credential: &str,
    args: &[&str],
    input: Option<&str>,
) -> Result<Value> {
    let mut child = Command::new(binary)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("BITROUTER_API_KEY", credential)
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
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '{address}', skip_auth: false}}\ndatabase: {{url: 'sqlite://service.db?mode=rwc'}}\nproviders:\n  fixture:\n    api_base: '{}'\n    api_key: fixture-provider-key\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n",
            provider.uri()
        ),
    )?;
    drop(port);
    let executable = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_bitrouter"))
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_bitrouter").into());
    let issuer = Command::new(&executable)
        .current_dir(service.path())
        .env_clear()
        .env("HOME", service.path())
        .env("BITROUTER_HOME", service.path())
        .args([
            "--json",
            "key",
            "sign",
            "--user",
            "bro-client",
            "--db",
            "sqlite://service.db?mode=rwc",
        ])
        .output()
        .await?;
    ensure!(
        issuer.status.success(),
        "gateway key issuance failed: {}",
        String::from_utf8_lossy(&issuer.stderr)
    );
    let issued: Value = serde_json::from_slice(&issuer.stdout)?;
    let credential = issued["secret"]
        .as_str()
        .context("missing client credential")?;

    let mut gateway = Command::new(&executable)
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
                .get(format!("{endpoint}/health"))
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
    for token in [None, Some("brvk_invalid_fixture")] {
        let mut attempt = http
            .post(format!("{endpoint}/v1/chat/completions"))
            .json(&json!({"model":"test-model","messages":[{"role":"user","content":"denied"}]}));
        if let Some(token) = token {
            attempt = attempt.bearer_auth(token);
        }
        let response = attempt.send().await?;
        ensure!(
            response.status() == reqwest::StatusCode::UNAUTHORIZED,
            "unauthorized request returned {}",
            response.status()
        );
    }
    ensure!(
        provider
            .received_requests()
            .await
            .context("provider history")?
            .is_empty()
    );
    let created = bro(
        &binary,
        client.path(),
        &endpoint,
        credential,
        &[
            "thread",
            "create",
            "--managed",
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
        credential,
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
        credential,
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
        credential,
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
    let history = bro(
        &binary,
        client.path(),
        &endpoint,
        credential,
        &["thread", "history", thread],
        None,
    )
    .await?;
    let steps = history["turns"][0]["steps"]
        .as_array()
        .context("BRO step ledger")?;
    ensure!(steps.len() == received.len());
    for (step, upstream) in steps.iter().zip(&received) {
        let caller = step["caller_request_id"]
            .as_str()
            .context("durable caller ID")?;
        ensure!(
            upstream
                .headers
                .get("x-bitrouter-request-id")
                .context("upstream correlation")?
                .to_str()?
                == caller
        );
        ensure!(step["request_id"] == caller);
    }
    ensure!(steps[0]["caller_request_id"] != steps[1]["caller_request_id"]);
    let first: Value = received[0].body_json()?;
    ensure!(
        first["messages"]
            .as_array()
            .context("message list")?
            .iter()
            .any(|m| m["role"] == "user" && m["content"].to_string().contains("write the note")),
        "piped input missing from upstream request"
    );
    // Continue the same native thread through a separately running gateway.
    // The child's actual read must see the post-completion edit.
    let function = |id: &str, name: &str, args: Value| {
        json!({"role":"assistant","tool_calls":[
        {"index":0,"type":"function","id":id,"function":{"name":name,"arguments":args.to_string()}}]})
    };
    for (role, needle, delta, finish) in [
        (
            "user",
            "verify saved note",
            function(
                "child-1",
                "spawn_agent",
                json!({"task":{"text":"inspect note independently"}}),
            ),
            "tool_calls",
        ),
        (
            "user",
            "inspect note independently",
            function("read-1", "read", json!({"path":"note"})),
            "tool_calls",
        ),
        (
            "tool",
            "after completion",
            json!({"role":"assistant","content":"verified current note"}),
            "stop",
        ),
        (
            "tool",
            "verified current note",
            json!({"role":"assistant","content":"independent read complete"}),
            "stop",
        ),
        (
            "user",
            "propose another edit",
            function(
                "write-2",
                "write",
                json!({"path":"note","content":"must be cancelled"}),
            ),
            "tool_calls",
        ),
    ] {
        model_call()
            .and(move |r: &wiremock::Request| {
                r.body_json::<Value>().is_ok_and(|v| {
                    v["messages"]
                        .as_array()
                        .and_then(|m| m.last())
                        .is_some_and(|m| {
                            m["role"] == role
                                && m["content"]
                                    .as_str()
                                    .is_some_and(|text| text.contains(needle))
                        })
                })
            })
            .respond_with(stream(delta, finish))
            .up_to_n_times(1)
            .mount(&provider)
            .await;
    }
    let continued = bro(
        &binary,
        client.path(),
        &endpoint,
        credential,
        &[
            "thread",
            "start",
            thread,
            "verify saved note",
            "--request-id",
            "next",
        ],
        None,
    )
    .await?;
    ensure!(
        continued["final_answer"] == "independent read complete",
        "{continued}"
    );
    let pending = bro(
        &binary,
        client.path(),
        &endpoint,
        credential,
        &["thread", "start", thread, "propose another edit"],
        None,
    )
    .await?;
    ensure!(pending["status"] == "waiting_for_input");
    let cancelled = bro(
        &binary,
        client.path(),
        &endpoint,
        credential,
        &[
            "thread",
            "cancel",
            thread,
            "--turn",
            pending["turn_id"].as_str().context("turn")?,
        ],
        None,
    )
    .await?;
    ensure!(
        cancelled["status"] == "cancelled"
            && std::fs::read_to_string(workspace.join("note"))? == "after completion"
    );
    ensure!(
        provider
            .received_requests()
            .await
            .context("provider history")?
            .len()
            == 7
    );
    gateway.kill().await?;
    Ok(())
}
