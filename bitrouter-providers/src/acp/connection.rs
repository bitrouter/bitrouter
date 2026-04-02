//! Agent subprocess management — spawn, protocol handshake, prompt loop.
//!
//! All ACP `!Send` types are confined to a dedicated OS thread running
//! a single-threaded tokio runtime with `LocalSet`.

use std::path::PathBuf;

use acp::Agent as _;
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::client::{AcpClient, convert_stop_reason};
use super::types::{AgentCommand, AgentEvent};

/// Spawn an agent subprocess on a dedicated OS thread.
///
/// Returns the thread handle and a command sender. The consumer
/// receives events through the `event_tx` channel.
pub(crate) fn spawn_agent_thread(
    agent_id: String,
    bin_path: PathBuf,
    args: Vec<String>,
    event_tx: mpsc::Sender<AgentEvent>,
) -> (std::thread::JoinHandle<()>, mpsc::Sender<AgentCommand>) {
    let (command_tx, command_rx) = mpsc::channel::<AgentCommand>(32);

    let handle = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.blocking_send(AgentEvent::Error {
                    agent_id,
                    message: format!("failed to create runtime: {e}"),
                });
                return;
            }
        };

        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(agent_task_local(
            agent_id, bin_path, args, event_tx, command_rx,
        )));
    });

    (handle, command_tx)
}

async fn agent_task_local(
    agent_id: String,
    bin_path: PathBuf,
    args: Vec<String>,
    event_tx: mpsc::Sender<AgentEvent>,
    mut command_rx: mpsc::Receiver<AgentCommand>,
) {
    if let Err(msg) =
        run_agent_connection(&agent_id, &bin_path, &args, &event_tx, &mut command_rx).await
    {
        let _ = event_tx
            .send(AgentEvent::Error {
                agent_id: agent_id.clone(),
                message: msg,
            })
            .await;
    }

    let _ = event_tx.send(AgentEvent::Disconnected { agent_id }).await;
}

async fn run_agent_connection(
    agent_id: &str,
    bin_path: &PathBuf,
    args: &[String],
    event_tx: &mpsc::Sender<AgentEvent>,
    command_rx: &mut mpsc::Receiver<AgentCommand>,
) -> Result<(), String> {
    // 1. Spawn subprocess
    let mut child = tokio::process::Command::new(bin_path)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn {agent_id}: {e}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{agent_id}: stdin not captured"))?
        .compat_write();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{agent_id}: stdout not captured"))?
        .compat();

    // 2. Set up ACP connection (spawn_local because ACP is !Send)
    let client = AcpClient::new(agent_id.to_string(), event_tx.clone());
    let (conn, io_future) = acp::ClientSideConnection::new(client, stdin, stdout, |fut| {
        tokio::task::spawn_local(fut);
    });

    // Drive I/O in the background
    tokio::task::spawn_local(async move {
        let _ = io_future.await;
    });

    // 3. Initialize
    conn.initialize(
        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
            acp::Implementation::new("bitrouter", env!("CARGO_PKG_VERSION")).title("BitRouter"),
        ),
    )
    .await
    .map_err(|e| format!("{agent_id} initialize failed: {e}"))?;

    // 4. Create session
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session_resp = conn
        .new_session(acp::NewSessionRequest::new(cwd))
        .await
        .map_err(|e| format!("{agent_id} new_session failed: {e}"))?;

    let session_id = session_resp.session_id;

    // 5. Notify consumer that the agent is connected
    let _ = event_tx
        .send(AgentEvent::Connected {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
        })
        .await;

    // 6. Prompt loop
    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            AgentCommand::Prompt(text) => {
                let result = conn
                    .prompt(acp::PromptRequest::new(
                        session_id.clone(),
                        vec![text.into()],
                    ))
                    .await;
                match result {
                    Ok(resp) => {
                        let _ = event_tx
                            .send(AgentEvent::PromptDone {
                                agent_id: agent_id.to_string(),
                                stop_reason: convert_stop_reason(resp.stop_reason),
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(AgentEvent::Error {
                                agent_id: agent_id.to_string(),
                                message: format!("prompt failed: {e}"),
                            })
                            .await;
                    }
                }
            }
        }
    }

    Ok(())
}
