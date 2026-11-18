//! Native stdio, gateway streaming and restart of the same on-disk BRO worker.
#[path = "support/binary.rs"]
mod binary;
#[path = "support/managed_model.rs"]
mod model;

use agent_client_protocol_schema::{v1, v2};
use anyhow::{Context, Result, ensure};
use model::{Model, answer, calls};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::atomic::Ordering, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

struct Peer {
    version: u64,
    child: Child,
    input: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    updates: Vec<Value>,
}
impl Peer {
    async fn start(home: &Path, endpoint: &str, version: u64) -> Result<Self> {
        Self::start_with(home, endpoint, version, None, false).await
    }

    async fn start_with(
        home: &Path,
        endpoint: &str,
        version: u64,
        workspace: Option<&Path>,
        read_only: bool,
    ) -> Result<Self> {
        let mut command = Command::new(binary::bro());
        command
            .arg("--data-dir")
            .arg(home)
            .args(["--endpoint", endpoint, "acp"]);
        if let Some(workspace) = workspace {
            command.arg("--workspace").arg(workspace);
        }
        command.arg("serve");
        if read_only {
            command.arg("--read-only");
        }
        let mut child = command
            .env_remove("BRO_HOME")
            .env("BITROUTER_API_KEY", "scoped-test-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("stdio input")?;
        let lines = BufReader::new(child.stdout.take().context("stdio output")?).lines();
        let mut peer = Self {
            version,
            child,
            input,
            lines,
            updates: vec![],
        };
        peer.rpc(0, "session/new", json!({})).await?;
        ensure!(peer.response(0).await?["error"]["code"] == -32002);
        peer.rpc(1,"initialize",json!({"protocolVersion":version,"clientCapabilities":{},"info":{"name":"test","version":"1"}})).await?;
        let initialized = peer.response(1).await?;
        ensure!(
            initialized["result"]["protocolVersion"] == version,
            "initialize: {initialized}"
        );
        if version == 1 {
            v1::InitializeResponse::deserialize(&initialized["result"])?;
        } else {
            v2::InitializeResponse::deserialize(&initialized["result"])?;
        }
        Ok(peer)
    }
    async fn send(&mut self, frame: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&frame)?;
        line.push(b'\n');
        self.input.write_all(&line).await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
    }
    async fn read(&mut self) -> Result<Value> {
        let line = timeout(Duration::from_secs(12), self.lines.next_line())
            .await??
            .context("native worker disconnected")?;
        let frame: Value = serde_json::from_str(&line)?;
        ensure!(frame["jsonrpc"] == "2.0", "worker wrote non-RPC output");
        match (self.version, frame["method"].as_str()) {
            (1, Some("session/update")) => {
                v1::SessionNotification::deserialize(&frame["params"])?;
            }
            (2, Some("session/update")) => {
                v2::UpdateSessionNotification::deserialize(&frame["params"])?;
            }
            (1, Some("session/request_permission")) => {
                v1::RequestPermissionRequest::deserialize(&frame["params"])?;
            }
            (2, Some("session/request_permission")) => {
                v2::RequestPermissionRequest::deserialize(&frame["params"])?;
            }
            _ => {}
        }
        Ok(frame)
    }
    async fn response(&mut self, id: u64) -> Result<Value> {
        loop {
            let frame = self.read().await?;
            if frame["id"] == id {
                return Ok(frame);
            }
            ensure!(
                frame["method"] == "session/update",
                "unexpected RPC {frame}"
            );
            self.updates.push(frame["params"]["update"].clone());
        }
    }
    async fn permission(&mut self) -> Result<Value> {
        loop {
            let f = self.read().await?;
            if f["method"] == "session/request_permission" {
                return Ok(f);
            }
            ensure!(f["method"] == "session/update", "expected permission: {f}");
        }
    }
    async fn close(mut self) -> Result<()> {
        drop(self.input);
        ensure!(
            timeout(Duration::from_secs(12), self.child.wait())
                .await??
                .success()
        );
        Ok(())
    }
}
fn prompt(id: &Value, text: &str, key: &str) -> Value {
    json!({"sessionId":id,"prompt":[{"type":"text","text":text}],"_meta":{"bitrouter":{"idempotencyKey":key}}})
}

#[tokio::test]
async fn native_stdio_streams_and_reopens_persisted_history_without_resending() -> Result<()> {
    let model = Model::start(|messages| {
        if messages.iter().any(|m| m["content"] == "second") {
            ensure!(
                messages
                    .iter()
                    .any(|m| m["role"] == "assistant" && m["content"] == "first answer")
            );
            Ok(answer("second answer"))
        } else {
            Ok(answer("first answer"))
        }
    })?;
    let root = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let mut peer = Peer::start(root.path(), &model.endpoint, 1).await?;
    peer.rpc(2, "session/new", json!({"cwd":work.path(),"mcpServers":[]}))
        .await?;
    let id = peer.response(2).await?["result"]["sessionId"].clone();
    ensure!(id.is_string());
    peer.rpc(3, "session/prompt", prompt(&id, "first", "first-key"))
        .await?;
    let response = peer.response(3).await?;
    ensure!(
        response["result"]["stopReason"] == "end_turn",
        "first prompt: {response}"
    );
    ensure!(
        peer.updates
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_message_chunk"
                && u["content"]["text"] == "first answer")
    );
    peer.close().await?;
    let mut peer = Peer::start(root.path(), &model.endpoint, 2).await?;
    peer.rpc(
        2,
        "session/resume",
        json!({"sessionId":id,"cwd":other.path(),"mcpServers":[]}),
    )
    .await?;
    ensure!(peer.response(2).await?["error"].is_object());
    peer.rpc(
        3,
        "session/resume",
        json!({"sessionId":id,"cwd":work.path(),"replayFrom":{"type":"start"}}),
    )
    .await?;
    ensure!(peer.response(3).await?["result"].is_object());
    ensure!(peer.updates.iter().any(
        |u| u["sessionUpdate"] == "agent_message" && u["content"][0]["text"] == "first answer"
    ));
    peer.rpc(30, "session/list", json!({"cwd":other.path()}))
        .await?;
    ensure!(peer.response(30).await?["result"]["sessions"] == json!([]));
    peer.rpc(31, "session/list", json!({"cwd":work.path()}))
        .await?;
    ensure!(peer.response(31).await?["result"]["sessions"][0]["sessionId"] == id);
    peer.rpc(
        32,
        "session/prompt",
        json!({"sessionId":id,"prompt":[{"type":"image","data":"AA==","mimeType":"image/png"}]}),
    )
    .await?;
    ensure!(peer.response(32).await?["error"].is_object());
    peer.rpc(33, "session/prompt", prompt(&id, "changed", "first-key"))
        .await?;
    ensure!(peer.response(33).await?["error"].is_object());
    peer.rpc(4, "session/prompt", prompt(&id, "first", "first-key"))
        .await?;
    ensure!(peer.response(4).await?["result"]["messageId"].is_string());
    peer.rpc(5, "session/prompt", prompt(&id, "second", "second-key"))
        .await?;
    ensure!(peer.response(5).await?["result"]["messageId"].is_string());
    loop {
        let f = peer.read().await?;
        if f["params"]["update"]["stopReason"] == "end_turn" {
            break;
        }
    }
    peer.close().await?;
    let mut restricted =
        Peer::start_with(root.path(), &model.endpoint, 2, Some(work.path()), true).await?;
    restricted
        .rpc(2, "session/new", json!({"cwd":other.path()}))
        .await?;
    ensure!(restricted.response(2).await?["error"].is_object());
    restricted
        .rpc(
            3,
            "session/resume",
            json!({"cwd":work.path(),"sessionId":id}),
        )
        .await?;
    ensure!(restricted.response(3).await?["error"].is_object());
    restricted
        .rpc(4, "session/new", json!({"cwd":work.path()}))
        .await?;
    ensure!(restricted.response(4).await?["result"]["sessionId"].is_string());
    restricted.close().await?;
    ensure!(model.peak.load(Ordering::SeqCst) == 1);
    ensure!(model.finish()?.len() == 2);
    Ok(())
}

#[tokio::test]
async fn cold_load_reissues_the_exact_permission_and_cancel_prevents_an_effect() -> Result<()> {
    let model = Model::start(|messages| {
        let last = messages.last().context("model input")?;
        if last["role"] == "tool" {
            return Ok(answer("write confirmed"));
        }
        Ok(calls(vec![(
            "call-w".into(),
            "write",
            json!({"path":"note","content":last["content"]}),
        )]))
    })?;
    let home = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let mut peer = Peer::start(home.path(), &model.endpoint, 2).await?;
    peer.rpc(2, "session/new", json!({"cwd":work.path()}))
        .await?;
    let id = peer.response(2).await?["result"]["sessionId"].clone();
    peer.rpc(3, "session/prompt", prompt(&id, "first", "write-one"))
        .await?;
    ensure!(peer.response(3).await?["result"]["messageId"].is_string());
    let old = peer.permission().await?;
    peer.rpc(4, "session/prompt", prompt(&id, "first", "write-one"))
        .await?;
    ensure!(peer.response(4).await?["result"]["messageId"].is_string());
    peer.rpc(5, "session/prompt", prompt(&id, "another", "new-key"))
        .await?;
    ensure!(peer.response(5).await?["error"].is_object());
    ensure!(old["params"]["subject"]["toolCall"]["title"] == "write");
    ensure!(!work.path().join("note").exists());
    peer.close().await?;
    let mut peer = Peer::start(home.path(), &model.endpoint, 1).await?;
    peer.rpc(2, "session/load", json!({"sessionId":id,"cwd":work.path()}))
        .await?;
    ensure!(peer.response(2).await?["result"].is_object());
    let callback = peer.permission().await?;
    ensure!(
        old["params"]["_meta"]["bitrouter"]["requestId"]
            == callback["params"]["_meta"]["bitrouter"]["requestId"]
    );
    ensure!(
        old["params"]["subject"]["toolCall"]["toolCallId"]
            == callback["params"]["toolCall"]["toolCallId"]
    );
    ensure!(old["id"] != callback["id"]);
    peer.send(json!({"jsonrpc":"2.0","id":old["id"],"result":{"outcome":{"outcome":"selected","optionId":"reject_once"}}})).await?;
    peer.send(json!({"jsonrpc":"2.0","id":callback["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await?;
    // A reconnect after the observed assistant output waits for durable completion.
    loop {
        let f = peer.read().await?;
        if f["params"]["update"]["content"]["text"] == "write confirmed" {
            break;
        }
    }
    peer.close().await?;
    ensure!(std::fs::read_to_string(work.path().join("note"))? == "first");
    let mut peer = Peer::start(home.path(), &model.endpoint, 1).await?;
    peer.rpc(
        2,
        "session/resume",
        json!({"sessionId":id,"cwd":work.path()}),
    )
    .await?;
    ensure!(peer.response(2).await?["result"].is_object());
    peer.rpc(3, "session/prompt", prompt(&id, "second", "write-two"))
        .await?;
    let callback = peer.permission().await?;
    peer.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
        .await?;
    ensure!(peer.response(3).await?["result"]["stopReason"] == "cancelled");
    peer.send(json!({"jsonrpc":"2.0","id":callback["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await?;
    peer.close().await?;
    ensure!(std::fs::read_to_string(work.path().join("note"))? == "first");
    let mut peer = Peer::start(home.path(), &model.endpoint, 2).await?;
    peer.rpc(
        2,
        "session/resume",
        json!({"sessionId":id,"cwd":work.path(),"replayFrom":{"type":"start"}}),
    )
    .await?;
    peer.response(2).await?;
    ensure!(
        peer.updates
            .iter()
            .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed")
    );
    peer.rpc(3, "session/prompt", prompt(&id, "denied", "write-three"))
        .await?;
    peer.response(3).await?;
    let callback = peer.permission().await?;
    peer.send(json!({"jsonrpc":"2.0","id":callback["id"],"result":{"outcome":{"outcome":"selected","optionId":"reject_once"}}})).await?;
    loop {
        let update = peer.read().await?;
        if update["params"]["update"]["stopReason"] == "end_turn" {
            break;
        }
    }
    peer.close().await?;
    ensure!(std::fs::read_to_string(work.path().join("note"))? == "first");
    let requests = model.finish()?;
    ensure!(requests.len() == 5);
    ensure!(
        requests[4]["messages"]
            .as_array()
            .and_then(|m| m.last())
            .and_then(|m| m["content"].as_str())
            .is_some_and(|s| s.contains("approval denied"))
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_a_paid_stream_keeps_its_receipt_and_blocks_cold_redispatch() -> Result<()> {
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let model = Model::start(move |_| {
        gate.lock()
            .map_err(|_| anyhow::anyhow!("fixture gate"))?
            .recv_timeout(Duration::from_secs(10))?;
        Ok(answer("late provider result"))
    })?;
    let home = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let mut peer = Peer::start(home.path(), &model.endpoint, 1).await?;
    peer.rpc(2, "session/new", json!({"cwd":work.path(),"mcpServers":[]}))
        .await?;
    let id = peer.response(2).await?["result"]["sessionId"].clone();
    let checkpoint = home
        .path()
        .join("threads")
        .join(format!("{}.json", id.as_str().context("session")?));
    peer.rpc(
        3,
        "session/prompt",
        prompt(&id, "wait for provider", "paid"),
    )
    .await?;
    // Wait until BRO has durably captured the actual HTTP response headers.
    let correlation = timeout(Duration::from_secs(8), async {
        loop {
            let thread: Value = serde_json::from_slice(&std::fs::read(&checkpoint)?)?;
            let step = &thread["turns"][0]["steps"][0];
            if step["execution_id"] == "exec-1" {
                return Ok::<_, anyhow::Error>(step["request_id"].clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    ensure!(correlation.is_string());
    peer.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
        .await?;
    let result = peer.response(3).await?;
    ensure!(result["result"]["stopReason"] == "cancelled");
    ensure!(result.pointer("/result/_meta/bitrouter/status") == Some(&json!("recovery_required")));
    release.send(())?;
    peer.close().await?;
    let mut peer = Peer::start(home.path(), &model.endpoint, 2).await?;
    peer.rpc(
        2,
        "session/resume",
        json!({"cwd":work.path(),"sessionId":id,"replayFrom":{"type":"start"}}),
    )
    .await?;
    ensure!(
        peer.response(2).await?["result"]["_meta"]["bitrouter"]["latestTurn"]["status"]
            == "recovery_required"
    );
    ensure!(peer.updates.iter().any(|u| u["state"] == "requires_action"));
    peer.rpc(
        3,
        "session/prompt",
        prompt(&id, "wait for provider", "paid"),
    )
    .await?;
    ensure!(peer.response(3).await?["result"]["messageId"].is_string());
    peer.rpc(
        4,
        "session/prompt",
        prompt(&id, "another request", "new-key"),
    )
    .await?;
    ensure!(peer.response(4).await?["error"].is_object());
    peer.close().await?;
    let thread: Value = serde_json::from_slice(&std::fs::read(checkpoint)?)?;
    let step = &thread["turns"][0]["steps"][0];
    ensure!(
        step["caller_request_id"] == correlation
            && step["request_id"] == correlation
            && step["execution_id"] == "exec-1"
    );
    ensure!(model.finish()?.len() == 1);
    Ok(())
}

#[tokio::test]
async fn manager_eof_during_stream_preserves_the_complete_message_for_cold_replay() -> Result<()> {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let (release, gate) = tokio::sync::oneshot::channel();
    let gateway = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut input = BufReader::new(socket);
        let mut headers = String::new();
        while !headers.ends_with("\r\n\r\n") {
            ensure!(input.read_line(&mut headers).await? > 0);
            ensure!(headers.len() < 16384);
        }
        let headers = headers.to_ascii_lowercase();
        ensure!(headers.starts_with("post /v1/chat/completions "));
        let field = |name: &str| headers.lines().find_map(|line| line.strip_prefix(name));
        let correlation = field("x-bitrouter-request-id: ").context("gateway correlation")?;
        let length: usize = field("content-length: ")
            .context("request length")?
            .parse()?;
        ensure!(length <= 512 * 1024);
        let mut body = vec![0; length];
        input.read_exact(&mut body).await?;
        let body: Value = serde_json::from_slice(&body)?;
        ensure!(
            body["messages"]
                .as_array()
                .and_then(|ms| ms.last())
                .and_then(|m| m["content"].as_str())
                .is_some_and(
                    |s| s.contains("ACP untrusted resource link") && s.contains("example.invalid")
                )
        );
        let mut socket = input.into_inner();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-BitRouter-Request-Id: {correlation}\r\nX-BitRouter-Execution-Id: exec-partial\r\nConnection: close\r\n\r\n").as_bytes()).await?;
        let chunk = |text: &str, reason: Option<&str>| {
            format!(
                "data: {}\n\n",
                json!({"id":"reply","model":"fixture","object":"chat.completion.chunk","created":1,"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":reason}]})
            )
        };
        socket.write_all(chunk("Déjà ", None).as_bytes()).await?;
        socket.flush().await?;
        gate.await?;
        socket
            .write_all(chunk("完成", Some("stop")).as_bytes())
            .await?;
        socket.write_all(b"data: [DONE]\n\n").await?;
        socket.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    });
    let home = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let mut peer = Peer::start(home.path(), &endpoint, 2).await?;
    peer.rpc(2, "session/new", json!({"cwd":work.path()}))
        .await?;
    let session = peer.response(2).await?["result"]["sessionId"].clone();
    peer.rpc(3,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"resource_link","uri":"https://example.invalid/reference","name":"reference"}]})).await?;
    let accepted = peer.response(3).await?;
    ensure!(accepted["result"]["messageId"].is_string());
    let message = loop {
        let f = peer.read().await?;
        let update = &f["params"]["update"];
        if update["sessionUpdate"] == "agent_message_chunk" {
            ensure!(update["content"]["text"] == "Déjà ");
            break update["messageId"].clone();
        }
    };
    ensure!(message.is_string());
    drop(peer.input);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("model already finished"))?;
    timeout(Duration::from_secs(12), gateway).await???;
    ensure!(
        timeout(Duration::from_secs(12), peer.child.wait())
            .await??
            .success()
    );
    let mut peer = Peer::start(home.path(), &endpoint, 1).await?;
    peer.rpc(
        2,
        "session/load",
        json!({"sessionId":session,"cwd":work.path(),"mcpServers":[]}),
    )
    .await?;
    let restored = peer.response(2).await?;
    ensure!(restored["result"]["_meta"]["bitrouter"]["latestTurn"]["status"] == "completed");
    let text: Vec<_> = peer
        .updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .collect();
    ensure!(
        text.len() == 1
            && text[0]["content"]["text"] == "Déjà 完成"
            && text[0]["_meta"]["bitrouter"]["itemId"] == message
    );
    peer.close().await
}
