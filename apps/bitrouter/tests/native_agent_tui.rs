//! Real terminal proof that bare `bro code` projects BRO task events and sends
//! identified input through the local task contract.

#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use bitrouter::agent_local::{Operation, ReplyResult};
use bitrouter_orchestrator::service::{TaskStatus, VerificationStatus};
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
}

impl TerminalClient {
    fn open(binary: &str, config: &std::path::Path, task_id: &str) -> Result<Self> {
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
        command.arg("--task-id");
        command.arg(task_id);
        command.arg("--config");
        command.arg(config);
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
            screen: vt100::Parser::new(24, 100, 0),
        })
    }

    fn wait_for(&mut self, needle: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.screen.screen().contents().contains(needle) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.output.recv_timeout(remaining) {
                Ok(bytes) => {
                    self.screen.process(&bytes);
                }
                Err(error) => anyhow::bail!(
                    "PTY output missing {needle:?}: {error}; screen: {:?}",
                    self.screen.screen().contents()
                ),
            }
        }
        anyhow::bail!(
            "PTY output missing {needle:?}; screen: {:?}",
            self.screen.screen().contents()
        )
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn close(mut self) -> Result<()> {
        self.send(b"\x04")?;
        let _ = self.child.wait()?;
        Ok(())
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
    let task_id = submit(
        &socket,
        &workspace,
        Some("test \"$(cat note.txt)\" = after".into()),
    )
    .await?;
    wait_status(&socket, &task_id, TaskStatus::WaitingForInput).await?;
    // Detach while there are no events to write: the server must observe EOF,
    // release each subscription, and keep the approval and task alive.
    for _ in 0..9 {
        let mut tui = TerminalClient::open(binary, &config, &task_id)?;
        tui.wait_for("Approve edit")?;
        tui.close()?;
        ensure!(
            wait_status(&socket, &task_id, TaskStatus::WaitingForInput)
                .await?
                .pending_input
                .is_some()
        );
    }
    let mut tui = TerminalClient::open(binary, &config, &task_id)?;
    tui.wait_for("Approve edit")?;
    tui.send(b"y")?;
    tui.wait_for("Approve bash")?;
    tui.send(b"y")?;
    let completed = wait_status(&socket, &task_id, TaskStatus::Completed).await?;
    ensure!(completed.verification == VerificationStatus::Passed);
    ensure!(std::fs::read_to_string(workspace.join("note.txt"))? == "after\n");
    tui.wait_for("Done in the TUI.")?;
    tui.close()?;

    let mut tui = TerminalClient::open(binary, &config, &task_id)?;
    tui.wait_for("status: completed")?;
    tui.close()?;

    std::fs::write(workspace.join("note.txt"), "before\n")?;
    let cancelled_id = submit(&socket, &workspace, None).await?;
    wait_status(&socket, &cancelled_id, TaskStatus::WaitingForInput).await?;
    let mut tui = TerminalClient::open(binary, &config, &cancelled_id)?;
    tui.wait_for("Approve edit")?;
    tui.send(b"\x03")?;
    wait_status(&socket, &cancelled_id, TaskStatus::Cancelled).await?;
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

async fn submit(
    socket: &std::path::Path,
    workspace: &std::path::Path,
    check: Option<String>,
) -> Result<String> {
    match bitrouter::agent_local::request(
        socket,
        Operation::Submit {
            prompt: "Change note.txt".into(),
            workspace: workspace.to_path_buf(),
            model: "test-model".into(),
            effort: None,
            read_only: false,
            verification_command: check,
            idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
        },
    )
    .await?
    {
        ReplyResult::Task { snapshot } => Ok(snapshot.task_id),
        _ => anyhow::bail!("unexpected submit reply"),
    }
}

async fn wait_status(
    socket: &std::path::Path,
    task_id: &str,
    wanted: TaskStatus,
) -> Result<bitrouter_orchestrator::service::TaskSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = match bitrouter::agent_local::request(
            socket,
            Operation::Read {
                task_id: task_id.into(),
            },
        )
        .await?
        {
            ReplyResult::Task { snapshot } => snapshot,
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
