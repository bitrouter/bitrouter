//! Agent subprocess management — spawn, protocol handshake, prompt loop.
//!
//! All ACP `!Send` types are confined to a dedicated OS thread running
//! a single-threaded tokio runtime with `LocalSet`.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use acp::Agent as _;
use agent_client_protocol as acp;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use bitrouter_core::agents::event::AgentEvent;
use bitrouter_core::agents::session::{AgentCapabilities, AgentSessionInfo};

use super::client::{AcpClient, PermissionBridge, convert_stop_reason};
use super::types::AgentCommand;

/// Maximum bytes of stderr we keep around to attach to handshake
/// errors. The agent may print megabytes of warnings; we only need
/// the tail.
const STDERR_TAIL_CAP: usize = 4096;

/// Result of the agent handshake, sent back to the caller of `connect`
/// or `load_session`.
pub(crate) struct HandshakeResult {
    pub session_info: AgentSessionInfo,
    pub command_tx: mpsc::Sender<AgentCommand>,
}

/// How the spawned agent thread should establish its session: a fresh
/// `session/new` call, or a `session/load` against an existing
/// agent-native session id whose replay events are streamed into
/// `replay_tx`.
pub(crate) enum InitMode {
    New,
    Load {
        external_id: String,
        replay_tx: mpsc::Sender<AgentEvent>,
    },
}

/// Spawn an agent subprocess on a dedicated OS thread.
///
/// Returns a thread handle. The `handshake_tx` oneshot resolves once
/// the ACP initialize + new_session/load_session handshake completes
/// (or fails). `cwd` is used both as the subprocess's working
/// directory and as the `cwd` advertised in the ACP request.
pub(crate) fn spawn_agent_thread(
    agent_name: String,
    bin_path: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    init_mode: InitMode,
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
            cwd,
            init_mode,
            handshake_tx,
        )));
    })
}

async fn agent_task_local(
    agent_name: String,
    bin_path: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    init_mode: InitMode,
    handshake_tx: tokio::sync::oneshot::Sender<Result<HandshakeResult, String>>,
) {
    let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::with_capacity(STDERR_TAIL_CAP)));

    let result =
        run_agent_connection(&agent_name, &bin_path, &args, &cwd, init_mode, &stderr_tail).await;

    match result {
        AgentRunResult::HandshakeFailed(msg) => {
            let combined = combine_with_stderr(&msg, &stderr_tail);
            tracing::error!(agent = %agent_name, "{combined}");
            let _ = handshake_tx.send(Err(combined));
        }
        AgentRunResult::HandshakeOk { handshake, run } => {
            if handshake_tx.send(Ok(handshake)).is_err() {
                // Caller dropped before we returned the handshake;
                // nothing more to do — the child will be killed when
                // the run future is dropped (kill_on_drop).
                return;
            }
            if let Err(msg) = run.await {
                tracing::error!(agent = %agent_name, "agent connection error: {msg}");
            }
        }
    }
}

/// Outcome of `run_agent_connection`. We split the handshake from the
/// command loop so the caller can surface a useful error before the
/// channel is consumed: if anything fails before `session/new` (or
/// `session/load`) returns, the stderr tail is attached and sent
/// through `handshake_tx`.
enum AgentRunResult {
    HandshakeFailed(String),
    HandshakeOk {
        handshake: HandshakeResult,
        /// The post-handshake command loop. Awaiting it drives the
        /// subprocess until [`AgentCommand::Disconnect`] (or the
        /// command channel closes).
        run: Pin<Box<dyn Future<Output = Result<(), String>>>>,
    },
}

async fn run_agent_connection(
    agent_name: &str,
    bin_path: &PathBuf,
    args: &[String],
    cwd: &std::path::Path,
    init_mode: InitMode,
    stderr_tail: &Arc<Mutex<Vec<u8>>>,
) -> AgentRunResult {
    // 1. Spawn subprocess — inherit the caller's requested cwd so that
    //    filesystem tools (which use relative paths against the process
    //    cwd) agree with the `cwd` advertised to the agent below.
    //    Stderr is piped (not null) so we can echo it back to the user
    //    when the agent crashes during handshake.
    let mut child = match tokio::process::Command::new(bin_path)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return AgentRunResult::HandshakeFailed(format!("failed to spawn {agent_name}: {e}"));
        }
    };

    let stdin = match child.stdin.take() {
        Some(s) => s.compat_write(),
        None => {
            return AgentRunResult::HandshakeFailed(format!("{agent_name}: stdin not captured"));
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s.compat(),
        None => {
            return AgentRunResult::HandshakeFailed(format!("{agent_name}: stdout not captured"));
        }
    };
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_drain(stderr, stderr_tail.clone());
    }

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

    // 3. Initialize.
    let init_resp = match conn
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                acp::Implementation::new("bitrouter", env!("CARGO_PKG_VERSION")).title("BitRouter"),
            ),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return AgentRunResult::HandshakeFailed(format!("{agent_name} initialize failed: {e}"));
        }
    };
    let capabilities = capabilities_from_initialize(&init_resp);

    // 4. Establish the session. For Load, the replay receiver is
    //    installed on the client BEFORE the call so streamed
    //    `session/update` notifications route into it during replay;
    //    we emit a synthetic `HistoryReplayDone` once the request
    //    resolves so the caller can detect end-of-replay.
    let session_id_acp: acp::SessionId = match &init_mode {
        InitMode::New => match conn
            .new_session(acp::NewSessionRequest::new(cwd.to_path_buf()))
            .await
        {
            Ok(resp) => resp.session_id,
            Err(e) => {
                return AgentRunResult::HandshakeFailed(format!(
                    "{agent_name} new_session failed: {e}"
                ));
            }
        },
        InitMode::Load {
            external_id,
            replay_tx,
        } => {
            *reply_tx_slot.borrow_mut() = Some(replay_tx.clone());
            let id = acp::SessionId::new(external_id.clone());
            let result = conn
                .load_session(acp::LoadSessionRequest::new(id.clone(), cwd.to_path_buf()))
                .await;
            // Emit HistoryReplayDone before clearing the slot so the
            // caller sees a clean end-of-stream marker even if the
            // load_session call itself errored AFTER streaming part
            // of the history.
            let _ = replay_tx.send(AgentEvent::HistoryReplayDone).await;
            *reply_tx_slot.borrow_mut() = None;
            if let Err(e) = result {
                return AgentRunResult::HandshakeFailed(format!(
                    "{agent_name} load_session failed: {e}"
                ));
            }
            id
        }
    };
    let session_id = session_id_acp.to_string();

    // 5. Build the handshake reply and the post-handshake command loop.
    let (command_tx, command_rx) = mpsc::channel::<AgentCommand>(32);
    let session_info = AgentSessionInfo {
        session_id,
        agent_name: agent_name.to_string(),
        capabilities,
    };
    let handshake = HandshakeResult {
        session_info,
        command_tx,
    };
    let run = run_command_loop(
        conn,
        permission_bridge,
        reply_tx_slot,
        session_id_acp,
        command_rx,
        child,
    );
    AgentRunResult::HandshakeOk {
        handshake,
        run: Box::pin(run),
    }
}

/// Drive the post-handshake command loop until disconnect or channel
/// close. Owns the child process so it is killed when the future is
/// dropped (`kill_on_drop`).
async fn run_command_loop(
    conn: acp::ClientSideConnection,
    permission_bridge: std::rc::Rc<PermissionBridge>,
    reply_tx_slot: std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<AgentEvent>>>>,
    session_id_acp: acp::SessionId,
    mut command_rx: mpsc::Receiver<AgentCommand>,
    _child: tokio::process::Child,
) -> Result<(), String> {
    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            AgentCommand::Prompt { text, reply_tx } => {
                *reply_tx_slot.borrow_mut() = Some(reply_tx.clone());

                let result = conn
                    .prompt(acp::PromptRequest::new(
                        session_id_acp.clone(),
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

/// Spawn a task that drains the agent's stderr into a shared ring
/// buffer capped at [`STDERR_TAIL_CAP`] bytes. We only keep the tail
/// so callers can attach the most recent output to a handshake error.
fn spawn_stderr_drain(stderr: tokio::process::ChildStderr, buf: Arc<Mutex<Vec<u8>>>) {
    tokio::task::spawn_local(async move {
        let mut chunk = [0u8; 1024];
        let mut reader = stderr;
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let Ok(mut guard) = buf.lock() else { return };
                    guard.extend_from_slice(&chunk[..n]);
                    if guard.len() > STDERR_TAIL_CAP {
                        let drop_n = guard.len() - STDERR_TAIL_CAP;
                        guard.drain(..drop_n);
                    }
                }
            }
        }
    });
}

/// Combine a handshake error message with the captured stderr tail,
/// trimming and dropping non-UTF8 bytes. Returns `msg` unchanged when
/// the buffer is empty.
fn combine_with_stderr(msg: &str, buf: &Mutex<Vec<u8>>) -> String {
    let bytes = match buf.lock() {
        Ok(g) => g.clone(),
        Err(_) => return msg.to_string(),
    };
    if bytes.is_empty() {
        return msg.to_string();
    }
    let tail = String::from_utf8_lossy(&bytes);
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        msg.to_string()
    } else {
        format!("{msg}\n--- agent stderr ---\n{trimmed}")
    }
}

/// Read the bitrouter-side capability flags out of an ACP
/// `InitializeResponse`. `supports_permissions` and `supports_thinking`
/// don't have direct ACP wire equivalents — they're left at the
/// historical defaults (true) so existing behaviour is unchanged.
fn capabilities_from_initialize(resp: &acp::InitializeResponse) -> AgentCapabilities {
    let agent = &resp.agent_capabilities;
    AgentCapabilities {
        supports_permissions: true,
        supports_thinking: true,
        load_session: agent.load_session,
        prompt_image: agent.prompt_capabilities.image,
        prompt_audio: agent.prompt_capabilities.audio,
    }
}
