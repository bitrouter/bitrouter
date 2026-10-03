//! Interactive Thread client; the server owns execution and durable context.
use crate::agent_local::{self, Operation, ReplyResult, ThreadClient};
use anyhow::Result;
use bitrouter_orchestrator::service::{ErrorCode, ServiceError, TurnSnapshot, TurnStatus};
use bitrouter_orchestrator::thread::{
    ThreadChange, ThreadEvent, ThreadObservation, ThreadStatus, ThreadView,
};
use bitrouter_sdk::language_model::Content;
use bitrouter_tui::editor::Edit;
use bitrouter_tui::native_agent::{NativeState, NativeView};
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct PendingSubmission {
    text: String,
    key: String,
    mode: SubmissionMode,
    uncertain: bool,
}
enum SubmissionMode {
    Start,
    Enqueue,
    Steer(String),
}
#[derive(Default)]
struct Session {
    thread_id: Option<String>,
    create_key: String,
    create_uncertain: bool,
    resume_key: Option<String>,
    pending: Option<PendingSubmission>,
}

pub async fn run(
    config: Option<&Path>,
    control_override: Option<&Path>,
    model_override: Option<String>,
    reattach: Option<String>,
    check: Option<String>,
    read_only: bool,
    workspace_override: Option<PathBuf>,
) -> Result<()> {
    let source = crate::paths::resolve_config(config)?;
    let cfg = crate::paths::load_config(&source).await?;
    let control_socket = control_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::daemon::socket_path_for(&source, &cfg));
    let socket = if control_override.is_some() {
        let socket = agent_local::socket_path(&control_socket);
        match agent_local::request(&socket, Operation::Capabilities).await? {
            ReplyResult::Capabilities { .. } => socket,
            _ => anyhow::bail!("unexpected capabilities reply"),
        }
    } else {
        agent_local::connect_or_start(&source, &control_socket).await?
    };
    let client = ThreadClient::connect(&socket).await?;
    let workspace = workspace_override.unwrap_or(std::env::current_dir()?);
    let mut state = NativeState {
        model: model_override.or(cfg.chat.model).unwrap_or_default(),
        status: "idle".into(),
        verification: "unavailable".into(),
        ..NativeState::default()
    };
    let mut session = Session {
        thread_id: reattach,
        create_key: uuid::Uuid::new_v4().to_string(),
        pending: None,
        ..Default::default()
    };
    let mut projection: Option<ThreadView> = None;
    let mut subscription = None;
    let mut cursor = 0;
    let mut reconnect = true;
    if let Some(id) = &session.thread_id {
        let view = read(&client, id).await?;
        state.push(format!("Reattached Thread: {id}"));
        update_view(&mut state, &view);
        subscription = Some(client.observe(id, Some(0)).await?);
        projection = Some(view);
    }
    let mut view = NativeView::open()?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    loop {
        view.draw(&state)?;
        tokio::select! {
            next = async { match subscription.as_mut() { Some(stream) => stream.next().await, None => std::future::pending().await } } => {
                match next {
                    Ok(Some(ThreadObservation::Snapshot { view:fresh, resynchronized, catchup })) => {
                        if resynchronized { state.push("History cache expired; current Thread refreshed"); }
                        for event in catchup { show_event(&mut state, &event); }
                        cursor = fresh.thread.cursor; update_view(&mut state, &fresh); projection = Some(*fresh);
                    },
                    Ok(Some(ThreadObservation::Event { event })) => {
                        cursor = event.seq; show_event(&mut state, &event);
                        if let Some(view) = projection.as_mut() { view.apply(&event); update_view(&mut state, view); }
                    },
                    Ok(Some(ThreadObservation::Live { event, .. })) => {
                        if let Some(view) = projection.as_mut() && let Some(turn) = &mut view.latest_turn && turn.turn_id == event.turn_id { turn.apply(&event); update_view(&mut state, view); }
                    },
                    Ok(None) => { subscription = None; state.status="disconnected".into(); state.pending_input_id=None; state.push("Connection lost; Thread remains on the server"); },
                    Err(error) => {
                        subscription = None;
                        reconnect = !error.downcast_ref::<ServiceError>().is_some_and(|error| matches!(error.code, ErrorCode::InstanceChanged | ErrorCode::UnknownThread));
                        state.status = if reconnect { "disconnected" } else { "server_instance_lost" }.into(); state.pending_input_id=None; state.push(error.to_string());
                    },
                }
            },
            _ = ticker.tick(), if subscription.is_none() && reconnect && session.thread_id.is_some() => {
                if let Some(id) = &session.thread_id { match client.observe(id, Some(cursor)).await { Ok(stream) => subscription=Some(stream), Err(error) => {
                    if error.downcast_ref::<ServiceError>().is_some_and(|error| matches!(error.code, ErrorCode::InstanceChanged | ErrorCode::UnknownThread)) {
                        reconnect=false; state.status="server_instance_lost".into();
                    }
                    state.push(format!("Reconnect pending: {error}"));
                } } }
            },
            next = events.next() => {
                let Some(next) = next else { break; };
                let event = next?;
                if handle_event(&client, &workspace, &check, read_only, &mut state, &mut session, &event).await? { break; }
            },
        }
    }
    Ok(())
}

async fn handle_event(
    client: &ThreadClient,
    workspace: &Path,
    check: &Option<String>,
    read_only: bool,
    state: &mut NativeState,
    session: &mut Session,
    event: &Event,
) -> Result<bool> {
    let steering = matches!(event, Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter);
    if let Event::Key(key) = event {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let (Some(thread_id), Some(turn_id)) = (&session.thread_id, &state.turn_id) {
                match client
                    .request(Operation::CancelTurn {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        idempotency_key: format!("cancel-{turn_id}"),
                    })
                    .await
                {
                    Ok(_) => state.push("Cancellation requested"),
                    Err(error) => state.push(error.to_string()),
                }
            } else {
                return Ok(true);
            }
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            if let Some(thread_id) = &session.thread_id {
                match client
                    .request(Operation::ResumeQueue {
                        thread_id: thread_id.clone(),
                        idempotency_key: session
                            .resume_key
                            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                            .clone(),
                    })
                    .await
                {
                    Ok(_) => {
                        session.resume_key = None;
                        state.push("Queue resume requested");
                    }
                    Err(error) => state.push(error.to_string()),
                }
            }
            return Ok(false);
        }
        if state.editor.is_empty()
            && let (Some(thread_id), Some(turn_id), Some(request_id)) =
                (&session.thread_id, &state.turn_id, &state.pending_input_id)
        {
            let approved = match key.code {
                KeyCode::Char('y') => Some(true),
                KeyCode::Char('n') => Some(false),
                _ => None,
            };
            if let Some(approved) = approved {
                match client
                    .request(Operation::Input {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        request_id: request_id.clone(),
                        approved,
                        idempotency_key: format!("{request_id}-{approved}"),
                    })
                    .await
                {
                    Ok(_) => {
                        state.pending_input_id = None;
                        state.push(if approved {
                            "Tool approved"
                        } else {
                            "Tool denied"
                        });
                    }
                    Err(error) => state.push(error.to_string()),
                }
                return Ok(false);
            }
        }
    }
    let edit = if steering {
        Edit::Submitted
    } else {
        state.edit(event)
    };
    match edit {
        Edit::Submitted => {
            let input = state.editor.text().to_owned();
            if input.trim().is_empty() {
                return Ok(false);
            }
            if matches!(
                state.status.as_str(),
                "disconnected" | "server_instance_lost"
            ) {
                state.push("Reconnect before submitting; draft retained");
                return Ok(false);
            }
            if state.model.is_empty() {
                state.model = input.trim().into();
                state.editor.clear();
                state.push(format!("Model selected: {}", state.model));
                return Ok(false);
            }
            if let Some(pending) = &session.pending
                && pending.text != input
            {
                state.push("Previous acceptance is unresolved; restore its input to retry with the same key");
                return Ok(false);
            }
            if session.pending.is_none() {
                let mode = if steering {
                    let Some(id) = &state.turn_id else {
                        state.push("No active Turn to steer");
                        return Ok(false);
                    };
                    SubmissionMode::Steer(id.clone())
                } else if matches!(
                    state.status.as_str(),
                    "accepted" | "running" | "waiting_for_input" | "busy" | "paused"
                ) {
                    SubmissionMode::Enqueue
                } else {
                    SubmissionMode::Start
                };
                session.pending = Some(PendingSubmission {
                    text: input.clone(),
                    key: uuid::Uuid::new_v4().to_string(),
                    mode,
                    uncertain: false,
                });
            }
            if session.thread_id.is_none() {
                match client
                    .request(Operation::CreateThread {
                        workspace: workspace.into(),
                        model: state.model.clone(),
                        effort: None,
                        read_only,
                        verification_command: check.clone(),
                        idempotency_key: session.create_key.clone(),
                    })
                    .await
                {
                    Ok(ReplyResult::Thread { snapshot }) => {
                        session.thread_id = Some(snapshot.thread_id.clone());
                        state.thread_id = Some(snapshot.thread_id);
                    }
                    Ok(_) => anyhow::bail!("unexpected create_thread reply"),
                    Err(error) => {
                        if known_rejection(&error) && !session.create_uncertain {
                            session.pending = None;
                            session.create_key = uuid::Uuid::new_v4().to_string();
                        } else {
                            session.create_uncertain = true;
                        }
                        state.push(format!("Create outcome: {error}; draft retained"));
                        return Ok(false);
                    }
                }
            }
            let pending = session
                .pending
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing pending input"))?;
            let id = session
                .thread_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing Thread identity"))?;
            let operation = match &pending.mode {
                SubmissionMode::Start => Operation::StartTurn {
                    thread_id: id,
                    prompt: input.clone(),
                    idempotency_key: pending.key.clone(),
                },
                SubmissionMode::Enqueue => Operation::EnqueueTurn {
                    thread_id: id,
                    prompt: input.clone(),
                    idempotency_key: pending.key.clone(),
                },
                SubmissionMode::Steer(turn_id) => Operation::Steer {
                    thread_id: id,
                    expected_turn_id: turn_id.clone(),
                    text: input.clone(),
                    idempotency_key: pending.key.clone(),
                },
            };
            match client.request(operation).await {
                Ok(ReplyResult::Receipt { receipt }) => {
                    if matches!(pending.mode, SubmissionMode::Start) {
                        state.turn_id = Some(receipt.turn_id.clone());
                        state.status = "accepted".into();
                    }
                    state.push(format!(
                        "Turn ID: {} ({:?})",
                        receipt.turn_id, receipt.status
                    ));
                }
                Ok(ReplyResult::Steering { receipt }) => {
                    state.push(format!("Steering received: {}", receipt.input_id));
                }
                Ok(_) => anyhow::bail!("unexpected input receipt"),
                Err(error) => {
                    state.push(format!("Acceptance outcome: {error}; draft retained"));
                    if known_rejection(&error) && !pending.uncertain {
                        session.pending = None;
                    } else if let Some(pending) = session.pending.as_mut() {
                        pending.uncertain = true;
                    }
                    return Ok(false);
                }
            }
            state.editor.clear();
            state.editor.push_history(input.clone());
            state.push(format!("You: {input}"));
            session.pending = None;
        }
        Edit::Ended | Edit::ExitRequested => return Ok(true),
        _ => {}
    }
    Ok(false)
}
async fn read(client: &ThreadClient, thread_id: &str) -> Result<ThreadView> {
    match client
        .request(Operation::ReadThread {
            thread_id: thread_id.into(),
        })
        .await?
    {
        ReplyResult::View { view } => Ok(*view),
        _ => anyhow::bail!("unexpected Thread view"),
    }
}
fn update_view(state: &mut NativeState, view: &ThreadView) {
    state.thread_id = Some(view.thread.thread_id.clone());
    state.model = view.thread.model.clone();
    if let Some(turn) = &view.latest_turn {
        state.turn_id = Some(turn.turn_id.clone());
        update_snapshot(state, turn);
    }
    if matches!(
        view.thread.status,
        ThreadStatus::Paused | ThreadStatus::RecoveryRequired
    ) {
        state.status = if view.thread.status == ThreadStatus::Paused {
            "paused"
        } else {
            "recovery_required"
        }
        .into();
    }
}
fn show_event(state: &mut NativeState, event: &ThreadEvent) {
    for change in &event.changes {
        let detail = match change {
            ThreadChange::AssistantResponse { message, .. } => message
                .content
                .iter()
                .filter_map(|part| match part {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            ThreadChange::AssistantInterrupted { detail, .. } => {
                format!("Assistant interrupted: {detail}")
            }
            ThreadChange::ToolIntent { call, .. } => {
                format!("Tool {} ({}) started", call.name, call.item_id)
            }
            ThreadChange::ToolResult {
                item_id, message, ..
            } => format!("Tool {item_id}: {:?}", message.content),
            ThreadChange::VerificationResult { evidence, .. } => {
                format!("Verification: {evidence:?}")
            }
            ThreadChange::TurnLifecycle { turn_id, lifecycle } => {
                format!("Turn {turn_id}: {lifecycle:?}")
            }
            ThreadChange::TurnQueued { receipt, .. } => {
                format!("Queued #{}: {}", receipt.queue_order, receipt.turn_id)
            }
            _ => continue,
        };
        state.push(detail.chars().take(800).collect::<String>());
    }
}
fn update_snapshot(state: &mut NativeState, snapshot: &TurnSnapshot) {
    state.model = snapshot.model.clone();
    let prior_pending = state.pending_input_id.clone();
    state.status = serde_json::to_value(snapshot.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    state.verification = serde_json::to_value(snapshot.verification)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    state.pending_input_id = snapshot.pending_input_id.clone();
    state.live = snapshot.live.as_ref().map(|live| {
        format!(
            "{}{}: {}",
            live.kind,
            if live.truncated {
                " (earlier output truncated)"
            } else {
                ""
            },
            live.text
        )
    });
    if snapshot.status == TurnStatus::WaitingForInput
        && prior_pending != snapshot.pending_input_id
        && let Some(input) = &snapshot.pending_input
    {
        state.push(format!(
            "Approve {} {}? (y/n)",
            input.tool_name, input.arguments
        ));
    }
}

fn known_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ServiceError>().is_some_and(|error| {
        matches!(
            error.code,
            ErrorCode::InvalidRequest
                | ErrorCode::Unauthorized
                | ErrorCode::Conflict
                | ErrorCode::ShuttingDown
        )
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn ambiguous_overload_preserves_draft_and_original_acceptance_key() -> Result<()> {
        let home = tempfile::tempdir()?;
        let socket = home.path().join("retry.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let mut original = None;
            for index in 0..4 {
                let (stream, _) = listener.accept().await?;
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(read).read_line(&mut line).await?;
                let command: agent_local::ThreadCommand = serde_json::from_str(&line)?;
                let result = if index == 0 {
                    ReplyResult::Capabilities {
                        runtime: Box::new(bitrouter_orchestrator::service::RuntimeCapabilities {
                            server_instance_id: "epoch".into(),
                            limits: Default::default(),
                            execution_ownership: None,
                            startup_discovery: None,
                        }),
                        operations: vec![],
                    }
                } else {
                    anyhow::ensure!(command.server_instance_id.as_deref() == Some("epoch"));
                    let Operation::StartTurn {
                        thread_id,
                        prompt,
                        idempotency_key,
                    } = command.operation
                    else {
                        anyhow::bail!("unexpected input operation");
                    };
                    anyhow::ensure!(thread_id == "thread" && prompt == "draft y");
                    if index == 1 {
                        original = Some(idempotency_key);
                        ReplyResult::Error {
                            code: ErrorCode::Overloaded,
                            message: "receipt readers full".into(),
                        }
                    } else if index == 2 {
                        anyhow::ensure!(original.as_ref() == Some(&idempotency_key));
                        ReplyResult::Error {
                            code: ErrorCode::Unauthorized,
                            message: "grant temporarily unavailable".into(),
                        }
                    } else {
                        anyhow::ensure!(original.as_ref() == Some(&idempotency_key));
                        ReplyResult::Receipt {
                            receipt: bitrouter_orchestrator::thread::TurnReceipt {
                                thread_id,
                                turn_id: "original-turn".into(),
                                queue_order: 1,
                                status: TurnStatus::Completed,
                            },
                        }
                    }
                };
                let reply = agent_local::ThreadReply {
                    version: agent_local::CONTRACT_VERSION,
                    command_id: Some(command.command_id),
                    result,
                };
                write.write_all(&serde_json::to_vec(&reply)?).await?;
                write.write_all(b"\n").await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let client = ThreadClient::connect(&socket).await?;
        let mut state = NativeState {
            model: "fixture-model".into(),
            status: "idle".into(),
            ..Default::default()
        };
        let mut session = Session {
            thread_id: Some("thread".into()),
            create_key: "create".into(),
            pending: None,
            ..Default::default()
        };
        for character in "draft y".chars() {
            state.edit(&Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            )));
        }
        let submit = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_event(
            &client,
            home.path(),
            &None,
            false,
            &mut state,
            &mut session,
            &submit,
        )
        .await?;
        assert_eq!(state.editor.text(), "draft y");
        assert!(session.pending.is_some());
        handle_event(
            &client,
            home.path(),
            &None,
            false,
            &mut state,
            &mut session,
            &submit,
        )
        .await?;
        assert_eq!(state.editor.text(), "draft y");
        assert!(session.pending.is_some());
        handle_event(
            &client,
            home.path(),
            &None,
            false,
            &mut state,
            &mut session,
            &submit,
        )
        .await?;
        assert!(state.editor.is_empty());
        assert!(session.pending.is_none());
        server.await??;
        Ok(())
    }
}
