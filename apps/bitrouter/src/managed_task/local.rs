//! Bounded owner-local transport. Disconnecting a client never owns the driver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
#[cfg(all(feature = "core", feature = "harness"))]
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(all(feature = "core", feature = "harness"))]
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(all(feature = "core", feature = "harness"))]
use tokio_util::sync::CancellationToken;

#[cfg(all(feature = "core", feature = "harness"))]
use super::owner::{MAX_ACTIVE, Owner};
use super::protocol::{
    Command, MAX_COMMAND, MAX_REPLY, Operation, Reply, Report, ResultBody, VERSION,
};
use crate::daemon::transport;

#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) async fn serve(
    mut listener: transport::ControlListener,
    owner: Owner,
    instance: String,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    let serving: Result<()> = async {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Ok(Err(error)) = result { tracing::debug!(%error, "managed client disconnected"); }
                }
                accepted = listener.accept() => {
                    let stream = accepted?;
                    if connections.len() >= 64 { drop(stream); continue; }
                    let owner = owner.clone();
                    let instance = instance.clone();
                    connections.spawn(async move { connection(stream, owner, &instance).await });
                }
            }
        }
        Ok(())
    }.await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    listener.cleanup().await;
    serving
}

#[cfg(all(feature = "core", feature = "harness"))]
async fn connection(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    owner: Owner,
    instance: &str,
) -> Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(read.take(MAX_COMMAND + 1)).read_line(&mut line),
    )
    .await??;
    let command = if line.len() as u64 <= MAX_COMMAND && line.ends_with('\n') {
        serde_json::from_str::<Command>(&line).ok()
    } else {
        None
    };
    let mut command_id = None;
    let outcome = match command {
        Some(command) => {
            command_id = Some(command.command_id.clone());
            dispatch(&owner, instance, command).await
        }
        None => Err(CoreError::rejected(
            ErrorCode::UnsupportedCapability,
            "invalid or oversized managed command",
        )),
    };
    let reply = Reply {
        version: VERSION,
        command_id,
        daemon_instance_id: instance.into(),
        result: outcome.unwrap_or_else(|error| ResultBody::Error { error }),
    };
    let mut bytes = serde_json::to_vec(&reply)?;
    ensure!(
        bytes.len() as u64 <= MAX_REPLY,
        "managed reply exceeds its bound; inspect retained session"
    );
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        write.write_all(&bytes).await?;
        write.flush().await
    })
    .await??;
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
async fn dispatch(
    owner: &Owner,
    instance: &str,
    command: Command,
) -> Result<ResultBody, CoreError> {
    if command.version != VERSION {
        return Err(CoreError::rejected(
            ErrorCode::UnsupportedVersion,
            "managed local contract requires version 1",
        ));
    }
    crate::control_output::validate_id(&command.command_id)
        .map_err(|e| CoreError::rejected(ErrorCode::OperationConflict, e.to_string()))?;
    if matches!(command.operation, Operation::Hello) {
        return Ok(ResultBody::Hello {
            max_active_sessions: MAX_ACTIVE,
            skill_consent: true,
            skill_constraints: true,
        });
    }
    if command.daemon_instance_id.as_deref() != Some(instance) {
        return Err(CoreError::rejected(
            ErrorCode::StaleEpoch,
            "managed daemon instance changed; do not resubmit against a new boot",
        ));
    }
    let report = match command.operation {
        Operation::Submit(request) => owner.submit(request).await?,
        Operation::Status { name, operation_id } => {
            owner.status(&name, operation_id.as_deref()).await?
        }
        Operation::Cancel { name, operation_id } => owner.cancel(&name, &operation_id).await?,
        Operation::AnswerSkill {
            name,
            operation_id,
            answer_operation_id,
            answer,
        } => {
            owner
                .answer_skill(&name, &operation_id, &answer_operation_id, answer)
                .await?
        }
        Operation::Hello => {
            return Ok(ResultBody::Hello {
                max_active_sessions: MAX_ACTIVE,
                skill_consent: true,
                skill_constraints: true,
            });
        }
    };
    Ok(ResultBody::Report {
        report: Box::new(report),
    })
}

/// An exact boot is retained across every operation and transport retry.
#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
    pub daemon_instance_id: String,
    pub skill_consent: bool,
    pub skill_constraints: bool,
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Self> {
        let reply = exchange(socket, None, Operation::Hello).await
            .context("managed daemon endpoint unavailable; start/upgrade the selected daemon; embedded fallback is disabled")?;
        let ResultBody::Hello {
            skill_consent,
            skill_constraints,
            ..
        } = reply.result
        else {
            anyhow::bail!("unexpected managed capabilities reply");
        };
        Ok(Self {
            socket: socket.into(),
            daemon_instance_id: reply.daemon_instance_id,
            skill_consent,
            skill_constraints,
        })
    }

    pub async fn request(&self, operation: Operation) -> Result<Report> {
        let reply = exchange(&self.socket, Some(&self.daemon_instance_id), operation).await?;
        match reply.result {
            ResultBody::Report { report } => Ok(*report),
            ResultBody::Error { error } => Err(error.into()),
            _ => anyhow::bail!("unexpected managed daemon reply"),
        }
    }
}

async fn exchange(socket: &Path, instance: Option<&str>, operation: Operation) -> Result<Reply> {
    let command_id = uuid::Uuid::new_v4().to_string();
    let command = Command {
        version: VERSION,
        command_id: command_id.clone(),
        daemon_instance_id: instance.map(str::to_owned),
        operation,
    };
    let mut bytes = serde_json::to_vec(&command)?;
    ensure!(
        bytes.len() as u64 <= MAX_COMMAND,
        "managed command exceeds 4 MiB"
    );
    bytes.push(b'\n');
    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = transport::connect(socket).await?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        let mut line = String::new();
        BufReader::new(stream.take(MAX_REPLY + 1))
            .read_line(&mut line)
            .await?;
        ensure!(
            line.len() as u64 <= MAX_REPLY && line.ends_with('\n'),
            "managed reply missing or oversized; operation outcome may be unknown"
        );
        let reply: Reply = serde_json::from_str(&line)?;
        ensure!(
            reply.version == VERSION,
            "unsupported managed daemon contract version"
        );
        ensure!(
            reply.command_id.as_deref() == Some(command_id.as_str()),
            "managed reply belongs to another command"
        );
        ensure!(
            instance.is_none_or(|expected| expected == reply.daemon_instance_id),
            "managed daemon boot changed; inspect original operation without resubmitting"
        );
        Ok::<_, anyhow::Error>(reply)
    })
    .await
    .context("managed transport timed out; inspect the original operation before retrying")??;
    Ok(reply)
}
