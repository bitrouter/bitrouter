//! Versioned OS-local task contract. This adapter only translates requests to
//! the BRO task service; it never advances the native model/tool loop itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_ai::types::ReasoningEffort;
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::service::{ErrorCode, RuntimeCapabilities, ThreadService};
use bitrouter_orchestrator::turn::TurnSnapshot;
use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::daemon::transport;

use bitrouter_orchestrator::thread::{
    PermissionProfile, ThreadHistoryPage, ThreadHistoryRequest, ThreadObservation, ThreadRequest,
    ThreadSnapshot, ThreadTarget, ThreadView,
};
use bitrouter_orchestrator::turn::{
    ApprovalAnswer, CancelTurnRequest, SteeringReceipt, SteeringRequest, TurnReceipt, TurnRequest,
};

pub const CONTRACT_VERSION: u32 = 15;
const MAX_COMMAND_BYTES: u64 = 64 * 1024;
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

pub fn socket_path(control_socket: &Path) -> PathBuf {
    control_socket.with_extension("agent.sock")
}

pub async fn connect_or_start(
    source: &crate::paths::ConfigSource,
    control_socket: &Path,
) -> Result<PathBuf> {
    if crate::daemon::probe_status(control_socket).await?.is_none() {
        let log = source.home().join("bitrouter.log");
        match crate::daemon::start_and_wait(
            source,
            &log,
            Some(control_socket),
            std::time::Duration::from_secs(15),
        )
        .await?
        {
            crate::daemon::DaemonStartOutcome::Ready(_) => {}
            crate::daemon::DaemonStartOutcome::Exited { status, log_tail } => {
                anyhow::bail!("BRO server exited ({status}): {log_tail}")
            }
            crate::daemon::DaemonStartOutcome::NotReadyInTime { pid } => {
                anyhow::bail!("BRO server process {pid} did not become ready")
            }
        }
    }
    let socket = socket_path(control_socket);
    for _ in 0..50 {
        match request(&socket, Operation::Capabilities).await {
            Ok(ReplyResult::Capabilities { .. }) => return Ok(socket),
            Err(error) if error.to_string().contains("contract version") => return Err(error),
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    anyhow::bail!(
        "BRO task service did not become ready at {}",
        socket.display()
    )
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadCommand {
    pub version: u32,
    pub command_id: String,
    pub server_instance_id: Option<String>,
    #[serde(flatten)]
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Capabilities,
    ListThreads {
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    },
    CreateThread {
        workspace: PathBuf,
        model: String,
        effort: Option<ReasoningEffort>,
        #[serde(default)]
        read_only: bool,
        verification_command: Option<String>,
        idempotency_key: String,
    },
    ReadThread {
        thread_id: String,
    },
    UnloadThread {
        thread_id: String,
    },
    ReadTurn {
        thread_id: String,
        turn_id: String,
    },
    StartTurn {
        thread_id: String,
        prompt: String,
        idempotency_key: String,
    },
    EnqueueTurn {
        thread_id: String,
        prompt: String,
        idempotency_key: String,
    },
    History {
        thread_id: String,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    },
    Observe {
        thread_id: String,
        after: Option<u64>,
    },
    Input {
        thread_id: String,
        turn_id: String,
        request_id: String,
        approved: bool,
        idempotency_key: String,
    },
    CancelTurn {
        thread_id: String,
        turn_id: String,
        idempotency_key: String,
    },
    CancelQueuedTurn {
        thread_id: String,
        turn_id: String,
        idempotency_key: String,
    },
    Steer {
        thread_id: String,
        expected_turn_id: String,
        text: String,
        idempotency_key: String,
    },
    ResumeQueue {
        thread_id: String,
        idempotency_key: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadReply {
    pub version: u32,
    pub command_id: Option<String>,
    #[serde(flatten)]
    pub result: ReplyResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplyResult {
    Directory {
        page: bitrouter_orchestrator::thread::ThreadDirectoryPage,
    },
    Capabilities {
        operations: Vec<String>,
        runtime: Box<RuntimeCapabilities>,
    },
    Observation {
        observation: Box<ThreadObservation>,
    },
    Thread {
        snapshot: Box<ThreadSnapshot>,
    },
    View {
        view: Box<ThreadView>,
    },
    Turn {
        snapshot: Box<TurnSnapshot>,
    },
    Receipt {
        receipt: TurnReceipt,
    },
    Steering {
        receipt: SteeringReceipt,
    },
    History {
        page: ThreadHistoryPage,
    },
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
    UnsupportedVersion {
        supported: u32,
    },
}

pub(crate) async fn serve(
    mut listener: transport::ControlListener,
    service: ThreadService,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Ok(Err(error)) = result { tracing::debug!(%error, "task observer disconnected"); }
            }
            accepted = listener.accept() => {
                let stream = accepted?;
                if connections.len() >= 64 {
                    connections.spawn(async move {
                        let mut stream = stream;
                        write_reply(&mut stream, None, ReplyResult::Error { code: ErrorCode::Overloaded, message: "too many task connections".into() }).await
                    });
                    // Rejected connections also consume resources; wait for this
                    // bounded reply before accepting another connection.
                    let _ = connections.join_next().await;
                } else {
                    let service = service.clone();
                    connections.spawn(async move { serve_connection(stream, service).await });
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn write_reply(
    write: &mut (impl AsyncWrite + Unpin),
    request_id: Option<&str>,
    result: ReplyResult,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(&ThreadReply {
        version: CONTRACT_VERSION,
        command_id: request_id.map(str::to_string),
        result,
    })?;
    encoded.push(b'\n');
    anyhow::ensure!(
        encoded.len() as u64 <= MAX_REPLY_BYTES,
        "task reply is too large"
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        write.write_all(&encoded).await?;
        write.flush().await
    })
    .await??;
    Ok(())
}

async fn serve_connection<S>(stream: S, service: ThreadService) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read.take(MAX_COMMAND_BYTES + 1));
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await??;
    let command = if line.len() as u64 <= MAX_COMMAND_BYTES {
        serde_json::from_str::<ThreadCommand>(&line).ok()
    } else {
        None
    };
    let Some(command) = command else {
        return write_reply(
            &mut write,
            None,
            ReplyResult::Error {
                code: ErrorCode::InvalidRequest,
                message: "invalid or oversized task command".into(),
            },
        )
        .await;
    };
    if let Operation::Observe { thread_id, after } = &command.operation
        && command.version == CONTRACT_VERSION
        && !command.command_id.is_empty()
        && command.command_id.len() <= 128
    {
        let subscription = match target(&service, &command, thread_id) {
            Ok(target) => match service.load_thread(&target, &CallerContext::local()).await {
                Ok(_) => service.observe_thread(&target, &CallerContext::local(), *after),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match subscription {
            Ok(mut subscription) => {
                loop {
                    let observation = tokio::select! {
                        // EOF detaches even when a task is idle awaiting input.
                        _ = reader.read_u8() => break,
                        next = subscription.next() => match next? {
                            Some(observation) => observation,
                            None => break,
                        },
                    };
                    write_reply(
                        &mut write,
                        Some(&command.command_id),
                        ReplyResult::Observation {
                            observation: Box::new(observation),
                        },
                    )
                    .await?;
                }
                return Ok(());
            }
            Err(error) => {
                return write_reply(
                    &mut write,
                    Some(&command.command_id),
                    ReplyResult::Error {
                        code: error.code,
                        message: error.message,
                    },
                )
                .await;
            }
        }
    }
    let request_id = command.command_id.clone();
    write_reply(
        &mut write,
        Some(&request_id),
        dispatch(&service, command).await,
    )
    .await
}

async fn dispatch(service: &ThreadService, command: ThreadCommand) -> ReplyResult {
    if command.command_id.is_empty() || command.command_id.len() > 128 {
        return ReplyResult::Error {
            code: ErrorCode::InvalidRequest,
            message: "invalid request ID".into(),
        };
    }
    if command.version != CONTRACT_VERSION {
        return ReplyResult::UnsupportedVersion {
            supported: CONTRACT_VERSION,
        };
    }
    if !matches!(command.operation, Operation::Capabilities)
        && let Err(error) = service.ensure_instance(command.server_instance_id.as_deref())
    {
        return ReplyResult::Error {
            code: error.code,
            message: error.message,
        };
    }
    let caller = CallerContext::local();
    let epoch = command.server_instance_id.clone().unwrap_or_default();
    let make_target = |thread_id: String| ThreadTarget {
        thread_id,
        server_instance_id: epoch.clone(),
    };
    let result = match command.operation {
        Operation::Capabilities => {
            return ReplyResult::Capabilities {
                runtime: Box::new(service.capabilities()),
                operations: [
                    "list_threads",
                    "create_thread",
                    "read_thread",
                    "read_turn",
                    "start_turn",
                    "enqueue_turn",
                    "history",
                    "observe",
                    "input",
                    "cancel_turn",
                    "cancel_queued_turn",
                    "steer",
                    "resume_queue",
                    "unload_thread",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            };
        }
        Operation::ListThreads {
            after,
            cutoff,
            limit,
        } => service
            .list_threads(&epoch, &caller, after, cutoff, limit)
            .await
            .map(|page| ReplyResult::Directory { page }),
        Operation::CreateThread {
            workspace,
            model,
            effort,
            read_only,
            verification_command,
            idempotency_key,
        } => match service.register_local_workspace(&workspace) {
            Ok(workspace) => service
                .create_thread(
                    &epoch,
                    ThreadRequest {
                        caller,
                        workspace,
                        config: if read_only {
                            AgentConfig::fixed(model, effort).read_only()
                        } else {
                            AgentConfig::fixed(model, effort)
                        },
                        permission_profile: if read_only {
                            PermissionProfile::ReadOnly
                        } else {
                            PermissionProfile::Ask
                        },
                        verification_command,
                        idempotency_key,
                    },
                )
                .await
                .map(|snapshot| ReplyResult::Thread {
                    snapshot: Box::new(snapshot),
                }),
            Err(error) => Err(error),
        },
        Operation::ReadThread { thread_id } => service
            .read_stored_thread_view(&make_target(thread_id), &caller)
            .await
            .map(|view| ReplyResult::View {
                view: Box::new(view),
            }),
        Operation::UnloadThread { thread_id } => service
            .unload_thread(&make_target(thread_id), &caller)
            .await
            .map(|_| ReplyResult::Ok),
        Operation::ReadTurn { thread_id, turn_id } => service
            .read_stored_turn(&make_target(thread_id), &caller, &turn_id)
            .await
            .map(|snapshot| ReplyResult::Turn {
                snapshot: Box::new(snapshot),
            }),
        Operation::StartTurn {
            thread_id,
            prompt,
            idempotency_key,
        } => service
            .start_turn(
                &make_target(thread_id),
                &caller,
                TurnRequest {
                    prompt,
                    idempotency_key,
                },
            )
            .await
            .map(|receipt| ReplyResult::Receipt { receipt }),
        Operation::EnqueueTurn {
            thread_id,
            prompt,
            idempotency_key,
        } => service
            .enqueue_turn(
                &make_target(thread_id),
                &caller,
                TurnRequest {
                    prompt,
                    idempotency_key,
                },
            )
            .await
            .map(|receipt| ReplyResult::Receipt { receipt }),
        Operation::History {
            thread_id,
            after,
            cutoff,
            limit,
        } => service
            .thread_history(
                &make_target(thread_id),
                &caller,
                ThreadHistoryRequest {
                    after,
                    cutoff,
                    limit,
                },
            )
            .await
            .map(|page| ReplyResult::History { page }),
        Operation::Input {
            thread_id,
            turn_id,
            request_id,
            approved,
            idempotency_key,
        } => service
            .answer_thread_input(
                &make_target(thread_id),
                &caller,
                ApprovalAnswer {
                    turn_id,
                    request_id,
                    approved,
                    idempotency_key,
                },
            )
            .await
            .map(|_| ReplyResult::Ok),
        Operation::CancelTurn {
            thread_id,
            turn_id,
            idempotency_key,
        } => service
            .cancel_turn(
                &make_target(thread_id),
                &caller,
                CancelTurnRequest {
                    turn_id,
                    idempotency_key,
                },
            )
            .await
            .map(|_| ReplyResult::Ok),
        Operation::CancelQueuedTurn {
            thread_id,
            turn_id,
            idempotency_key,
        } => service
            .cancel_queued_turn(&make_target(thread_id), &caller, &turn_id, idempotency_key)
            .await
            .map(|receipt| ReplyResult::Receipt { receipt }),
        Operation::Steer {
            thread_id,
            expected_turn_id,
            text,
            idempotency_key,
        } => service
            .steer(
                &make_target(thread_id),
                &caller,
                SteeringRequest {
                    expected_turn_id,
                    text,
                    idempotency_key,
                },
            )
            .await
            .map(|receipt| ReplyResult::Steering { receipt }),
        Operation::ResumeQueue {
            thread_id,
            idempotency_key,
        } => service
            .resume_queue(&make_target(thread_id), &caller, idempotency_key)
            .await
            .map(|snapshot| ReplyResult::Thread {
                snapshot: Box::new(snapshot),
            }),
        Operation::Observe { .. } => Err("observe requires a streaming connection".into()),
    };
    result.unwrap_or_else(|error| ReplyResult::Error {
        code: error.code,
        message: error.message,
    })
}

fn target(
    service: &ThreadService,
    command: &ThreadCommand,
    thread_id: &str,
) -> std::result::Result<ThreadTarget, bitrouter_orchestrator::service::ServiceError> {
    service.ensure_instance(command.server_instance_id.as_deref())?;
    Ok(ThreadTarget {
        thread_id: thread_id.into(),
        server_instance_id: command.server_instance_id.clone().unwrap_or_default(),
    })
}

/// A client stays bound to the instance it negotiated. Network retries never
/// renegotiate identity or repeat a submit implicitly.
#[derive(Clone)]
pub struct ThreadClient {
    socket: PathBuf,
    pub server_instance_id: String,
}

impl ThreadClient {
    pub async fn connect(socket: &Path) -> Result<Self> {
        match exchange(socket, None, Operation::Capabilities).await? {
            ReplyResult::Capabilities { runtime, .. } => Ok(Self {
                socket: socket.into(),
                server_instance_id: runtime.server_instance_id,
            }),
            _ => anyhow::bail!("unexpected task capabilities reply"),
        }
    }

    pub async fn request(&self, operation: Operation) -> Result<ReplyResult> {
        exchange(
            &self.socket,
            Some(self.server_instance_id.clone()),
            operation,
        )
        .await
    }

    pub async fn create_and_start(
        &self,
        workspace: PathBuf,
        model: String,
        effort: Option<ReasoningEffort>,
        read_only: bool,
        verification_command: Option<String>,
        prompt: String,
    ) -> Result<(ThreadSnapshot, TurnReceipt)> {
        let request_key = uuid::Uuid::new_v4().to_string();
        let create_key = format!("{request_key}:create");
        let turn_key = format!("{request_key}:start");
        let create = || Operation::CreateThread {
            workspace: workspace.clone(),
            model: model.clone(),
            effort,
            read_only,
            verification_command: verification_command.clone(),
            idempotency_key: create_key.clone(),
        };
        let created = match self.request(create()).await {
            Ok(reply) => reply,
            Err(error) if uncertain(&error) => self.request(create()).await.with_context(|| format!("create outcome unknown; inspect original acceptance key {create_key}; original error: {error}"))?,
            Err(error) => return Err(error),
        };
        let ReplyResult::Thread { snapshot } = created else {
            anyhow::bail!("unexpected create_thread reply");
        };
        let operation = || Operation::StartTurn {
            thread_id: snapshot.thread_id.clone(),
            prompt: prompt.clone(),
            idempotency_key: turn_key.clone(),
        };
        let reply = match self.request(operation()).await {
            Ok(reply) => reply,
            Err(error) if uncertain(&error) => {
                self.request(operation()).await.with_context(|| format!("start outcome unknown; inspect Thread {} with acceptance key {turn_key}; original error: {error}", snapshot.thread_id))?
            },
            Err(error) => {
                let _ = self.request(Operation::UnloadThread { thread_id:snapshot.thread_id.clone() }).await;
                return Err(error.context(format!("Thread {} was created; start was rejected", snapshot.thread_id)));
            },
        };
        let ReplyResult::Receipt { receipt } = reply else {
            anyhow::bail!("unexpected start_turn reply");
        };
        Ok((*snapshot, receipt))
    }

    pub async fn observe(&self, thread_id: &str, after: Option<u64>) -> Result<ThreadStream> {
        let stream = transport::connect(&self.socket).await?;
        let (read, mut write) = tokio::io::split(stream);
        let request_id = send_command(
            &mut write,
            Some(self.server_instance_id.clone()),
            Operation::Observe {
                thread_id: thread_id.into(),
                after,
            },
        )
        .await?;
        Ok(ThreadStream {
            reader: Box::new(BufReader::new(read)),
            instance: self.server_instance_id.clone(),
            buffer: Vec::new(),
            request_id,
        })
    }
}

pub struct ThreadStream {
    reader: Box<dyn tokio::io::AsyncBufRead + Send + Unpin>,
    instance: String,
    buffer: Vec<u8>,
    request_id: String,
}

impl ThreadStream {
    pub async fn next(&mut self) -> Result<Option<ThreadObservation>> {
        // read_until retains partially read bytes if select! cancels next().
        // Limit each frame, not the lifetime of the stream.
        anyhow::ensure!(
            self.buffer.len() as u64 <= MAX_REPLY_BYTES,
            "task observation is too large"
        );
        let remaining = MAX_REPLY_BYTES.saturating_sub(self.buffer.len() as u64) + 1;
        let count = (&mut self.reader)
            .take(remaining)
            .read_until(b'\n', &mut self.buffer)
            .await?;
        if count == 0 {
            anyhow::ensure!(
                self.buffer.is_empty(),
                "task observation ended during a frame"
            );
            return Ok(None);
        }
        anyhow::ensure!(
            self.buffer.len() as u64 <= MAX_REPLY_BYTES,
            "task observation is too large"
        );
        let line = String::from_utf8(std::mem::take(&mut self.buffer))?;
        match decode_reply(&line, &self.request_id)? {
            ReplyResult::Observation { observation } => {
                let instance = match observation.as_ref() {
                    ThreadObservation::Snapshot { view, .. } => &view.thread.server_instance_id,
                    ThreadObservation::Event { event } => &event.server_instance_id,
                    ThreadObservation::Live { event, .. } => &event.server_instance_id,
                };
                anyhow::ensure!(
                    instance == &self.instance,
                    "server instance changed; do not automatically resubmit"
                );
                Ok(Some(*observation))
            }
            _ => anyhow::bail!("unexpected task observation reply"),
        }
    }
}

/// Convenience for isolated requests. Interactive clients must retain a
/// ThreadClient so a restart is detected across operations.
pub async fn request(socket: &Path, operation: Operation) -> Result<ReplyResult> {
    if matches!(operation, Operation::Capabilities) {
        exchange(socket, None, operation).await
    } else {
        ThreadClient::connect(socket)
            .await?
            .request(operation)
            .await
    }
}

fn uncertain(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<bitrouter_orchestrator::service::ServiceError>()
        .is_none_or(|failure| {
            matches!(
                failure.code,
                ErrorCode::StorageUnavailable
                    | ErrorCode::RecoveryRequired
                    | ErrorCode::InstanceChanged
                    | ErrorCode::Overloaded
            )
        })
}

async fn send_command(
    write: &mut (impl AsyncWrite + Unpin),
    instance: Option<String>,
    operation: Operation,
) -> Result<String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut encoded = serde_json::to_vec(&ThreadCommand {
        version: CONTRACT_VERSION,
        command_id: request_id.clone(),
        server_instance_id: instance,
        operation,
    })?;
    encoded.push(b'\n');
    anyhow::ensure!(
        encoded.len() as u64 <= MAX_COMMAND_BYTES,
        "task command is too large"
    );
    write.write_all(&encoded).await?;
    write.flush().await?;
    Ok(request_id)
}

async fn exchange(
    socket: &Path,
    instance: Option<String>,
    operation: Operation,
) -> Result<ReplyResult> {
    let stream = transport::connect(socket).await?;
    let (read, mut write) = tokio::io::split(stream);
    let request_id = send_command(&mut write, instance, operation).await?;
    let mut reader = BufReader::new(read.take(MAX_REPLY_BYTES + 1));
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    )
    .await??;
    anyhow::ensure!(!line.is_empty(), "task server closed without a reply");
    anyhow::ensure!(
        line.len() as u64 <= MAX_REPLY_BYTES,
        "task reply is too large"
    );
    decode_reply(&line, &request_id)
}

fn decode_reply(line: &str, request_id: &str) -> Result<ReplyResult> {
    // Check the envelope before decoding version-specific payloads.
    let value: serde_json::Value = serde_json::from_str(line).context("decode task reply")?;
    anyhow::ensure!(
        value["version"].as_u64() == Some(u64::from(CONTRACT_VERSION)),
        "task server contract version is incompatible with client version {CONTRACT_VERSION}"
    );
    let reply: ThreadReply = serde_json::from_value(value)?;
    anyhow::ensure!(
        reply.command_id.as_deref() == Some(request_id)
            || matches!(
                reply.result,
                ReplyResult::Error {
                    code: ErrorCode::Overloaded,
                    ..
                }
            ),
        "task reply request ID mismatch"
    );
    match reply.result {
        ReplyResult::Error { code, message } => Err(anyhow::Error::new(
            bitrouter_orchestrator::service::ServiceError { code, message },
        )),
        ReplyResult::UnsupportedVersion { supported } => anyhow::bail!(
            "task server supports contract version {supported}; client requires {CONTRACT_VERSION}"
        ),
        result => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_approval_ids_round_trip_independently() -> Result<()> {
        let command = ThreadCommand {
            version: CONTRACT_VERSION,
            command_id: "control-request".into(),
            server_instance_id: Some("current-instance".into()),
            operation: Operation::Input {
                thread_id: "thread".into(),
                turn_id: "turn".into(),
                request_id: "approval-request".into(),
                approved: true,
                idempotency_key: "answer-key".into(),
            },
        };
        let decoded: ThreadCommand = serde_json::from_str(&serde_json::to_string(&command)?)?;
        assert_eq!(decoded.command_id, "control-request");
        assert!(
            matches!(decoded.operation, Operation::Input { request_id, approved: true, .. } if request_id == "approval-request")
        );
        Ok(())
    }

    #[test]
    fn replies_are_correlated_to_the_request() -> Result<()> {
        let encoded = serde_json::to_string(&ThreadReply {
            version: CONTRACT_VERSION,
            command_id: Some("other-request".into()),
            result: ReplyResult::Ok,
        })?;
        assert!(decode_reply(&encoded, "expected-request").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_frame_read_keeps_partial_bytes() -> Result<()> {
        let (read, mut write) = tokio::io::duplex(4096);
        let mut stream = ThreadStream {
            reader: Box::new(BufReader::new(read)),
            instance: "same-boot".into(),
            buffer: Vec::new(),
            request_id: "observe-request".into(),
        };
        let message = serde_json::to_vec(&ThreadReply {
            version: CONTRACT_VERSION,
            command_id: Some("observe-request".into()),
            result: ReplyResult::Error {
                code: ErrorCode::InstanceChanged,
                message: "instance was lost".into(),
            },
        })?;
        let middle = message.len() / 2;
        write.write_all(&message[..middle]).await?;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), stream.next())
                .await
                .is_err()
        );
        assert!(!stream.buffer.is_empty());
        write.write_all(&message[middle..]).await?;
        write.write_all(b"\n").await?;
        let error = stream
            .next()
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("missing instance error"))?;
        assert!(
            error
                .downcast_ref::<bitrouter_orchestrator::service::ServiceError>()
                .is_some_and(|error| error.code == ErrorCode::InstanceChanged)
        );
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn lost_create_and_start_replies_retry_original_keys_and_epoch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("lost-replies.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let mut create_key = None;
            let mut turn_key = None;
            for index in 0..4 {
                let (stream, _) = listener.accept().await?;
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await?;
                let command: ThreadCommand = serde_json::from_str(&line)?;
                anyhow::ensure!(command.server_instance_id.as_deref() == Some("fixture-epoch"));
                match command.operation {
                    Operation::CreateThread {
                        idempotency_key, ..
                    } if index < 2 => {
                        if index == 0 {
                            create_key = Some(idempotency_key);
                            continue;
                        }
                        anyhow::ensure!(create_key.as_ref() == Some(&idempotency_key));
                        write_reply(
                            &mut write,
                            Some(&command.command_id),
                            ReplyResult::Thread {
                                snapshot: Box::new(ThreadSnapshot {
                                    server_instance_id: "fixture-epoch".into(),
                                    thread_id: "original-thread".into(),
                                    status: bitrouter_orchestrator::thread::ThreadStatus::Idle,
                                    workspace: PathBuf::from("/fixture"),
                                    model: "fixture-model".into(),
                                    permission_profile: PermissionProfile::ReadOnly,
                                    context_version: 0,
                                    cursor: 3,
                                    active_turn_id: None,
                                    queued: Vec::new(),
                                    pause_reason: None,
                                    waiting_for_capacity: false,
                                }),
                            },
                        )
                        .await?;
                    }
                    Operation::StartTurn {
                        thread_id,
                        idempotency_key,
                        prompt,
                    } if index >= 2 => {
                        anyhow::ensure!(
                            thread_id == "original-thread" && prompt == "original prompt"
                        );
                        if index == 2 {
                            turn_key = Some(idempotency_key);
                            continue;
                        }
                        anyhow::ensure!(turn_key.as_ref() == Some(&idempotency_key));
                        write_reply(
                            &mut write,
                            Some(&command.command_id),
                            ReplyResult::Receipt {
                                receipt: TurnReceipt {
                                    thread_id,
                                    turn_id: "original-turn".into(),
                                    queue_order: 1,
                                    status: bitrouter_orchestrator::turn::TurnStatus::Accepted,
                                },
                            },
                        )
                        .await?;
                    }
                    _ => anyhow::bail!("unexpected retry operation"),
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let client = ThreadClient {
            socket,
            server_instance_id: "fixture-epoch".into(),
        };
        let (thread, turn) = client
            .create_and_start(
                PathBuf::from("/fixture"),
                "fixture-model".into(),
                None,
                true,
                None,
                "original prompt".into(),
            )
            .await?;
        assert_eq!(thread.thread_id, "original-thread");
        assert_eq!(turn.turn_id, "original-turn");
        server.await??;
        Ok(())
    }
}
