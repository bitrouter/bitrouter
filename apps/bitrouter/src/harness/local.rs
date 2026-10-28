//! OS-local ThreadService listener and dispatch. The transport contracts and
//! client live in the application client layer. Session authority stays here.

use anyhow::Result;
use bitrouter_ai::harness::agent::AgentConfig;
use bitrouter_ai::harness::runtime::ErrorCode;
use bitrouter_ai::harness::thread::PermissionProfile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio_util::sync::CancellationToken;

use crate::client::agent_local::{
    CONTRACT_VERSION, MAX_COMMAND_BYTES, Operation, ReplyResult, ThreadCommand, write_reply,
};
use crate::daemon::transport;
use crate::gateway::caller::CallerContext;
use crate::harness::agent::AgentConfigExt;
use crate::harness::service::ThreadService;
use crate::harness::thread::{ThreadHistoryRequest, ThreadRequest, ThreadTarget};
use crate::harness::turn::{ApprovalAnswer, CancelTurnRequest, SteeringRequest, TurnRequest};

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
            let runtime = service.capabilities();
            let operations: &[&str] = if runtime.native_execution {
                &[
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
                    "load_thread",
                    "close_thread",
                    "recover_thread",
                ]
            } else {
                &[
                    "list_threads",
                    "read_thread",
                    "read_turn",
                    "history",
                    "observe",
                    "unload_thread",
                ]
            };
            return ReplyResult::Capabilities {
                runtime: Box::new(runtime),
                operations: operations.iter().map(|name| (*name).to_owned()).collect(),
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
            model_mode,
            effort,
            max_output_tokens,
            read_only,
            verification_command,
            idempotency_key,
            skills,
        } => match ThreadService::require_native_execution()
            .and_then(|()| service.register_local_workspace(&workspace))
        {
            Ok(workspace) => service
                .create_thread(
                    &epoch,
                    ThreadRequest {
                        caller,
                        workspace,
                        config: if read_only {
                            AgentConfig::fixed(model, effort)
                                .with_model_mode(model_mode)
                                .with_output_reservation(max_output_tokens)
                                .with_skill_constraints(skills)
                                .read_only()
                        } else {
                            AgentConfig::fixed(model, effort)
                                .with_model_mode(model_mode)
                                .with_output_reservation(max_output_tokens)
                                .with_skill_constraints(skills)
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
        Operation::LoadThread {
            thread_id,
            workspace,
        } => match service.register_local_workspace(&workspace) {
            Ok(workspace) => match service.load_thread(&make_target(thread_id), &caller).await {
                Ok(view) if view.thread.workspace == workspace => Ok(ReplyResult::View {
                    view: Box::new(view),
                }),
                Ok(_) => Err("Thread workspace cannot change on load".into()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        Operation::CloseThread {
            thread_id,
            idempotency_key,
        } => service
            .close_thread(&make_target(thread_id), &caller, idempotency_key)
            .await
            .map(|receipt| ReplyResult::Thread {
                snapshot: Box::new(receipt.snapshot),
            }),
        Operation::RecoverThread { thread_id, request } => service
            .recover_thread(&make_target(thread_id), &caller, request)
            .await
            .map(|view| ReplyResult::View {
                view: Box::new(view),
            }),
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
) -> std::result::Result<ThreadTarget, crate::harness::service::ServiceError> {
    service.ensure_instance(command.server_instance_id.as_deref())?;
    Ok(ThreadTarget {
        thread_id: thread_id.into(),
        server_instance_id: command.server_instance_id.clone().unwrap_or_default(),
    })
}

#[cfg(all(test, not(feature = "core")))]
mod core_disabled_tests;
