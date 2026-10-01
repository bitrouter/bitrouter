//! Separate-process proof that the CLI, daemon, routed model, and BRO task
//! service share one coding path. The upstream here is deliberately a fixture.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_TOKEN: &str = "fixture-task-token-0123456789abcdef";

#[tokio::test]
async fn separate_process_client_and_server_finish_verified_coding_task() -> Result<()> {
    let upstream = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let turns_for_reply = turns.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let turn = turns_for_reply.fetch_add(1, Ordering::SeqCst);
            let body = if turn == 0 {
                tool_sse(vec![
                    tool_call(0, "read-1", "read", json!({"path": ".", "limit": 1})),
                    tool_call(3, "glob-1", "glob", json!({"pattern": "*.txt"})),
                    tool_call(4, "grep-1", "grep", json!({"pattern": "before"})),
                    tool_call(5, "write-1", "write", json!({"path": "NOTES.md", "content": "Updated note."})),
                    tool_call(1, "edit-1", "edit", json!({"path": "note.txt", "edits": [{"oldText": "before", "newText": "after"}]})),
                    tool_call(2, "shell-1", "shell", json!({"command": "cat note.txt"})),
                ])
            } else {
                text_sse("Changed note.txt and checked it.")
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .mount(&upstream)
        .await;

    let home = tempfile::tempdir()?;
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("note.txt"), "before\n")?;
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n",
            home.path().join("bitrouter.db").display(),
            upstream.uri()
        ),
    )?;
    let binary = env!("CARGO_BIN_EXE_bro");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(binary)
            .args([
                "task",
                "run",
                "Change the word in note.txt",
                "--model",
                "test-model",
                "--check",
                "test \"$(cat note.txt)\" = after",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--config")
            .arg(&config)
            .output(),
    )
    .await
    .context("native task timed out")??;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let stop = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(binary)
            .arg("stop")
            .arg("--config")
            .arg(&config)
            .output(),
    )
    .await;
    ensure!(output.status.success(), "task failed: {stderr}\n{stdout}");
    ensure!(stop.is_ok(), "daemon stop timed out");
    ensure!(std::fs::read_to_string(workspace.join("note.txt"))? == "after\n");
    ensure!(
        turns.load(Ordering::SeqCst) == 2,
        "expected two routed model turns"
    );
    ensure!(std::fs::read_to_string(workspace.join("NOTES.md"))? == "Updated note.");
    for request in upstream
        .received_requests()
        .await
        .context("missing model requests")?
    {
        let body: Value = serde_json::from_slice(&request.body)?;
        let names: Vec<_> = body["tools"]
            .as_array()
            .context("missing tools")?
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        ensure!(names == ["read", "glob", "grep", "write", "edit", "shell"]);
        ensure!(
            body["tools"][5]["function"]["description"]
                .as_str()
                .is_some_and(
                    |description| description.contains("Bash") || description.contains("POSIX sh")
                )
        );
    }
    let lines = stdout
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(lines.iter().any(|line| line["type"] == "accepted"));
    ensure!(lines.iter().any(|line| line["type"] == "event"));
    let terminal = lines.last().context("missing terminal output")?;
    ensure!(terminal["type"] == "terminal");
    ensure!(terminal["status"] == "completed", "{terminal}");
    ensure!(terminal["verification"] == "passed", "{terminal}");
    ensure!(terminal["final_answer"] == "Changed note.txt and checked it.");
    Ok(())
}

fn tool_call(index: usize, id: &str, name: &str, args: Value) -> Value {
    json!({"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}})
}

fn sse(delta: Value, reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-native", "object": "chat.completion.chunk", "model": "test-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        })
    )
}

fn text_sse(text: &str) -> String {
    format!(
        "{}{}data: [DONE]\n\n",
        sse(json!({"role": "assistant", "content": text}), None),
        sse(json!({}), Some("stop"))
    )
}

fn tool_sse(calls: Vec<Value>) -> String {
    format!(
        "{}{}data: [DONE]\n\n",
        sse(json!({"role": "assistant", "tool_calls": calls}), None),
        sse(json!({}), Some("tool_calls"))
    )
}

#[tokio::test]
async fn simultaneous_local_clients_join_one_server() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse("Done."), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let home = tempfile::tempdir()?;
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models: [{{id: test-model}}]\n",
            home.path().join("bitrouter.db").display(),
            upstream.uri()
        ),
    )?;
    let binary = env!("CARGO_BIN_EXE_bro");
    let workspaces = [home.path().join("one"), home.path().join("two")];
    for workspace in &workspaces {
        std::fs::create_dir(workspace)?;
    }
    let command = |workspace: &std::path::Path| {
        let mut child = Command::new(binary);
        child
            .arg("task")
            .arg("run")
            .arg("Say done")
            .arg("--model")
            .arg("test-model")
            .arg("--workspace")
            .arg(workspace)
            .arg("--config")
            .arg(&config);
        if workspace == workspaces[0] {
            child.arg("--read-only");
        }
        child
    };
    let (first, second) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(40), command(&workspaces[0]).output()),
        tokio::time::timeout(Duration::from_secs(40), command(&workspaces[1]).output()),
    );
    let first = first.context("first client timed out")??;
    let second = second.context("second client timed out")??;
    let _ = Command::new(binary)
        .arg("stop")
        .arg("--config")
        .arg(&config)
        .output()
        .await;
    ensure!(
        first.status.success(),
        "first client failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    ensure!(
        second.status.success(),
        "second client failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let first_events = String::from_utf8(first.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        first_events
            .iter()
            .any(|line| { line["type"] == "accepted" && line["tool_mode"] == "read_only" }),
        "read-only task mode missing from events: {first_events:?}"
    );
    Ok(())
}

#[tokio::test]
async fn local_contract_rejects_mismatched_server_version() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let home = tempfile::tempdir()?;
    let socket = home.path().join("wrong-version.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        tokio::io::BufReader::new(read).read_line(&mut line).await?;
        write
            .write_all(b"{\"version\":2,\"type\":\"capabilities\",\"operations\":[]}\n")
            .await?;
        Ok::<_, std::io::Error>(())
    });
    let error =
        bitrouter::agent_local::request(&socket, bitrouter::agent_local::Operation::Capabilities)
            .await
            .err()
            .context("version mismatch should fail")?;
    ensure!(error.to_string().contains("contract version"));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn authenticated_http_and_local_client_share_one_runtime() -> Result<()> {
    use bitrouter::agent_local::{Operation, ReplyResult};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_sse("Done."), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let home = tempfile::tempdir()?;
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace)?;
    let port_probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let api_address = port_probe.local_addr()?;
    drop(port_probe);
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\nagent_api:\n  enabled: true\n  listen: '{api_address}'\n  token_env: BRO_AGENT_API_TEST_TOKEN\n  workspaces: ['{}']\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models: [{{id: test-model}}]\n",
            workspace.display(),
            home.path().join("bitrouter.db").display(),
            upstream.uri()
        ),
    )?;
    let binary = env!("CARGO_BIN_EXE_bro");
    let mut server = Command::new(binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .env("BRO_AGENT_API_TEST_TOKEN", API_TOKEN)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let base = format!("http://{api_address}/agent/v1");
    let control = bitrouter::daemon::socket_path_for(
        &bitrouter::paths::ConfigSource::File(config.clone()),
        &bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(config.clone()))
            .await?,
    );
    let local_socket = bitrouter::agent_local::socket_path(&control);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if matches!(
            bitrouter::agent_local::request(&local_socket, Operation::Capabilities).await,
            Ok(ReplyResult::Capabilities { .. })
        ) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "task service did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let capabilities: Value = client
        .get(format!("{base}/capabilities"))
        .bearer_auth(API_TOKEN)
        .send()
        .await?
        .json()
        .await?;
    let instance = capabilities["runtime"]["server_instance_id"]
        .as_str()
        .context("server instance")?
        .to_string();
    let body = json!({"prompt": "Say done", "workspace": workspace, "model": "test-model"});
    let invalid_read_only = client
        .post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .header("Idempotency-Key", "read-only-with-check")
        .json(&json!({"prompt": "Inspect", "workspace": workspace, "model": "test-model", "read_only": true, "verification_command": "echo forbidden"}))
        .send()
        .await?;
    ensure!(invalid_read_only.status() == reqwest::StatusCode::BAD_REQUEST);
    let unauthorized = client
        .post(format!("{base}/tasks"))
        .header("Idempotency-Key", "same-task")
        .json(&body)
        .send()
        .await?;
    ensure!(unauthorized.status() == reqwest::StatusCode::UNAUTHORIZED);
    let accepted = client
        .post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .header("Idempotency-Key", "same-task")
        .json(&body)
        .send()
        .await?;
    ensure!(
        accepted.status() == reqwest::StatusCode::ACCEPTED,
        "API rejected authorized submit: {}",
        accepted.text().await?
    );
    let accepted: Value = accepted.json().await?;
    let task_id = accepted["task"]["task_id"]
        .as_str()
        .context("task id")?
        .to_string();
    let duplicate: Value = client
        .post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .header("Idempotency-Key", "same-task")
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    ensure!(duplicate["task"]["task_id"] == task_id);
    let conflict = client
        .post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .header("Idempotency-Key", "same-task")
        .json(&json!({"prompt": "Different", "workspace": workspace, "model": "test-model"}))
        .send()
        .await?;
    ensure!(conflict.status() == reqwest::StatusCode::CONFLICT);
    let stale = client
        .post(format!("{base}/tasks/{task_id}/inputs"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .json(&json!({"request_id": "stale", "approved": true}))
        .send()
        .await?;
    ensure!(stale.status() == reqwest::StatusCode::CONFLICT);
    let mut snapshot;
    loop {
        snapshot = match bitrouter::agent_local::request(
            &local_socket,
            Operation::Read {
                task_id: task_id.clone(),
            },
        )
        .await?
        {
            ReplyResult::Task { snapshot } => snapshot,
            _ => anyhow::bail!("unexpected local task reply"),
        };
        if matches!(
            snapshot.status,
            bitrouter_orchestrator::service::TaskStatus::Completed
        ) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "task did not finish: {:?}: {:?}",
            snapshot.status,
            snapshot.detail
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let local_events = match bitrouter::agent_local::request(
        &local_socket,
        Operation::Events {
            task_id: task_id.clone(),
            after: 1,
        },
    )
    .await?
    {
        ReplyResult::Events { events } => events,
        _ => anyhow::bail!("unexpected local event reply"),
    };
    let http_events: Value = client
        .get(format!("{base}/tasks/{task_id}/events?after=1"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        http_events["events"]
            .as_array()
            .is_some_and(|events| events.len() == local_events.len())
    );
    ensure!(http_events["events"].as_array().is_some_and(|events| {
        events.last().and_then(|event| event["seq"].as_u64()) == Some(snapshot.cursor)
    }));
    let resumed: Value = client
        .get(format!("{base}/tasks/{task_id}/events?after=2"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .send()
        .await?
        .json()
        .await?;
    ensure!(resumed["events"].as_array().is_some_and(|events| {
        events
            .iter()
            .all(|event| event["seq"].as_u64().is_some_and(|seq| seq > 2))
    }));
    let observed = client
        .get(format!("{base}/tasks/{task_id}/observe?after=1"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .send()
        .await?;
    ensure!(
        observed
            .headers()
            .get("content-type")
            .is_some_and(|header| header == "text/event-stream")
    );
    let stream_text = observed.text().await?;
    ensure!(
        stream_text.contains("snapshot")
            && stream_text.contains(&instance)
            && stream_text.contains(&task_id)
    );
    let stale_instance = client
        .post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", "previous-boot")
        .header("Idempotency-Key", "no-retry")
        .json(&body)
        .send()
        .await?;
    ensure!(stale_instance.status() == reqwest::StatusCode::CONFLICT);
    let stale_instance: Value = stale_instance.json().await?;
    ensure!(stale_instance["code"] == "instance_changed");
    let unlisted = home.path().join("local-only");
    std::fs::create_dir(&unlisted)?;
    let local_only = match bitrouter::agent_local::request(
        &local_socket,
        Operation::Submit {
            prompt: "Local inspection".into(),
            workspace: unlisted.clone(),
            model: "test-model".into(),
            effort: None,
            read_only: true,
            verification_command: None,
            idempotency_key: None,
        },
    )
    .await?
    {
        ReplyResult::Task { snapshot } => snapshot,
        _ => anyhow::bail!("unexpected local submit reply"),
    };
    // Registering a workspace locally must not expand the HTTP allowlist.
    let forbidden = client.post(format!("{base}/tasks"))
        .bearer_auth(API_TOKEN).header("X-Bro-Server-Instance", &instance)
        .header("Idempotency-Key", "outside-http-workspaces")
        .json(&json!({"prompt":"Inspect", "workspace":unlisted, "model":"test-model", "read_only":true}))
        .send().await?;
    ensure!(forbidden.status() == reqwest::StatusCode::FORBIDDEN);
    let forbidden_read = client
        .get(format!("{base}/tasks/{}", local_only.task_id))
        .bearer_auth(API_TOKEN)
        .header("X-Bro-Server-Instance", &instance)
        .send()
        .await?;
    ensure!(forbidden_read.status() == reqwest::StatusCode::FORBIDDEN);
    ensure!(!home.path().join("agent-tasks").exists());
    let _ = Command::new(binary)
        .arg("stop")
        .arg("--config")
        .arg(&config)
        .output()
        .await;
    let _ = server.wait().await;
    Ok(())
}
