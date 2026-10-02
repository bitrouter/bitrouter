//! Versioned OS-local task contract. This adapter only translates requests to
//! the BRO task service; it never advances the native model/tool loop itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::service::{
    ErrorCode, Observation, RuntimeCapabilities, TaskEvent, TaskRequest, TaskService, TaskSnapshot,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::daemon::transport;

pub const CONTRACT_VERSION: u32 = 13;
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
pub struct TaskCommand {
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
    Submit {
        prompt: String,
        workspace: PathBuf,
        model: String,
        effort: Option<ReasoningEffort>,
        #[serde(default)]
        read_only: bool,
        verification_command: Option<String>,
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    Read {
        task_id: String,
    },
    Events {
        task_id: String,
        after: u64,
    },
    Observe {
        task_id: String,
        after: Option<u64>,
    },
    Input {
        task_id: String,
        request_id: String,
        approved: bool,
    },
    Cancel {
        task_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskReply {
    pub version: u32,
    pub command_id: Option<String>,
    #[serde(flatten)]
    pub result: ReplyResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplyResult {
    Capabilities {
        operations: Vec<String>,
        runtime: Box<RuntimeCapabilities>,
    },
    Observation {
        observation: Box<Observation>,
    },
    Task {
        snapshot: Box<TaskSnapshot>,
    },
    Events {
        events: Vec<TaskEvent>,
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
    service: TaskService,
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
    let mut encoded = serde_json::to_vec(&TaskReply {
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

async fn serve_connection<S>(stream: S, service: TaskService) -> Result<()>
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
        serde_json::from_str::<TaskCommand>(&line).ok()
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
    if let Operation::Observe { task_id, after } = &command.operation
        && command.version == CONTRACT_VERSION
        && !command.command_id.is_empty()
        && command.command_id.len() <= 128
    {
        let subscription = service
            .ensure_instance(command.server_instance_id.as_deref())
            .and_then(|()| service.observe(task_id, *after));
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

async fn dispatch(service: &TaskService, command: TaskCommand) -> ReplyResult {
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
    let result = match command.operation {
        Operation::Capabilities => {
            return ReplyResult::Capabilities {
                runtime: Box::new(service.capabilities()),
                operations: ["submit", "read", "events", "observe", "input", "cancel"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            };
        }
        Operation::Submit {
            prompt,
            workspace,
            model,
            effort,
            read_only,
            verification_command,
            idempotency_key,
        } => match service.register_local_workspace(&workspace) {
            Ok(workspace) => service
                .submit(TaskRequest {
                    prompt,
                    workspace,
                    caller: CallerContext::local(),
                    config: if read_only {
                        AgentConfig::fixed(model, effort).read_only()
                    } else {
                        AgentConfig::fixed(model, effort)
                    },
                    verification_command,
                    idempotency_key,
                })
                .await
                .map(|snapshot| ReplyResult::Task {
                    snapshot: Box::new(snapshot),
                }),
            Err(error) => Err(error),
        },
        Operation::Read { task_id } => service.read(&task_id).map(|snapshot| ReplyResult::Task {
            snapshot: Box::new(snapshot),
        }),
        Operation::Events { task_id, after } => service
            .events_after(&task_id, after)
            .map(|events| ReplyResult::Events { events }),
        Operation::Input {
            task_id,
            request_id,
            approved,
        } => service
            .answer_input(&task_id, &request_id, approved)
            .await
            .map(|()| ReplyResult::Ok),
        Operation::Observe { .. } => Err("observe requires a streaming connection".into()),
        Operation::Cancel { task_id } => service.cancel(&task_id).await.map(|()| ReplyResult::Ok),
    };
    result.unwrap_or_else(|error| ReplyResult::Error {
        code: error.code,
        message: error.message,
    })
}

/// A client stays bound to the instance it negotiated. Network retries never
/// renegotiate identity or repeat a submit implicitly.
pub struct TaskClient {
    socket: PathBuf,
    pub server_instance_id: String,
}

impl TaskClient {
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

    pub async fn observe(&self, task_id: &str, after: Option<u64>) -> Result<TaskStream> {
        let stream = transport::connect(&self.socket).await?;
        let (read, mut write) = tokio::io::split(stream);
        let request_id = send_command(
            &mut write,
            Some(self.server_instance_id.clone()),
            Operation::Observe {
                task_id: task_id.into(),
                after,
            },
        )
        .await?;
        Ok(TaskStream {
            reader: Box::new(BufReader::new(read)),
            instance: self.server_instance_id.clone(),
            buffer: Vec::new(),
            request_id,
        })
    }
}

pub struct TaskStream {
    reader: Box<dyn tokio::io::AsyncBufRead + Send + Unpin>,
    instance: String,
    buffer: Vec<u8>,
    request_id: String,
}

impl TaskStream {
    pub async fn next(&mut self) -> Result<Option<Observation>> {
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
                    Observation::Snapshot { snapshot, .. } => &snapshot.server_instance_id,
                    Observation::Event { event } => &event.server_instance_id,
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
/// TaskClient so a restart is detected across operations.
pub async fn request(socket: &Path, operation: Operation) -> Result<ReplyResult> {
    if matches!(operation, Operation::Capabilities) {
        exchange(socket, None, operation).await
    } else {
        TaskClient::connect(socket).await?.request(operation).await
    }
}

async fn send_command(
    write: &mut (impl AsyncWrite + Unpin),
    instance: Option<String>,
    operation: Operation,
) -> Result<String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut encoded = serde_json::to_vec(&TaskCommand {
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
    let reply: TaskReply = serde_json::from_value(value)?;
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
        let command = TaskCommand {
            version: CONTRACT_VERSION,
            command_id: "control-request".into(),
            server_instance_id: Some("current-instance".into()),
            operation: Operation::Input {
                task_id: "task".into(),
                request_id: "approval-request".into(),
                approved: true,
            },
        };
        let decoded: TaskCommand = serde_json::from_str(&serde_json::to_string(&command)?)?;
        assert_eq!(decoded.command_id, "control-request");
        assert!(
            matches!(decoded.operation, Operation::Input { request_id, approved: true, .. } if request_id == "approval-request")
        );
        Ok(())
    }

    #[test]
    fn replies_are_correlated_to_the_request() -> Result<()> {
        let encoded = serde_json::to_string(&TaskReply {
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
        let mut stream = TaskStream {
            reader: Box::new(BufReader::new(read)),
            instance: "same-boot".into(),
            buffer: Vec::new(),
            request_id: "observe-request".into(),
        };
        let message = serde_json::to_vec(&TaskReply {
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
}
