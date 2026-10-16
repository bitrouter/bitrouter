//! Real terminal proof that bare `bro code` projects BRO task events and sends
//! identified input through the local task contract.

#![cfg(unix)]

use bitrouter::harness::agent::AgentConfigExt;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use bitrouter::agent_local::{Operation, ReplyResult};
use bitrouter_ai::harness::turn::TurnStatus;
use bitrouter_ai::harness::turn::VerificationStatus;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct TerminalClient {
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    screen: vt100::Parser,
    raw: Vec<u8>,
    master: Box<dyn portable_pty::MasterPty + Send>,
}

impl TerminalClient {
    fn open(binary: &str, config: &std::path::Path, execution: &Execution) -> Result<Self> {
        Self::open_at(binary, config, execution, None)
    }

    fn open_at(
        binary: &str,
        config: &std::path::Path,
        execution: &Execution,
        control: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::open_with_policy(binary, config, execution, control, false)
    }

    fn open_with_policy(
        binary: &str,
        config: &std::path::Path,
        execution: &Execution,
        control: Option<&std::path::Path>,
        policy: bool,
    ) -> Result<Self> {
        let pty = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(binary);
        command.arg("code");
        command.arg("--model");
        command.arg("test-model");
        if !execution.thread_id.is_empty() {
            command.arg("--thread-id");
            command.arg(&execution.thread_id);
        }
        command.arg("--config");
        command.arg(config);
        if let Some(control) = control {
            command.arg("--socket");
            command.arg(control);
        }
        if policy {
            command.arg("--model-policy");
            command.arg("--read-only");
            command.arg("--workspace");
            command.arg(
                config
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("fixture config has no parent"))?,
            );
        }
        let child = pty.slave.spawn_command(command)?;
        let writer = pty.master.take_writer()?;
        let mut reader = pty.master.try_clone_reader()?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            writer,
            output,
            child,
            screen: vt100::Parser::new(24, 100, 2000),
            raw: Vec::new(),
            master: pty.master,
        })
    }

    fn wait_for(&mut self, needle: &str) -> Result<()> {
        self.wait_for_presence(needle, true)
    }

    fn frame_complete(&self) -> bool {
        // PTY reads can split synchronized updates; observe only committed frames.
        // https://github.com/contour-terminal/vt-extensions/blob/master/synchronized-output.md
        let begin = self
            .raw
            .windows(b"\x1b[?2026h".len())
            .rposition(|window| window == b"\x1b[?2026h");
        let end = self
            .raw
            .windows(b"\x1b[?2026l".len())
            .rposition(|window| window == b"\x1b[?2026l");
        begin.is_none_or(|begin| end.is_some_and(|end| end > begin))
    }

    fn wait_for_presence(&mut self, needle: &str, present: bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.screen.screen().contents().contains(needle) == present && self.frame_complete()
            {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.output.recv_timeout(remaining) {
                Ok(bytes) => {
                    self.raw.extend_from_slice(&bytes);
                    self.screen.process(&bytes);
                }
                Err(error) => anyhow::bail!(
                    "PTY output did not reach {needle:?} presence={present}: {error}; screen: {:?}",
                    self.screen.screen().contents()
                ),
            }
        }
        anyhow::bail!(
            "PTY output did not reach {needle:?} presence={present}; screen: {:?}",
            self.screen.screen().contents()
        )
    }

    fn wait_for_raw_since(&mut self, offset: usize, needle: &[u8]) -> Result<usize> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(position) = self.raw[offset..]
                .windows(needle.len())
                .position(|window| window == needle)
            {
                return Ok(offset + position + needle.len());
            }
            let bytes = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            self.raw.extend_from_slice(&bytes);
            self.screen.process(&bytes);
        }
    }

    fn wait_for_frame_since(&mut self, offset: usize) -> Result<()> {
        let end = b"\x1b[?2026l";
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.raw[offset..]
                .windows(end.len())
                .any(|window| window == end)
                && self.frame_complete()
            {
                return Ok(());
            }
            let bytes = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            self.raw.extend_from_slice(&bytes);
            self.screen.process(&bytes);
        }
    }

    fn wait_for_spinner_change(&mut self, phase: &str) -> Result<()> {
        let glyphs = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let current = |screen: &vt100::Parser| {
            screen
                .screen()
                .contents()
                .lines()
                .find(|line| line.contains(phase))
                .and_then(|line| glyphs.iter().find(|glyph| line.contains(**glyph)).copied())
        };
        let first = current(&self.screen)
            .ok_or_else(|| anyhow::anyhow!("{phase} has no visible spinner"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if current(&self.screen).is_some_and(|glyph| glyph != first) && self.frame_complete() {
                return Ok(());
            }
            let bytes = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| {
                    anyhow::anyhow!(
                        "{phase} spinner did not change: {error}; screen: {:?}",
                        self.screen.screen().contents()
                    )
                })?;
            self.raw.extend_from_slice(&bytes);
            self.screen.process(&bytes);
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.screen.set_size(rows, cols);
        Ok(())
    }

    fn close(mut self) -> Result<()> {
        self.send(b"\x04")?;
        let _ = self.child.wait()?;
        Ok(())
    }
}

impl Drop for TerminalClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_approves_reattaches_and_cancels_server_tasks() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|request: &Request| {
            let saw_tool = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|body| body["messages"].as_array().cloned())
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            let (delta, reason) = if saw_tool {
                (json!({"role": "assistant", "content": "Done in the TUI."}), "stop")
            } else {
                (json!({"role": "assistant", "tool_calls": [{
                    "index": 0, "id": "edit-tui", "type": "function", "function": {
                        "name": "edit", "arguments": json!({"path": "note.txt", "edits": [{"oldText": "before", "newText": "after"}]}).to_string()
                    }
                }]}), "tool_calls")
            };
            let chunk = |delta, reason| format!("data: {}\n\n", json!({
                "id": "chatcmpl-tui", "object": "chat.completion.chunk", "model": "test-model",
                "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
            }));
            ResponseTemplate::new(200).set_body_raw(
                format!("{}{}data: [DONE]\n\n", chunk(delta, None::<&str>), chunk(json!({}), Some(reason))),
                "text/event-stream",
            )
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
            "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models: [{{id: test-model}}]\n",
            home.path().join("bitrouter.db").display(),
            upstream.uri()
        ),
    )?;
    let binary = env!("CARGO_BIN_EXE_bro");
    let mut server = Command::new(binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket =
        bitrouter::agent_local::socket_path(&bitrouter::daemon::socket_path_for(&source, &cfg));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(
            bitrouter::agent_local::request(&socket, Operation::Capabilities).await,
            Ok(ReplyResult::Capabilities { .. })
        ) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "server did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut fresh = TerminalClient::open(
        binary,
        &config,
        &Execution {
            thread_id: String::new(),
            turn_id: String::new(),
        },
    )?;
    fresh.wait_for("Ctrl-D detach")?;
    fresh.wait_for_frame_since(0)?;
    let initial = fresh.screen.screen().contents();
    ensure!(
        initial
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("  >_ BRO")),
        "banner did not start at the top: {initial:?}"
    );
    ensure!(
        initial
            .lines()
            .nth(20)
            .is_some_and(|line| line.starts_with("  › Ask")),
        "composer was not docked: {initial:?}"
    );
    ensure!(!fresh.screen.screen().alternate_screen());
    fresh.send("中文 draft".as_bytes())?;
    fresh.wait_for("中文 draft")?;
    fresh.resize(20, 60)?;
    fresh.send(b"\x0c")?;
    fresh.wait_for("中文 draft")?;
    fresh.send(b"\x15")?;
    fresh.close()?;
    let turn_id = submit(
        &socket,
        &workspace,
        Some("test \"$(cat note.txt)\" = after".into()),
    )
    .await?;
    wait_status(&socket, &turn_id, TurnStatus::WaitingForInput).await?;
    // Drop a real observation connection while preserving an unsubmitted draft,
    // a pending approval and a durable queue, then reconnect in this same TUI.
    let proxy_control = home.path().join("proxy.sock");
    let proxy_socket = bitrouter::agent_local::socket_path(&proxy_control);
    let listener = tokio::net::UnixListener::bind(&proxy_socket)?;
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (disconnect, _) = tokio::sync::watch::channel(0_u64);
    let proxy = tokio::spawn(observation_proxy(
        listener,
        socket.clone(),
        blocked.clone(),
        disconnect.clone(),
    ));
    let local = bitrouter::agent_local::ThreadClient::connect(&socket).await?;
    let queued = local
        .request(Operation::EnqueueTurn {
            thread_id: turn_id.thread_id.clone(),
            prompt: "queued before detach".into(),
            idempotency_key: "queue-before-detach".into(),
        })
        .await?;
    let ReplyResult::Receipt { receipt: queued } = queued else {
        anyhow::bail!("missing queued receipt");
    };
    let mut reconnecting =
        TerminalClient::open_at(binary, &config, &turn_id, Some(&proxy_control))?;
    reconnecting.wait_for("Approve edit")?;
    reconnecting.send(b"draft y n retained")?;
    reconnecting.wait_for("draft y n retained")?;
    blocked.store(true, std::sync::atomic::Ordering::SeqCst);
    disconnect.send(1)?;
    reconnecting.wait_for("OFFLINE")?;
    reconnecting.wait_for("draft y n retained")?;
    let pending = wait_status(&socket, &turn_id, TurnStatus::WaitingForInput).await?;
    ensure!(
        pending.pending_input.is_some(),
        "typing draft answered approval"
    );
    blocked.store(false, std::sync::atomic::Ordering::SeqCst);
    reconnecting.wait_for("APPROVAL")?;
    reconnecting.wait_for("draft y n retained")?;
    reconnecting.close()?;
    let ReplyResult::View { view } = local
        .request(Operation::ReadThread {
            thread_id: turn_id.thread_id.clone(),
        })
        .await?
    else {
        anyhow::bail!("missing Thread view");
    };
    ensure!(view.thread.queued.len() == 1 && view.thread.queued[0].turn_id == queued.turn_id);
    ensure!(
        view.latest_turn
            .as_ref()
            .is_some_and(|turn| turn.pending_input.is_some())
    );
    let requests = upstream
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing requests"))?;
    ensure!(
        !requests
            .iter()
            .any(|request| String::from_utf8_lossy(&request.body).contains("draft y n retained")),
        "detach submitted draft"
    );
    local
        .request(Operation::CancelQueuedTurn {
            thread_id: turn_id.thread_id.clone(),
            turn_id: queued.turn_id,
            idempotency_key: "withdraw-before-continuing".into(),
        })
        .await?;
    proxy.abort();
    let _ = proxy.await;
    // Detach while there are no events to write: the server must observe EOF,
    // release each subscription, and keep the approval and task alive.
    for _ in 0..9 {
        let mut tui = TerminalClient::open(binary, &config, &turn_id)?;
        tui.wait_for("Approve edit")?;
        tui.close()?;
        ensure!(
            wait_status(&socket, &turn_id, TurnStatus::WaitingForInput)
                .await?
                .pending_input
                .is_some()
        );
    }
    let mut tui = TerminalClient::open(binary, &config, &turn_id)?;
    tui.wait_for("Approve edit")?;
    tui.send(b"y")?;
    tui.wait_for("Approve shell")?;
    tui.send(b"y")?;
    let completed = wait_status(&socket, &turn_id, TurnStatus::Completed).await?;
    ensure!(completed.verification == VerificationStatus::Passed);
    ensure!(std::fs::read_to_string(workspace.join("note.txt"))? == "after\n");
    tui.wait_for("Done in the TUI.")?;
    tui.wait_for("DONE")?;
    ensure!(!tui.screen.screen().contents().contains("ToolResult {"));
    ensure!(
        tui.screen
            .screen()
            .contents()
            .matches("Done in the TUI.")
            .count()
            == 1
    );
    tui.send(b"Remember prior answer\r")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let reply = bitrouter::agent_local::request(
            &socket,
            Operation::ReadThread {
                thread_id: turn_id.thread_id.clone(),
            },
        )
        .await?;
        let ReplyResult::View { view } = reply else {
            anyhow::bail!("expected Thread view");
        };
        if view.latest_turn.as_ref().is_some_and(|turn| {
            turn.turn_id != turn_id.turn_id && turn.status == TurnStatus::WaitingForInput
        }) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "second verification approval did not arrive"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tui.wait_for("APPROVAL")?;
    tui.send(b"y")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let reply = bitrouter::agent_local::request(
            &socket,
            Operation::ReadThread {
                thread_id: turn_id.thread_id.clone(),
            },
        )
        .await?;
        let ReplyResult::View { view } = reply else {
            anyhow::bail!("expected Thread view");
        };
        if view.latest_turn.as_ref().is_some_and(|turn| {
            turn.turn_id != turn_id.turn_id && turn.status == TurnStatus::Completed
        }) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "second TUI turn did not settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let requests = upstream
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing model requests"))?;
    ensure!(
        requests.iter().any(
            |request| serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(
                |body| {
                    let text = body["messages"].to_string();
                    text.contains("Remember prior answer")
                        && text.contains("Done in the TUI.")
                        && text.contains("Change note.txt")
                }
            )
        ),
        "second TUI input lost settled first-turn context"
    );
    tui.close()?;

    let mut tui = TerminalClient::open(binary, &config, &turn_id)?;
    tui.wait_for("DONE")?;
    tui.close()?;

    std::fs::write(workspace.join("note.txt"), "before\n")?;
    let cancelled_id = submit(&socket, &workspace, None).await?;
    wait_status(&socket, &cancelled_id, TurnStatus::WaitingForInput).await?;
    let mut tui = TerminalClient::open(binary, &config, &cancelled_id)?;
    tui.wait_for("Approve edit")?;
    tui.send(b"\x03")?;
    wait_status(&socket, &cancelled_id, TurnStatus::Cancelled).await?;
    ensure!(std::fs::read_to_string(workspace.join("note.txt"))? == "before\n");
    tui.close()?;
    let _ = Command::new(binary)
        .arg("stop")
        .arg("--config")
        .arg(&config)
        .output()
        .await;
    let _ = server.wait().await;
    Ok(())
}

struct Execution {
    thread_id: String,
    turn_id: String,
}
async fn submit(
    socket: &std::path::Path,
    workspace: &std::path::Path,
    check: Option<String>,
) -> Result<Execution> {
    let client = bitrouter::agent_local::ThreadClient::connect(socket).await?;
    let (thread, turn) = client
        .create_and_start(
            workspace.into(),
            bitrouter_ai::harness::agent::AgentConfig::fixed("test-model", None),
            check,
            "Change note.txt".into(),
        )
        .await?;
    Ok(Execution {
        thread_id: thread.thread_id,
        turn_id: turn.turn_id,
    })
}

async fn wait_status(
    socket: &std::path::Path,
    execution: &Execution,
    wanted: TurnStatus,
) -> Result<bitrouter_ai::harness::turn::TurnSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = match bitrouter::agent_local::request(
            socket,
            Operation::ReadTurn {
                thread_id: execution.thread_id.clone(),
                turn_id: execution.turn_id.clone(),
            },
        )
        .await?
        {
            ReplyResult::Turn { snapshot } => snapshot,
            _ => anyhow::bail!("unexpected read reply"),
        };
        if snapshot.status == wanted {
            return Ok(*snapshot);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "task never reached {wanted:?}: {:?}",
            snapshot.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn observation_proxy(
    listener: tokio::net::UnixListener,
    upstream: std::path::PathBuf,
    blocked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    disconnect: tokio::sync::watch::Sender<u64>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            connection=listener.accept()=> {
                let (incoming,_)=connection?;
                let upstream=upstream.clone();let blocked=blocked.clone();let mut disconnect=disconnect.subscribe();
                connections.spawn(async move {
                    let mut incoming=tokio::io::BufReader::new(incoming);
                    let mut line=String::new();incoming.read_line(&mut line).await?;
                    let command:bitrouter::agent_local::ThreadCommand=serde_json::from_str(&line)?;
                    let observing=matches!(command.operation,Operation::Observe { .. });
                    if observing && blocked.load(std::sync::atomic::Ordering::SeqCst) { return Ok::<_,anyhow::Error>(()); }
                    let mut outgoing=tokio::net::UnixStream::connect(&upstream).await?;
                    outgoing.write_all(line.as_bytes()).await?;
                    if observing {
                        tokio::select! {
                            result=tokio::io::copy_bidirectional(&mut incoming,&mut outgoing)=> { result?; },
                            _=disconnect.changed()=> {},
                        }
                    } else { tokio::io::copy_bidirectional(&mut incoming,&mut outgoing).await?; }
                    Ok(())
                });
            },
            Some(_)=connections.join_next(),if !connections.is_empty()=> {},
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_agents_navigation_opens_durable_history_without_submitting() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(|_: &Request| ResponseTemplate::new(200).set_body_raw(format!("data: {}\n\ndata: [DONE]\n\n", json!({
            "id":"reply", "object":"chat.completion.chunk", "model":"test-model",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"Retained answer for this conversation."},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":4,"total_tokens":8}
        })), "text/event-stream")).mount(&upstream).await;
    let home = tempfile::tempdir()?;
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace)?;
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
    let mut server = Command::new(binary)
        .arg("serve")
        .arg("--managed-child")
        .arg("--config")
        .arg(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket =
        bitrouter::agent_local::socket_path(&bitrouter::daemon::socket_path_for(&source, &cfg));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(
            bitrouter::agent_local::request(&socket, Operation::Capabilities).await,
            Ok(ReplyResult::Capabilities { .. })
        ) {
            break;
        }
        ensure!(tokio::time::Instant::now() < deadline, "server unavailable");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = submit(&socket, &workspace, None).await?;
    wait_status(&socket, &first, TurnStatus::Completed).await?;
    let second = submit(&socket, &workspace, None).await?;
    wait_status(&socket, &second, TurnStatus::Completed).await?;
    let handoff = bitrouter::daemon::send_command(
        &bitrouter::daemon::socket_path_for(&source, &cfg),
        &bitrouter::daemon::DaemonCommand::HandoffPrepare,
    )
    .await?;
    ensure!(
        matches!(handoff, bitrouter::daemon::DaemonResponse::HandoffBusy { ref reason } if reason.contains("operations")),
        "resident native runtime did not block automatic daemon replacement: {handoff:?}"
    );
    let requests_before = upstream
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing requests"))?
        .len();
    let mut tui = TerminalClient::open(binary, &config, &first)?;
    tui.wait_for("Retained answer for this conversation.")?;
    tui.wait_for("← agents")?;
    tui.send(b"\x1b[D")?;
    tui.wait_for("Agents")?;
    tui.wait_for("Enter view")?;
    tui.resize(16, 40)?;
    tui.send(b"\x0c")?;
    tui.wait_for("Agents")?;
    tui.wait_for("Enter view")?;
    let pid = tui
        .child
        .process_id()
        .ok_or_else(|| anyhow::anyhow!("PTY process has no PID"))?;
    ensure!(
        Command::new("kill")
            .arg("-TSTP")
            .arg(pid.to_string())
            .status()
            .await?
            .success()
    );
    let stopped_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = Command::new("ps")
            .args(["-o", "state=", "-p"])
            .arg(pid.to_string())
            .output()
            .await?;
        if String::from_utf8_lossy(&status.stdout).contains('T') {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < stopped_deadline,
            "native UI did not suspend"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let frame_offset = tui.raw.len();
    // Resize while stopped, then wait for raw mode and the resumed frame before
    // sending keys, so input cannot be buffered by the restored canonical mode.
    tui.resize(24, 100)?;
    ensure!(
        Command::new("kill")
            .arg("-CONT")
            .arg(pid.to_string())
            .status()
            .await?
            .success()
    );
    let resume_frame = tui.wait_for_raw_since(frame_offset, b"\x1b[?2004h")?;
    tui.wait_for_frame_since(resume_frame)?;
    tui.wait_for("Enter view")?;
    let redraw_offset = tui.raw.len();
    tui.send(b"\x0c")?;
    tui.wait_for_frame_since(redraw_offset)?;
    tui.send(b"/")?;
    tui.wait_for("Agents · search:")?;
    tui.send(second.thread_id.as_bytes())?;
    tui.wait_for(&format!("{}▏", second.thread_id))?;
    // Acknowledge leaving the search editor before requesting the preview.
    // PTY writes and terminal redraws do not share message boundaries.
    tui.send(b"\r")?;
    tui.wait_for_presence(&format!("{}▏", second.thread_id), false)?;
    tui.send(b"\r")?;
    tui.wait_for("o Open conversation")?;
    tui.wait_for(&format!("Thread: {}", second.thread_id))?;
    ensure!(
        upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("missing requests"))?
            .len()
            == requests_before,
        "preview submitted or executed"
    );
    tui.send(b"o")?;
    tui.wait_for(&format!(
        "{} q:0",
        second
            .thread_id
            .get(..8)
            .ok_or_else(|| anyhow::anyhow!("short Thread ID"))?
    ))?;
    tui.wait_for("Retained answer for this conversation.")?;
    ensure!(
        upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("missing requests"))?
            .len()
            == requests_before,
        "opening history started a Turn"
    );
    ensure!(
        !tui.screen.screen().alternate_screen(),
        "native view entered alternate screen"
    );
    for forbidden in [
        b"\x1b[?1049h".as_slice(),
        b"\x1b[?1047h",
        b"\x1b[?47h",
        b"\x1b[3J",
    ] {
        ensure!(
            !tui.raw
                .windows(forbidden.len())
                .any(|window| window == forbidden),
            "native view cleared/replaced terminal history"
        );
    }
    tui.send(b"Continue the opened conversation\r")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let requests = upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("missing requests"))?;
        if let Some(last) = requests.last()
            && requests.len() > requests_before
        {
            let body: serde_json::Value = serde_json::from_slice(&last.body)?;
            let messages = body["messages"].to_string();
            ensure!(
                messages.contains("Continue the opened conversation")
                    && messages.contains("Retained answer for this conversation.")
                    && messages.contains("Change note.txt"),
                "opened Thread lost context"
            );
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "next Turn not submitted"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tui.close()?;
    let _ = Command::new(binary)
        .arg("stop")
        .arg("--config")
        .arg(&config)
        .output()
        .await;
    let _ = server.wait().await;
    Ok(())
}

#[tokio::test]
async fn native_status_bar_shows_classifier_workflow_model_and_live_spinner() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(|request: &Request| {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else { return ResponseTemplate::new(400) };
        let Some(questions) = body["questions"].as_object() else { return ResponseTemplate::new(400) };
        let answers = questions.iter().map(|(id, question)| {
            let choice = match id.as_str() { "routing_task_family" => "code:frontend_ui", "routing_next_role" => "implement", "routing_progress" => "progressing", _ => "full" };
            let probabilities = question["criteria"].as_object().into_iter().flat_map(|criteria| criteria.keys()).map(|key| (key.clone(), json!(if key == choice {1.0} else {0.0}))).collect::<serde_json::Map<_,_>>();
            (id.clone(), json!({"type":"choice","choice":choice,"confidence":0.99,"probabilities":probabilities}))
        }).collect::<serde_json::Map<_,_>>();
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(json!({"model":"ui-classifier", "answers":answers,"usage":{"input_tokens":12,"output_tokens":3}}))
    }).mount(&upstream).await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_raw(
        format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"id":"ui-generation","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"content":"Status bar complete."},"finish_reason":null}]}), json!({"id":"ui-generation","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}})), "text/event-stream")).mount(&upstream).await;
    let home = tempfile::tempdir()?;
    let config = home.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite://{}?mode=rwc'}}\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\ndecision_model:\n  model: ui-classifier\n  base_url: {}\n  api_key_env: BRO_UI_TEST_DECISION_KEY\n",
            home.path().join("state.db").display(),
            upstream.uri(),
            upstream.uri()
        ),
    )?;
    let binary = env!("CARGO_BIN_EXE_bro");
    let mut server = Command::new(binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .env("BRO_UI_TEST_DECISION_KEY", "fixture")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let source = bitrouter::paths::ConfigSource::File(config.clone());
    let cfg = bitrouter::paths::load_config(&source).await?;
    let socket =
        bitrouter::agent_local::socket_path(&bitrouter::daemon::socket_path_for(&source, &cfg));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(
            bitrouter::agent_local::request(&socket, Operation::Capabilities).await,
            Ok(ReplyResult::Capabilities { .. })
        ) {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "server did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut client = TerminalClient::open_with_policy(
        binary,
        &config,
        &Execution {
            thread_id: String::new(),
            turn_id: String::new(),
        },
        Some(&bitrouter::daemon::socket_path_for(&source, &cfg)),
        true,
    )?;
    client.wait_for("← agents")?;
    client
        .writer
        .write_all(b"Improve the frontend UI. Reply briefly without tools.\r")?;
    client.writer.flush()?;
    client.wait_for("CLASSIFYING")?;
    client.wait_for_spinner_change("CLASSIFYING")?;
    client.wait_for("GENERATING")?;
    client.wait_for("frontend/implement")?;
    client.wait_for("mock:test-model")?;
    if let Some(path) = std::env::var_os("BRO_UI_CAPTURE") {
        let mut cells = Vec::new();
        for row in 0..24 {
            for column in 0..100 {
                if let Some(cell) = client.screen.screen().cell(row, column) {
                    cells.push(json!({"row":row,"column":column,"text":cell.contents(),"foreground":format!("{:?}",cell.fgcolor()),"background":format!("{:?}",cell.bgcolor()),"bold":cell.bold()}));
                }
            }
        }
        std::fs::write(
            path,
            serde_json::to_vec(&json!({"columns":100,"rows":24,"cells":cells}))?,
        )?;
    }
    client.wait_for_spinner_change("GENERATING")?;
    client.wait_for("Status bar complete.")?;
    client.wait_for("DONE")?;
    ensure!(
        client
            .screen
            .screen()
            .contents()
            .contains("frontend/implement"),
        "final workflow lost"
    );
    client.writer.write_all(b"\x04")?;
    client.writer.flush()?;
    client.child.wait()?;
    server.start_kill()?;
    server.wait().await?;
    Ok(())
}
