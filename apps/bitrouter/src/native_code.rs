//! BRO's interactive local task client. The server owns every model and tool
//! effect; this module keeps only a projection and a prompt editor.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use bitrouter_orchestrator::service::{
    ErrorCode, Observation, ServiceError, TaskEvent, TaskEventPayload, TaskSnapshot, TaskStatus,
};
use bitrouter_sdk::language_model::Content;
use bitrouter_tui::editor::Edit;
use bitrouter_tui::native_agent::{NativeState, NativeView};
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;

use crate::agent_local::{self, Operation, ReplyResult, TaskClient, TaskStream};

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
            _ => anyhow::bail!("BRO task server returned an unexpected capability reply"),
        }
    } else {
        agent_local::connect_or_start(&source, &control_socket).await?
    };
    let client = TaskClient::connect(&socket).await?;
    let workspace = workspace_override.unwrap_or(std::env::current_dir()?);
    let mut state = NativeState {
        model: model_override.or(cfg.chat.model).unwrap_or_default(),
        status: "idle".into(),
        verification: "unavailable".into(),
        ..NativeState::default()
    };
    let mut cursor = 0;
    let mut projection = None;
    let mut subscription: Option<TaskStream> = None;
    let mut reconnect = true;
    if let Some(task_id) = reattach {
        let snapshot = read(&client, &task_id).await?;
        state.push(format!("Reattached task: {task_id}"));
        state.push(format!("Tool mode: {:?}", snapshot.tool_mode));
        state.task_id = Some(task_id.clone());
        update_snapshot(&mut state, &snapshot);
        subscription = Some(client.observe(&task_id, Some(0)).await?);
        projection = Some(snapshot);
    }
    let mut view = NativeView::open()?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    loop {
        view.draw(&state)?;
        tokio::select! {
            next = async { match subscription.as_mut() {
                Some(stream) => stream.next().await,
                None => std::future::pending().await,
            } } => {
                match next {
                    Ok(Some(Observation::Snapshot { snapshot, resynchronized, catchup })) => {
                        if resynchronized { state.push("Event cache expired; refreshed current task state"); }
                        let omitted_history = resynchronized || catchup.is_empty();
                        for event in catchup {
                            if !matches!(event.payload, TaskEventPayload::AssistantDelta { .. } | TaskEventPayload::ToolOutputDelta { .. }) { state.push(format_event(&event)); }
                        }
                        if snapshot.status.terminal() && omitted_history && let Some(answer) = &snapshot.final_answer {
                            state.push(answer.chars().take(800).collect::<String>());
                        }
                        cursor = snapshot.cursor;
                        update_snapshot(&mut state, &snapshot);
                        projection = Some(*snapshot);
                    }
                    Ok(Some(Observation::Event { event })) => {
                        cursor = event.seq;
                        if !matches!(event.payload, TaskEventPayload::AssistantDelta { .. } | TaskEventPayload::ToolOutputDelta { .. }) { state.push(format_event(&event)); }
                        if let Some(snapshot) = projection.as_mut() {
                            snapshot.apply(&event);
                            update_snapshot(&mut state, snapshot);
                        }
                    }
                    Ok(None) => {
                        subscription = None;
                        if projection.as_ref().is_some_and(|snapshot| !snapshot.status.terminal()) {
                            state.status = "disconnected".into(); state.pending_input_id = None;
                            state.push("Connection lost; task may still be running on the server");
                        }
                    }
                    Err(error) => {
                        subscription = None;
                        reconnect = !error.downcast_ref::<ServiceError>().is_some_and(|error| matches!(error.code, ErrorCode::InstanceChanged | ErrorCode::UnknownTask));
                        state.status = if reconnect { "disconnected" } else { "server_instance_lost" }.into();
                        state.pending_input_id = None;
                        state.push(format!("{error}; task was not resubmitted"));
                    }
                }
            }
            _ = ticker.tick(), if subscription.is_none() && reconnect && projection.as_ref().is_some_and(|snapshot| !snapshot.status.terminal()) => {
                if let Some(task_id) = state.task_id.as_ref() {
                    match client.observe(task_id, Some(cursor)).await {
                        Ok(stream) => subscription = Some(stream),
                        Err(error) => state.push(format!("Reconnect pending: {error}")),
                    }
                }
            }
            next = events.next() => {
                let Some(next) = next else { break; };
                let event = next?;
                let prior_task = state.task_id.clone();
                if handle_event(&client, &workspace, &check, read_only, &mut state, &mut projection, &event).await? {
                    break;
                }
                if state.task_id != prior_task && let Some(snapshot) = projection.as_ref() {
                    cursor = snapshot.cursor;
                    subscription = Some(client.observe(&snapshot.task_id, Some(cursor)).await?);
                }
            }
        }
    }
    Ok(())
}

async fn handle_event(
    client: &TaskClient,
    workspace: &Path,
    check: &Option<String>,
    read_only: bool,
    state: &mut NativeState,
    projection: &mut Option<TaskSnapshot>,
    event: &Event,
) -> Result<bool> {
    if let Event::Key(key) = event {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(task_id) = state.task_id.as_ref() {
                let _ = client
                    .request(Operation::Cancel {
                        task_id: task_id.clone(),
                    })
                    .await?;
                state.push("Cancellation requested");
            } else {
                return Ok(true);
            }
            return Ok(false);
        }
        if let (Some(task_id), Some(request_id)) = (&state.task_id, &state.pending_input_id) {
            let approved = match key.code {
                KeyCode::Char('y') => Some(true),
                KeyCode::Char('n') => Some(false),
                _ => None,
            };
            if let Some(approved) = approved {
                let response = client
                    .request(Operation::Input {
                        task_id: task_id.clone(),
                        request_id: request_id.clone(),
                        approved,
                    })
                    .await;
                if let Err(error) = response {
                    state.push(format!("Approval response: {error}"));
                    if error
                        .downcast_ref::<ServiceError>()
                        .is_some_and(|error| error.code == ErrorCode::InstanceChanged)
                    {
                        state.status = "server_instance_lost".into();
                    }
                    state.pending_input_id = None;
                    return Ok(false);
                }
                state.pending_input_id = None;
                state.push(if approved {
                    "Tool approved"
                } else {
                    "Tool denied"
                });
                return Ok(false);
            }
        }
    }
    if matches!(
        state.status.as_str(),
        "disconnected" | "server_instance_lost"
    ) {
        return Ok(false);
    }
    if state.pending_input_id.is_some() {
        return Ok(false);
    }
    match state.edit(event) {
        Edit::Submitted => {
            let input = state.editor.take();
            if input.trim().is_empty() {
                return Ok(false);
            }
            if state.model.is_empty() {
                state.model = input.trim().into();
                state.push(format!("Model selected: {}", state.model));
            } else {
                let busy = state.task_id.as_ref().is_some_and(|_| {
                    matches!(
                        state.status.as_str(),
                        "accepted" | "running" | "waiting_for_input"
                    )
                });
                if busy {
                    state.push(
                        "Current task is still active; cancel or wait before submitting another",
                    );
                    return Ok(false);
                }
                let reply = client
                    .request(Operation::Submit {
                        prompt: input.clone(),
                        workspace: workspace.to_path_buf(),
                        model: state.model.clone(),
                        effort: None,
                        read_only,
                        verification_command: check.clone(),
                        idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
                    })
                    .await?;
                let ReplyResult::Task { snapshot } = reply else {
                    anyhow::bail!("BRO task server returned an unexpected submit reply");
                };
                state.editor.push_history(input.clone());
                state.push(format!("You: {input}"));
                state.task_id = Some(snapshot.task_id.clone());
                state.live = None;
                state.push(format!("Task ID: {}", snapshot.task_id));
                update_snapshot(state, &snapshot);
                *projection = Some(*snapshot);
            }
        }
        Edit::Ended | Edit::ExitRequested => return Ok(true),
        _ => {}
    }
    Ok(false)
}

async fn read(client: &TaskClient, task_id: &str) -> Result<TaskSnapshot> {
    match client
        .request(Operation::Read {
            task_id: task_id.into(),
        })
        .await?
    {
        ReplyResult::Task { snapshot } => Ok(*snapshot),
        _ => anyhow::bail!("BRO task server returned an unexpected read reply"),
    }
}

fn update_snapshot(state: &mut NativeState, snapshot: &TaskSnapshot) {
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
    if snapshot.status == TaskStatus::WaitingForInput
        && prior_pending != snapshot.pending_input_id
        && let Some(input) = &snapshot.pending_input
    {
        state.push(format!(
            "Approve {} {}? (y/n)",
            input.tool_name, input.arguments
        ));
    }
}

fn format_event(event: &TaskEvent) -> String {
    let detail = match &event.payload {
        TaskEventPayload::SteeringUpdated { receipt, .. } => {
            format!("Steering {}: {:?}", receipt.input_id, receipt.status)
        }
        TaskEventPayload::TurnQueued {
            prompt,
            queue_order,
            ..
        } => format!("Queued #{queue_order}: {prompt}"),
        TaskEventPayload::Accepted {
            prompt, tool_mode, ..
        } => format!("Accepted ({tool_mode:?}): {prompt}"),
        TaskEventPayload::AssistantDelta { text, .. } => text.clone(),
        TaskEventPayload::AssistantStarted { .. } => "Assistant started".into(),
        TaskEventPayload::AssistantInterrupted {
            detail, partial, ..
        } => format!(
            "Assistant interrupted: {detail}\n{}",
            partial
                .content
                .iter()
                .filter_map(|part| match part {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        ),
        TaskEventPayload::ToolOutputDelta { source, text, .. } => format!("[{source}] {text}"),
        TaskEventPayload::TaskStarted => "Started".into(),
        TaskEventPayload::ModelTurn {
            request_id,
            requested_model,
            usage,
            ..
        } => format!(
            "Model turn {request_id} · requested {requested_model} · usage {}",
            usage.as_ref().map_or_else(
                || "unavailable".into(),
                |usage| format!(
                    "{} input / {} output tokens ({:?})",
                    usage.prompt_tokens, usage.completion_tokens, usage.origin
                )
            )
        ),
        TaskEventPayload::AssistantMessage { message, .. } => message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        TaskEventPayload::ToolStarted { id, name, origin } => {
            format!("Tool {name} ({id}, {origin:?}) started")
        }
        TaskEventPayload::ToolFinished {
            id,
            name,
            output,
            origin,
        } => {
            format!("Tool {name} ({id}, {origin:?}): {output:?}")
        }
        TaskEventPayload::InputRequested {
            tool_name,
            arguments,
            ..
        } => format!("Approve {tool_name} {arguments}? (y/n)"),
        TaskEventPayload::InputResolved { approved, .. } => {
            format!("Approval {}", if *approved { "granted" } else { "denied" })
        }
        TaskEventPayload::CancelRequested => "Cancellation requested".into(),
        TaskEventPayload::TaskFinished {
            status,
            final_answer,
            verification,
            detail,
            ..
        } => format!(
            "{status:?} · verification {verification:?} · {detail}\n{}",
            final_answer.as_deref().unwrap_or("")
        ),
    };
    let clipped = detail.chars().take(800).collect::<String>();
    format!("#{} {clipped}", event.seq)
}
