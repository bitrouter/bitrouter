//! Interactive Thread client; the server owns execution and durable context.
use crate::agent_local::{self, Operation, ReplyResult, ThreadClient};
use anyhow::Result;
use bitrouter_orchestrator::service::{ErrorCode, ServiceError, TurnSnapshot};
use bitrouter_orchestrator::thread::{
    ThreadChange, ThreadDirectoryPage, ThreadEvent, ThreadObservation, ThreadStatus, ThreadView,
    TurnLifecycle,
};
use bitrouter_sdk::language_model::Content;
use bitrouter_tui::agents_menu::MenuEntry;
use bitrouter_tui::editor::{Edit, Editor, press};
use bitrouter_tui::native_agent::{NativeEntryKind, NativeState, NativeView};
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::{FutureExt, StreamExt};
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

#[derive(Default)]
struct Directory {
    after: u64,
    cutoff: Option<u64>,
    next: Option<u64>,
    previous: Vec<u64>,
}

fn inventory_request(
    client: &ThreadClient,
    directory: &Directory,
) -> futures::future::BoxFuture<'static, Result<ThreadDirectoryPage>> {
    let client = client.clone();
    let operation = Operation::ListThreads {
        after: directory.after,
        cutoff: directory.cutoff,
        limit: 16,
    };
    async move {
        match client.request(operation).await? {
            ReplyResult::Directory { page } => Ok(page),
            _ => anyhow::bail!("unexpected directory reply"),
        }
    }
    .boxed()
}

fn show_directory(state: &mut NativeState, directory: &mut Directory, page: ThreadDirectoryPage) {
    directory.cutoff = Some(page.cutoff);
    directory.next = page.next_after;
    state.inventory = page
        .entries
        .into_iter()
        .map(|entry| {
            let thread = entry.thread;
            let status = match thread.status {
                ThreadStatus::Paused => "paused",
                ThreadStatus::RecoveryRequired => "recovery required",
                ThreadStatus::Closing => "closing",
                _ if entry.needs_input => "needs input",
                ThreadStatus::Busy => "working",
                _ => "inactive",
            };
            MenuEntry {
                id: thread.thread_id.clone(),
                label: format!(
                    "{} · {}",
                    thread.model,
                    thread.thread_id.get(..8).unwrap_or(&thread.thread_id)
                ),
                directory: thread.workspace.display().to_string(),
                status: status.into(),
                needs_input: entry.needs_input,
                working: thread.status == ThreadStatus::Busy && !entry.needs_input,
                metadata: vec![
                    format!("Thread: {}", thread.thread_id),
                    format!(
                        "Model: {} · Directory: {}",
                        thread.model,
                        thread.workspace.display()
                    ),
                    format!(
                        "Turn: {} · {} · Queue: {}",
                        entry.turn_id.as_deref().unwrap_or("none"),
                        entry
                            .turn_status
                            .and_then(|status| serde_json::to_value(status).ok())
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_else(|| "none".into()),
                        thread.queued.len()
                    ),
                    format!(
                        "Permissions: {}",
                        match thread.permission_profile {
                            bitrouter_orchestrator::thread::PermissionProfile::ReadOnly =>
                                "read only",
                            bitrouter_orchestrator::thread::PermissionProfile::Ask => "ask",
                            bitrouter_orchestrator::thread::PermissionProfile::AllowEffects =>
                                "allow effects",
                        }
                    ),
                    thread.pause_reason.unwrap_or_default(),
                ],
            }
        })
        .collect();
    state.menu.receive_entries(&state.inventory);
    state.inventory_help = format!(
        "↑↓ · Tab · / search page · Enter view · r refresh{}{} · Esc back",
        if directory.next.is_some() {
            " · n next"
        } else {
            ""
        },
        if directory.previous.is_empty() {
            ""
        } else {
            " · p previous"
        }
    );
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
        workspace: workspace.display().to_string(),
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
    if state.model.is_empty() {
        state.model_editor = Some(Editor::default());
    }
    let mut directory = Directory::default();
    let mut inventory_poll = None;
    let mut inventory_ticks = 0_u8;
    let mut projection: Option<ThreadView> = None;
    let mut subscription = None;
    let mut cursor = 0;
    let mut reconnect = true;
    if let Some(id) = &session.thread_id {
        let view = restore_history(&client, id, &mut state).await?;
        state.push(format!("Reattached Thread: {id}"));
        update_view(&mut state, &view);
        cursor = view.thread.cursor;
        subscription = Some(client.observe(id, Some(cursor)).await?);
        projection = Some(view);
    }
    let mut view = NativeView::open()?;
    let mut events = Some(EventStream::new());
    let mut shutdown = crate::chat::signals::Shutdown::install();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    loop {
        view.draw(&mut state)?;
        tokio::select! {
            next = async { match subscription.as_mut() { Some(stream) => stream.next().await, None => std::future::pending().await } } => {
                match next {
                    Ok(Some(ThreadObservation::Snapshot { view:fresh, resynchronized, catchup })) => {
                        if fresh.thread.cursor < cursor { continue; }
                        if resynchronized && let Some(id) = &session.thread_id {
                            match restore_history_at(&client, id, &fresh, &mut state).await {
                                Ok(()) => state.push("History cache expired; committed history restored"),
                                Err(error) => state.push(format!("History unavailable: {error}")),
                            }
                        }
                        for event in catchup { if event.seq > cursor { show_event(&mut state, &event); } }
                        cursor = fresh.thread.cursor; update_view(&mut state, &fresh); projection = Some(*fresh);
                    },
                    Ok(Some(ThreadObservation::Event { event })) => {
                        if event.seq <= cursor { continue; }
                        cursor = event.seq; show_event(&mut state, &event);
                        if let Some(view) = projection.as_mut() { view.apply(&event); update_view(&mut state, view); }
                    },
                    Ok(Some(ThreadObservation::Live { event, after_cursor })) => {
                        if after_cursor != cursor { continue; }
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
            result = async { match inventory_poll.as_mut() { Some(poll) => poll.await, None => std::future::pending().await } } => {
                inventory_poll = None;
                match result {
                    Ok(page) => show_directory(&mut state, &mut directory, page),
                    Err(error) => state.menu.set_error(Some(format!("BRO inventory unavailable: {error}"))),
                }
            },
            _ = ticker.tick() => {
                inventory_ticks = inventory_ticks.wrapping_add(1);
                if state.menu.is_open() && inventory_poll.is_none() && inventory_ticks.is_multiple_of(4) {
                    inventory_poll = Some(inventory_request(&client, &directory));
                }
                if subscription.is_none() && reconnect && let Some(id) = &session.thread_id { match client.observe(id, Some(cursor)).await { Ok(stream) => subscription=Some(stream), Err(error) => {
                    if error.downcast_ref::<ServiceError>().is_some_and(|error| matches!(error.code, ErrorCode::InstanceChanged | ErrorCode::UnknownThread)) {
                        reconnect=false; state.status="server_instance_lost".into();
                    }
                    state.push(format!("Reconnect pending: {error}"));
                } } }
            },
            signal = shutdown.recv() => match signal {
                crate::chat::signals::TerminalSignal::Shutdown => break,
                crate::chat::signals::TerminalSignal::Suspend => {
                    drop(events.take()); view.suspend()?;
                    crate::chat::signals::suspend_current_process()?;
                    view.resume()?; events = Some(EventStream::new());
                }
            },
            next = async { match events.as_mut() { Some(events) => events.next().await, None => std::future::pending().await } } => {
                let Some(next) = next else { break; };
                let event = next?;
                if bitrouter_tui::editor::is_redraw(&event) { view.invalidate(); continue; }
                if state.menu.is_open() {
                    if let Some(key) = press(&event) {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            if handle_event(&client, &workspace, &check, read_only, &mut state, &mut session, &event).await? { break; }
                            continue;
                        }
                        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) { break; }
                        if key.code == KeyCode::Char('o') && key.modifiers.is_empty() && state.input_ready()
                            && let Some(id) = state.menu.previewed_id().map(str::to_owned)
                        {
                            if session.pending.is_some() || session.create_uncertain || !state.editor.is_empty() {
                                state.menu.set_error(Some("Resolve the pending acceptance before switching conversations".into()));
                            } else {
                                let mut next = NativeState { inventory: std::mem::take(&mut state.inventory), ..Default::default() };
                                match restore_history(&client, &id, &mut next).await {
                                    Ok(fresh) => match client.observe(&id, Some(fresh.thread.cursor)).await {
                                        Ok(stream) => {
                                            cursor = fresh.thread.cursor; projection = Some(fresh);
                                            subscription = Some(stream); reconnect = true;
                                            session = Session { thread_id: Some(id), create_key: uuid::Uuid::new_v4().to_string(), ..Default::default() };
                                            next.menu = std::mem::take(&mut state.menu); next.menu.close();
                                            state = next;
                                        },
                                        Err(error) => { state.inventory = next.inventory; state.menu.set_error(Some(error.to_string())); },
                                    },
                                    Err(error) => { state.inventory = next.inventory; state.menu.set_error(Some(error.to_string())); },
                                }
                            }
                            continue;
                        }
                        if key.modifiers.is_empty() && state.menu.is_listing() && inventory_poll.is_none() {
                            let changed = match key.code {
                                KeyCode::Char('n') => if let Some(next) = directory.next { directory.previous.push(directory.after); directory.after = next; true } else { false },
                                KeyCode::Char('p') => if let Some(previous) = directory.previous.pop() { directory.after = previous; true } else { false },
                                KeyCode::Char('r') => { directory = Directory::default(); true },
                                _ => false,
                            };
                            if changed { inventory_poll = Some(inventory_request(&client, &directory)); continue; }
                        }
                    }
                    // Menu keys never reach approval, steering or the composer.
                    let page_size = state.viewport.map_or(4, |size| usize::from((size.height.saturating_mul(2) / 5).max(6).saturating_sub(3)).max(1));
                    state.menu.event_entries(&event, &state.inventory, page_size);
                    continue;
                }
                if let Some(key) = press(&event) && key.code == KeyCode::Left && key.modifiers.is_empty() && state.can_open_agents() {
                    state.menu.open_entries(&state.inventory);
                    inventory_poll = Some(inventory_request(&client, &directory));
                    continue;
                }
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
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(false);
        }
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
        if !state.input_ready()
            && (key.code == KeyCode::Enter
                || (state.editor.is_empty()
                    && state.pending_input_id.is_some()
                    && matches!(key.code, KeyCode::Char('y' | 'n'))))
        {
            state.push("Resize to at least 40×16 before submitting or approving");
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
    if let Some(editor) = state.model_editor.as_mut() {
        if bitrouter_tui::native_agent::edit(editor, event) == Edit::Submitted
            && !editor.text().trim().is_empty()
        {
            state.model = editor.text().trim().to_owned();
            state.model_editor = None;
            state.push(format!("Model selected: {}", state.model));
        }
        return Ok(false);
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
                state.model_editor = Some(Editor::default());
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
    state.model_editor = None;
    state.workspace = view.thread.workspace.display().to_string();
    state.queued = view.thread.queued.len();
    if let Some(turn) = &view.latest_turn {
        update_snapshot(state, turn);
    } else {
        state.status = "idle".into();
        state.live = None;
        state.pending_input_id = None;
        state.pending_input_detail = None;
    }
    state.turn_id = view.thread.active_turn_id.clone();
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
fn message_text(message: &bitrouter_sdk::language_model::Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_event(state: &mut NativeState, event: &ThreadEvent) {
    for change in &event.changes {
        let (id, text, kind) =
            match change {
                ThreadChange::TurnQueued {
                    user_item_id,
                    prompt,
                    ..
                } => (
                    user_item_id.clone(),
                    format!("You: {prompt}"),
                    NativeEntryKind::User,
                ),
                ThreadChange::AssistantResponse {
                    item_id, message, ..
                } => (
                    item_id.clone(),
                    message_text(message),
                    NativeEntryKind::Assistant,
                ),
                ThreadChange::AssistantInterrupted {
                    item_id,
                    partial,
                    detail,
                    ..
                } => (
                    item_id.clone(),
                    format!("{}\nAssistant interrupted: {detail}", message_text(partial)),
                    NativeEntryKind::Assistant,
                ),
                ThreadChange::ToolIntent { call, .. } => (
                    call.item_id.clone(),
                    format!(
                        "Tool {} ({}) started\n{}",
                        call.name, call.item_id, call.arguments
                    ),
                    NativeEntryKind::Detail,
                ),
                ThreadChange::ToolResult {
                    item_id, message, ..
                } => (
                    item_id.clone(),
                    format!("Tool {item_id}: {:?}", message.content),
                    NativeEntryKind::Detail,
                ),
                ThreadChange::VerificationResult { call, evidence, .. } => (
                    call.item_id.clone(),
                    format!("Verification: {evidence:?}"),
                    NativeEntryKind::Detail,
                ),
                ThreadChange::TurnLifecycle {
                    lifecycle:
                        TurnLifecycle::InputRequested {
                            request_id,
                            tool_name,
                            arguments,
                            ..
                        },
                    ..
                } => (
                    format!("approval:{request_id}"),
                    format!("Approval required: {tool_name}\n{arguments}"),
                    NativeEntryKind::Detail,
                ),
                ThreadChange::TurnLifecycle {
                    lifecycle:
                        TurnLifecycle::InputResolved {
                            request_id,
                            approved,
                        },
                    ..
                } => (
                    format!("approval:{request_id}"),
                    format!("Approval {}", if *approved { "accepted" } else { "denied" }),
                    NativeEntryKind::Detail,
                ),
                ThreadChange::TurnLifecycle {
                    lifecycle: TurnLifecycle::SteeringUpdated { receipt, text },
                    ..
                } => {
                    let id = format!("steering:{}", receipt.input_id);
                    let prompt =
                        text.as_deref()
                            .or_else(|| {
                                state.entries.iter().find(|entry| entry.id == id).and_then(
                                    |entry| {
                                        entry
                                            .text
                                            .split_once("\nSteering status:")
                                            .map(|(prompt, _)| prompt)
                                    },
                                )
                            })
                            .unwrap_or("Steering input");
                    let status = match receipt.status {
                        bitrouter_orchestrator::thread::SteeringStatus::Received => "received",
                        bitrouter_orchestrator::thread::SteeringStatus::Applied => "applied",
                        bitrouter_orchestrator::thread::SteeringStatus::NotApplied => "not applied",
                    };
                    (
                        id,
                        format!(
                            "{prompt}\nSteering status: {status}{}",
                            receipt
                                .reason
                                .as_ref()
                                .map(|reason| format!(" · {reason}"))
                                .unwrap_or_default()
                        ),
                        NativeEntryKind::User,
                    )
                }
                _ => continue,
            };
        state.upsert(id, text, kind);
    }
}

async fn restore_history(
    client: &ThreadClient,
    thread_id: &str,
    state: &mut NativeState,
) -> Result<ThreadView> {
    let view = read(client, thread_id).await?;
    restore_history_at(client, thread_id, &view, state).await?;
    update_view(state, &view);
    Ok(view)
}

async fn restore_history_at(
    client: &ThreadClient,
    thread_id: &str,
    view: &ThreadView,
    state: &mut NativeState,
) -> Result<()> {
    let cutoff = view.thread.cursor;
    let mut after = 0;
    loop {
        let ReplyResult::History { page } = client
            .request(Operation::History {
                thread_id: thread_id.into(),
                after,
                cutoff: Some(cutoff),
                limit: 128,
            })
            .await?
        else {
            anyhow::bail!("unexpected history reply");
        };
        anyhow::ensure!(
            page.cutoff == cutoff && page.thread_id == thread_id,
            "history identity changed"
        );
        for event in page.events {
            anyhow::ensure!(
                event.seq > after && event.seq <= cutoff,
                "history sequence changed"
            );
            after = event.seq;
            show_event(state, &event);
        }
        match page.next_after {
            Some(next) => {
                anyhow::ensure!(next == after && next > 0, "history made no progress");
            }
            None => break,
        }
    }
    anyhow::ensure!(after == cutoff, "history lacks its committed cutoff");
    Ok(())
}

fn update_snapshot(state: &mut NativeState, snapshot: &TurnSnapshot) {
    state.model = snapshot.model.clone();
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
    state.pending_input_detail = snapshot
        .pending_input
        .as_ref()
        .map(|input| format!("Approve {} {}? (y/n)", input.tool_name, input.arguments));
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
    use bitrouter_orchestrator::service::TurnStatus;
    use crossterm::event::KeyEvent;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn busy_enter_enqueues_and_control_enter_targets_active_turn() -> Result<()> {
        let home = tempfile::tempdir()?;
        let socket = home.path().join("controls.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (stream, _) = listener.accept().await?;
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(read).read_line(&mut line).await?;
                let command: agent_local::ThreadCommand = serde_json::from_str(&line)?;
                let result = match (index, command.operation) {
                    (0, Operation::Capabilities) => ReplyResult::Capabilities {
                        runtime: Box::new(bitrouter_orchestrator::service::RuntimeCapabilities {
                            server_instance_id: "epoch".into(),
                            limits: Default::default(),
                            execution_ownership: None,
                            startup_discovery: None,
                        }),
                        operations: vec![],
                    },
                    (
                        1,
                        Operation::EnqueueTurn {
                            thread_id, prompt, ..
                        },
                    ) => {
                        anyhow::ensure!(thread_id == "thread" && prompt == "follow up");
                        ReplyResult::Receipt {
                            receipt: bitrouter_orchestrator::thread::TurnReceipt {
                                thread_id,
                                turn_id: "queued".into(),
                                queue_order: 2,
                                status: TurnStatus::Accepted,
                            },
                        }
                    }
                    (
                        2,
                        Operation::Steer {
                            thread_id,
                            expected_turn_id,
                            text,
                            ..
                        },
                    ) => {
                        anyhow::ensure!(
                            thread_id == "thread"
                                && expected_turn_id == "active"
                                && text == "adjust course"
                        );
                        ReplyResult::Steering {
                            receipt: bitrouter_orchestrator::thread::SteeringReceipt {
                                input_id: "steering".into(),
                                turn_id: expected_turn_id,
                                order: 1,
                                status: bitrouter_orchestrator::thread::SteeringStatus::Received,
                                context_version: None,
                                next_step_id: None,
                                reason: None,
                            },
                        }
                    }
                    _ => anyhow::bail!("input reached the wrong operation"),
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
            status: "busy".into(),
            turn_id: Some("active".into()),
            ..Default::default()
        };
        let mut session = Session {
            thread_id: Some("thread".into()),
            ..Default::default()
        };
        for (text, modifiers) in [
            ("follow up", KeyModifiers::NONE),
            ("adjust course", KeyModifiers::CONTROL),
        ] {
            state.editor.set_text(text);
            let event = Event::Key(KeyEvent::new(KeyCode::Enter, modifiers));
            let mut release = KeyEvent::new(KeyCode::Enter, modifiers);
            release.kind = crossterm::event::KeyEventKind::Release;
            handle_event(
                &client,
                home.path(),
                &None,
                false,
                &mut state,
                &mut session,
                &Event::Key(release),
            )
            .await?;
            assert_eq!(state.editor.text(), text);
            state.viewport = Some((39, 15).into());
            handle_event(
                &client,
                home.path(),
                &None,
                false,
                &mut state,
                &mut session,
                &event,
            )
            .await?;
            assert_eq!(state.editor.text(), text);
            assert!(session.pending.is_none());
            state.viewport = None;
            handle_event(
                &client,
                home.path(),
                &None,
                false,
                &mut state,
                &mut session,
                &event,
            )
            .await?;
            assert!(state.editor.is_empty());
            assert_eq!(state.turn_id.as_deref(), Some("active"));
        }
        server.await??;
        Ok(())
    }

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
