//! Agent subprocess management — spawn, protocol handshake, prompt loop.
//!
//! All ACP `!Send` types are confined to a dedicated OS thread running
//! a single-threaded tokio runtime with `LocalSet`.

use std::path::PathBuf;

use acp::Agent as _;
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use bitrouter_core::agents::event::AgentEvent;
use bitrouter_core::agents::session::{AgentCapabilities, AgentSessionInfo};

use super::client::{AcpClient, PermissionBridge, convert_stop_reason};
use super::types::AgentCommand;

/// Result of the agent handshake, sent back to the caller of `connect`.
pub(crate) struct HandshakeResult {
    pub session_info: AgentSessionInfo,
    pub command_tx: mpsc::Sender<AgentCommand>,
}

/// Spawn an agent subprocess on a dedicated OS thread.
///
/// Returns a thread handle. The `handshake_tx` oneshot resolves once
/// the ACP initialize + new_session handshake completes (or fails).
///
/// `routing_env` is injected into the subprocess environment to redirect
/// the agent's LLM traffic through BitRouter's proxy.
pub(crate) fn spawn_agent_thread(
    agent_name: String,
    bin_path: PathBuf,
    args: Vec<String>,
    routing_env: std::collections::HashMap<String, String>,
    handshake_tx: tokio::sync::oneshot::Sender<Result<HandshakeResult, String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = handshake_tx.send(Err(format!("failed to create runtime: {e}")));
                return;
            }
        };

        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(agent_task_local(
            agent_name,
            bin_path,
            args,
            routing_env,
            handshake_tx,
        )));
    })
}

async fn agent_task_local(
    agent_name: String,
    bin_path: PathBuf,
    args: Vec<String>,
    routing_env: std::collections::HashMap<String, String>,
    handshake_tx: tokio::sync::oneshot::Sender<Result<HandshakeResult, String>>,
) {
    if let Err(msg) =
        run_agent_connection(&agent_name, &bin_path, &args, &routing_env, handshake_tx).await
    {
        tracing::error!(agent = %agent_name, "agent connection error: {msg}");
    }
}

async fn run_agent_connection(
    agent_name: &str,
    bin_path: &PathBuf,
    args: &[String],
    routing_env: &std::collections::HashMap<String, String>,
    handshake_tx: tokio::sync::oneshot::Sender<Result<HandshakeResult, String>>,
) -> Result<(), String> {
    // 1. Spawn subprocess with routing env vars injected
    let mut cmd = tokio::process::Command::new(bin_path);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    if !routing_env.is_empty() {
        cmd.envs(routing_env);
        tracing::debug!(
            agent = %agent_name,
            vars = ?routing_env.keys().collect::<Vec<_>>(),
            "injecting routing env vars"
        );
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {agent_name}: {e}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{agent_name}: stdin not captured"))?
        .compat_write();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{agent_name}: stdout not captured"))?
        .compat();

    // 2. Set up ACP connection (spawn_local because ACP is !Send)
    let permission_bridge = std::rc::Rc::new(PermissionBridge::new());
    let reply_tx_slot: std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<AgentEvent>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    let client = AcpClient::new(permission_bridge.clone(), reply_tx_slot.clone());
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
    .map_err(|e| format!("{agent_name} initialize failed: {e}"))?;

    // 4. Create session
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session_resp = conn
        .new_session(acp::NewSessionRequest::new(cwd))
        .await
        .map_err(|e| format!("{agent_name} new_session failed: {e}"))?;

    let session_id = session_resp.session_id.to_string();

    // 5. Send handshake result back to the caller
    let (command_tx, mut command_rx) = mpsc::channel::<AgentCommand>(32);

    let session_info = AgentSessionInfo {
        session_id: session_id.clone(),
        agent_name: agent_name.to_string(),
        capabilities: AgentCapabilities {
            supports_permissions: true,
            supports_thinking: true,
        },
    };

    if handshake_tx
        .send(Ok(HandshakeResult {
            session_info,
            command_tx,
        }))
        .is_err()
    {
        return Err("caller dropped before handshake completed".to_owned());
    }

    // 6. Command loop
    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            AgentCommand::Prompt { text, reply_tx } => {
                // Install the per-turn reply channel.
                *reply_tx_slot.borrow_mut() = Some(reply_tx.clone());

                let result = conn
                    .prompt(acp::PromptRequest::new(
                        session_resp.session_id.clone(),
                        vec![text.into()],
                    ))
                    .await;

                match result {
                    Ok(resp) => {
                        let _ = reply_tx
                            .send(AgentEvent::TurnDone {
                                stop_reason: convert_stop_reason(resp.stop_reason),
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = reply_tx
                            .send(AgentEvent::Error {
                                message: format!("prompt failed: {e}"),
                            })
                            .await;
                    }
                }

                // Clear the per-turn channel. Dropping the sender
                // closes the receiver naturally.
                *reply_tx_slot.borrow_mut() = None;
            }
            AgentCommand::RespondPermission {
                request_id,
                response,
            } => {
                permission_bridge.resolve(request_id, response);
            }
            AgentCommand::Disconnect => {
                break;
            }
        }
    }

    Ok(())
}
