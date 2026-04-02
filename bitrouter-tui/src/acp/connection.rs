use std::path::PathBuf;

use acp::Agent as _;
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::client::TuiClient;
use crate::acp::discovery::AgentLaunch;
use crate::event::AppEvent;

/// Command sent from the main event loop to the agent connection task.
pub(crate) enum AgentCommand {
    Prompt(String),
}

/// Handle to a running agent connection. The main event loop holds this to
/// send prompts and check liveness.
pub(crate) struct AgentConnection {
    pub command_tx: mpsc::Sender<AgentCommand>,
}

/// Spawn an agent subprocess on a dedicated thread with its own
/// single-threaded tokio runtime + `LocalSet` (required because ACP types
/// are `!Send`).
///
/// Returns the thread handle and the command sender for sending prompts.
pub(crate) fn spawn_agent(
    agent_name: String,
    launch: AgentLaunch,
    event_tx: mpsc::Sender<AppEvent>,
) -> (std::thread::JoinHandle<()>, mpsc::Sender<AgentCommand>) {
    let (command_tx, command_rx) = mpsc::channel::<AgentCommand>(32);

    let handle = std::thread::spawn(move || {
        // Build a single-threaded runtime for this agent connection.
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.blocking_send(AppEvent::AgentError {
                    agent_id: agent_name,
                    message: format!("failed to create runtime: {e}"),
                });
                return;
            }
        };

        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(agent_task_local(agent_name, launch, event_tx, command_rx)));
    });

    (handle, command_tx)
}

async fn agent_task_local(
    agent_name: String,
    launch: AgentLaunch,
    event_tx: mpsc::Sender<AppEvent>,
    mut command_rx: mpsc::Receiver<AgentCommand>,
) {
    if let Err(msg) = run_agent_connection(&agent_name, &launch, &event_tx, &mut command_rx).await {
        let _ = event_tx
            .send(AppEvent::AgentError {
                agent_id: agent_name.clone(),
                message: msg,
            })
            .await;
    }

    // Notify TUI that agent connection is gone.
    let _ = event_tx
        .send(AppEvent::AgentDisconnected {
            agent_id: agent_name,
        })
        .await;
}

async fn run_agent_connection(
    agent_name: &str,
    launch: &AgentLaunch,
    event_tx: &mpsc::Sender<AppEvent>,
    command_rx: &mut mpsc::Receiver<AgentCommand>,
) -> Result<(), String> {
    // 1. Spawn subprocess with the correct binary and args
    let mut child = tokio::process::Command::new(&launch.bin_path)
        .args(&launch.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
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

    // 2. Set up ACP connection (uses spawn_local because ACP is !Send)
    let client = TuiClient::new(agent_name.to_string(), event_tx.clone());
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
            acp::Implementation::new("bitrouter-tui", env!("CARGO_PKG_VERSION"))
                .title("BitRouter TUI"),
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

    let session_id = session_resp.session_id;

    // 5. Notify TUI that agent is connected (with session_id)
    let _ = event_tx
        .send(AppEvent::AgentConnected {
            agent_id: agent_name.to_string(),
            session_id: session_id.clone(),
        })
        .await;

    // 6. Prompt loop — receive commands from the TUI and forward to the agent
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
                            .send(AppEvent::PromptDone {
                                agent_id: agent_name.to_string(),
                                _stop_reason: resp.stop_reason,
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(AppEvent::AgentError {
                                agent_id: agent_name.to_string(),
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
